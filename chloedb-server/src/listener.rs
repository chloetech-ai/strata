//! What each listener holds a client to, beyond the accept loop's
//! connection cap and header timeout ([`crate::server::serve_router_until`]):
//!
//! - [`Clients`]: a share of the connections per client address, so that
//!   one client cannot hold them all;
//! - [`Activity`]: the requests under way on one connection, so that a
//!   connection idle for [`crate::auth::Limits::idle`] is closed, over HTTP/2 too, where
//!   answering pings would otherwise keep it open for ever;
//! - [`Slow`]: a request body that pauses longer than [`crate::auth::Limits::body`], or
//!   trickles slower than [`MIN_BODY_RATE`] once it has taken that long,
//!   ends in an error;
//! - [`Stall`]: a write the client does not read for [`crate::auth::Limits::write`]
//!   fails, and the connection with it;
//! - [`HEALTH_ROUTES`]: past the cap, a few connections are still taken
//!   for the probes, so that a full server is not restarted for failing
//!   them.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::auth::MIN_BODY_RATE;

/// The connections a client address holds on one listener, and how many
/// it may.
pub struct Clients {
    held: Mutex<HashMap<IpAddr, usize>>,
    max: usize,
}

/// One connection a client holds ([`Clients::take`]), given back on drop.
pub struct Held {
    clients: Arc<Clients>,
    ip: IpAddr,
}

impl Clients {
    /// `max` connections for each address (at least one).
    pub fn new(max: usize) -> Arc<Clients> {
        Arc::new(Clients { held: Mutex::new(HashMap::new()), max: max.max(1) })
    }

    /// One more connection for `ip`, or `None` when it holds its share.
    pub fn take(self: &Arc<Self>, ip: IpAddr, max: Option<usize>) -> Option<Held> {
        let ip = canonical(ip);
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let n = held.entry(ip).or_insert(0);
        if *n >= max.unwrap_or(self.max) {
            return None;
        }
        *n += 1;
        Some(Held { clients: self.clone(), ip })
    }

    /// The connections `ip` holds now.
    pub fn of(&self, ip: IpAddr) -> usize {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).get(&canonical(ip)).copied().unwrap_or(0)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut held = self.clients.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = held.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.ip);
            }
        }
    }
}

/// An IPv4 client that came in on an IPv6 socket counts as its IPv4
/// address.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

/// The requests under way on one connection, and when the last ended.
pub struct Activity {
    busy: AtomicUsize,
    /// Milliseconds since `start` at which the connection was last busy.
    last: AtomicU64,
    start: Instant,
}

