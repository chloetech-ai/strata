//! The frontier: the URLs waiting to be fetched, one queue per host,
//! handed out breadth first and politely, with every URL ever queued
//! remembered so that none is fetched twice.
//!
//! Politely: at most `per_host` requests to a host at a time, and between
//! the starts of two the longest of the crawl's `delay`, the host's
//! robots.txt `Crawl-delay`, and twice the time the host has been taking
//! to answer (so that a site that slows down gets fewer requests). A host
//! that pushes back (429, 503, 403, no answer) waits: as long as its
//! Retry-After says, else 5 s, doubled at each refusal in a row up to 5
//! minutes; a busy answer's page is asked again later, up to
//! [`MAX_TRIES`] times. After [`GIVE_UP`] refusals in a row, or when a
//! host asks to wait more than [`MAX_WAIT`], the crawl gives up on it.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use url::Url;

/// Times a page is asked for, a busy site's answers included.
pub const MAX_TRIES: u8 = 4;
/// Refusals in a row after which the crawl stops asking a host.
pub const GIVE_UP: u32 = 10;
/// The longest a host is waited for: one that asks for more is given up.
pub const MAX_WAIT: Duration = Duration::from_secs(600);
/// The first wait after a refusal with no Retry-After, and the longest.
const BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// How much longer than a host takes to answer the crawl waits between
/// two of its requests, and the most that makes it wait.
const SLOW_FACTOR: f64 = 2.0;
const MAX_SLOW: Duration = Duration::from_secs(30);

/// A URL to fetch, how far from a seed it is, and how many times it was
/// asked for already.
#[derive(Clone, Debug)]
pub struct Item {
    pub url: Url,
    pub depth: u32,
    pub tries: u8,
}

impl Item {
    pub fn new(url: Url, depth: u32) -> Item {
        Item { url, depth, tries: 0 }
    }
}

struct Host {
    queue: VecDeque<Item>,
    /// Requests out now.
    active: usize,
    /// When the next request may start.
    next_at: Instant,
    /// The crawl's delay between two requests, and its robots.txt's.
    delay: Duration,
    asked: Option<Duration>,
    /// How long it has been taking to answer (a moving average, ms).
    answer_ms: Option<f64>,
    /// Refusals in a row.
    refusals: u32,
    /// Given up on: nothing more is queued or asked.
    gave_up: bool,
    /// Whether the host is in `ready` (so that it is there once).
    scheduled: bool,
}

impl Host {
    /// The time between the starts of two requests.
    fn pace(&self) -> Duration {
        let slow = self.answer_ms.map_or(Duration::ZERO, |ms| Duration::from_secs_f64(ms * SLOW_FACTOR / 1000.0).min(MAX_SLOW));
        self.delay.max(self.asked.unwrap_or_default()).max(slow)
    }
}

/// What a refusal did: how long the host waits, whether the page will be
/// asked again, the refusals in a row, and the pages dropped if the crawl
/// gave up on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub wait: Duration,
    pub again: bool,
    pub in_a_row: u32,
    pub gave_up: Option<usize>,
}

struct Inner {
    hosts: HashMap<String, Host>,
    /// Hosts with URLs that may be handed out, by when.
    ready: BinaryHeap<Reverse<(Instant, String)>>,
    seen: HashSet<u64>,
    queued: usize,
    in_flight: usize,
    /// URLs handed out so far, which `limit` caps.
    handed: u64,
    limit: u64,
    /// URLs not queued because the queue was full.
    dropped: u64,
    paused: bool,
    stopped: bool,
}

pub struct Frontier {
    inner: Mutex<Inner>,
    notify: Notify,
    per_host: usize,
    delay: Duration,
    max_queued: usize,
}

/// How the frontier stands, for the status.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Counts {
    pub queued: usize,
    pub in_flight: usize,
    pub seen: usize,
    pub hosts: usize,
    pub handed: u64,
    pub dropped: u64,
}

