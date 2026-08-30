//! xdf-mcp: X68000 アーカイブ索引を MCP 経由で公開
//!
//! トランスポートは stdio (既定) と Streamable HTTP (`--http`) の2種。
//! 公開ツールは 7 つ:
//!   - archive_search:         全文索引クエリ (拡張子フィルタ対応)
//!   - archive_read:           image:partition:path から内容取得 (SJIS→UTF-8)
//!   - archive_list:           パーティション/ディレクトリのエントリ一覧
//!   - archive_summarize:      イメージ単位の AI 要約を参照 (生成は xdf-index 側)
//!   - archive_view_image:     PIC 画像を PNG に変換してインライン表示
//!   - archive_metadata_query: 構造化メタデータへの確定的クエリ
//!   - archive_extract:        複数ファイルを ZIP にまとめて配布

use anyhow::{Context, Result};
use clap::Parser;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeRequestParams, ProtocolVersion,
        ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::{
        stdio,
        streamable_http_server::{
            session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
        },
    },
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xdf_fs::bpb::Bpb;
use xdf_fs::direntry::Attr;
use xdf_fs::fs::Filesystem;
use xdf_fs::image::{DiskImage, OpenedImage};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use png2pic::pic_decoder::decode_pic;
use xdf_index::builder::compute_image_id;
use xdf_index::metadata::{self, MetadataFilters, MetadataView, OrderBy, QueryOpts};
use xdf_index::searcher::{SearchOpts, Searcher};
use xdf_index::summary::{find_summary_by_image_id, ImageSummary};
use xdf_fs::walker;
use std::collections::{HashMap, HashSet};
use std::io::Write;

#[derive(Parser)]
#[command(about = "MCP server for the xdf-fs / xdf-index toolchain")]
struct Args {
    /// tantivy 索引ディレクトリ
    #[arg(long)]
    index: PathBuf,

    /// HTTP モードで起動 (例: 127.0.0.1:8765 または 0.0.0.0:8765)。
    /// 省略時は stdio (Claude Desktop / Claude Code の標準形)。
    #[arg(long)]
    http: Option<String>,

    /// HTTP モードでの MCP エンドポイントのパス (デフォルト /mcp)
    ///
    /// Tailscale Funnel 等の公開 HTTPS で運用する場合、長いランダム文字列を含めて
    /// 「URL 自体を秘密」にする運用も可能 (例: `/8a3f9c2e7b1d4a5f.../mcp`)。
    /// 環境変数 `XDF_MCP_HTTP_PATH` でも設定可。
    #[arg(long, env = "XDF_MCP_HTTP_PATH", default_value = "/mcp")]
    http_path: String,

    /// HTTP モードで Host ヘッダ検証に追加で許可するホスト名 (DNS rebinding 防御を維持しつつ
    /// 自分の Tailscale ホスト名等を許可するため)。カンマ区切りで複数指定可、
    /// 環境変数 `XDF_MCP_ALLOWED_HOSTS` でも設定可。
    /// rmcp デフォルトの `localhost` / `127.0.0.1` / `::1` は常に許可される。
    /// 例: `--allowed-host myhost.your-tailnet.ts.net`
    #[arg(long, value_delimiter = ',', env = "XDF_MCP_ALLOWED_HOSTS")]
    allowed_host: Vec<String>,

    /// HTTP モードで Host ヘッダ検証を無効化 (信頼できる Tailnet 内などで全ホスト許可)。
    /// 環境変数 `XDF_MCP_NO_HOST_CHECK=1` でも有効化可。
    #[arg(long, env = "XDF_MCP_NO_HOST_CHECK")]
    no_host_check: bool,

    /// HTTP モードで Streamable HTTP の stateful (セッション) モードを有効化する。
    ///
    /// 既定は stateless。stateful にすると rmcp は `Mcp-Session-Id` を発行し、以降の
    /// POST でそれを要求するが、セッションは `LocalSessionManager` によりプロセスの
    /// メモリ上にしか存在しない。サーバを再起動するとセッションは全消えし、古い ID を
    /// 掴んだままのクライアントには 404 `Not Found: Session not found` (プレーンテキストで、
    /// JSON-RPC ですらない) が返り続ける。リバースプロキシ越しだとこれが
    /// `-32600 Invalid content from server` 等に化けて原因を追いにくい。
    ///
    /// 本サーバのツールは全て単発のリクエスト / レスポンスで、サーバ起点の通知も
    /// 再開 (resumability) も使っていないため、セッションを持つ必要がない。
    /// 環境変数 `XDF_MCP_STATEFUL=1` でも有効化可。
    #[arg(long, env = "XDF_MCP_STATEFUL")]
    stateful: bool,

    /// HTTP モードで Bearer token 認証を有効化 (Tailscale Funnel 等で公開する場合に必須)。
    /// 設定するとリクエストに `Authorization: Bearer <token>` ヘッダが必要になる。
    /// 環境変数 `XDF_MCP_AUTH_TOKEN` でも設定可。空文字列の場合は認証無し。
    /// トークン生成例: `openssl rand -hex 32`
    #[arg(long, env = "XDF_MCP_AUTH_TOKEN", default_value = "")]
    auth_token: String,

    /// archive_extract で生成した ZIP を一時保存するディレクトリ。
    /// 省略時は OS の一時ディレクトリ配下の `xdf-extracts`
    /// (Linux / Docker では `/tmp/xdf-extracts` となり、docker-compose の
    ///  tmpfs マウント先と一致する。Windows では %TEMP% 配下)。
    #[arg(long, env = "XDF_MCP_EXTRACT_DIR")]
    extract_dir: Option<PathBuf>,

