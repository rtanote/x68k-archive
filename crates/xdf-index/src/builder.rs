//! 索引ビルダ (T-3.3) + 増分更新 (T-3.5)

use crate::document::{
    attr_str, build_document, extract_text_body, mtime_to_unix, DocFields,
};
use crate::lzh::{index_lzh_bytes, LzhOpts};
use crate::schema::ArchiveSchema;
use anyhow::{Context, Result};
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tantivy::{Index, IndexWriter, TantivyDocument};
use xdf_fs::bpb::Bpb;
use xdf_fs::direntry::Attr;
use xdf_fs::fs::Filesystem;
use xdf_fs::image::{DiskImage, OpenedImage};
use xdf_fs::walker;

/// ビルドオプション
pub struct BuildOpts {
    /// IndexWriter 用バッファサイズ (バイト)。tantivy 推奨 50MB+
    pub writer_memory: usize,
    /// 既存索引を全消ししてからビルド (false = 増分追加)
    pub fresh: bool,
    /// LZH アーカイブの中身を索引するか (true = 索引、false = LZH を1ファイルとして扱うのみ)
    pub index_lzh: bool,
    /// LZH 展開時のオプション
    pub lzh_opts: LzhOpts,
}

impl Default for BuildOpts {
    fn default() -> Self {
        Self {
            writer_memory: 50_000_000,
            fresh: false,
            index_lzh: true,
            lzh_opts: LzhOpts::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct BuildStats {
    pub images_indexed: usize,
    pub images_skipped: usize,
    pub files_indexed: usize,
}

/// archive_dir を再帰スキャンして全 X68000 イメージを索引化
pub fn build_index<A: AsRef<Path>, I: AsRef<Path>>(
    archive_dir: A,
    index_dir: I,
    opts: BuildOpts,
) -> Result<BuildStats> {
    let archive_dir = archive_dir.as_ref();
    let index_dir = index_dir.as_ref();
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("Cannot create index dir {:?}", index_dir))?;

    let schema = ArchiveSchema::build();

    let index = if opts.fresh {
        // 既存索引を消して新規作成
        clear_index_dir(index_dir)?;
        Index::create_in_dir(index_dir, schema.schema.clone())?
    } else if index_dir.join("meta.json").exists() {
        Index::open_in_dir(index_dir)?
    } else {
        Index::create_in_dir(index_dir, schema.schema.clone())?
    };

    let existing_ids = if opts.fresh {
        HashSet::new()
    } else {
        collect_existing_image_ids(&index, &schema)?
    };

    let mut writer: IndexWriter = index.writer(opts.writer_memory)?;
    let mut stats = BuildStats::default();

    let images = discover_images(archive_dir)?;
    for img_path in images {
        let id = compute_image_id(&img_path)
            .with_context(|| format!("Cannot hash {:?}", img_path))?;
        if existing_ids.contains(&id) {
            stats.images_skipped += 1;
            continue;
        }
        match index_one_image(&schema, &mut writer, &img_path, &id, &opts) {
            Ok(file_count) => {
                stats.images_indexed += 1;
                stats.files_indexed += file_count;
            }
            Err(e) => {
                eprintln!("warning: cannot index {}: {}", img_path.display(), e);
            }
        }
    }

    // 索引に「どの版の規則で作ったか」を刻む。公開リポジトリなので、
    // バイナリだけ更新して索引を作り直さなかった利用者が気づけるように。
    // 置き場は tantivy が使わない IndexMeta.payload（crate::index_version）。
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&crate::index_version::current_payload());
    prepared.commit()?;
    Ok(stats)
}

/// archive_dir 配下から XDF/HDS/HDF/IMG を再帰列挙
pub fn discover_images(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    discover_recursive(dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// 索引対象から除外するディレクトリ名
/// - Synology のごみ箱 `#recycle`
/// - macOS の `.Trashes`, `.fseventsd`, `.Spotlight-V100`
/// - Windows の `$RECYCLE.BIN`, `System Volume Information`
fn is_excluded_dir(name: &str) -> bool {
    matches!(
        name,
        "#recycle"
            | ".Trashes"
            | ".Trash"
            | ".fseventsd"
            | ".Spotlight-V100"
            | "$RECYCLE.BIN"
            | "System Volume Information"
    )
}

fn discover_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // ごみ箱・OS メタディレクトリは再帰しない
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if is_excluded_dir(name) {
                continue;
            }
            discover_recursive(&path, out)?;
        } else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            let ext = ext.to_ascii_lowercase();
            if matches!(ext.as_str(), "xdf" | "hds" | "hdf" | "img") {
                out.push(path);
            }
        }
    }
    Ok(())
}

/// 内容ハッシュの先頭 8 バイト (= 16 hex chars) を image_id とする
pub fn compute_image_id(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for b in digest.iter().take(8) {
        hex.push_str(&format!("{:02x}", b));
    }
    Ok(hex)
}

