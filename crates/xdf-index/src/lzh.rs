//! LZH (LHA) アーカイブの内部メンバーを索引するためのヘルパ
//!
//! - `delharc` で展開
//! - X68000 当時のファイル名は Shift-JIS なので、UTF-8 → SJIS の順にデコード試行
//! - パスは `outer.lzh!/inner/file.zms` の形式 (JAR 慣習)
//! - 再帰深度・サイズ上限あり (LZHボム対策)

use crate::document::{
    attr_str_from_dir_flag, build_document, decode_sjis_lossy, make_excerpt, DocFields,
    EXCERPT_BYTES,
};
use crate::schema::{is_text_extension, ArchiveSchema};
use anyhow::{anyhow, Result};
use delharc::header::{LhaHeader, TimestampResult};
use delharc::LhaDecodeReader;
use std::io::{Cursor, Read};
use tantivy::IndexWriter;

/// LZH 展開オプション
#[derive(Debug, Clone)]
pub struct LzhOpts {
    /// 再帰深度の最大値 (1 = LZH内部の直接メンバーのみ、2 = LZH内のLZH まで)
    pub max_depth: usize,
    /// メンバー1個の展開上限バイト (これより大きいメンバーは skip)
    pub max_member_size: u64,
    /// アーカイブ1個あたり展開できる総バイト (これを超えたら以降のメンバーを打ち切り)
    pub max_total_expansion: u64,
}

impl Default for LzhOpts {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_member_size: 50 * 1024 * 1024,           // 50 MB
            max_total_expansion: 500 * 1024 * 1024,      // 500 MB
        }
    }
}

/// LZH バイト列を展開し、メンバーを tantivy に追加する。
/// 戻り値: 追加したドキュメント数。
pub fn index_lzh_bytes(
    schema: &ArchiveSchema,
    writer: &mut IndexWriter,
    bytes: &[u8],
    image_path: &str,
    image_id: &str,
    partition: usize,
    outer_file_path: &str, // 外側ファイルのパス (例: "/MUSIC/SONGS.LZH")
    depth_remaining: usize,
    opts: &LzhOpts,
) -> Result<usize> {
    if depth_remaining == 0 {
        return Ok(0);
    }
    let mut count = 0usize;
    let mut total_expanded: u64 = 0;
    let cursor = Cursor::new(bytes);
    // delharc の LhaDecodeError<R> は R に対して invariant なので、
    // 文字列化して anyhow::Error に変換する (借用が外に漏れるのを防ぐ)
    let mut reader = LhaDecodeReader::new(cursor)
        .map_err(|e| anyhow!("LZH header parse failed in {}: {}", outer_file_path, e))?;

    loop {
        let header_clone = reader.header().clone();
        let header = &header_clone;
        if header.is_directory() {
            // ディレクトリエントリは index しない (子ファイルから path 推測できる)
        } else {
            let inner_name = decode_lzh_filename_from_header(header);
            let inner_size: u64 = header.original_size;

            if inner_size > opts.max_member_size {
                eprintln!(
                    "  lzh skip (too large): {}!/{} ({} B)",
                    outer_file_path, inner_name, inner_size
                );
            } else if total_expanded.saturating_add(inner_size) > opts.max_total_expansion {
                eprintln!(
                    "  lzh skip (expansion budget exhausted): {}!/{}",
                    outer_file_path, inner_name
                );
            } else if !reader.is_decoder_supported() {
                eprintln!(
                    "  lzh skip (unsupported compression): {}!/{}",
                    outer_file_path, inner_name
                );
            } else {
                let mut data = Vec::with_capacity(inner_size as usize);
                if let Err(e) = reader.read_to_end(&mut data) {
                    eprintln!(
                        "  lzh skip (decode error): {}!/{}: {}",
                        outer_file_path, inner_name, e
                    );
                } else {
                    total_expanded = total_expanded.saturating_add(data.len() as u64);
                    count += add_member_document(
                        schema,
                        writer,
                        &data,
                        image_path,
                        image_id,
                        partition,
                        outer_file_path,
                        &inner_name,
                        header_mtime_unix(header),
                    )?;

                    // ネスト LZH の再帰
                    if is_lzh_member(&inner_name) && depth_remaining > 1 {
                        let nested_outer = format!("{}!/{}", outer_file_path, inner_name);
                        match index_lzh_bytes(
                            schema,
                            writer,
                            &data,
                            image_path,
                            image_id,
                            partition,
                            &nested_outer,
                            depth_remaining - 1,
                            opts,
                        ) {
                            Ok(n) => count += n,
                            Err(e) => eprintln!("  nested lzh failed: {}: {}", nested_outer, e),
                        }
                    }
                }
            }
        }
        // 次のヘッダへ
        match reader.next_file() {
            Ok(true) => continue,
            Ok(false) => break,
            Err(e) => {
                eprintln!("  lzh next_file error: {}", e);
                break;
            }
        }
    }
    Ok(count)
}

