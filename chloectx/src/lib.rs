//! ChloeCtx: automatic long-term context for coding agents, stored in ChloeDB.
//!
//! Claude Code hooks feed every prompt, tool call, answer and compaction
//! summary to a per-project daemon ([`daemon`]), which stores them as
//! [`record::Record`]s in one Strata file ([`store`]). On each prompt the
//! daemon searches that history, gates and packs the result ([`recall`]),
//! and returns it as context the model sees. An MCP server ([`mcp`]) gives
//! the model search, timeline and SQL tools over the same store.

pub mod anchor;
pub mod app;
pub mod capsule;
pub mod benchdata;
pub mod blame;
pub mod capture;
pub mod cluster;
pub mod consolidate;
pub mod context;
pub mod daemon;
pub mod deep;
pub mod distill;
pub mod doctor;
pub mod embed;
pub mod envelope;
pub mod env;
pub mod fixes;
pub mod health;
pub mod hook;
pub mod inspect;
pub mod install;
pub mod llm;
pub mod mcp;
pub mod next_files;
pub mod paths;
pub mod plugin;
pub mod recall;
pub mod receipt;
pub mod record;
pub mod redact;
pub mod report;
pub mod snapshots;
pub mod sqlguard;
pub mod store;
pub mod stores;
pub mod sync;
pub mod transcript;
pub mod ui;
pub mod version;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<chloedb::Error> for Error {
    fn from(e: chloedb::Error) -> Self {
        Error(format!("chloedb: {e}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(format!("io: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error(format!("json: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn err(m: impl Into<String>) -> Error {
    Error(m.into())
}

/// Seconds since the Unix epoch, moved by `CHLOECTX_CLOCK_OFFSET` seconds
/// when it is set: for a recording replayed as though it happened earlier
/// (`demo/seed.py` plays three weeks of sessions in a few minutes, each
/// with its own daemon and offset). Read once per process.
pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) + clock_offset()
}

/// `CHLOECTX_CLOCK_OFFSET`, in seconds (0 when unset or not a number).
pub fn clock_offset() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| std::env::var("CHLOECTX_CLOCK_OFFSET").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0))
}
