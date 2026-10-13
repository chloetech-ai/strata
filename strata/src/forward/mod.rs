//! Forward index (DESIGN.md §5.2): columns with zone maps, keyword and
//! range postings, doc store.
//!
//! * [`NumericColumn`] (INT, FLOAT): values as order-preserving integer
//!   keys in 128-document frame-of-reference blocks with min/max zone maps;
//!   O(1) access, range scans, probes, histogram estimates.
//! * [`RangeIndex`]: an equi-depth histogram's buckets as Pyramid lists;
//!   [`eval_range`] picks between it and a column scan.
//! * [`KeywordColumn`] (KEYWORD): sorted dictionary, bit-packed codes and a
//!   Pyramid list per value; equality, `IN`, probes and facet counts.
//! * [`DocStore`]: stored fields as rows in LZ4-compressed blocks.
//!
//! Every reader is a zero-copy view of its segment sections:
//!
//! | section | contents |
//! |---|---|
//! | `col.<f>` | numeric column (kind, blocks) |
//! | `col.<f>.hist` | equi-depth histogram |
//! | `range.<f>` | histogram bucket lists |
//! | `kw.<f>.dict`, `kw.<f>.codes`, `kw.<f>.post` | keyword dictionary, codes, value lists |
//! | `store.index`, `store.data` | doc store block index and blocks |
//!
//! Build options: `forward.range_buckets` (histogram buckets, default 64;
//! 0 builds neither histogram buckets nor range postings),
//! `forward.kw_rare` (values with fewer documents use a plain docID array
//! instead of a Pyramid list, default 64), `forward.store_block`
//! (uncompressed doc store block size, default 16384).

mod blocks;
mod keyword;
mod lists;
mod numeric;
mod range;
mod store;

pub use blocks::{KeyRange, BLOCK};
pub use keyword::{CodeSetFilter, FacetStrategy, KeywordColumn, KeywordFilter, KeywordSizes, DEFAULT_RARE};
pub use numeric::{float_key, ColumnRangeFilter, Histogram, NumericColumn, NumericKind, DEFAULT_BUCKETS, MAX_SCALE};
pub use range::{choose_range_strategy, count_range, eval_range, range_costs, RangeIndex, RangePlan, RangeStrategy};
pub use store::{DocStore, DEFAULT_BLOCK};

use crate::error::{Error, Result};
use crate::schema::{Field, FieldType, Schema};
use crate::segment::build::BuildOptions;
use crate::segment::{FieldIndexBuilder, SegmentWriter};
use crate::value::Document;

/// Builder for an indexed KEYWORD, INT or FLOAT field.
pub fn field_builder(field: &Field, opts: &BuildOptions) -> Result<Box<dyn FieldIndexBuilder>> {
    match field.ty {
        FieldType::Keyword => Ok(Box::new(keyword::KeywordBuilder::new(field, opts)?)),
        FieldType::Int | FieldType::Float => Ok(Box::new(numeric::NumericBuilder::new(field, opts)?)),
        _ => Err(Error::Schema(format!("field {} has no forward column", field.name))),
    }
}

/// Build option `key` as a number, `default` if unset.
fn usize_option(opts: &BuildOptions, key: &str, default: usize) -> Result<usize> {
    opts.get(key).map_or(Ok(default), |v| v.parse().map_err(|_| Error::Schema(format!("{key} must be a number, not {v}"))))
}

/// Writes the doc store (stored, non-vector fields of every document).
pub fn build_doc_store(schema: &Schema, docs: &[Document], opts: &BuildOptions, out: &mut SegmentWriter) -> Result<()> {
    store::build(schema, docs, opts, out)
}
