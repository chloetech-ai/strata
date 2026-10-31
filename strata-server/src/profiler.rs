//! CPU profiles: what every thread of the server was running, sampled.
//!
//! While a profile is taken, a timer interrupts the process `hz` times a
//! second of its CPU time (SIGPROF, through `pprof`), and the thread it
//! interrupts records its stack. The stacks come back counted, root first,
//! by thread, their frames' names in one table: a flame graph's data
//! ([`CpuProfile`]), or folded lines (`thread;root;...;leaf count`) that
//! flamegraph.pl, inferno and speedscope read ([`CpuProfile::folded`]).
//!
//! One profile is taken at a time: the timer is the process's. Sampling
//! slows the threads it interrupts: in a 4-core cloud container, a BM25
//! query run in a loop by one caller ran 6% fewer times a second sampled
//! at 99 Hz and 11% fewer at 997 Hz, and by four callers within the noise
//! (about 4%). The names are read from the binary's symbol table after the
//! timer stops, so that work is not in the profile. The first profile
//! makes the sampler's table, about 70 MB, which the process keeps
//! (`release`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

/// The longest profile, in seconds.
pub const MAX_SECONDS: f64 = 60.0;
/// The most samples a second.
pub const MAX_HZ: i32 = 1000;
/// The most distinct stacks kept; the samples of the rest are counted in
/// [`CpuProfile::dropped`].
pub const MAX_STACKS: usize = 50_000;

/// A CPU profile.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CpuProfile {
    /// Samples asked for a second of CPU time.
    pub hz: i32,
    /// How long it was taken, in seconds.
    pub seconds: f64,
    /// The process's CPU time (user and system, every thread) while it was
    /// taken, in seconds.
    pub cpu_seconds: f64,
    /// Samples taken. The system delivers at most one a tick of its timer
    /// for the whole process (250 a second on many Linux kernels), so a
    /// busy process gets fewer than `hz` a CPU second; a sample stands for
    /// `cpu_seconds / samples` of CPU time.
    pub samples: u64,
    /// The names of the frames, once each; a stack names its frames by
    /// their place here.
    pub names: Vec<String>,
    /// The threads, by name with the number that tells a pool's threads
    /// apart taken off (`strata-query`, `tokio-runtime-worker`), the most
    /// sampled first.
    pub threads: Vec<Threads>,
    /// Each distinct stack, with the threads it ran on and its samples.
    pub stacks: Vec<Stack>,
    /// Samples of stacks past [`MAX_STACKS`].
    pub dropped: u64,
    /// The libraries no sample is taken in, by name (the C library only
    /// before glibc 2.35, see `blocklist`): their CPU time is spread over
    /// the rest.
    pub blocked: Vec<String>,
}

/// The threads of one name.
#[derive(Clone, Debug, Serialize)]
pub struct Threads {
    pub name: String,
    /// How many threads of this name were sampled.
    pub count: usize,
    pub samples: u64,
}

/// A stack and how often it was sampled.
#[derive(Clone, Debug, Serialize)]
pub struct Stack {
    /// Its thread, by place in [`CpuProfile::threads`].
    pub thread: u32,
    /// Its frames, root first, by place in [`CpuProfile::names`].
    pub frames: Vec<u32>,
    pub samples: u64,
}

impl CpuProfile {
    /// The cores it kept busy on average: CPU seconds over wall seconds.
    pub fn cores(&self) -> f64 {
        if self.seconds > 0.0 {
            self.cpu_seconds / self.seconds
        } else {
            0.0
        }
    }

    /// Samples taken a second of CPU time: `hz`, or fewer when the system's
    /// timer could not deliver them all.
    pub fn rate(&self) -> f64 {
        if self.cpu_seconds > 0.0 {
            self.samples as f64 / self.cpu_seconds
        } else {
            0.0
        }
    }

    /// As folded lines, one a stack: the thread, then the frames from the
    /// root, separated by `;`, a space and the samples.
    pub fn folded(&self) -> String {
        let mut out = String::new();
        for s in &self.stacks {
            out.push_str(&self.threads[s.thread as usize].name.replace(';', ":"));
            for &f in &s.frames {
                out.push(';');
                out.push_str(&self.names[f as usize].replace(';', ":"));
            }
            out.push(' ');
            out.push_str(&s.samples.to_string());
            out.push('\n');
        }
        out
    }
}