    /// archive_extract の download_url を組み立てる際の公開 URL の prefix。
    /// 例: `https://myhost.your-tailnet.ts.net/<secret>` を指定すると
    /// `<prefix>/downloads/<uuid>.zip` を返す。空なら download_url は空文字列を返し、
    /// クライアントは output_path (コンテナ内パス) のみで取り回す必要がある。
    #[arg(long, env = "XDF_MCP_PUBLIC_URL", default_value = "")]
    public_url: String,
}

// ---- ツール引数 / 戻り値の型 ----

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// 検索クエリ (本文 + ファイル名にヒット)。空文字列なら ext フィルタのみで絞り込み。
    #[serde(default)]
    query: String,
    /// 最大ヒット数 (デフォルト 20)
    #[serde(default = "default_limit")]
    limit: usize,
    /// 拡張子フィルタ (`["ZMS","MML","DOC"]` のように複数指定で OR)。
    /// 大文字小文字は無視 (内部で大文字統一)。
    #[serde(default)]
    ext: Vec<String>,
}
fn default_limit() -> usize {
    20
}

#[derive(Serialize)]
struct SearchOutput {
    hits: Vec<HitOut>,
    total: usize,
}

#[derive(Serialize)]
struct HitOut {
    score: f32,
    image_path: String,
    image_id: String,
    partition: u64,
    file_path: String,
    file_name: String,
    ext: String,
    size: u64,
    excerpt: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadParams {
    /// イメージファイルのパス (例: "/archive/disk.hds")
    image_path: String,
    /// パーティション番号 (HDD のみ、デフォルト 0)
    #[serde(default)]
    partition: Option<usize>,
    /// イメージ内のファイルパス (例: "/Z-MUSIC/SAMPLE/SAMPLE.DOC")
    file_path: String,
    /// 返却するバイト数の上限 (デフォルト 64KB)
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}
fn default_max_bytes() -> usize {
    64 * 1024
}

#[derive(Serialize)]
struct ReadOutput {
    encoding: &'static str, // "utf8" or "binary"
    content: String,
    truncated: bool,
    total_size: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListParams {
    image_path: String,
    #[serde(default)]
    partition: Option<usize>,
    /// ディレクトリパス (省略時はルート)
    #[serde(default)]
    dir_path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SummarizeParams {
    /// イメージファイルのパス (例: "/archive/disk.hds")
    image_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ViewImageParams {
    /// ディスクイメージのパス (例: "/archive/disk.hds")
    image_path: String,
    /// パーティション番号 (HDD のみ、デフォルト 0)
    #[serde(default)]
    partition: Option<usize>,
    /// イメージ内のファイルパス (例: "/MUSIC/COVER.PIC"、LZH内部は "outer.lzh!/inner.pic")
    file_path: String,
    /// 描画後 PNG の最大幅。これより大きい場合は等比縮小 (デフォルト: 縮小なし)
    #[serde(default)]
    max_width: Option<u32>,
}

#[derive(Serialize)]
struct ViewImageMetadata {
    format: &'static str,
    width: u32,
    height: u32,
    bit_depth: u16,
    mode: u16,
    /// PIC ヘッダ内のコメント (SJIS → UTF-8)
    comment: String,
    /// 出典: image_path:partition:file_path
    source: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExtractParams {
    /// ディスクイメージのパス
    image_path: String,
    /// 取り出すファイル / ディレクトリ / glob パターンの並び。混在可。
    /// 例:
    /// - `["IKAP/"]` → `/IKAP` 配下を再帰的に全部
    /// - `["*.X"]` → イメージ全体から拡張子 X のファイル
    /// - `["MUSIC/SAMPLE.ZMS", "TOOLS/**/*.S"]` → 個別ファイル + glob 混在
    paths: Vec<String>,
    /// パーティション番号 (HDD のみ、デフォルト 0)
    #[serde(default)]
    partition: Option<usize>,
    /// ディレクトリ階層を平坦化するか (デフォルト false = 階層保持)
    #[serde(default)]
    flatten: bool,
}

#[derive(Serialize)]
struct ExtractOutput {
    /// HTTP モード + XDF_MCP_PUBLIC_URL 設定時: ZIP の公開ダウンロード URL。
    /// それ以外: 空文字列 (output_path で参照)。
    download_url: String,
    /// コンテナ内の絶対パス (HTTP/stdio 共通)
    output_path: String,
    /// ZIP に含まれた各ファイル
    files: Vec<ExtractEntry>,
    total_size_uncompressed: u64,
    total_size_compressed: u64,
    /// 100 MB 上限を伝えるための補足
    note: &'static str,
}

#[derive(Serialize)]
struct ExtractEntry {
    /// ZIP 内でのパス (flatten=false なら元の階層、true ならファイル名のみ)
    path: String,
    /// 元のファイルのパス (出典)
    source_path: String,
    /// 元のファイルサイズ (圧縮前)
    size: u64,
}

#[derive(Serialize)]
struct ListOutput {
    entries: Vec<EntryOut>,
}

// ---- archive_metadata_query (Phase 5b) ----

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
struct MetadataQueryFilters {
    /// 号番号下限 (含む)
    #[serde(default)]
    issue_no_min: Option<u64>,
    /// 号番号上限 (含む)
    #[serde(default)]
    issue_no_max: Option<u64>,
    /// "main" (本誌) / "extra" (別冊) / "all" (デフォルト)
    #[serde(default)]
    issue_kind: Option<String>,
    /// 投稿者名の部分一致 (例: "山田")
    #[serde(default)]
    submitter_contains: Option<String>,
    /// タイトル部分一致
    #[serde(default)]
    title_contains: Option<String>,
    /// 演奏エンジン部分一致 (例: "Z-MUSIC")
    #[serde(default)]
    engine_contains: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MetadataQueryParams {
    /// プリセットビュー: 現状は "dennou_tracks" のみ
    #[serde(default = "default_view")]
    view: String,
    #[serde(default)]
    filters: MetadataQueryFilters,
    /// "issue_asc" (デフォルト) / "issue_desc" / "title_asc"
    #[serde(default = "default_order_by")]
    order_by: String,
    /// 最大行数 (デフォルト 5000)。CSV を1コールで返すための上限。
    #[serde(default = "default_query_limit")]
    limit: usize,
    /// "json" (デフォルト) / "csv" (BOM 付き UTF-8 1ストリング)
    #[serde(default = "default_format")]
    format: String,
}

fn default_view() -> String {
    "dennou_tracks".into()
}
fn default_order_by() -> String {
    "issue_asc".into()
}
fn default_query_limit() -> usize {
    5000
}
fn default_format() -> String {
    "json".into()
}

#[derive(Serialize)]
struct MetadataQueryOutput {
    view: String,
    columns: Vec<String>,
    /// "json" のとき配列、"csv" のとき1ストリング (BOM 付き)
    rows: serde_json::Value,
    row_count: usize,
    truncated: bool,
}

#[derive(Serialize)]
struct EntryOut {
    name: String,
    kind: &'static str, // "file" | "dir"
    size: u64,
    mtime_unix: i64,
    attr: String,
}

// ---- サーバ本体 ----

#[derive(Clone)]
struct XdfMcpServer {
    searcher: Arc<Searcher>,
    index_dir: Arc<PathBuf>,
    /// archive_extract が ZIP を書き出すディレクトリ
    extract_dir: Arc<PathBuf>,
    /// archive_extract の download_url 組み立てに使う公開 URL prefix。
    /// 例: `https://host.example.ts.net/<secret>` → `<prefix>/downloads/<uuid>.zip`。
    /// None なら download_url は空文字列を返す (stdio モード等)。
    download_url_prefix: Arc<Option<String>>,
    tool_router: ToolRouter<XdfMcpServer>,
}

#[tool_router]
impl XdfMcpServer {
    fn new(
        searcher: Arc<Searcher>,
        index_dir: Arc<PathBuf>,
        extract_dir: Arc<PathBuf>,
        download_url_prefix: Arc<Option<String>>,
    ) -> Self {
        Self {
            searcher,
            index_dir,
            extract_dir,
            download_url_prefix,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Full-text search across all indexed X68000 disk images. \
                       Returns hits with image_path/partition/file_path citation and an excerpt. \
                       Optional `ext` array filters by file extension (e.g. [\"ZMS\",\"DOC\"]) \
                       — useful for browsing source code or MML data without a specific keyword."
    )]
    async fn archive_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let hits = self
            .searcher
            .search_with(SearchOpts {
                query: params.query,
                ext: params.ext,
                limit: params.limit,
            })
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let total = hits.len();
        let out = SearchOutput {
            hits: hits
                .into_iter()
                .map(|h| HitOut {
                    score: h.score,
                    image_path: h.image_path,
                    image_id: h.image_id,
                    partition: h.partition,
                    file_path: h.file_path,
                    file_name: h.file_name,
                    ext: h.ext,
                    size: h.size,
                    excerpt: h.excerpt,
                })
                .collect(),
            total,
        };
        let json = serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Read a file from a disk image. Returns UTF-8 text (decoded from \
                       Shift-JIS) for text files, or hex-encoded bytes for binary files."
    )]
    async fn archive_read(
        &self,
        Parameters(params): Parameters<ReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let out = read_file_impl(&params)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "List entries in a directory of a disk image (root if dir_path is omitted)."
    )]
    async fn archive_list(
        &self,
        Parameters(params): Parameters<ListParams>,
    ) -> Result<CallToolResult, McpError> {
        let out = list_dir_impl(&params)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Get the cached AI summary of a disk image (generated by `xdf-index summarize` CLI). \
                       Returns the summary, categories (e.g. music/game), topics (e.g. Z-MUSIC), \
                       and highlight files. If the image is part of a multi-disk group (e.g. side A/B/X \
                       of one magazine issue), the returned summary covers the entire set; the `members` \
                       field lists all disks in the group. If no summary is cached yet, returns an error \
                       suggesting to run the CLI first."
    )]
    async fn archive_summarize(
        &self,
        Parameters(params): Parameters<SummarizeParams>,
    ) -> Result<CallToolResult, McpError> {
        let summary = lookup_summary(&self.index_dir, &params.image_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&summary)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Render an X68000 PIC image as inline PNG. \
                       Takes a PIC file from a disk image (XDF/HDS/HDF, including LZH-internal paths via `!/`) \
                       and decodes it (paletted 4/8-bit + 15/16-bit RGB). \
                       \
                       Returns metadata text + a Content::image block with PNG. The image appears \
                       inside the tool-result UI; in Claude Desktop the result is collapsed by \
                       default — your reply should remind the user to click the 'Archive view image' \
                       row to expand it and view the image. Use `max_width: 512` for large PIC \
                       images (768x512) to keep the response payload small."
    )]
    async fn archive_view_image(
        &self,
        Parameters(params): Parameters<ViewImageParams>,
    ) -> Result<CallToolResult, McpError> {
        let (metadata, png_b64) = view_image_impl(&params)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let meta_json = serde_json::to_string_pretty(&metadata)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![
            Content::text(meta_json),
            Content::image(png_b64, "image/png"),
        ]))
    }

