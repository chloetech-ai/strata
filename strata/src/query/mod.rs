//! SQL front end, planner and executor (DESIGN.md §6–7).
//!
//! * [`sql`]: parses the dialect and binds it to a schema.
//! * [`logical`]: the bound query: projection, predicate tree, score.
//! * [`agg`]: aggregates and groups.
//! * [`stats`]: collection statistics exchanged with a coordinator.

pub mod agg;
pub mod compound;
pub mod exec;
pub mod fusion;
pub mod logical;
pub mod plan;
pub mod sql;
pub mod stats;

pub use logical::{AggFn, Decay, DecayShape, Direction, Expand, Fuzzy, GraphKind, GraphQuery, Having, OrderBy, Pred, Projection, Score, Seeds, Select, Tiebreak};
pub use sql::{bind, bind_write, Assign, Params, Write};
