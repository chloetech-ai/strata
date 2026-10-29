//! Threads shared by every query in the process.
//!
//! A query that searches its segments on several threads borrows them from
//! one pool of as many threads as the machine has cores, instead of
//! starting its own: no thread starts per query, and queries running at
//! once never keep more threads busy than the pool and their callers. The
//! caller works too. It runs the work itself while the helpers it asked for
//! join in as pool threads come free, and once its own run returns, a
//! helper that has not started yet never will: a query on a busy pool runs
//! on fewer threads instead of waiting for one.

use std::any::Any;
use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

/// A fixed set of threads that help [`Pool::run`] callers.
pub struct Pool {
    shared: Arc<Shared>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

#[derive(Default)]
struct Queue {
    /// One entry per helper a caller asked for, oldest first.
    jobs: VecDeque<Arc<Job>>,
    stop: bool,
}

/// One call of [`Pool::run`].
struct Job {
    /// The caller's work with its type and lifetime erased, and the
    /// function that calls it. Called only by helpers that joined while
    /// the job was open (see [`Job::help`]).
    work: *const (),
    call: unsafe fn(*const ()),
    state: Mutex<State>,
    done: Condvar,
}

struct State {
    /// Whether helpers may still join.
    open: bool,
    /// Helpers running the work now.
    running: usize,
    /// The first panic of a helper's run, raised again in the caller.
    panic: Option<Box<dyn Any + Send>>,
}

// SAFETY: `work` points at an `F: Fn() + Sync` (see `Pool::run`), which
// any thread may call through a shared reference; the other fields are
// `Send` and `Sync` themselves.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

/// Calls the `F` that `work` points at.
///
/// # Safety
///
/// `work` must point at a live `F`.
unsafe fn call<F: Fn() + Sync>(work: *const ()) {
    // SAFETY: the caller's contract.
    unsafe { (*work.cast::<F>())() }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A run's panic is caught outside every lock, so a poisoned lock
    // still holds consistent state.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Pool {
    /// A pool of `threads` threads (at least one), named `strata-query-N`.
    pub fn new(threads: usize) -> Pool {
        let threads = threads.max(1);
        let shared = Arc::new(Shared { queue: Mutex::new(Queue::default()), ready: Condvar::new() });
        let handles = (0..threads)
            .map(|i| {
                let shared = shared.clone();
                std::thread::Builder::new().name(format!("strata-query-{i}")).spawn(move || shared.serve()).expect("start a query thread")
            })
            .collect();
        Pool { shared, handles }
    }

    /// The process's pool, as many threads as cores, started on first use.
    pub fn global() -> &'static Pool {
        static POOL: OnceLock<Pool> = OnceLock::new();
        POOL.get_or_init(|| Pool::new(std::thread::available_parallelism().map_or(1, |n| n.get())))
    }

    pub fn threads(&self) -> usize {
        self.handles.len()
    }

    /// Helpers asked for and not started yet, across every caller.
    pub fn queued(&self) -> usize {
        lock(&self.shared.queue).jobs.len()
    }

    /// Runs `work` on this thread and on up to `helpers` pool threads at
    /// once, and returns when every run has returned. Helpers join while
    /// this thread's own run lasts, as pool threads come free; so `work`
    /// should take its tasks from something shared (a counter of the next
    /// one) and return when none are left, leaving none to a helper that
    /// never comes. A panic in any run is raised here once all have ended.
    pub fn run<F: Fn() + Sync>(&self, helpers: usize, work: &F) {
        let helpers = helpers.min(self.threads());
        if helpers == 0 {
            return work();
        }
        let job = Arc::new(Job {
            work: (work as *const F).cast(),
            call: call::<F>,
            state: Mutex::new(State { open: true, running: 0, panic: None }),
            done: Condvar::new(),
        });
        {
            let mut q = lock(&self.shared.queue);
            q.jobs.extend(std::iter::repeat_n(job.clone(), helpers));
        }
        for _ in 0..helpers {
            self.shared.ready.notify_one();
        }
        // Closes the job and waits out its helpers even if `work` panics
        // here, so that none calls it after `run` returns.
        let close = Close { pool: self, job: &job };
        work();
        drop(close);
        let panicked = lock(&job.state).panic.take();
        if let Some(p) = panicked {
            panic::resume_unwind(p);
        }
    }
}

