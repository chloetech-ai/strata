//! SQL front end, planner and executor (DESIGN.md §6–7).
//!
//! * [`sql`]: parses the dialect and binds it to a schema.
//! * [`logical`]: the bound query: projection, predicate tree, score.

pub mod exec;
pub mod fusion;
pub mod logical;
pub mod plan;
pub mod sql;

pub use logical::{Direction, GraphKind, GraphQuery, OrderBy, Pred, Projection, Score, Seeds, Select, Tiebreak};
pub use sql::{bind, Params};
