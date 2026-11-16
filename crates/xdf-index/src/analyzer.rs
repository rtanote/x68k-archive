//! 日本語を索引できるアナライザと、`Index` の取得口の一本化。
//!
//! ## なぜ必要か
//!
//! tantivy 既定の `default` アナライザは `SimpleTokenizer` で、
//! `char::is_alphanumeric()` が false の位置でしか切らない。日本語は漢字・かな・
//! カタカナがすべて true なので、**句読点と空白の間が丸ごと 1 トークンになる**。
//! さらに `RemoveLongFilter::limit(40)` の判定が**バイト長**なので、
//! 40 バイト (= 日本語で約 14 文字) 以上の塊は**索引から消える**。
//!
//! 本番規模の索引で実測した。クエリはいずれも対象文書に literal に存在する、
//! 句読点や空白を挟まない日本語の連なり:
//!
//! | クエリの長さ | 結果 |
//! |---|---|
//! | 27 文字 (81 バイト) | 0 件 |
//! | 14 文字 (42 バイト) | 0 件 |
//! | 13 文字 (39 バイト) | ヒット |
//! | 上の 13 文字の先頭 10 文字 (30 バイト) | 0 件 |
//!
//! **境界が 1 文字差で再現する**: 39 バイトは当たり、42 バイトは 0 件。
//! 最後の行は、連なり全体と完全一致しないと引けない = 部分一致ができないこと。
//! 検体 1 ファイルでは、日本語 1,551 字のうち **508 字 (33%) が索引に無かった**。
//!
//! ## 方式: 文字 bigram
//!
//! CJK の連なりを重なり 2 文字に割る。`電脳倶楽部` →
//! `電脳` `脳倶` `倶楽` `楽部` (位置 0,1,2,3)。
//! 語順は tantivy の `PhraseQuery` が位置で保証するので、`音声合成` の検索が
//! `合成音声` に当たることはない。
//!
//! **tantivy 同梱の `NgramTokenizer` は使えない。** `position` を常に 0 に固定する
//! 実装 (doc にも "the `position` is always 0." と明記) で、隣接を表現できないため
//! フレーズが単なる AND に退化する。
//!
//! ## 既知の制限 (意図的)
//!
//! - **1 文字の CJK クエリは当たらない。** 索引にあるのは 2 文字単位なので、
//!   `音` では `音源` を引けない。形態素解析フィールドを併用すれば覆える (将来)
//! - **行をまたぐ語は当たらない。** 改行・空白はすべて素の区切りとして扱う。
//!   「CJK 間の改行 1 個を透過させる」案は実データで棄却した: 4 文書 3 系統で
//!   「行末が CJK・次行頭が CJK」は 28 件あったが**全てインデント付き**で、
//!   素の継続は 0 件 = 規則が一度も発火しない。インデントも飛ばす変種にすると、
//!   箇条書きの行が結合されてゴミの隣接を作るほうが多かった
//! - **`・` が CJK に挟まれている場合は落とす。** 「ディスク・ユーティリティ」を
//!   「ディスクユーティリティ」として索引する。索引側とクエリ側の両方で同じ処理が
//!   走るので対称になる。実測では無作為な部分文字列クエリ 50 件の再現率が
//!   46 → 48 件に上がり、**他文書へのヒットは増えなかった** (37 件で同数)。
//!   代償は `東京・大阪` が `京大` という偽の隣接を作ること。2 文字クエリで
//!   まれに出るが、カタカナ製品名が主役のこの資料群では利得が大きいと判断した
//!
//! ## 句読点で位置を飛ばす必要が無い理由
//!
//! 境界をまたぐ bigram はそもそも生成されないので、フレーズがそれを要求して
//! 必ず外れる。`結合。成功` は `結合`(0) `成功`(1) になり、`合成` という
//! トークンはどこにも無いため当たらない。位置に穴を空ける必要はない。

