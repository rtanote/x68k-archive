//! 構造化メタデータクエリ (Phase 5b)
//!
//! `archive_metadata_query` MCP ツール / `xdf-index query` CLI のバックエンド。
//! Tantivy の構造化フィールドに対するフィルタ + Rust 側の post-filter (部分一致) で
//! 確定的なリスト出力を行う。
//!
//! ## なぜ post-filter か
//! 仕様書では `lindera-tantivy` を前提に payload JSON への dot-path クエリが
//! 想定されているが、現状は形態素解析を入れていない。代わりに:
//!   1. 構造化フィールド (category, issue_no, issue_kind) で **Tantivy 側で粗く絞る**
//!   2. payload を `serde_json::Value` で取り出し、`submitter_contains` などは
//!      **Rust の `String::contains` で post-filter** する。
//! 想定スケール (数千レコード) では十分実用的で、形態素解析の依存を増やさずに済む。

use crate::schema::ArchiveSchema;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Bound;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, RangeQuery, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{Index, ReloadPolicy, TantivyDocument, Term};

/// プリセットビュー
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MetadataView {
    /// 電脳倶楽部の楽曲一覧 (本誌+別冊横断)
    DennouTracks,
}

impl MetadataView {
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "dennou_tracks" => Ok(Self::DennouTracks),
            other => Err(anyhow!("Unknown view: {}", other)),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DennouTracks => "dennou_tracks",
        }
    }
    /// このビューが既定で絞る category 値
    pub fn category(&self) -> &'static str {
        match self {
            Self::DennouTracks => "music",
        }
    }
    /// CSV / JSON のカラム順 (T1-18)
    pub fn columns(&self) -> &'static [&'static str] {
        match self {
            Self::DennouTracks => &[
                "issue_no",
                "issue_kind",
                "title",
                "original_artist",
                "composer",
                "arranger",
                "submitter",
                "submitter_pref",
                "engine",
                "duration",
                "image_path",
                "source_path",
            ],
        }
    }
}

