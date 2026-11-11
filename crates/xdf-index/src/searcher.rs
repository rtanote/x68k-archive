//! 索引クエリ実行 (T-3.4)

use crate::schema::ArchiveSchema;
use anyhow::{anyhow, Result};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

/// 検索オプション。`query` 空 + `ext` 空はエラー、`ext` のみで全件絞り込み可。
#[derive(Debug, Clone, Default)]
pub struct SearchOpts {
    /// 全文クエリ (空文字列許容)
    pub query: String,
    /// 拡張子フィルタ (大文字統一、複数指定で OR)
    pub ext: Vec<String>,
    /// 上位 N 件
    pub limit: usize,
}

pub struct Searcher {
    schema: ArchiveSchema,
    reader: IndexReader,
    parser: QueryParser,
}

impl Searcher {
    pub fn open<P: AsRef<Path>>(index_dir: P) -> Result<Self> {
        let schema = ArchiveSchema::build();
        let index = Index::open_in_dir(index_dir.as_ref())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        // デフォルトの検索対象フィールド: 本文 + ファイル名
        let parser = QueryParser::for_index(&index, vec![schema.body, schema.file_name]);
        Ok(Self { schema, reader, parser })
    }

    /// 全文クエリのみで検索 (拡張子フィルタなし)
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.search_with(SearchOpts {
            query: query.to_string(),
            ext: vec![],
            limit,
        })
    }

    /// SearchOpts ベースの検索 (拡張子フィルタあり)
    pub fn search_with(&self, opts: SearchOpts) -> Result<Vec<Hit>> {
        if opts.limit == 0 {
            return Ok(Vec::new());
        }
        if opts.query.trim().is_empty() && opts.ext.is_empty() {
            return Err(anyhow!(
                "Either query or ext filter must be provided"
            ));
        }
        let combined = self.build_query(&opts)?;
        let searcher = self.reader.searcher();
        // 0.27 の `TopDocs` はビルダーで、それ自身は `Collector` ではない。
        // スコア順のコレクタは `order_by_score()` が返す。
        let top_docs =
            searcher.search(&*combined, &TopDocs::with_limit(opts.limit).order_by_score())?;
        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, addr) in top_docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            hits.push(doc_to_hit(&self.schema, &doc, score));
        }
        Ok(hits)
    }

    fn build_query(&self, opts: &SearchOpts) -> Result<Box<dyn Query>> {
        let has_query = !opts.query.trim().is_empty();
        let has_ext = !opts.ext.is_empty();

        // ベースクエリ
        let base: Box<dyn Query> = if has_query {
            self.parser.parse_query(&opts.query)?
        } else {
            Box::new(AllQuery)
        };

        if !has_ext {
            return Ok(base);
        }

        // 拡張子フィルタ: 各 ext を Term にして OR で結合
        let ext_subqueries: Vec<(Occur, Box<dyn Query>)> = opts
            .ext
            .iter()
            .map(|e| {
                let upper = e.to_ascii_uppercase();
                let term = Term::from_field_text(self.schema.ext, &upper);
                let q: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                (Occur::Should, q)
            })
            .collect();
        let ext_query: Box<dyn Query> = Box::new(BooleanQuery::new(ext_subqueries));

        // ベース AND 拡張子フィルタ
        Ok(Box::new(BooleanQuery::new(vec![
            (Occur::Must, base),
            (Occur::Must, ext_query),
        ])))
    }

    /// 索引内の総ドキュメント数 (status 用)
    pub fn doc_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub score: f32,
    pub image_path: String,
    pub image_id: String,
    pub partition: u64,
    pub file_path: String,
    pub file_name: String,
    pub ext: String,
    pub size: u64,
    pub mtime: i64,
    pub excerpt: String,
}

fn doc_to_hit(schema: &ArchiveSchema, doc: &TantivyDocument, score: f32) -> Hit {
    let get_str = |f| {
        doc.get_first(f)
            .and_then(value_as_str)
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    let get_u64 = |f| doc.get_first(f).and_then(value_as_u64).unwrap_or(0);
    let get_i64 = |f| doc.get_first(f).and_then(value_as_i64).unwrap_or(0);
    Hit {
        score,
        image_path: get_str(schema.image_path),
        image_id: get_str(schema.image_id),
        partition: get_u64(schema.partition),
        file_path: get_str(schema.file_path),
        file_name: get_str(schema.file_name),
        ext: get_str(schema.ext),
        size: get_u64(schema.size),
        mtime: get_i64(schema.mtime),
        excerpt: get_str(schema.body_excerpt),
    }
}

/// 0.27 で `get_first` が返すのは `CompactDocValue` で、所有値ではない。
/// `Value` 境界にしておけば `&OwnedValue` でも通るので、呼び出し側は変えずに済む。
fn value_as_str<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<&'a str> {
    v.as_str()
}
fn value_as_u64<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<u64> {
    v.as_u64()
}
fn value_as_i64<'a, V: tantivy::schema::Value<'a>>(v: V) -> Option<i64> {
    v.as_i64()
}
