//! A shard's part in moving slots to another shard ([`crate::slots::Move`]).
//! The coordinator moving them copies their rows from the shard they
//! leave ([`Service::export_slots`]) to the one they go to (writes marked
//! moved), then what changed in them since, again and again until little
//! did; then it holds the writes to them here ([`Service::freeze_slots`]),
//! copies the last changes, and gives the slots to the other shard in a
//! new cluster map. The writes held then find the map changed and are
//! refused, for their coordinators to send them to the slots' new shard.
//! Each shard hides the rows of moving slots it holds without serving them
//! ([`Service::hide_residue`]), from reads and from UPDATE and DELETE
//! statements ([`Service::statement_guard`]), until it deletes them
//! ([`Service::cleanup_slots`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::storage::{Mask, Snapshot, SOURCES};

use super::{conflict, Service};
use crate::slots::{self, Move, MoveState, SlotMap};
use crate::{json as js, pb, ApiError};

/// How long a hold lasts unless the coordinator asks for it again.
pub const FREEZE_TTL: Duration = Duration::from_secs(30);

/// How long a hold waits for the writes under way to commit.
const FREEZE_WAIT: Duration = Duration::from_secs(30);

#[derive(Default)]
pub(crate) struct Moving {
    /// The moving slots whose rows this shard holds without serving them,
    /// and the mask that hides those rows from reads and from UPDATE and
    /// DELETE statements.
    residue: RwLock<Option<(Vec<usize>, Arc<Mask>)>>,
    /// Passed (read) by each write to slots moving from this shard until
    /// it commits; a hold takes it (write).
    gate: Arc<tokio::sync::RwLock<()>>,
    /// The hold [`Service::freeze_slots`] took, and the number of the
    /// request that last asked for it: the one its time is counted from.
    frozen: Arc<tokio::sync::Mutex<Option<(u64, tokio::sync::OwnedRwLockWriteGuard<()>)>>>,
    holds: AtomicU64,
    /// The last export of slots moving from here: the move, the mask of
    /// its slots, and the version exported, which the next export of
    /// changes starts from.
    exported: Arc<Mutex<Option<(Move, Arc<Mask>, Snapshot)>>>,
    /// The writes that passed no gate, from before they look at the map
    /// until they commit, by number ([`Self::enter`]); a hold waits for
    /// those that came before it.
    entered: Arc<Entered>,
}

#[derive(Default)]
struct Entered {
    /// The next number, and the writes in.
    state: Mutex<(u64, std::collections::BTreeSet<u64>)>,
    left: tokio::sync::Notify,
}

/// A write in [`Moving::entered`], until dropped.
pub(crate) struct Entry {
    entered: Arc<Entered>,
    n: u64,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.entered.state.lock().unwrap_or_else(|e| e.into_inner()).1.remove(&self.n);
        self.entered.left.notify_waiters();
    }
}