impl Frontier {
    pub fn new(per_host: usize, delay: Duration, limit: u64, max_queued: usize) -> Frontier {
        Frontier {
            inner: Mutex::new(Inner {
                hosts: HashMap::new(),
                ready: BinaryHeap::new(),
                seen: HashSet::new(),
                queued: 0,
                in_flight: 0,
                handed: 0,
                limit,
                dropped: 0,
                paused: false,
                stopped: false,
            }),
            notify: Notify::new(),
            per_host: per_host.max(1),
            delay,
            max_queued,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks a URL seen without queueing it (a redirect's target, a
    /// page's canonical address). Whether it was new.
    pub fn mark_seen(&self, url: &str) -> bool {
        self.lock().seen.insert(crate::urls::hash(url))
    }

    /// Whether a URL was seen (queued, fetched, or marked).
    pub fn seen(&self, url: &str) -> bool {
        self.lock().seen.contains(&crate::urls::hash(url))
    }

    /// How many of these URL hashes were not seen.
    pub fn unseen(&self, hashes: &HashSet<u64>) -> usize {
        let g = self.lock();
        hashes.iter().filter(|h| !g.seen.contains(h)).count()
    }

    /// The most URLs handed out (`max_pages`).
    pub fn limit(&self) -> u64 {
        self.lock().limit
    }

    /// Queues a URL unless it was seen. Whether it was queued.
    pub fn push(&self, item: Item) -> bool {
        let mut g = self.lock();
        if g.stopped || !g.seen.insert(crate::urls::hash(item.url.as_str())) {
            return false;
        }
        if g.queued >= self.max_queued {
            g.dropped += 1;
            return false;
        }
        let host = item.url.host_str().unwrap_or("").to_string();
        let now = Instant::now();
        let per_host = self.per_host;
        let delay = self.delay;
        let inner = &mut *g;
        let h = inner.hosts.entry(host.clone()).or_insert_with(|| Host { queue: VecDeque::new(), active: 0, next_at: now, delay, asked: None, answer_ms: None, refusals: 0, gave_up: false, scheduled: false });
        if h.gave_up {
            return false;
        }
        h.queue.push_back(item);
        inner.queued += 1;
        if !h.scheduled && h.active < per_host {
            h.scheduled = true;
            inner.ready.push(Reverse((h.next_at.max(now), host)));
        }
        drop(g);
        self.notify.notify_waiters();
        true
    }

    /// Sets the delay a host's robots.txt asks for between requests (it
    /// counts when longer than the crawl's own). Whether it changed.
    pub fn slow_down(&self, host: &str, delay: Duration) -> bool {
        let mut g = self.lock();
        let Some(h) = g.hosts.get_mut(host) else { return false };
        if h.asked == Some(delay) {
            return false;
        }
        h.asked = Some(delay);
        h.next_at = h.next_at.max(Instant::now() + delay);
        true
    }

    /// A host answered a request without pushing back (in `ms`, for a
    /// page): its pace follows how long it takes, and its refusals in a
    /// row are over.
    pub fn answered(&self, host: &str, ms: Option<u64>) {
        let mut g = self.lock();
        if let Some(h) = g.hosts.get_mut(host) {
            if let Some(ms) = ms {
                h.answer_ms = Some(h.answer_ms.map_or(ms as f64, |a| a * 0.7 + ms as f64 * 0.3));
            }
            h.refusals = 0;
        }
    }

    /// A host pushed back on a request `next` handed out (before `done`):
    /// it waits `after` (its Retry-After) or the backoff, the page is
    /// queued again first when `again` and it has tries left, and after
    /// [`GIVE_UP`] refusals in a row, or a wait longer than [`MAX_WAIT`],
    /// the crawl gives up on the host.
    pub fn refused(&self, item: &Item, again: bool, after: Option<Duration>) -> Refused {
        let mut g = self.lock();
        let host = item.url.host_str().unwrap_or("").to_string();
        let now = Instant::now();
        let inner = &mut *g;
        let Some(h) = inner.hosts.get_mut(&host) else { return Refused { wait: Duration::ZERO, again: false, in_a_row: 0, gave_up: None } };
        if h.gave_up {
            return Refused { wait: Duration::ZERO, again: false, in_a_row: h.refusals, gave_up: None };
        }
        h.refusals += 1;
        let backoff = BACKOFF.saturating_mul(1 << (h.refusals - 1).min(10)).min(MAX_BACKOFF);
        let wait = after.unwrap_or(backoff).max(h.pace());
        if h.refusals >= GIVE_UP || wait > MAX_WAIT {
            h.gave_up = true;
            let dropped = h.queue.len();
            let in_a_row = h.refusals;
            h.queue.clear();
            inner.queued -= dropped;
            drop(g);
            // Workers waiting on this host's pages may be done.
            self.notify.notify_waiters();
            return Refused { wait, again: false, in_a_row, gave_up: Some(dropped) };
        }
        h.next_at = h.next_at.max(now + wait);
        let again = again && item.tries + 1 < MAX_TRIES;
        if again {
            h.queue.push_front(Item { url: item.url.clone(), depth: item.depth, tries: item.tries + 1 });
            inner.queued += 1;
            // Asked again, not one more page: the limit counts pages.
            inner.handed = inner.handed.saturating_sub(1);
        }
        Refused { wait, again, in_a_row: h.refusals, gave_up: None }
    }

    /// The next URL to fetch, waiting while every host waits out its delay
    /// or has its requests out; `None` once nothing is queued or out (the
    /// crawl is done), or once stopped or the limit is reached.
    pub async fn next(&self) -> Option<Item> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let mut g = self.lock();
                // Out of URLs for good: stopped, at the limit, or nothing
                // queued and nothing out that could find more.
                if g.stopped || g.handed >= g.limit || (g.queued == 0 && g.in_flight == 0) {
                    drop(g);
                    self.notify.notify_waiters();
                    return None;
                }
                if g.paused {
                    None
                } else {
                    let now = Instant::now();
                    match g.ready.peek().map(|Reverse((t, _))| *t) {
                        Some(t) if t <= now => {
                            let Reverse((_, host)) = g.ready.pop().expect("peeked");
                            let per_host = self.per_host;
                            let inner = &mut *g;
                            let h = inner.hosts.get_mut(&host).expect("a ready host is known");
                            h.scheduled = false;
                            if h.active >= per_host || h.queue.is_empty() {
                                continue;
                            }
                            // Made to wait since it was scheduled (a refusal, robots.txt).
                            if h.next_at > now {
                                h.scheduled = true;
                                inner.ready.push(Reverse((h.next_at, host)));
                                continue;
                            }
                            let item = h.queue.pop_front().expect("not empty");
                            h.active += 1;
                            h.next_at = now + h.pace();
                            if !h.queue.is_empty() && h.active < per_host {
                                h.scheduled = true;
                                inner.ready.push(Reverse((h.next_at, host)));
                            }
                            inner.queued -= 1;
                            inner.in_flight += 1;
                            inner.handed += 1;
                            return Some(item);
                        }
                        Some(t) => Some(t - now),
                        // URLs queued only behind hosts at their limit: wait for one to finish.
                        None => None,
                    }
                }
            };
            match wait {
                Some(d) => {
                    let _ = tokio::time::timeout(d, notified).await;
                }
                None => {
                    let _ = tokio::time::timeout(Duration::from_secs(1), notified).await;
                }
            }
        }
    }