use anyhow::{Context, Result};
use std::path::Path;
use tantivy::schema::Schema;
use tantivy::tokenizer::{
    LowerCaser, RemoveLongFilter, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::Index;
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

/// 文字 bigram アナライザの登録名。スキーマの `set_tokenizer` と必ず一致させる。
pub const NGRAM_TOKENIZER: &str = "ja_bigram";

/// tantivy 既定と同じ上限。
///
/// **下げても上げてもいけない。** 分割後の CJK トークンは最大 8 バイト
/// (4 バイト文字 2 つ) なのでこの壁には当たらない。一方 `.DOC` に混じる
/// 16 進ダンプや base64 の長い塊を落とす働きは残したい。
const MAX_TOKEN_BYTES: usize = 40;

/// CJK (漢字・かな・カタカナ) か。bigram に割る対象。
///
/// `・` (U+30FB) はカタカナブロックの中にあるが**区切りとして扱う**ため除く。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3005 | 0x3007          // 々 〇
        | 0x3041..=0x309F        // ひらがな (濁点・繰り返し記号を含む)
        | 0x30A0..=0x30FA        // カタカナ (・ の手前まで)
        | 0x30FC..=0x30FF        // ー ヽ ヾ ヿ
        | 0x3400..=0x4DBF        // CJK 拡張A
        | 0x4E00..=0x9FFF        // CJK 統合漢字
        | 0xF900..=0xFAFF        // CJK 互換漢字
        | 0x20000..=0x2FA1F      // CJK 拡張B 以降
    )
}

/// 語の一部か (ASCII 英数、ラテン、ハングル等)。連なりを丸ごと 1 トークンにする。
fn is_word(c: char) -> bool {
    c.is_alphanumeric() && !is_cjk(c)
}

/// カタカナ中黒 (U+30FB)。
const INTERPUNCT: char = '・';

/// CJK に挟まれた `・` を落とす。
///
/// 「ディスク・ユーティリティ」を「ディスクユーティリティ」として索引する。
/// 索引側とクエリ側で同じ処理が走るので、`・` のあり/なしが対称になる。
/// 行頭の箇条書き `・` は直前が CJK ではないので落ちない。
fn elide_interpuncts(cs: Vec<NormChar>) -> Vec<NormChar> {
    let keep: Vec<bool> = cs
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let sandwiched = n.ch == INTERPUNCT
                && i > 0
                && is_cjk(cs[i - 1].ch)
                && cs.get(i + 1).map(|x| is_cjk(x.ch)).unwrap_or(false);
            !sandwiched
        })
        .collect();
    cs.into_iter()
        .zip(keep)
        .filter_map(|(n, k)| if k { Some(n) } else { None })
        .collect()
}

/// CJK を重なり bigram に、それ以外の語を塊のまま切り出すトークナイザ。
#[derive(Clone, Default)]
pub struct JaBigramTokenizer;

/// 直前のグループに「くっつく」文字か。
///
/// NFKC は直前の starter と合成するので、**1 文字ずつ正規化してはいけない**
/// (`ﾊ` + `ﾞ` が `バ` にならない)。どこまでを 1 グループにするかの判定。
///
/// 本筋は**正準結合クラス** (`ccc != 0` = non-starter)。これを見ないと、
/// かなの濁点だけを特別扱いして `A` + U+030A (結合リング) を取りこぼし、
/// 丸ごと NFKC すれば `Å` になるものが `A` と結合文字に分かれてしまう。
/// 半角カナの濁点 (U+FF9E/FF9F) は `ccc` が 0 なのに合成されるので足す。
///
/// **残る差**: ハングルの字母合成 (LV + T) は starter 同士なので合成されない。
/// SJIS 由来の本文には出ず、クエリでも現実的に来ないため許容している。
fn attaches_to_previous(c: char) -> bool {
    canonical_combining_class(c) != 0 || matches!(c, '\u{FF9E}' | '\u{FF9F}')
}