    #[tool(
        description = "Query the structured-metadata index for deterministic listings (NOT BM25 ranking). \
                       PRIMARY use case: produce a CSV / JSON list of every track / artwork / tool across \
                       all indexed magazines in a single tool call. Built for the 'I want every Z-MUSIC \
                       track from issues 50-100 by 山田 太郎' shape of question. \
                       \
                       View `dennou_tracks` returns one row per song from 電脳倶楽部 (本誌+別冊): \
                       issue_no, issue_kind, title, original_artist, composer, arranger, submitter, \
                       submitter_pref, engine, duration, image_path, source_path. \
                       \
                       Filters (all optional): issue_no_min/max, issue_kind (main/extra/all), \
                       submitter_contains, title_contains, engine_contains. Substring matches are \
                       Unicode-literal — `submitter_contains: \"山田\"` matches both `山田 太郎` and `山田太郎`. \
                       \
                       Use format=\"csv\" to get a BOM-marked UTF-8 single string (Excel-friendly), \
                       or format=\"json\" to get a row-per-record array. \
                       \
                       Prerequisite: an admin must run `xdf-index extract --schema dennou` once to \
                       populate the structured fields; otherwise this returns 0 rows."
    )]
    async fn archive_metadata_query(
        &self,
        Parameters(params): Parameters<MetadataQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let view = MetadataView::from_str(&params.view)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let order_by = match params.order_by.as_str() {
            "issue_asc" => OrderBy::IssueAsc,
            "issue_desc" => OrderBy::IssueDesc,
            "title_asc" => OrderBy::TitleAsc,
            other => {
                return Err(McpError::invalid_params(
                    format!("Unknown order_by: {}", other),
                    None,
                ))
            }
        };
        let opts = QueryOpts {
            view,
            filters: MetadataFilters {
                issue_no_min: params.filters.issue_no_min,
                issue_no_max: params.filters.issue_no_max,
                issue_kind: params.filters.issue_kind,
                submitter_contains: params.filters.submitter_contains,
                title_contains: params.filters.title_contains,
                engine_contains: params.filters.engine_contains,
            },
            order_by,
            limit: params.limit,
        };

        let result = metadata::run_query(&*self.index_dir, &opts)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let rows_value = match params.format.as_str() {
            "csv" => serde_json::Value::String(metadata::rows_to_csv(&result)),
            "json" => serde_json::to_value(&result.rows)
                .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            other => {
                return Err(McpError::invalid_params(
                    format!("Unknown format: {} (use csv or json)", other),
                    None,
                ))
            }
        };

        let out = MetadataQueryOutput {
            view: params.view,
            columns: result.columns,
            rows: rows_value,
            row_count: result.row_count,
            truncated: result.truncated,
        };
        let json = serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(
        description = "Bundle multiple files / directories from a disk image into a ZIP file. \
                       PRIMARY use case: bulk extraction (whole directory, dozens of files) \
                       where individual archive_read calls would waste tokens via hex round-tripping. \
                       \
                       The bytes are written into the ZIP **unmodified** — Shift-JIS file names, \
                       binary executable headers, CR/LF line endings are all preserved as-is so the \
                       resulting ZIP can be fed directly to an X68000 emulator. \
                       \
                       `paths` accepts files, directories, and globs (`*`, `**`, `?`) mixed: \
                       `[\"IKAP/\", \"*.X\", \"BIN/**/*.S\"]`. Directory inputs are recursively included. \
                       \
                       Total uncompressed size is capped at 100 MB. The ZIP is written to a host-side \
                       tmpfs path; in HTTP mode the response includes `download_url` (HTTPS via Tailscale) \
                       which the user can open in a browser. LZH-internal members (`outer.lzh!/inner`) \
                       are NOT supported — use archive_read for those individually."
    )]
    async fn archive_extract(
        &self,
        Parameters(params): Parameters<ExtractParams>,
    ) -> Result<CallToolResult, McpError> {
        let prefix: Option<&str> = (&*self.download_url_prefix).as_deref();
        let out = extract_impl(&params, &self.extract_dir, prefix)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let json = serde_json::to_string_pretty(&out)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for XdfMcpServer {
    fn get_info(&self) -> ServerInfo {
        // 注: Implementation::from_build_env() は rmcp 自身の crate 名を返してしまうので
        // env! は呼び出し側 (xdf-mcp) で展開する必要がある。Implementation::new(...) +
        // builder メソッドで構築する (struct は #[non_exhaustive])。
        let info = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .with_title("xdf-mcp — X68000 archive search")
            .with_description(
                "MCP server exposing X68000 disk image archives (XDF/HDS/HDF) \
                 via full-text search and on-demand file read.",
            );
        // プロトコルバージョンは固定しない。
        //
        // プロトコルバージョンは固定しない (以前は V_2024_11_05 を固定していた)。
        // 2024-11-05 は Streamable HTTP がまだ存在しない版の仕様であり、HTTP
        // トランスポートで serve しながらこれを名乗るのは整合しない。ここでは
        // 既定 (`ProtocolVersion::default()` = `LATEST`) を上限として提示し、
        // 実際に使う版は下の `initialize` でクライアントと擦り合わせる。
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(info)
            .with_instructions(
                "MCP server exposing X68000 disk image archives (XDF/HDS/HDF) via full-text \
                 search and on-demand file read. All results carry an image_path:partition:file_path \
                 citation. Useful for retro-computing knowledge retrieval (Z-MUSIC, X-BASIC, \
                 sprite examples, etc.)."
                    .to_string(),
            )
    }

    /// プロトコルバージョンのネゴシエーションを明示的に行う。
    ///
    /// rmcp が `min(クライアント提示, サーバ提示)` を適用してくれるのは、initialize
    /// ハンドシェイクを経由する経路 (stdio / stateful HTTP) だけである。stateless HTTP
    /// では各リクエストが `serve_directly` で単発処理されるため、この既定実装は
    /// `get_info()` をそのまま返してしまい、クライアントが要求した版より新しい版を
    /// 返しうる。MCP 仕様上クライアントは未対応の版を返されたら切断してよいので、
    /// ここで自前で擦り合わせる。
    ///
    /// - クライアント提示が SDK の既知バージョンかつサーバ提示より古い → それを採用
    /// - それ以外 (未知の版 / サーバより新しい版) → サーバ提示 (= `LATEST`) を返す
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerInfo, McpError> {
        let client_version = request.protocol_version.clone();
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        let mut info = self.get_info();
        if ProtocolVersion::KNOWN_VERSIONS.contains(&client_version)
            && client_version < info.protocol_version
        {
            info.protocol_version = client_version;
        }
        Ok(info)
    }
}

// ---- ツール実装 (sync ヘルパ) ----

fn read_file_impl(params: &ReadParams) -> Result<ReadOutput> {
    let opened = OpenedImage::open(&params.image_path)?;
    let normalized = params.file_path.trim_start_matches('/');

    // "outer.lzh!/inner/path" 形式なら LZH 内のメンバーを返す
    let (outer_path, inner_path) = match normalized.split_once("!/") {
        Some((o, i)) => (o, Some(i)),
        None => (normalized, None),
    };

    let outer_bytes = match opened {
        OpenedImage::Floppy(img) => {
            let fs = Filesystem::open(&img)?;
            let entry = fs.resolve(outer_path)?;
            fs.read_file(&entry)?
        }
        OpenedImage::Hdd(hdd) => {
            let idx = params.partition.unwrap_or(0);
            let part = hdd.partition(idx)?;
            let bpb = Bpb::parse_hdd(part.read_sector(0)?)?;
            let fs = Filesystem::open_with_bpb(&part, bpb)?;
            let entry = fs.resolve(outer_path)?;
            fs.read_file(&entry)?
        }
    };

    let bytes = if let Some(inner) = inner_path {
        // ネスト LZH (`a.lzh!/b.lzh!/file.txt`) には未対応 — 1階層まで
        if inner.contains("!/") {
            return Err(anyhow::anyhow!(
                "Nested LZH paths (multi-level !/) are not supported in archive_read"
            ));
        }
        xdf_index::lzh::extract_member(&outer_bytes, inner)?
    } else {
        outer_bytes
    };

    let total_size = bytes.len() as u64;
    let truncated = bytes.len() > params.max_bytes;
    let slice = &bytes[..bytes.len().min(params.max_bytes)];

    // SJIS デコードして印字可能率で text/binary 判定
    let (text_cow, _enc, _err) = encoding_rs::SHIFT_JIS.decode(slice);
    let text = text_cow.into_owned();
    let total_chars = text.chars().count().max(1);
    let printable = text
        .chars()
        .filter(|c| !c.is_control() || matches!(*c, '\n' | '\r' | '\t'))
        .count();
    let printable_ratio = printable as f64 / total_chars as f64;
    if printable_ratio > 0.9 {
        Ok(ReadOutput {
            encoding: "utf8",
            content: text,
            truncated,
            total_size,
        })
    } else {
        let mut hex = String::with_capacity(slice.len() * 2);
        for b in slice {
            hex.push_str(&format!("{:02x}", b));
        }
        Ok(ReadOutput {
            encoding: "binary",
            content: hex,
            truncated,
            total_size,
        })
    }
}

fn list_dir_impl(params: &ListParams) -> Result<ListOutput> {
    let opened = OpenedImage::open(&params.image_path)?;
    let dir = params.dir_path.as_deref().unwrap_or("");
    let normalized = dir.trim_matches('/');

    let entries = match opened {
        OpenedImage::Floppy(img) => {
            let fs = Filesystem::open(&img)?;
            if normalized.is_empty() {
                fs.read_root_dir()?
            } else {
                fs.list_path(normalized)?
            }
        }
        OpenedImage::Hdd(hdd) => {
            let idx = params.partition.unwrap_or(0);
            let part = hdd.partition(idx)?;
            let bpb = Bpb::parse_hdd(part.read_sector(0)?)?;
            let fs = Filesystem::open_with_bpb(&part, bpb)?;
            if normalized.is_empty() {
                fs.read_root_dir()?
            } else {
                fs.list_path(normalized)?
            }
        }
    };

    let out_entries: Vec<EntryOut> = entries
        .iter()
        .filter(|e| e.name != "." && e.name != "..")
        .map(|e| {
            let is_dir = e.attr.contains(Attr::DIRECTORY);
            EntryOut {
                name: e.display_name(),
                kind: if is_dir { "dir" } else { "file" },
                size: e.size as u64,
                mtime_unix: xdf_index::document::mtime_to_unix(e),
                attr: xdf_index::document::attr_str(e.attr),
            }
        })
        .collect();

    Ok(ListOutput {
        entries: out_entries,
    })
}

/// PIC ファイルを取得 → デコード → PNG エンコード → base64
fn view_image_impl(params: &ViewImageParams) -> Result<(ViewImageMetadata, String)> {
    // archive_read と同じロジックでバイトを取得 (LZH 対応含む)
    let read = ReadParams {
        image_path: params.image_path.clone(),
        partition: params.partition,
        file_path: params.file_path.clone(),
        // PIC は最大数百KBなので大きめに
        max_bytes: 4 * 1024 * 1024,
    };
    let bytes = read_raw_bytes(&read)?;

    // PIC マジック確認 → デコード
    if bytes.len() < 3 || &bytes[..3] != b"PIC" {
        anyhow::bail!(
            "Not a PIC file (magic mismatch). First bytes: {:02X?}",
            &bytes[..bytes.len().min(8)]
        );
    }
    let (header, rgb) = decode_pic(&bytes)
        .map_err(|e| anyhow::anyhow!("PIC decode failed: {}", e))?;

    let width = header.width as u32;
    let height = header.height as u32;
    let img = image::RgbImage::from_raw(width, height, rgb)
        .ok_or_else(|| anyhow::anyhow!("RGB buffer size mismatch"))?;

    // max_width が指定されていれば等比縮小
    let img = if let Some(mw) = params.max_width {
        if width > mw {
            let ratio = mw as f32 / width as f32;
            let new_h = (height as f32 * ratio) as u32;
            image::imageops::resize(&img, mw, new_h, image::imageops::FilterType::Lanczos3)
        } else {
            img
        }
    } else {
        img
    };

    // PNG エンコード (in-memory)
    let mut png_bytes: Vec<u8> = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut png_bytes),
        image::ImageFormat::Png,
    )?;
    let b64 = B64_STANDARD.encode(&png_bytes);

    let source = match params.partition {
        Some(p) => format!("{}:{}:{}", params.image_path, p, params.file_path),
        None => format!("{}:{}", params.image_path, params.file_path),
    };
    let metadata = ViewImageMetadata {
        format: "PIC",
        width,
        height,
        bit_depth: header.bit_depth,
        mode: header.mode,
        comment: header.comment,
        source,
    };
    Ok((metadata, b64))
}

