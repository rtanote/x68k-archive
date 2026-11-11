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

/// 索引のパス (`target`) と LZH メンバー名の照合結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameMatch {
    /// パスが丸ごと一致した。paths v2 の索引で起きる。
    Full,
    /// ベース名だけ一致した。paths v1 の索引 (ディレクトリ抜き) で起きる。
    /// 同名が複数あると取り違えうるので、呼び出し側は警告を出す。
    BaseOnly,
    No,
}

/// 照合を関数に出してあるのは、実アーカイブを用意せずに検査できるようにする
/// ため。`LhaHeader` は外から組み立てられないので、ヘッダ経由では検査できない。
fn match_member_name(name_norm: &str, target: &str) -> NameMatch {
    if name_norm == target || name_norm.trim_start_matches('/') == target {
        return NameMatch::Full;
    }
    // ディレクトリを持つ名前に限る。持たない名前は上で判定が終わっていて、
    // ここに来るのは「別のファイル」なので、ベース名を見る意味がない。
    if name_norm.contains('/') && name_norm.rsplit('/').next() == Some(target) {
        return NameMatch::BaseOnly;
    }
    NameMatch::No
}

/// どのメンバーを返すかの規則。走査しながら使える形にしてある。
///
/// 規則は 2 つだけ。
///
/// 1. **完全一致が出たらその場で確定。** アーカイブの残りは見ない。後ろに
///    壊れたヘッダがあっても、手前の正しいメンバーは読めるようにするため。
///    索引側も壊れた手前までは採っているので、読み出しもそれに揃える。
/// 2. **完全一致が無いときだけ、最初のベース名候補。** 候補は走査中に覚えて
///    おき、完全一致が無いと確定してから使う。1 周で早押しすると、
///    `src/foo.c` が root の `foo.c` より前にあるとき `foo.c` の要求に
///    ベース名一致を返してしまう（作り直した索引でも間違う）。
///
/// 実装をこの型だけに置いているのは、[`extract_member`] の走査と検査の両方が
/// 同じ規則を使うようにするため。2 箇所に書くとずれる。
#[derive(Default)]
struct Pick {
    base_only: Option<usize>,
}

impl Pick {
    /// このメンバーで確定するなら `true`。`false` なら走査を続ける。
    fn offer(&mut self, index: usize, m: NameMatch) -> bool {
        match m {
            NameMatch::Full => true,
            NameMatch::BaseOnly if self.base_only.is_none() => {
                self.base_only = Some(index);
                false
            }
            _ => false,
        }
    }

    /// 走査が終わった（または打ち切られた）ときの答え。
    fn settle(&self) -> Option<usize> {
        self.base_only
    }
}

/// 名前の一覧に対して [`Pick`] の規則を適用する。検査用。
fn choose_member(names: &[String], target: &str) -> Option<(usize, NameMatch)> {
    let mut pick = Pick::default();
    for (i, n) in names.iter().enumerate() {
        if pick.offer(i, match_member_name(n, target)) {
            return Some((i, NameMatch::Full));
        }
    }
    pick.settle().map(|i| (i, NameMatch::BaseOnly))
}

/// ベース名での後退に入ったことを、プロセスにつき一度だけ伝える。
///
/// 毎回出すと、索引全体を走るような使い方でログが埋まる。知りたいのは
/// 「この索引は作り直しが必要」という一点なので、一度で足りる。
fn warn_stale_index_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "warning: LZH メンバーをベース名で引き当てました。索引が \
             paths v1 (ディレクトリ抜き) で作られている可能性があります。\n\
             同名ファイルが複数ある場合、別のファイルの内容が返ることがあります。\n\
             `xdf-index build --fresh` で作り直してください。"
        );
    }
}

