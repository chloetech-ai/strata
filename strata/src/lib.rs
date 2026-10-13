//! Strata: an embedded, single-file search database.
//!
//! One file holds a table whose documents are indexed three ways: posting
//! lists (full text and keyword filters), a columnar forward index (structured
//! fields, doc store) and vector ANN indexes. Queries are SQL with search
//! functions, planned by a cost-based optimizer. See `DESIGN.md`.
//!
//! * [`schema`], [`value`]: tables, field types, documents.
//! * [`segment`]: the immutable unit of storage, a directory of aligned,
//!   named sections, and the builder that turns documents into one.
//! * [`storage`]: the single-file pager, commit protocol, WAL, snapshots.
//! * [`text`], [`forward`], [`vector`]: the three index families, and
//!   [`graph`]: edges between rows, with traversals.
//! * [`query`]: SQL front end, planner and executor.
//! * [`data`], [`metrics`]: BEIR loaders and retrieval metrics for the
//!   evaluation.
//! * [`workload`]: the query workloads shared with the baselines, and the
//!   measurement harness; [`baselines`] (feature `baselines`): SQLite and
//!   Tantivy behind the same interface.

#[cfg(feature = "baselines")]
pub mod baselines;
pub mod data;
pub mod db;
pub mod error;
pub mod forward;
pub mod graph;
pub mod iostat;
pub mod metrics;
pub mod query;
pub mod schema;
pub mod segment;
pub mod storage;
pub mod text;
pub mod value;
pub mod vector;
pub mod workload;

pub use error::{Error, Result};
pub use schema::{Field, FieldType, Schema, VectorIndexKind};
pub use segment::DocId;
pub use value::{Document, Value};
