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
//! * [`slots`]: the cluster map: a shard taking a map and checking what is
//!   routed to it, holding writes to slots moving from it and exporting
//!   their rows; a coordinator changing the map ([`slots::Change`]) and
//!   moving slots between shards ([`slots::MoveRun`]).
//!
//! The server runs [`slots`]: a shard takes maps and checks rows and
//! epochs with it ([`crate::service::Service::set_slots`]), and a
//! coordinator changes the map ([`crate::coord::Coordinator::change`]) and
//! moves slots ([`crate::coord::moves`]) by its steps, which find what the
//! simulator's [`crate::sim::cluster::Bug`]s put back.
//!
//! The server runs [`shard`] too: [`crate::replication`] carries out its
//! steps for a replica and a primary, [`crate::service`]'s failover looks
//! when [`shard::Failover`] says, and a server starts again in the part
//! [`shard::restart`] gives it. Both cores fix the bugs the simulator
//! found, which [`crate::sim::Bug`] puts back: a shard's first copies vote
//! from the start ([`shard::Replica::open`]), a replica asks above the
//! newest term a copy told it ([`shard::Replica::heard`]), a replica
//! promoted by hand waits out the lease of the primary it replaced
//! ([`shard::handover_hold`]), and a replica takes what a newer primary
//! holds of an older term past its last
//! ([`strata::storage::Database::apply_shipped`], as
//! [`crate::sim::db::ModelDb::apply_shipped`] models it).

pub mod shard;
pub mod slots;