impl Drop for Pool {
    /// Stops the threads (no caller can be inside [`Pool::run`] by now).
    fn drop(&mut self) {
        lock(&self.shared.queue).stop = true;
        self.shared.ready.notify_all();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

struct Close<'a> {
    pool: &'a Pool,
    job: &'a Arc<Job>,
}

impl Drop for Close<'_> {
    fn drop(&mut self) {
        lock(&self.job.state).open = false;
        // Helpers not started yet leave the queue: no thread wakes for them.
        lock(&self.pool.shared.queue).jobs.retain(|j| !Arc::ptr_eq(j, self.job));
        let mut s = lock(&self.job.state);
        while s.running > 0 {
            s = self.job.done.wait(s).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Shared {
    /// A pool thread: helps each job in turn until the pool stops.
    fn serve(&self) {
        loop {
            let job = {
                let mut q = lock(&self.queue);
                loop {
                    if let Some(j) = q.jobs.pop_front() {
                        break j;
                    }
                    if q.stop {
                        return;
                    }
                    q = self.ready.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            job.help();
        }
    }
}

impl Job {
    /// Runs the work once if the job is still open.
    fn help(&self) {
        {
            let mut s = lock(&self.state);
            if !s.open {
                return;
            }
            s.running += 1;
        }
        // SAFETY: the job was open when this helper joined, so its caller
        // is inside `Pool::run`, and its `Close` waits for `running` to
        // fall back to zero before `run` returns: `work` still points at
        // the caller's live `F`, the type `call` was made for.
        let r = panic::catch_unwind(AssertUnwindSafe(|| unsafe { (self.call)(self.work) }));
        let mut s = lock(&self.state);
        if let Err(p) = r {
            s.panic.get_or_insert(p);
        }
        s.running -= 1;
        if s.running == 0 && !s.open {
            self.done.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::Duration;

    fn on_pool() -> bool {
        std::thread::current().name().is_some_and(|n| n.starts_with("strata-query-"))
    }

    /// Takes tasks from a counter until none are left, as callers should.
    fn tasks(n: usize, next: &AtomicUsize, done: &[AtomicUsize]) {
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= n {
                break;
            }
            done[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn every_task_runs_once() {
        let pool = Pool::new(3);
        for helpers in [0, 1, 3, 8] {
            let n = 50;
            let next = AtomicUsize::new(0);
            let done: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
            pool.run(helpers, &|| tasks(n, &next, &done));
            assert!(done.iter().all(|d| d.load(Ordering::Relaxed) == 1), "{helpers} helpers");
        }
        assert_eq!(pool.queued(), 0);
    }

    #[test]
    fn helpers_run_at_once_with_the_caller() {
        let pool = Pool::new(2);
        // Returns only once three threads are inside it together.
        let barrier = Barrier::new(3);
        let runs = AtomicUsize::new(0);
        pool.run(2, &|| {
            runs.fetch_add(1, Ordering::Relaxed);
            barrier.wait();
        });
        assert_eq!(runs.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_busy_pool_leaves_the_caller_to_work_alone() {
        let pool = Pool::new(1);
        let joined = AtomicBool::new(false);
        let (started, release) = (Barrier::new(2), Barrier::new(2));
        std::thread::scope(|s| {
            // Holds the pool's only thread until released.
            s.spawn(|| {
                pool.run(1, &|| {
                    if on_pool() {
                        joined.store(true, Ordering::Relaxed);
                        started.wait();
                        release.wait();
                    } else {
                        while !joined.load(Ordering::Relaxed) {
                            std::thread::yield_now();
                        }
                    }
                })
            });
            started.wait();
            // The caller does every task itself and returns without
            // waiting for a thread; its helper never runs.
            let next = AtomicUsize::new(0);
            let done: Vec<AtomicUsize> = (0..10).map(|_| AtomicUsize::new(0)).collect();
            let helped = AtomicBool::new(false);
            pool.run(1, &|| {
                helped.fetch_or(on_pool(), Ordering::Relaxed);
                tasks(10, &next, &done)
            });
            assert!(!helped.load(Ordering::Relaxed));
            assert!(done.iter().all(|d| d.load(Ordering::Relaxed) == 1));
            assert_eq!(pool.queued(), 0);
            release.wait();
        });
    }

    #[test]
    fn a_helpers_panic_reaches_the_caller_after_every_run_ends() {
        let pool = Pool::new(2);
        let barrier = Barrier::new(3);
        let (helpers, ended) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let r = panic::catch_unwind(AssertUnwindSafe(|| {
            pool.run(2, &|| {
                let first = on_pool() && helpers.fetch_add(1, Ordering::Relaxed) == 0;
                barrier.wait();
                if first {
                    panic!("helper");
                }
                std::thread::sleep(Duration::from_millis(20));
                ended.fetch_add(1, Ordering::Relaxed);
            })
        }));
        assert_eq!(*r.unwrap_err().downcast::<&str>().unwrap(), "helper");
        assert_eq!(ended.load(Ordering::Relaxed), 2);
        // The pool goes on serving.
        let next = AtomicUsize::new(0);
        let done: Vec<AtomicUsize> = (0..10).map(|_| AtomicUsize::new(0)).collect();
        pool.run(2, &|| tasks(10, &next, &done));
        assert!(done.iter().all(|d| d.load(Ordering::Relaxed) == 1));
    }

    #[test]
    fn a_callers_panic_waits_for_its_helper() {
        let pool = Pool::new(1);
        let barrier = Barrier::new(2);
        let ended = AtomicUsize::new(0);
        let r = panic::catch_unwind(AssertUnwindSafe(|| {
            pool.run(1, &|| {
                barrier.wait();
                if !on_pool() {
                    panic!("caller");
                }
                std::thread::sleep(Duration::from_millis(20));
                ended.fetch_add(1, Ordering::Relaxed);
            })
        }));
        assert_eq!(*r.unwrap_err().downcast::<&str>().unwrap(), "caller");
        // The helper had returned before the panic left `run`.
        assert_eq!(ended.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn many_callers_share_the_pool() {
        let pool = Pool::new(2);
        let rounds = if cfg!(miri) { 5 } else { 200 };
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..rounds {
                        let next = AtomicUsize::new(0);
                        let done: Vec<AtomicUsize> = (0..20).map(|_| AtomicUsize::new(0)).collect();
                        pool.run(2, &|| tasks(20, &next, &done));
                        assert!(done.iter().all(|d| d.load(Ordering::Relaxed) == 1));
                    }
                });
            }
        });
        assert_eq!(pool.queued(), 0);
    }
}
