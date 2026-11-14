//! A crawl: its seeds and limits (the [`Spec`]), the fetchers that walk
//! the frontier, the sample its schema is inferred from, the table made
//! from it, and the writer that feeds the table and adds columns as new
//! fields turn common. Its status is what the API and the view show.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use url::Url;

use crate::extract::{self, Rule, TemplateField};
use crate::fetch::{Fetcher, Miss};
use crate::frontier::{Frontier, Item};
use crate::infer::{Fetched, Plan, Source, Stats};
use crate::strata::Strata;
use crate::urls::{self, Scope};

/// Links past `max_depth` remembered at most (to count them).
const MAX_BEYOND: usize = 1_000_000;

/// The longest robots.txt `Crawl-delay` honoured; a site that asks for more
/// gets this.
const MAX_CRAWL_DELAY: Duration = Duration::from_secs(600);

fn d_max_pages() -> u64 {
    1000
}
fn d_max_depth() -> u32 {
    5
}
fn d_concurrency() -> usize {
    16
}
fn d_per_host() -> usize {
    1
}
fn d_delay() -> u64 {
    1000
}
fn d_true() -> bool {
    true
}
fn d_sample() -> usize {
    30
}
fn d_promote() -> f64 {
    0.3
}
fn d_timeout() -> u64 {
    20_000
}
fn d_max_bytes() -> usize {
    5 << 20
}
fn d_batch() -> usize {
    200
}

/// What to crawl and how.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Where to start: one URL or more.
    pub seeds: Vec<String>,
    /// The table to fill (made from the sample); by default the first
    /// seed's host (`books_toscrape_com`).
    #[serde(default)]
    pub table: Option<String>,
    /// Which links to follow: the seeds' hosts, their sites, below their
    /// paths, or anywhere.
    #[serde(default)]
    pub scope: Scope,
    /// Regular expressions on URLs: links are followed only when they
    /// match one (`include`, if any) and none of `exclude`.
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pages made rows only when their URL matches one of these (all,
    /// when none): crawl a shop's lists, keep its product pages.
    #[serde(default)]
    pub keep: Vec<String>,
    #[serde(default = "d_max_pages")]
    pub max_pages: u64,
    #[serde(default = "d_max_depth")]
    pub max_depth: u32,
    /// Requests out at once, over all hosts.
    #[serde(default = "d_concurrency")]
    pub concurrency: usize,
    /// Requests out at once to one host.
    #[serde(default = "d_per_host")]
    pub per_host: usize,
    /// Milliseconds between the starts of two requests to a host (longer
    /// when its robots.txt asks, or when it takes long to answer).
    #[serde(default = "d_delay")]
    pub delay_ms: u64,
    #[serde(default = "d_true")]
    pub robots: bool,
    /// Seeds also from the sites' sitemaps (robots.txt's, or /sitemap.xml).
    #[serde(default = "d_true")]
    pub sitemaps: bool,
    /// Pages read before the table is made: its schema is inferred from them.
    #[serde(default = "d_sample")]
    pub sample: usize,
    /// The share of pages a marked-up field needs to be a column (else it
    /// is kept in `data`), when the table is made and later.
    #[serde(default = "d_promote")]
    pub promote_at: f64,
    /// Fields from CSS selectors, always columns.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Learn fields from the site's templates ([`extract::learn_template`]).
    #[serde(default = "d_true")]
    pub learn: bool,
    /// A vector field `emb` embedded from the title and body by this model
    /// (`potion-base-8M`), which the server downloads.
    #[serde(default)]
    pub embed: Option<String>,
    /// The crawl's own user agent (the crawler's product token is added
    /// after it, unless it names the crawler).
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Whom a site's owner reaches about this crawl (an email address or a
    /// URL), in the user agent.
    #[serde(default)]
    pub contact: Option<String>,
    #[serde(default = "d_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "d_max_bytes")]
    pub max_bytes: usize,
    /// Rows a write sends.
    #[serde(default = "d_batch")]
    pub batch: usize,
}

impl Spec {
    pub fn seeds(&self) -> Result<Vec<Url>, String> {
        let mut out = Vec::new();
        for s in &self.seeds {
            let s = s.trim();
            let with_scheme = if s.contains("://") { s.to_string() } else { format!("https://{s}") };
            let u = Url::parse(&with_scheme).map_err(|e| format!("{s:?} is not a URL: {e}"))?;
            out.push(urls::normalize(&u).ok_or_else(|| format!("{s:?} is not a web page's address (http or https)"))?);
        }
        if out.is_empty() {
            return Err("a crawl needs a URL to start from (seeds)".into());
        }
        Ok(out)
    }

    pub fn table_name(&self) -> Result<String, String> {
        let t = match &self.table {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => urls::table_name(&self.seeds()?[0]),
        };
        if !t.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || t.len() > 64 {
            return Err(format!("{t:?} is not a table name: letters, digits and _, starting with a letter"));
        }
        Ok(t)
    }

