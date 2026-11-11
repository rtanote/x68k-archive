//! xdf-index: X68000 アーカイブの索引化・検索 CLI
//!
//! 例:
//!   xdf-index build /archive --out ~/.xdf-fs/index
//!   xdf-index search "Z-MUSIC CMD" --index ~/.xdf-fs/index
//!   xdf-index status --index ~/.xdf-fs/index

use anyhow::Result;
use clap::{Parser, Subcommand};
use xdf_index::anthropic::MODEL_SONNET;
use xdf_index::builder::{build_index, BuildOpts};
use xdf_index::dennou::DennouPlugin;
use xdf_index::extract::{
    extract_archive, extract_one_image_dry, ExtractOpts, SchemaPlugin,
};
use xdf_index::metadata::{self, MetadataFilters, MetadataView, OrderBy, QueryOpts};
use xdf_index::schema::ArchiveSchema;
use xdf_index::searcher::Searcher;
use xdf_index::grouping::GroupConfig;
use xdf_index::summarizer::{show_groups, summarize_archive, SummarizeOpts};

#[derive(Parser)]
#[command(about = "Build and query the X68000 archive full-text index")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// スキーマ情報を表示
    SchemaInfo,
    /// アーカイブディレクトリから索引を構築 (増分)
    Build {
        archive_dir: std::path::PathBuf,
        #[arg(long)]
        out: std::path::PathBuf,
        /// 既存索引を破棄して新規ビルド
        #[arg(long)]
        fresh: bool,
    },
    /// クエリを実行
    Search {
        /// 全文クエリ (空文字列なら --ext のみで絞り込み)
        #[arg(default_value = "")]
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        index: std::path::PathBuf,
        /// 拡張子フィルタ (カンマ区切り、複数指定で OR): `--ext ZMS,DOC`
        #[arg(long, value_delimiter = ',')]
        ext: Vec<String>,
    },
    /// 索引の状態を表示
    Status {
        #[arg(long)]
        index: std::path::PathBuf,
    },
    /// AI 要約を生成 (Anthropic Messages API)
    Summarize {
        /// アーカイブディレクトリ
        archive_dir: std::path::PathBuf,
        /// 索引ディレクトリ (要約は <index>/summaries/<group_id>.json に保存)
        #[arg(long)]
        index: std::path::PathBuf,
        /// モデル名 (デフォルト: claude-sonnet-4-6)
        #[arg(long, default_value = MODEL_SONNET)]
        model: String,
        /// 言語 (ja or en)
        #[arg(long, default_value = "ja")]
        lang: String,
        /// 既存サマリも上書きして再生成
        #[arg(long)]
        force: bool,
        /// この USD コストを超えたら以降のグループを skip
        #[arg(long)]
        max_cost_usd: Option<f64>,
        /// dry-run: API 呼ばずプロンプトだけ表示
        #[arg(long)]
        dry_run: bool,
        /// グルーピング設定 TOML (フロッピー複数枚を1サマリに束ねる)
        #[arg(long)]
        groups: Option<std::path::PathBuf>,
        /// API 呼び出し間の sleep 秒数 (Anthropic Tier 1 の 30K TPM 制限対策。
        /// 各グループ ~10K tokens なので 25-30 秒推奨)
        #[arg(long, default_value_t = 0)]
        rate_sleep: u64,
    },
    /// グルーピング結果のプレビュー (API 呼ばない)
    ShowGroups {
        archive_dir: std::path::PathBuf,
        /// グルーピング設定 TOML (省略時は全イメージ単独)
        #[arg(long)]
        groups: Option<std::path::PathBuf>,
    },
    /// 構造化メタデータ抽出 (Phase 5b)。archive_dir 配下の全イメージにプラグインを適用。
    Extract {
        archive_dir: std::path::PathBuf,
        #[arg(long)]
        index: std::path::PathBuf,
        /// プラグイン名で絞り込み (例: dennou)。省略時は全プラグイン
        #[arg(long)]
        schema: Option<String>,
        /// 既に同 plugin/version で抽出済みでも再処理する
        #[arg(long)]
        force: bool,
    },
    /// 単一イメージに対して構造化抽出を実行し標準出力に表示 (索引には書かない)
    ExtractOne {
        image_path: std::path::PathBuf,
        /// 使うプラグイン名 (デフォルト: dennou)
        #[arg(long, default_value = "dennou")]
        schema: String,
        /// JSON 整形出力 (デフォルト) / pretty で1行サマリ
        #[arg(long, default_value = "json")]
        format: String,
    },
    /// 構造化フィールドへのクエリ (CSV / JSON 出力)
    Query {
        /// プリセットビュー: dennou_tracks
        #[arg(long, default_value = "dennou_tracks")]
        view: String,
        #[arg(long)]
        index: std::path::PathBuf,
        /// 出力形式: json / csv
        #[arg(long, default_value = "csv")]
        format: String,
        /// 出力先 (省略時は標準出力)
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// 最大行数 (デフォルト 5000)
        #[arg(long, default_value_t = 5000)]
        limit: usize,
        /// 号番号下限
        #[arg(long)]
        issue_no_min: Option<u64>,
        /// 号番号上限
        #[arg(long)]
        issue_no_max: Option<u64>,
        /// "main" / "extra" / "all"
        #[arg(long)]
        issue_kind: Option<String>,
        /// 投稿者部分一致
        #[arg(long)]
        submitter: Option<String>,
        /// タイトル部分一致
        #[arg(long)]
        title: Option<String>,
        /// エンジン部分一致 (例: Z-MUSIC)
        #[arg(long)]
        engine: Option<String>,
        /// ソート順: issue_asc / issue_desc / title_asc
        #[arg(long, default_value = "issue_asc")]
        order_by: String,
    },
}

