//! xdf-index: X68000 アーカイブの全文索引
//!
//! tantivy を使って複数の XDF / HDS / HDF にまたがるメタ + 本文索引を構築・検索する。

pub mod anthropic;
pub mod builder;
pub mod dennou;
pub mod document;
pub mod extract;
pub mod grouping;
pub mod index_version;
pub mod lzh;
pub mod metadata;
pub mod schema;
pub mod searcher;
pub mod summarizer;
pub mod summary;
pub mod text_norm;