    pub fn check(&self) -> Result<(), String> {
        self.seeds()?;
        self.table_name()?;
        for r in self.include.iter().chain(&self.exclude).chain(&self.keep) {
            Regex::new(r).map_err(|e| format!("{r:?} is not a regular expression: {e}"))?;
        }
        for r in &self.rules {
            if r.name.trim().is_empty() {
                return Err("a rule needs a name".into());
            }
            scraper::Selector::parse(&r.selector).map_err(|e| format!("rule {:?}: {:?} is not a CSS selector: {e}", r.name, r.selector))?;
        }
        if !(0.0..=1.0).contains(&self.promote_at) {
            return Err("promote_at is a share, from 0 to 1".into());
        }
        if self.concurrency == 0 || self.concurrency > 1024 {
            return Err("concurrency is from 1 to 1024".into());
        }
        for (name, v) in [("user_agent", &self.user_agent), ("contact", &self.contact)] {
            if v.as_deref().is_some_and(|v| !crate::fetch::header_text(v)) {
                return Err(format!("{name} is printable ASCII, at most 300 characters"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Reading the sample the schema is inferred from.
    Sampling,
    Crawling,
    Paused,
    /// Stopping: the requests out finish, the rows are written.
    Stopping,
    Done,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Counts {
    /// Pages fetched (any answer).
    pub fetched: u64,
    /// HTML pages read.
    pub pages: u64,
    pub written: u64,
    /// Rows the server refused.
    pub refused: u64,
    pub noindex: u64,
    pub duplicates: u64,
    /// Pages fetched but not kept (`keep`).
    pub not_kept: u64,
    pub bytes: u64,
    /// Answers that were no page, by why ([`Miss::kind`]).
    pub misses: BTreeMap<String, u64>,
    /// 429s from Strata waited out, and writes sent again.
    pub busy: u64,
    pub retries: u64,
    /// Pages asked for again after a busy site's answer.
    pub asked_again: u64,
    /// Hosts the crawl gave up on (they kept refusing).
    pub gave_up: Vec<String>,
    /// Columns added during the crawl.
    pub added_columns: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Recent {
    pub at_ms: u64,
    pub url: String,
    pub status: u16,
    pub title: Option<String>,
    pub ms: u64,
    pub bytes: usize,
    pub words: usize,
    /// Marked-up and learned fields on the page.
    pub fields: usize,
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    pub at_ms: u64,
    pub url: String,
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub at_ms: u64,
    pub kind: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Sample {
    pub t_ms: u64,
    pub fetched: u64,
    pub written: u64,
    pub queued: usize,
}

struct State {
    phase: Phase,
    error: Option<String>,
    counts: Counts,
    recent: VecDeque<Recent>,
    problems: VecDeque<Problem>,
    events: Vec<Event>,
    series: VecDeque<Sample>,
    plan: Option<Plan>,
    stats: Stats,
    template: Vec<TemplateField>,
    /// `data` paths whose column could not be added.
    failed: HashSet<String>,
    finished_ms: Option<u64>,
    /// Why a crawl that ran to its end ended ([`Job::why_done`]).
    why_done: Option<String>,
}

pub struct Job {
    pub id: String,
    pub spec: Spec,
    pub table: String,
    pub started_ms: u64,
    frontier: Frontier,
    fetcher: Fetcher,
    st: Mutex<State>,
    sampling: AtomicBool,
    template: RwLock<Arc<Vec<TemplateField>>>,
    hashes: Mutex<HashSet<u64>>,
    /// Links not followed because they were past `max_depth` (hashes).
    beyond: Mutex<HashSet<u64>>,
    seeds: Vec<Url>,
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    keep: Vec<Regex>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn iso(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)).unwrap_or_default()
}

fn push_cap<T>(q: &mut VecDeque<T>, v: T, cap: usize) {
    if q.len() >= cap {
        q.pop_front();
    }
    q.push_back(v);
}

impl Job {
    pub fn new(id: String, spec: Spec) -> Result<Arc<Job>, String> {
        spec.check()?;
        let seeds = spec.seeds()?;
        let table = spec.table_name()?;
        let re = |v: &[String]| v.iter().map(|r| Regex::new(r).expect("checked")).collect::<Vec<_>>();
        let agent = crate::fetch::user_agent(spec.user_agent.as_deref(), spec.contact.as_deref());
        Ok(Arc::new(Job {
            id,
            table,
            started_ms: now_ms(),
            frontier: Frontier::new(spec.per_host, Duration::from_millis(spec.delay_ms), spec.max_pages, 2_000_000),
            fetcher: Fetcher::new(&agent, Duration::from_millis(spec.timeout_ms.max(1000)), spec.max_bytes.max(64 << 10), spec.per_host, spec.robots),
            st: Mutex::new(State {
                phase: Phase::Sampling,
                error: None,
                counts: Counts::default(),
                recent: VecDeque::new(),
                problems: VecDeque::new(),
                events: Vec::new(),
                series: VecDeque::new(),
                plan: None,
                stats: Stats::default(),
                template: Vec::new(),
                failed: HashSet::new(),
                finished_ms: None,
                why_done: None,
            }),
            sampling: AtomicBool::new(true),
            template: RwLock::new(Arc::new(Vec::new())),
            hashes: Mutex::new(HashSet::new()),
            beyond: Mutex::new(HashSet::new()),
            include: re(&spec.include),
            exclude: re(&spec.exclude),
            keep: re(&spec.keep),
            seeds,
            spec,
        }))
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn event(&self, kind: &str, text: String, sql: Option<String>) {
        tracing::info!(job = %self.id, kind, "{text}");
        let mut g = self.st();
        if g.events.len() >= 300 {
            g.events.remove(1);
        }
        g.events.push(Event { at_ms: now_ms(), kind: kind.into(), text, sql });
    }

    fn problem(&self, url: &str, kind: &str, message: String) {
        let mut g = self.st();
        push_cap(&mut g.problems, Problem { at_ms: now_ms(), url: url.into(), kind: kind.into(), message }, 100);
    }

    pub fn phase(&self) -> Phase {
        self.st().phase
    }

    pub fn active(&self) -> bool {
        matches!(self.phase(), Phase::Sampling | Phase::Crawling | Phase::Paused | Phase::Stopping)
    }

    pub fn pause(&self, paused: bool) {
        let mut g = self.st();
        match (g.phase, paused) {
            (Phase::Sampling | Phase::Crawling, true) => g.phase = Phase::Paused,
            (Phase::Paused, false) => g.phase = if self.sampling.load(Ordering::Relaxed) { Phase::Sampling } else { Phase::Crawling },
            _ => return,
        }
        drop(g);
        self.frontier.set_paused(paused);
        self.event(if paused { "pause" } else { "resume" }, if paused { "paused: the requests out finish, no new ones start".into() } else { "resumed".into() }, None);
    }

    pub fn stop(&self) {
        {
            let mut g = self.st();
            if !matches!(g.phase, Phase::Sampling | Phase::Crawling | Phase::Paused) {
                return;
            }
            g.phase = Phase::Stopping;
        }
        self.frontier.set_paused(false);
        self.frontier.stop();
        self.event("stop", "stopping: the requests out finish and their rows are written".into(), None);
    }

    /// Whether a link is one to follow.
    fn follows(&self, u: &Url) -> bool {
        if urls::not_a_page(u) || !urls::in_scope(self.spec.scope, &self.seeds, u) {
            return false;
        }
        let s = u.as_str();
        (self.include.is_empty() || self.include.iter().any(|r| r.is_match(s))) && !self.exclude.iter().any(|r| r.is_match(s))
    }

    /// A host pushed back: it waits, the page may be asked again, and the
    /// crawl may give up on the host; said in the crawl's events at the
    /// first refusals in a row (1, 2, 4, 8) and when it gives up.
    fn refused(&self, host: &str, item: &Item, miss: &Miss, again: bool, after: Option<Duration>) {
        let r = self.frontier.refused(item, again, after);
        if r.again {
            self.st().counts.asked_again += 1;
        }
        if let Some(dropped) = r.gave_up {
            self.st().counts.gave_up.push(host.to_string());
            let why = if r.in_a_row >= crate::frontier::GIVE_UP { format!("{} refusals in a row, the last {miss}", r.in_a_row) } else { format!("{miss}, longer than the {} it waits at most", crate::fetch::secs(crate::frontier::MAX_WAIT)) };
            self.event("polite", format!("gave up on {host}: {why}; {dropped} of its queued pages dropped"), None);
        } else if r.in_a_row.is_power_of_two() {
            let then = if r.again { ", then asks for the page again" } else { "" };
            self.event("polite", format!("{host}: {miss}; waits {} before its next request{then}", crate::fetch::secs(r.wait)), None);
        }
    }

    fn kept(&self, u: &str) -> bool {
        self.keep.is_empty() || self.keep.iter().any(|r| r.is_match(u))
    }

    /// Runs the crawl to its end: the table is made after the sample, and
    /// every page after it is written as it is read.
    pub async fn run(self: Arc<Self>, strata: Strata) {
        let (tx, rx) = mpsc::channel::<Fetched>(512);
        for s in &self.seeds {
            self.frontier.push(Item::new(s.clone(), 0));
        }
        self.event("start", format!("crawling from {} into table {}", self.spec.seeds.join(", "), self.table), None);

        // The sitemaps' pages, at depth 1, as they are read.
        if self.spec.sitemaps {
            let me = self.clone();
            tokio::spawn(async move {
                let mut origins = HashSet::new();
                for s in me.seeds.clone() {
                    if !origins.insert(s.origin().ascii_serialization()) {
                        continue;
                    }
                    let robots = me.fetcher.robots(&s).await;
                    let maps = if robots.sitemaps.is_empty() { vec![format!("{}/sitemap.xml", s.origin().ascii_serialization())] } else { robots.sitemaps.clone() };
                    let mut n = 0;
                    for m in maps.iter().take(10) {
                        for loc in me.fetcher.sitemap(m, (me.spec.max_pages as usize).saturating_mul(2).min(200_000)).await {
                            if let Some(u) = Url::parse(&loc).ok().and_then(|u| urls::normalize(&u)) {
                                if me.follows(&u) && me.frontier.push(Item::new(u, 1)) {
                                    n += 1;
                                }
                            }
                        }
                    }
                    if n > 0 {
                        me.event("sitemap", format!("{n} pages from the sitemaps of {}", s.host_str().unwrap_or("")), None);
                    }
                }
            });
        }

        // The rates, once a second.
        let ticker = {
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let q = me.frontier.counts().queued;
                    let mut g = me.st();
                    let s = Sample { t_ms: now_ms(), fetched: g.counts.fetched, written: g.counts.written, queued: q };
                    push_cap(&mut g.series, s, 900);
                }
            })
        };

        let workers: Vec<_> = (0..self.spec.concurrency)
            .map(|_| {
                let me = self.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(item) = me.frontier.next().await {
                        me.visit(&item, &tx).await;
                        me.frontier.done(&item.url);
                    }
                })
            })
            .collect();
        drop(tx);

        let res = self.clone().write(rx, strata).await;
        for w in workers {
            let _ = w.await;
        }
        ticker.abort();
        let mut g = self.st();
        g.finished_ms = Some(now_ms());
        match res {
            Ok(()) => {
                g.phase = if g.phase == Phase::Stopping { Phase::Stopped } else { Phase::Done };
                let c = &g.counts;
                let mut text = format!("{}: {} pages fetched, {} rows written", if g.phase == Phase::Stopped { "stopped" } else { "done" }, c.fetched, c.written);
                let done = g.phase == Phase::Done;
                drop(g);
                if done {
                    let why = self.why_done();
                    text = format!("{text}; {why}");
                    self.st().why_done = Some(why);
                }
                self.event("done", text, None);
            }
            Err(e) => {
                g.phase = Phase::Failed;
                g.error = Some(e.clone());
                drop(g);
                self.frontier.stop();
                self.event("error", e, None);
            }
        }
    }

    /// Fetches one URL, reads it and queues its links; the page goes to
    /// the writer.
    async fn visit(&self, item: &Item, tx: &mpsc::Sender<Fetched>) {
        let host = item.url.host_str().unwrap_or("").to_string();
        // The Crawl-delay of every host the crawl reaches, as its
        // robots.txt is read (and read again).
        if self.spec.robots {
            if let Some(d) = self.fetcher.robots(&item.url).await.crawl_delay {
                let d = Duration::from_secs_f64(d.min(MAX_CRAWL_DELAY.as_secs_f64()));
                if self.frontier.slow_down(&host, d) && d > Duration::from_millis(self.spec.delay_ms) {
                    self.event("robots", format!("{host} asks for {} between requests", crate::fetch::secs(d)), None);
                }
            }
        }
        let got = self.fetcher.page_following(&item.url, &|u| self.follows(u)).await;
        let got = match got {
            Ok(g) => g,
            Err(miss) => {
                {
                    let mut g = self.st();
                    g.counts.fetched += u64::from(miss.fetched());
                    *g.counts.misses.entry(miss.kind().into()).or_default() += 1;
                }
                if miss.is_error() {
                    self.problem(item.url.as_str(), miss.kind(), miss.to_string());
                }
                match miss.refusal() {
                    Some((again, after)) => self.refused(&host, item, &miss, again, after),
                    None if miss.fetched() => self.frontier.answered(&host, None),
                    None => {}
                }
                return;
            }
        };
        self.frontier.answered(&host, Some(got.ms));
        {
            let mut g = self.st();
            g.counts.fetched += 1;
            g.counts.bytes += got.bytes as u64;
        }
        // Where a redirect led: a page of its own, unless it was seen.
        let url = urls::normalize(&got.url).unwrap_or_else(|| item.url.clone());
        if url != item.url && (!urls::in_scope(self.spec.scope, &self.seeds, &url) || !self.frontier.mark_seen(url.as_str())) {
            self.st().counts.duplicates += 1;
            return;
        }
        let learn = self.spec.learn && self.sampling.load(Ordering::Relaxed);
        let template = self.template.read().unwrap_or_else(|e| e.into_inner()).clone();
        let rules = self.spec.rules.clone();
        let body = got.body;
        let u2 = url.clone();
        let page = tokio::task::spawn_blocking(move || extract::extract(&body, &u2, &extract::Options { rules: &rules, template: &template, learn, max_text: 200_000 })).await;
        let Ok(page) = page else {
            self.problem(url.as_str(), "parse", "the page could not be read".into());
            return;
        };
        // Queued in turn from each shape of link, so that the pages read
        // first (the sample the table is made from) are a list's items as
        // well as its other lists. A list's next page is as deep as the
        // list; a link past `max_depth` is counted, so that the crawl can
        // say what the depth left out.
        let follow: Vec<Url> = page.links.iter().filter(|l| self.follows(l)).cloned().collect();
        let n = follow.len();
        for l in spread(follow, n, usize::MAX) {
            let depth = if page.next.contains(&l) { item.depth } else { item.depth + 1 };
            if depth <= self.spec.max_depth {
                self.frontier.push(Item::new(l, depth));
            } else if !self.frontier.seen(l.as_str()) {
                let mut b = self.beyond.lock().unwrap_or_else(|e| e.into_inner());
                if b.len() < MAX_BEYOND {
                    b.insert(urls::hash(l.as_str()));
                }
            }
        }
        {
            let mut g = self.st();
            g.counts.pages += 1;
            push_cap(
                &mut g.recent,
                Recent {
                    at_ms: now_ms(),
                    url: url.to_string(),
                    status: got.status,
                    title: page.title.clone(),
                    ms: got.ms,
                    bytes: got.bytes,
                    words: page.words,
                    fields: page.structured.values().map(|v| v.as_object().map_or(1, |o| o.len())).sum(),
                    kind: page.kind.clone(),
                },
                60,
            );
            if page.noindex {
                g.counts.noindex += 1;
                return;
            }
        }
        if !self.kept(url.as_str()) {
            self.st().counts.not_kept += 1;
            return;
        }
        if page.words > 0 && !self.hashes.lock().unwrap_or_else(|e| e.into_inner()).insert(page.content_hash) {
            self.st().counts.duplicates += 1;
            return;
        }
        let f = Fetched {
            url: url.to_string(),
            host: url.host_str().unwrap_or("").to_string(),
            depth: item.depth,
            status: got.status,
            fetched: iso(now_ms()),
            bytes: got.bytes,
            fetch_ms: got.ms,
            page,
        };
        let _ = tx.send(f).await;
    }

    /// The writer: the sample, the table, then every page as a row.
    async fn write(self: Arc<Self>, mut rx: mpsc::Receiver<Fetched>, strata: Strata) -> Result<(), String> {
        // The sample: until it is full, or every page there is was read.
        let mut sample: Vec<Fetched> = Vec::new();
        let sample_n = self.spec.sample.max(1);
        let started = Instant::now();
        while sample.len() < sample_n {
            match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
                Ok(Some(f)) => sample.push(f),
                Ok(None) => break,
                Err(_) => {
                    // Stopped while sampling, or a site that is slow: go on with what there is after a minute.
                    if self.phase() == Phase::Stopping || (started.elapsed() > Duration::from_secs(60) && !sample.is_empty()) {
                        break;
                    }
                }
            }
        }
        if sample.is_empty() {
            self.sampling.store(false, Ordering::Relaxed);
            let g = self.st();
            if g.counts.fetched == 0 && g.counts.misses.get("robots").copied().unwrap_or(0) > 0 {
                return Err("robots.txt does not allow the seeds".into());
            }
            if g.counts.fetched == 0 && g.counts.misses.get("robots_unreachable").copied().unwrap_or(0) > 0 {
                let why = g.problems.iter().rev().find(|p| p.kind == "robots_unreachable").map(|p| p.message.clone()).unwrap_or_default();
                return Err(format!("the seeds' robots.txt could not be read, so nothing was fetched: {why}"));
            }
            if !g.counts.gave_up.is_empty() && g.counts.pages == 0 {
                return Err(format!("{} kept refusing the crawler, so the crawl gave up (see its events)", g.counts.gave_up.join(", ")));
            }
            if g.counts.fetched > 0 && g.counts.pages == 0 {
                return Err(format!("no HTML page among the {} fetched (see the problems)", g.counts.fetched));
            }
            return Ok(());
        }

        // Fields from the site's templates, learned on the sample.
        let template = if self.spec.learn {
            let sigs: Vec<(&[(String, String, u32)], Option<&str>)> = sample.iter().map(|f| (&f.page.signatures[..], f.page.title.as_deref())).collect();
            extract::learn_template(&sigs, self.spec.promote_at.max(0.2))
        } else {
            Vec::new()
        };
        for f in &mut sample {
            apply_template(f, &template);
        }
        *self.template.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(template.clone());
        self.sampling.store(false, Ordering::Relaxed);

        let mut stats = Stats::default();
        for f in &sample {
            stats.add(f, &self.spec.rules);
        }
        let plan = self.table_plan(&strata, &stats).await?;
        {
            let mut g = self.st();
            g.plan = Some(plan.clone());
            g.template = template;
            g.stats = stats;
            if g.phase == Phase::Sampling {
                g.phase = Phase::Crawling;
            }
        }
        let mut plan = plan;

        // Every page from here on: a row, written in batches.
        let mut batch: Vec<serde_json::Map<String, Value>> = sample.iter().map(|f| plan.row(f)).collect();
        let mut since_check = 0usize;
        let mut n_batches = 0u64;
        let mut last = Instant::now();
        // Columns added during the crawl, filled on the rows written before.
        let mut fills = tokio::task::JoinSet::new();
        loop {
            let full = batch.len() >= self.spec.batch.max(1);
            let due = !batch.is_empty() && last.elapsed() >= Duration::from_millis(500);
            if full || due {
                n_batches += 1;
                self.flush(&strata, std::mem::take(&mut batch), n_batches).await?;
                last = Instant::now();
            }
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Some(f)) => {
                    {
                        let mut g = self.st();
                        g.stats.add(&f, &self.spec.rules);
                    }
                    batch.push(plan.row(&f));
                    since_check += 1;
                    if since_check >= 100 {
                        since_check = 0;
                        // The rows built before a column is added are
                        // written first, so the column's fill finds them.
                        if !batch.is_empty() {
                            n_batches += 1;
                            self.flush(&strata, std::mem::take(&mut batch), n_batches).await?;
                            last = Instant::now();
                        }
                        for c in self.promote(&strata, &mut plan).await {
                            fills.spawn(self.clone().fill(strata.clone(), c));
                        }
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        if !batch.is_empty() {
            n_batches += 1;
            self.flush(&strata, batch, n_batches).await?;
        }
        for c in self.promote(&strata, &mut plan).await {
            fills.spawn(self.clone().fill(strata.clone(), c));
        }
        while fills.join_next().await.is_some() {}
        Ok(())
    }

    /// Fills a column added during the crawl on the rows written before
    /// it: their values are in `data`, at the column's path. (A list's
    /// column is left to the rows from here on: `set` takes one value.)
    async fn fill(self: Arc<Self>, strata: Strata, c: crate::infer::Column) {
        let Source::Path(path) = &c.source else { return };
        if matches!(c.ty, crate::infer::Ty::KeywordList | crate::infer::Ty::IntList) || !path.split('.').all(|s| !s.is_empty() && s.chars().all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '-')) {
            return;
        }
        // At most the rows written so far: the ones fetched until now.
        let (before, limit) = {
            let g = self.st();
            (iso(now_ms() + 1000), g.counts.pages as usize + 100)
        };
        let sql = format!("SELECT url, {col} AS c, json_extract(data, '{path}') AS v FROM {t} WHERE json_extract(data, '{path}') IS NOT NULL AND fetched <= :before LIMIT {limit}", col = c.name, t = self.table);
        let rows = match strata.query(&sql, json!({ "before": before })).await {
            Ok(j) => j["rows"].as_array().cloned().unwrap_or_default(),
            Err(e) => {
                self.problem(path, "fill", format!("reading the rows to fill column {}: {e}", c.name));
                return;
            }
        };
        let ops: Vec<Value> = rows
            .iter()
            .filter(|r| r["c"].is_null())
            .filter_map(|r| Some(json!({"op": "set", "table": self.table, "key": r["url"].as_str()?, "field": c.name, "value": crate::infer::coerce(&r["v"], c.ty)?})))
            .collect();
        let mut filled = 0usize;
        let mut out = crate::strata::Written::default();
        for (i, part) in ops.chunks(500).enumerate() {
            let key = format!("fill-{}-{}-{}-{i}", self.id, self.started_ms, c.name);
            match strata.write(part.to_vec(), &key, Duration::from_secs(300), &mut out).await {
                Ok(_) => filled += part.len(),
                Err(e) => {
                    self.problem(path, "fill", format!("filling column {}: {e}", c.name));
                    break;
                }
            }
        }
        if filled > 0 {
            self.event("fill", format!("column {} filled on the {filled} rows written before it, from data.{path}", c.name), None);
        }
    }

    async fn flush(&self, strata: &Strata, rows: Vec<serde_json::Map<String, Value>>, n: u64) -> Result<(), String> {
        let key = format!("crawl-{}-{}-{n}", self.id, self.started_ms);
        let w = strata.upsert(&self.table, rows, &key, Duration::from_secs(300)).await.map_err(|e| format!("writing to {}: {e}", strata.url))?;
        let mut g = self.st();
        g.counts.written += w.rows as u64;
        g.counts.busy += w.busy as u64;
        g.counts.retries += w.retries as u64;
        g.counts.refused += w.refused.len() as u64;
        drop(g);
        for (url, why) in w.refused {
            self.problem(&url, "refused", why);
        }
        Ok(())
    }

    /// The table: made from the sample's plan, or, when it is there
    /// already, its own columns with any of the plan's it lacks added.
    async fn table_plan(&self, strata: &Strata, stats: &Stats) -> Result<Plan, String> {
        let fresh = Plan::from_sample(&self.table, stats, &self.spec.rules, self.spec.promote_at, self.spec.embed.clone());
        let existing = strata.table(&self.table).await.map_err(|e| format!("reading the tables of {}: {e}", strata.url))?;
        match existing {
            None => {
                let sql = fresh.create_sql();
                strata.sql(&sql).await.map_err(|e| format!("making table {}: {e}", self.table))?;
                let cols = fresh.columns.len() + usize::from(fresh.data);
                let typed = fresh.columns.iter().filter(|c| matches!(c.source, Source::Path(_) | Source::Rule(_))).count();
                self.event("schema", format!("table {} made from {} pages: {cols} columns, {typed} of them from the pages' fields", self.table, stats.pages), Some(sql));
                Ok(fresh)
            }
            Some(fields) => {
                // The most pages' first: a column named alike by two
                // sources is the one more pages have.
                let known: Vec<String> = stats.top(usize::MAX).into_iter().map(|t| t.0).collect();
                let mut plan = Plan::from_existing(&self.table, &fields, &self.spec.rules, self.spec.embed.clone(), &known);
                if !plan.columns.iter().any(|c| c.name == "url") {
                    return Err(format!("table {} is there, without a url column: name another table", self.table));
                }
                let mut added = Vec::new();
                for c in &fresh.columns {
                    if !plan.columns.iter().any(|x| x.name == c.name) {
                        let mut c = c.clone();
                        c.added_at = Some(0);
                        let sql = plan.add_sql(&c);
                        if strata.sql(&sql).await.is_ok() {
                            added.push(c.name.clone());
                            plan.columns.push(c);
                        }
                    }
                }
                self.event(
                    "schema",
                    format!("table {} is there: its {} columns kept{}", self.table, fields.len(), if added.is_empty() { String::new() } else { format!(", {} added ({})", added.len(), added.join(", ")) }),
                    None,
                );
                Ok(plan)
            }
        }
    }

    /// Adds a column for each field of `data` that `promote_at` of the
    /// pages since the table was made have, once 50 pages have been read.
    /// The columns added.
    async fn promote(&self, strata: &Strata, plan: &mut Plan) -> Vec<crate::infer::Column> {
        let mut added = Vec::new();
        let cands: Vec<(String, crate::infer::Ty, crate::infer::FieldStats, f64)> = {
            let g = self.st();
            if g.stats.pages < 50 || !plan.data {
                return added;
            }
            let n = g.stats.pages as f64;
            g.stats
                .top(400)
                .into_iter()
                .filter(|(p, share, ty, _)| *share >= self.spec.promote_at && ty.is_some() && !p.starts_with("rule.") && !g.failed.contains(p) && !plan.columns.iter().any(|c| c.source == Source::Path(p.clone())))
                .map(|(p, _, ty, _)| {
                    let s = g.stats.fields[&p].clone();
                    (p, ty.expect("filtered"), s, n)
                })
                .collect()
        };
        for (path, ty, s, n) in cands {
            let mut trial = plan.clone();
            let Some(c) = trial.add_path(&path, ty, &s, n, Some(self.st().counts.pages)).cloned() else { continue };
            let sql = plan.add_sql(&c);
            match strata.sql(&sql).await {
                Ok(_) => {
                    *plan = trial;
                    let mut g = self.st();
                    g.counts.added_columns += 1;
                    g.plan = Some(plan.clone());
                    let at = g.counts.pages;
                    drop(g);
                    self.event("column", format!("column {} {} added: {} of the pages since the start have {path} (at page {at})", c.name, c.ty.sql(), pct(c.fill)), Some(sql));
                    added.push(c);
                }
                Err(e) => {
                    self.problem(&path, "alter", format!("adding column {}: {e}", c.name));
                    // Not tried again.
                    self.st().failed.insert(path);
                }
            }
        }
        added
    }

    /// Links on the deepest pages that were not followed (and were not
    /// reached another way).
    fn beyond_depth(&self) -> usize {
        let b = self.beyond.lock().unwrap_or_else(|e| e.into_inner());
        self.frontier.unseen(&b)
    }

    /// Why a crawl that was not stopped ended: the page limit, the depth
    /// limit (with the links it left out), a full queue, or no links left.
    fn why_done(&self) -> String {
        let f = self.frontier.counts();
        let beyond = self.beyond_depth();
        let limit = self.frontier.limit();
        if f.handed >= limit && f.queued > 0 {
            format!("it reached max pages ({limit}) with {} links still queued: raise max pages to read them", f.queued)
        } else if beyond > 0 {
            format!("it reached max depth ({}): {beyond} links on the deepest pages were not followed; raise max depth to reach them", self.spec.max_depth)
        } else if f.dropped > 0 {
            format!("the queue was full: {} links were dropped", f.dropped)
        } else if f.handed >= limit {
            format!("it reached max pages ({limit})")
        } else {
            "it followed every link in scope".into()
        }
    }

    /// A line for lists.
    pub fn summary(&self) -> Value {
        let g = self.st();
        let f = self.frontier.counts();
        json!({
            "id": self.id, "table": self.table, "seeds": self.spec.seeds, "phase": g.phase,
            "started_ms": self.started_ms, "finished_ms": g.finished_ms,
            "fetched": g.counts.fetched, "written": g.counts.written, "queued": f.queued,
            "error": g.error, "rate": rate(&g.series),
        })
    }

    /// Everything the view shows.
    pub fn status(&self) -> Value {
        let g = self.st();
        let f = self.frontier.counts();
        let fields: Vec<Value> = g
            .stats
            .top(80)
            .into_iter()
            .map(|(path, share, ty, example)| {
                let column = g.plan.as_ref().and_then(|p| p.columns.iter().find(|c| c.source == Source::Path(path.clone()) || matches!(&c.source, Source::Rule(r) if format!("rule.{r}") == path)).map(|c| c.name.clone()));
                json!({"path": path, "share": share, "type": ty, "example": example, "column": column})
            })
            .collect();
        json!({
            "id": self.id,
            "table": self.table,
            "spec": self.spec,
            "phase": g.phase,
            "error": g.error,
            "started_ms": self.started_ms,
            "finished_ms": g.finished_ms,
            "counts": g.counts,
            "frontier": f,
            "beyond_depth": self.beyond_depth(),
            "why_done": g.why_done,
            "rate": rate(&g.series),
            "series": g.series.iter().rev().take(600).rev().collect::<Vec<_>>(),
            "plan": g.plan,
            "sql": g.plan.as_ref().map(Plan::create_sql),
            "template": g.template,
            "fields": fields,
            "pages_seen": g.stats.pages,
            "recent": g.recent.iter().rev().collect::<Vec<_>>(),
            "problems": g.problems.iter().rev().collect::<Vec<_>>(),
            "events": g.events,
        })
    }
}

/// Pages fetched and rows written a second, over the last 10 seconds.
fn rate(series: &VecDeque<Sample>) -> Value {
    let (Some(last), Some(first)) = (series.back(), series.iter().rev().nth(10).or(series.front())) else { return json!({"fetched": 0.0, "written": 0.0}) };
    let dt = (last.t_ms.saturating_sub(first.t_ms)) as f64 / 1000.0;
    if dt <= 0.0 {
        return json!({"fetched": 0.0, "written": 0.0});
    }
    json!({"fetched": (last.fetched - first.fetched) as f64 / dt, "written": (last.written - first.written) as f64 / dt})
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

/// The learned fields of a page read while sampling: from its signatures.
fn apply_template(f: &mut Fetched, template: &[TemplateField]) {
    let mut page = serde_json::Map::new();
    for t in template {
        if let Some((_, text, _)) = f.page.signatures.iter().find(|(s, ..)| *s == t.selector) {
            page.insert(t.name.clone(), Value::String(text.clone()));
        }
    }
    f.page.signatures.clear();
    if !page.is_empty() {
        f.page.structured.insert("page".into(), Value::Object(page));
    }
}

/// A link's shape: how many segments its path has, its first, and its
/// last when that names a file (`/catalogue/*/index.html`). A site's list
/// links one shape of page (its items) many times.
fn shape(u: &Url) -> (usize, String, String) {
    let segs: Vec<&str> = u.path().split('/').filter(|s| !s.is_empty()).collect();
    let last = segs.last().filter(|l| l.contains('.') && !l.chars().any(|c| c.is_ascii_digit())).map_or("*".to_string(), |l| l.to_string());
    (segs.len(), segs.first().map_or(String::new(), |f| f.to_string()), last)
}

/// Up to `want` of the links, taken in turn from each shape of link (the
/// most linked shapes first, at most `shapes` of them), so that a preview
/// and a crawl's sample read a site's lists and its items alike.
fn spread(links: Vec<Url>, want: usize, shapes: usize) -> Vec<Url> {
    let mut groups: Vec<((usize, String, String), VecDeque<Url>)> = Vec::new();
    for l in links {
        let k = shape(&l);
        match groups.iter_mut().find(|(g, _)| *g == k) {
            Some((_, v)) => v.push_back(l),
            None => groups.push((k, VecDeque::from([l]))),
        }
    }
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    // A shape linked once (the home page, an about page) comes after the
    // shapes linked many times: those are the site's lists and items.
    let (mut many, ones): (Vec<_>, Vec<_>) = groups.into_iter().partition(|(_, v)| v.len() > 1);
    many.truncate(shapes);
    let mut out = Vec::new();
    while out.len() < want && many.iter().any(|(_, v)| !v.is_empty()) {
        for (_, v) in many.iter_mut() {
            if out.len() >= want {
                break;
            }
            if let Some(u) = v.pop_front() {
                out.push(u);
            }
        }
    }
    out.extend(ones.into_iter().flat_map(|(_, v)| v).take(want.saturating_sub(out.len())));
    out
}

/// A preview: the seed and up to `n - 1` pages it links to, read and
/// planned as a crawl would, and nothing written.
pub async fn preview(spec: &Spec, n: usize) -> Result<Value, String> {
    spec.check()?;
    let seeds = spec.seeds()?;
    let table = spec.table_name()?;
    let fetcher = Fetcher::new(&crate::fetch::user_agent(spec.user_agent.as_deref(), spec.contact.as_deref()), Duration::from_millis(spec.timeout_ms.max(1000)), spec.max_bytes.max(64 << 10), 4, spec.robots);
    let opts_learn = spec.learn;
    let read = |body: String, u: Url, rules: Vec<Rule>| tokio::task::spawn_blocking(move || extract::extract(&body, &u, &extract::Options { rules: &rules, template: &[], learn: opts_learn, max_text: 200_000 }));
    let mut pages: Vec<Fetched> = Vec::new();
    let mut problems = Vec::new();
    let to_fetched = |u: &Url, got: &crate::fetch::Got, page: extract::Page, depth: u32| Fetched {
        url: u.to_string(),
        host: u.host_str().unwrap_or("").to_string(),
        depth,
        status: got.status,
        fetched: iso(now_ms()),
        bytes: got.bytes,
        fetch_ms: got.ms,
        page,
    };
    let first = fetcher.page(&seeds[0]).await.map_err(|m| format!("{}: {m}", seeds[0]))?;
    let fu = urls::normalize(&first.url).unwrap_or_else(|| seeds[0].clone());
    let page = read(first.body.clone(), fu.clone(), spec.rules.clone()).await.map_err(|_| "the page could not be read".to_string())?;
    // The links to read next: in scope, spread over the page, the
    // deepest paths first (a list's items rather than its menu).
    let include: Vec<Regex> = spec.include.iter().filter_map(|r| Regex::new(r).ok()).collect();
    let exclude: Vec<Regex> = spec.exclude.iter().filter_map(|r| Regex::new(r).ok()).collect();
    let keep: Vec<Regex> = spec.keep.iter().filter_map(|r| Regex::new(r).ok()).collect();
    let follows = |l: &Url| {
        let s = l.as_str();
        !urls::not_a_page(l) && urls::in_scope(spec.scope, &seeds, l) && (include.is_empty() || include.iter().any(|r| r.is_match(s))) && !exclude.iter().any(|r| r.is_match(s))
    };
    let kept = |u: &Url| keep.is_empty() || keep.iter().any(|r| r.is_match(u.as_str()));
    let mut seen: HashSet<String> = HashSet::from([fu.to_string()]);
    let mut next: Vec<Url> = page.links.iter().filter(|l| follows(l)).cloned().collect();
    pages.push(to_fetched(&fu, &first, page, 0));
    let fetcher = Arc::new(fetcher);
    // Two levels within the budget: a shop's home links to its lists, whose
    // links are its products (DAP-14: the preview read the home and a list,
    // and showed no product's fields). A level's pages are its links that
    // would be rows, spread over their shapes; links that would not be
    // rows are read only when nothing else is left, for their links.
    for depth in 1..=2u32 {
        let room = n.saturating_sub(pages.len());
        if room == 0 || next.is_empty() {
            break;
        }
        next.retain(|u| seen.insert(u.to_string()));
        let (rows, others): (Vec<Url>, Vec<Url>) = next.drain(..).partition(|u| kept(u));
        let mut picked = spread(rows, room, 4);
        picked.truncate(room);
        if picked.len() < room && depth == 1 {
            // the lists the rows are on (a shop's keep is its products)
            let mut more = spread(others, room - picked.len(), 4);
            picked.append(&mut more);
        }
        let mut set = tokio::task::JoinSet::new();
        for u in picked {
            let fetcher = fetcher.clone();
            let rules = spec.rules.clone();
            set.spawn(async move {
                let got = fetcher.page(&u).await;
                match got {
                    Ok(g) => {
                        let page = tokio::task::spawn_blocking({
                            let body = g.body.clone();
                            let u = u.clone();
                            move || extract::extract(&body, &u, &extract::Options { rules: &rules, template: &[], learn: opts_learn, max_text: 200_000 })
                        })
                        .await
                        .map_err(|_| (u.to_string(), "could not be read".to_string()))?;
                        Ok((u, g, page))
                    }
                    Err(m) => Err((u.to_string(), m.to_string())),
                }
            });
        }
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok((u, g, page))) => {
                    next.extend(page.links.iter().filter(|l| follows(l) && !seen.contains(l.as_str())).cloned());
                    pages.push(to_fetched(&u, &g, page, depth));
                }
                Ok(Err((u, m))) => problems.push(json!({"url": u, "message": m})),
                Err(_) => {}
            }
        }
    }
    // One of each page (`/` and `/index.html` are often one), and only
    // those that would be rows.
    let mut hashes = HashSet::new();
    pages.retain(|f| f.page.words == 0 || hashes.insert(f.page.content_hash));
    let rows_of: Vec<&Fetched> = pages.iter().filter(|f| kept(&Url::parse(&f.url).unwrap_or_else(|_| seeds[0].clone())) || pages.len() == 1).collect();
    let sigs: Vec<(&[(String, String, u32)], Option<&str>)> = rows_of.iter().map(|f| (&f.page.signatures[..], f.page.title.as_deref())).collect();
    let template = if spec.learn { extract::learn_template(&sigs, spec.promote_at.max(0.2)) } else { Vec::new() };
    let mut pages2: Vec<Fetched> = rows_of.into_iter().cloned().collect();
    for f in &mut pages2 {
        apply_template(f, &template);
    }
    let mut stats = Stats::default();
    for f in &pages2 {
        stats.add(f, &spec.rules);
    }
    let plan = Plan::from_sample(&table, &stats, &spec.rules, spec.promote_at, spec.embed.clone());
    let fields: Vec<Value> = stats
        .top(120)
        .into_iter()
        .map(|(path, share, ty, example)| {
            let column = plan.columns.iter().find(|c| c.source == Source::Path(path.clone()) || matches!(&c.source, Source::Rule(r) if format!("rule.{r}") == path)).map(|c| c.name.clone());
            json!({"path": path, "share": share, "type": ty, "example": example, "column": column})
        })
        .collect();
    let rows: Vec<Value> = pages2.iter().take(3).map(|f| Value::Object(plan.row(f))).map(|mut r| {
        if let Some(b) = r.get_mut("body") {
            if let Some(s) = b.as_str() {
                *b = Value::String(s.chars().take(400).collect::<String>() + if s.len() > 400 { "…" } else { "" });
            }
        }
        r
    }).collect();
    Ok(json!({
        "table": table,
        "pages": pages2.iter().map(|f| json!({"url": f.url, "title": f.page.title, "kind": f.page.kind, "words": f.page.words, "links": f.page.links.len(), "ms": f.fetch_ms})).collect::<Vec<_>>(),
        "read": pages.len(),
        "problems": problems,
        "template": template,
        "fields": fields,
        "plan": plan,
        "sql": plan.create_sql(),
        "rows": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(max_pages: u64) -> Arc<Job> {
        let spec: Spec = serde_json::from_value(json!({"seeds": ["https://shop.example/"], "max_pages": max_pages, "max_depth": 3})).unwrap();
        Job::new("j1".into(), spec).unwrap()
    }

    /// A crawl that ran to its end says why: no links left, the depth (with
    /// the links it left out, less those reached another way), or the page
    /// limit (DAP-10: a crawl cut by its depth said only "done").
    #[test]
    fn a_finished_crawl_says_why_it_ended() {
        let j = job(100);
        assert_eq!(j.why_done(), "it followed every link in scope");
        for p in ["/a", "/b", "/c"] {
            j.beyond.lock().unwrap().insert(urls::hash(&format!("https://shop.example{p}")));
        }
        // one of them was reached by a shorter path after all
        assert!(j.frontier.push(Item::new(Url::parse("https://shop.example/c").unwrap(), 1)));
        assert_eq!(j.beyond_depth(), 2);
        assert_eq!(j.why_done(), "it reached max depth (3): 2 links on the deepest pages were not followed; raise max depth to reach them");
        assert_eq!(j.status()["beyond_depth"], 2);
    }
}
