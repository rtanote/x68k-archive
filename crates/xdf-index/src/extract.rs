//! 構造化メタデータ抽出: SchemaPlugin trait + ExtractedRecord + Tantivy Document 変換
//!
//! 各プラグインは「このイメージは自分の対象か」を判定し、対象なら 1 イメージから N レコードを生成する。
//! レコードは `unique_id` をキーに upsert (delete_term → add_document) で索引へ書き込む。
//!
//! 設計: `docs/additional_spec/design.md`「電脳倶楽部スキーマプラグイン」参照。

use crate::builder::{compute_image_id, discover_images};
use crate::schema::ArchiveSchema;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use tantivy::schema::Facet;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};
use xdf_fs::bpb::Bpb;
use xdf_fs::fs::Filesystem;
use xdf_fs::image::{DiskImage, OpenedImage};

/// 抽出時のコンテキスト (1 image / 1 partition 単位)
pub struct ExtractContext<'a> {
    /// ホスト側のイメージファイルパス (例: "/archive/Dennou094A.img")
    pub image_path: &'a str,
    /// 内容ハッシュ先頭 16hex (例: "9dc9b31fab1c2d3e")
    pub image_id: &'a str,
    /// HDD のパーティション番号。フロッピーは 0
    pub partition: usize,
    /// 当該パーティションの読み出し可能な Filesystem
    pub fs: &'a Filesystem<'a>,
}

impl<'a> ExtractContext<'a> {
    /// `image_id` の先頭 8hex (人間が見やすい短縮形)
    pub fn image_id_short(&self) -> &str {
        let n = self.image_id.len().min(8);
        &self.image_id[..n]
    }
}

/// 抽出結果。0 レコード + warnings のみのケースもある。
pub struct ExtractResult {
    pub records: Vec<ExtractedRecord>,
    pub warnings: Vec<String>,
}

impl ExtractResult {
    pub fn empty() -> Self {
        Self {
            records: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// 1 件の構造化レコード (= Tantivy ドキュメント1件)
#[derive(Debug, Clone)]
pub struct ExtractedRecord {
    /// upsert キー。`{plugin}:{image_id_short}:{source_path}` 形式を推奨
    pub unique_id: String,
    /// "music" / "gallery" / "tool" など
    pub category: String,
    /// レコードが由来するイメージ内ファイルパス (例: "MUSIC/SONGA.DOC")
    pub source_path: String,
    pub image_path: String,
    pub image_id: String,
    pub partition: usize,
    pub issue_no: u64,
    pub issue_kind: String, // "main" / "extra"
    pub issue_role: String, // "A" / "B" / "X" or ""
    /// "/dennou/main/094" 形式
    pub location_facet: String,
    /// 構造化ペイロード。warnings は payload 内の `_warnings` 配列に格納する
    pub payload: serde_json::Value,
    pub schema_plugin: String,
    pub schema_version: u64,
}

/// プラグイン trait。1 plugin = 1 雑誌 (例: 電脳倶楽部、Oh!X 付録)
pub trait SchemaPlugin: Send + Sync {
    /// プラグイン名 (例: "dennou")
    fn name(&self) -> &'static str;

    /// プラグインのバージョン。上げると対象イメージの再抽出が走る (extract --plugin-version)
    fn version(&self) -> u64;

    /// このイメージを処理対象とするか判定
    fn detect(&self, ctx: &ExtractContext<'_>) -> bool;

    /// 抽出を実行 (1 image / 1 partition から N レコード)
    fn extract(&self, ctx: &ExtractContext<'_>) -> Result<ExtractResult>;
}

/// `ExtractedRecord` → Tantivy `TantivyDocument`
pub fn record_to_doc(schema: &ArchiveSchema, record: &ExtractedRecord) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_text(schema.unique_id, &record.unique_id);
    doc.add_text(schema.category, &record.category);
    doc.add_text(schema.image_path, &record.image_path);
    doc.add_text(schema.image_id, &record.image_id);
    doc.add_u64(schema.partition, record.partition as u64);
    doc.add_text(schema.file_path, &record.source_path);
    // file_name は source_path のベース名 (検索対象として一応入れる)
    let file_name = record.source_path.rsplit('/').next().unwrap_or(&record.source_path);
    doc.add_text(schema.file_name, file_name);
    doc.add_u64(schema.issue_no, record.issue_no);
    doc.add_text(schema.issue_kind, &record.issue_kind);
    doc.add_text(schema.issue_role, &record.issue_role);
    doc.add_text(schema.schema_plugin, &record.schema_plugin);
    doc.add_u64(schema.schema_version, record.schema_version);
    doc.add_facet(schema.location_facet, Facet::from(record.location_facet.as_str()));
    // payload JSON: serde_json::Value → tantivy の add_field_value 経由で追加
    add_json_payload(&mut doc, schema.payload, &record.payload);
    doc
}

/// serde_json::Value (object) を tantivy の json field に追加。
/// payload は object であることを前提にする (top-level scalar は弾く)。
fn add_json_payload(
    doc: &mut TantivyDocument,
    field: tantivy::schema::Field,
    value: &serde_json::Value,
) {
    use tantivy::schema::OwnedValue;
    let owned = json_value_to_owned(value);
    // OwnedValue::Object でなければ無視 (payload は object 必須)
    if let OwnedValue::Object(_) = &owned {
        // 0.27 の `add_field_value` は `V: Value<'a>` を取る。`Value` を実装して
        // いるのは `&OwnedValue` の側なので、参照で渡す。
        doc.add_field_value(field, &owned);
    }
}

fn json_value_to_owned(v: &serde_json::Value) -> tantivy::schema::OwnedValue {
    use tantivy::schema::OwnedValue;
    match v {
        serde_json::Value::Null => OwnedValue::Null,
        serde_json::Value::Bool(b) => OwnedValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                OwnedValue::I64(i)
            } else if let Some(u) = n.as_u64() {
                OwnedValue::U64(u)
            } else if let Some(f) = n.as_f64() {
                OwnedValue::F64(f)
            } else {
                OwnedValue::Null
            }
        }
        serde_json::Value::String(s) => OwnedValue::Str(s.clone()),
        serde_json::Value::Array(arr) => {
            OwnedValue::Array(arr.iter().map(json_value_to_owned).collect())
        }
        serde_json::Value::Object(map) => {
            // 0.27 で `OwnedValue::Object` の中身が `BTreeMap` から
            // `Vec<(String, OwnedValue)>` になった。serde_json::Map の反復は
            // 既に決まった順（既定では挿入順、preserve_order なしなら辞書順）
            // なので、そのまま詰めれば元と同じ並びになる。
            OwnedValue::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), json_value_to_owned(v)))
                    .collect(),
            )
        }
    }
}