/// What a write holds until it commits, for a hold on writes to moving
/// slots to wait for it: the move's gate, or its entry.
pub(crate) enum Pass {
    Gate(#[allow(dead_code)] tokio::sync::OwnedRwLockReadGuard<()>),
    Entry(#[allow(dead_code)] Entry),
}

impl Moving {
    /// Counts a write in, before it looks at the slot map: a hold that
    /// starts after waits for it to commit, as it may have seen the map
    /// from before slots started moving and passed no gate.
    pub(super) fn enter(&self) -> Entry {
        let mut st = self.entered.state.lock().unwrap_or_else(|e| e.into_inner());
        let n = st.0;
        st.0 += 1;
        st.1.insert(n);
        Entry { entered: self.entered.clone(), n }
    }

    /// Waits until every write counted in before now has left, until
    /// `deadline`: whether they did.
    async fn drained(&self, deadline: tokio::time::Instant) -> bool {
        let upto = self.entered.state.lock().unwrap_or_else(|e| e.into_inner()).0;
        loop {
            let left = self.entered.left.notified();
            tokio::pin!(left);
            left.as_mut().enable();
            if self.entered.state.lock().unwrap_or_else(|e| e.into_inner()).1.first().is_none_or(|&n| n >= upto) {
                return true;
            }
            if tokio::time::timeout_at(deadline, left).await.is_err() {
                return false;
            }
        }
    }

    /// The mask of the rows this shard hides, if any.
    pub fn mask(&self) -> Option<Arc<Mask>> {
        self.residue.read().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(_, m)| m.clone())
    }
}

/// A mask of the rows placed in `slots`: by a table's shard_by value, or
/// else its key. The streams' offsets (`_sources`) are each shard's own.
fn mask_of(slots: &[usize]) -> Arc<Mask> {
    let mut set = vec![false; slots::SLOTS];
    for &s in slots {
        set[s] = true;
    }
    Arc::new(Mask::new(move |schema, v| schema.table != SOURCES && set[slots::slot(v)]))
}

/// A mask of the rows placed outside `slots`.
pub(super) fn mask_outside(slots: &[usize]) -> Arc<Mask> {
    let mut set = vec![true; slots::SLOTS];
    for &s in slots {
        set[s] = false;
    }
    Arc::new(Mask::new(move |schema, v| schema.table != SOURCES && set[slots::slot(v)]))
}

impl Service {
    /// Hides the rows of moving slots this shard holds without serving
    /// them under `map` ([`SlotMap::residue`]), from reads and from UPDATE
    /// and DELETE statements, and shows them again once the move leaves
    /// them none. Called as the map is set, under its lock.
    pub(super) fn hide_residue(&self, map: &SlotMap, shard: usize) {
        let want = map.residue(shard).map(<[usize]>::to_vec);
        let mut held = self.moving.residue.write().unwrap_or_else(|e| e.into_inner());
        let exporting = map.moving().is_some_and(|m| m.from == shard && m.state == MoveState::Copying);
        if !exporting {
            self.moving.exported.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        if held.as_ref().map(|(s, _)| s) == want.as_ref() {
            return;
        }
        *held = want.map(|s| {
            let m = mask_of(&s);
            (s, m)
        });
        tracing::info!(epoch = map.epoch, hidden = held.as_ref().map_or(0, |(s, _)| s.len()), "slots of rows held here but not served");
    }

    /// The guard of an UPDATE or DELETE here routed by the cluster map of
    /// epoch `routed` ([`strata::db::PreparedWrite::guarded`]): as it
    /// starts, under the writer lock, the map this shard holds must route
    /// to it the slots it served under that one, and it passes over the
    /// rows this shard holds without serving them. Else it is refused, as
    /// `refused` keeps (409), for the coordinator to send it again by the
    /// newer map: one checked before a move changed the map, and run
    /// after, would change rows the coordinator counts elsewhere.
    pub(super) fn statement_guard(self: &Arc<Self>, routed: u64, refused: Arc<Mutex<Option<ApiError>>>) -> impl Fn() -> strata::Result<Option<Arc<Mask>>> + Send + Sync + 'static {
        let svc = self.clone();
        move || {
            let held = svc.slots.read().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = svc.epoch_ok(held.as_ref().map_or(0, |(m, _)| m.epoch), routed) {
                let message = e.message.clone();
                *refused.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                // Not Busy: the writer would try it again.
                return Err(strata::Error::Query(message));
            }
            Ok(svc.moving.mask())
        }
    }

    /// Waits, while slots move from this shard, for a hold on writes to
    /// them to end ([`Self::freeze_slots`]): a write placed in one of them
    /// (`places`), or an UPDATE or DELETE `statement`, which may change
    /// rows of them. The gate stays passed until the write commits, so
    /// that a hold waits for it. A write that waited while the cluster map
    /// changed is refused (409), for its coordinator to route it by the new
    /// map.
    ///
    /// Any other write holds its `entry`, counted in before it looked at
    /// the map ([`Moving::enter`]), until it commits: a hold waits for
    /// those too, which may have looked before the slots started moving.
    pub(super) async fn pass_gate(&self, entry: Entry, places: &[(&str, strata::value::Value)], statement: bool) -> Result<Pass, ApiError> {
        let Some((map, shard)) = self.slots() else { return Ok(Pass::Entry(entry)) };
        let Some(m) = map.moving().filter(|m| m.from == shard && m.state == MoveState::Copying) else { return Ok(Pass::Entry(entry)) };
        if !statement && !places.iter().any(|(_, v)| m.has(slots::slot(v))) {
            return Ok(Pass::Entry(entry));
        }
        // The gate covers it from here: a hold under way waits for the
        // writes entered before it, not this one, which waits for the hold.
        drop(entry);
        let g = self.moving.gate.clone().read_owned().await;
        let now = self.slot_epoch();
        if now != map.epoch {
            return Err(conflict(format!(
                "slot map epoch {} is older than this shard's ({now}): it changed while this write waited for slots moving from this shard; the coordinator must load the newer map",
                map.epoch
            )));
        }
        Ok(Pass::Gate(g))
    }

    /// The move from this shard (copying) in its cluster map, which only
    /// its primary (or a server of its own) exports and holds writes for.
    fn moving_from_here(&self) -> Result<(Arc<Db>, Move), ApiError> {
        let db = self.local().cloned().ok_or_else(|| ApiError::bad("a coordinator holds no slots"))?;
        self.takes_writes()?;
        let (map, shard) = self.slots().ok_or_else(|| ApiError::bad("this server holds no cluster map"))?;
        match map.moving() {
            Some(m) if m.from == shard && m.state == MoveState::Copying => Ok((db, m.clone())),
            _ => Err(conflict(format!("no slots are moving from this shard (shard {shard}) under its cluster map, epoch {}", map.epoch))),
        }
    }