impl Activity {
    pub fn new() -> Arc<Activity> {
        Arc::new(Activity { busy: AtomicUsize::new(0), last: AtomicU64::new(0), start: Instant::now() })
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Ends once the connection has had no request under way for `idle`
    /// (never, for a zero `idle`).
    pub async fn idle_for(&self, idle: Duration) {
        if idle.is_zero() {
            return std::future::pending().await;
        }
        loop {
            let quiet = Duration::from_millis(self.now_ms().saturating_sub(self.last.load(Ordering::Acquire)));
            if self.busy.load(Ordering::Acquire) == 0 && quiet >= idle {
                return;
            }
            // Check again when it would be idle long enough, if nothing
            // starts meanwhile (or a little later, while busy).
            tokio::time::sleep(idle.saturating_sub(quiet).max(Duration::from_millis(50)).min(idle)).await;
        }
    }
}

/// One request under way on a connection, until its answer's body is
/// sent or dropped.
struct Busy(Arc<Activity>);

impl Busy {
    fn new(a: Arc<Activity>) -> Busy {
        a.busy.fetch_add(1, Ordering::AcqRel);
        Busy(a)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.last.store(self.0.now_ms(), Ordering::Release);
        self.0.busy.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How a connection was taken: within the cap, or past it for the probes
/// alone ([`HEALTH_ROUTES`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Full,
    Health,
}

/// Connections closed at once, before their handshake, because their
/// client held its share of them (`--max-connections-per-client`), since
/// the process started (`chloedb_connections_closed_past_share_total`).
pub static CLOSED_PAST_SHARE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The routes a connection taken past the cap may call, once: the probes
/// of a supervisor, so that a server at its cap is not taken for dead.
pub const HEALTH_ROUTES: [&str; 3] = ["/livez", "/readyz", "/v1/health"];

/// Connections taken past the cap for [`HEALTH_ROUTES`], at once.
pub const HEALTH_SLOTS: usize = 8;

/// What each request on a connection carries for [`track`].
#[derive(Clone)]
pub struct Conn {
    pub activity: Arc<Activity>,
    pub slot: Slot,
    /// [`crate::auth::Limits::body`], or zero where bodies are not timed (gRPC).
    pub body: Duration,
}

/// The middleware every listener's router runs inside: counts the request
/// under way on its connection until its answer is sent, times its body
/// ([`Slow`]), and on a connection taken past the cap answers only the
/// probes.
pub async fn track(req: Request, next: Next) -> Response {
    let Some(conn) = req.extensions().get::<Conn>().cloned() else { return next.run(req).await };
    if conn.slot == Slot::Health && !HEALTH_ROUTES.contains(&req.uri().path()) {
        // The API's error shape ([`crate::errors`]), as every other answer.
        let e = crate::ApiError::coded(503, "overloaded", "the server holds as many connections as it takes (--max-connections): try again shortly");
        let mut r = (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(crate::errors::answer(&e))).into_response();
        r.headers_mut().insert(axum::http::header::RETRY_AFTER, axum::http::HeaderValue::from_static("1"));
        r.headers_mut().insert(axum::http::header::CONNECTION, axum::http::HeaderValue::from_static("close"));
        return r;
    }
    let busy = Busy::new(conn.activity.clone());
    let req = match conn.body.is_zero() {
        true => req,
        false => req.map(|b| Body::new(Slow::new(b, conn.body))),
    };
    let mut res = next.run(req).await;
    if conn.slot == Slot::Health {
        res.headers_mut().insert(axum::http::header::CONNECTION, axum::http::HeaderValue::from_static("close"));
    }
    res.map(|b| Body::new(Tracked { body: b, _busy: busy }))
}

/// An answer's body, which keeps its request counted as under way
/// ([`Busy`]) until it is sent or dropped.
struct Tracked {
    body: Body,
    _busy: Busy,
}

impl http_body::Body for Tracked {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// A request's body that fails when it pauses longer than `limit` between
/// two parts, or averages fewer than [`MIN_BODY_RATE`] bytes a second once
/// it has taken longer than `limit`. Time is counted only while the
/// handler waits for it: a handler that reads the body late does not use
/// the client's time.
pub struct Slow {
    body: Body,
    limit: Duration,
    pause: Option<Pin<Box<tokio::time::Sleep>>>,
    first: Option<Instant>,
    bytes: u64,
}

impl Slow {
    pub fn new(body: Body, limit: Duration) -> Slow {
        Slow { body, limit, pause: None, first: None, bytes: 0 }
    }
}

/// The error a [`Slow`] body ends in.
fn slow_body(why: String) -> axum::Error {
    axum::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, why))
}

impl http_body::Body for Slow {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        let first = *this.first.get_or_insert_with(Instant::now);
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.pause = None;
                if let Some(d) = frame.data_ref() {
                    this.bytes += d.len() as u64;
                }
                let took = first.elapsed();
                if took > this.limit && (this.bytes as f64) < took.as_secs_f64() * MIN_BODY_RATE as f64 {
                    return Poll::Ready(Some(Err(slow_body(format!("the request body came at {:.0} bytes a second, under {MIN_BODY_RATE} (--body-timeout-secs)", this.bytes as f64 / took.as_secs_f64())))));
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Pending => {
                let limit = this.limit;
                let pause = this.pause.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
                match pause.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Some(Err(slow_body(format!("the request body paused for longer than {} s (--body-timeout-secs)", limit.as_secs_f64()))))),
                    Poll::Pending => Poll::Pending,
                }
            }
            done => done,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

/// A connection whose writes fail once one has waited `limit` for the
/// client to read (a zero `limit`: never).
pub struct Stall<T> {
    io: T,
    limit: Duration,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<T> Stall<T> {
    pub fn new(io: T, limit: Duration) -> Stall<T> {
        Stall { io, limit, wait: None }
    }

    /// `r`, the result of a write or flush: once it has been pending for
    /// `limit`, an error.
    fn timed<R>(&mut self, cx: &mut Context<'_>, r: Poll<std::io::Result<R>>) -> Poll<std::io::Result<R>> {
        match r {
            Poll::Pending if !self.limit.is_zero() => {
                let limit = self.limit;
                let wait = self.wait.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
                match wait.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "the client read nothing of its answer for --write-timeout-secs"))),
                    Poll::Pending => Poll::Pending,
                }
            }
            r => {
                if r.is_ready() {
                    self.wait = None;
                }
                r
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Stall<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Stall<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.io).poll_write(cx, buf);
        self.timed(cx, r)
    }

    fn poll_write_vectored(mut self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[std::io::IoSlice<'_>]) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.io).poll_write_vectored(cx, bufs);
        self.timed(cx, r)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let r = Pin::new(&mut self.io).poll_flush(cx);
        self.timed(cx, r)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let r = Pin::new(&mut self.io).poll_shutdown(cx);
        self.timed(cx, r)
    }
}

