# x68k-archive

X68000 (Human68k) のディスクイメージを直接読み出し、全文索引化して
**温故知新ナレッジベース** として活用するための Rust ツール群。
MCP (Model Context Protocol) サーバを同梱しており、Claude から自然言語で
アーカイブを検索・閲覧できる。

対象フォーマット: **XDF** (フロッピー) / **HDS** (SCSI HDD) / **HDF** (SASI HDD)

[![CI](https://github.com/rtanote/x68k-archive/actions/workflows/ci.yml/badge.svg)](https://github.com/rtanote/x68k-archive/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**→ [紹介ページ / Overview](https://rtanote.github.io/x68k-archive/)**

## これは何か

3つのクレートからなる Rust workspace:

| クレート | 役割 |
|---|---|
| `xdf-fs` | Human68k ファイルシステムのリーダ (FAT12/16 + 拡張ディレクトリエントリ) + CLI 3種 |
| `xdf-index` | tantivy 全文索引 / AI 要約 (Anthropic API) / 構造化メタデータ抽出 |
| `xdf-mcp` | MCP サーバ (stdio + Streamable HTTP)、7 ツール |

```
[ Claude Desktop / Claude Code ]
        ↓ MCP (stdio or HTTP)
[ xdf-mcp: search / read / list / summarize / view_image / metadata_query / extract ]
        ↓
[ xdf-index: tantivy 索引 + 要約キャッシュ (JSON) ]
        ↓
[ xdf-fs: Human68k FS リーダ ]
        ↓
[ ディスクイメージ群 (XDF / HDS / HDF) ]
```

LZH アーカイブは索引時に内部まで展開されるため、`outer.lzh!/inner/file.zms`
のようなパスで内部ファイルを直接読める。

## ⚠️ 著作権について

**このリポジトリはツールのみを配布する。** ディスクイメージ・索引・AI 要約・
抽出物は一切含まれていない。

- 雑誌付属ディスク (電脳倶楽部、Oh!X 等) やそこに収録されたソフトウェア・楽曲・
  文書は**著作物**である。本ツールは、**利用者が自ら正当に取得したイメージ**に
  対してのみ使用すること
- 生成される索引 (`index/`) と AI 要約 (`index/summaries/`) は
  **原本の逐語テキストを含む**。これらを公開・再配布しないこと
  (`.gitignore` で除外済み)
- `archive_extract` / `xdfcp` で取り出した内容の再配布も同様に避けること
- テストフィクスチャ中の人名・楽曲名・許諾番号は**すべて架空**であり、
  実在の人物・作品とは関係がない
- MIT ライセンスは**このソースツリーにのみ**適用され、本ツールが処理する
  イメージの内容には及ばない

## クイックスタート

### Docker (推奨)

ホストに Rust 不要。`./archive` と `./index` は `.gitignore` 済み。

```bash
docker build -t x68k-archive .

# 一覧
docker run --rm -v $PWD/archive:/archive:ro x68k-archive xdfls /archive/disk.xdf
docker run --rm -v $PWD/archive:/archive:ro x68k-archive xdfls --partitions /archive/disk.hds

# 索引化 + 検索
docker run --rm \
  -v $PWD/archive:/archive:ro \
  -v $PWD/index:/index \
  x68k-archive xdf-index build /archive --out /index

docker run --rm -v $PWD/index:/index x68k-archive \
  xdf-index search "スプライト" --index /index
```

### Cargo

```bash
cargo build --release --workspace --bins --examples
./target/release/examples/xdfls disk.xdf
./target/release/xdf-index build ./archive --out ./index
./target/release/xdf-index search "Z-MUSIC CMD" --index ./index
```

## MCP サーバ (`xdf-mcp`)

索引を構築しておけば、Claude から自然言語でアーカイブを扱える。

### 提供ツール (7)

| ツール | 用途 |
|---|---|
| `archive_search` | 全文検索。出典 (`image:partition:path`) と抜粋を返す。`ext` で拡張子フィルタ |
| `archive_read` | ファイル読み出し。テキストは SJIS→UTF-8、バイナリは hex |
| `archive_list` | ディレクトリ一覧 |
| `archive_summarize` | イメージ単位の AI 要約を参照 (生成は CLI 側) |
| `archive_view_image` | PIC 画像を PNG に変換してインライン表示 |
| `archive_metadata_query` | 構造化メタデータへの確定的クエリ (号数・投稿者・カテゴリ等) |
| `archive_extract` | 複数ファイル/ディレクトリを ZIP にまとめてダウンロード URL を返す |

### 起動

```bash
# stdio (Claude Desktop / Claude Code の標準形。--http 省略時が stdio)
xdf-mcp --index ~/.xdf-fs/index

# Streamable HTTP
xdf-mcp --index ~/.xdf-fs/index --http 0.0.0.0:8765

# docker compose (HTTP 常駐)
cp .env.example .env    # ARCHIVE_DIR 等を設定
docker compose up -d
```

接続手順は [docs/claude-desktop-setup.md](docs/claude-desktop-setup.md)、
Linux サーバでの常駐運用は [docs/server-deployment.md](docs/server-deployment.md) を参照。

## CLI ツール

### `xdfls` — ファイル一覧

```bash
xdfls disk.xdf                       # ルート
xdfls disk.xdf -R                    # 再帰
xdfls disk.xdf:/MUSIC                # 内部パス指定
xdfls disk.hds:0                     # HDD パーティション 0
xdfls disk.hds:0:/Z-MUSIC -R         # パーティション + パス + 再帰
xdfls --partitions disk.hds          # パーティション一覧 (HDD のみ)
xdfls --deleted disk.xdf             # 削除エントリも表示
```

### `xdfcp` — ホストFSへコピー

```bash
xdfcp disk.xdf:/MUSIC/SAMPLE.ZMS ./out/               # 単一ファイル
xdfcp -R disk.xdf:/MUSIC ./out/                       # ディレクトリ再帰
xdfcp -R disk.hds:0:/Z-MUSIC ./out/                   # HDD から
xdfcp -R -v -n disk.xdf:/ ./out/                      # verbose + 上書き禁止
```

ファイル名は SJIS → UTF-8 変換 + Windows 互換サニタイズ済み。
mtime は X68000 のタイムスタンプを保持。

### `xdfgrep` — グロブパターン検索

```bash
xdfgrep disk.xdf "*.ZMS"
xdfgrep -i disk.hds:0 "song*.*"              # 大文字小文字無視
xdfgrep disk.hds:0:/Z-MUSIC "*.DOC"          # スコープ付き
xdfgrep -l disk.xdf "*.X"                    # サイズ表示
```

### `xdf-index` — 索引 / 要約 / 構造化抽出

```bash
# 全文索引
xdf-index build /path/to/archive --out ~/.xdf-fs/index          # 増分
xdf-index build /path/to/archive --out ~/.xdf-fs/index --fresh  # 全消し再構築
xdf-index search "FM音源" --index ~/.xdf-fs/index --limit 30
xdf-index search "" --index ~/.xdf-fs/index --ext ZMS,MML       # 拡張子のみで列挙
xdf-index status --index ~/.xdf-fs/index

# AI 要約 (要 ANTHROPIC_API_KEY)
xdf-index summarize /archive --index ~/.xdf-fs/index \
  --groups xdf-grouping.toml --max-cost-usd 10.0 --rate-sleep 25
xdf-index show-groups /archive --groups xdf-grouping.toml       # API 呼ばずプレビュー

# 構造化メタデータ
xdf-index extract /archive --index ~/.xdf-fs/index
xdf-index extract-one /archive/disk.img                          # 索引に書かず確認
xdf-index query --index ~/.xdf-fs/index                          # CSV / JSON 出力
```

検索結果は `score / image:partition:path / size / 拡張子 / 抜粋` の形式で出力。
本文索引対象は `.DOC` `.TXT` `.ZMS` `.BAS` `.X` `.S` `.C` `.H` 等。

複数枚組のフロッピー (`Dennou074A/B/X.img` のような面記号付き) を1つの要約に
まとめるには `xdf-grouping.example.toml` をコピーして使う。

## イメージ指定構文

```
path                       フロッピー (or HDD パーティション 0)、ルート
path:/inner/path           内部パス指定
path:N                     HDD の N 番目パーティション、ルート
path:N:/inner/path         同上、内部パス指定
path:/outer.lzh!/inner     LZH 内部のファイル
```

Windows ドライブレター (`C:\foo.xdf`) は適切にスキップされます。

## ライブラリ API

```rust
use xdf_fs::image::OpenedImage;
use xdf_fs::fs::Filesystem;
use xdf_fs::bpb::Bpb;

match OpenedImage::open("disk.hds")? {
    OpenedImage::Floppy(img) => {
        let fs = Filesystem::open(&img)?;
        for e in fs.list_path("MUSIC")? { println!("{}", e.display_name()); }
    }
    OpenedImage::Hdd(hdd) => {
        let part = hdd.partition(0)?;
        let bpb = Bpb::parse_hdd(part.read_sector(0)?)?;
        let fs = Filesystem::open_with_bpb(&part, bpb)?;
        let entry = fs.resolve("Z-MUSIC/SAMPLE/SAMPLE.DOC")?;
        let data = fs.read_file(&entry)?;
    }
}
```

CLI 用には `xdf_fs::spec::with_filesystem(&spec, |fs, inner_path| {...})` で
HDD/フロッピー差分を吸収できるヘルパも提供。

## 設定 (環境変数)

`.env.example` をコピーして `.env` を作る。

| 変数 | 対象 | 既定 | 用途 |
|---|---|---|---|
| `ANTHROPIC_API_KEY` | xdf-index | (必須) | AI 要約バッチ |
| `ARCHIVE_DIR` | compose | `./tests/data` | アーカイブの bind mount 元 |
| `HTTP_PORT` | compose | `8765` | 公開ポート |
| `XDF_MCP_HTTP_PATH` | xdf-mcp | `/mcp` | MCP エンドポイントのパス (秘密化に使える) |
| `XDF_MCP_ALLOWED_HOSTS` | xdf-mcp | (空) | Host ヘッダ allowlist (DNS rebinding 防御) |
| `XDF_MCP_AUTH_TOKEN` | xdf-mcp | (空=認証なし) | Bearer token 認証 |
| `XDF_MCP_PUBLIC_URL` | xdf-mcp | (空) | `download_url` 組立用の公開 URL prefix |
| `XDF_MCP_EXTRACT_DIR` | xdf-mcp | OS の一時ディレクトリ | ZIP の書き出し先 |
| `XDF_MCP_NO_HOST_CHECK` | xdf-mcp | `false` | Host 検証の無効化 (デバッグ用) |
| `XDF_MCP_STATEFUL` | xdf-mcp | `false` | Streamable HTTP を stateful (セッション) モードにする |

すべて対応する CLI フラグ (`--http-path` 等) でも指定できる。

## ビルド & テスト

```bash
# データ不要のテストのみ
cargo test --workspace

# tests/data/ にイメージを配置してある場合は統合テストも実行
cargo test --workspace -- --include-ignored

# Docker (ホストに Rust 不要)
docker build -t x68k-archive:test --target builder .
docker run --rm x68k-archive:test cargo test --release --locked --workspace
```

統合テストは著作権物のディスクイメージを必要とするため `#[ignore]` が付いている。
`--include-ignored` を付けてもデータが無ければ、必要なファイル名を挙げて失敗する。

## テストデータ配置

統合テストを動かすには `tests/data/` にディスクイメージを手動配置する
(Git 管理対象外)。**これらは配布していない。各自が正当に取得したイメージを置くこと。**

テストが期待するファイル名:

| ファイル | 想定するもの |
|---|---|
| `Dennou074A.img` | 1232KB の 2HD フロッピーイメージ |
| `SCSIHDD1.HDS` | SCSI HDD イメージ (FAT16 パーティション) |
| `hd1.hdf` | SASI HDD イメージ (256B 物理セクタ) |

ライブラリ自体は任意の XDF / HDS / HDF で動作する。上記は既存のアサーションが
特定のイメージ内容に依存しているというだけで、ツールの対応範囲を示すものではない。

## ドキュメント

- [docs/hdd-format.md](docs/hdd-format.md) — HDS/HDF パーティションテーブル + Human68k HDD BPB の実機解析結果
- [docs/claude-desktop-setup.md](docs/claude-desktop-setup.md) — Claude Desktop / Claude Code からの接続手順
- [docs/server-deployment.md](docs/server-deployment.md) — Linux + Tailscale Funnel での常駐運用
- [紹介ページ](https://rtanote.github.io/x68k-archive/) — 概要と使い方 (GitHub Pages)
- [skills/x68k-archive/](skills/x68k-archive/) — Claude Skill (利用作法 + 現代プラットフォームへの翻案マッピング)

## 主な設計判断

### 物理 vs 論理セクタの分離
XDF は物理 512B / 論理 1024B、HDS は同じく 512B / 1024B、HDF は 256B / 1024B。
`Filesystem::open` で `phys_per_logi` を算出して吸収。

### Human68k 拡張ディレクトリエントリ
MS-DOS FAT12 で予約だった `0x0C-0x14` を主名 part2 として使うことで、
**8 + 9 = 17バイト** (全角換算約8文字) のファイル名を実現。

### HDS/HDF と XDF で BPB レイアウトが違う
- XDF: MS-DOS互換 (jump 3B + OEM 8B + LE多バイト)
- HDS/HDF: m68k native (jump 2B + OEM 16B + **BE多バイト**、`sectors_per_fat` が u8)

詳細は [docs/hdd-format.md](docs/hdd-format.md) 参照。

### FAT12 vs FAT16 の自動判別
`Fat::load` がクラスタ数で判定し、`Fat::Fat12` または `Fat::Fat16` を返す。
XDF は通常 FAT12、HDD はクラスタ数 > 4084 のため FAT16。
**FAT16 エントリは BE u16** (Human68k HDD 独自、MS-DOS LE と異なる)。

### 削除エントリは表示専用
0xE5 始まりのエントリは識別はするが、walker / resolve / 索引対象から除外。
復元機能は範囲外。

### 索引粒度はファイル単位
チャンク分割はしない。tantivy のスニペット機能で十分で、
数千〜数万ファイル規模なら full body indexing が簡明。

## ライセンス

MIT — [LICENSE](LICENSE) 参照。

依存する [png2pic](https://github.com/rtanote/png2pic) (PIC→PNG 変換) も MIT。

上記「著作権について」のとおり、この MIT 許諾はソースコードに対するものであり、
本ツールが処理するディスクイメージの内容には及ばない。
