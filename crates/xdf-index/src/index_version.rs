//! 索引が「どの版の規則で作られたか」を索引自身に刻む。
//!
//! このリポジトリは公開されていて、手元に索引を持っている利用者がいる。索引の
//! 作り方を変えたとき、**バイナリだけ新しくして索引を作り直さなかった人**が、
//! なぜ結果がおかしいのか分からないまま使い続けることになる。それを防ぐための
//! 印。
//!
//! 置き場は tantivy の `IndexMeta.payload`。tantivy 自身は一切使わない領域で、
//! まさにこの用途のために用意されている ("This payload is entirely unused by
//! tantivy")。別ファイルにしないのは、索引と一緒にコピーされ、一緒に消える
//! ものにしたいため。
//!
//! **版を上げるのは、索引に入る値が変わったときだけ。** 検索の仕方や表示の
//! 変更では上げない。上げると利用者に作り直しを促すことになるので、安くない。

use anyhow::Result;
use std::path::Path;
use tantivy::{Index, IndexWriter};

/// 索引に入るパスの表し方の版。
///
/// - `1`: LZH メンバーのパスがベース名のみ (ディレクトリが落ちていた)。
///   同名ファイルがサブディレクトリにあると同一パスに潰れ、読み出しが
///   別のファイルを返していた。
/// - `2`: LZH メンバーのパスにディレクトリを含む。
pub const PATHS_VERSION: u32 = 2;

/// 本文の分割規則 (アナライザ) の版。
///
/// - 印が無い (= `0` 扱い): tantivy 既定の `default` アナライザ。日本語は
///   句読点と空白の間が丸ごと 1 トークンになり、40 バイト (約 14 文字) 以上の
///   塊は `RemoveLongFilter` のバイト長判定で落ちて**索引に存在しない**。
///   ASCII の語も日本語に隣接していると一緒に潰れて引けない。
/// - `1`: 文字 bigram + NFKC ([`crate::analyzer::NGRAM_TOKENIZER`])。
pub const ANALYZER_VERSION: u32 = 1;

/// 索引に刻まれている版の組。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexVersions {
    /// 索引内のパスの表し方
    pub paths: u32,
    /// 本文の分割規則
    pub analyzer: u32,
}

/// payload に入れる文字列。項目が増える前提なので JSON で持つ。
pub fn current_payload() -> String {
    serde_json::json!({
        "paths": PATHS_VERSION,
        "analyzer": ANALYZER_VERSION,
    })
    .to_string()
}

/// 索引から版を読む。
///
/// **欠落時の既定値が軸ごとに違う。** `paths` は印を入れる前から規則が v1
/// だったので欠落を `1` とみなす。`analyzer` は印と同時に導入したので、
/// 欠落は `0` = 既定アナライザで作られた索引という意味になる。
/// ここを揃えてしまうと、古い索引に「新しい」と名乗らせることになり、
/// 作り直しの案内が出なくなる。
pub fn read_versions(index: &Index) -> IndexVersions {
    let parsed = index
        .load_metas()
        .ok()
        .and_then(|m| m.payload)
        .and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok());
    let get = |key: &str, when_missing: u32| {
        parsed
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(|x| x.as_u64())
            .map(|n| n as u32)
            .unwrap_or(when_missing)
    };
    IndexVersions {
        paths: get("paths", 1),
        analyzer: get("analyzer", 0),
    }
}

/// 索引ディレクトリを開いて版を読む。開けなければ `None`。
pub fn read_versions_at(index_dir: &Path) -> Option<IndexVersions> {
    crate::analyzer::open_in_dir(index_dir)
        .ok()
        .as_ref()
        .map(read_versions)
}

/// 現在の版を刻んで commit する。
///
/// **索引の中身が丸ごとその版で作られたときだけ使う。** 一部しか触っていない
/// のに刻むと、古い規則で入ったドキュメントが残っているのに「新しい索引」と
/// 名乗ることになり、作り直しの案内が出なくなる。
pub fn commit_stamped(writer: &mut IndexWriter) -> Result<()> {
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&current_payload());
    prepared.commit()?;
    Ok(())
}

/// いまの印を保ったまま commit する。
///
/// tantivy の payload は commit ごとに持ち越されない。何も指定せずに commit
/// すると印が消え、次に開いたとき v1 と判定されて、作り直したばかりの索引に
/// 対してまた作り直しを促すことになる。索引の一部だけを書き換える処理
/// (増分ビルド、メタデータ抽出) はこちらを使う。
pub fn commit_preserving(index: &Index, writer: &mut IndexWriter) -> Result<()> {
    let keep = index.load_metas().ok().and_then(|m| m.payload);
    match keep {
        Some(p) => {
            let mut prepared = writer.prepare_commit()?;
            prepared.set_payload(&p);
            prepared.commit()?;
        }
        // 元から印が無い索引。足してしまうと嘘になるので、そのまま。
        None => {
            writer.commit()?;
        }
    }
    Ok(())
}

