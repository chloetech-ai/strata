//! Memory: how much the server's heap holds, how fast it allocates, and
//! where.
//!
//! [`Counting`] is the server's global allocator (the binary's
//! `#[global_allocator]`): the system's, counting as it goes.
//! - Each allocation and free adds to or takes from one of 64 counters of
//!   the bytes in use, picked by thread so that threads seldom share one.
//!   Their sum is the heap in use ([`stats`]); [`watch`] keeps its peak,
//!   sampled every 100 ms.
//! - Each thread counts the bytes it allocated and its allocations, which
//!   a query's steps read ([`thread_allocated`]), and adds them to the
//!   process's totals every 64 allocations and frees (a thread that ends
//!   leaves fewer than that uncounted).
//! - While an allocation profile is taken ([`take`]), about one allocation
//!   in every `every` bytes a thread allocates records its stack and the
//!   bytes it stands for; one freed before the profile ends is marked so.
//!   The profile is where the bytes allocated while it was taken came
//!   from, and where those still in use at its end are held.
//!
//! In a binary without the allocator (a program of its own that uses this
//! crate), nothing is counted and [`installed`] says so.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::profiler::{clean, pool_name, Refused};

/// The system's allocator, counting: see the module's documentation.
pub struct Counting;

// SAFETY: every call goes to the system's allocator with the same
// arguments; the counting around it neither allocates through this
// allocator in a way that recurses without bound (a thread records a
// sample with `busy` set, so allocations it makes then are only counted)
// nor touches the memory it hands out.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            allocated(p, l.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = System.alloc_zeroed(l);
        if !p.is_null() {
            allocated(p, l.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        freed(p, l.size());
        System.dealloc(p, l)
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let q = System.realloc(p, l, new_size);
        if !q.is_null() {
            freed(p, l.size());
            allocated(q, new_size);
        }
        q
    }
}

const SHARDS: usize = 64;
/// Allocations and frees a thread makes before it adds them to the totals.
const FLUSH: u64 = 64;

/// One of the counters, on a cache line of its own.
#[repr(align(128))]
struct Shard {
    in_use: AtomicI64,
    allocated: AtomicU64,
    allocs: AtomicU64,
    frees: AtomicU64,
}

static SHARDS_: [Shard; SHARDS] = [const { Shard { in_use: AtomicI64::new(0), allocated: AtomicU64::new(0), allocs: AtomicU64::new(0), frees: AtomicU64::new(0) } }; SHARDS];
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static PEAK: AtomicI64 = AtomicI64::new(0);

/// A thread's own counts. It has no destructor, so reading it never
/// allocates and works until the thread is gone.
struct Local {
    shard: Cell<usize>,
    allocated: Cell<u64>,
    allocs: Cell<u64>,
    /// Bytes allocated, allocations and frees not yet in the totals.
    pending: Cell<(u64, u64, u64)>,
    /// Recording a sample: what it allocates is not sampled.
    busy: Cell<bool>,
    /// The profile `until` counts toward, and the bytes left to the next
    /// sample.
    profile: Cell<u32>,
    until: Cell<i64>,
    rng: Cell<u64>,
}

thread_local! {
    static LOCAL: Local = const {
        Local {
            shard: Cell::new(usize::MAX),
            allocated: Cell::new(0),
            allocs: Cell::new(0),
            pending: Cell::new((0, 0, 0)),
            busy: Cell::new(false),
            profile: Cell::new(0),
            until: Cell::new(0),
            rng: Cell::new(0),
        }
    };
}

fn shard(l: &Local) -> &'static Shard {
    let mut i = l.shard.get();
    if i == usize::MAX {
        i = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS;
        l.shard.set(i);
    }
    &SHARDS_[i]
}

fn flush(s: &Shard, (bytes, allocs, frees): (u64, u64, u64)) {
    s.allocated.fetch_add(bytes, Ordering::Relaxed);
    s.allocs.fetch_add(allocs, Ordering::Relaxed);
    s.frees.fetch_add(frees, Ordering::Relaxed);
}