    /// Holds writes to the slots moving from this shard, and UPDATE and
    /// DELETE statements, for `ttl` from now (a hold asked for again lasts
    /// `ttl` from then), or ends the hold (`thaw`). Answers this shard's
    /// last transaction once the writes under way have committed.
    pub async fn freeze_slots(&self, ttl: Duration, thaw: bool) -> Result<u64, ApiError> {
        let db = self.local().cloned().ok_or_else(|| ApiError::bad("a coordinator holds no slots"))?;
        if thaw {
            self.moving.frozen.lock().await.take();
        } else {
            self.moving_from_here()?;
            let id = self.moving.holds.fetch_add(1, Ordering::Relaxed) + 1;
            let mut held = self.moving.frozen.lock().await;
            match held.as_mut() {
                Some((n, _)) => *n = id,
                None => {
                    let late = || ApiError { status: 503, message: format!("the writes under way to the moving slots did not commit within {} s", FREEZE_WAIT.as_secs()) };
                    let deadline = tokio::time::Instant::now() + FREEZE_WAIT;
                    let g = tokio::time::timeout_at(deadline, self.moving.gate.clone().write_owned()).await.map_err(|_| late())?;
                    // And those that looked at the map before the slots
                    // started moving, passing no gate.
                    if !self.moving.drained(deadline).await {
                        return Err(late());
                    }
                    *held = Some((id, g));
                }
            }
            drop(held);
            let frozen = self.moving.frozen.clone();
            tokio::spawn(async move {
                tokio::time::sleep(ttl).await;
                let mut held = frozen.lock().await;
                if held.as_ref().is_some_and(|(n, _)| *n == id) {
                    tracing::warn!(ttl_s = ttl.as_secs_f64(), "a hold on writes to moving slots ended unasked: its coordinator did not ask again");
                    *held = None;
                }
            });
        }
        Ok(db.storage().seq_no().map_or(0, |s| s.seq))
    }

    /// Whether writes to the moving slots are held here.
    pub async fn frozen(&self) -> bool {
        self.moving.frozen.lock().await.is_some()
    }

    /// Exports the rows of the slots moving from this shard, as write
    /// operations in chunks of `batch` rows: every row of them at the
    /// latest version, or with `changes`, what changed since the export
    /// before (rows written since, and deletes of those gone). A shard that
    /// cannot tell what changed (it does not number its writes) holds the
    /// writes to the moving slots first, and says so (`frozen`). Every
    /// table needs a primary key, by which rows are replaced.
    pub async fn export_slots(&self, changes: bool, batch: usize) -> Result<tokio::sync::mpsc::Receiver<Result<pb::ExportSlotsChunk, ApiError>>, ApiError> {
        let (db, m) = self.moving_from_here()?;
        if let Some(t) = db.catalog().iter().find(|t| t.primary_key.is_none()) {
            return Err(ApiError::bad(format!("table {} has no primary key: rows are moved between shards by their keys", t.table)));
        }
        let numbered = db.storage().numbering().is_some();
        let prev = match changes {
            false => None,
            true => match &*self.moving.exported.lock().unwrap_or_else(|e| e.into_inner()) {
                Some((pm, mask, snap)) if *pm == m && numbered => Some((mask.clone(), snap.clone())),
                _ => return Err(conflict("this shard has no export of the moving slots to follow (it restarted, or its primary changed): export every row again".into())),
            },
        };
        let mut frozen = false;
        if prev.is_none() && !numbered {
            self.freeze_slots(FREEZE_TTL, false).await?;
            frozen = true;
        }
        let mask = prev.as_ref().map(|(m, _)| m.clone()).unwrap_or_else(|| mask_of(&m.slots));
        let snap = db.snapshot();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let batch = batch.clamp(1, 100_000);
        let exported = self.moving.exported.clone();
        tokio::task::spawn_blocking(move || {
            let mut sent = 0u64;
            let r = export_rows(&db, &snap, prev.as_ref().map(|(_, s)| s), &mask, batch, &mut |ops: Vec<Json>| {
                sent += ops.len() as u64;
                let chunk = pb::ExportSlotsChunk { ops_json: serde_json::to_vec(&ops).unwrap_or_default(), rows: ops.len() as u64, ..Default::default() };
                tx.blocking_send(Ok(chunk)).is_ok()
            });
            let last = match r {
                Ok(()) => {
                    *exported.lock().unwrap_or_else(|e| e.into_inner()) = Some((m, mask, snap.clone()));
                    Ok(pb::ExportSlotsChunk { last: true, seq_no: snap.seq_no().unwrap_or(0), frozen, rows: sent, ..Default::default() })
                }
                Err(e) => Err(e),
            };
            let _ = tx.blocking_send(last);
        });
        Ok(rx)
    }