/// 索引を作り直す手順。**案内を出す箇所はすべてここを通す。**
///
/// `build` は `<archive_dir>` と `--out` が必須なので、`--fresh` だけ書いた
/// 案内は「引数が足りない」で失敗する。案内する箇所が増えるたびに同じ間違いを
/// するより、文面を 1 つにして検査で縛る方が確実。
pub const REBUILD_HOWTO: &str = "次で作り直してください (summaries/ は消えません)。\n\
     \n\
     \x20   xdf-index build --fresh --out <索引のパス> <アーカイブのパス>\n\
     \n\
     --out とアーカイブのパスは省略できません。";

/// 作り直しが必要かどうかと、その理由。
///
/// 返すのは「利用者に見せる文」。`None` なら何も言わなくてよい。
///
/// **軸が増えても案内は 1 つ。** 理由を列挙して [`REBUILD_HOWTO`] を一度だけ
/// 付ける。軸ごとに別々の案内を出すと、同じコマンドを 2 回見せることになる。
pub fn staleness_note(found: IndexVersions) -> Option<String> {
    let mut reasons: Vec<String> = Vec::new();
    if found.paths < PATHS_VERSION {
        reasons.push(format!(
            "- パスの表し方が v{} です (このビルドは v{PATHS_VERSION})。\n\
             \x20 LZH 内のサブディレクトリにあるファイルは、パスにディレクトリが\n\
             \x20 入っていないため曖昧です。",
            found.paths
        ));
    }
    if found.analyzer < ANALYZER_VERSION {
        reasons.push(format!(
            "- 本文の分割規則が v{} です (このビルドは v{ANALYZER_VERSION})。\n\
             \x20 日本語は句読点の間が丸ごと 1 トークンになっており、14 文字以上の\n\
             \x20 塊は索引に入っていません。日本語に隣接した ASCII 語も引けません。",
            found.analyzer
        ));
    }
    if reasons.is_empty() {
        return None;
    }
    Some(format!(
        "索引が古い規則で作られています。\n{}\n\n{REBUILD_HOWTO}",
        reasons.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// このビルドが刻む版。テストで何度も書くので関数にする。
    fn current() -> IndexVersions {
        IndexVersions {
            paths: PATHS_VERSION,
            analyzer: ANALYZER_VERSION,
        }
    }

    #[test]
    fn payload_round_trips() {
        let v: serde_json::Value = serde_json::from_str(&current_payload()).unwrap();
        assert_eq!(v["paths"].as_u64(), Some(PATHS_VERSION as u64));
        assert_eq!(v["analyzer"].as_u64(), Some(ANALYZER_VERSION as u64));
    }

    /// payload を好きな値にした索引を RAM に作る。`None` なら印を書かない
    /// （= 印を入れる前に作られた索引の再現）。
    fn ram_index_with_payload(payload: Option<&str>) -> Index {
        use tantivy::schema::{Schema, TEXT};
        let mut sb = Schema::builder();
        sb.add_text_field("t", TEXT);
        let index = crate::analyzer::create_in_ram(sb.build());
        let mut writer: tantivy::IndexWriter = index.writer(15_000_000).unwrap();
        match payload {
            Some(p) => {
                let mut prepared = writer.prepare_commit().unwrap();
                prepared.set_payload(p);
                prepared.commit().unwrap();
            }
            None => {
                writer.commit().unwrap();
            }
        }
        index
    }

    #[test]
    fn reads_the_version_we_wrote() {
        let index = ram_index_with_payload(Some(&current_payload()));
        assert_eq!(read_versions(&index), current());
    }

    #[test]
    fn missing_or_broken_payload_means_version_one() {
        // 印が無い索引 = 印を入れる前のもの。壊れた印も同じ扱いにする。
        // 読めないものを「新しい」と誤解すると、作り直しの案内が出なくなる。
        // **軸ごとに既定値が違う。** paths は印の前から規則が v1 だったので 1、
        // analyzer は印と同時に導入したので 0 (= 既定アナライザ) になる。
        let none = read_versions(&ram_index_with_payload(None));
        assert_eq!(none.paths, 1, "印なしの paths");
        assert_eq!(none.analyzer, 0, "印なしの analyzer は 0 = 既定アナライザ");
        for p in ["", "{}", "not json", r#"{"paths":"two"}"#, r#"{"other":5}"#] {
            let v = read_versions(&ram_index_with_payload(Some(p)));
            assert_eq!(v.paths, 1, "payload {p:?} の paths");
            assert_eq!(v.analyzer, 0, "payload {p:?} の analyzer");
        }

        // 片方だけ刻まれている索引 (段階 0 までの索引がこの形)
        let only_paths = read_versions(&ram_index_with_payload(Some(r#"{"paths":2}"#)));
        assert_eq!(only_paths.paths, 2);
        assert_eq!(only_paths.analyzer, 0, "analyzer が無いなら陳腐化扱い");
    }

    #[test]
    fn preserving_commit_keeps_the_mark() {
        let index = ram_index_with_payload(Some(&current_payload()));
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        commit_preserving(&index, &mut writer).unwrap();
        assert_eq!(
            read_versions(&index),
            current(),
            "一部だけ書き換える commit で印が消えないこと (両方の軸)"
        );
    }

    #[test]
    fn preserving_commit_does_not_invent_a_mark() {
        // 印の無い索引に勝手に v2 を付けない。付けると、古い規則のまま
        // 「新しい」と名乗ることになる。
        let index = ram_index_with_payload(None);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        commit_preserving(&index, &mut writer).unwrap();
        assert_eq!(read_versions(&index).paths, 1);
        assert_eq!(read_versions(&index).analyzer, 0);
    }

    #[test]
    fn stamped_commit_sets_the_mark() {
        let index = ram_index_with_payload(None);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        commit_stamped(&mut writer).unwrap();
        assert_eq!(read_versions(&index), current());
    }

    #[test]
    fn note_only_when_behind() {
        let cur = current();
        assert!(staleness_note(cur).is_none(), "現行の索引には何も言わない");
        assert!(
            staleness_note(IndexVersions {
                paths: PATHS_VERSION + 1,
                analyzer: ANALYZER_VERSION + 1
            })
            .is_none(),
            "進んでいる索引には何も言わない"
        );

        // 片方だけ古い
        let old_analyzer = IndexVersions { analyzer: 0, ..cur };
        let note = staleness_note(old_analyzer).expect("アナライザが古ければ言う");
        assert!(note.contains("分割規則"), "理由を述べること: {note}");
        assert!(
            !note.contains("パスの表し方"),
            "古くない軸には触れないこと: {note}"
        );

        // **両方古くても案内は 1 回。** 同じコマンドを 2 回見せない。
        let both = IndexVersions {
            paths: 1,
            analyzer: 0,
        };
        let note = staleness_note(both).expect("両方古ければ言う");
        assert!(
            note.contains("パスの表し方"),
            "両方の理由を述べること: {note}"
        );
        assert!(note.contains("分割規則"), "両方の理由を述べること: {note}");
        assert_eq!(
            note.matches(REBUILD_HOWTO).count(),
            1,
            "手順は 1 回だけ付けること: {note}"
        );
    }

    /// 手順がそのまま打てる形であることを、文面側で縛る。`build` は
    /// `<archive_dir>` と `--out` が必須なので、`--fresh` だけの案内は失敗する。
    /// 失敗するコマンドを教えていては、印を入れた意味がない。
    #[test]
    fn howto_is_a_command_that_runs() {
        for must in ["xdf-index build", "--fresh", "--out", "アーカイブのパス"] {
            assert!(REBUILD_HOWTO.contains(must), "手順に {must} が無い");
        }
    }

    /// 作り直しの案内は、このクレートのどこに書いても「そのまま打てる」こと。
    ///
    /// `build` は `<archive_dir>` と `--out` が必須なので、`--fresh` だけ書いた
    /// 案内は「引数が足りない」で失敗する。実際に 3 箇所で同じ間違いをしていた
    /// ので、文面を [`REBUILD_HOWTO`] に寄せるだけでなく、後から書き足された
    /// 案内も見張る。1 箇所直して終わりにすると、また同じことになる。
    #[test]
    fn no_source_advises_fresh_without_out() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0usize;
        let mut dirs = vec![src];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).expect("src を読めること") {
                let path = entry.expect("entry を読めること").path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("読めること");
                for (i, line) in text.lines().enumerate() {
                    assert!(
                        !(line.contains("build --fresh") && !line.contains("--out")),
                        "{}:{} の案内に --out が無い。打つと失敗する: {}",
                        path.display(),
                        i + 1,
                        line.trim()
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 5, "src を読めていない (見たファイル数 {checked})");
    }
}
