//! A query's profile: the timed steps of its execution
//! ([`ExecStats::steps`](crate::query::exec::ExecStats::steps)), which the
//! executor, the operators of a compound query and a coordinator's merge
//! record as they run, for an `EXPLAIN ANALYZE` of every query.
//!
//! The steps are a tree in pre-order: a step's children follow it, one
//! level deeper ([`Step::depth`]). Each is recorded once it ends, its
//! children already recorded at depth 0 after the mark taken when it
//! began; [`wrap`] then puts the step before them and moves them a level
//! down, so that no step needs to know how deep it runs.
//!
//! Times are microseconds since [`epoch`], a moment fixed once per
//! process, so that steps recorded on any thread, by any executor of a
//! compound query, line up; a server moves them onto its request's start
//! ([`rebase`]) before answering, and a coordinator moves a shard's onto
//! its own clock. Reading the clock costs about 25 ns, so every query
//! records its steps. The CPU time of a step's thread ([`thread_cpu_us`])
//! costs a system call, about 0.3 µs, and is read only for a query run
//! with [`ExecOptions::profile`](crate::query::exec::ExecOptions::profile),
//! as are the bytes a step allocated, when the process counts each
//! thread's allocations ([`count_allocations`]).

use std::sync::OnceLock;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// One timed step of a query's execution.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// What ran: `rank`, `bm25`, `vector`, `segment`, `filter`, `rerank`,
    /// `project`, a compound query's `join` or `with`, a coordinator's
    /// `shard` and `merge`, ...
    pub op: String,
    /// What it ran on: a field and its query, a segment's rows, a shard.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Depth in the tree: the steps after this one that are deeper than it,
    /// up to the next that is not, are inside it.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub depth: u16,
    /// When it began (see the module's documentation), and how long it
    /// took, in microseconds.
    pub start_us: f64,
    pub us: f64,
    /// CPU time spent in it, on every thread that worked for it, in
    /// microseconds; measured with `ExecOptions::profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_us: Option<f64>,
    /// Bytes allocated in it, and how many allocations, on every thread
    /// that worked for it; measured with `ExecOptions::profile` in a
    /// process that counts them ([`count_allocations`]). Bytes allocated,
    /// not kept: what it freed is not taken off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alloc_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocs: Option<u64>,
    /// The rows (documents, candidates, groups) it ended with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// The segment it searched, by ordinal in its snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seg: Option<u32>,
    /// Which of the query's threads ran a segment's step: 0 the thread the
    /// query runs on, 1 and up the pool's threads that helped
    /// ([`super::pool::Pool`]), in the order they joined.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub thread: u32,
    /// The shard a coordinator's step asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<u32>,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// The most steps a query keeps: one over many segments, or a compound
/// query over many shards, could make thousands. Past it, steps are
/// dropped (and counted, [`super::exec::ExecStats::steps_dropped`]).
pub const MAX_STEPS: usize = 4000;

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// The moment step times count from.
pub fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

/// `t` in microseconds since [`epoch`], to a tenth.
pub fn us_at(t: Instant) -> f64 {
    tenth(t.saturating_duration_since(epoch()).as_nanos() as f64 / 1e3)
}

/// Microseconds to a tenth, which is all a step's clock tells.
pub fn tenth(us: f64) -> f64 {
    (us * 10.0).round() / 10.0
}

