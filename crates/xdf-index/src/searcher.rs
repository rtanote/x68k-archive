//! 索引クエリ実行 (T-3.4)

use crate::analyzer;
use crate::document::{make_excerpt, EXCERPT_BYTES};
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema};
use tantivy::{IndexReader, ReloadPolicy, TantivyDocument, Term};

/// 検索オプション。`query` 空 + `ext` 空はエラー、`ext` のみで全件絞り込み可。
#[derive(Debug, Clone, Default)]
pub struct SearchOpts {
    /// 全文クエリ (空文字列許容)
    pub query: String,
    /// 拡張子フィルタ (大文字統一、複数指定で OR)
    pub ext: Vec<String>,
    /// 上位 N 件
    pub limit: usize,
}

/// 読み出しに使うフィールド。**ディスク上のスキーマから名前で引く。**
///
/// `ArchiveSchema::build()` が返す `Field` は**序数**で、コンパイル時のスキーマの
/// 並び順に依存する。それを別のスキーマで開いた索引に渡すと、範囲外の序数に
/// なったり、黙って別のフィールドを指したりする。これまでフィールドは追加しか
/// されなかったので生き延びてきたが、**フィールドを 1 本足した瞬間に、索引を
/// 作り直さずバイナリだけ上げた利用者が全員踏む**。
/// 書き込み側 (`extract.rs`) は `has_structured_fields` で守られているのに、
/// ここには守りが無かった。
#[derive(Debug)]
struct ReadFields {
    image_path: Field,
    image_id: Field,
    partition: Field,
    file_path: Field,
    file_name: Field,
    ext: Field,
    size: Field,
    mtime: Field,
    /// 本文。新スキーマでは STORED なので、抜粋はここから切り出す
    body: Field,
    /// 旧索引の `body_excerpt`。新スキーマには無い (本文の重複だったので廃止)
    legacy_excerpt: Option<Field>,
    /// 既定の検索対象。**索引に実在するものだけ**を入れる。
    query_fields: Vec<Field>,
}

/// 本文フィールドの候補。索引の版によって名前が違うので、実在するものを使う。
/// 先にあるものが優先。
const BODY_FIELD_CANDIDATES: &[&str] = &["body_ngram", "body"];

impl ReadFields {
    fn resolve(schema: &Schema) -> Result<Self> {
        let f = |name: &str| {
            schema.get_field(name).with_context(|| {
                format!(
                    "索引に '{name}' フィールドがありません。古いスキーマで作られた\
                     索引の可能性があります。{}",
                    crate::index_version::REBUILD_HOWTO
                )
            })
        };
        let file_name = f("file_name")?;

        let bodies: Vec<Field> = BODY_FIELD_CANDIDATES
            .iter()
            .filter_map(|n| schema.get_field(n).ok())
            .collect();
        let body = *bodies.first().ok_or_else(|| {
            anyhow!(
                "索引に本文フィールド ({}) がありません。{}",
                BODY_FIELD_CANDIDATES.join(" / "),
                crate::index_version::REBUILD_HOWTO
            )
        })?;
        let mut query_fields = bodies;
        query_fields.push(file_name);

        Ok(Self {
            image_path: f("image_path")?,
            image_id: f("image_id")?,
            partition: f("partition")?,
            file_path: f("file_path")?,
            file_name,
            ext: f("ext")?,
            size: f("size")?,
            mtime: f("mtime")?,
            body,
            // 旧索引にしか無いので、無くても失敗にしない
            legacy_excerpt: schema.get_field("body_excerpt").ok(),
            query_fields,
        })
    }
}

pub struct Searcher {
    fields: ReadFields,
    reader: IndexReader,
    parser: QueryParser,
}

impl Searcher {
    pub fn open<P: AsRef<Path>>(index_dir: P) -> Result<Self> {
        let index = analyzer::open_in_dir(index_dir.as_ref())?;
        let fields = ReadFields::resolve(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        // デフォルトの検索対象フィールド: 本文 + ファイル名
        let parser = QueryParser::for_index(&index, fields.query_fields.clone());
        Ok(Self {
            fields,
            reader,
            parser,
        })
    }

    /// 全文クエリのみで検索 (拡張子フィルタなし)
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.search_with(SearchOpts {
            query: query.to_string(),
            ext: vec![],
            limit,
        })
    }