/// プラグインカタログ。新プラグインを足すならここに登録する。
fn build_plugins(filter: Option<&str>) -> Vec<Box<dyn SchemaPlugin>> {
    let all: Vec<Box<dyn SchemaPlugin>> = vec![Box::new(DennouPlugin)];
    match filter {
        Some(name) => all.into_iter().filter(|p| p.name() == name).collect(),
        None => all,
    }
}

fn parse_order_by(s: &str) -> Result<OrderBy> {
    match s {
        "issue_asc" => Ok(OrderBy::IssueAsc),
        "issue_desc" => Ok(OrderBy::IssueDesc),
        "title_asc" => Ok(OrderBy::TitleAsc),
        other => Err(anyhow::anyhow!(
            "Unknown order_by: {}. Expected one of: issue_asc, issue_desc, title_asc",
            other
        )),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::SchemaInfo => {
            let s = ArchiveSchema::build();
            println!("xdf-index schema fields:");
            for (_, fe) in s.schema.fields() {
                println!("  {}", fe.name());
            }
        }
        Cmd::Build {
            archive_dir,
            out,
            fresh,
        } => {
            let stats = build_index(
                &archive_dir,
                &out,
                BuildOpts {
                    fresh,
                    ..Default::default()
                },
            )?;
            println!(
                "Indexed: {} new image(s), {} skipped (already in index), {} files total",
                stats.images_indexed, stats.images_skipped, stats.files_indexed
            );
        }
        Cmd::Search {
            query,
            limit,
            index,
            ext,
        } => {
            let searcher = Searcher::open(&index)?;
            let hits = searcher.search_with(xdf_index::searcher::SearchOpts {
                query,
                ext,
                limit,
            })?;
            if hits.is_empty() {
                eprintln!("(no hits)");
                return Ok(());
            }
            for h in hits {
                println!(
                    "{:.3}  {}:{}:{}  ({} B  .{})",
                    h.score, h.image_path, h.partition, h.file_path, h.size, h.ext
                );
                if !h.excerpt.is_empty() {
                    let first_line = h.excerpt.lines().next().unwrap_or("");
                    let preview: String = first_line.chars().take(100).collect();
                    println!("    │ {}", preview);
                }
            }
        }
        Cmd::Status { index } => {
            let searcher = Searcher::open(&index)?;
            println!("Index: {}", index.display());
            println!("  documents: {}", searcher.doc_count());
            let summarized =
                xdf_index::summary::list_summarized_ids(&index).unwrap_or_default();
            println!("  summaries: {}", summarized.len());
            // 索引の作り方の版。作り直しが要るなら、ここで理由ごと伝える。
            match xdf_index::index_version::read_paths_version_at(&index) {
                Some(v) => {
                    println!("  paths: v{v}");
                    if let Some(note) = xdf_index::index_version::staleness_note(v) {
                        println!();
                        println!("{note}");
                    }
                }
                None => println!("  paths: 不明 (索引のメタを読めませんでした)"),
            }
        }
        Cmd::Summarize {
            archive_dir,
            index,
            model,
            lang,
            force,
            max_cost_usd,
            dry_run,
            groups,
            rate_sleep,
        } => {
            let opts = SummarizeOpts {
                model,
                lang,
                force,
                max_cost_usd,
                dry_run,
                rate_sleep_secs: rate_sleep,
                ..Default::default()
            };
            let cfg = match groups.as_ref() {
                Some(p) => Some(GroupConfig::load(p)?),
                None => None,
            };
            let stats = summarize_archive(&archive_dir, &index, opts, cfg.as_ref())?;
            println!(
                "Summarized: {} new, {} skipped (already cached), {} failed",
                stats.processed, stats.skipped_existing, stats.failed
            );
            println!(
                "Tokens: {} in / {} out  ~${:.4}",
                stats.total_input_tokens, stats.total_output_tokens, stats.total_cost_usd
            );
        }
        Cmd::Extract {
            archive_dir,
            index,
            schema,
            force,
        } => {
            let plugins = build_plugins(schema.as_deref());
            if plugins.is_empty() {
                anyhow::bail!(
                    "No plugins matched filter {:?}. Available: dennou",
                    schema
                );
            }
            let opts = ExtractOpts {
                force,
                only_plugin: schema.clone(),
                ..Default::default()
            };
            let stats = extract_archive(&archive_dir, &index, &plugins, opts)?;
            println!(
                "Extract: {} seen, {} extracted, {} already-extracted (skipped), {} records added",
                stats.images_seen,
                stats.images_extracted,
                stats.images_skipped_extracted,
                stats.records_added
            );
            for (name, count) in &stats.by_plugin {
                println!("  plugin {}: {} image(s)", name, count);
            }
        }
        Cmd::ExtractOne {
            image_path,
            schema,
            format,
        } => {
            let plugins = build_plugins(Some(&schema));
            let plugin = plugins
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("Unknown plugin: {}", schema))?;
            let results = extract_one_image_dry(&*plugin, &image_path)?;
            match format.as_str() {
                "json" => {
                    let combined: Vec<&serde_json::Value> = results
                        .iter()
                        .flat_map(|r| r.records.iter().map(|rec| &rec.payload))
                        .collect();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&combined)
                            .unwrap_or_else(|_| "[]".into())
                    );
                }
                "pretty" => {
                    for r in &results {
                        println!(
                            "[{} records, {} warnings]",
                            r.records.len(),
                            r.warnings.len()
                        );
                        for rec in &r.records {
                            let title = rec
                                .payload
                                .get("title")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?");
                            let composer = rec
                                .payload
                                .get("composer")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let submitter = rec
                                .payload
                                .get("submitter")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            println!(
                                "  #{} {:5} kind={} role={} title={:?} composer={:?} submitter={:?}",
                                rec.issue_no,
                                rec.source_path,
                                rec.issue_kind,
                                rec.issue_role,
                                title,
                                composer,
                                submitter
                            );
                        }
                        for w in &r.warnings {
                            println!("  ⚠ {}", w);
                        }
                    }
                }
                other => anyhow::bail!("Unknown --format: {} (use json or pretty)", other),
            }
        }
        Cmd::Query {
            view,
            index,
            format,
            out,
            limit,
            issue_no_min,
            issue_no_max,
            issue_kind,
            submitter,
            title,
            engine,
            order_by,
        } => {
            let view = MetadataView::from_str(&view)?;
            let opts = QueryOpts {
                view,
                filters: MetadataFilters {
                    issue_no_min,
                    issue_no_max,
                    issue_kind,
                    submitter_contains: submitter,
                    title_contains: title,
                    engine_contains: engine,
                },
                order_by: parse_order_by(&order_by)?,
                limit,
            };
            let result = metadata::run_query(&index, &opts)?;
            let body = match format.as_str() {
                "csv" => metadata::rows_to_csv(&result),
                "json" => serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|_| "{}".into()),
                other => anyhow::bail!("Unknown --format: {} (use csv or json)", other),
            };
            if let Some(path) = out {
                std::fs::write(&path, body.as_bytes())?;
                eprintln!(
                    "Wrote {} row(s){} → {}",
                    result.row_count,
                    if result.truncated { " (truncated)" } else { "" },
                    path.display()
                );
            } else {
                print!("{}", body);
            }
        }
        Cmd::ShowGroups {
            archive_dir,
            groups,
        } => {
            let cfg = match groups.as_ref() {
                Some(p) => GroupConfig::load(p)?,
                None => GroupConfig::empty(),
            };
            let resolved = show_groups(&archive_dir, &cfg)?;
            let mut multi = 0;
            for g in &resolved {
                let tag = if g.members.len() == 1 { "solo" } else { "group" };
                println!(
                    "[{}] {} ({} member{}) origin={}",
                    tag,
                    g.id,
                    g.members.len(),
                    if g.members.len() == 1 { "" } else { "s" },
                    g.origin
                );
                for m in &g.members {
                    let name = m
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?");
                    println!("    {}", name);
                }
                if g.members.len() > 1 {
                    multi += 1;
                }
            }
            println!(
                "\n{} groups total ({} multi-disk groups, {} solo)",
                resolved.len(),
                multi,
                resolved.len() - multi
            );
        }
    }
    Ok(())
}