#[inline]
fn allocated(p: *mut u8, size: usize) {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
    let counted = LOCAL.try_with(|l| {
        let s = shard(l);
        s.in_use.fetch_add(size as i64, Ordering::Relaxed);
        l.allocated.set(l.allocated.get() + size as u64);
        l.allocs.set(l.allocs.get() + 1);
        let (b, a, f) = l.pending.get();
        let pending = (b + size as u64, a + 1, f);
        if pending.1 + pending.2 >= FLUSH {
            flush(s, pending);
            l.pending.set((0, 0, 0));
        } else {
            l.pending.set(pending);
        }
        if SAMPLING.load(Ordering::Relaxed) {
            sample(l, p, size);
        }
    });
    if counted.is_err() {
        let s = &SHARDS_[0];
        s.in_use.fetch_add(size as i64, Ordering::Relaxed);
        flush(s, (size as u64, 1, 0));
    }
}

#[inline]
fn freed(p: *mut u8, size: usize) {
    let mut busy = false;
    let counted = LOCAL.try_with(|l| {
        busy = l.busy.get();
        let s = shard(l);
        s.in_use.fetch_sub(size as i64, Ordering::Relaxed);
        let (b, a, f) = l.pending.get();
        let pending = (b, a, f + 1);
        if pending.1 + pending.2 >= FLUSH {
            flush(s, pending);
            l.pending.set((0, 0, 0));
        } else {
            l.pending.set(pending);
        }
    });
    if counted.is_err() {
        let s = &SHARDS_[0];
        s.in_use.fetch_sub(size as i64, Ordering::Relaxed);
        flush(s, (0, 0, 1));
    }
    // Not while recording a sample, which holds the recorder: what it
    // frees then was never sampled.
    if SAMPLING.load(Ordering::Relaxed) && !busy {
        forget(p as usize);
    }
}

/// Whether this binary allocates through [`Counting`].
pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// The bytes the calling thread has allocated so far, and how many
/// allocations: what a query's steps count
/// ([`strata::query::steps::count_allocations`]).
pub fn thread_allocated() -> (u64, u64) {
    LOCAL.try_with(|l| (l.allocated.get(), l.allocs.get())).unwrap_or((0, 0))
}

/// The heap's counts.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    /// Whether the counting allocator is installed; without it the rest
    /// is zero.
    pub installed: bool,
    /// Bytes allocated and not yet freed.
    pub in_use_bytes: i64,
    /// The most bytes in use seen, sampled every 100 ms ([`watch`]) and at
    /// each read.
    pub peak_bytes: i64,
    /// Bytes allocated, allocations and frees since the process started.
    pub allocated_bytes: u64,
    pub allocs: u64,
    pub frees: u64,
}

fn in_use() -> i64 {
    SHARDS_.iter().map(|s| s.in_use.load(Ordering::Relaxed)).sum()
}

/// The heap's counts now.
pub fn stats() -> Stats {
    let in_use = in_use();
    let peak = PEAK.fetch_max(in_use, Ordering::Relaxed).max(in_use);
    let sum = |f: fn(&Shard) -> &AtomicU64| SHARDS_.iter().map(|s| f(s).load(Ordering::Relaxed)).sum::<u64>();
    Stats {
        installed: installed(),
        in_use_bytes: in_use,
        peak_bytes: peak,
        allocated_bytes: sum(|s| &s.allocated),
        allocs: sum(|s| &s.allocs),
        frees: sum(|s| &s.frees),
    }
}

/// Keeps the peak of the heap in use, sampled every 100 ms on a thread of
/// its own (`strata-heap`). Once per process; later calls do nothing.
pub fn watch() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !installed() || STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = std::thread::Builder::new().name("strata-heap".into()).spawn(|| loop {
        PEAK.fetch_max(in_use(), Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
    });
}

// ---------------------------------------------------------------- profiles

/// The longest allocation profile, in seconds.
pub const MAX_SECONDS: f64 = 60.0;
/// The sampling interval's bounds, in bytes: a sample about every
/// `every` bytes a thread allocates.
pub const MIN_EVERY: u64 = 4 << 10;
pub const MAX_EVERY: u64 = 64 << 20;
/// The interval when none is asked for. Each sample walks its thread's
/// stack, so finer costs more: a BM25 query loop ran 3 to 6% fewer times
/// a second sampled every 2 MiB, 6 to 29% at 512 KiB and 20 to 46% at
/// 64 KiB (the server's documentation says how this was measured).
pub const EVERY_DEFAULT: u64 = 2 << 20;
/// The samples whose frees are watched; past them a sample still counts
/// in the bytes allocated, but not in those in use ([`HeapProfile::untracked`]).
const WATCHED: usize = 1 << 16;
/// Frames kept of a sample's stack.
const DEPTH: usize = 96;