/// archive_read と同じくイメージから生バイトを取得 (LZH `!/` パス対応、エンコード判定なし)
fn read_raw_bytes(params: &ReadParams) -> Result<Vec<u8>> {
    let opened = OpenedImage::open(&params.image_path)?;
    let normalized = params.file_path.trim_start_matches('/');
    let (outer_path, inner_path) = match normalized.split_once("!/") {
        Some((o, i)) => (o, Some(i)),
        None => (normalized, None),
    };

    let outer_bytes = match opened {
        OpenedImage::Floppy(img) => {
            let fs = Filesystem::open(&img)?;
            let entry = fs.resolve(outer_path)?;
            fs.read_file(&entry)?
        }
        OpenedImage::Hdd(hdd) => {
            let idx = params.partition.unwrap_or(0);
            let part = hdd.partition(idx)?;
            let bpb = Bpb::parse_hdd(part.read_sector(0)?)?;
            let fs = Filesystem::open_with_bpb(&part, bpb)?;
            let entry = fs.resolve(outer_path)?;
            fs.read_file(&entry)?
        }
    };

    if let Some(inner) = inner_path {
        if inner.contains("!/") {
            anyhow::bail!("Nested LZH paths (multi-level !/) are not supported");
        }
        Ok(xdf_index::lzh::extract_member(&outer_bytes, inner)?)
    } else {
        Ok(outer_bytes)
    }
}