/// NFKC 後の 1 文字と、それを生んだ**元テキスト**のバイト範囲。
struct NormChar {
    ch: char,
    orig_start: usize,
    orig_end: usize,
}

/// 元テキストを NFKC しつつ、各文字が元のどのバイト範囲から来たかを保つ。
///
/// **`Token` の `offset_from`/`offset_to` は元テキストの範囲でなければならない**
/// (`&text[offset_from..offset_to]` が有効な文字列であること)。NFKC はバイト長を
/// 変える (`ＺＭＳ` 9B → `ZMS` 3B、`㍿` 3B → `株式会社` 12B) ので、正規化後の
/// 位置をそのまま入れると元テキストの文字境界を割る。
///
/// 実際に `SnippetGenerator` がそこで slice してパニックした
/// (`start byte index N is not a char boundary`)。トークン文字列と
/// オフセットの指す範囲は長さが違ってよい (ステミング等と同じ)。
fn normalize_with_offsets(text: &str) -> Vec<NormChar> {
    let mut out: Vec<NormChar> = Vec::with_capacity(text.len() / 2);
    let mut group = String::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let mut end = start + c.len_utf8();
        group.clear();
        group.push(c);
        // 続く結合文字・濁点類は同じグループに入れる (合成のため)
        while let Some(&(ns, nc)) = chars.peek() {
            if !attaches_to_previous(nc) {
                break;
            }
            group.push(nc);
            end = ns + nc.len_utf8();
            chars.next();
        }
        // 1 グループが複数文字に展開されることもある (`㍿` → `株式会社`)。
        // その場合は全員が同じ元範囲を指す。
        for nc in group.nfkc() {
            out.push(NormChar {
                ch: nc,
                orig_start: start,
                orig_end: end,
            });
        }
    }
    out
}

/// [`JaBigramTokenizer`] のトークン列。
///
/// NFKC 正規化した**自前の文字列を所有する**。借用しないのは、NFKC が
/// 文字数を変える (`㍿` → `株式会社`) うえ半角カナの濁点を合成するため、
/// 正規化を**トークン化より前**に済ませる必要があるから。
/// `TokenFilter` では境界が決まった後しか触れず、この順序を作れない。
pub struct JaBigramTokenStream {
    /// NFKC 後の文字と、元テキスト上の位置。bigram は 1 文字先読みが要る。
    cs: Vec<NormChar>,
    /// 次に見る `cs` の添字
    i: usize,
    token: Token,
}

impl Tokenizer for JaBigramTokenizer {
    type TokenStream<'a> = JaBigramTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> JaBigramTokenStream {
        let mut token = Token::default();
        token.reset();
        JaBigramTokenStream {
            cs: elide_interpuncts(normalize_with_offsets(text)),
            i: 0,
            token,
        }
    }
}

impl JaBigramTokenStream {
    fn char_at(&self, i: usize) -> Option<char> {
        self.cs.get(i).map(|n| n.ch)
    }

    fn is_cjk_at(&self, i: usize) -> bool {
        self.char_at(i).map(is_cjk).unwrap_or(false)
    }

    fn is_word_at(&self, i: usize) -> bool {
        self.char_at(i).map(is_word).unwrap_or(false)
    }

    /// `cs[from..=to]` を現在のトークンに書き出す。
    ///
    /// **オフセットは元テキストの範囲**。トークン文字列の長さとは一致しない
    /// (NFKC で縮んだり伸びたりする)。
    fn emit(&mut self, from: usize, to: usize) {
        self.token.offset_from = self.cs[from].orig_start;
        self.token.offset_to = self.cs[to].orig_end;
        for k in from..=to {
            self.token.text.push(self.cs[k].ch);
        }
    }
}