/// LZH ヘッダからファイル名 (UTF-8 文字列) を取得。
///
/// **ディレクトリ部分と SJIS の両方を保つ必要がある。**
///
/// レベル 1 以上のヘッダでは、`header.filename` に入るのは**ベース名だけ**で、
/// ディレクトリは拡張ヘッダ側にある (レベル 2/3 では `filename` は常に空)。
/// 一方 `parse_pathname()` はディレクトリを含む完全なパスを組み立ててくれるが、
/// 非 ASCII バイトを `%8e%f1` のようにパーセントエンコードして返す
/// (delharc の `parse_str_nilterm`)。X68000 の LZH はファイル名が SJIS なので、
/// そのままでは読めない。
///
/// そこで `parse_pathname_to_str()` の結果をパーセントデコードしてバイト列に戻し、
/// それを SJIS としてデコードする。`/` や `.` は ASCII なのでエンコードされず、
/// ディレクトリ構造は保たれる。
///
/// 以前は `header.filename` を直接 SJIS デコードしていたため、
/// **ディレクトリが常に失われ、サブディレクトリにある同名ファイルが
/// 同一パスに潰れていた** (読み出しは先頭の 1 件しか返せず、別ファイルの
/// 内容が返る状態だった)。
fn decode_lzh_filename_from_header(header: &LhaHeader) -> String {
    let base_raw: &[u8] = &header.filename;

    // delharc がパス分割に使うバイト。SJIS の 2 バイト目にも現れうる (ダメ文字)。
    let splits_on = |b: &u8| matches!(*b, 0xFF | b'/' | b'\\');

    // レベル 2/3 では filename が空でディレクトリ込みの名前が拡張ヘッダにしかない。
    // またベース名自体が分割対象バイトを含む場合 (能=94 5C, 表=95 5C など) は、
    // delharc が**文字の途中で分割してバイトを捨てている**ため復元できない。
    // どちらの場合も従来どおりベース名のみを返し、退行させない。
    if base_raw.is_empty() || base_raw.iter().any(splits_on) {
        return decode_sjis_lossy(base_raw);
    }

    // ここまで来れば base_raw は分割されていないので、
    // parse_pathname_to_str() の末尾セグメントがベース名に対応する。
    // その手前がディレクトリ。
    //
    // ディレクトリを拾う目的は、LZH 内のサブディレクトリにある同名ファイルが
    // 同一パスに潰れて読み出しが別ファイルを返すのを防ぐこと。
    let encoded = header.parse_pathname_to_str();
    let base = decode_sjis_lossy(base_raw);
    match encoded.rfind('/') {
        Some(i) => {
            let dir = decode_sjis_lossy(&percent_decode_to_bytes(&encoded[..i]));
            if dir.is_empty() {
                base
            } else {
                format!("{dir}/{base}")
            }
        }
        None => base,
    }
}

/// `parse_str_nilterm` が作る `%xx` 表記をバイト列に戻す。
///
/// 注意: delharc は `%` 自体をエスケープしない (0x25 は ASCII 範囲なのでそのまま出る)。
/// そのため `%` の後ろが 16 進 2 桁でない場合は**エスケープではない**とみなし、
/// `%` をそのまま 1 バイトとして扱う。こうしないと `100%ab.txt` のような
/// ファイル名を壊してしまう。
fn percent_decode_to_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
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

/// ディレクトリを飛ばして数えた添字で、そのメンバーを展開する。
///
/// ベース名で後退したときにだけ使う。完全一致はその場で読むので通らない。
fn read_member_at(bytes: &[u8], want: usize) -> Result<Vec<u8>> {
    let cursor = Cursor::new(bytes);
    let mut reader = LhaDecodeReader::new(cursor)
        .map_err(|e| anyhow!("LZH header parse failed: {}", e))?;
    let mut i = 0usize;
    loop {
        let header = reader.header().clone();
        if !header.is_directory() {
            if i == want {
                let name = decode_lzh_filename_from_header(&header);
                if !reader.is_decoder_supported() {
                    return Err(anyhow!("Unsupported compression for: {}", name));
                }
                let mut data = Vec::with_capacity(header.original_size as usize);
                reader
                    .read_to_end(&mut data)
                    .map_err(|e| anyhow!("LZH decode error for {}: {}", name, e))?;
                return Ok(data);
            }
            i += 1;
        }
        match reader.next_file() {
            Ok(true) => continue,
            Ok(false) => break,
            Err(e) => return Err(anyhow!("LZH next_file error: {}", e)),
        }
    }
    Err(anyhow!("LZH member index {} vanished between passes", want))
}