/// archive_extract: ファイル/ディレクトリ/glob を ZIP にまとめて出力
///
/// バイト無加工 (SJIS 名そのまま、バイナリ無変換) で ZIP に格納するので、
/// X68000 エミュレータに渡しても元データのまま動作する。100 MB 上限あり。
fn extract_impl(
    params: &ExtractParams,
    extract_dir: &Path,
    download_url_prefix: Option<&str>,
) -> Result<ExtractOutput> {
    const MAX_TOTAL_SIZE: u64 = 100 * 1024 * 1024;

    if params.paths.is_empty() {
        anyhow::bail!("`paths` must contain at least one entry");
    }

    let opened = OpenedImage::open(&params.image_path)?;
    let collected: Vec<(String, Vec<u8>)> = match opened {
        OpenedImage::Floppy(img) => {
            let fs = Filesystem::open(&img)?;
            collect_matching(&fs, &params.paths, MAX_TOTAL_SIZE)?
        }
        OpenedImage::Hdd(hdd) => {
            let idx = params.partition.unwrap_or(0);
            let part = hdd.partition(idx)?;
            let bpb = Bpb::parse_hdd(part.read_sector(0)?)?;
            let fs = Filesystem::open_with_bpb(&part, bpb)?;
            collect_matching(&fs, &params.paths, MAX_TOTAL_SIZE)?
        }
    };

    if collected.is_empty() {
        anyhow::bail!(
            "No files matched any of the provided paths: {:?}. \
             (Note: archive_extract does not look inside LZH archives — \
             use archive_read for `outer.lzh!/inner` paths.)",
            params.paths
        );
    }

    std::fs::create_dir_all(extract_dir)
        .with_context(|| format!("Cannot create extract dir {:?}", extract_dir))?;

    let id = uuid::Uuid::new_v4();
    let zip_filename = format!("{}.zip", id.simple());
    let zip_path = extract_dir.join(&zip_filename);

    let file = std::fs::File::create(&zip_path)?;
    let mut zip = zip::ZipWriter::new(file);
    // 無圧縮 (Stored)。X68000 ファイルは LZH 等で既に圧縮済みのケースが多く、
    // 再圧縮の利得は小さいため依存を増やさない選択。
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o644);

    let mut entries: Vec<ExtractEntry> = Vec::with_capacity(collected.len());
    let mut total_uncompressed: u64 = 0;

    for (src_path, bytes) in &collected {
        let zip_path_in: String = if params.flatten {
            std::path::Path::new(src_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(src_path)
                .to_string()
        } else {
            src_path.clone()
        };

        total_uncompressed = total_uncompressed.saturating_add(bytes.len() as u64);
        if total_uncompressed > MAX_TOTAL_SIZE {
            // ZIP を破棄して上限超過エラー
            drop(zip);
            std::fs::remove_file(&zip_path).ok();
            anyhow::bail!(
                "Total uncompressed size exceeded {} MB cap (got {} MB so far). \
                 Narrow the `paths` patterns to a smaller subset.",
                MAX_TOTAL_SIZE / 1024 / 1024,
                total_uncompressed / 1024 / 1024
            );
        }

        zip.start_file(&zip_path_in, opts)?;
        zip.write_all(bytes)?;

        entries.push(ExtractEntry {
            path: zip_path_in,
            source_path: src_path.clone(),
            size: bytes.len() as u64,
        });
    }

    zip.finish()?;
    let compressed_size = std::fs::metadata(&zip_path)?.len();

    let download_url = match download_url_prefix {
        Some(prefix) if !prefix.is_empty() => format!(
            "{}/downloads/{}",
            prefix.trim_end_matches('/'),
            zip_filename
        ),
        _ => String::new(),
    };

    Ok(ExtractOutput {
        download_url,
        output_path: zip_path.to_string_lossy().to_string(),
        files: entries,
        total_size_uncompressed: total_uncompressed,
        total_size_compressed: compressed_size,
        note: "Bytes are stored unmodified (SJIS file names, raw binary). \
               Cap: 100 MB uncompressed.",
    })
}

