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

/// payload に入れる文字列。将来項目が増えてもいいように JSON にしておく。
pub fn current_payload() -> String {
    format!(r#"{{"paths":{}}}"#, PATHS_VERSION)
}

/// 索引から版を読む。印が無ければ `1` 扱い (印を入れる前に作られた索引)。
pub fn read_paths_version(index: &Index) -> u32 {
    let metas = match index.load_metas() {
        Ok(m) => m,
        Err(_) => return 1,
    };
    let payload = match metas.payload.as_deref() {
        Some(p) => p,
        None => return 1,
    };
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("paths").and_then(|p| p.as_u64()))
        .map(|n| n as u32)
        .unwrap_or(1)
}

/// 索引ディレクトリを開いて版を読む。開けなければ `None`。
pub fn read_paths_version_at(index_dir: &Path) -> Option<u32> {
    Index::open_in_dir(index_dir).ok().as_ref().map(read_paths_version)
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

/// 作り直しが必要かどうかと、その理由。
///
/// 返すのは「利用者に見せる 1 行」。`None` なら何も言わなくてよい。
pub fn staleness_note(found: u32) -> Option<String> {
    if found >= PATHS_VERSION {
        return None;
    }
    Some(format!(
        "索引は paths v{found} の規則で作られています (このビルドは v{PATHS_VERSION})。\n\
         LZH 内のサブディレクトリにあるファイルは、パスにディレクトリが入って\n\
         いないため曖昧です。次で作り直してください (summaries/ は消えません)。\n\
         \n\
         \x20   xdf-index build --fresh --out <索引のパス> <アーカイブのパス>\n\
         \n\
         --out とアーカイブのパスは省略できません。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trips() {
        let v: serde_json::Value = serde_json::from_str(&current_payload()).unwrap();
        assert_eq!(v["paths"].as_u64(), Some(PATHS_VERSION as u64));
    }

    /// payload を好きな値にした索引を RAM に作る。`None` なら印を書かない
    /// （= 印を入れる前に作られた索引の再現）。
    fn ram_index_with_payload(payload: Option<&str>) -> Index {
        use tantivy::schema::{Schema, TEXT};
        let mut sb = Schema::builder();
        sb.add_text_field("t", TEXT);
        let index = Index::create_in_ram(sb.build());
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
        assert_eq!(read_paths_version(&index), PATHS_VERSION);
    }

    #[test]
    fn missing_or_broken_payload_means_version_one() {
        // 印が無い索引 = 印を入れる前のもの。壊れた印も同じ扱いにする。
        // 読めないものを「新しい」と誤解すると、作り直しの案内が出なくなる。
        assert_eq!(read_paths_version(&ram_index_with_payload(None)), 1, "印なし");
        for p in ["", "{}", "not json", r#"{"paths":"two"}"#, r#"{"other":5}"#] {
            assert_eq!(
                read_paths_version(&ram_index_with_payload(Some(p))),
                1,
                "payload {p:?} は v1 扱いになること"
            );
        }
    }

    #[test]
    fn preserving_commit_keeps_the_mark() {
        let index = ram_index_with_payload(Some(&current_payload()));
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        commit_preserving(&index, &mut writer).unwrap();
        assert_eq!(
            read_paths_version(&index),
            PATHS_VERSION,
            "一部だけ書き換える commit で印が消えないこと"
        );
    }

    #[test]
    fn preserving_commit_does_not_invent_a_mark() {
        // 印の無い索引に勝手に v2 を付けない。付けると、古い規則のまま
        // 「新しい」と名乗ることになる。
        let index = ram_index_with_payload(None);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        commit_preserving(&index, &mut writer).unwrap();
        assert_eq!(read_paths_version(&index), 1);
    }

    #[test]
    fn stamped_commit_sets_the_mark() {
        let index = ram_index_with_payload(None);
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        commit_stamped(&mut writer).unwrap();
        assert_eq!(read_paths_version(&index), PATHS_VERSION);
    }

    #[test]
    fn note_only_when_behind() {
        assert!(staleness_note(PATHS_VERSION).is_none());
        assert!(staleness_note(PATHS_VERSION + 1).is_none(), "進んでいる索引には何も言わない");
        let note = staleness_note(1).expect("v1 には注意を出す");
        // そのまま打てる形であること。build は archive_dir と --out が必須で、
        // `--fresh` だけ書くと「引数が足りない」で失敗する。案内が失敗する
        // コマンドを教えていては、印を入れた意味がない。
        for must in ["xdf-index build", "--fresh", "--out", "アーカイブのパス"] {
            assert!(note.contains(must), "案内に {must} が無い: {note}");
        }
    }
}
