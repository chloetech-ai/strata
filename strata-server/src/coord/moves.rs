//! Moving slots between shards while the cluster serves: rebalancing onto
//! a shard just added, draining one to remove it, or one move asked for.
//!
//! A move ([`crate::slots::Move`]) goes through the cluster map: started
//! (copying), the rows of its slots are copied from the shard that holds
//! them to the one they go to, then what changed in them since, round after
//! round until little does; the shard they leave then holds writes to them
//! (a fraction of a second), the last changes are copied, and the map gives
//! the slots to the other shard (cleaning), first to the shard they leave.
//! The writes held there find the map changed and come back here, refused,
//! to go to the slots' new shard. The shard they left deletes its copies,
//! and the move ends. A move that fails before its flip is given up: the
//! shard the slots were to go to deletes what it was sent.
//!
//! One move at a time in the cluster; a job (several moves) runs in the
//! background, and [`Coordinator::moves`] says how it does. What a move
//! does next is [`crate::cores::slots::MoveRun`]'s to decide, which the
//! simulator runs too; this module carries out its steps.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::cores::slots::{MoveRun, MoveStep, HOLD_RENEW, HOLD_TTL};
use crate::slots::SLOTS;

/// Slots a planned move takes at most: a sixteenth of the cluster, so that
/// a move's copy, and the hold at its end, stay short.
pub const MOVE_SLOTS: usize = 256;

/// Rows per chunk of an export.
const EXPORT_ROWS: u32 = 1_000;

/// The moves this coordinator runs, and the last job's account.
#[derive(Default)]
pub(crate) struct Moves {
    /// Held while a job runs.
    running: Arc<tokio::sync::Mutex<()>>,
    job: Mutex<Option<Job>>,
    /// Asked to stop after the move under way.
    stop: AtomicBool,
}

/// Moves (`slots` of shard `from` to shard `to`), each of at most `chunk`
/// slots, that leave every shard but `empty` holding an even share of the
/// slots and `empty` (a shard being drained) none, moving as few as can be.
/// Each takes the highest slots of the shard it moves them from.
pub fn plan_even(map: &SlotMap, empty: Option<usize>, chunk: usize) -> Vec<(Vec<usize>, usize, usize)> {
    let n = map.shards();
    let mut owner: Vec<usize> = (0..SLOTS).map(|s| map.owner(s)).collect();
    let mut count = vec![0usize; n];
    for &o in &owner {
        count[o] += 1;
    }
    // The shards holding most keep the remainder.
    let mut takers: Vec<usize> = (0..n).filter(|&i| Some(i) != empty).collect();
    if takers.is_empty() {
        return Vec::new();
    }
    takers.sort_by_key(|&i| (std::cmp::Reverse(count[i]), i));
    let mut target = vec![0usize; n];
    for (k, &i) in takers.iter().enumerate() {
        target[i] = SLOTS / takers.len() + usize::from(k < SLOTS % takers.len());
    }
    let mut plan = Vec::new();
    loop {
        let donor = (0..n).filter(|&i| count[i] > target[i]).max_by_key(|&i| (count[i] - target[i], std::cmp::Reverse(i)));
        let taker = (0..n).filter(|&i| count[i] < target[i]).max_by_key(|&i| (target[i] - count[i], std::cmp::Reverse(i)));
        let (Some(d), Some(r)) = (donor, taker) else { break };
        let k = (count[d] - target[d]).min(target[r] - count[r]).min(chunk.max(1));
        let mut slots: Vec<usize> = (0..SLOTS).rev().filter(|&s| owner[s] == d).take(k).collect();
        slots.reverse();
        for &s in &slots {
            owner[s] = r;
        }
        count[d] -= k;
        count[r] += k;
        plan.push((slots, d, r));
    }
    plan
}

/// One move of a job, and how it went.
#[derive(Clone)]
struct Step {
    from: usize,
    to: usize,
    slots: Vec<usize>,
    /// planned, copying, catching up, holding writes, flipping, cleaning,
    /// done, failed, given up.
    phase: &'static str,
    rows: u64,
    rounds: u32,
    /// How long writes to its slots were held.
    held_ms: Option<f64>,
    ms: Option<f64>,
    error: Option<String>,
}

struct Job {
    kind: String,
    steps: Vec<Step>,
    started: i64,
    finished: Option<i64>,
    state: &'static str,
    error: Option<String>,
}