static SAMPLING: AtomicBool = AtomicBool::new(false);
static PROFILE: AtomicU32 = AtomicU32::new(0);
static EVERY: AtomicU64 = AtomicU64::new(EVERY_DEFAULT);
static TAKING: AtomicBool = AtomicBool::new(false);
static RECORDER: Mutex<Option<Recorder>> = Mutex::new(None);

/// What a profile records while it is taken.
#[derive(Default)]
struct Recorder {
    by_stack: HashMap<(u64, Vec<usize>), u32>,
    stacks: Vec<Site>,
    /// Each sample: its stack, the bytes it stands for, whether it was
    /// freed.
    samples: Vec<(u32, f64, bool)>,
    untracked: u64,
}

/// A stack that allocated: on which thread, and what its samples stand
/// for.
struct Site {
    thread: String,
    tid: u64,
    ips: Vec<usize>,
    bytes: f64,
    allocs: f64,
}

// The samples being watched, in an open-addressed table that a free looks
// its address up in without a lock: a key is a sampled address, 0 a free
// place, GONE a place whose sample was freed; its value is the sample's
// place in `Recorder::samples`.
const GONE: usize = 1;
const PROBES: usize = 32;
static KEYS: [AtomicUsize; WATCHED] = [const { AtomicUsize::new(0) }; WATCHED];
static VALUES: [AtomicU32; WATCHED] = [const { AtomicU32::new(0) }; WATCHED];