/// `paths` のパターンに合致するファイルを fs から収集して `(path, bytes)` で返す。
///
/// パターンは混在可:
/// - 通常パス (`MUSIC/SAMPLE.ZMS`): 厳密一致 (大文字小文字無視)
/// - ディレクトリ (`IKAP/` or `IKAP`): 配下を再帰的に取得
/// - glob (`*`, `?`, `[...]`, `**` を含む): globset で一致判定 (大文字小文字無視)
///
/// 早期に上限チェックして無駄な read を避ける。
fn collect_matching(
    fs: &Filesystem,
    patterns: &[String],
    max_total_bytes: u64,
) -> Result<Vec<(String, Vec<u8>)>> {
    use globset::{GlobBuilder, GlobSetBuilder};

    let mut literal_files: HashMap<String, ()> = HashMap::new(); // 大文字キー
    let mut dir_prefixes: Vec<String> = Vec::new(); // 末尾 "/" 込み大文字
    let mut glob_builder = GlobSetBuilder::new();
    let mut has_glob = false;

    for raw in patterns {
        let trimmed = raw.trim_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        let is_glob = trimmed.contains(|c: char| matches!(c, '*' | '?' | '[' | ']'));
        if is_glob {
            let g = GlobBuilder::new(trimmed)
                .case_insensitive(true)
                .literal_separator(false)
                .build()
                .map_err(|e| anyhow::anyhow!("Invalid glob pattern '{}': {}", trimmed, e))?;
            glob_builder.add(g);
            has_glob = true;
            continue;
        }
        // 非 glob: ファイル/ディレクトリの可能性
        match fs.resolve(trimmed) {
            Ok(entry) => {
                if entry.attr.contains(Attr::DIRECTORY) {
                    let mut prefix = trimmed.to_ascii_uppercase();
                    if !prefix.ends_with('/') {
                        prefix.push('/');
                    }
                    dir_prefixes.push(prefix);
                } else {
                    literal_files.insert(trimmed.to_ascii_uppercase(), ());
                }
            }
            Err(_) => {
                // 末尾 "/" を持つパスはディレクトリ扱いで前方一致するように
                if raw.ends_with('/') {
                    let prefix = trimmed.to_ascii_uppercase() + "/";
                    dir_prefixes.push(prefix);
                }
                // 見つからないパターンは静かに無視
                // (ユーザは複数パターンを投げる、一部が空振りしても全体は成立する想定)
            }
        }
    }

    let glob_set = if has_glob {
        Some(
            glob_builder
                .build()
                .context("Failed to compile globset")?,
        )
    } else {
        None
    };

    let mut collected: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut running_total: u64 = 0;
    let mut walk_err: Option<anyhow::Error> = None;

    walker::walk(fs, |item| {
        if item.entry.attr.contains(Attr::DIRECTORY) {
            return true; // ディレクトリ自体は ZIP に入れない
        }
        let path = item.path.clone();
        let path_upper = path.to_ascii_uppercase();

        let matched = literal_files.contains_key(&path_upper)
            || dir_prefixes.iter().any(|p| path_upper.starts_with(p))
            || glob_set
                .as_ref()
                .map(|g| g.is_match(&path))
                .unwrap_or(false);

        if !matched || seen.contains(&path) {
            return true;
        }
        seen.insert(path.clone());

        let size = item.entry.size as u64;
        running_total = running_total.saturating_add(size);
        if running_total > max_total_bytes {
            walk_err = Some(anyhow::anyhow!(
                "Total uncompressed size exceeded {} MB cap (running total {} MB at '{}'). \
                 Narrow the `paths` patterns.",
                max_total_bytes / 1024 / 1024,
                running_total / 1024 / 1024,
                path
            ));
            return false;
        }

        match fs.read_file(item.entry) {
            Ok(bytes) => collected.push((path, bytes)),
            Err(e) => {
                walk_err = Some(anyhow::anyhow!("Failed to read {}: {}", path, e));
                return false;
            }
        }
        true
    })?;

    if let Some(e) = walk_err {
        return Err(e);
    }

    Ok(collected)
}

