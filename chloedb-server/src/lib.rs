//! Strata over the network.
//!
//! * [`service`]: the operations, over a local database or, as a
//!   coordinator ([`coord`]), over shard servers.
//! * [`http`]: the JSON API (`/v1/...`), Prometheus metrics (`/metrics`)
//!   and the command center ([`ui`], at `/`).
//! * [`grpc`]: the same operations over gRPC (`proto/strata.proto`), which
//!   a coordinator uses to reach its shards; [`plan_pb`]: plans as protobuf.
//! * [`agent`]: answers a question in words by letting a Claude model
//!   query the table.
//! * [`replication`]: a primary shipping its writes to its replicas.
//! * [`cores`]: the cluster's protocols as decisions with no I/O, and
//!   [`sim`], which runs them on a seeded model of a cluster.
//! * [`postgres`]: following a Postgres database by logical replication.
//! * [`media`]: fetching the files MEDIA fields name over HTTP(S).
//! * [`keys`]: API keys with scopes, and the audit log; [`alerts`]: alert
//!   rules over the series.
//! * [`obs`]: logs and request ids; [`metrics`]: what `/metrics` serves;
//!   [`telemetry`]: the running and latest queries, their traces, and the
//!   series of rates and gauges the UI draws; [`logstore`]: the log lines
//!   and queries kept in a Strata database of the server's own, and
//!   searched across a cluster.

pub mod agent;
pub mod answers;
pub mod alerts;
pub mod app_procs;
pub mod admin_cli;
pub mod apps;
pub mod aws;
pub mod cloud;
pub mod digitalocean;
pub mod doctor;
pub mod auth;
pub mod autopilot;
pub mod board;
pub mod budget;
pub mod catalog;
pub mod changes;
pub mod coord;
pub mod copy;
pub mod connectors;
pub mod contain;
pub mod cores;
pub mod demo;
pub mod errors;
pub mod events;
pub mod feedback;
pub mod export;
pub mod fleet;
pub mod follow;
pub mod grpc;
pub mod heap;
pub mod hub;
pub mod hub_follow;
pub mod http;
pub mod idem;
pub mod inspect;
pub mod json;
pub mod judge;
pub mod keys;
pub mod keysync;
pub mod listener;
pub mod logstore;
pub mod mcp;
pub mod names;
pub mod media;
pub mod metrics;
pub mod obs;
pub mod oncall;
pub mod ops;
pub mod otlp;
pub mod operator;
pub mod opensearch;
// fleet: coordinators that know each other.
pub mod peers;
pub mod plan_pb;
// `chloedb-server follow-postgres`: Postgres's logical replication into Strata.
pub mod postgres;
pub mod profiler;
pub mod profiling;
pub mod reads;
pub mod runbooks;
pub mod replication;
pub mod scoped;
pub mod server;
pub mod service;
pub mod sim;
pub mod slices;
pub mod sql_check;
pub mod slots;
pub mod telemetry;
pub mod tenants;
pub mod traffic;
pub mod ui;

/// A builder of the HTTP clients the server makes. They trust the roots
/// built in (webpki-roots), not the platform's store: the `s3` feature's
/// S3 client (object_store) turns on reqwest's native roots for every
/// client in the build, and those are loaded again each time a client is
/// built, which on macOS reads the keychain (about a second each on CI's
/// runners). S3 itself keeps them, in object_store's own client.
pub fn http_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_built_in_native_certs(false)
}

/// Writes a line of a command's answer to stdout. A reader that stops early
/// (`chloedb-server admin catalog | head -1`) closes the pipe, and that ends
/// the command quietly, with exit code 0, as it ends other tools, where
/// `println!` panics (exit code 101).
pub fn print_out(line: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    if let Err(e) = writeln!(out, "{line}").and_then(|()| out.flush()) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("chloedb-server: writing to stdout: {e}");
        std::process::exit(1);
    }
}

pub mod pb {
    tonic::include_proto!("strata.v1");
}

/// An error with the HTTP status it answers with.
#[derive(Clone, Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// Its code ([`crate::errors`]); `None`: told by its status and
    /// sentence ([`crate::errors::classify`]).
    pub code: Option<&'static str>,
}

impl ApiError {
    pub fn bad(m: impl Into<String>) -> ApiError {
        ApiError { code: None, status: 400, message: m.into() }
    }

    pub fn internal(e: impl std::fmt::Display) -> ApiError {
        ApiError { code: None, status: 500, message: e.to_string() }
    }