impl TokenStream for JaBigramTokenStream {
    fn advance(&mut self) -> bool {
        self.token.text.clear();
        self.token.position = self.token.position.wrapping_add(1);

        while self.i < self.cs.len() {
            if self.is_cjk_at(self.i) {
                if self.is_cjk_at(self.i + 1) {
                    // 連なりの途中: 重なり bigram を出して 1 文字だけ進む
                    self.emit(self.i, self.i + 1);
                    self.i += 1;
                    return true;
                }
                // 連なりの最後の文字。長さ 1 の連なりだけ unigram として出す。
                // 長さ 2 以上なら直前の bigram に含まれているので出さない。
                let alone = self.i == 0 || !self.is_cjk_at(self.i - 1);
                let at = self.i;
                self.i += 1;
                if alone {
                    self.emit(at, at);
                    return true;
                }
                continue;
            }

            if self.is_word_at(self.i) {
                let from = self.i;
                while self.is_word_at(self.i) {
                    self.i += 1;
                }
                self.emit(from, self.i - 1);
                return true;
            }

            // 区切り
            self.i += 1;
        }
        false
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

/// [`NGRAM_TOKENIZER`] に登録するアナライザ。
pub fn ja_bigram_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(JaBigramTokenizer)
        .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
        .filter(LowerCaser)
        .build()
}

/// このクレートのスキーマが名前で参照するアナライザを全部登録する。
///
/// **書く側と読む側の両方で呼ぶ必要がある。** 片方で忘れると、原因から遠い
/// 場所で `UnknownTokenizer` が出る。だから [`open_in_dir`] /
/// [`create_in_dir`] / [`create_in_ram`] の中で必ず通すようにしてあり、
/// `Index::*_in_*` の直呼びは `no_direct_index_constructors` テストで禁じている。
pub fn register(index: &Index) {
    index
        .tokenizers()
        .register(NGRAM_TOKENIZER, ja_bigram_analyzer());
}

/// 既存の索引を開く。**`Index::open_in_dir` を直接呼ばないこと。**
pub fn open_in_dir(dir: &Path) -> Result<Index> {
    let index = Index::open_in_dir(dir).with_context(|| format!("Cannot open index at {dir:?}"))?;
    register(&index);
    Ok(index)
}

/// 索引を新規作成する。**`Index::create_in_dir` を直接呼ばないこと。**
pub fn create_in_dir(dir: &Path, schema: Schema) -> Result<Index> {
    let index = Index::create_in_dir(dir, schema)
        .with_context(|| format!("Cannot create index at {dir:?}"))?;
    register(&index);
    Ok(index)
}

/// テスト用。**`Index::create_in_ram` を直接呼ばないこと。**
pub fn create_in_ram(schema: Schema) -> Index {
    let index = Index::create_in_ram(schema);
    register(&index);
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名前で引いたアナライザを通したトークン列 (テキストと位置)。
    fn toks(name: &str, text: &str) -> Vec<(String, usize)> {
        let index = create_in_ram(tantivy::schema::Schema::builder().build());
        let mut analyzer = index.tokenizers().get(name).expect("registered tokenizer");
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            let t = stream.token();
            out.push((t.text.clone(), t.position));
        }
        out
    }

    fn texts(name: &str, text: &str) -> Vec<String> {
        toks(name, text).into_iter().map(|(t, _)| t).collect()
    }

    #[test]
    fn bigrams_with_contiguous_positions() {
        assert_eq!(
            toks(NGRAM_TOKENIZER, "電脳倶楽部"),
            vec![
                ("電脳".to_string(), 0),
                ("脳倶".to_string(), 1),
                ("倶楽".to_string(), 2),
                ("楽部".to_string(), 3),
            ]
        );
    }

    #[test]
    fn does_not_split_at_script_boundaries() {
        // 漢字とかなを割ると送り仮名の語が長さ1の連なりに潰れる。
        assert_eq!(texts(NGRAM_TOKENIZER, "書き込み"), ["書き", "き込", "込み"]);
    }

