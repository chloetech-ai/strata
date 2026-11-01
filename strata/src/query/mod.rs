//! SQL front end, planner and executor (DESIGN.md §6–7).
//!
//! * [`sql`]: parses the dialect and binds it to a schema.
//! * [`ddl`]: `CREATE TABLE` and `DROP TABLE`.
//! * [`tables`]: the tables a query reads.
//! * [`logical`]: the bound query: projection, predicate tree, score.
//! * [`rank`]: fused scores (RRF, weighted sums, products) as one expression
//!   over their leaves, and its evaluator.
//! * [`ir`]: the plan of a statement, in JSON and in `EXPLAIN`.
//! * [`agg`]: aggregates and groups.
//! * [`stats`]: collection statistics exchanged with a coordinator.
//! * [`memory`]: the memory breaker queries count what they hold against.
//! * [`steps`]: a query's profile, the timed steps of its execution.
//! * [`model`]: ranking models (`gbdt()`), read from LightGBM and XGBoost
//!   files.
//! * [`cross_encoder`]: cross-encoders (`cross_encoder()`), which a global
//!   rerank phase scores rows' text with.

pub mod agg;
pub mod compound;
pub mod cross_encoder;
pub mod ddl;
#[cfg(feature = "datafusion")]
pub mod df;
pub mod exec;
pub mod fusion;
pub mod ir;
pub mod logical;
pub mod memory;
pub mod model;
pub mod ops;
pub mod parse;
pub mod plan;
pub mod profile;
pub mod pool;
pub mod rank;
pub mod sequence;
pub mod sql;
pub mod stats;
pub mod steps;
pub mod tables;

pub use logical::{AggFn, Decay, DecayShape, Direction, Expand, Fuzzy, GraphKind, GraphQuery, Having, OrderBy, PathTest, Pred, Projection, Score, Seeds, Select, Tiebreak};
pub use sql::{bind, bind_command, bind_write, Assign, Command, Conflict, Params, Write};
