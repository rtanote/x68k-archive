//! ファイルシステムエントリ → tantivy `TantivyDocument` 変換 (T-3.2)

use crate::schema::{is_text_extension, ArchiveSchema};
use encoding_rs::SHIFT_JIS;
use tantivy::TantivyDocument;
use xdf_fs::direntry::{Attr, DirEntry};
use xdf_fs::export::entry_to_filetime;
use xdf_fs::fs::Filesystem;

/// 検索結果に出す抜粋の最大バイト数。
///
/// 以前は `body_excerpt` として索引に持っていたが、本文が STORED に
/// なったので**読み出し時に切り出す**ようにした (searcher.rs)。
pub const EXCERPT_BYTES: usize = 2048;

/// SJIS バイト列を UTF-8 に変換 (lossy)
pub fn decode_sjis_lossy(bytes: &[u8]) -> String {
    let (cow, _enc, _had_errors) = SHIFT_JIS.decode(bytes);
    cow.into_owned()
}

/// テキストの先頭から最大 max_bytes バイトを切り出す (UTF-8 文字境界尊重)
pub fn make_excerpt(text: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// 属性ビットを 7 文字の文字列に
pub fn attr_str(a: Attr) -> String {
    let mut s = String::with_capacity(7);
    s.push(if a.contains(Attr::DIRECTORY) { 'D' } else { '-' });
    s.push(if a.contains(Attr::READ_ONLY) { 'R' } else { '-' });
    s.push(if a.contains(Attr::HIDDEN) { 'H' } else { '-' });
    s.push(if a.contains(Attr::SYSTEM) { 'S' } else { '-' });
    s.push(if a.contains(Attr::VOLUME) { 'V' } else { '-' });
    s.push(if a.contains(Attr::ARCHIVE) { 'A' } else { '-' });
    s.push(if a.contains(Attr::LINK) { 'L' } else { '-' });
    s
}

/// LZH メンバー等、X68000 属性ビットを持たないファイル用の汎用属性文字列
pub fn attr_str_from_dir_flag(is_dir: bool) -> String {
    let d = if is_dir { 'D' } else { '-' };
    format!("{}-----A-", d)
}

/// FAT date/time → Unix epoch 秒。失敗時は 0
pub fn mtime_to_unix(entry: &DirEntry) -> i64 {
    entry_to_filetime(entry).map(|ft| ft.unix_seconds()).unwrap_or(0)
}

/// テキスト系拡張子なら本文 (UTF-8) と先頭抜粋を返す。
/// バイナリ or 読み出し失敗なら None。
pub fn extract_text_body(
    fs: &Filesystem,
    entry: &DirEntry,
) -> Option<String> {
    let ext_upper = entry.ext.to_ascii_uppercase();
    if !is_text_extension(&ext_upper) {
        return None;
    }
    let bytes = fs.read_file(entry).ok()?;
    Some(decode_sjis_lossy(&bytes))
}

/// tantivy ドキュメントの生フィールド (LZH メンバーのような DirEntry を持たないものも統一して扱う)
pub struct DocFields<'a> {
    pub image_path: &'a str,
    pub image_id: &'a str,
    pub partition: usize,
    pub file_path: &'a str,
    pub file_name: &'a str,
    /// 大文字統一済みの拡張子
    pub ext: &'a str,
    pub size: u64,
    pub mtime_unix: i64,
    /// 7 文字の属性文字列 (例: "----A--")
    pub attr: &'a str,
    pub body: Option<&'a str>,
}

/// 1 ファイル = 1 tantivy ドキュメント
pub fn build_document(schema: &ArchiveSchema, fields: DocFields<'_>) -> TantivyDocument {
    let mut doc = TantivyDocument::default();
    doc.add_text(schema.image_path, fields.image_path);
    doc.add_text(schema.image_id, fields.image_id);
    doc.add_u64(schema.partition, fields.partition as u64);
    doc.add_text(schema.file_path, fields.file_path);
    doc.add_text(schema.file_name, fields.file_name);
    doc.add_text(schema.ext, fields.ext);
    doc.add_u64(schema.size, fields.size);
    doc.add_i64(schema.mtime, fields.mtime_unix);
    doc.add_text(schema.attr, fields.attr);
    if let Some(b) = fields.body {
        // STORED なので、この 1 回で索引用のトークンと本文そのものの両方が入る。
        doc.add_text(schema.body_ngram, b);
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_respects_char_boundary() {
        // 先頭付近に多バイト文字 (3B) があるとき、切断位置がずれる
        let s = "abc\u{3042}defg"; // "abcあdefg" (a,b,cが3B + 'あ'=3B + d,e,f,g)
        // bytes = a(1) b(1) c(1) あ(3) d(1) e(1) f(1) g(1) = 10 bytes
        // max_bytes = 5 はちょうど 'あ' の途中。境界尊重で 3 まで戻る。
        let cut = make_excerpt(s, 5);
        assert_eq!(cut, "abc");
    }

    #[test]
    fn excerpt_full_when_short() {
        assert_eq!(make_excerpt("hello", 100), "hello");
    }

    #[test]
    fn sjis_decode_basic_ascii() {
        let bytes = b"Hello, World!";
        assert_eq!(decode_sjis_lossy(bytes), "Hello, World!");
    }

    #[test]
    fn sjis_decode_japanese() {
        // SJIS for 'こんにちは' = 82 B1 82 F1 82 C9 82 BF 82 CD
        let bytes = b"\x82\xB1\x82\xF1\x82\xC9\x82\xBF\x82\xCD";
        assert_eq!(decode_sjis_lossy(bytes), "こんにちは");
    }
}