/// 1 レコードを upsert。delete_term は次の commit まで遅延される。
pub fn upsert_record(
    schema: &ArchiveSchema,
    writer: &mut IndexWriter,
    record: &ExtractedRecord,
) -> Result<()> {
    let term = Term::from_field_text(schema.unique_id, &record.unique_id);
    writer.delete_term(term);
    let doc = record_to_doc(schema, record);
    writer.add_document(doc)?;
    Ok(())
}

// ---- ランナー (archive_dir 全体に対して plugins を適用) ----

pub struct ExtractOpts {
    /// IndexWriter 用バッファサイズ (バイト)。50MB+ を推奨。
    pub writer_memory: usize,
    /// 既に抽出済みでも再処理する (false: schema_plugin+version で skip 判定)
    pub force: bool,
    /// 特定プラグインのみ走らせる (None なら全プラグイン)
    pub only_plugin: Option<String>,
}

impl Default for ExtractOpts {
    fn default() -> Self {
        Self {
            writer_memory: 50_000_000,
            force: false,
            only_plugin: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ExtractStats {
    pub images_seen: usize,
    pub images_extracted: usize,
    pub images_skipped_extracted: usize,
    pub records_added: usize,
    pub warnings: usize,
    /// plugin名 → 適用イメージ数
    pub by_plugin: BTreeMap<String, usize>,
}

/// archive_dir 配下の全イメージに対し plugins を順に試して構造化抽出を実行する。
pub fn extract_archive(
    archive_dir: &Path,
    index_dir: &Path,
    plugins: &[Box<dyn SchemaPlugin>],
    opts: ExtractOpts,
) -> Result<ExtractStats> {
    let schema = ArchiveSchema::build();
    let index = open_index_for_extract(index_dir, &schema)?;
    let mut writer: IndexWriter = index.writer(opts.writer_memory)?;

    // 既に当該 (plugin, version, image_id) で抽出済みのイメージを把握
    let already: BTreeMap<(String, u64), std::collections::HashSet<String>> = if opts.force {
        BTreeMap::new()
    } else {
        collect_extracted_keys(&index, &schema)?
    };

    let mut stats = ExtractStats::default();
    let images = discover_images(archive_dir)?;
    for img_path in images {
        stats.images_seen += 1;
        let id = match compute_image_id(&img_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warning: cannot hash {:?}: {}", img_path, e);
                continue;
            }
        };
        let img_str = img_path.to_string_lossy().to_string();
        let opened = match OpenedImage::open(&img_path) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("warning: cannot open {}: {}", img_str, e);
                continue;
            }
        };

        let mut extracted_for_this = false;
        let plugin_iter = plugins.iter().filter(|p| {
            opts.only_plugin
                .as_deref()
                .map_or(true, |n| n == p.name())
        });
        for plugin in plugin_iter {
            // skip-if-already-extracted 判定
            let key = (plugin.name().to_string(), plugin.version());
            if let Some(set) = already.get(&key) {
                if set.contains(&id) {
                    stats.images_skipped_extracted += 1;
                    extracted_for_this = true;
                    break;
                }
            }
            match try_extract_with_plugin(
                &**plugin,
                &schema,
                &mut writer,
                &opened,
                &img_str,
                &id,
            ) {
                Ok(Some(n_recs)) => {
                    stats.images_extracted += 1;
                    stats.records_added += n_recs;
                    *stats.by_plugin.entry(plugin.name().into()).or_insert(0) += 1;
                    extracted_for_this = true;
                    break;
                }
                Ok(None) => continue, // detect == false
                Err(e) => {
                    eprintln!(
                        "warning: plugin {} failed on {}: {}",
                        plugin.name(),
                        img_str,
                        e
                    );
                }
            }
        }
        if !extracted_for_this {
            // どのプラグインにもヒットしなかったケースは静かにスキップ
        }
    }