/// 絞り込みフィルタ (どれも省略可)
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct MetadataFilters {
    pub issue_no_min: Option<u64>,
    pub issue_no_max: Option<u64>,
    /// "main" / "extra" / "all"
    pub issue_kind: Option<String>,
    pub submitter_contains: Option<String>,
    pub title_contains: Option<String>,
    pub engine_contains: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderBy {
    IssueAsc,
    IssueDesc,
    TitleAsc,
}

impl Default for OrderBy {
    fn default() -> Self {
        OrderBy::IssueAsc
    }
}

#[derive(Debug, Clone)]
pub struct QueryOpts {
    pub view: MetadataView,
    pub filters: MetadataFilters,
    pub order_by: OrderBy,
    /// 1 〜 N 件
    pub limit: usize,
}

impl Default for QueryOpts {
    fn default() -> Self {
        Self {
            view: MetadataView::DennouTracks,
            filters: MetadataFilters::default(),
            order_by: OrderBy::IssueAsc,
            limit: 1000,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    /// 行ごとのカラム値 (`columns` の順序)。値は文字列化済み。
    pub rows: Vec<Vec<String>>,
    pub columns: Vec<String>,
    pub row_count: usize,
    /// Tantivy 取得段階で limit にぶつかって切り詰めたか
    pub truncated: bool,
}

/// 索引にクエリを実行して行を返す
pub fn run_query<P: AsRef<Path>>(index_dir: P, opts: &QueryOpts) -> Result<QueryResult> {
    let schema = ArchiveSchema::build();
    let index = Index::open_in_dir(index_dir.as_ref())?;
    if !ArchiveSchema::has_structured_fields(&index.schema()) {
        return Err(anyhow!(
            "Index at {:?} lacks structured fields. \
             Rebuild with `xdf-index build --fresh` then run `xdf-index extract`.",
            index_dir.as_ref()
        ));
    }
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    let searcher = reader.searcher();

    // Tantivy 側: category + issue_no 範囲 + issue_kind
    let query = build_tantivy_query(&schema, opts)?;

    // 取得は post-filter のため少し多めに引いておく (limit*4、上限 50000)
    let prefetch = opts.limit.saturating_mul(4).clamp(opts.limit, 50_000);
    // 0.27 の `TopDocs` はビルダーで、それ自身は `Collector` ではない。
    // スコア順のコレクタは `order_by_score()` が返す。
    let top_docs = searcher.search(&*query, &TopDocs::with_limit(prefetch).order_by_score())?;

    // ロード + post-filter
    let mut rows: Vec<RowExtracted> = Vec::with_capacity(top_docs.len());
    for (_score, addr) in &top_docs {
        let doc: TantivyDocument = searcher.doc(*addr)?;
        let row = extract_row(&schema, &doc);
        if pass_post_filters(&row, &opts.filters) {
            rows.push(row);
        }
    }

    // ソート (Rust 側で確定的に)
    sort_rows(&mut rows, opts.order_by);

    // limit に切り詰め
    let truncated = top_docs.len() >= prefetch || rows.len() > opts.limit;
    rows.truncate(opts.limit);

    let columns: Vec<String> = opts.view.columns().iter().map(|s| s.to_string()).collect();
    let formatted: Vec<Vec<String>> =
        rows.iter().map(|r| format_row(r, opts.view)).collect();

    Ok(QueryResult {
        row_count: formatted.len(),
        rows: formatted,
        columns,
        truncated,
    })
}

/// CSV (BOM 付き UTF-8) で出力。Excel 等で文字化けしないよう先頭に EF BB BF。
pub fn rows_to_csv(result: &QueryResult) -> String {
    let mut out = String::new();
    // BOM
    out.push('\u{FEFF}');
    out.push_str(&csv_join(&result.columns));
    out.push('\n');
    for row in &result.rows {
        out.push_str(&csv_join(row));
        out.push('\n');
    }
    out
}

fn csv_join(fields: &[String]) -> String {
    let mut out = String::new();
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&csv_quote(f));
    }
    out
}

fn csv_quote(s: &str) -> String {
    let needs_quote =
        s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r');
    if needs_quote {
        let escaped = s.replace('"', "\"\"");
        format!("\"{}\"", escaped)
    } else {
        s.to_string()
    }
}

// ---- 内部 ----

fn build_tantivy_query(schema: &ArchiveSchema, opts: &QueryOpts) -> Result<Box<dyn Query>> {
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
    // category 必須
    let cat = opts.view.category();
    let term = Term::from_field_text(schema.category, cat);
    clauses.push((
        Occur::Must,
        Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
    ));

    // issue_no 範囲。0.22 は `RangeQuery::new_u64_bounds(field_name, lower, upper)`
    // だったが、0.27 は `RangeQuery::new(Bound<Term>, Bound<Term>)` になった。
    // 境界は値ではなく `Term` で表すので、フィールド名の文字列ではなく
    // schema の `Field` から組む（名前の打ち間違いが型で防げるようにもなった）。
    let min = opts.filters.issue_no_min.unwrap_or(0);
    let max = opts.filters.issue_no_max.unwrap_or(u64::MAX);
    if opts.filters.issue_no_min.is_some() || opts.filters.issue_no_max.is_some() {
        let lower = Bound::Included(Term::from_field_u64(schema.issue_no, min));
        let upper = Bound::Included(Term::from_field_u64(schema.issue_no, max));
        clauses.push((Occur::Must, Box::new(RangeQuery::new(lower, upper))));
    }

    // issue_kind ("all" は無指定扱い)
    if let Some(k) = &opts.filters.issue_kind {
        if k != "all" && !k.is_empty() {
            let term = Term::from_field_text(schema.issue_kind, k);
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }
    }

    if clauses.len() == 1 {
        Ok(clauses.pop().map(|(_, q)| q).unwrap())
    } else if clauses.is_empty() {
        Ok(Box::new(AllQuery))
    } else {
        Ok(Box::new(BooleanQuery::new(clauses)))
    }
}

#[derive(Debug, Clone, Default)]
struct RowExtracted {
    image_path: String,
    source_path: String,
    issue_no: u64,
    issue_kind: String,
    /// payload を flat にした key→string マップ (filter / format で使う)
    payload: HashMap<String, String>,
}

fn extract_row(schema: &ArchiveSchema, doc: &TantivyDocument) -> RowExtracted {
    let mut row = RowExtracted::default();
    if let Some(v) = doc.get_first(schema.image_path) {
        if let Some(s) = value_as_str(v) {
            row.image_path = s.to_string();
        }
    }
    if let Some(v) = doc.get_first(schema.file_path) {
        if let Some(s) = value_as_str(v) {
            row.source_path = s.to_string();
        }
    }
    if let Some(v) = doc.get_first(schema.issue_no) {
        if let Some(n) = value_as_u64(v) {
            row.issue_no = n;
        }
    }
    if let Some(v) = doc.get_first(schema.issue_kind) {
        if let Some(s) = value_as_str(v) {
            row.issue_kind = s.to_string();
        }
    }
    if let Some(v) = doc.get_first(schema.payload) {
        flatten_owned_value("", v, &mut row.payload);
    }
    row
}

/// `OwnedValue::Object` を再帰的に flat なドット記法に展開する。
/// 配列は最初の要素のみ拾う (ペイロード要件: タイトル等はスカラー、files も alias なので OK)。
/// 0.27 では variant を直に match できない（`get_first` が返すのは
/// `CompactDocValue`）。`Value` のアクセサで辿る形にした。容れ物を先に見るのは
/// 元の match の並びと同じ意図で、Object / Array は再帰し、葉だけ文字列にする。
fn flatten_owned_value<'a, V: tantivy::schema::Value<'a>>(
    prefix: &str,
    v: V,
    out: &mut HashMap<String, String>,
) {
    if let Some(obj) = v.as_object() {
        for (k, vv) in obj {
            let key = if prefix.is_empty() {
                k.to_string()
            } else {
                format!("{}.{}", prefix, k)
            };
            flatten_owned_value(&key, vv, out);
        }
        return;
    }
    if let Some(arr) = v.as_array() {
        // 配列は要素を `key.0`, `key.1`, ... にする
        for (i, item) in arr.enumerate() {
            flatten_owned_value(&format!("{}.{}", prefix, i), item, out);
        }
        return;
    }
    // 葉。Null / Bytes / Date などは元の `_ => {}` と同じく落とす。
    if let Some(s) = v.as_str() {
        out.insert(prefix.to_string(), s.to_string());
    } else if let Some(n) = v.as_u64() {
        out.insert(prefix.to_string(), n.to_string());
    } else if let Some(n) = v.as_i64() {
        out.insert(prefix.to_string(), n.to_string());
    } else if let Some(n) = v.as_f64() {
        out.insert(prefix.to_string(), n.to_string());
    } else if let Some(b) = v.as_bool() {
        out.insert(prefix.to_string(), b.to_string());
    }
}

fn pass_post_filters(row: &RowExtracted, f: &MetadataFilters) -> bool {
    if let Some(s) = &f.submitter_contains {
        let v = row.payload.get("submitter").cloned().unwrap_or_default();
        if !v.contains(s.as_str()) {
            return false;
        }
    }
    if let Some(s) = &f.title_contains {
        let v = row.payload.get("title").cloned().unwrap_or_default();
        if !v.contains(s.as_str()) {
            return false;
        }
    }
    if let Some(s) = &f.engine_contains {
        let v = row.payload.get("engine").cloned().unwrap_or_default();
        if !v.contains(s.as_str()) {
            return false;
        }
    }
    true
}

fn sort_rows(rows: &mut [RowExtracted], order: OrderBy) {
    match order {
        OrderBy::IssueAsc => rows.sort_by(|a, b| {
            a.issue_kind
                .cmp(&b.issue_kind)
                .then(a.issue_no.cmp(&b.issue_no))
                .then(a.source_path.cmp(&b.source_path))
        }),
        OrderBy::IssueDesc => rows.sort_by(|a, b| {
            a.issue_kind
                .cmp(&b.issue_kind)
                .then(b.issue_no.cmp(&a.issue_no))
                .then(a.source_path.cmp(&b.source_path))
        }),
        OrderBy::TitleAsc => rows.sort_by(|a, b| {
            let at = a.payload.get("title").cloned().unwrap_or_default();
            let bt = b.payload.get("title").cloned().unwrap_or_default();
            at.cmp(&bt)
        }),
    }
}

fn format_row(row: &RowExtracted, view: MetadataView) -> Vec<String> {
    view.columns()
        .iter()
        .map(|col| match *col {
            "issue_no" => row.issue_no.to_string(),
            "issue_kind" => row.issue_kind.clone(),
            "image_path" => row.image_path.clone(),
            "source_path" => row.source_path.clone(),
            other => row.payload.get(other).cloned().unwrap_or_default(),
        })
        .collect()
}

/// 0.27 で `get_first` が返すのは `CompactDocValue` で、所有値ではない。
/// `Value` 境界にしておけば `&OwnedValue` でも通るので、呼び出し側は変えずに済む。
fn value_as_str<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<&'a str> {
    v.as_str()
}
fn value_as_u64<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<u64> {
    v.as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_columns_match_t1_18() {
        let cols = MetadataView::DennouTracks.columns();
        assert_eq!(cols[0], "issue_no");
        assert!(cols.contains(&"title"));
        assert!(cols.contains(&"composer"));
        assert!(cols.contains(&"submitter"));
        assert_eq!(cols[cols.len() - 1], "source_path");
    }

    #[test]
    fn csv_quotes_special_chars() {
        let q = csv_quote("Hello, World");
        assert_eq!(q, "\"Hello, World\"");
        let q = csv_quote("plain");
        assert_eq!(q, "plain");
        let q = csv_quote("with \"quote\"");
        assert_eq!(q, "\"with \"\"quote\"\"\"");
    }

    #[test]
    fn csv_starts_with_bom() {
        let result = QueryResult {
            rows: vec![vec!["94".into(), "main".into()]],
            columns: vec!["issue_no".into(), "issue_kind".into()],
            row_count: 1,
            truncated: false,
        };
        let csv = rows_to_csv(&result);
        assert!(csv.starts_with('\u{FEFF}'));
        assert!(csv.contains("issue_no,issue_kind"));
    }

    #[test]
    fn pass_post_filter_substring() {
        let mut row = RowExtracted::default();
        row.payload.insert("submitter".into(), "山田 太郎".into());
        let f = MetadataFilters {
            submitter_contains: Some("山田".into()),
            ..Default::default()
        };
        assert!(pass_post_filters(&row, &f));
        let f2 = MetadataFilters {
            submitter_contains: Some("鈴木".into()),
            ..Default::default()
        };
        assert!(!pass_post_filters(&row, &f2));
    }

    #[test]
    fn flatten_object_payload() {
        // 0.27 で `OwnedValue::Object` は `Vec<(String, OwnedValue)>`。
        // この検査はキー引きなので、並びは元の BTreeMap 版と関係ない。
        use tantivy::schema::OwnedValue;
        let inner = OwnedValue::Object(vec![(
            "doc".into(),
            OwnedValue::Str("MUSIC/SONGA.DOC".into()),
        )]);
        let v = OwnedValue::Object(vec![
            ("title".into(), OwnedValue::Str("SONGA".into())),
            ("files".into(), inner),
        ]);
        let mut m = HashMap::new();
        flatten_owned_value("", &v, &mut m);
        assert_eq!(m.get("title").map(|s| s.as_str()), Some("SONGA"));
        assert_eq!(m.get("files.doc").map(|s| s.as_str()), Some("MUSIC/SONGA.DOC"));
    }
}