    /// SearchOpts ベースの検索 (拡張子フィルタあり)
    pub fn search_with(&self, opts: SearchOpts) -> Result<Vec<Hit>> {
        if opts.limit == 0 {
            return Ok(Vec::new());
        }
        if opts.query.trim().is_empty() && opts.ext.is_empty() {
            return Err(anyhow!(
                "Either query or ext filter must be provided"
            ));
        }
        let combined = self.build_query(&opts)?;
        let searcher = self.reader.searcher();
        // 0.27 の `TopDocs` はビルダーで、それ自身は `Collector` ではない。
        // スコア順のコレクタは `order_by_score()` が返す。
        let top_docs =
            searcher.search(&*combined, &TopDocs::with_limit(opts.limit).order_by_score())?;
        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            hits.push(doc_to_hit(&self.fields, &doc, score));
        }
        Ok(hits)
    }

    fn build_query(&self, opts: &SearchOpts) -> Result<Box<dyn Query>> {
        let has_query = !opts.query.trim().is_empty();
        let has_ext = !opts.ext.is_empty();

        // ベースクエリ
        let base: Box<dyn Query> = if has_query {
            self.parser.parse_query(&opts.query)?
        } else {
            Box::new(AllQuery)
        };

        if !has_ext {
            return Ok(base);
        }

        // 拡張子フィルタ: 各 ext を Term にして OR で結合
        let ext_subqueries: Vec<(Occur, Box<dyn Query>)> = opts
            .ext
            .iter()
            .map(|e| {
                let upper = e.to_ascii_uppercase();
                let term = Term::from_field_text(self.fields.ext, &upper);
                let q: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                (Occur::Should, q)
            })
            .collect();
        let ext_query: Box<dyn Query> = Box::new(BooleanQuery::new(ext_subqueries));

        // ベース AND 拡張子フィルタ
        Ok(Box::new(BooleanQuery::new(vec![
            (Occur::Must, base),
            (Occur::Must, ext_query),
        ])))
    }

    /// 索引内の総ドキュメント数 (status 用)
    pub fn doc_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub score: f32,
    pub image_path: String,
    pub image_id: String,
    pub partition: u64,
    pub file_path: String,
    pub file_name: String,
    pub ext: String,
    pub size: u64,
    pub mtime: i64,
    pub excerpt: String,
}