fn place(p: usize) -> usize {
    ((p >> 4) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize >> (usize::BITS - WATCHED.trailing_zeros())
}

/// Watches sampled address `p`, sample `v`; false if the table is full
/// around it.
fn watch_sample(p: usize, v: u32) -> bool {
    let h = place(p);
    for i in 0..PROBES {
        let k = (h + i) & (WATCHED - 1);
        let cur = KEYS[k].load(Ordering::Acquire);
        if (cur == 0 || cur == GONE) && KEYS[k].compare_exchange(cur, p, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            VALUES[k].store(v, Ordering::Release);
            return true;
        }
    }
    false
}

/// A free: if `p` was sampled, its sample is marked freed.
fn forget(p: usize) {
    let h = place(p);
    for i in 0..PROBES {
        let k = (h + i) & (WATCHED - 1);
        let cur = KEYS[k].load(Ordering::Acquire);
        if cur == p {
            if KEYS[k].compare_exchange(p, GONE, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                let v = VALUES[k].load(Ordering::Acquire) as usize;
                if let Ok(mut r) = RECORDER.lock() {
                    if let Some(s) = r.as_mut().and_then(|r| r.samples.get_mut(v)) {
                        s.2 = true;
                    }
                }
            }
            return;
        }
        if cur == 0 {
            return;
        }
    }
}

/// Counts an allocation toward the next sample; past it, records one.
fn sample(l: &Local, p: *mut u8, size: usize) {
    if l.busy.get() {
        return;
    }
    let profile = PROFILE.load(Ordering::Relaxed);
    let every = EVERY.load(Ordering::Relaxed).max(1);
    if l.profile.get() != profile {
        l.profile.set(profile);
        l.until.set(gap(l, every));
    }
    let left = l.until.get() - size as i64;
    if left > 0 {
        l.until.set(left);
        return;
    }
    l.until.set(gap(l, every));
    l.busy.set(true);
    record(p as usize, size, every);
    l.busy.set(false);
}

/// Bytes to the next sample: exponential, `every` on average, so that each
/// byte is as likely as any other to be sampled.
fn gap(l: &Local, every: u64) -> i64 {
    let mut x = l.rng.get();
    if x == 0 {
        x = (l as *const Local as u64) ^ 0x2545_F491_4F6C_DD1D | 1;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    l.rng.set(x);
    let u = ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
    (-u.ln() * every as f64).max(1.0) as i64
}

/// Records the allocation at `p` of `size` bytes: its stack, and the bytes
/// it stands for, `size` over the chance it had to be sampled.
fn record(p: usize, size: usize, every: u64) {
    let mut ips = [0usize; DEPTH];
    let mut n = 0;
    // SAFETY: no other unwinding runs on this thread meanwhile (`busy` is
    // set, and this is not a signal handler).
    unsafe {
        backtrace::trace_unsynchronized(|f| {
            ips[n] = f.ip() as usize;
            n += 1;
            n < DEPTH
        })
    };
    let chance = 1.0 - (-(size as f64) / every as f64).exp();
    let bytes = if chance > 0.0 { size as f64 / chance } else { every as f64 };
    let tid = thread_id();
    let Ok(mut guard) = RECORDER.lock() else { return };
    let Some(r) = guard.as_mut() else { return };
    let key = (tid, ips[..n].to_vec());
    let at = match r.by_stack.get(&key) {
        Some(&i) => i,
        None => {
            let i = r.stacks.len() as u32;
            r.stacks.push(Site { thread: thread_name(), tid, ips: key.1.clone(), bytes: 0.0, allocs: 0.0 });
            r.by_stack.insert(key, i);
            i
        }
    };
    let site = &mut r.stacks[at as usize];
    site.bytes += bytes;
    site.allocs += bytes / size.max(1) as f64;
    let v = r.samples.len() as u32;
    r.samples.push((at, bytes, false));
    if !watch_sample(p, v) {
        r.untracked += 1;
    }
}

fn thread_id() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: pthread_self has no preconditions.
        (unsafe { libc::pthread_self() }) as u64
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// The calling thread's name, without the standard library's (which may
/// allocate, or be gone while the thread ends).
fn thread_name() -> String {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut buf = [0 as libc::c_char; 64];
        // SAFETY: the buffer is as long as it is said to be, and the name
        // written into it ends with a NUL.
        let ok = unsafe { libc::pthread_getname_np(libc::pthread_self(), buf.as_mut_ptr(), buf.len()) } == 0;
        if ok {
            // SAFETY: pthread_getname_np wrote a NUL-terminated string.
            return unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
        }
    }
    String::new()
}

/// An allocation profile: where the bytes allocated while it was taken came
/// from, by stack, and which of them were still in use at its end.
#[derive(Clone, Debug, Default, Serialize)]
pub struct HeapProfile {
    /// A sample about every this many bytes a thread allocated.
    pub every: u64,
    /// How long it was taken, in seconds.
    pub seconds: f64,
    /// Bytes allocated and allocations made while it was taken, counted
    /// (not sampled): every thread's, to within 64 allocations a thread.
    pub allocated_bytes: u64,
    pub allocs: u64,
    /// The heap in use when it began and when it ended, counted.
    pub in_use_start: i64,
    pub in_use_end: i64,
    /// Allocations sampled; those whose frees could not be watched (the
    /// table was full) count as freed.
    pub samples: u64,
    pub untracked: u64,
    /// The names of the frames, once each, as in a CPU profile.
    pub names: Vec<String>,
    /// The threads that allocated, by pool, the most first.
    pub threads: Vec<HeapThreads>,
    /// Each stack that allocated, the most bytes first.
    pub stacks: Vec<HeapStack>,
}

/// The threads of one name.
#[derive(Clone, Debug, Serialize)]
pub struct HeapThreads {
    pub name: String,
    pub count: usize,
    /// Bytes they allocated (estimated from the samples).
    pub bytes: u64,
}

/// A stack and what it allocated, estimated from its samples.
#[derive(Clone, Debug, Serialize)]
pub struct HeapStack {
    pub thread: u32,
    /// Root first, by place in [`HeapProfile::names`].
    pub frames: Vec<u32>,
    /// Bytes allocated, and allocations.
    pub bytes: u64,
    pub allocs: u64,
    /// Of those bytes, the ones still in use when the profile ended.
    pub in_use: u64,
}

impl HeapProfile {
    /// As folded lines, one a stack: the thread, then the frames from the
    /// root, and the bytes allocated (or with `in_use`, still in use).
    pub fn folded(&self, in_use: bool) -> String {
        let mut out = String::new();
        for s in &self.stacks {
            let v = if in_use { s.in_use } else { s.bytes };
            if v == 0 {
                continue;
            }
            out.push_str(&self.threads[s.thread as usize].name.replace(';', ":"));
            for &f in &s.frames {
                out.push(';');
                out.push_str(&self.names[f as usize].replace(';', ":"));
            }
            out.push_str(&format!(" {v}\n"));
        }
        out
    }
}

/// Samples allocations, about one every `every` bytes a thread allocates,
/// while `during` runs, for `seconds` at most (as [`crate::profiler::take`]).
pub async fn take<T>(every: u64, seconds: f64, during: impl Future<Output = T>) -> Result<(HeapProfile, Option<T>), Refused> {
    if !installed() {
        return Err(Refused::Unavailable("this binary does not count its allocations (no `heap::Counting` global allocator)".into()));
    }
    if TAKING.swap(true, Ordering::SeqCst) {
        return Err(Refused::Busy);
    }
    struct Taking;
    impl Drop for Taking {
        fn drop(&mut self) {
            SAMPLING.store(false, Ordering::SeqCst);
            TAKING.store(false, Ordering::SeqCst);
        }
    }
    let taking = Taking;
    let every = every.clamp(MIN_EVERY, MAX_EVERY);
    let seconds = seconds.clamp(0.01, MAX_SECONDS);
    for k in KEYS.iter() {
        k.store(0, Ordering::Relaxed);
    }
    *RECORDER.lock().unwrap_or_else(|e| e.into_inner()) = Some(Recorder::default());
    EVERY.store(every, Ordering::SeqCst);
    PROFILE.fetch_add(1, Ordering::SeqCst);
    let (before, t0) = (stats(), Instant::now());
    SAMPLING.store(true, Ordering::SeqCst);
    let out = tokio::time::timeout(Duration::from_secs_f64(seconds), during).await.ok();
    SAMPLING.store(false, Ordering::SeqCst);
    let (after, took) = (stats(), t0.elapsed().as_secs_f64());
    let r = RECORDER.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    drop(taking);
    let mut p = tokio::task::spawn_blocking(move || {
        let p = report(r);
        crate::profiler::release();
        p
    })
    .await
    .map_err(|e| Refused::Unavailable(e.to_string()))?;
    p.every = every;
    p.seconds = (took * 1000.0).round() / 1000.0;
    p.allocated_bytes = after.allocated_bytes.saturating_sub(before.allocated_bytes);
    p.allocs = after.allocs.saturating_sub(before.allocs);
    p.in_use_start = before.in_use_bytes;
    p.in_use_end = after.in_use_bytes;
    Ok((p, out))
}

/// Names the recorded stacks' frames and counts them by thread and stack.
fn report(r: Recorder) -> HeapProfile {
    let mut in_use = vec![0.0f64; r.stacks.len()];
    for &(s, bytes, freed) in &r.samples {
        if !freed {
            in_use[s as usize] += bytes;
        }
    }
    let mut named: HashMap<usize, Vec<String>> = HashMap::new();
    let stacks = r.stacks.iter().zip(in_use).map(|(site, in_use)| {
        // Root first, each address's inlined functions outermost first.
        let mut frames: Vec<String> = Vec::new();
        for &ip in site.ips.iter().rev() {
            let names = named.entry(ip).or_insert_with(|| {
                let mut n = Vec::new();
                // `{:#}`: without the hashes, legacy (`::h…`) or v0 (`crate[…]`).
                backtrace::resolve(ip as *mut std::ffi::c_void, |s| n.push(s.name().map(|n| clean(&format!("{n:#}"))).unwrap_or_else(|| "Unknown".into())));
                if n.is_empty() {
                    n.push("Unknown".into());
                }
                n.reverse();
                n
            });
            frames.extend(names.iter().cloned());
        }
        (site, callers(frames), in_use)
    });
    let mut names: Vec<String> = Vec::new();
    let mut name_at: HashMap<String, u32> = HashMap::new();
    let mut threads: Vec<(String, std::collections::HashSet<u64>, f64)> = Vec::new();
    let mut thread_at: HashMap<String, u32> = HashMap::new();
    let mut by: HashMap<(u32, Vec<u32>), (f64, f64, f64)> = HashMap::new();
    for (site, frames, in_use) in stacks {
        let t = pool_name(&site.thread, site.tid);
        let ti = *thread_at.entry(t.clone()).or_insert_with(|| {
            threads.push((t, Default::default(), 0.0));
            threads.len() as u32 - 1
        });
        threads[ti as usize].1.insert(site.tid);
        threads[ti as usize].2 += site.bytes;
        let fs: Vec<u32> = frames
            .into_iter()
            .map(|f| {
                *name_at.entry(f.clone()).or_insert_with(|| {
                    names.push(f);
                    names.len() as u32 - 1
                })
            })
            .collect();
        let e = by.entry((ti, fs)).or_default();
        e.0 += site.bytes;
        e.1 += site.allocs;
        e.2 += in_use;
    }
    let mut order: Vec<u32> = (0..threads.len() as u32).collect();
    order.sort_by(|&a, &b| threads[b as usize].2.total_cmp(&threads[a as usize].2));
    let mut place = vec![0u32; threads.len()];
    for (to, &from) in order.iter().enumerate() {
        place[from as usize] = to as u32;
    }
    let mut stacks: Vec<HeapStack> = by
        .into_iter()
        .map(|((t, frames), (bytes, allocs, in_use))| HeapStack { thread: place[t as usize], frames, bytes: bytes.round() as u64, allocs: allocs.round() as u64, in_use: in_use.round() as u64 })
        .collect();
    stacks.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.frames.cmp(&b.frames)));
    HeapProfile {
        samples: r.samples.len() as u64,
        untracked: r.untracked,
        names,
        threads: order.iter().map(|&i| &threads[i as usize]).map(|(name, ids, bytes)| HeapThreads { name: name.clone(), count: ids.len(), bytes: bytes.round() as u64 }).collect(),
        stacks,
        ..HeapProfile::default()
    }
}