/// The CPU time this thread has spent, in microseconds; `None` where the
/// system cannot tell.
pub fn thread_cpu_us() -> Option<f64> {
    #[cfg(unix)]
    {
        // SAFETY: clock_gettime writes the struct it is given.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0;
        ok.then(|| ts.tv_sec as f64 * 1e6 + ts.tv_nsec as f64 / 1e3)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

static ALLOCATED: OnceLock<fn() -> (u64, u64)> = OnceLock::new();

/// Has steps measured with CPU time carry the bytes they allocated too:
/// `f` answers the bytes the calling thread has allocated so far, and how
/// many allocations, as a counting allocator keeps them. Set once per
/// process; later calls do nothing.
pub fn count_allocations(f: fn() -> (u64, u64)) {
    let _ = ALLOCATED.set(f);
}

/// The bytes the calling thread has allocated so far, and how many
/// allocations; `None` unless the process counts them.
pub fn thread_allocated() -> Option<(u64, u64)> {
    ALLOCATED.get().map(|f| f())
}

/// Where a step began: the steps recorded so far, the time, and with CPU
/// time measured, this thread's CPU time and allocations and what helper
/// threads had spent and allocated for the query by then
/// ([`Clock::helped`]).
#[derive(Clone, Copy, Debug)]
pub struct Mark {
    pub at: usize,
    pub t0: Instant,
    pub cpu0: Option<(f64, f64)>,
    pub alloc0: Option<((u64, u64), (u64, u64))>,
}

/// What a query's steps are timed with: whether CPU time is measured, and
/// the CPU time helper threads spent on the query so far and what they
/// allocated, which a step on the query's thread counts as its own when it
/// spans their work.
#[derive(Debug, Default)]
pub struct Clock {
    pub cpu: bool,
    pub helped: f64,
    pub helped_alloc: (u64, u64),
}

impl Clock {
    pub fn new(cpu: bool) -> Clock {
        // Fixed before any step's time is read, so none comes before it.
        epoch();
        Clock { cpu, ..Clock::default() }
    }

    /// A step begins, `steps` being the steps recorded so far.
    pub fn mark(&self, steps: &[Step]) -> Mark {
        let cpu0 = if self.cpu { thread_cpu_us().map(|c| (c, self.helped)) } else { None };
        let alloc0 = if self.cpu { thread_allocated().map(|a| (a, self.helped_alloc)) } else { None };
        Mark { at: steps.len(), t0: Instant::now(), cpu0, alloc0 }
    }

    /// What helper threads did for the query in a step of theirs: it
    /// counts in the steps around it.
    pub fn helped_by(&mut self, s: &Step) {
        self.helped += s.cpu_us.unwrap_or(0.0);
        self.helped_alloc.0 += s.alloc_bytes.unwrap_or(0);
        self.helped_alloc.1 += s.allocs.unwrap_or(0);
    }

    /// The step that began at `m` ends: it goes before the steps recorded
    /// since, which move under it. Returns where it went, `None` if the cap
    /// dropped it.
    pub fn wrap(&self, steps: &mut Vec<Step>, m: Mark, op: &str, detail: String, rows: Option<u64>) -> Option<usize> {
        let cpu_us = match m.cpu0 {
            Some((c0, h0)) => thread_cpu_us().map(|c| tenth((c - c0).max(0.0) + (self.helped - h0).max(0.0))),
            None => None,
        };
        let (alloc_bytes, allocs) = match (m.alloc0, thread_allocated()) {
            (Some(((b0, n0), (hb0, hn0))), Some((b, n))) => {
                let (hb, hn) = self.helped_alloc;
                (Some(b.saturating_sub(b0) + hb.saturating_sub(hb0)), Some(n.saturating_sub(n0) + hn.saturating_sub(hn0)))
            }
            _ => (None, None),
        };
        let step = Step { op: op.to_string(), detail, start_us: us_at(m.t0), us: tenth(m.t0.elapsed().as_nanos() as f64 / 1e3), cpu_us, alloc_bytes, allocs, rows, ..Step::default() };
        wrap(steps, m.at, step).then_some(m.at.min(steps.len().saturating_sub(1)))
    }
}

/// Puts `step` at `at`, before the steps recorded since it began, and
/// moves those a level down, under it. Past [`MAX_STEPS`] the step is
/// dropped; it returns whether it was kept.
pub fn wrap(steps: &mut Vec<Step>, at: usize, mut step: Step) -> bool {
    let at = at.min(steps.len());
    if steps.len() >= MAX_STEPS {
        return false;
    }
    for s in &mut steps[at..] {
        s.depth = s.depth.saturating_add(1);
    }
    step.depth = 0;
    steps.insert(at, step);
    true
}

/// Appends `inner` (a tree of its own, from depth 0) `depth` levels down,
/// up to [`MAX_STEPS`] steps in all. Returns the steps dropped.
pub fn nest(steps: &mut Vec<Step>, inner: &[Step], depth: u16) -> usize {
    let room = MAX_STEPS.saturating_sub(steps.len());
    let take = inner.len().min(room);
    steps.extend(inner[..take].iter().map(|s| Step { depth: s.depth.saturating_add(depth), ..s.clone() }));
    inner.len() - take
}

/// `text` cut to about `max` bytes (at a character), with "…" when cut,
/// for a step's detail.
pub fn short(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Moves step times so that they count from `t0_us` (microseconds since
/// [`epoch`]): from a request's start.
pub fn rebase(steps: &mut [Step], t0_us: f64) {
    for s in steps {
        s.start_us = tenth(s.start_us - t0_us);
    }
}

/// Shifts step times by `by` microseconds: a shard's, from its request's
/// start onto the coordinator's clock.
pub fn shift(steps: &mut [Step], by: f64) {
    for s in steps {
        s.start_us = tenth(s.start_us + by);
    }
}

/// Steps for the time around a query's run that its own steps do not
/// hold, for a server that ran it on a thread of its own: from `queued`,
/// when it asked for the thread, to `started`, when the run began
/// ("wait for a thread"); from the end of its last step to `ended`, when
/// the run returned ("finish": the executor's state freed, the answer
/// built); and from then to `back`, when the server had the answer again
/// ("hand back"). Each is left out under a microsecond.
pub fn around(steps: &mut Vec<Step>, queued: Instant, started: Instant, ended: Instant, back: Instant) {
    let gap = |op: &str, detail: &str, a: f64, b: f64| Step { op: op.into(), detail: detail.into(), start_us: tenth(a), us: tenth(b - a), ..Step::default() };
    let last = steps.iter().filter(|s| s.depth == 0).map(|s| s.start_us + s.us).fold(us_at(started), f64::max);
    let (q, s, e, b) = (us_at(queued), us_at(started), us_at(ended), us_at(back));
    if s - q >= 1.0 {
        steps.insert(0, gap("wait for a thread", "for a thread to run on", q, s));
    }
    if e - last >= 1.0 {
        steps.push(gap("finish", "the executor's state freed, the answer built", last, e));
    }
    if b - e >= 1.0 {
        steps.push(gap("hand back", "to the server's task", e, b));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(op: &str) -> Step {
        Step { op: op.into(), ..Step::default() }
    }

    #[test]
    fn a_step_goes_before_the_steps_inside_it() {
        let clock = Clock::new(true);
        let mut steps = Vec::new();
        let outer = clock.mark(&steps);
        steps.push(step("a"));
        let inner = clock.mark(&steps);
        steps.push(step("b"));
        clock.wrap(&mut steps, inner, "inner", String::new(), Some(3));
        clock.wrap(&mut steps, outer, "outer", "x".into(), None);
        let shape: Vec<(&str, u16)> = steps.iter().map(|s| (s.op.as_str(), s.depth)).collect();
        assert_eq!(shape, [("outer", 0), ("a", 1), ("inner", 1), ("b", 2)]);
        assert_eq!(steps[2].rows, Some(3));
        assert!(steps[0].us >= steps[2].us);
        assert!(steps[0].start_us <= steps[2].start_us);
        if cfg!(unix) {
            assert!(steps[0].cpu_us.is_some());
        }
    }

    #[test]
    fn nested_trees_keep_their_shape_and_the_cap_holds() {
        let mut steps = vec![step("shard")];
        let inner = vec![step("rank"), Step { depth: 1, ..step("segment") }];
        assert_eq!(nest(&mut steps, &inner, 1), 0);
        assert_eq!(steps.iter().map(|s| s.depth).collect::<Vec<_>>(), [0, 1, 2]);
        let mut full = vec![step("x"); MAX_STEPS];
        assert_eq!(nest(&mut full, &inner, 0), 2);
        assert!(!wrap(&mut full, 0, step("y")));
        let mut t = vec![Step { start_us: 150.0, ..step("a") }];
        rebase(&mut t, 100.0);
        assert_eq!(t[0].start_us, 50.0);
        shift(&mut t, 25.5);
        assert_eq!(t[0].start_us, 75.5);
    }

    #[test]
    fn the_time_around_a_run_is_steps_too() {
        let ms = std::time::Duration::from_millis;
        let queued = Instant::now();
        let (started, ended, back) = (queued + ms(1), queued + ms(5), queued + ms(6));
        let mut steps = vec![Step { start_us: us_at(started) + 100.0, us: 2000.0, ..step("rank") }, Step { depth: 1, start_us: us_at(started) + 200.0, us: 3000.0, ..step("segment") }];
        around(&mut steps, queued, started, ended, back);
        let ops: Vec<&str> = steps.iter().map(|s| s.op.as_str()).collect();
        assert_eq!(ops, ["wait for a thread", "rank", "segment", "finish", "hand back"]);
        assert!((steps[0].us - 1000.0).abs() < 1.0, "{:?}", steps[0]);
        // "finish" runs from the end of the last step at the top.
        assert!((steps[3].start_us - (us_at(started) + 2100.0)).abs() < 1.0 && (steps[3].us - 1900.0).abs() < 1.0, "{:?}", steps[3]);
        assert!((steps[4].us - 1000.0).abs() < 1.0);
        let mut none = Vec::new();
        around(&mut none, queued, queued, queued, queued);
        assert!(none.is_empty());
    }
}