    #[test]
    fn nfkc_runs_before_pairing() {
        assert_eq!(texts(NGRAM_TOKENIZER, "ＺＭＳ"), ["zms"], "全角英字");
        // 半角カナの濁点は 2 文字。合成より先にペアリングされると
        // 「ﾊ」と「ﾞ」が別扱いになり壊れる = TokenFilter では作れない順序。
        assert_eq!(texts(NGRAM_TOKENIZER, "ﾊﾞｲﾄ"), ["バイ", "イト"]);
    }

    /// 直したバグを両方向から固定する。**このテストが本命。**
    #[test]
    fn long_japanese_is_dropped_by_default_but_not_by_ours() {
        // 42 バイト (本番索引で 0 件になった長さ)。合成した文字列。
        let s = "音源ドライバの常駐を解除する";
        assert!(s.len() >= MAX_TOKEN_BYTES, "検体が 40 バイト以上であること");
        assert!(
            texts("default", s).is_empty(),
            "既定アナライザはこれを丸ごと捨てる (これが元のバグ)"
        );
        assert!(
            !texts(NGRAM_TOKENIZER, s).is_empty(),
            "ja_bigram は捨てない"
        );
    }

    #[test]
    fn substring_of_a_run_is_findable() {
        // 既定では塊単位しか一致しなかった。bigram なら部分列が含まれる。
        let all = texts(NGRAM_TOKENIZER, "画面モードを元に戻しました");
        for pair in ["画面", "に戻", "した"] {
            assert!(all.contains(&pair.to_string()), "{pair} が無い: {all:?}");
        }
    }

    #[test]
    fn single_cjk_char_only_when_alone() {
        assert_eq!(texts(NGRAM_TOKENIZER, "音"), ["音"], "単独なら unigram");
        // 長さ 2 以上の連なりでは unigram を出さない (出すと索引が膨らみ、
        // 1 文字クエリが全文書に当たるようになる)。
        assert_eq!(texts(NGRAM_TOKENIZER, "音声"), ["音声"]);
        assert_eq!(texts(NGRAM_TOKENIZER, "音。声"), ["音", "声"]);
    }

    /// `・` を落とすので、中黒の有無が対称になること。
    #[test]
    fn interpunct_inside_katakana_is_elided() {
        assert_eq!(
            texts(NGRAM_TOKENIZER, "ディスク・ユーティリティ"),
            texts(NGRAM_TOKENIZER, "ディスクユーティリティ"),
            "中黒の有無で結果が変わらないこと"
        );
        assert!(texts(NGRAM_TOKENIZER, "ディスク・ユーティリティ").contains(&"クユ".to_string()));
    }

    /// 箇条書きの `・` は落とさないこと。
    ///
    /// 直前が CJK でなければ落とさない規則なので、行頭の中黒は区切りのまま。
    /// ここを落とすと、箇条書きの各行が繋がってゴミの隣接を作る。
    #[test]
    fn bullet_interpunct_stays_a_separator() {
        let t = texts(NGRAM_TOKENIZER, "項目一\n・項目二");
        assert!(
            !t.contains(&"一項".to_string()),
            "行頭の中黒をまたいで繋がらないこと: {t:?}"
        );
    }

    /// `・` を落とすことで生まれる偽の隣接を、受容した仕様として固定する。
    ///
    /// `東京・大阪` のような並列表記が `京大` を作る。実測では他文書への
    /// ヒットは増えなかったが、理屈の上では存在するので記録しておく。
    #[test]
    fn elision_creates_a_known_false_adjacency() {
        let t = texts(NGRAM_TOKENIZER, "東京・大阪");
        assert_eq!(t, ["東京", "京大", "大阪"], "既知かつ受容した帰結");
    }

    #[test]
    fn newline_and_punctuation_are_plain_separators() {
        // 行をまたぐ語は当たらない。これは v1 の仕様 (実データで決めた)。
        // 将来規則を足したらこのテストが落ちて気づける。
        let t = texts(NGRAM_TOKENIZER, "電脳倶\n楽部");
        assert!(
            !t.contains(&"倶楽".to_string()),
            "改行をまたがないこと: {t:?}"
        );
        // 境界をまたぐ bigram が無いので、位置に穴を空ける必要が無い。
        let t = texts(NGRAM_TOKENIZER, "結合。成功");
        assert!(
            !t.contains(&"合成".to_string()),
            "句読点をまたがないこと: {t:?}"
        );
    }