/// Why a profile was not taken.
#[derive(Debug)]
pub enum Refused {
    /// Another is being taken.
    Busy,
    /// The system cannot sample (not Linux or macOS), or the timer failed.
    Unavailable(String),
}

static TAKING: AtomicBool = AtomicBool::new(false);

/// Lets the next profile be taken once this one ends, however it ends.
struct Taking;

impl Drop for Taking {
    fn drop(&mut self) {
        TAKING.store(false, Ordering::SeqCst);
    }
}

/// Samples every thread at `hz` while `during` runs, for `seconds` at
/// most: `during` is cut off then (a profile of the whole server passes a
/// future that never ends; one of a query, its runs). Answers the profile
/// and what `during` gave, if it ended.
pub async fn take<T>(hz: i32, seconds: f64, during: impl Future<Output = T>) -> Result<(CpuProfile, Option<T>), Refused> {
    if TAKING.swap(true, Ordering::SeqCst) {
        return Err(Refused::Busy);
    }
    let taking = Taking;
    let hz = hz.clamp(1, MAX_HZ);
    let seconds = seconds.clamp(0.01, MAX_SECONDS);
    let r = sample(hz, seconds, during).await;
    drop(taking);
    r
}

#[cfg(unix)]
async fn sample<T>(hz: i32, seconds: f64, during: impl Future<Output = T>) -> Result<(CpuProfile, Option<T>), Refused> {
    let blocked = blocklist();
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(hz)
        .blocklist(blocked)
        .build()
        .map_err(|e| Refused::Unavailable(e.to_string()))?;
    let (t0, cpu0) = (Instant::now(), crate::telemetry::process_usage().1);
    let out = tokio::time::timeout(Duration::from_secs_f64(seconds), during).await.ok();
    let (took, cpu) = (t0.elapsed().as_secs_f64(), crate::telemetry::process_usage().1 - cpu0);
    let raw = guard.report().build_unresolved().map_err(|e| Refused::Unavailable(e.to_string()))?;
    // The timer stops before the names are read.
    drop(guard);
    let stacks: Vec<(pprof::Frames, u64)> = tokio::task::spawn_blocking(move || {
        let named = raw.data.into_iter().map(|(f, n)| (pprof::Frames::from(f), n.max(0) as u64)).collect();
        release();
        named
    })
    .await
    .map_err(|e| Refused::Unavailable(e.to_string()))?;
    let mut p = profile(hz, took, stacks.iter().map(|(f, n)| (f.thread_name.as_str(), f.thread_id, f.frames.iter().rev().flat_map(|syms| syms.iter().rev()).map(|s| s.name()), *n)));
    p.seconds = (took * 1000.0).round() / 1000.0;
    p.cpu_seconds = (cpu * 1e4).round() / 1e4;
    p.blocked = blocked.iter().map(|s| s.to_string()).collect();
    Ok((p, out))
}