    /// A URL `next` handed out is done with.
    pub fn done(&self, url: &Url) {
        let mut g = self.lock();
        let host = url.host_str().unwrap_or("").to_string();
        let now = Instant::now();
        let per_host = self.per_host;
        let inner = &mut *g;
        inner.in_flight = inner.in_flight.saturating_sub(1);
        if let Some(h) = inner.hosts.get_mut(&host) {
            h.active = h.active.saturating_sub(1);
            if !h.scheduled && !h.queue.is_empty() && h.active < per_host {
                h.scheduled = true;
                inner.ready.push(Reverse((h.next_at.max(now), host)));
            }
        }
        drop(g);
        self.notify.notify_waiters();
    }

    pub fn set_paused(&self, paused: bool) {
        self.lock().paused = paused;
        self.notify.notify_waiters();
    }

    /// No more URLs: `next` answers `None` from now on.
    pub fn stop(&self) {
        let mut g = self.lock();
        g.stopped = true;
        drop(g);
        self.notify.notify_waiters();
    }

    /// Raises (or lowers) how many URLs may be handed out in all.
    pub fn set_limit(&self, limit: u64) {
        self.lock().limit = limit;
        self.notify.notify_waiters();
    }

    pub fn counts(&self) -> Counts {
        let g = self.lock();
        Counts { queued: g.queued, in_flight: g.in_flight, seen: g.seen.len(), hosts: g.hosts.len(), handed: g.handed, dropped: g.dropped }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(s: &str, depth: u32) -> Item {
        Item::new(Url::parse(s).unwrap(), depth)
    }

    #[tokio::test]
    async fn breadth_first_once_each_and_done() {
        let f = Frontier::new(1, Duration::ZERO, 100, 1000);
        assert!(f.push(item("http://a/1", 0)));
        assert!(!f.push(item("http://a/1", 0)), "seen once");
        assert!(f.push(item("http://a/2", 1)));
        let a = f.next().await.unwrap();
        assert_eq!(a.url.path(), "/1");
        // a's one request is out: the next waits until it is done.
        let waiting = tokio::time::timeout(Duration::from_millis(50), f.next()).await;
        assert!(waiting.is_err());
        f.done(&a.url);
        let b = f.next().await.unwrap();
        assert_eq!(b.url.path(), "/2");
        f.done(&b.url);
        assert!(f.next().await.is_none(), "nothing queued or out: done");
    }

    #[tokio::test]
    async fn delay_between_requests_and_limit() {
        let f = Frontier::new(4, Duration::from_millis(80), 2, 1000);
        for i in 0..5 {
            f.push(item(&format!("http://a/{i}"), 0));
        }
        let t0 = Instant::now();
        let x = f.next().await.unwrap();
        let y = f.next().await.unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(75), "the second start waits the delay");
        f.done(&x.url);
        f.done(&y.url);
        assert!(f.next().await.is_none(), "the limit of 2 is reached");
        assert_eq!(f.counts().handed, 2);
    }