fn collect_existing_image_ids(
    index: &Index,
    schema: &ArchiveSchema,
) -> Result<HashSet<String>> {
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let mut ids = HashSet::new();
    for sr in searcher.segment_readers() {
        let store = sr.get_store_reader(50)?;
        let max = sr.max_doc();
        for doc_id in 0..max {
            if let Ok(doc) = store.get::<TantivyDocument>(doc_id) {
                if let Some(v) = doc.get_first(schema.image_id) {
                    if let Some(s) = value_as_str(v) {
                        ids.insert(s.to_string());
                    }
                }
            }
        }
    }
    Ok(ids)
}

fn value_as_str(v: &tantivy::schema::OwnedValue) -> Option<&str> {
    match v {
        tantivy::schema::OwnedValue::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn clear_index_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        for entry in std::fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_file() {
                std::fs::remove_file(&p)?;
            }
        }
    }
    Ok(())
}

fn index_one_image(
    schema: &ArchiveSchema,
    writer: &mut IndexWriter,
    img_path: &Path,
    image_id: &str,
    opts: &BuildOpts,
) -> Result<usize> {
    let opened = OpenedImage::open(img_path)?;
    let img_path_str = img_path.to_string_lossy().to_string();

    match opened {
        OpenedImage::Floppy(img) => {
            let fs = Filesystem::open(&img)?;
            index_filesystem(schema, writer, &fs, &img_path_str, image_id, 0, opts)
        }
        OpenedImage::Hdd(hdd) => {
            let mut total = 0;
            for idx in 0..hdd.partitions().len() {
                let part = match hdd.partition(idx) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("  partition {} skipped: {}", idx, e);
                        continue;
                    }
                };
                let boot = match part.read_sector(0) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("  partition {} boot read failed: {}", idx, e);
                        continue;
                    }
                };
                let bpb = match Bpb::parse_hdd(boot) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("  partition {} BPB parse failed: {}", idx, e);
                        continue;
                    }
                };
                let fs = match Filesystem::open_with_bpb(&part, bpb) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("  partition {} mount failed: {}", idx, e);
                        continue;
                    }
                };
                total += index_filesystem(
                    schema,
                    writer,
                    &fs,
                    &img_path_str,
                    image_id,
                    idx,
                    opts,
                )?;
            }
            Ok(total)
        }
    }
}

fn index_filesystem(
    schema: &ArchiveSchema,
    writer: &mut IndexWriter,
    fs: &Filesystem,
    image_path: &str,
    image_id: &str,
    partition: usize,
    opts: &BuildOpts,
) -> Result<usize> {
    let mut count = 0usize;
    let mut error: Option<anyhow::Error> = None;

    walker::walk(fs, |item| {
        // ボリュームラベル / ディレクトリ / 削除エントリ / ".." はスキップ
        if item.entry.attr.contains(Attr::VOLUME)
            || item.entry.attr.contains(Attr::DIRECTORY)
        {
            return true;
        }
        match index_one_file(
            schema, writer, fs, image_path, image_id, partition, item, opts,
        ) {
            Ok(n) => count += n,
            Err(e) => {
                error = Some(e);
                return false;
            }
        }
        true
    })?;

    if let Some(e) = error {
        return Err(e);
    }
    Ok(count)
}

/// ファイル1個 = 1 Document + (LZH の場合は中身のメンバーも索引)
fn index_one_file(
    schema: &ArchiveSchema,
    writer: &mut IndexWriter,
    fs: &Filesystem,
    image_path: &str,
    image_id: &str,
    partition: usize,
    item: &xdf_fs::walker::WalkItem<'_>,
    opts: &BuildOpts,
) -> Result<usize> {
    let entry = item.entry;
    let display_name = entry.display_name();
    let ext_upper = entry.ext.to_ascii_uppercase();
    let attr = attr_str(entry.attr);

    let body_pair = extract_text_body(fs, entry);
    let (body, excerpt) = match &body_pair {
        Some((b, e)) => (Some(b.as_str()), Some(e.as_str())),
        None => (None, None),
    };
    let doc = build_document(
        schema,
        DocFields {
            image_path,
            image_id,
            partition,
            file_path: &item.path,
            file_name: &display_name,
            ext: &ext_upper,
            size: entry.size as u64,
            mtime_unix: mtime_to_unix(entry),
            attr: &attr,
            body,
            excerpt,
        },
    );
    writer.add_document(doc)?;
    let mut count = 1usize;

    // LZH の中身も索引する
    if opts.index_lzh && (ext_upper == "LZH" || ext_upper == "LHA") {
        match fs.read_file(entry) {
            Ok(bytes) => {
                let inner_count = index_lzh_bytes(
                    schema,
                    writer,
                    &bytes,
                    image_path,
                    image_id,
                    partition,
                    &item.path,
                    opts.lzh_opts.max_depth,
                    &opts.lzh_opts,
                )
                .unwrap_or_else(|e| {
                    eprintln!("  lzh failed for {}: {}", item.path, e);
                    0
                });
                count += inner_count;
            }
            Err(e) => eprintln!("  cannot read LZH {}: {}", item.path, e),
        }
    }
    Ok(count)
}