/// A sample's frames (root first) up to the allocation: without the
/// allocator's own, from where it was entered on, nor the standard
/// library's allocation functions that called it.
fn callers(mut frames: Vec<String>) -> Vec<String> {
    if let Some(i) = frames.iter().position(|f| allocator_entry(f)) {
        frames.truncate(i);
    }
    while frames.last().is_some_and(|f| allocating(f)) {
        frames.pop();
    }
    // The stack's end, past the thread's start, has no name.
    let unnamed = frames.iter().take_while(|f| *f == "Unknown").count();
    frames.drain(..unnamed);
    frames
}

/// The standard library's functions that get memory for a type: a
/// sample's bytes are its caller's.
fn allocating(f: &str) -> bool {
    ["alloc::alloc::", "<alloc::alloc::Global", "alloc::raw_vec::", "<alloc::raw_vec::", "alloc::boxed::box_new"].iter().any(|p| f.starts_with(p))
}

/// Where a call enters the allocator.
fn allocator_entry(f: &str) -> bool {
    f.contains("strata_server::heap::") || f.starts_with("__rust_alloc") || f.starts_with("__rust_realloc") || f.starts_with("__rdl_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gaps_average_the_interval() {
        let l = Local {
            shard: Cell::new(0),
            allocated: Cell::new(0),
            allocs: Cell::new(0),
            pending: Cell::new((0, 0, 0)),
            busy: Cell::new(false),
            profile: Cell::new(0),
            until: Cell::new(0),
            rng: Cell::new(0),
        };
        let n = 20_000;
        let mean = (0..n).map(|_| gap(&l, 1000) as f64).sum::<f64>() / n as f64;
        assert!((mean - 1000.0).abs() < 40.0, "{mean}");
    }

    #[test]
    fn a_watched_sample_is_found_once_and_freed() {
        let p = 0x7f00_0000_1230usize;
        assert!(watch_sample(p, 7));
        let h = place(p);
        assert!((0..PROBES).any(|i| KEYS[(h + i) & (WATCHED - 1)].load(Ordering::Relaxed) == p));
        forget(p);
        assert!(!(0..PROBES).any(|i| KEYS[(h + i) & (WATCHED - 1)].load(Ordering::Relaxed) == p));
        // Its place is taken again.
        assert!(watch_sample(p, 8));
        forget(p);
    }

    #[test]
    fn the_allocator_s_frames_are_not_the_caller_s() {
        let s = |v: &[&str]| v.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        let frames = s(&[
            "main",
            "strata::db::Db::execute",
            "<alloc::raw_vec::RawVec<T,A>>::grow_one",
            "alloc::alloc::realloc",
            "__rust_realloc",
            "<strata_server::heap::Counting as core::alloc::global::GlobalAlloc>::realloc",
            "std::thread::local::LocalKey<T>::try_with",
            "strata_server::heap::record",
            "backtrace::backtrace::trace_unsynchronized",
        ]);
        // A vector's growth is its user's.
        assert_eq!(callers(frames), s(&["main", "strata::db::Db::execute"]));
        assert_eq!(callers(s(&["main", "f"])), s(&["main", "f"]));
        assert_eq!(callers(s(&["Unknown", "clone3", "start_thread", "f", "__rust_alloc"])), s(&["clone3", "start_thread", "f"]));
        assert_eq!(callers(s(&["f", "<alloc::vec::Vec<u8>>::push", "<alloc::raw_vec::RawVec<u8>>::grow_one", "<alloc::raw_vec::RawVecInner>::finish_grow", "alloc::alloc::realloc", "__rust_realloc"])), s(&["f", "<alloc::vec::Vec<u8>>::push"]));
    }
}