    /// 「ASCII は劣化しない」を主張ではなくテストにする。
    #[test]
    fn ascii_is_identical_to_the_default_tokenizer() {
        for s in [
            "SONGA.DOC Ver 2.06",
            "Z-MUSIC",
            "#include <stdio.h>",
            "t200 @1 o4 cdefgab",
            "PCM8 ADPCM 15.6kHz",
        ] {
            assert_eq!(
                toks(NGRAM_TOKENIZER, s),
                toks("default", s),
                "ASCII の挙動が変わってはいけない: {s:?}"
            );
        }
    }

    /// 日本語に隣接した ASCII の語が、独立して引けるようになること。
    ///
    /// **既定アナライザの取りこぼしは日本語だけの話ではなかった。**
    /// `SimpleTokenizer` は字種の境界で切らないので、日本語に続く `PCM8` が
    /// 丸ごと 1 トークンになり、`PCM8` では引けなかった。この資料群は
    /// `PCM8使用` のような書き方が常態なので、製品名・API 名という
    /// **最も検索されるものが落ちていた**。実イメージ 2 ファイルで確認済み。
    #[test]
    fn ascii_glued_to_japanese_becomes_searchable() {
        let s = "音源はPCM8.Xを使う";
        let before = texts("default", s);
        assert!(
            before.contains(&"音源はpcm8".to_string()),
            "既定では日本語と ASCII が 1 トークンに潰れる (これが元の挙動): {before:?}"
        );
        assert!(
            !before.contains(&"pcm8".to_string()),
            "だから PCM8 では引けなかった: {before:?}"
        );

        let after = texts(NGRAM_TOKENIZER, s);
        assert!(
            after.contains(&"pcm8".to_string()),
            "字種の境界で切るので ASCII 語が独立する: {after:?}"
        );
    }

    #[test]
    fn long_ascii_runs_are_still_dropped() {
        let long = "a".repeat(MAX_TOKEN_BYTES);
        assert!(
            texts(NGRAM_TOKENIZER, &long).is_empty(),
            "40 バイト以上は落とす"
        );
        let ok = "a".repeat(MAX_TOKEN_BYTES - 1);
        assert_eq!(texts(NGRAM_TOKENIZER, &ok).len(), 1, "39 バイトは残す");
    }

