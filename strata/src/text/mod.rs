//! Full-text index (DESIGN.md §5.1): analyzer, term dictionary, scored
//! postings with block impacts, positions, norms, boolean retrieval and BM25
//! top-k.
//!
//! A TEXT field `f` with `indexed` set is stored in five sections:
//!
//! | section | contents |
//! |---|---|
//! | `text.f.meta` | format, counts, totals (JSON) |
//! | `text.f.dict` | front-coded term dictionary ([`dict`]) |
//! | `text.f.post` | posting lists: docIDs, frequencies, impacts ([`postings`]) |
//! | `text.f.pos` | positions, if the field has them ([`cursor`]) |
//! | `text.f.norms` | one-byte document lengths ([`codec::encode_norm`]) |
//!
//! Build options: `text.postings = loom | bp128` (docID encoding, default
//! `loom`) and `text.loom_min_df` (shortest list stored as Loom in the Loom
//! format, default [`postings::LOOM_MIN_DF`] = 512).
//!
//! Reading: [`TextFieldReader::open`] maps nothing and copies nothing; every
//! array is a view into the segment. Queries: [`parse_match`] /
//! [`BoolQuery`] for boolean retrieval ([`TextFieldReader::eval`]), and
//! [`TextFieldReader::top_k`] for BM25 ranking.

pub mod analyzer;
pub mod bm25;
pub mod builder;
pub mod codec;
pub mod cursor;
pub mod dict;
pub mod postings;
pub mod query;
pub mod reader;

pub use analyzer::{analyze, analyze_query, analyze_query_with, analyze_with_positions, QUERY_STOPWORDS};
pub use bm25::{Bm25Params, CollectionStats, FilterStrategy, TopKAlgo};
pub use builder::TextFieldBuilder;
pub use cursor::{PostingCursor, END};
pub use postings::PostingsFormat;
pub use query::{parse_match, BoolQuery};
pub use reader::{TextFieldReader, TextSizes};

use crate::error::{Error, Result};
use crate::schema::{Field, FieldType};
use crate::segment::build::BuildOptions;
use crate::segment::FieldIndexBuilder;

/// Builder for an indexed TEXT field.
pub fn field_builder(field: &Field, opts: &BuildOptions) -> Result<Box<dyn FieldIndexBuilder>> {
    let FieldType::Text { positions } = field.ty else {
        return Err(Error::Schema(format!("field {} is not TEXT", field.name)));
    };
    let (format, loom_min_df) = builder::options(opts)?;
    Ok(Box::new(TextFieldBuilder::new(&field.name, positions, format, loom_min_df)))
}