impl Job {
    fn json(&self) -> Json {
        let steps: Vec<Json> = self
            .steps
            .iter()
            .map(|s| {
                json!({
                    "from": s.from, "to": s.to, "slots": slots::runs(s.slots.iter().copied()), "count": s.slots.len(),
                    "phase": s.phase, "rows": s.rows, "rounds": s.rounds, "held_ms": s.held_ms, "ms": s.ms, "error": s.error,
                })
            })
            .collect();
        let done = self.steps.iter().filter(|s| s.phase == "done").count();
        json!({
            "kind": self.kind, "state": self.state, "moves": steps, "done": done, "of": self.steps.len(),
            "slots_moved": self.steps.iter().filter(|s| s.phase == "done").map(|s| s.slots.len()).sum::<usize>(),
            "rows_moved": self.steps.iter().map(|s| s.rows).sum::<u64>(),
            "started_us": self.started, "finished_us": self.finished, "error": self.error,
        })
    }
}

/// Ends a hold's renewals when dropped.
struct Renewing(tokio::task::JoinHandle<()>);

impl Drop for Renewing {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Coordinator {
    /// The job of moves running or last run here, and the move under way
    /// in the cluster map (which another coordinator may run).
    pub fn moves(&self) -> Json {
        let job = lock(&self.moves.job).as_ref().map(Job::json);
        let moving = self.map().and_then(|m| m.to_json().get("moving").cloned());
        json!({"job": job, "moving": moving})
    }

    /// Moves `slots` (of one shard) to shard `to`, in the background.
    pub fn start_move(self: &Arc<Self>, slots: Vec<usize>, to: usize) -> Result<Json, ApiError> {
        let map = self.map().ok_or_else(|| ApiError::bad("a coordinator routing by the hash before slots cannot move slots"))?;
        // Checked now, so that the caller hears what is wrong.
        let next = map.start_move(&slots, to).map_err(ApiError::bad)?;
        let m = next.moving().cloned().ok_or_else(|| ApiError::internal("a move that does not move"))?;
        self.start_job("move", vec![(m.slots, m.from, m.to)])
    }

    /// Evens out the slots over the shards, `chunk` slots per move at most
    /// ([`plan_even`]), in the background: after adding a shard.
    pub fn rebalance(self: &Arc<Self>, chunk: usize) -> Result<Json, ApiError> {
        let map = self.map().ok_or_else(|| ApiError::bad("a coordinator routing by the hash before slots cannot move slots"))?;
        self.start_job("rebalance", plan_even(&map, None, chunk))
    }

    /// Moves every slot of shard `i` to the others, evenly, in the
    /// background: before removing it.
    pub fn drain(self: &Arc<Self>, i: usize, chunk: usize) -> Result<Json, ApiError> {
        let map = self.map().ok_or_else(|| ApiError::bad("a coordinator routing by the hash before slots cannot move slots"))?;
        if i >= map.shards() {
            return Err(ApiError::bad(format!("there is no shard {i} of {}", map.shards())));
        }
        if map.shards() == 1 {
            return Err(ApiError::bad("a cluster of one shard has nowhere to move its slots"));
        }
        self.start_job(&format!("drain shard {i}"), plan_even(&map, Some(i), chunk))
    }

    /// Asks the job running here to stop once its move under way ends.
    pub fn stop_moves(&self) -> Json {
        self.moves.stop.store(true, Ordering::Relaxed);
        self.moves()
    }

    fn start_job(self: &Arc<Self>, kind: &str, plan: Vec<(Vec<usize>, usize, usize)>) -> Result<Json, ApiError> {
        let Ok(running) = self.moves.running.clone().try_lock_owned() else {
            return Err(ApiError { status: 409, message: "a job of moves runs already: wait for it, or stop it (POST /v1/admin/moves/stop)".into() });
        };
        if let Some(m) = self.map().and_then(|m| m.moving().cloned()) {
            return Err(ApiError { status: 409, message: format!("slots are moving already, from shard {} to {} ({}): finish that move first (POST /v1/admin/moves/finish)", m.from, m.to, m.state.name()) });
        }
        let steps: Vec<Step> = plan.into_iter().map(|(slots, from, to)| Step { from, to, slots, phase: "planned", rows: 0, rounds: 0, held_ms: None, ms: None, error: None }).collect();
        let state = if steps.is_empty() { "done" } else { "running" };
        let now = strata::timestamp::now();
        *lock(&self.moves.job) = Some(Job { kind: kind.to_string(), steps, started: now, finished: (state == "done").then_some(now), state, error: None });
        self.moves.stop.store(false, Ordering::Relaxed);
        let me = self.clone();
        tokio::spawn(async move {
            let _running = running;
            me.run_job().await;
        });
        Ok(self.moves())
    }