    /// Deletes this shard's rows of moving slots that it holds without
    /// serving them (those it hides): what a move left on the shard the
    /// slots moved from, or what a move given up sent to the shard they
    /// were to move to. Returns the rows deleted.
    pub async fn cleanup_slots(self: &Arc<Self>) -> Result<u64, ApiError> {
        let db = self.local().cloned().ok_or_else(|| ApiError::bad("a coordinator holds no slots"))?;
        self.takes_writes()?;
        let Some(mask) = self.moving.mask() else { return Ok(0) };
        let mut deleted = 0;
        loop {
            let (d, m) = (db.clone(), mask.clone());
            let ops = tokio::task::spawn_blocking(move || residue_deletes(&d, &m, 1000)).await.map_err(ApiError::internal)??;
            if ops.is_empty() {
                return Ok(deleted);
            }
            let n = self.write_ops_as(ops, None, true).await?.rows;
            if n == 0 {
                return Err(ApiError::internal("deleting the rows of moving slots held here deleted none"));
            }
            deleted += n;
        }
    }
}

/// Gives `send` the rows `mask` covers in `snap`, of every table, as
/// upserts in chunks of `batch`; with `prev`, an earlier version, only
/// those changed since it, and deletes of those gone. Stops when `send`
/// says the receiver left.
fn export_rows(db: &Db, snap: &Snapshot, prev: Option<&Snapshot>, mask: &Mask, batch: usize, send: &mut dyn FnMut(Vec<Json>) -> bool) -> Result<(), ApiError> {
    let catalog = db.catalog();
    let first = catalog.first().map(|t| t.table.clone()).unwrap_or_default();
    let after = prev.map(|p| p.seq_no().unwrap_or(0));
    let mut ops = Vec::with_capacity(batch);
    let mut flush = |ops: &mut Vec<Json>, all: bool| -> Result<(), ApiError> {
        if (ops.len() >= batch || all) && !ops.is_empty() && !send(std::mem::take(ops)) {
            return Err(ApiError::internal("the coordinator stopped reading the export"));
        }
        Ok(())
    };
    for schema in &catalog {
        let Some(t) = snap.table(&schema.table) else { continue };
        let named = |mut op: Json| {
            if schema.table != first {
                op["table"] = json!(schema.table);
            }
            op
        };
        let rows = t.covered(mask, after)?;
        for chunk in rows.chunks(batch) {
            for d in db.documents_of(&t, chunk)? {
                ops.push(named(json!({"op": "upsert", "doc": js::document_json(schema, &d)})));
                flush(&mut ops, false)?;
            }
        }
        if let Some(pt) = prev.and_then(|p| p.table(&schema.table)) {
            for (key, place) in t.covered_gone(&pt, mask)? {
                let mut op = json!({"op": "delete", "key": js::to_json(&key)});
                if schema.shard_by.is_some() {
                    op["routing"] = js::to_json(&place);
                }
                ops.push(named(op));
                flush(&mut ops, false)?;
            }
        }
    }
    flush(&mut ops, true)
}

/// Deletes of up to `n` rows `mask` covers at the latest version, of any
/// table: by key, with the shard_by value of a table placed by one.
fn residue_deletes(db: &Db, mask: &Mask, n: usize) -> Result<Vec<Json>, ApiError> {
    let snap = db.snapshot();
    let catalog = db.catalog();
    let first = catalog.first().map(|t| t.table.clone()).unwrap_or_default();
    let mut ops = Vec::new();
    for schema in &catalog {
        let (Some(t), Some(_)) = (snap.table(&schema.table), schema.primary_key) else { continue };
        let rows = t.covered(mask, None)?;
        let rows = &rows[..rows.len().min(n - ops.len())];
        let places = match schema.shard_by_field() {
            Some(f) => db.documents_of(&t, rows)?.into_iter().map(|d| Some(d[f].clone())).collect(),
            None => vec![None; rows.len()],
        };
        for (&(seg, d), place) in rows.iter().zip(places) {
            let key = t.key(seg, d).ok_or_else(|| ApiError::internal(format!("table {}: a row without its key", schema.table)))?;
            let mut op = json!({"op": "delete", "key": js::to_json(&key)});
            if let Some(p) = place {
                op["routing"] = js::to_json(&p);
            }
            if schema.table != first {
                op["table"] = json!(schema.table);
            }
            ops.push(op);
        }
        if ops.len() >= n {
            break;
        }
    }
    Ok(ops)
}