/// メンバーバイトを 1 ドキュメントとして追加 (テキスト系のみ本文索引)
#[allow(clippy::too_many_arguments)]
fn add_member_document(
    schema: &ArchiveSchema,
    writer: &mut IndexWriter,
    data: &[u8],
    image_path: &str,
    image_id: &str,
    partition: usize,
    outer_file_path: &str,
    inner_name: &str,
    mtime_unix: i64,
) -> Result<usize> {
    let combined_path = format!("{}!/{}", outer_file_path, inner_name);
    let display_name = inner_name
        .rsplit('/')
        .next()
        .unwrap_or(inner_name)
        .to_string();
    let ext = display_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let attr = attr_str_from_dir_flag(false);

    let (body_text, excerpt_text);
    if is_text_extension(&ext) {
        let text = decode_sjis_lossy(data);
        let excerpt = make_excerpt(&text, EXCERPT_BYTES);
        body_text = Some(text);
        excerpt_text = Some(excerpt);
    } else {
        body_text = None;
        excerpt_text = None;
    }

    let doc = build_document(
        schema,
        DocFields {
            image_path,
            image_id,
            partition,
            file_path: &combined_path,
            file_name: &display_name,
            ext: &ext,
            size: data.len() as u64,
            mtime_unix,
            attr: &attr,
            body: body_text.as_deref(),
            excerpt: excerpt_text.as_deref(),
        },
    );
    writer.add_document(doc)?;
    Ok(1)
}

/// LZH ヘッダからファイル名 (UTF-8 文字列) を取得。
/// `parse_pathname()` は ISO-8859-1 やヘッダ指定エンコーディングで返すため、
/// X68000 LZH (SJIS) ではバイト列を経由して再デコードする必要がある。
fn decode_lzh_filename_from_header(header: &LhaHeader) -> String {
    // 第一手: parse_pathname がそれっぽい文字列を返すならそれ
    let parsed = header.parse_pathname();
    let s = parsed.to_string_lossy().to_string();
    // 化けたかどうかの簡易判定: 多バイト相当が含まれず制御文字が多いなら SJIS 再デコードを試す
    if s.bytes().any(|b| b >= 0x80) || s.is_empty() {
        // それなりに非ASCIIが残っているなら、parse_pathname の戻りがそのまま使えそう
        return s;
    }
    // それ以外は raw バイト列 (filename フィールド) から SJIS デコード
    decode_sjis_lossy(&header.filename)
}

/// メンバーが LZH かどうか (拡張子チェック)
fn is_lzh_member(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".lzh") || lower.ends_with(".lha")
}

/// LZH ヘッダの mtime を Unix epoch に変換 (失敗時は 0)
fn header_mtime_unix(header: &LhaHeader) -> i64 {
    match header.parse_last_modified() {
        TimestampResult::Utc(dt) => dt.timestamp(),
        TimestampResult::Naive(naive) => naive.and_utc().timestamp(),
        TimestampResult::None => 0,
    }
}