/// gRPC's `grpc-timeout` header (`100m`, `5S`), which tonic's own server
/// honours: the call fails with DEADLINE_EXCEEDED once its answer has not
/// started within it.
pub async fn grpc_deadline(req: Request, next: Next) -> Response {
    let Some(d) = req.headers().get("grpc-timeout").and_then(|v| v.to_str().ok()).and_then(parse_grpc_timeout) else { return next.run(req).await };
    match tokio::time::timeout(d, next.run(req)).await {
        Ok(r) => r,
        Err(_) => tonic::Status::deadline_exceeded(format!("no answer within the call's grpc-timeout of {d:?}")).into_http().map(Body::new),
    }
}

/// A `grpc-timeout` value: at most eight digits and a unit.
fn parse_grpc_timeout(v: &str) -> Option<Duration> {
    let (n, unit) = v.split_at(v.len().checked_sub(1)?);
    if n.is_empty() || n.len() > 8 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = n.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(n * 3600),
        "M" => Duration::from_secs(n * 60),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_holds_its_share_and_gives_it_back() {
        let c = Clients::new(2);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let mapped: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        let a = c.take(ip, None).unwrap();
        let _b = c.take(mapped, None).expect("the same client over IPv6");
        assert!(c.take(ip, None).is_none(), "past its share");
        assert!(c.take("10.0.0.2".parse().unwrap(), None).is_some(), "another client");
        drop(a);
        assert_eq!(c.of(ip), 1);
        assert!(c.take(ip, None).is_some());
    }

    #[test]
    fn grpc_timeouts_parse_as_tonic_reads_them() {
        assert_eq!(parse_grpc_timeout("100m"), Some(Duration::from_millis(100)));
        assert_eq!(parse_grpc_timeout("5S"), Some(Duration::from_secs(5)));
        assert_eq!(parse_grpc_timeout("2H"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_grpc_timeout("123456789S"), None);
        assert_eq!(parse_grpc_timeout("S"), None);
        assert_eq!(parse_grpc_timeout("10x"), None);
        assert_eq!(parse_grpc_timeout(""), None);
    }

    #[tokio::test]
    async fn an_idle_connection_is_seen_once_nothing_is_under_way() {
        let a = Activity::new();
        let busy = Busy::new(a.clone());
        let idle = tokio::time::timeout(Duration::from_millis(300), a.idle_for(Duration::from_millis(100))).await;
        assert!(idle.is_err(), "idle with a request under way");
        drop(busy);
        tokio::time::timeout(Duration::from_secs(2), a.idle_for(Duration::from_millis(100))).await.expect("idle once it ended");
    }
}
