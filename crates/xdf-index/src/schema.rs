//! tantivy スキーマ定義
//!
//! ドキュメント1件は次のいずれか:
//! - **ファイル本文ドキュメント** (既存): 1 image 内の 1 ファイル。`category` は空文字列。
//! - **構造化メタデータドキュメント** (Phase 5b): 雑誌等から抽出した1レコード
//!   (例: `category="music"` の楽曲1件)。`unique_id`, `payload` JSON, `location_facet` を持つ。
//!
//! 両者は同一スキーマ・同一インデクス内で共存する。`archive_search` は body + file_name を
//! クエリパーサに渡しているため、構造化ドキュメントは（payload が TEXT 索引対象でも）
//! 既存検索の挙動を変えない。
//!
//! 設計詳細は `docs/additional_spec/design.md`「Tantivy スキーマ拡張」参照。

use crate::analyzer::NGRAM_TOKENIZER;
use tantivy::schema::{
    FacetOptions, IndexRecordOption, JsonObjectOptions, Schema, TextFieldIndexing, TextOptions,
    FAST, INDEXED, STORED, STRING,
};

/// 日本語を索引できるテキストフィールドの設定。
///
/// `WithFreqsAndPositions` は必須。bigram のフレーズ照合が位置に依存しており、
/// 位置が無いフィールドを検索対象にすると `QueryParser` が
/// `FieldDoesNotHavePositionsIndexed` で失敗する。
fn ja_text_options(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_index_option(IndexRecordOption::WithFreqsAndPositions)
            .set_tokenizer(tokenizer),
    )
}

/// 索引のフィールド集合をまとめた構造体
pub struct ArchiveSchema {
    pub schema: Schema,
    pub image_path: tantivy::schema::Field,
    pub image_id: tantivy::schema::Field,
    pub partition: tantivy::schema::Field,
    pub file_path: tantivy::schema::Field,
    pub file_name: tantivy::schema::Field,
    pub ext: tantivy::schema::Field,
    pub size: tantivy::schema::Field,
    pub mtime: tantivy::schema::Field,
    pub attr: tantivy::schema::Field,
    /// 本文。文字 bigram で索引し、**STORED** で本文そのものも持つ
    pub body_ngram: tantivy::schema::Field,

    // --- Phase 5b: 構造化メタデータ ---
    /// 例: "music" / "gallery" / "tool" / "" (= 構造化レコードでないファイル本文)
    pub category: tantivy::schema::Field,
    /// 号番号 (例: 94)
    pub issue_no: tantivy::schema::Field,
    /// "main" (本誌) / "extra" (別冊) / "" (該当なし)
    pub issue_kind: tantivy::schema::Field,
    /// "A" / "B" / "X" (多面ディスク識別)、または ""
    pub issue_role: tantivy::schema::Field,
    /// 再抽出用の delete_term キー (例: "dennou:9dc9b31f:MUSIC/SONGA.DOC")
    pub unique_id: tantivy::schema::Field,
    /// 抽出に使ったプラグイン名 (例: "dennou")
    pub schema_plugin: tantivy::schema::Field,
    /// プラグインのバージョン (上げると再抽出対象になる)
    pub schema_version: tantivy::schema::Field,
    /// カテゴリ依存の構造化データ (JSON)
    pub payload: tantivy::schema::Field,
    /// 階層ファセット (例: "/dennou/main/094")
    pub location_facet: tantivy::schema::Field,
}