    /// Runs the job's moves one after the other, stopping at the first that
    /// fails, or when asked.
    async fn run_job(&self) {
        let n = lock(&self.moves.job).as_ref().map_or(0, |j| j.steps.len());
        let mut outcome: (&'static str, Option<String>) = ("done", None);
        for k in 0..n {
            if self.moves.stop.load(Ordering::Relaxed) {
                outcome = ("stopped", None);
                break;
            }
            let Some(step) = lock(&self.moves.job).as_ref().map(|j| j.steps[k].clone()) else { return };
            let t0 = Instant::now();
            tracing::info!(from = step.from, to = step.to, slots = step.slots.len(), "moving slots");
            let r = self.run_move(&step.slots, step.to, k).await;
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let failed = r.as_ref().err().map(|e| e.message.clone());
            self.step(k, |s| {
                s.ms = Some(ms);
                match &r {
                    Ok(true) => s.phase = "done",
                    Ok(false) => s.phase = "given up",
                    Err(e) => {
                        s.phase = "failed";
                        s.error = Some(e.message.clone());
                    }
                }
            });
            match r {
                Ok(true) => tracing::info!(from = step.from, to = step.to, slots = step.slots.len(), ms, "moved slots"),
                _ => {
                    let why = failed.unwrap_or_else(|| "the move was given up".into());
                    tracing::warn!(from = step.from, to = step.to, error = %why, "a slot move failed");
                    outcome = ("failed", Some(why));
                    break;
                }
            }
        }
        if let Some(j) = lock(&self.moves.job).as_mut() {
            j.state = outcome.0;
            j.error = outcome.1;
            j.finished = Some(strata::timestamp::now());
        }
    }

    fn step(&self, k: usize, f: impl FnOnce(&mut Step)) {
        if let Some(s) = lock(&self.moves.job).as_mut().and_then(|j| j.steps.get_mut(k)) {
            f(s);
        }
    }

    /// Moves `slots` to shard `to` (step `k` of the job): whether they
    /// moved (false: given up, and the rows sent deleted).
    async fn run_move(&self, slots: &[usize], to: usize, k: usize) -> Result<bool, ApiError> {
        let (run, step) = MoveRun::new(slots.to_vec(), to);
        self.drive(run, step, Some(k)).await
    }

    /// Finishes the move in the cluster map that no job here runs (one a
    /// stopped coordinator left): one flipped (cleaning) ends once the
    /// shard the slots left deletes its copies; one copying, or being given
    /// up, is given up.
    pub async fn finish_move(&self) -> Result<Json, ApiError> {
        let Ok(_running) = self.moves.running.try_lock() else {
            return Err(ApiError { status: 409, message: "a job of moves runs here: it finishes its own moves (stop it with POST /v1/admin/moves/stop)".into() });
        };
        self.sync().await;
        let Some(now) = self.map() else { return Ok(json!({"finished": null})) };
        let Some(mv) = now.moving().cloned() else { return Ok(json!({"finished": null})) };
        let Some((run, step)) = MoveRun::finish(&now) else { return Ok(json!({"finished": null})) };
        let moved = self.drive(run, step, None).await?;
        Ok(json!({"finished": {"from": mv.from, "to": mv.to, "slots": mv.ranges(), "was": mv.state.name(), "moved": moved}}))
    }

    /// Carries out a move's steps from `step`, answering `run` with what
    /// each did, until it ends (step `k` of the job, if any, shows where it
    /// is): whether its slots moved.
    async fn drive(&self, mut run: MoveRun, mut step: MoveStep, k: Option<usize>) -> Result<bool, ApiError> {
        // The server of the shard the slots leave that their rows come
        // from, and holds writes to them: its primary.
        let mut src: Option<usize> = None;
        let mut renewing: Option<Renewing> = None;
        let mut held: Option<Instant> = None;
        let show = |run: &MoveRun| {
            if let Some(k) = k {
                let (phase, rounds) = (run.phase().name(), run.rounds());
                self.step(k, |s| {
                    s.phase = phase;
                    s.rounds = rounds;
                });
            }
        };
        loop {
            show(&run);
            let mv = run.moving().cloned();
            match (run.renewing(), renewing.is_some(), &mv) {
                (true, false, Some(mv)) => {
                    renewing = Some(self.renew_hold(mv.from, src.unwrap_or(0)));
                    held = held.or(Some(Instant::now()));
                }
                (false, true, _) => renewing = None,
                _ => {}
            }
            let started = || mv.clone().ok_or_else(|| ApiError::internal("a step of a move that has not started"));
            step = match step {
                MoveStep::Change { edit, first_from } => {
                    let first: Vec<String> = match (first_from, &mv, src) {
                        (true, Some(m), Some(s)) => self.topo().replicas.get(m.from).and_then(|rs| rs.get(s)).map(|r| r.url.clone()).into_iter().collect(),
                        _ => Vec::new(),
                    };
                    let r = self.change(|m| edit.apply(m), &first).await.map(|(m, _)| m);
                    run.changed(r, self.map().as_ref())
                }
                MoveStep::Source => {
                    let mv = started()?;
                    let r = self.primary(&self.topo(), mv.from).await.map(|s| src = Some(s.unwrap_or(0)));
                    run.found(r)
                }
                MoveStep::Cleanup { shard } => {
                    let t = self.topo();
                    run.cleaned(self.cleanup(&t, shard).await)
                }
                MoveStep::Export { changes } => {
                    let mv = started()?;
                    run.exported(self.export_round(&mv, src.unwrap_or(0), changes, k).await)
                }
                MoveStep::Hold => {
                    let mv = started()?;
                    run.held(self.hold(mv.from, src.unwrap_or(0), false).await)
                }
                MoveStep::Thaw => {
                    let mv = started()?;
                    renewing = None;
                    if let (Some(k), Some(h)) = (k, held.take()) {
                        let ms = h.elapsed().as_secs_f64() * 1e3;
                        self.step(k, |s| s.held_ms = Some(ms));
                    }
                    // A move another coordinator left: its source is the
                    // primary now.
                    let at = match src {
                        Some(s) => Some(s),
                        None => self.primary(&self.topo(), mv.from).await.ok().map(|s| s.unwrap_or(0)),
                    };
                    match at {
                        Some(s) => {
                            if let Err(e) = self.hold(mv.from, s, true).await {
                                tracing::warn!(error = %e.message, "could not end the hold on writes to the moving slots: it ends by itself");
                            }
                        }
                        None => tracing::warn!("could not find the server holding writes to the moving slots: its hold ends by itself"),
                    }
                    run.thawed()
                }
                MoveStep::Sync => {
                    tracing::warn!("a step of a slot move failed: giving it up, unless its flip was taken");
                    self.sync().await;
                    run.synced(self.map().as_ref())
                }
                MoveStep::Done(r) => return r,
            };
        }
    }

    /// One export of `mv`'s slots from server `src` of the shard they
    /// leave (every row, or what changed since the export before), written
    /// to the shard they move to: the rows sent, and whether the shard
    /// they leave holds their writes.
    async fn export_round(&self, mv: &Move, src: usize, changes: bool, k: Option<usize>) -> Result<(u64, bool), ApiError> {
        let t = self.topo();
        let rep = t.replicas.get(mv.from).and_then(|rs| rs.get(src)).ok_or_else(|| ApiError::internal("the shard slots move from changed places"))?;
        let mut c = rep.client.clone();
        let named = |s: tonic::Status| self.named(&rep.url, s);
        let mut stream = c.export_slots(pb::ExportSlotsRequest { epoch: t.epoch(), changes, batch_rows: EXPORT_ROWS }).await.map_err(named)?.into_inner();
        let mut rows = 0;
        loop {
            let chunk = tokio::time::timeout(Duration::from_secs(120), stream.message())
                .await
                .map_err(|_| ApiError { status: 504, message: format!("shard {}: no chunk of the export within 120 s", rep.url) })?
                .map_err(named)?
                .ok_or_else(|| ApiError::internal(format!("shard {}: the export ended without its last chunk", rep.url)))?;
            if chunk.last {
                return Ok((rows, chunk.frozen));
            }
            if chunk.ops_json.is_empty() {
                continue;
            }
            let t = self.topo();
            let r = pb::WriteRequest { ops_json: chunk.ops_json, epoch: t.epoch(), moved: true, ..Default::default() };
            self.write(&t, mv.to, r, CLUSTER_PROTOCOL).await?;
            rows += chunk.rows;
            if let Some(k) = k {
                self.step(k, |s| s.rows += chunk.rows);
            }
        }
    }

    /// Holds writes to the slots moving from shard `i` on its server `src`
    /// ([`HOLD_TTL`]), or ends the hold (`thaw`).
    async fn hold(&self, i: usize, src: usize, thaw: bool) -> Result<u64, ApiError> {
        let t = self.topo();
        let rep = &t.replicas[i][src];
        let mut c = rep.client.clone();
        let r = pb::FreezeRequest { epoch: t.epoch(), ttl_ms: HOLD_TTL.as_millis() as u32, thaw };
        let a = tokio::time::timeout(Duration::from_secs(60), c.freeze_slots(r)).await.map_err(|_| ApiError { status: 504, message: format!("shard {}: no answer to the hold within 60 s", rep.url) })?;
        Ok(a.map_err(|s| self.named(&rep.url, s))?.into_inner().seq_no)
    }

    /// Asks for the hold on shard `i`'s server `src` again every
    /// [`HOLD_RENEW`], until dropped.
    fn renew_hold(&self, i: usize, src: usize) -> Renewing {
        let t = self.topo();
        let (mut c, url) = (t.replicas[i][src].client.clone(), t.replicas[i][src].url.clone());
        Renewing(tokio::spawn(async move {
            loop {
                tokio::time::sleep(HOLD_RENEW).await;
                let r = pb::FreezeRequest { epoch: t.epoch(), ttl_ms: HOLD_TTL.as_millis() as u32, thaw: false };
                if let Err(e) = c.freeze_slots(r).await {
                    tracing::warn!(shard = %url, error = %e.message(), "could not renew the hold on writes to moving slots");
                }
            }
        }))
    }

    /// Has shard `i` delete the rows of moving slots it holds without
    /// serving them: at its primary, or at each server while none has a
    /// role. Returns the rows deleted.
    async fn cleanup(&self, t: &Topology, i: usize) -> Result<u64, ApiError> {
        let to: Vec<usize> = match self.primary(t, i).await? {
            Some(k) => vec![k],
            None => (0..t.replicas[i].len()).collect(),
        };
        let mut deleted = 0;
        for k in to {
            let rep = &t.replicas[i][k];
            let mut c = rep.client.clone();
            deleted += c.cleanup_slots(pb::CleanupRequest { epoch: t.epoch() }).await.map_err(|s| self.named(&rep.url, s))?.into_inner().deleted;
        }
        Ok(deleted)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_even_out_the_slots() {
        let one = SlotMap::even(1, 1).with_replicas(vec![vec!["a".into()]]).unwrap();
        let two = one.add_shard(vec!["b".into()]).unwrap();
        let plan = plan_even(&two, None, MOVE_SLOTS);
        assert_eq!(plan.len(), 8);
        assert!(plan.iter().all(|(s, from, to)| s.len() == 256 && *from == 0 && *to == 1));
        let mut m = two.clone();
        for (s, _, to) in &plan {
            m = m.move_slots(s, *to).unwrap();
        }
        assert_eq!((m.count(0), m.count(1)), (2048, 2048));
        assert_eq!(m.ranges(1), vec![(2048, 4096)]);
        assert!(plan_even(&m, None, MOVE_SLOTS).is_empty());
        // Three shards from two: each gives a third of its own.
        let three = m.add_shard(vec!["c".into()]).unwrap();
        let plan = plan_even(&three, None, 4096);
        let mut m3 = three.clone();
        for (s, _, to) in &plan {
            m3 = m3.move_slots(s, *to).unwrap();
        }
        let counts: Vec<usize> = (0..3).map(|i| m3.count(i)).collect();
        assert_eq!(counts.iter().sum::<usize>(), 4096);
        assert!(counts.iter().all(|&c| (1365..=1366).contains(&c)), "{counts:?}");
        assert_eq!(plan.iter().map(|(s, ..)| s.len()).sum::<usize>(), 1365);
        // Draining shard 0 of three gives its slots to the other two.
        let plan = plan_even(&m3, Some(0), MOVE_SLOTS);
        let mut d = m3.clone();
        for (s, from, to) in &plan {
            assert_eq!(*from, 0);
            d = d.move_slots(s, *to).unwrap();
        }
        assert_eq!((d.count(0), d.count(1), d.count(2)), (0, 2048, 2048));
    }
}