/// Gives back what a profile leaves behind once its frames are named: the
/// debug information read to name them, which `backtrace` would otherwise
/// keep parsed (about 30 MB of heap for this binary), and the memory the C
/// library holds freed. The CPU sampler's table of stacks (`pprof`'s,
/// 16,384 places of 128 frames, about 70 MB) is made the first time a
/// profile is taken and kept by the process; each profile that ends makes
/// a new one and frees the old, which glibc would otherwise keep,
/// resident, for later allocations.
pub(crate) fn release() {
    backtrace::clear_symbol_cache();
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim takes the allocator's own locks, and is called
    // outside any of its functions.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// The libraries a sample is not taken in. A sample unwinds the stack of
/// the thread it interrupts from a signal handler, through the unwinder
/// in libgcc, which looks up each frame's unwinding tables. Before glibc
/// 2.35 that lookup takes the loader's lock (`dl_iterate_phdr`), so a
/// sample that interrupts a thread holding it, inside the C library,
/// would wait on it forever: the C library is left out, and with it the
/// time in malloc, memcpy and system calls (40% of the samples of a query
/// run in a loop on the demo data). glibc 2.35 and up look frames up without a lock
/// (`_dl_find_object`, which libgcc 12 and up use), so there only the
/// unwinder itself, the loader and the vDSO are left out.
#[cfg(unix)]
fn blocklist() -> &'static [&'static str] {
    if lock_free_lookup() {
        &["libgcc", "ld-linux", "vdso"]
    } else {
        &["libc", "libgcc", "pthread", "vdso"]
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn lock_free_lookup() -> bool {
    // SAFETY: dlsym only looks a symbol up, by a NUL-terminated name.
    unsafe { !libc::dlsym(libc::RTLD_DEFAULT, c"_dl_find_object".as_ptr()).is_null() }
}

#[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
fn lock_free_lookup() -> bool {
    false
}

#[cfg(not(unix))]
async fn sample<T>(_hz: i32, _seconds: f64, _during: impl Future<Output = T>) -> Result<(CpuProfile, Option<T>), Refused> {
    Err(Refused::Unavailable("CPU profiles are taken on Linux and macOS".into()))
}

/// A profile of counted stacks: (thread's name, thread's id, frames from
/// the root, samples), its CPU time taken as `samples / hz`.
pub fn profile<'a, F>(hz: i32, seconds: f64, stacks: impl Iterator<Item = (&'a str, u64, F, u64)>) -> CpuProfile
where
    F: Iterator<Item = String>,
{
    let mut names: Vec<String> = Vec::new();
    let mut name_at: HashMap<String, u32> = HashMap::new();
    let mut threads: Vec<(String, std::collections::HashSet<u64>, u64)> = Vec::new();
    let mut thread_at: HashMap<String, u32> = HashMap::new();
    let mut by: HashMap<(u32, Vec<u32>), u64> = HashMap::new();
    let mut samples = 0;
    for (thread, id, frames, n) in stacks {
        samples += n;
        let t = pool_name(thread, id);
        let ti = *thread_at.entry(t.clone()).or_insert_with(|| {
            threads.push((t, Default::default(), 0));
            threads.len() as u32 - 1
        });
        threads[ti as usize].1.insert(id);
        threads[ti as usize].2 += n;
        let fs: Vec<u32> = frames
            .map(|f| {
                let f = clean(&f);
                *name_at.entry(f.clone()).or_insert_with(|| {
                    names.push(f);
                    names.len() as u32 - 1
                })
            })
            .collect();
        *by.entry((ti, fs)).or_insert(0) += n;
    }
    // The threads by samples, the stacks renumbered to match.
    let mut order: Vec<u32> = (0..threads.len() as u32).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(threads[i as usize].2));
    let mut place = vec![0u32; threads.len()];
    for (to, &from) in order.iter().enumerate() {
        place[from as usize] = to as u32;
    }
    let mut stacks: Vec<Stack> = by.into_iter().map(|((t, frames), samples)| Stack { thread: place[t as usize], frames, samples }).collect();
    stacks.sort_by(|a, b| b.samples.cmp(&a.samples).then_with(|| a.frames.cmp(&b.frames)));
    let dropped = stacks.iter().skip(MAX_STACKS).map(|s| s.samples).sum();
    stacks.truncate(MAX_STACKS);
    CpuProfile {
        hz,
        seconds,
        cpu_seconds: samples as f64 / hz.max(1) as f64,
        samples,
        names,
        threads: order.iter().map(|&i| &threads[i as usize]).map(|(name, ids, samples)| Threads { name: name.clone(), count: ids.len(), samples: *samples }).collect(),
        stacks,
        dropped,
        blocked: Vec::new(),
    }
}

/// A thread's name without the number that tells a pool's threads apart
/// (`strata-query-3` is `strata-query`); a thread with no name is named by
/// its id.
pub(crate) fn pool_name(name: &str, id: u64) -> String {
    if name.is_empty() {
        return format!("thread {id}");
    }
    let t = name.trim_end_matches(|c: char| c.is_ascii_digit());
    let t = t.trim_end_matches(['-', '_', ' ', '#']);
    if t.is_empty() {
        name.to_string()
    } else {
        t.to_string()
    }
}

/// A frame's name without the hash a Rust symbol ends with
/// (`strata::db::Db::execute::h0123456789abcdef`).
pub(crate) fn clean(name: &str) -> String {
    match name.rfind("::h") {
        Some(i) if name.len() - i == 19 && name[i + 3..].bytes().all(|b| b.is_ascii_hexdigit()) => name[..i].to_string(),
        _ => name.to_string(),
    }
}