impl ArchiveSchema {
    pub fn build() -> Self {
        let mut sb = Schema::builder();
        // メタフィールド
        let image_path = sb.add_text_field("image_path", STRING | STORED);
        let image_id = sb.add_text_field("image_id", STRING | STORED);
        let partition = sb.add_u64_field("partition", INDEXED | STORED | FAST);
        let file_path = sb.add_text_field("file_path", STRING | STORED);
        // 半角カナのファイル名が多いので、NFKC を本文と揃えないと
        // クエリ側だけ正規化されて「今より引きにくくなる」。
        let file_name =
            sb.add_text_field("file_name", ja_text_options(NGRAM_TOKENIZER).set_stored());
        let ext = sb.add_text_field("ext", STRING | STORED | FAST);
        let size = sb.add_u64_field("size", INDEXED | STORED | FAST);
        let mtime = sb.add_i64_field("mtime", INDEXED | STORED | FAST);
        let attr = sb.add_text_field("attr", STRING | STORED);
        // 本文 (テキスト系のみ)。旧 `body` は既定アナライザで日本語を取りこぼして
        // いたので、名前を変えて別フィールドにしてある。読み出し側は名前で引くため、
        // 旧索引は `body` に落ちて従来どおり動く (searcher.rs の ReadFields)。
        // **STORED にしている理由**は 3 つ。
        //   1. アナライザを変えるたびにアーカイブ全体を読み直す必要がなくなる
        //      (索引自身から再索引できる)。再構築の回数を削るより、再構築を安くする
        //   2. ヒット箇所まわりの抜粋を出せる。旧 `body_excerpt` はファイル先頭
        //      2048 バイト固定で、マッチ位置とは無関係だった
        //   3. 精度の自動評価ができる (ヒットが本当にその文字列を含むかの照合)
        // 実測コスト: 約 1 万ファイルの索引で 1.8 倍。ビルド時間と検索時間は変わらない。
        // 旧 `body_excerpt` は本文の先頭 2048 バイトそのものなので**完全な重複**になり、
        // 廃止した。抜粋は読み出し時に本文から切り出す (searcher.rs)。
        let body_ngram =
            sb.add_text_field("body_ngram", ja_text_options(NGRAM_TOKENIZER).set_stored());

        // --- 構造化メタデータ (Phase 5b) ---
        let category = sb.add_text_field("category", STRING | STORED | FAST);
        let issue_no = sb.add_u64_field("issue_no", INDEXED | STORED | FAST);
        let issue_kind = sb.add_text_field("issue_kind", STRING | STORED | FAST);
        let issue_role = sb.add_text_field("issue_role", STRING | STORED | FAST);
        let unique_id = sb.add_text_field("unique_id", STRING | STORED);
        let schema_plugin = sb.add_text_field("schema_plugin", STRING | STORED | FAST);
        let schema_version = sb.add_u64_field("schema_version", STORED | FAST);
        // payload: JSON。本文と同じアナライザで indexed かつ stored。
        // 40 バイトを超える曲名が既定アナライザでは同じバグで落ちていた。
        let payload_opts = JsonObjectOptions::default()
            .set_stored()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions)
                    .set_tokenizer(NGRAM_TOKENIZER),
            );
        let payload = sb.add_json_field("payload", payload_opts);
        let location_facet = sb.add_facet_field("location_facet", FacetOptions::default());

        Self {
            schema: sb.build(),
            image_path,
            image_id,
            partition,
            file_path,
            file_name,
            ext,
            size,
            mtime,
            attr,
            body_ngram,
            category,
            issue_no,
            issue_kind,
            issue_role,
            unique_id,
            schema_plugin,
            schema_version,
            payload,
            location_facet,
        }
    }

    /// 既存索引のスキーマに構造化フィールドが含まれているかを判定。
    /// 旧 (Phase 4 まで) で作られた索引には含まれず、抽出は失敗する。
    pub fn has_structured_fields(s: &Schema) -> bool {
        s.get_field("category").is_ok()
            && s.get_field("issue_no").is_ok()
            && s.get_field("payload").is_ok()
            && s.get_field("unique_id").is_ok()
            && s.get_field("location_facet").is_ok()
    }
}

/// 本文索引対象とする拡張子のデフォルト集合 (大文字統一)
pub const DEFAULT_TEXT_EXTENSIONS: &[&str] = &[
    "DOC", "TXT", "ZMS", "MDD", "BAS", "X", "S", "C", "H",
    "MAC", "INC", "ASM", "BAT", "MD", "INI", "CFG",
];

/// 拡張子 (ASCII大文字に正規化済み) がテキスト系か
pub fn is_text_extension(ext: &str) -> bool {
    DEFAULT_TEXT_EXTENSIONS.contains(&ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_builds_and_has_all_fields() {
        let s = ArchiveSchema::build();
        // 各フィールドが取得できることだけ確認 (panic しない)
        let _ = s.schema.get_field("image_id").unwrap();
        let _ = s.schema.get_field("partition").unwrap();
        let _ = s.schema.get_field("file_path").unwrap();
        let _ = s.schema.get_field("body_ngram").unwrap();
        assert!(
            s.schema.get_field("body_excerpt").is_err(),
            "body_excerpt は廃止した (本文が STORED なので完全な重複だった)"
        );
        // 構造化フィールド (Phase 5b)
        let _ = s.schema.get_field("category").unwrap();
        let _ = s.schema.get_field("issue_no").unwrap();
        let _ = s.schema.get_field("payload").unwrap();
        let _ = s.schema.get_field("location_facet").unwrap();
        let _ = s.schema.get_field("unique_id").unwrap();
        assert!(ArchiveSchema::has_structured_fields(&s.schema));
    }

    #[test]
    fn text_extensions_recognized() {
        assert!(is_text_extension("DOC"));
        assert!(is_text_extension("ZMS"));
        assert!(is_text_extension("BAS"));
        assert!(!is_text_extension("X8")); // 実行ファイルは除外
        assert!(!is_text_extension("PIC")); // 画像は除外
    }
}