    writer.commit().context("Failed to commit extraction writes")?;
    Ok(stats)
}

/// 1 イメージに 1 plugin を試す。
/// detect=false なら Ok(None)、detect=true で抽出が走ったら Ok(Some(records))、
/// 例外なら Err。
fn try_extract_with_plugin(
    plugin: &dyn SchemaPlugin,
    schema: &ArchiveSchema,
    writer: &mut IndexWriter,
    opened: &OpenedImage,
    image_path: &str,
    image_id: &str,
) -> Result<Option<usize>> {
    let mut total_records: usize = 0;
    let mut any_detected = false;

    match opened {
        OpenedImage::Floppy(img) => {
            let fs = Filesystem::open(img)?;
            let ctx = ExtractContext {
                image_path,
                image_id,
                partition: 0,
                fs: &fs,
            };
            if plugin.detect(&ctx) {
                any_detected = true;
                let result = plugin.extract(&ctx)?;
                for rec in &result.records {
                    upsert_record(schema, writer, rec)?;
                }
                total_records += result.records.len();
                for w in &result.warnings {
                    eprintln!("  [{}] {}: {}", plugin.name(), image_path, w);
                }
            }
        }
        OpenedImage::Hdd(hdd) => {
            for idx in 0..hdd.partitions().len() {
                let part = match hdd.partition(idx) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let boot = match part.read_sector(0) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let bpb = match Bpb::parse_hdd(boot) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let fs = match Filesystem::open_with_bpb(&part, bpb) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let ctx = ExtractContext {
                    image_path,
                    image_id,
                    partition: idx,
                    fs: &fs,
                };
                if plugin.detect(&ctx) {
                    any_detected = true;
                    let result = plugin.extract(&ctx)?;
                    for rec in &result.records {
                        upsert_record(schema, writer, rec)?;
                    }
                    total_records += result.records.len();
                    for w in &result.warnings {
                        eprintln!("  [{}] {}:{}: {}", plugin.name(), image_path, idx, w);
                    }
                }
            }
        }
    }

    if any_detected {
        Ok(Some(total_records))
    } else {
        Ok(None)
    }
}

/// 単独イメージに対して 1 plugin の extract のみを実行 (--print 用)。
/// インデクスには書かない。
pub fn extract_one_image_dry(
    plugin: &dyn SchemaPlugin,
    image_path: &Path,
) -> Result<Vec<ExtractResult>> {
    let id = compute_image_id(image_path)?;
    let img_str = image_path.to_string_lossy().to_string();
    let opened = OpenedImage::open(image_path)?;
    let mut out = Vec::new();
    match opened {
        OpenedImage::Floppy(img) => {
            let fs = Filesystem::open(&img)?;
            let ctx = ExtractContext {
                image_path: &img_str,
                image_id: &id,
                partition: 0,
                fs: &fs,
            };
            if !plugin.detect(&ctx) {
                return Err(anyhow!(
                    "Plugin {} did not detect this image as in-scope",
                    plugin.name()
                ));
            }
            out.push(plugin.extract(&ctx)?);
        }
        OpenedImage::Hdd(hdd) => {
            for idx in 0..hdd.partitions().len() {
                let part = match hdd.partition(idx) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let boot = match part.read_sector(0) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let bpb = match Bpb::parse_hdd(boot) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let fs = match Filesystem::open_with_bpb(&part, bpb) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let ctx = ExtractContext {
                    image_path: &img_str,
                    image_id: &id,
                    partition: idx,
                    fs: &fs,
                };
                if plugin.detect(&ctx) {
                    out.push(plugin.extract(&ctx)?);
                }
            }
            if out.is_empty() {
                return Err(anyhow!(
                    "Plugin {} did not detect any partition as in-scope",
                    plugin.name()
                ));
            }
        }
    }
    Ok(out)
}