    #[tokio::test]
    async fn a_busy_host_waits_and_its_page_is_asked_again() {
        let f = Frontier::new(1, Duration::ZERO, 100, 1000);
        f.push(item("http://a/1", 0));
        f.push(item("http://a/2", 0));
        let a1 = f.next().await.unwrap();
        let r = f.refused(&a1, true, Some(Duration::from_millis(150)));
        assert_eq!(r, Refused { wait: Duration::from_millis(150), again: true, in_a_row: 1, gave_up: None });
        f.done(&a1.url);
        let t0 = Instant::now();
        let again = f.next().await.unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(140), "the host waited its Retry-After");
        assert_eq!((again.url.path(), again.tries), ("/1", 1), "the same page first");
        assert_eq!(f.counts().handed, 1, "asked again, not one more page");
        // Out of tries: a refusal no longer queues it.
        let last = Item { tries: MAX_TRIES - 1, ..again.clone() };
        assert!(!f.refused(&last, true, Some(Duration::ZERO)).again);
        f.answered("a", Some(1));
        f.done(&again.url);
        assert_eq!(f.next().await.unwrap().url.path(), "/2");
    }

    #[tokio::test]
    async fn refusals_back_off_and_the_host_is_given_up() {
        let f = Frontier::new(1, Duration::ZERO, 1000, 1000);
        for i in 0..20 {
            f.push(item(&format!("http://a/{i}"), 0));
        }
        f.push(item("http://b/1", 0));
        let a0 = item("http://a/0", 0);
        let waits: Vec<u64> = (0..GIVE_UP - 1).map(|_| f.refused(&a0, false, None).wait.as_secs()).collect();
        assert_eq!(waits, [5, 10, 20, 40, 80, 160, 300, 300, 300], "doubled up to 5 minutes");
        // An answer that is no refusal starts the count again.
        f.answered("a", None);
        assert_eq!(f.refused(&a0, false, None).in_a_row, 1);
        for _ in 0..GIVE_UP - 2 {
            assert_eq!(f.refused(&a0, false, None).gave_up, None);
        }
        let r = f.refused(&a0, false, None);
        assert_eq!((r.in_a_row, r.gave_up), (GIVE_UP, Some(20)), "given up: its queue dropped");
        assert!(!f.push(item("http://a/new", 0)), "nothing more queued for it");
        assert_eq!(f.counts().queued, 1);
        // The other host goes on.
        let b = tokio::time::timeout(Duration::from_secs(1), f.next()).await.unwrap().unwrap();
        assert_eq!(b.url.host_str(), Some("b"));
        f.done(&b.url);
        assert!(f.next().await.is_none());
    }

    #[tokio::test]
    async fn a_host_that_asks_to_wait_too_long_is_given_up() {
        let f = Frontier::new(1, Duration::ZERO, 100, 1000);
        f.push(item("http://a/1", 0));
        f.push(item("http://a/2", 0));
        let a1 = f.next().await.unwrap();
        let r = f.refused(&a1, true, Some(MAX_WAIT + Duration::from_secs(1)));
        assert_eq!((r.again, r.gave_up), (false, Some(1)));
        f.done(&a1.url);
        assert!(f.next().await.is_none());
    }

    #[tokio::test]
    async fn the_pace_follows_robots_txt_and_how_long_a_host_takes() {
        let f = Frontier::new(2, Duration::ZERO, 100, 1000);
        for i in 1..=4 {
            f.push(item(&format!("http://a/{i}"), 0));
        }
        let a1 = f.next().await.unwrap();
        // Took 100 ms to answer: two of its requests start 200 ms apart.
        f.answered("a", Some(100));
        f.done(&a1.url);
        let _a2 = f.next().await.unwrap();
        let t0 = Instant::now();
        let _a3 = f.next().await.unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(190), "{:?}", t0.elapsed());
        // robots.txt's Crawl-delay, once (and again when it changes).
        assert!(f.slow_down("a", Duration::from_millis(300)));
        assert!(!f.slow_down("a", Duration::from_millis(300)));
        assert!(f.slow_down("a", Duration::from_millis(250)));
        assert!(!f.slow_down("nowhere", Duration::from_millis(250)));
    }

    #[tokio::test]
    async fn hosts_in_parallel() {
        let f = Frontier::new(1, Duration::from_secs(10), 100, 1000);
        f.push(item("http://a/1", 0));
        f.push(item("http://b/1", 0));
        let t0 = Instant::now();
        let x = f.next().await.unwrap();
        let y = f.next().await.unwrap();
        assert_ne!(x.url.host_str(), y.url.host_str());
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
}
