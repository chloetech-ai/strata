//! The cluster's protocols as decisions with no I/O ("sans-IO" cores).
//!
//! A core holds the state a protocol decides by and answers each event (a
//! call that arrived, an answer, a timer) with what to do next: a call to
//! make, a file to keep, a wait. It reads no clock (the caller passes the
//! time), opens no file and makes no call, so the same code runs in the
//! server, which does the I/O, and in the simulator ([`crate::sim`]), which
//! runs a seeded cluster on a model network, clock and disk and checks what
//! the protocol promises. A failing seed replays exactly.
//!
//! * [`shard`]: a shard's copies: votes and elections, a replica taking
//!   what its primary ships, a primary shipping, counting the copies that
//!   hold a write, fencing, and its lease on reads.
//!
//! The server does not drive these cores yet: [`crate::replication`]
//! decides on its own, and the cores mirror its decisions so that the
//! simulator exercises them first. Both fix the three bugs the simulator
//! found, which [`crate::sim::Bug`] puts back: a replica asks above the
//! newest term a copy told it ([`shard::Replica::heard`]), a replica
//! promoted by hand waits out the lease of the primary it replaced
//! ([`shard::handover_hold`]), and a replica takes what a newer primary
//! holds of an older term past its last
//! ([`strata::storage::Database::apply_shipped`], as
//! [`crate::sim::db::ModelDb::apply_shipped`] models it).

pub mod shard;