fn open_index_for_extract(index_dir: &Path, schema: &ArchiveSchema) -> Result<Index> {
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("Cannot create index dir {:?}", index_dir))?;
    let index = if index_dir.join("meta.json").exists() {
        let idx = Index::open_in_dir(index_dir)
            .with_context(|| format!("Cannot open index at {:?}", index_dir))?;
        let stored = idx.schema();
        if !ArchiveSchema::has_structured_fields(&stored) {
            return Err(anyhow!(
                "Index at {:?} was built with an older schema (missing 'category'/'payload'/etc.). \
                 Rebuild it first: `xdf-index build --fresh <archive_dir> --out {:?}`",
                index_dir,
                index_dir
            ));
        }
        idx
    } else {
        Index::create_in_dir(index_dir, schema.schema.clone())?
    };
    Ok(index)
}

/// 既に Tantivy 索引に入っている `(plugin, version) → {image_id...}` を集める
fn collect_extracted_keys(
    index: &Index,
    schema: &ArchiveSchema,
) -> Result<BTreeMap<(String, u64), std::collections::HashSet<String>>> {
    use std::collections::HashSet;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut out: BTreeMap<(String, u64), HashSet<String>> = BTreeMap::new();
    for sr in searcher.segment_readers() {
        let store = sr.get_store_reader(50)?;
        let max = sr.max_doc();
        for doc_id in 0..max {
            let doc: TantivyDocument = match store.get(doc_id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let plugin = match doc.get_first(schema.schema_plugin).and_then(value_as_str) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let version = doc
                .get_first(schema.schema_version)
                .and_then(value_as_u64)
                .unwrap_or(0);
            let image_id = match doc.get_first(schema.image_id).and_then(value_as_str) {
                Some(s) => s.to_string(),
                None => continue,
            };
            out.entry((plugin, version)).or_default().insert(image_id);
        }
    }
    Ok(out)
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

    fn sample_record() -> ExtractedRecord {
        ExtractedRecord {
            unique_id: "dennou:9dc9b31f:MUSIC/SONGA.DOC".into(),
            category: "music".into(),
            source_path: "MUSIC/SONGA.DOC".into(),
            image_path: "/archive/Dennou094A.img".into(),
            image_id: "9dc9b31fab1c2d3e".into(),
            partition: 0,
            issue_no: 94,
            issue_kind: "main".into(),
            issue_role: "A".into(),
            location_facet: "/dennou/main/094".into(),
            payload: serde_json::json!({
                "title": "SAMPLE SONG TITLE",
                "composer": "山田 太郎",
                "submitter": "田中花子",
            }),
            schema_plugin: "dennou".into(),
            schema_version: 1,
        }
    }

    #[test]
    fn record_to_doc_round_trip() {
        let schema = ArchiveSchema::build();
        let rec = sample_record();
        let doc = record_to_doc(&schema, &rec);
        // 主要フィールドが入っていることを確認
        assert!(doc.get_first(schema.unique_id).is_some());
        assert!(doc.get_first(schema.category).is_some());
        assert!(doc.get_first(schema.issue_no).is_some());
        assert!(doc.get_first(schema.payload).is_some());
        assert!(doc.get_first(schema.location_facet).is_some());
    }

    #[test]
    fn json_value_owned_supports_nested() {
        let v = serde_json::json!({
            "title": "SONGA",
            "files": {
                "score": "MUSIC/SONGA.ZMS",
                "doc": "MUSIC/SONGA.DOC",
            },
            "n": 94,
            "is_original": false,
            "tags": ["JPOP", "ARRANGE"],
        });
        let owned = json_value_to_owned(&v);
        match owned {
            tantivy::schema::OwnedValue::Object(_) => {}
            _ => panic!("expected object"),
        }
    }
}