/// `image_path` から `image_id` を計算してキャッシュ JSON を読む
///
/// 単独サマリ (`<image_id>.json`) と グループサマリ (`<group_id>.json` で `members[]` 内に該当)
/// の両方を検索する。
fn lookup_summary(index_dir: &Path, image_path: &str) -> Result<ImageSummary> {
    let path = Path::new(image_path);
    if !path.exists() {
        return Err(anyhow::anyhow!("Image file not found: {}", image_path));
    }
    let id = compute_image_id(path)?;
    let summary = find_summary_by_image_id(index_dir, &id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "No summary cached for image_id={}. Run: xdf-index summarize <archive_dir> --index <index_dir>",
            id
        )
    })?;
    Ok(summary)
}

// ---- main ----

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let searcher = Arc::new(
        Searcher::open(&args.index)
            .with_context(|| format!("Cannot open index at {:?}", args.index))?,
    );
    let index_dir = Arc::new(args.index.clone());
    let extract_dir = Arc::new(
        args.extract_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("xdf-extracts")),
    );
    let download_url_prefix = Arc::new(if args.public_url.is_empty() {
        None
    } else {
        Some(args.public_url.clone())
    });

    // tmpfs (or regular dir) を予め作成しておく
    std::fs::create_dir_all(&*extract_dir).with_context(|| {
        format!(
            "Cannot create extract dir {:?} (mount tmpfs or chmod the path)",
            extract_dir
        )
    })?;

    if let Some(addr) = args.http.as_deref() {
        run_http(
            searcher,
            index_dir,
            extract_dir,
            download_url_prefix,
            addr,
            &args.http_path,
            &args.allowed_host,
            args.no_host_check,
            &args.auth_token,
            args.stateful,
        )
        .await
    } else {
        run_stdio(searcher, index_dir, extract_dir, download_url_prefix).await
    }
}