fn doc_to_hit(fields: &ReadFields, doc: &TantivyDocument, score: f32) -> Hit {
    let get_str = |f| {
        doc.get_first(f)
            .and_then(value_as_str)
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    let get_u64 = |f| doc.get_first(f).and_then(value_as_u64).unwrap_or(0);
    let get_i64 = |f| doc.get_first(f).and_then(value_as_i64).unwrap_or(0);
    // 抜粋は STORED 本文から切り出す。旧索引には専用の `body_excerpt` があるので
    // そちらに落とす (バイナリだけ上げた利用者の見え方を変えないため)。
    let excerpt = match doc.get_first(fields.body).and_then(value_as_str) {
        Some(text) if !text.is_empty() => make_excerpt(text, EXCERPT_BYTES),
        _ => fields.legacy_excerpt.map(get_str).unwrap_or_default(),
    };
    Hit {
        score,
        image_path: get_str(fields.image_path),
        image_id: get_str(fields.image_id),
        partition: get_u64(fields.partition),
        file_path: get_str(fields.file_path),
        file_name: get_str(fields.file_name),
        ext: get_str(fields.ext),
        size: get_u64(fields.size),
        mtime: get_i64(fields.mtime),
        excerpt,
    }
}

/// 0.27 で `get_first` が返すのは `CompactDocValue` で、所有値ではない。
/// `Value` 境界にしておけば `&OwnedValue` でも通るので、呼び出し側は変えずに済む。
fn value_as_str<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<&'a str> {
    v.as_str()
}
fn value_as_u64<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<u64> {
    v.as_u64()
}
fn value_as_i64<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<i64> {
    v.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ArchiveSchema;
    use tantivy::schema::{Schema, STORED, STRING, TEXT};

    /// 本文フィールドの名前だけを変えた最小スキーマ。
    /// `None` なら本文フィールドを入れない。
    fn schema_with_body(body: Option<&str>) -> Schema {
        let mut sb = Schema::builder();
        for name in [
            "image_path",
            "image_id",
            "partition",
            "file_path",
            "ext",
            "size",
            "mtime",
        ] {
            sb.add_text_field(name, STRING | STORED);
        }
        sb.add_text_field("file_name", TEXT | STORED);
        sb.add_text_field("body_excerpt", STORED);
        if let Some(b) = body {
            sb.add_text_field(b, TEXT);
        }
        sb.build()
    }

    #[test]
    fn resolves_against_the_current_schema() {
        let s = ArchiveSchema::build();
        let f = ReadFields::resolve(&s.schema).expect("現行スキーマで解決できること");
        assert_eq!(f.query_fields.len(), 2, "本文 + file_name");
    }

    /// 旧スキーマ (`body`) の索引を新しいバイナリで開いても壊れないこと。
    ///
    /// **公開リポジトリの利用者が、索引を作り直さずバイナリだけ上げる経路。**
    /// ここが落ちると全員が踏む。
    #[test]
    fn falls_back_to_the_legacy_body_field() {
        let s = schema_with_body(Some("body"));
        let f = ReadFields::resolve(&s).expect("旧スキーマでも開けること");
        assert_eq!(f.query_fields.len(), 2);
        assert_eq!(f.query_fields[0], s.get_field("body").unwrap());
    }

    /// 新スキーマ (`body_ngram`) が入ったらそちらを優先すること。
    /// 段階 1 でスキーマを変えたときに、ここを直さなくても効く。
    #[test]
    fn prefers_body_ngram_when_present() {
        let s = schema_with_body(Some("body_ngram"));
        let f = ReadFields::resolve(&s).expect("新スキーマで開けること");
        assert_eq!(f.query_fields[0], s.get_field("body_ngram").unwrap());
    }

    /// 本文フィールドが無い索引は、panic ではなく「作り直して」と言うこと。
    #[test]
    fn missing_body_field_is_an_error_not_a_panic() {
        let s = schema_with_body(None);
        let err = ReadFields::resolve(&s).expect_err("エラーになること");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("xdf-index build"),
            "作り直しの手順を案内すること: {msg}"
        );
    }

    /// 抜粋が STORED 本文から切り出されること。
    ///
    /// 旧 `body_excerpt` は本文の先頭 2048 バイトそのものだったので、本文が
    /// STORED になった時点で完全な重複になり廃止した。**出力を変えない**のが
    /// 条件なのでここで縛る。将来誰かが本文を切り詰めて保存したら落ちる。
    #[test]
    fn excerpt_is_cut_from_the_stored_body() {
        let s = schema_with_body(Some("body_ngram"));
        let f = ReadFields::resolve(&s).unwrap();
        let long = "あ".repeat(2000); // 6000 バイト
        let mut doc = TantivyDocument::default();
        doc.add_text(f.body, &long);
        let hit = doc_to_hit(&f, &doc, 1.0);
        assert_eq!(hit.excerpt, make_excerpt(&long, EXCERPT_BYTES));
        assert!(hit.excerpt.len() <= EXCERPT_BYTES, "上限を超えないこと");
    }

    /// 旧索引では `body_excerpt` に落ちること。
    ///
    /// 旧スキーマの `body` は STORED ではないので doc から本文は取れない。
    /// 索引を作り直さずバイナリだけ上げた利用者の見え方を変えないための経路。
    #[test]
    fn legacy_index_excerpt_comes_from_body_excerpt() {
        let s = schema_with_body(Some("body"));
        let f = ReadFields::resolve(&s).unwrap();
        let mut doc = TantivyDocument::default();
        doc.add_text(
            f.legacy_excerpt.expect("旧スキーマには在る"),
            "旧索引の抜粋",
        );
        let hit = doc_to_hit(&f, &doc, 1.0);
        assert_eq!(hit.excerpt, "旧索引の抜粋");
    }

    /// フィールドを足しても序数で壊れないこと = この修正の本題。
    ///
    /// 旧スキーマの索引に対して、**新しい** `ArchiveSchema` の序数をそのまま
    /// 渡していたのが元のバグ。名前で引いていれば、並びが違っても
    /// 指すものは変わらない。
    #[test]
    fn field_order_does_not_matter() {
        let mut sb = Schema::builder();
        // わざと並びを変え、先頭に知らないフィールドを足す
        sb.add_text_field("something_new", TEXT);
        sb.add_text_field("body", TEXT);
        sb.add_text_field("body_excerpt", STORED);
        sb.add_text_field("file_name", TEXT | STORED);
        for name in [
            "mtime",
            "size",
            "ext",
            "file_path",
            "partition",
            "image_id",
            "image_path",
        ] {
            sb.add_text_field(name, STRING | STORED);
        }
        let s = sb.build();
        let f = ReadFields::resolve(&s).expect("並びが違っても解決できること");
        assert_eq!(f.image_path, s.get_field("image_path").unwrap());
        assert_eq!(f.body, s.get_field("body").unwrap());
        assert_eq!(f.legacy_excerpt, s.get_field("body_excerpt").ok());
    }
}
