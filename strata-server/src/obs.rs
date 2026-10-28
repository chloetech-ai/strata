//! Logs and request ids. Every HTTP and gRPC request gets an id (the
//! client's `x-request-id` if it is a plain one, a new one otherwise),
//! which its answer carries back, each log line written while serving it
//! names, and a coordinator passes on to its shards.
//!
//! Logs go to stderr through `tracing`, as text or as JSON lines
//! ([`init_logs`]).

use std::collections::hash_map::RandomState;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

tokio::task_local! {
    static REQUEST_ID: Arc<str>;
}

/// The header (and gRPC metadata key) that carries a request's id.
pub const HEADER: &str = "x-request-id";

/// `given` if it is 1 to 64 letters, digits, '-', '_', '.' or ':', or a
/// new id.
pub fn request_id(given: Option<&str>) -> Arc<str> {
    match given {
        Some(s) if (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)) => s.into(),
        _ => new_id().into(),
    }
}

/// 16 hex digits: a counter hashed with keys drawn at random once per
/// process, so ids neither repeat nor can be guessed.
fn new_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static KEYS: OnceLock<RandomState> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut h = KEYS.get_or_init(RandomState::new).build_hasher();
    h.write_u64(NEXT.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", h.finish())
}

/// The id of the request this task serves, if it serves one.
pub fn current() -> Option<Arc<str>> {
    REQUEST_ID.try_with(|id| id.clone()).ok()
}

/// Runs `f` as the request `id`'s (none: as it is).
pub async fn scope<F: Future>(id: Option<Arc<str>>, f: F) -> F::Output {
    match id {
        Some(id) => REQUEST_ID.scope(id, f).await,
        None => f.await,
    }
}

/// How log lines are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// One line of text per event, with the request's span.
    Text,
    /// One JSON object per line: `timestamp`, `level`, `target`, the
    /// event's fields (`message` among them) and the request's (`span`).
    Json,
}

impl LogFormat {
    pub fn parse(s: &str) -> Option<LogFormat> {
        match s {
            "text" => Some(LogFormat::Text),
            "json" => Some(LogFormat::Json),
            _ => None,
        }
    }
}

/// Sends log events at `filter`'s levels (`info`, or per module as
/// `info,strata=debug`) to stderr in `format`. The libraries the server is
/// built on log only warnings unless `filter` names them.
pub fn init_logs(filter: &str, format: LogFormat) -> Result<(), String> {
    use tracing_subscriber::EnvFilter;
    let quiet = ["h2", "hyper", "hyper_util", "tower", "tonic", "rustls", "reqwest"];
    let mut directives = filter.to_string();
    for m in quiet.iter().filter(|m| !filter.contains(*m)) {
        directives.push_str(&format!(",{m}=warn"));
    }
    let filter = EnvFilter::try_new(&directives).map_err(|e| format!("bad log filter {filter:?}: {e}"))?;
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr);
    let r = match format {
        LogFormat::Text => b.with_ansi(false).try_init(),
        LogFormat::Json => b.json().flatten_event(true).with_current_span(true).with_span_list(false).try_init(),
    };
    r.map_err(|e| e.to_string())
}

/// Lets through at most `per_second` lines a second and counts the rest,
/// so that a failure every request meets (a full disk) cannot flood the
/// log.
pub struct RateLimit {
    per_second: u64,
    /// The second counted, and the lines passed and held back in it.
    window: Mutex<(Option<Instant>, u64, u64)>,
}

impl RateLimit {
    pub const fn new(per_second: u64) -> RateLimit {
        RateLimit { per_second, window: Mutex::new((None, 0, 0)) }
    }

    /// `Some(held back since the last line passed)` if a line may pass now.
    pub fn admit(&self) -> Option<u64> {
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if w.0.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1)) {
            w.0 = Some(now);
            w.1 = 0;
        }
        if w.1 >= self.per_second {
            w.2 += 1;
            return None;
        }
        w.1 += 1;
        Some(std::mem::take(&mut w.2))
    }
}

/// Logs a request that failed on the server's side (status 500 and up),
/// at most ten a second.
pub fn server_error(status: u16, message: &str) {
    static LIMIT: RateLimit = RateLimit::new(10);
    if let Some(held) = LIMIT.admit() {
        tracing::error!(status, error = message, held_back = held, "request failed");
    }
}

/// `s` cut to at most `max` bytes (at a character boundary), marked when
/// cut.
pub fn truncate(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.len() <= max {
        return s.into();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes)", &s[..end], s.len()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ids_are_kept_and_others_replaced() {
        assert_eq!(&*request_id(Some("abc-123_x.y:z")), "abc-123_x.y:z");
        for bad in ["", "has space", "new\nline", "é", &"x".repeat(65)] {
            let id = request_id(Some(bad));
            assert_ne!(&*id, bad);
            assert_eq!(id.len(), 16);
        }
        let ids: std::collections::HashSet<Arc<str>> = (0..1000).map(|_| request_id(None)).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn the_rate_limit_counts_what_it_holds_back() {
        let l = RateLimit::new(2);
        assert_eq!((l.admit(), l.admit(), l.admit(), l.admit()), (Some(0), Some(0), None, None));
        l.window.lock().unwrap().0 = Some(Instant::now() - Duration::from_secs(2));
        assert_eq!(l.admit(), Some(2));
    }

    #[test]
    fn long_text_is_cut_at_a_character() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("ééé", 3), "é... (6 bytes)");
    }

    #[tokio::test]
    async fn the_id_is_the_scope_s() {
        assert!(current().is_none());
        let got = scope(Some("r1".into()), async { current() }).await;
        assert_eq!(got.as_deref(), Some("r1"));
    }
}