async fn run_stdio(
    searcher: Arc<Searcher>,
    index_dir: Arc<PathBuf>,
    extract_dir: Arc<PathBuf>,
    download_url_prefix: Arc<Option<String>>,
) -> Result<()> {
    let server = XdfMcpServer::new(searcher, index_dir, extract_dir, download_url_prefix);
    let service = server
        .serve(stdio())
        .await
        .context("Cannot start MCP server (stdio)")?;
    service.waiting().await?;
    Ok(())
}

async fn run_http(
    searcher: Arc<Searcher>,
    index_dir: Arc<PathBuf>,
    extract_dir: Arc<PathBuf>,
    download_url_prefix: Arc<Option<String>>,
    addr: &str,
    path: &str,
    extra_allowed_hosts: &[String],
    no_host_check: bool,
    auth_token: &str,
    stateful: bool,
) -> Result<()> {
    let cancel = tokio_util::sync::CancellationToken::new();

    // セッションごとに新しい XdfMcpServer インスタンスを生成 (Arc clone は cheap)
    let factory_searcher = searcher.clone();
    let factory_index_dir = index_dir.clone();
    let factory_extract_dir = extract_dir.clone();
    let factory_download_url_prefix = download_url_prefix.clone();

    // Host ヘッダ allowlist の構築:
    // - rmcp デフォルトは localhost / 127.0.0.1 / ::1 のみ許可 (DNS rebinding 防御)
    // - --allowed-host で追加 (Tailscale ホスト名等)、--no-host-check で完全無効化
    //
    // セッションモードの選択:
    // - 既定は stateless (`Mcp-Session-Id` を発行しない)。公開ツールは全て単発の
    //   リクエスト / レスポンスで、サーバ起点の通知も resumability も使っていないため、
    //   セッションを持つ必然性がない。stateful だとサーバ再起動でセッションが全消えし、
    //   古い ID を握ったままのクライアントに 404 (プレーンテキスト) を返し続けることになる。
    // - stateless では SSE の枠を外して素の application/json を返せる
    //   (MCP Streamable HTTP 仕様 2025-06-18 で許可)。プロキシ越しでの取り回しも良い。
    let mut config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancel.child_token())
        .with_stateful_mode(stateful)
        .with_json_response(!stateful);
    eprintln!(
        "xdf-mcp: Streamable HTTP mode = {}",
        if stateful {
            "stateful (Mcp-Session-Id required; sessions are lost on restart)"
        } else {
            "stateless (plain JSON responses)"
        }
    );
    if no_host_check {
        config = config.disable_allowed_hosts();
        eprintln!("xdf-mcp: Host header check DISABLED (all hosts allowed)");
    } else if !extra_allowed_hosts.is_empty() {
        let mut hosts: Vec<String> = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        hosts.extend(extra_allowed_hosts.iter().cloned());
        eprintln!(
            "xdf-mcp: allowed Host headers = {}",
            hosts.join(", ")
        );
        config = config.with_allowed_hosts(hosts);
    }

    let service = StreamableHttpService::new(
        move || {
            Ok(XdfMcpServer::new(
                factory_searcher.clone(),
                factory_index_dir.clone(),
                factory_extract_dir.clone(),
                factory_download_url_prefix.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        config,
    );

    // /<secret>/mcp が path、/<secret>/downloads/<uuid>.zip が静的配信
    // path = "/abc/mcp" なら secret_prefix = "/abc"、downloads_path = "/abc/downloads"
    // path = "/mcp" なら secret_prefix = ""、downloads_path = "/downloads"
    let secret_prefix = if path.ends_with("/mcp") {
        path[..path.len() - 4].to_string()
    } else {
        // ユーザがカスタムパスを指定した場合は素朴に path 直下に /downloads を追加
        path.to_string()
    };
    let downloads_path = format!("{}/downloads", secret_prefix);

    let mut router = axum::Router::new().nest_service(path, service).nest_service(
        &downloads_path,
        tower_http::services::ServeDir::new(&*extract_dir),
    );

    // Bearer token 認証 (公開 HTTPS で運用する場合の必須防御)
    if !auth_token.is_empty() {
        let token = auth_token.to_string();
        router = router.layer(axum::middleware::from_fn(move |req, next| {
            let token = token.clone();
            async move { bearer_auth(token, req, next).await }
        }));
        eprintln!("xdf-mcp: Bearer token auth ENABLED");
    } else {
        eprintln!(
            "xdf-mcp: NO authentication (set XDF_MCP_AUTH_TOKEN if exposing publicly)"
        );
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("Cannot bind HTTP listener to {}", addr))?;
    eprintln!("xdf-mcp HTTP listening on http://{}{}", addr, path);
    eprintln!(
        "xdf-mcp downloads served from http://{}{} (extract_dir = {:?})",
        addr, downloads_path, &*extract_dir
    );
    if let Some(prefix) = (&*download_url_prefix).as_deref() {
        eprintln!("xdf-mcp public download URL prefix: {}/downloads/", prefix);
    }

    let graceful_shutdown = async move {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\nshutdown signal received, stopping...");
        cancel.cancel();
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(graceful_shutdown)
        .await
        .context("HTTP server error")?;
    Ok(())
}

/// Bearer token 認証 middleware (axum)
///
/// `Authorization: Bearer <token>` ヘッダが期待値と一致する場合のみ次の handler に進む。
/// 一致しない / ヘッダ無し → 401 Unauthorized。
///
/// 定数時間比較で timing attack 対策。
async fn bearer_auth(
    expected: String,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let provided = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");

    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer realm=\"xdf-mcp\"")],
            "Unauthorized: missing or invalid Bearer token\n",
        )
            .into_response()
    }
}

/// 定数時間文字列比較 (timing attack 対策)
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