/// 多段 LZH パスに対応したメンバー取り出し。
///
/// `inner_path` に `!/` が含まれる場合 (例: `"INNER.LZH!/file.txt"`)、
/// 外側から順に展開していく。索引側 (`index_lzh_bytes`) は `LzhOpts::max_depth`
/// の既定値 2 で「LZH の中の LZH」まで展開するため、読み出し側も同じ深さまで
/// 辿れないと**索引には載っているのに読めないドキュメント**が生まれる。
///
/// `max_depth` は LZH の入れ子の総数 (1 = 入れ子なし、2 = LZH 内の LZH まで)。
/// 索引側と同じ値を渡すこと。
pub fn extract_member_nested(bytes: &[u8], inner_path: &str, max_depth: usize) -> Result<Vec<u8>> {
    let segments: Vec<&str> = inner_path.split("!/").collect();
    if segments.len() > max_depth {
        return Err(anyhow!(
            "LZH nesting too deep: {} levels requested, limit is {}",
            segments.len(),
            max_depth
        ));
    }

    // 1 段目は借用のまま、2 段目以降は展開結果を持ち回る
    let mut current: Vec<u8> = extract_member(bytes, segments[0])?;
    for seg in &segments[1..] {
        current = extract_member(&current, seg)?;
    }
    Ok(current)
}

/// LZH バイト列から指定メンバーを取り出す。
/// `inner_path` は LZH 内部のフルパス (例: "SHOKA1.ZMS" や "subdir/file.zms")。
/// 大文字小文字無視マッチで探索。
///
/// 多段 (`a.lzh!/b.lzh!/file`) を扱う場合は [`extract_member_nested`] を使うこと。
pub fn extract_member(bytes: &[u8], inner_path: &str) -> Result<Vec<u8>> {
    let cursor = Cursor::new(bytes);
    let mut reader = LhaDecodeReader::new(cursor)
        .map_err(|e| anyhow!("LZH header parse failed: {}", e))?;
    let target = inner_path.to_ascii_lowercase();
    let target = target.trim_start_matches('/').to_string();

    loop {
        let header_clone = reader.header().clone();
        let header = &header_clone;
        if !header.is_directory() {
            let name = decode_lzh_filename_from_header(header);
            let name_norm = name.to_ascii_lowercase();
            if name_norm == target || name_norm.trim_start_matches('/') == target {
                if !reader.is_decoder_supported() {
                    return Err(anyhow!("Unsupported compression for: {}", name));
                }
                let mut data = Vec::with_capacity(header.original_size as usize);
                reader
                    .read_to_end(&mut data)
                    .map_err(|e| anyhow!("LZH decode error for {}: {}", name, e))?;
                return Ok(data);
            }
        }
        match reader.next_file() {
            Ok(true) => continue,
            Ok(false) => break,
            Err(e) => return Err(anyhow!("LZH next_file error: {}", e)),
        }
    }
    Err(anyhow!("Member not found in LZH: {}", inner_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 深度上限を超えるパスは、展開を試みる前にエラーにする。
    /// (LZH ボム対策の上限が効いていることの確認)
    #[test]
    fn nested_extract_rejects_paths_deeper_than_limit() {
        // 3 セグメント = 3 段。既定の 2 を超えるので弾かれる
        let err = extract_member_nested(b"dummy", "a.lzh!/b.lzh!/c.txt", 2).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nesting too deep"),
            "深度超過のエラーが返るべき: {msg}"
        );

        // 上限を上げれば深度チェックは通過し、実際の展開処理まで進む
        // (バイト列が LZH ではないので別のエラーになる = 深度では弾かれていない)
        let err = extract_member_nested(b"dummy", "a.lzh!/b.lzh!/c.txt", 3).unwrap_err();
        assert!(
            !err.to_string().contains("nesting too deep"),
            "上限内なら深度エラーにはならないはず: {err}"
        );
    }

    /// 単段のパスは深度 1 でも通る (= 深度チェックのオフバイワンが無いこと)
    #[test]
    fn nested_extract_allows_single_segment_at_depth_one() {
        let err = extract_member_nested(b"dummy", "file.txt", 1).unwrap_err();
        assert!(
            !err.to_string().contains("nesting too deep"),
            "単段は深度 1 で通るべき: {err}"
        );
    }

    /// 索引側の既定深度と、読み出し側が想定する深度が揃っていること。
    /// ここがずれると「索引には載っているのに読めない」ドキュメントが生まれる。
    #[test]
    fn index_default_depth_is_two() {
        assert_eq!(
            LzhOpts::default().max_depth,
            2,
            "既定深度を変えるなら xdf-mcp の LZH_MAX_DEPTH も合わせること"
        );
    }
}
