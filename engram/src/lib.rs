//! Engram: automatic long-term context for coding agents, stored in Strata.
//!
//! Claude Code hooks feed every prompt, tool call, answer and compaction
//! summary to a per-project daemon ([`daemon`]), which stores them as
//! [`record::Record`]s in one Strata file ([`store`]). On each prompt the
//! daemon searches that history, gates and packs the result ([`recall`]),
//! and returns it as context the model sees. An MCP server ([`mcp`]) gives
//! the model search, timeline and SQL tools over the same store.

pub mod benchdata;
pub mod capture;
pub mod daemon;
pub mod distill;
pub mod embed;
pub mod hook;
pub mod inspect;
pub mod install;
pub mod mcp;
pub mod paths;
pub mod recall;
pub mod record;
pub mod redact;
pub mod store;
pub mod transcript;

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<strata::Error> for Error {
    fn from(e: strata::Error) -> Self {
        Error(format!("strata: {e}"))
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

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