    /// An error with its code ([`crate::errors::CODES`]).
    pub fn coded(status: u16, code: &'static str, m: impl Into<String>) -> ApiError {
        debug_assert!(crate::errors::lookup(code).is_some(), "{code} is not in errors::CODES");
        ApiError { code: Some(code), status, message: m.into() }
    }

    /// Its code: its own, or as its status and sentence tell
    /// ([`crate::errors::code_of`]).
    pub fn code(&self) -> &'static str {
        crate::errors::code_of(self)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<chloedb::Error> for ApiError {
    fn from(e: chloedb::Error) -> ApiError {
        let status = match &e {
            chloedb::Error::Query(_) | chloedb::Error::Schema(_) => 400,
            chloedb::Error::Busy(_) => 503,
            // As Elasticsearch's circuit breakers.
            chloedb::Error::Memory(_) => 429,
            chloedb::Error::Timeout(_) => 504,
            // A hosted embedding model that refused or did not answer.
            chloedb::Error::Unavailable(_) => 503,
            chloedb::Error::Io(_) | chloedb::Error::Corrupt(_) => 500,
        };
        ApiError { code: None, status, message: e.to_string() }
    }
}

/// Whether a server refused a request whose message it could not decode
/// for nesting deeper than Protobuf's limit: a plan whose expression
/// nests too deep (`a + b + ...` of a hundred terms), the request's fault
/// and not the copy's.
pub fn too_deep_to_decode(s: &tonic::Status) -> bool {
    s.code() == tonic::Code::Internal && s.message().contains("failed to decode Protobuf message") && s.message().contains("recursion limit reached")
}

impl From<tonic::Status> for ApiError {
    fn from(s: tonic::Status) -> ApiError {
        use tonic::Code;
        if too_deep_to_decode(&s) {
            return ApiError {
                code: Some("query_invalid"),
                status: 400,
                message: "query error: the statement nests too deep to be sent to the shards (its plan passes Protobuf's limit of 100 nested messages): write its expressions with fewer levels, or split it".into(),
            };
        }
        let status = match s.code() {
            Code::InvalidArgument => 400,
            Code::NotFound => 404,
            Code::FailedPrecondition => 409,
            Code::AlreadyExists => 422,
            Code::DeadlineExceeded => 504,
            Code::ResourceExhausted => 429,
            Code::Unavailable => 503,
            Code::Unimplemented => 501,
            _ => 502,
        };
        // The code another server's error had ([`crate::errors`]).
        let code = s.metadata().get(ERROR_CODE).and_then(|v| v.to_str().ok()).and_then(crate::errors::lookup).map(|c| c.code);
        ApiError { code, status, message: s.message().to_string() }
    }
}

/// The gRPC metadata that carries an error's code between servers.
pub const ERROR_CODE: &str = "chloedb-error-code";

impl From<ApiError> for tonic::Status {
    fn from(e: ApiError) -> tonic::Status {
        use tonic::Code;
        let code = match e.status {
            400 => Code::InvalidArgument,
            401 => Code::Unauthenticated,
            // A credential's scope does not allow what the call does.
            403 => Code::PermissionDenied,
            // A write by key whose row is not there (`"if_missing": "error"`).
            404 => Code::NotFound,
            // Refused in the state the server is in: an older slot map, a
            // write with the same idempotency key still running.
            409 => Code::FailedPrecondition,
            // An idempotency key used already, for other operations or
            // another statement.
            422 => Code::AlreadyExists,
            // Too many queries running: another replica would only be
            // loaded more ([`crate::coord`] does not retry it elsewhere).
            429 => Code::ResourceExhausted,
            // Not on this server: a profile without its sampler.
            501 => Code::Unimplemented,
            503 => Code::Unavailable,
            504 => Code::DeadlineExceeded,
            _ => Code::Internal,
        };
        // Whether a refused write was applied, as a code (protocol 19).
        let outcome = crate::cores::shard::Primary::write_outcome(&e);
        let error_code = e.code;
        let mut s = tonic::Status::new(code, e.message);
        if let Some(v) = error_code.and_then(|c| c.parse().ok()) {
            s.metadata_mut().insert(ERROR_CODE, v);
        }
        if let Some(v) = outcome.and_then(|o| o.parse().ok()) {
            s.metadata_mut().insert(crate::cores::shard::WRITE_OUTCOME, v);
        }
        s
    }
}