    /// オフセットと位置を全部集める。
    fn spans(text: &str) -> Vec<(String, usize, usize)> {
        let index = create_in_ram(tantivy::schema::Schema::builder().build());
        let mut analyzer = index.tokenizers().get(NGRAM_TOKENIZER).unwrap();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            let t = stream.token();
            out.push((t.text.clone(), t.offset_from, t.offset_to));
        }
        out
    }

    /// **グループ単位の正規化が、丸ごと NFKC したものと一致すること。**
    ///
    /// オフセットを保つためにグループ単位で正規化しているので、グループの
    /// 切り方を間違えると合成が壊れる。かなの濁点だけを特別扱いしていた間は
    /// `A` + 結合リング が `Å` にならず `A` と結合文字に分かれていた
    /// (丸ごと NFKC すれば `Å`)。SJIS 由来の本文には結合文字は出ないが、
    /// **クエリは任意の UTF-8 で来る**ので索引側とクエリ側がずれる。
    ///
    /// 既知の差はハングルの字母合成 (LV + T) だけ。starter 同士なので
    /// グループが分かれる。この資料群では現実的に来ないため許容している。
    #[test]
    fn group_normalization_matches_whole_string_nfkc() {
        for text in [
            "ＺＭＳファイル",
            "ﾊﾞｲﾄとﾊﾞｯﾌｧ",
            "㍿の略記",
            "A\u{030A}ngstrom",     // 結合リング: 丸ごとなら Å
            "か\u{3099}き\u{309A}", // 結合濁点・半濁点
            "e\u{0301}\u{0327}",    // 結合文字が 2 つ (並べ替えが起きる)
            "音声合成／ＰＣＭ８・ＡＤＰＣＭ",
        ] {
            let got: String = normalize_with_offsets(text).iter().map(|n| n.ch).collect();
            assert_eq!(
                got,
                crate::text_norm::nfkc(text),
                "グループ単位の正規化が丸ごとと一致しない: {text:?}"
            );
        }
    }

    #[test]
    fn offsets_point_at_the_original_text() {
        // 正規化で変わらない入力なら、そのままの位置
        assert_eq!(spans("音声合成")[0], ("音声".to_string(), 0, 6));

        // **全角は NFKC で縮むが、オフセットは元テキストの範囲を指す。**
        // `ＺＭＳ` は元で 9 バイト、トークンは `zms` の 3 バイト。
        assert_eq!(spans("ＺＭＳ"), vec![("zms".to_string(), 0, 9)]);

        // 半角カナ + 濁点は 2 文字 (6 バイト) が 1 文字に合成される。
        // 最初の bigram `バイ` は元の `ﾊﾞｲ` = 9 バイト分。
        assert_eq!(spans("ﾊﾞｲﾄ")[0], ("バイ".to_string(), 0, 9));
    }

    /// **オフセットで元テキストを slice してもパニックしないこと。**
    ///
    /// `Token` の契約は「`&text[offset_from..offset_to]` が有効な文字列」。
    /// 正規化後の位置を入れていた間は `SnippetGenerator` がここで
    /// `start byte index N is not a char boundary` で落ちた。
    /// 合成テストでは境界にたまたま乗って通ってしまい、**実データで初めて出た**。
    #[test]
    fn offsets_always_slice_the_original_text() {
        for text in [
            "ＺＭＳファイルは音源ドライバで使う",
            "ﾊﾞｲﾄ数とﾊﾞｯﾌｧの話。全角もＡＢＣ混在",
            "㍿の略記と、ディスク・ユーティリティ",
            "音声合成／ＰＣＭ８・ＡＤＰＣＭ",
            "PCM8使用で重い。ＦＭ音源は軽い",
        ] {
            for (tok, from, to) in spans(text) {
                assert!(
                    text.is_char_boundary(from) && text.is_char_boundary(to),
                    "トークン {tok:?} のオフセット {from}..{to} が文字境界でない: {text:?}"
                );
                assert!(to <= text.len(), "範囲外: {from}..{to} / {}", text.len());
                // 実際に slice してみる (ここでパニックしていた)
                let _ = &text[from..to];
            }
        }
    }

    /// 登録漏れを防ぐ。`Index::*_in_*` はこのモジュールの中だけで呼ぶ。
    ///
    /// トークナイザは**書く側と読む側の両方**で登録が必要で、漏れると
    /// 実行時に原因から遠い場所で `UnknownTokenizer` が出る。1 箇所直して
    /// 終わりにすると、次に追加されたファイルで同じことになる。
    #[test]
    fn no_direct_index_constructors() {
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
                if path.file_name().and_then(|n| n.to_str()) == Some("analyzer.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("読めること");
                for (i, line) in text.lines().enumerate() {
                    for bad in [
                        "Index::open_in_dir",
                        "Index::create_in_dir",
                        "Index::create_in_ram",
                    ] {
                        assert!(
                            !line.contains(bad),
                            "{}:{} が {} を直接呼んでいる。analyzer:: 経由にすること \
                             (トークナイザ登録が漏れる): {}",
                            path.display(),
                            i + 1,
                            bad,
                            line.trim()
                        );
                    }
                }
                checked += 1;
            }
        }
        assert!(checked > 5, "src を読めていない (見たファイル数 {checked})");
    }
}