/// LZH バイト列から指定メンバーを取り出す。
/// `inner_path` は LZH 内部のフルパス (例: "SHOKA1.ZMS" や "subdir/file.zms")。
/// 大文字小文字無視マッチで探索。
///
/// 多段 (`a.lzh!/b.lzh!/file`) を扱う場合は [`extract_member_nested`] を使うこと。
pub fn extract_member(bytes: &[u8], inner_path: &str) -> Result<Vec<u8>> {
    let target = inner_path.to_ascii_lowercase();
    let target = target.trim_start_matches('/').to_string();

    let cursor = Cursor::new(bytes);
    let mut reader = LhaDecodeReader::new(cursor)
        .map_err(|e| anyhow!("LZH header parse failed: {}", e))?;
    let mut pick = Pick::default();
    let mut i = 0usize;

    loop {
        let header = reader.header().clone();
        if !header.is_directory() {
            let name = decode_lzh_filename_from_header(&header);
            let m = match_member_name(&name.to_ascii_lowercase(), &target);
            if pick.offer(i, m) {
                // 完全一致。ここで読んで返る。残りのヘッダは見ない。
                if !reader.is_decoder_supported() {
                    return Err(anyhow!("Unsupported compression for: {}", name));
                }
                let mut data = Vec::with_capacity(header.original_size as usize);
                reader
                    .read_to_end(&mut data)
                    .map_err(|e| anyhow!("LZH decode error for {}: {}", name, e))?;
                return Ok(data);
            }
            i += 1;
        }
        match reader.next_file() {
            Ok(true) => continue,
            Ok(false) => break,
            Err(e) => {
                // ヘッダが壊れている。ここより手前にベース名の候補があるなら、
                // それで答える。索引は壊れた手前までのメンバーを持っているので、
                // 読み出しだけ全滅させる理由がない。候補も無ければ諦める。
                if pick.settle().is_none() {
                    return Err(anyhow!("LZH next_file error: {}", e));
                }
                break;
            }
        }
    }

    // paths v1 の索引はディレクトリ抜きのパスを持っている。完全一致が無いときに
    // ベース名で拾うのは、索引を作り直していない利用者が pull した瞬間に
    // 読めなくならないようにするため。曖昧さは残るが、それは v1 索引での
    // 従来の挙動と同じで退行ではない。
    match pick.settle() {
        Some(idx) => {
            warn_stale_index_once();
            read_member_at(bytes, idx)
        }
        None => Err(anyhow!("Member not found in LZH: {}", inner_path)),
    }
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

    /// パーセントデコードが SJIS バイト列を正しく復元すること。
    /// delharc は非 ASCII を `%8e%f1` のように出すので、これを戻して
    /// SJIS デコードしないと日本語ファイル名が読めない。
    #[test]
    fn percent_decode_restores_sjis_bytes() {
        // "QUIZDATA/400_首相.QZ" の SJIS 表現
        let encoded = "QUIZDATA/400_%8e%f1%91%8a.QZ";
        let bytes = percent_decode_to_bytes(encoded);
        // SJIS の "首相" は 0x8e 0xf1 0x91 0x8a。
        // バイト列リテラルに非 ASCII を直接書くとソースの文字コードに依存するので、
        // 期待値はエスケープで組み立てる。
        let mut want = b"QUIZDATA/400_".to_vec();
        want.extend_from_slice(&[0x8e, 0xf1, 0x91, 0x8a]);
        want.extend_from_slice(b".QZ");
        assert_eq!(
            bytes, want,
            "ASCII 部分はそのまま、%xx のみバイトに戻ること"
        );
        let decoded = decode_sjis_lossy(&bytes);
        assert!(
            decoded.starts_with("QUIZDATA/400_") && decoded.ends_with(".QZ"),
            "ディレクトリと拡張子が保たれること: {decoded}"
        );
        assert!(
            decoded.contains('首') || decoded.contains('相'),
            "SJIS が日本語にデコードされること: {decoded}"
        );
    }

    /// delharc は `%` 自体をエスケープしない (0x25 は ASCII 範囲)。
    /// そのため `%` の後ろが 16 進 2 桁でなければエスケープ扱いしてはいけない。
    #[test]
    fn percent_decode_keeps_literal_percent() {
        assert_eq!(percent_decode_to_bytes("100%.txt"), b"100%.txt".to_vec());
        assert_eq!(percent_decode_to_bytes("a%zz"), b"a%zz".to_vec());
        assert_eq!(percent_decode_to_bytes("%"), b"%".to_vec());
        assert_eq!(percent_decode_to_bytes("ab%2"), b"ab%2".to_vec());
        // 一方、正しい 16 進 2 桁はバイトに戻す
        assert_eq!(percent_decode_to_bytes("a%41b"), b"aAb".to_vec());
    }

    /// ディレクトリ区切りは ASCII なのでエンコードされず、そのまま残ること。
    #[test]
    fn percent_decode_preserves_directory_separators() {
        let b = percent_decode_to_bytes("src/Makefile");
        assert_eq!(b, b"src/Makefile".to_vec());
        assert_eq!(decode_sjis_lossy(&b), "src/Makefile");
    }

    /// SJIS の「ダメ文字」を含むベース名は、ディレクトリ付与を諦めて
    /// 従来どおりベース名のみを返すこと (退行防止)。
    ///
    /// SJIS の 2 バイト文字は 2 バイト目に 0x5C (`\`) や 0x2F (`/`) を取りうる
    /// (能=94 5C, 表=95 5C, ソ=83 5C, 予=97 5C, 構=8D 5C)。delharc の
    /// `parse_pathname_to_str()` は **エンコードより先に** 0xFF / `/` / `\` で
    /// 分割するため、この 2 バイト目は捨てられて後段では復元できない。
    /// したがって「SJIS を解釈して区切りを判定し直す」方式は原理的に成立しない。
    /// 該当するベース名は検出してフォールバックするのが唯一安全な方法。
    #[test]
    fn dame_moji_base_names_fall_back_to_raw_filename() {
        // 能 = 94 5C (2 バイト目が 0x5C)
        let dame: Vec<u8> = {
            let mut v = b"566_".to_vec();
            v.extend_from_slice(&[0x8c, 0x7c]); // 芸
            v.extend_from_slice(&[0x94, 0x5c]); // 能
            v.extend_from_slice(b".QZ");
            v
        };
        assert!(
            dame.iter().any(|b| matches!(*b, 0xFF | b'/' | b'\\')),
            "このベース名は分割対象バイトを含む = フォールバック経路に入る"
        );

        // フォールバック先の挙動: 生バイトをそのまま SJIS デコードする
        let decoded = decode_sjis_lossy(&dame);
        assert!(
            !decoded.contains('/'),
            "偽のパス区切りが混入しないこと: {decoded}"
        );
        assert!(
            !decoded.contains('\u{FFFD}'),
            "ダメ文字が置換文字にならないこと: {decoded}"
        );

        // 一方、分割対象バイトを含まない名前はディレクトリ付与の対象になる
        let safe = b"Makefile";
        assert!(
            !safe.iter().any(|b| matches!(*b, 0xFF | b'/' | b'\\')),
            "ASCII のベース名はディレクトリ付与の対象"
        );
    }

    /// 索引側の既定深度と、読み出し側が想定する深度が揃っていること。
    /// ここがずれると「索引には載っているのに読めない」ドキュメントが生まれる。
    #[test]
    fn full_path_matches_exactly() {
        assert_eq!(match_member_name("src/foo.c", "src/foo.c"), NameMatch::Full);
        assert_eq!(match_member_name("foo.c", "foo.c"), NameMatch::Full);
        // 先頭のスラッシュは無視する（従来からの挙動）
        assert_eq!(match_member_name("/foo.c", "foo.c"), NameMatch::Full);
    }

    #[test]
    fn base_name_falls_back_for_v1_indexes() {
        // paths v1 の索引は "foo.c" しか持っていない。v2 のコードが
        // "src/foo.c" を作っても、読み出しが失敗しないこと。
        assert_eq!(match_member_name("src/foo.c", "foo.c"), NameMatch::BaseOnly);
        assert_eq!(
            match_member_name("a/b/deep.txt", "deep.txt"),
            NameMatch::BaseOnly
        );
    }

    #[test]
    fn exact_match_wins_even_when_it_comes_later() {
        // Copilot が見つけた並び順の罠。src/foo.c が先にあると、1 周で
        // 決める作りでは "foo.c" の要求にベース名一致を返してしまう。
        let names = vec!["src/foo.c".to_string(), "foo.c".to_string()];
        assert_eq!(
            choose_member(&names, "foo.c"),
            Some((1, NameMatch::Full)),
            "完全一致が後ろにあっても、そちらを選ぶこと"
        );
    }

    #[test]
    fn base_name_is_used_only_when_no_exact_match_exists() {
        let names = vec!["a/x.c".to_string(), "src/foo.c".to_string()];
        assert_eq!(
            choose_member(&names, "foo.c"),
            Some((1, NameMatch::BaseOnly)),
            "完全一致が無いならベース名で拾う"
        );
        // 候補が複数あるときは先に出てきた方（v1 索引での従来の挙動）
        let dup = vec!["b/foo.c".to_string(), "a/foo.c".to_string()];
        assert_eq!(choose_member(&dup, "foo.c"), Some((0, NameMatch::BaseOnly)));
    }

    #[test]
    fn exact_match_settles_immediately() {
        // 「完全一致が出たら残りは見ない」の核。後ろに壊れたヘッダがあっても
        // 手前の正しいメンバーが読めるのは、offer が true を返して走査が
        // 止まるからで、ここが崩れるとその性質も崩れる。
        let mut pick = Pick::default();
        assert!(
            pick.offer(0, NameMatch::Full),
            "完全一致はその場で確定すること"
        );

        // ベース名候補は確定させない（完全一致が後ろにあるかもしれない）
        let mut pick = Pick::default();
        assert!(!pick.offer(0, NameMatch::BaseOnly));
        assert_eq!(pick.settle(), Some(0));
        // 候補は最初のものだけ覚える
        assert!(!pick.offer(1, NameMatch::BaseOnly));
        assert_eq!(pick.settle(), Some(0));
    }

    #[test]
    fn no_match_never_settles() {
        let mut pick = Pick::default();
        assert!(!pick.offer(0, NameMatch::No));
        assert_eq!(pick.settle(), None);
    }

    #[test]
    fn choose_member_finds_nothing_when_absent() {
        let names = vec!["src/foo.c".to_string(), "bar.c".to_string()];
        assert_eq!(choose_member(&names, "baz.c"), None);
    }

    #[test]
    fn base_name_fallback_does_not_match_unrelated_files() {
        // 別物を拾わないこと。ここが緩いと、存在しないパスの要求に対して
        // 無関係なファイルを返してしまう。
        assert_eq!(match_member_name("src/foo.c", "bar.c"), NameMatch::No);
        assert_eq!(match_member_name("foo.c", "src/foo.c"), NameMatch::No);
        assert_eq!(match_member_name("srcfoo.c", "foo.c"), NameMatch::No);
        // ディレクトリを持たない名前は後退の対象にしない
        assert_eq!(match_member_name("foo.c", "o.c"), NameMatch::No);
    }

    #[test]
    fn index_default_depth_is_two() {
        assert_eq!(
            LzhOpts::default().max_depth,
            2,
            "既定深度を変えるなら xdf-mcp の LZH_MAX_DEPTH も合わせること"
        );
    }
}