/// A thread of the process, as the system sees it.
#[derive(Clone, Debug, Serialize)]
pub struct ThreadInfo {
    pub tid: u64,
    pub name: String,
    /// R running, S sleeping, D waiting on the disk, ...
    pub state: String,
    /// CPU time, user and system, in seconds.
    pub cpu_s: f64,
    /// Times it gave the CPU up (waiting on a lock, a read, a timer) and
    /// times it was made to.
    pub voluntary: u64,
    pub involuntary: u64,
}

/// The process's threads, from `/proc/self/task` (Linux; empty elsewhere).
pub fn threads() -> Vec<ThreadInfo> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: sysconf has no preconditions.
        let tick = (unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).max(1) as f64;
        let Ok(dir) = std::fs::read_dir("/proc/self/task") else { return Vec::new() };
        let mut out: Vec<ThreadInfo> = dir.filter_map(|e| thread_info(&e.ok()?.path(), tick)).collect();
        out.sort_by_key(|t| t.tid);
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
fn thread_info(dir: &std::path::Path, tick: f64) -> Option<ThreadInfo> {
    let tid = dir.file_name()?.to_str()?.parse().ok()?;
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    // pid (name) state ...: the name may hold spaces and parentheses.
    let (head, rest) = stat.rsplit_once(')')?;
    let name = head.split_once('(')?.1.to_string();
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state is field 3, utime 14 and stime 15.
    let state = f.first()?.to_string();
    let ticks = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    let status = std::fs::read_to_string(dir.join("status")).unwrap_or_default();
    let field = |k: &str| status.lines().find_map(|l| l.strip_prefix(k)?.trim().parse::<u64>().ok()).unwrap_or(0);
    Some(ThreadInfo { tid, name, state, cpu_s: ticks / tick, voluntary: field("voluntary_ctxt_switches:"), involuntary: field("nonvoluntary_ctxt_switches:") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacks_are_counted_by_thread_and_frames() {
        let st = |t: &'static str, id: u64, f: &[&str], n: u64| (t, id, f.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter(), n);
        let p = profile(
            100,
            2.0,
            vec![
                st("strata-query-1", 7, &["main", "run::h0123456789abcdef", "bm25"], 3),
                st("strata-query-2", 8, &["main", "run", "bm25"], 2),
                st("tokio-runtime-worker", 9, &["main", "poll"], 1),
                st("", 10, &["main"], 1),
            ]
            .into_iter(),
        );
        assert_eq!(p.samples, 7);
        assert_eq!(p.threads[0].name, "strata-query");
        assert_eq!((p.threads[0].count, p.threads[0].samples), (2, 5));
        assert_eq!(p.stacks[0].samples, 5, "both query threads' stacks are one: {p:?}");
        assert_eq!(p.stacks[0].frames.iter().map(|&f| p.names[f as usize].as_str()).collect::<Vec<_>>(), ["main", "run", "bm25"]);
        assert!(p.threads.iter().any(|t| t.name == "thread 10"));
        assert!((p.cores() - 0.035).abs() < 1e-9);
        assert!((p.rate() - 100.0).abs() < 1e-9);
        let folded = p.folded();
        assert!(folded.lines().any(|l| l == "strata-query;main;run;bm25 5"), "{folded}");
        assert_eq!(folded.lines().count(), 3);
    }

    #[test]
    fn the_threads_are_listed_with_their_cpu_time() {
        let ts = threads();
        if cfg!(target_os = "linux") {
            // This thread, reading them, is running.
            assert!(ts.iter().any(|t| t.state == "R" && !t.name.is_empty()), "{ts:?}");
            assert!(ts.iter().all(|t| t.cpu_s >= 0.0));
        } else {
            assert!(ts.is_empty());
        }
    }

    #[test]
    fn names_lose_their_hash_and_pools_their_numbers() {
        assert_eq!(clean("strata::db::Db::execute::h0123456789abcdef"), "strata::db::Db::execute");
        assert_eq!(clean("a::hello"), "a::hello");
        assert_eq!(pool_name("strata-query-12", 1), "strata-query");
        assert_eq!(pool_name("main", 1), "main");
        assert_eq!(pool_name("42", 1), "42");
    }
}
