//! The per-project daemon: owns the Strata store, keeps session state, and
//! answers hooks, MCP calls and CLI requests over a Unix socket (one JSON
//! request line, one JSON response line per connection).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};
use chloedb::query::Params;
use chloedb::Value;

use crate::anchor::{self, Checker};
use crate::capture::{self, EpisodeAcc};
use crate::paths::StorePaths;
use crate::recall::{self, Decision, Gate, PackParams, RankParams};
use crate::record::{Draft, Kind, Record};
use crate::store::{Filter, Store, StoreOptions};
use crate::{embed, err, now, Result};

// ---------------------------------------------------------------- config

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Embedder spec (see [`embed::open`]).
    pub embedder: String,
    /// Exit after this many idle seconds.
    pub idle_secs: u64,
    pub rank: RankParams,
    pub pack: PackParams,
    pub gate: Gate,
    pub briefing_chars: usize,
    pub knowledge: recall::KnowledgeParams,
    /// Notes about a file before Edit/Write.
    pub file_notes: bool,
    /// Before Edit/Write, also the files usually edited next, with a note
    /// on each ([`crate::next_files`]; an experiment, `CHLOECTX_NEXT_FILES=1`).
    pub next_files: bool,
    /// A command that fails with an error cleared before gets the steps
    /// that cleared it, and fixes are written down ([`crate::fixes`]).
    pub error_fixes: bool,
    pub sync: bool,
    /// Knowledge written down by a small model after each turn.
    pub distill: crate::distill::DistillConfig,
    /// Related notes merged now and then, so the knowledge stays small.
    pub consolidate: crate::consolidate::ConsolidateConfig,
    /// Query understanding: model-planned searches and a model ranker.
    pub deep: crate::deep::DeepConfig,
    /// A receipt for every recall, and snapshots to replay them on.
    pub receipts: ReceiptConfig,
}

/// Receipts ([`crate::receipt`]) and kept snapshots ([`crate::snapshots`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ReceiptConfig {
    /// Write receipts (and keep snapshots). Unless config.json sets it,
    /// `CHLOECTX_RECEIPTS=0` turns them off, read from the daemon's
    /// environment and from each hook's, so it holds whichever process
    /// started the daemon.
    pub enabled: bool,
    pub snapshots: crate::snapshots::Retention,
    /// How long receipts are kept.
    pub retention: crate::receipt::BookRetention,
    /// config.json sets `enabled` (then the environment does not).
    #[serde(skip)]
    pub explicit: bool,
}

impl Default for ReceiptConfig {
    fn default() -> Self {
        ReceiptConfig { enabled: receipts_env().unwrap_or(true), snapshots: Default::default(), retention: Default::default(), explicit: false }
    }
}

/// What `CHLOECTX_RECEIPTS` says, if it is set.
pub fn receipts_env() -> Option<bool> {
    std::env::var("CHLOECTX_RECEIPTS").ok().filter(|v| !v.is_empty()).map(|v| v != "0")
}

impl Default for Config {
    fn default() -> Self {
        Config {
            embedder: std::env::var("CHLOECTX_EMBEDDER").unwrap_or_else(|_| "static".into()),
            idle_secs: 1800,
            rank: RankParams::default(),
            pack: PackParams::default(),
            gate: Gate::default(),
            briefing_chars: 6000,
            knowledge: Default::default(),
            file_notes: true,
            next_files: std::env::var("CHLOECTX_NEXT_FILES").is_ok_and(|v| v == "1"),
            error_fixes: std::env::var("CHLOECTX_ERROR_FIXES").map_or(true, |v| v != "0"),
            sync: true,
            distill: Default::default(),
            consolidate: Default::default(),
            deep: Default::default(),
            receipts: Default::default(),
        }
    }
}

impl Config {
    /// The store's config.json; the gate defaults to the one fitted for the
    /// configured embedder unless the file sets one.
    pub fn load(path: &Path) -> Config {
        let raw: Json = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null);
        let mut cfg: Config = serde_json::from_value(raw.clone()).unwrap_or_default();
        if raw.get("gate").is_none() {
            cfg.gate = Gate::for_embedder(&cfg.embedder);
        }
        cfg.receipts.explicit = raw.get("receipts").and_then(|r| r.get("enabled")).is_some_and(|v| v.is_boolean());
        cfg.deep = cfg.deep.resolved();
        cfg
    }
}

// ---------------------------------------------------------------- sessions

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    /// Records of this session at or after this sequence number are in the
    /// model's context.
    pub window_start: i64,
    pub turn: i64,
    pub injected: HashSet<String>,
    pub annotated: HashSet<String>,
    pub episode: EpisodeAcc,
    pub episode_stored: bool,
    pub last_prompts: VecDeque<String>,
    pub last_compact: i64,
    pub last_seen: i64,
    /// Bytes of the transcript already scanned for narration.
    pub transcript_offset: u64,
    /// Failed commands followed until they pass ([`crate::fixes`]).
    #[serde(default)]
    pub fixes: crate::fixes::Tracker,
}

/// A query-understanding search started after a prompt (`async` mode).
#[derive(Clone, Debug, Default)]
struct DeepJob {
    turn: i64,
    done: bool,
    text: String,
    ids: Vec<String>,
    confidence: String,
    delivered: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    sessions: HashMap<String, Session>,
    active: String,
}

pub struct Daemon {
    pub paths: StorePaths,
    pub project: String,
    pub store: Arc<Store>,
    pub cfg: Config,
    state: Mutex<State>,
    /// The state changed since it was last written (it is written after
    /// the response, off the hook's critical path).
    dirty: std::sync::atomic::AtomicBool,
    /// Background work still running (distillation); shutdown waits for it.
    pending: std::sync::atomic::AtomicUsize,
    /// Held while state.json is written, so an older state never lands
    /// after a newer one.
    saving: Mutex<()>,
    /// Held while events.log is appended to or rewritten (`redact`).
    logging: Mutex<()>,
    /// Distillations run one at a time, so each sees what the last wrote.
    distilling: Mutex<()>,
    /// Turns distilled since the notes were last consolidated.
    distilled_since: std::sync::atomic::AtomicUsize,
    /// Query-understanding searches by session.
    deep_jobs: Mutex<HashMap<String, DeepJob>>,
    /// Files edited after each file, counted from the stored edits.
    next_files: Mutex<crate::next_files::NextFiles>,
    /// The settings Claude Code's hooks last ran with (their environment):
    /// they win over the daemon's own, which may have been started from a
    /// shell or the MCP server without them.
    from_hooks: Mutex<(String, Option<crate::deep::DeepConfig>)>,
    /// `CHLOECTX_DISTILL` as the hooks saw it: 0 unset, 1 on, 2 off.
    hook_distill: std::sync::atomic::AtomicU8,
    last_activity: AtomicI64,
    /// Receipts of recalls: opened when they are on or the file exists
    /// (a forget or redact scrubs it while they are off), unset when its
    /// file failed to open.
    receipts: std::sync::OnceLock<crate::receipt::Book>,
    /// Receipts are written (`receipts.enabled`, `CHLOECTX_RECEIPTS`,
    /// `chloectx receipts on|off`).
    receipts_on: std::sync::atomic::AtomicBool,
    /// Receipts made while answering, written once the answer is sent.
    unsaved: Mutex<Vec<crate::receipt::Receipt>>,
    /// Work to run after an answer, queued or running; shutdown waits.
    deferred: std::sync::atomic::AtomicUsize,
    /// `chloectx receipts on|off` decided: the hooks' environment no longer does.
    no_hook_receipts: std::sync::atomic::AtomicBool,
    snapshots: crate::snapshots::Snapshots,
    /// A recall showed records since the last snapshot was kept.
    keep_pending: std::sync::atomic::AtomicBool,
}

/// Work to do after the response is sent (so hooks return at once).
pub type Deferred = Box<dyn FnOnce() + Send>;

impl Daemon {
    pub fn open(dir: &Path, project: &str) -> Result<Daemon> {
        let paths = StorePaths::new(dir);
        crate::paths::create_private_dir(dir)?;
        let cfg = Config::load(&paths.config);
        let embedder: Arc<dyn embed::Embedder> = Arc::from(embed::open(&cfg.embedder)?);
        let store = Arc::new(Store::open(&paths.db, embedder, StoreOptions { sync: cfg.sync, ..Default::default() })?);
        let state: State = std::fs::read_to_string(&paths.state).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let receipts = std::sync::OnceLock::new();
        if cfg.receipts.enabled || dir.join(crate::receipt::BOOK_FILE).exists() {
            match crate::receipt::Book::open(dir, cfg.sync) {
                Ok(b) => {
                    let _ = receipts.set(b);
                }
                Err(e) => eprintln!("chloectx: receipts are off: {e}"),
            }
        }
        let receipts_on = std::sync::atomic::AtomicBool::new(cfg.receipts.enabled);
        let snapshots = crate::snapshots::Snapshots::new(dir);
        Ok(Daemon { paths, project: project.to_string(), store, cfg, state: Mutex::new(state), dirty: Default::default(), pending: Default::default(), saving: Mutex::new(()),
            logging: Mutex::new(()), distilling: Mutex::new(()), distilled_since: Default::default(), deep_jobs: Mutex::new(HashMap::new()), next_files: Default::default(), from_hooks: Mutex::new((String::new(), None)), hook_distill: Default::default(), last_activity: AtomicI64::new(now()),
            receipts, receipts_on, unsaved: Default::default(), deferred: Default::default(), no_hook_receipts: Default::default(), snapshots, keep_pending: Default::default() })
    }

    /// Wraps work to run after the answer, so that shutdown waits for it.
    /// Counted from now, before the answer is sent.
    fn after(self: &Arc<Self>, f: impl FnOnce(&Daemon) + Send + 'static) -> Deferred {
        self.deferred.fetch_add(1, Ordering::AcqRel);
        let me = self.clone();
        Box::new(move || {
            struct Done<'a>(&'a std::sync::atomic::AtomicUsize);
            impl Drop for Done<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _done = Done(&me.deferred);
            f(&me);
        })
    }

    /// Housekeeping when the daemon starts: drops receipts and automatic
    /// snapshots past their retention, and what lies under `snapshots/`
    /// that no snapshot owns. Runs whatever the settings say now: turning
    /// receipts off must not keep the old ones for good.
    pub fn housekeep(&self) {
        let swept = self.snapshots.sweep();
        let pruned = self.snapshots.prune(&self.cfg.receipts.snapshots).unwrap_or_else(|e| {
            self.log(&json!({"ts": now(), "error": e.to_string(), "event": "snapshot_prune"}));
            0
        });
        let receipts = match self.receipts.get().map(|b| b.prune(&self.cfg.receipts.retention)) {
            Some(Ok(n)) => n,
            Some(Err(e)) => {
                self.log(&json!({"ts": now(), "error": e.to_string(), "event": "receipt_prune"}));
                0
            }
            None => 0,
        };
        if swept + pruned + receipts > 0 {
            self.log(&json!({"ts": now(), "event": "housekeep", "snapshot_dirs_swept": swept, "snapshots_pruned": pruned, "receipts_pruned": receipts}));
        }
    }

    /// Whether receipts are written now.
    pub fn receipts_on(&self) -> bool {
        self.receipts_on.load(Ordering::Acquire) && self.receipts.get().is_some()
    }

    /// Writes the receipts made while answering.
    fn save_receipts(&self) {
        let rs = std::mem::take(&mut *self.unsaved.lock().unwrap());
        if rs.is_empty() {
            return;
        }
        if let Some(b) = self.receipts.get() {
            if let Err(e) = b.put_all(&rs) {
                self.log(&json!({"ts": now(), "error": e.to_string(), "event": "receipt"}));
            }
        }
    }

    /// Writes the session state: copied under the session lock, written
    /// outside it, so a hook never waits for the file.
    fn save_state(&self) {
        let _one = self.saving.lock().unwrap_or_else(|e| e.into_inner());
        let mut st2 = {
            let st = self.state.lock().unwrap();
            State { sessions: st.sessions.clone(), active: st.active.clone() }
        };
        if st2.sessions.len() > 64 {
            let mut by: Vec<(String, i64)> = st2.sessions.iter().map(|(k, v)| (k.clone(), v.last_seen)).collect();
            by.sort_by_key(|x| -x.1);
            let keep: HashSet<String> = by.into_iter().take(64).map(|x| x.0).collect();
            st2.sessions.retain(|k, _| keep.contains(k));
        }
        if let Ok(s) = serde_json::to_string(&st2) {
            let tmp = self.paths.state.with_extension("json.tmp");
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(tmp, &self.paths.state);
            }
        }
    }

    /// Writes the session state if it changed.
    pub fn persist(&self) {
        if self.dirty.swap(false, Ordering::AcqRel) {
            self.save_state();
        }
    }

    /// Appends an event to events.log, its strings redacted.
    fn log(&self, line: &Json) {
        let mut line = line.clone();
        crate::redact::redact_json(&mut line);
        let _one = self.logging.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.paths.dir.join("events.log")) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Handles one request. Returns the response and work to run after it.
    pub fn handle(self: &Arc<Self>, req: &Json) -> (Json, Option<Deferred>) {
        self.last_activity.store(now(), Ordering::Relaxed);
        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let res = match op {
            "ping" => Ok((json!({"ok": true, "pid": std::process::id(), "records": self.store.len()}), None)),
            "hook" => {
                self.note_hook_settings(req);
                self.hook(req.get("event").and_then(|v| v.as_str()).unwrap_or(""), req.get("payload").unwrap_or(&Json::Null))
            }
            "search" => self.op_search(req).map(|r| (r, Some(self.after(|me| me.keep_after_recall())))),
            "receipts" => self.op_receipts(req).map(|r| (r, None)),
            "receipt" => self.op_receipt(req).map(|r| (r, None)),
            "blame" => self.op_blame(req).map(|r| (r, None)),
            "snapshot" => self.op_snapshot(req).map(|r| (r, None)),
            "timeline" => self.op_timeline(req).map(|r| (r, None)),
            "sql" => self.op_sql(req).map(|r| (r, None)),
            "remember" => self.op_remember(req).map(|r| (r, None)),
            "forget" => self.op_forget(req).map(|r| (r, None)),
            "find" => self.op_find(req).map(|r| (r, None)),
            "receipts_switch" => self.op_receipts_switch(req).map(|r| (r, None)),
            "redact" => self.op_redact(req).map(|r| (r, None)),
            "append" => self.op_append(req).map(|r| (r, None)),
            "stats" => self.op_stats().map(|r| (r, None)),
            "backup" => self.op_backup(req).map(|r| (r, None)),
            // Read-only views for the demo inspector (demo/server.py).
            "inspect_query" => crate::inspect::query(&self.store, req).map(|r| (r, None)),
            "inspect_layout" => crate::inspect::layout(&self.store).map(|r| (r, None)),
            "inspect_embed" => crate::inspect::embed(&self.store, req).map(|r| (r, None)),
            "distill_backfill" => self.distill_backfill().map(|(t, n)| (json!({"turns": t, "items": n}), None)),
            "consolidate" => self.op_consolidate().map(|r| (r, None)),
            "ask" => self.op_ask(req).map(|r| (r, None)),
            // What `chloectx app --local-stores` shows of this store, as it
            // shows a space of the cluster ([`crate::app::view`]).
            "view" => crate::app::view::by_name(&self.store.pinned(), req).map(|r| (r, None)),
            "capsule" => self.op_capsule(req).map(|r| (r, None)),
            "briefing" => {
                let sid = req.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let mut ck = self.checker();
                recall::briefing_checked(&self.store, sid, self.cfg.briefing_chars, now(), Some(&mut ck)).map(|(t, _)| (json!({"text": t}), None))
            }
            "sync" => self.op_sync(req).map(|r| (r, None)),
            "shutdown" => {
                let me = self.clone();
                Ok((json!({"ok": true}), Some(Box::new(move || me.shutdown()) as Deferred)))
            }
            _ => Err(err(format!("unknown op {op:?}"))),
        };
        match res {
            Ok(r) => r,
            Err(e) => (json!({"error": e.to_string()}), None),
        }
    }

    pub fn shutdown(&self) {
        let until = Instant::now() + Duration::from_secs(self.cfg.distill.timeout_secs + 30);
        // Distillation, and the work after each answer: the prompt's own
        // record, a tool call's, a snapshot being written.
        while (self.pending.load(Ordering::Acquire) > 0 || self.deferred.load(Ordering::Acquire) > 0) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        // What was recorded since the last push goes before the daemon does.
        let _ = self.sync_now(false);
        let _ = self.store.flush();
        self.save_receipts();
        if let Some(b) = self.receipts.get() {
            let _ = b.flush();
        }
        self.save_state();
        let _ = std::fs::remove_file(&self.paths.socket);
        std::process::exit(0);
    }

    /// Pushes what the store recorded since the last push to the server
    /// `chloectx sync setup` named (and, with `pull`, first stores the
    /// project's records the server holds and the store lacks): the
    /// sync's state after, or `None` with no server set up. One at a time.
    pub fn sync_now(&self, pull: bool) -> Option<crate::sync::SyncState> {
        static ONE: Mutex<()> = Mutex::new(());
        let remote = crate::sync::remote(&crate::paths::home())?;
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut st = crate::sync::load_state(&self.paths.dir, &remote.url);
        let key = crate::sync::project_key(&crate::paths::home(), &self.project);
        let pushed = crate::sync::push(&remote, &self.store, &key, &mut st);
        // What a pull stores keeps the server's sequence numbers, and the
        // next push sends again those past what was pushed: the same rows,
        // replaced by themselves. Taking them as pushed (pushed_seq set to
        // the store's last) also took as pushed whatever the hooks recorded
        // while the pull ran, which then never reached the server.
        let pulled = if pull && pushed.is_ok() { Some(crate::sync::pull(&remote, &self.store, &key, &mut st)) } else { None };
        st.last_error = match (&pushed, &pulled) {
            (Err(e), _) | (_, Some(Err(e))) => e.to_string(),
            _ => String::new(),
        };
        let (p, g) = (pushed.as_ref().map_or(0, |n| *n), pulled.as_ref().and_then(|r| r.as_ref().ok()).copied().unwrap_or(0));
        if p > 0 || g > 0 || !st.last_error.is_empty() {
            self.log(&json!({"ts": now(), "event": "sync", "server": remote.url, "pushed": p, "pulled": g, "error": st.last_error}));
        }
        crate::sync::save_state(&self.paths.dir, &st);
        Some(st)
    }

    /// `{"op": "sync", "what": "status" | "push" | "pull"}`.
    fn op_sync(&self, req: &Json) -> Result<Json> {
        let what = req.get("what").and_then(|v| v.as_str()).unwrap_or("status");
        let Some(remote) = crate::sync::remote(&crate::paths::home()) else {
            return Ok(json!({"server": null, "note": "no server set up: chloectx sync setup --url URL"}));
        };
        let st = match what {
            "push" => self.sync_now(false),
            "pull" => self.sync_now(true),
            "status" => Some(crate::sync::load_state(&self.paths.dir, &remote.url)),
            w => return Err(err(format!("sync: unknown {w:?} (status, push or pull)"))),
        }
        .unwrap_or_default();
        let key = crate::sync::project_key(&crate::paths::home(), &self.project);
        let on_server = if what == "status" { remote.count(&key).map_err(|e| e.to_string()) } else { Err(String::new()) };
        Ok(json!({
            "server": remote.url, "token": remote.has_token(), "project": self.project, "synced_as": key,
            "local_records": self.store.len(), "last_seq": self.store.last_seq(),
            "pushed_seq": st.pushed_seq, "pushed": st.pushed, "pulled": st.pulled,
            "last_push": st.last_push, "last_pull": st.last_pull, "last_error": st.last_error,
            "server_records": on_server.as_ref().ok(), "server_error": on_server.err().filter(|e| !e.is_empty()),
        }))
    }

    fn with_session<T>(&self, sid: &str, f: impl FnOnce(&mut Session) -> T) -> T {
        let mut st = self.state.lock().unwrap();
        st.active = sid.to_string();
        let s = st.sessions.entry(sid.to_string()).or_default();
        s.last_seen = now();
        let out = f(s);
        self.dirty.store(true, Ordering::Release);
        out
    }

    fn active_session(&self) -> (String, i64) {
        let st = self.state.lock().unwrap();
        let w = st.sessions.get(&st.active).map_or(0, |s| s.window_start);
        (st.active.clone(), w)
    }

    // ------------------------------------------------------------ hooks

    fn hook(self: &Arc<Self>, event: &str, p: &Json) -> Result<(Json, Option<Deferred>)> {
        let sid = p.get("session_id").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
        let deep_cfg = self.deep_cfg();
        let mode = deep_cfg.mode.clone();
        let ctx_out = |event: &str, text: String| -> Json {
            if text.is_empty() {
                json!({"output": null})
            } else {
                json!({"output": {"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}})
            }
        };
        match event {
            "SessionStart" => {
                let source = p.get("source").and_then(|v| v.as_str()).unwrap_or("startup").to_string();
                let next = self.store.last_seq() + 1;
                let last_prompts = self.with_session(&sid, |s| {
                    match source.as_str() {
                        "resume" => {}
                        "compact" => {
                            if now() - s.last_compact > 120 {
                                s.window_start = next;
                            }
                        }
                        _ => s.window_start = next,
                    }
                    if source != "resume" {
                        s.injected.clear();
                        s.annotated.clear();
                    }
                    s.last_prompts.clone()
                });
                let mut ck = self.checker();
                let pinned = self.store.pinned();
                let (mut text, shown) = recall::briefing_checked(&pinned, &sid, self.cfg.briefing_chars, now(), Some(&mut ck))?;
                if !shown.is_empty() {
                    let inputs = crate::receipt::Inputs::briefing(&sid, self.cfg.briefing_chars);
                    let rows = crate::receipt::retrieve(&pinned, &inputs);
                    if let Ok(mut out) = rows {
                        // The receipt names what was shown, checks and all.
                        out.ids = shown.clone();
                        let turn = self.state.lock().unwrap().sessions.get(&sid).map_or(0, |s| s.turn);
                        self.write_receipt(&sid, turn, "briefing", &pinned, inputs, &out, &text);
                    }
                }
                self.with_session(&sid, |s| s.injected.extend(shown));
                if source == "compact" && !last_prompts.is_empty() {
                    let q: Vec<String> = last_prompts.iter().rev().take(2).cloned().collect();
                    if let Some((r, _)) = self.recall_for(&sid, &q.join("\n"), true, "compact")? {
                        text += "\n\nRecords relevant to the work in progress, which the compaction summary may have left out:\n";
                        text += &r;
                    }
                }
                // What doctor would report that the daemon sees itself
                // (distillation failing, a newer version): said at the top.
                let home = crate::paths::home();
                let notice = crate::health::session_notice(&self.paths.dir, &home);
                if !notice.model.is_empty() {
                    let head = notice.model.join("\n");
                    text = if text.is_empty() { head } else { format!("{head}\n\n{text}") };
                }
                self.log(&json!({"ts": now(), "event": event, "session": sid, "source": source, "chars": text.len()}));
                let mut out = ctx_out("SessionStart", text);
                if !notice.user.is_empty() {
                    if out["output"].is_null() {
                        out["output"] = json!({});
                    }
                    out["output"]["systemMessage"] = json!(notice.user.join("\n"));
                }
                Ok((out, Some(self.after(move |me| {
                    me.keep_after_recall();
                    // The checkout is looked at again (no fetch) every few hours.
                    if crate::version::cached(&home, crate::version::CHECK_EVERY).is_none() {
                        crate::version::check(&home, None);
                    }
                }))))
            }
            "UserPromptSubmit" => {
                // Redacted at once: the prompt is kept in the session state
                // (state.json) and in the context of the turn's records.
                // A harness's envelopes around the prompt go first: recalled
                // later, they read as the conversation ([`crate::envelope`]).
                let prompt = crate::redact::redact(&crate::envelope::clean(p.get("prompt").and_then(|v| v.as_str()).unwrap_or("")));
                let (turn, recent) = self.with_session(&sid, |s| {
                    s.turn += 1;
                    s.episode = EpisodeAcc { prompt: prompt.clone(), ..Default::default() };
                    s.episode_stored = false;
                    let recent: Vec<String> = s.last_prompts.iter().rev().take(2).rev().cloned().collect();
                    s.last_prompts.push_back(prompt.chars().take(1000).collect());
                    while s.last_prompts.len() > 4 {
                        s.last_prompts.pop_front();
                    }
                    (s.turn, recent)
                });
                // Recall first: the prompt is in the live window anyway, and
                // storing it afterwards keeps the write (and the memtable
                // rebuild it causes) off the prompt's critical path.
                let mut text = self.recall_for(&sid, &prompt, false, "prompt")?.map(|x| x.0).unwrap_or_default();
                // The previous turn's search, when it finished after that turn
                // ended: offered now, once.
                if let Some(t) = self.take_late_deep(&sid, turn - 1) {
                    self.log(&json!({"ts": now(), "event": "deep_late", "session": sid, "turn": turn - 1, "chars": t.len()}));
                    let t = format!("ChloeCtx's search for your previous request finished after that turn ended:\n{t}");
                    text = if text.is_empty() { t } else { format!("{text}\n\n{t}") };
                }
                let deep = deep_cfg.wants(&prompt, self.store.len());
                let deep_async = deep && mode == "async";
                if deep && mode == "sync" {
                    let (t, _) = self.deep_for(&sid, &prompt, &recent, "sync");
                    if !t.is_empty() {
                        text = if text.is_empty() { t } else { format!("{text}\n\n{t}") };
                    }
                }
                {
                    let mut jobs = self.deep_jobs.lock().unwrap();
                    if deep_async {
                        jobs.insert(sid.clone(), DeepJob { turn, ..Default::default() });
                    } else {
                        jobs.remove(&sid);
                    }
                }
                Ok((
                    ctx_out("UserPromptSubmit", text),
                    Some(self.after(move |me| {
                        // The receipt's snapshot holds the version the recall
                        // read, so it is kept before the prompt is stored;
                        // when none is due (at most one an hour by default),
                        // the prompt is stored at once, ahead of the turn's
                        // tool calls.
                        me.save_receipts();
                        let due = me.keep_pending.load(Ordering::Acquire) && me.snapshot_due();
                        if due {
                            me.keep_after_recall();
                        }
                        if let Err(e) = me.store.append(&sid, turn, now(), capture::prompt_drafts(&prompt)) {
                            me.log(&json!({"ts": now(), "error": e.to_string(), "event": "UserPromptSubmit"}));
                        }
                        if !due {
                            me.keep_after_recall();
                        }
                        me.store.warm();
                        if deep_async {
                            let (t, res) = me.deep_for(&sid, &prompt, &recent, "async");
                            let mut jobs = me.deep_jobs.lock().unwrap();
                            if let Some(j) = jobs.get_mut(&sid).filter(|j| j.turn == turn) {
                                j.done = true;
                                j.text = t;
                                j.confidence = res.as_ref().map(|r| r.confidence.clone()).unwrap_or_default();
                                j.ids = res.map(|r| r.ids()).unwrap_or_default();
                            }
                        }
                    })),
                ))
            }
            "PreToolUse" => {
                if !self.cfg.file_notes {
                    return Ok((json!({"output": null}), None));
                }
                let tool = p.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
                let path = p.get("tool_input").and_then(|i| i.get("file_path")).and_then(|v| v.as_str()).unwrap_or("");
                if !matches!(tool, "Edit" | "MultiEdit" | "Write") || path.is_empty() {
                    return Ok((json!({"output": null}), None));
                }
                let rel = path.strip_prefix(&self.project).map(|x| x.trim_start_matches('/')).unwrap_or(path).to_string();
                let mut text = self.file_notes(&sid, &rel)?;
                if self.cfg.next_files {
                    let next = self.next_file_notes(&sid, &rel, &text)?;
                    if !next.is_empty() {
                        self.log(&json!({"ts": now(), "event": "next_files", "session": sid, "file": rel, "chars": next.len()}));
                        text = if text.is_empty() { next } else { format!("{text}\n{next}") };
                    }
                }
                Ok((ctx_out("PreToolUse", text), None))
            }
            "PostToolUse" | "PostToolUseFailure" => {
                let tool = p.get("tool_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let input = p.get("tool_input").cloned().unwrap_or(Json::Null);
                let mut resp = p.get("tool_response").cloned().unwrap_or(Json::Null);
                if event == "PostToolUseFailure" {
                    resp = json!({"is_error": true, "error": p.get("error").cloned().unwrap_or(resp)});
                }
                let project = self.project.clone();
                let fixes_on = self.cfg.error_fixes;
                let (turn, ctx, fixed) = self.with_session(&sid, |s| {
                    s.episode.add_tool(&tool, &input, &resp, &project);
                    let fixed = if fixes_on { s.fixes.observe(&tool, &input, &resp, &project) } else { Vec::new() };
                    (s.turn, format!("Turn {} request: {}", s.turn, s.episode.prompt.chars().take(200).collect::<String>()), fixed)
                });
                let mut text = self.take_deep(&sid, turn, Duration::ZERO, false).unwrap_or_default();
                if fixes_on && tool == "Bash" {
                    let fix = self.fix_for(&sid, &resp);
                    if !fix.is_empty() {
                        text = if text.is_empty() { fix } else { format!("{text}\n\n{fix}") };
                    }
                }
                Ok((
                    ctx_out(event, text),
                    Some(self.after(move |me| {
                        let mut drafts: Vec<Draft> = capture::tool_drafts(&tool, &input, &resp, &project).into_iter().map(|d| d.ctx(ctx.clone())).collect();
                        if !fixed.is_empty() {
                            let from = me.prompt_of(&sid, turn);
                            for f in &fixed {
                                let d = f.draft(&ctx);
                                drafts.push(match &from {
                                    Some(p) => d.link("from", p.clone()),
                                    None => d,
                                });
                            }
                            me.log(&json!({"ts": now(), "event": "fix", "session": sid, "turn": turn, "fixes": fixed.len()}));
                        }
                        if let Err(e) = me.store.append(&sid, turn, now(), drafts) {
                            me.log(&json!({"ts": now(), "error": e.to_string(), "event": "PostToolUse"}));
                        }
                        me.store.warm();
                    })),
                ))
            }
            "Stop" => {
                let answer = p.get("last_assistant_message").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let active = p.get("stop_hook_active").and_then(|v| v.as_bool()).unwrap_or(false);
                let cur = self.with_session(&sid, |s| s.turn);
                // By default the end of a turn neither waits for a search
                // still running nor asks the model to go on: what the search
                // finds is offered at the next prompt (CTX-1: waiting held
                // short turns ~10 s, and the block that followed made Claude
                // answer twice). With `stop_recheck` turned on, it waits a few
                // seconds at most and, when the search found records the turn
                // did not see, asks the model once to check its answer
                // against them, without repeating it.
                if !active && mode == "async" && deep_cfg.stop_recheck {
                    if let Some(t) = self.take_deep(&sid, cur, deep_cfg.stop_wait(), true) {
                        self.log(&json!({"ts": now(), "event": "deep_recheck", "session": sid, "turn": cur, "chars": t.len()}));
                        let reason = format!(
                            "{t}\n\nChloeCtx found these records for this request after the turn's last tool call (`stop_recheck` is on). If one of them changes your answer or your changes, make the correction now and say in a sentence or two what changed. If none does, end your turn with one short line saying so; do not repeat your answer."
                        );
                        return Ok((json!({"output": {"decision": "block", "reason": reason}}), None));
                    }
                }
                let (turn, ep, first) = self.with_session(&sid, |s| {
                    let first = !s.episode_stored;
                    s.episode_stored = true;
                    (s.turn, s.episode.clone(), first)
                });
                if !first {
                    return Ok((json!({"output": null}), None));
                }
                let tpath = p.get("transcript_path").and_then(|v| v.as_str()).map(String::from);
                Ok((
                    json!({"output": null}),
                    Some(self.after(move |me| {
                        let ctx = format!("Turn {turn} request: {}", ep.prompt.chars().take(200).collect::<String>());
                        if let Some(t) = &tpath {
                            me.sync_narration(&sid, turn, t, &answer, &ctx);
                        }
                        let mut drafts: Vec<Draft> = capture::answer_drafts(&answer).into_iter().map(|d| d.ctx(ctx.clone())).collect();
                        drafts.extend(ep.card(turn, &answer));
                        if let Err(e) = me.store.append(&sid, turn, now(), drafts) {
                            me.log(&json!({"ts": now(), "error": e.to_string(), "event": "Stop"}));
                        }
                        me.store.warm();
                        let files: Vec<String> = ep.files.iter().cloned().collect();
                        me.distill_turn(&sid, turn, now(), &ep.prompt, &answer, &ep.commands, &files);
                    })),
                ))
            }
            "PostCompact" => {
                let summary = p.get("compact_summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let turn = self.with_session(&sid, |s| s.turn);
                let recs = self.store.append(&sid, turn, now(), capture::summary_drafts(&summary))?;
                let first = recs.first().map_or(self.store.last_seq() + 1, |r| r.seq);
                self.with_session(&sid, |s| {
                    s.window_start = first;
                    s.last_compact = now();
                    s.injected.clear();
                    s.annotated.clear();
                });
                self.log(&json!({"ts": now(), "event": event, "session": sid, "summary_records": recs.len()}));
                Ok((json!({"output": null}), None))
            }
            "PreCompact" => {
                let tpath = p.get("transcript_path").and_then(|v| v.as_str()).map(String::from);
                let (turn, prompt) = self.with_session(&sid, |s| (s.turn, s.episode.prompt.clone()));
                Ok((
                    json!({"output": null}),
                    Some(self.after(move |me| {
                        if let Some(t) = &tpath {
                            let ctx = format!("Turn {turn} request: {}", prompt.chars().take(200).collect::<String>());
                            me.sync_narration(&sid, turn, t, "", &ctx);
                        }
                    })),
                ))
            }
            "SessionEnd" => {
                Ok((json!({"output": null}), Some(self.after(move |me| {
                    let _ = me.store.flush();
                }))))
            }
            _ => Ok((json!({"output": null}), None)),
        }
    }

    /// Writes down what a turn decided, ruled or announced (see
    /// [`crate::distill`]): new items that are not near-copies of what is
    /// already noted, replacements (the old item is marked superseded), and
    /// planned work it completed (marked done).
    #[allow(clippy::too_many_arguments)]
    pub fn distill_turn(&self, sid: &str, turn: i64, ts: i64, prompt: &str, answer: &str, commands: &[String], files: &[String]) {
        if !self.distill_cfg().active() || (prompt.trim().is_empty() && answer.trim().is_empty()) {
            return;
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        let _one = self.distilling.lock().unwrap_or_else(|e| e.into_inner());
        // The notes most related to this exchange, wherever they are in the
        // history, and the newest few: the model can update an old note
        // instead of adding a near-copy next to it.
        let about: String = format!("{}\n{}", prompt.chars().take(1500).collect::<String>(), answer.chars().take(1500).collect::<String>());
        let mut known: Vec<Record> = recall::related(&self.store, &about, 30).unwrap_or_default();
        for r in crate::consolidate::live_notes(&self.store, 40).unwrap_or_default() {
            if known.len() >= 40 {
                break;
            }
            if !known.iter().any(|k| k.id == r.id) {
                known.push(r);
            }
        }
        known.sort_by_key(|r| -r.seq);
        let listed: Vec<(String, Kind, String)> = known.iter().rev().map(|r| (r.id.clone(), r.kind, r.text.clone())).collect();
        let t0 = Instant::now();
        match crate::distill::distill(&self.distill_cfg(), prompt, answer, commands, &listed, ts) {
            Ok(d) => {
                let is_known = |id: &String| known.iter().any(|r| &r.id == id);
                let replaced: Vec<String> = d.replaced.into_iter().filter(|id| is_known(id)).collect();
                let done: Vec<String> = d.done.into_iter().filter(|id| known.iter().any(|r| &r.id == id && r.kind == Kind::Todo)).collect();
                // Near-copies of what is noted (other than what they replace) add nothing.
                let terms = |t: &str| -> HashSet<String> { chloedb::text::analyze(t).into_iter().collect() };
                let mut seen: Vec<HashSet<String>> = known.iter().filter(|r| !replaced.contains(&r.id)).map(|r| terms(&r.text)).collect();
                let ctx = format!("Turn {turn} request: {}", prompt.chars().take(200).collect::<String>());
                // Each note links to the turn it was written from.
                let source = self.prompt_of(sid, turn);
                let n = d.drafts.len();
                let mut drafts = Vec::new();
                for x in d.drafts {
                    let t = terms(&x.text);
                    let dup = seen.iter().any(|s| {
                        let inter = s.intersection(&t).count() as f64;
                        let union = (s.len() + t.len()) as f64 - inter;
                        union > 0.0 && inter / union >= 0.6
                    });
                    if !dup {
                        seen.push(t);
                        // Anchored to the code the note is about, as it is now.
                        let a = anchor::make(Path::new(&self.project), &x.text, &x.files, files);
                        let ctx = if a.is_empty() { ctx.clone() } else { format!("{ctx}\n{}", a.to_line()) };
                        let mut x = x.ctx(ctx);
                        if let Some(src) = &source {
                            x = x.link("from", src.clone());
                        }
                        drafts.push(x);
                    }
                }
                let kept = drafts.len();
                if !drafts.is_empty() {
                    if let Err(e) = self.store.append(sid, turn, ts, drafts) {
                        self.log(&json!({"ts": now(), "error": e.to_string(), "event": "distill"}));
                    }
                }
                for id in &replaced {
                    let _ = self.store.retag(id, "superseded");
                }
                for id in &done {
                    let _ = self.store.retag(id, "done");
                }
                self.store.warm();
                self.log(&json!({"ts": now(), "event": "distill", "session": sid, "turn": turn, "items": n, "kept": kept,
                    "replaced": replaced, "done": done, "cost_usd": d.cost_usd, "tokens": d.usage.json(), "ms": t0.elapsed().as_millis() as u64}));
            }
            Err(e) => self.log(&json!({"ts": now(), "event": "distill", "session": sid, "turn": turn, "error": e.to_string()})),
        }
        let since = self.distilled_since.fetch_add(1, Ordering::AcqRel) + 1;
        let cc = &self.cfg.consolidate;
        if cc.active() && since >= cc.every_turns {
            let live = crate::consolidate::live_notes(&self.store, 2000).map(|v| v.len()).unwrap_or(0);
            if live >= cc.min_live {
                self.distilled_since.store(0, Ordering::Release);
                self.consolidate_now();
            }
        }
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }

    /// Merges related notes (the caller holds the distillation lock).
    fn consolidate_now(&self) -> Json {
        let t0 = Instant::now();
        match crate::consolidate::run(&self.store, &self.cfg.consolidate, &self.distill_cfg(), now()) {
            Ok(r) => {
                let j = json!({"ts": now(), "event": "consolidate", "live_before": r.live_before, "groups": r.groups, "replaced": r.replaced,
                    "written": r.written, "cost_usd": r.cost_usd, "tokens": r.usage.json(), "errors": r.errors, "ms": t0.elapsed().as_millis() as u64});
                self.log(&j);
                j
            }
            Err(e) => {
                let j = json!({"ts": now(), "event": "consolidate", "error": e.to_string()});
                self.log(&j);
                j
            }
        }
    }

    fn op_consolidate(&self) -> Result<Json> {
        let _one = self.distilling.lock().unwrap_or_else(|e| e.into_inner());
        self.distilled_since.store(0, Ordering::Release);
        let j = self.consolidate_now();
        let text = format!("{j}");
        Ok(json!({"text": text, "report": j}))
    }

    /// Checks notes' anchors against this project's working tree.
    /// Remembers the settings a hook ran with (see `from_hooks`).
    fn note_hook_settings(&self, req: &Json) {
        // CHLOECTX_RECEIPTS as the hook saw it, unless config.json or
        // `chloectx receipts on|off` decided.
        if let Some(on) = req.get("receipts").and_then(|v| v.as_bool()) {
            if !self.cfg.receipts.explicit && !self.no_hook_receipts.load(Ordering::Acquire) {
                self.receipts_on.store(on, Ordering::Release);
            }
        }
        if let Some(c) = req.get("claude").and_then(|v| v.as_str()) {
            crate::llm::set_hook_cli(Path::new(c));
        }
        if let Some(on) = req.get("distill").and_then(|v| v.as_bool()) {
            self.hook_distill.store(if on { 1 } else { 2 }, Ordering::Release);
        }
        let model = req.get("distill_model").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let deep = req.get("deep").cloned().and_then(|d| serde_json::from_value::<crate::deep::DeepConfig>(d).ok());
        let mut h = self.from_hooks.lock().unwrap();
        if !model.is_empty() {
            h.0 = model;
        }
        if deep.is_some() {
            h.1 = deep;
        }
    }

    /// The distiller's settings, with the model the hooks asked for.
    fn distill_cfg(&self) -> crate::distill::DistillConfig {
        let mut c = self.cfg.distill.clone();
        // CHLOECTX_DISTILL=0 in Claude Code's environment turns it off,
        // whichever process started the daemon.
        if self.hook_distill.load(Ordering::Acquire) == 2 {
            c.enabled = false;
        }
        let h = self.from_hooks.lock().unwrap();
        if !h.0.is_empty() && std::env::var("CHLOECTX_DISTILL_MODEL").map_or(true, |m| m.is_empty()) {
            c.model = h.0.clone();
        }
        c
    }

    /// Query-understanding settings: as the hooks resolved them, else the daemon's.
    fn deep_cfg(&self) -> crate::deep::DeepConfig {
        self.from_hooks.lock().unwrap().1.clone().unwrap_or_else(|| self.cfg.deep.clone())
    }

    fn checker(&self) -> Checker {
        Checker::new(Path::new(&self.project))
    }

    /// Runs query understanding for a prompt of a session: the context
    /// block (records not shown to the session yet) and the result.
    fn deep_for(&self, sid: &str, prompt: &str, recent: &[String], mode: &str) -> (String, Option<crate::deep::DeepResult>) {
        let (w, shown) = {
            let st = self.state.lock().unwrap();
            let s = st.sessions.get(sid).cloned().unwrap_or_default();
            (s.window_start, s.injected)
        };
        let filter = Filter { exclude_window: Some((sid.to_string(), w)), ..Default::default() };
        let project = Path::new(&self.project);
        let res = crate::deep::run(&self.store, &self.deep_cfg(), &self.distill_cfg(), prompt, recent, &filter, Some(project), now());
        let text = crate::deep::format(&res, &shown, now(), Some(project));
        let mut log = crate::deep::log_json(&res);
        log["ts"] = json!(now());
        log["event"] = json!("deep");
        log["session"] = json!(sid);
        log["mode"] = json!(mode);
        log["chars"] = json!(text.len());
        self.log(&log);
        if !text.is_empty() {
            let ids = res.ids();
            let mut st = self.state.lock().unwrap();
            if let Some(s) = st.sessions.get_mut(sid) {
                if mode == "sync" {
                    s.injected.extend(ids);
                }
            }
            self.dirty.store(true, Ordering::Release);
        }
        (text, Some(res))
    }

    /// The id of a turn's prompt record, which notes and fixes link to.
    fn prompt_of(&self, sid: &str, turn: i64) -> Option<String> {
        self.store
            .query_with(
                &format!("SELECT id FROM memory WHERE session = :session AND turn = {turn} AND kind = 'prompt' ORDER BY seq ASC LIMIT 1"),
                &Params::new().set("session", Value::Str(sid.to_string())),
            )
            .ok()
            .and_then(|rs| rs.rows.first().and_then(|r| r[0].as_str().map(String::from)))
    }

    /// For a failed command: the fixes known for its error that this
    /// session has not been shown, as context ([`crate::fixes`]).
    fn fix_for(&self, sid: &str, resp: &Json) -> String {
        let text = capture::response_text(resp);
        if !capture::looks_like_error(resp, &text) {
            return String::new();
        }
        let Some((sig, _)) = crate::fixes::signature(&text, &self.project) else { return String::new() };
        // Not one the model was shown, nor one of this session's live window
        // (in its context already).
        let (shown, window) = self.with_session(sid, |s| (s.injected.clone(), s.window_start));
        let found: Vec<Record> = match crate::fixes::known(&self.store, &sig, 4) {
            Ok(f) => f.into_iter().filter(|r| !shown.contains(&r.id) && !(r.session == sid && r.seq >= window)).take(2).collect(),
            Err(e) => {
                self.log(&json!({"ts": now(), "event": "fix_lookup", "error": e.to_string()}));
                return String::new();
            }
        };
        if found.is_empty() {
            return String::new();
        }
        let ids: Vec<String> = found.iter().map(|r| r.id.clone()).collect();
        self.log(&json!({"ts": now(), "event": "fix_shown", "session": sid, "ids": ids}));
        self.with_session(sid, |s| s.injected.extend(ids));
        crate::fixes::render(&found)
    }

    /// A search of an earlier `turn` that finished after its turn ended and
    /// was not delivered: its text, once, when the ranker was confident and
    /// the session has not been shown all of its picks since.
    fn take_late_deep(&self, sid: &str, turn: i64) -> Option<String> {
        let ids = {
            let jobs = self.deep_jobs.lock().unwrap();
            let j = jobs.get(sid).filter(|j| j.turn == turn && j.done && !j.delivered && !j.text.is_empty())?;
            j.ids.clone()
        };
        let seen = self.with_session(sid, |s| ids.iter().all(|i| s.injected.contains(i)));
        if seen {
            if let Some(j) = self.deep_jobs.lock().unwrap().get_mut(sid) {
                j.delivered = true;
            }
            return None;
        }
        self.take_deep(sid, turn, Duration::ZERO, true)
    }

    /// The finished search of `turn` not delivered yet, once: waits up to
    /// `wait` for one still running, and with `confident` returns it only
    /// when the ranker's confidence is high or medium.
    fn take_deep(&self, sid: &str, turn: i64, wait: Duration, confident: bool) -> Option<String> {
        let until = Instant::now() + wait;
        loop {
            {
                let mut jobs = self.deep_jobs.lock().unwrap();
                let j = jobs.get_mut(sid).filter(|j| j.turn == turn && !j.delivered)?;
                if j.done {
                    j.delivered = true;
                    if j.text.is_empty() || (confident && !matches!(j.confidence.as_str(), "high" | "medium")) {
                        return None;
                    }
                    let (text, ids) = (j.text.clone(), j.ids.clone());
                    drop(jobs);
                    let mut st = self.state.lock().unwrap();
                    if let Some(s) = st.sessions.get_mut(sid) {
                        s.injected.extend(ids);
                    }
                    self.dirty.store(true, Ordering::Release);
                    return Some(text);
                }
            }
            if Instant::now() >= until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// memory_ask: query understanding for a question, everything it picks.
    fn op_ask(&self, req: &Json) -> Result<Json> {
        let q = req.get("question").or_else(|| req.get("query")).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if q.is_empty() {
            return Err(err("question is empty"));
        }
        // By search alone unless a model is asked to plan and pick.
        let deep = req.get("deep").and_then(|v| v.as_bool()).unwrap_or(false) || req.get("model").and_then(|v| v.as_str()).is_some_and(|m| !m.is_empty());
        if !deep {
            let (active, w) = self.active_session();
            let mut filter = Filter::default();
            if !req.get("include_current").and_then(|v| v.as_bool()).unwrap_or(false) && !active.is_empty() {
                filter.exclude_window = Some((active.clone(), w));
            }
            let built = crate::context::build(&self.store, &q, &filter, &crate::context::ContextParams::default())?;
            let text = if built.text.is_empty() { "ChloeCtx found nothing relevant in memory.".to_string() } else { built.text };
            let ids: Vec<String> = built.records.iter().map(|r| r.id.clone()).collect();
            self.log(&json!({"ts": now(), "event": "ask", "mode": "search", "question": q, "records": ids.len(), "ms": built.ms}));
            return Ok(json!({"text": text, "ids": ids, "ms": built.ms}));
        }
        let mut cfg = self.deep_cfg();
        if let Some(m) = req.get("model").and_then(|v| v.as_str()).filter(|m| !m.is_empty()) {
            cfg.planner_model = m.to_string();
            cfg.ranker_model = m.to_string();
        }
        let (active, w) = self.active_session();
        let mut filter = Filter::default();
        if !req.get("include_current").and_then(|v| v.as_bool()).unwrap_or(false) && !active.is_empty() {
            filter.exclude_window = Some((active.clone(), w));
        }
        let project = Path::new(&self.project);
        let distill = self.distill_cfg();
        let mut res = crate::deep::run(&self.store, &cfg, &distill, &q, &[], &filter, Some(project), now());
        // A question asked on purpose gets an answer even when the planner
        // thought memory could not matter.
        if !res.plan.need {
            res.plan.need = true;
            let (cands, _) = crate::deep::gather(&self.store, &cfg, &q, &res.plan, &filter);
            let r = crate::deep::rank(&self.store, &cfg, &distill, &q, &cands, &HashMap::new(), false, now());
            res.adopt(r, cands);
        }
        let mut text = crate::deep::format(&res, &HashSet::new(), now(), Some(project));
        if text.is_empty() {
            text = format!("ChloeCtx found nothing relevant in memory ({} searches over {} candidates).", res.plan.queries.len() + 2, res.candidates.len());
        }
        let mut log = crate::deep::log_json(&res);
        log["ts"] = json!(now());
        log["event"] = json!("ask");
        self.log(&log);
        Ok(json!({"text": text, "ids": res.ids(), "answer": res.answer, "confidence": res.confidence, "cost_usd": res.cost_usd, "ms": res.ms}))
    }

    /// Distills every past turn of the store that has a prompt (for stores
    /// recorded before distillation existed). Returns (turns, items).
    fn distill_backfill(&self) -> Result<(usize, usize)> {
        let rs = self.store.query(&format!("SELECT {} FROM memory WHERE kind IN ('prompt', 'answer', 'episode') ORDER BY seq ASC LIMIT 1000000", crate::record::COLUMNS))?;
        let recs: Vec<Record> = rs.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
        // (session, turn) -> (prompt, answer, commands, ts, files)
        let mut turns: Vec<((String, i64), (String, String, Vec<String>, i64, Vec<String>))> = Vec::new();
        for r in recs {
            let key = (r.session.clone(), r.turn);
            if turns.last().map_or(true, |(k, _)| *k != key) {
                turns.push((key.clone(), Default::default()));
            }
            let (_, (p, a, c, ts, files)) = turns.last_mut().unwrap();
            *ts = (*ts).max(r.ts);
            match r.kind {
                Kind::Prompt => {
                    p.push_str(&r.text);
                    p.push('\n');
                }
                Kind::Answer if r.tag != "narration" => {
                    a.push_str(&r.text);
                    a.push('\n');
                }
                Kind::Episode => {
                    if let Some(line) = r.text.lines().find(|l| l.starts_with("Ran: ")) {
                        c.extend(line[5..].split(" ; ").map(String::from));
                    }
                    files.extend(r.files.split_whitespace().map(String::from));
                }
                _ => {}
            }
        }
        let before = self.store.len();
        let n = turns.len();
        for ((sid, turn), (p, a, c, ts, files)) in turns {
            if !p.trim().is_empty() {
                self.distill_turn(&sid, turn, ts, p.trim(), a.trim(), &c, &files);
            }
        }
        Ok((n, (self.store.len() - before) as usize))
    }

    /// Stores assistant text written between tool calls since the last scan
    /// of the transcript (hooks only see the final message of a turn).
    fn sync_narration(&self, sid: &str, turn: i64, path: &str, final_answer: &str, ctx: &str) {
        let offset = {
            let st = self.state.lock().unwrap();
            st.sessions.get(sid).map_or(0, |s| s.transcript_offset)
        };
        let Ok((texts, next)) = crate::transcript::tail_texts(Path::new(path), offset) else { return };
        self.with_session(sid, |s| s.transcript_offset = next);
        let drafts: Vec<Draft> = texts
            .iter()
            .filter(|t| t.trim() != final_answer.trim())
            .flat_map(|t| capture::narration_drafts(t))
            .map(|d| d.ctx(ctx.to_string()))
            .collect();
        if let Err(e) = self.store.append(sid, turn, now(), drafts) {
            self.log(&json!({"ts": now(), "error": e.to_string(), "event": "narration"}));
        }
    }

    /// Gated recall for a prompt in a session; `force` skips the gate.
    /// Returns the context and the ids it shows. `source` names the recall
    /// in its receipt (`prompt`, `compact`).
    fn recall_for(&self, sid: &str, prompt: &str, force: bool, source: &str) -> Result<Option<(String, Vec<String>)>> {
        let (w, skip, turn) = {
            let st = self.state.lock().unwrap();
            let s = st.sessions.get(sid).cloned().unwrap_or_default();
            (s.window_start, s.injected, s.turn)
        };
        let query: String = prompt.chars().take(2000).collect();
        let filter = Filter { exclude_window: Some((sid.to_string(), w)), ..Default::default() };
        let t0 = Instant::now();
        // Knowledge that matches the prompt on its own text is shown even
        // when the gate holds back the rest: a request that does not sound
        // like earlier work can still be bound by what was agreed.
        // The two searches are independent (only packing needs both), so
        // they run side by side when a core is free, on one snapshot
        // ([`crate::receipt::retrieve`], which a receipt's replay runs too).
        let store = self.store.pinned();
        let inputs = crate::receipt::Inputs::recall(&query, sid, &filter, &skip, force, &self.cfg.rank, &self.cfg.pack, &self.cfg.gate, &self.cfg.knowledge);
        let out = crate::receipt::retrieve(&store, &inputs)?;
        let decision = out.decision.unwrap_or(Decision::Skip);
        let mut parts = Vec::new();
        let mut ck = self.checker();
        if !out.known.is_empty() {
            parts.push(recall::fmt_knowledge_checked(&out.known, now(), Some(&mut ck)));
        }
        if !out.items.is_empty() {
            let mut t = recall::fmt_recall_checked(&out.items, now(), out.weak, Some(&mut ck));
            t += &recall::fmt_more(&out.hits, &out.shown, if out.weak { crate::receipt::MORE_BRIEF } else { crate::receipt::MORE_FULL });
            parts.push(t);
        }
        let ids = out.ids.clone();
        let text = if parts.is_empty() { None } else { Some((parts.join("\n"), ids.clone())) };
        if !ids.is_empty() {
            let mut st = self.state.lock().unwrap();
            if let Some(s) = st.sessions.get_mut(sid) {
                s.injected.extend(ids.iter().cloned());
            }
            self.dirty.store(true, Ordering::Release);
        }
        let receipt = self.write_receipt(sid, turn, source, &store, inputs, &out, text.as_ref().map_or("", |t| t.0.as_str()));
        let known: Vec<Json> = out.known.iter().map(|(r, c, v)| json!({"id": r.id, "cos": c, "cov": v})).collect();
        self.log(&json!({
            "ts": now(), "event": "recall", "session": sid, "decision": format!("{decision:?}"), "gate": out.gate,
            "evidence": out.ev, "candidates": out.hits.len(), "injected": ids, "knowledge": known, "chars": text.as_ref().map_or(0, |t| t.0.len()),
            "receipt": receipt, "txn": store.db().map(|_| store.snapshot().txn()),
            "ms": t0.elapsed().as_secs_f64() * 1e3,
        }));
        Ok(text)
    }

    /// Makes a recall's receipt; its id, or None when receipts are off.
    /// It is written after the answer is sent ([`Daemon::save_receipts`]),
    /// off the hook's critical path. A recall that showed records asks
    /// for a snapshot to be kept, which [`Daemon::keep_after_recall`]
    /// takes once the hook has answered.
    #[allow(clippy::too_many_arguments)]
    fn write_receipt(&self, sid: &str, turn: i64, source: &str, store: &Store, inputs: crate::receipt::Inputs, out: &crate::receipt::Outcome, text: &str) -> Option<String> {
        if !self.receipts_on() {
            return None;
        }
        let book = self.receipts.get()?;
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64) + crate::clock_offset() * 1000;
        let r = crate::receipt::Receipt::new(book.next_id(ms), now(), sid, turn, source, store, inputs, out, text);
        let id = r.id.clone();
        self.unsaved.lock().unwrap().push(r);
        if !out.ids.is_empty() && self.cfg.receipts.snapshots.after_recall {
            self.keep_pending.store(true, Ordering::Release);
        }
        Some(id)
    }

    /// Whether an automatic snapshot would be taken now.
    fn snapshot_due(&self) -> bool {
        self.store.db().is_some_and(|db| self.snapshots.due(db, &self.cfg.receipts.snapshots))
    }

    /// Writes the receipts made while answering, then keeps a snapshot of
    /// the store if a recall asked for one since the last, before anything
    /// else is written: the receipt then replays on the very version it
    /// read. Old snapshots are pruned first, whether or not one is taken.
    /// Runs after the hook has answered.
    fn keep_after_recall(&self) {
        self.save_receipts();
        if !self.keep_pending.swap(false, Ordering::AcqRel) {
            return;
        }
        let Some(db) = self.store.db() else { return };
        let r = &self.cfg.receipts.snapshots;
        if let Err(e) = self.snapshots.prune(r) {
            self.log(&json!({"ts": now(), "error": e.to_string(), "event": "snapshot_prune"}));
        }
        if !self.receipts_on() {
            return;
        }
        if let Err(e) = self.snapshots.keep(db, None, r) {
            self.log(&json!({"ts": now(), "error": e.to_string(), "event": "snapshot"}));
        }
    }

    fn file_notes(&self, sid: &str, rel: &str) -> Result<String> {
        let (w, seen) = {
            let st = self.state.lock().unwrap();
            let s = st.sessions.get(sid).cloned().unwrap_or_default();
            (s.window_start, s.annotated.contains(rel))
        };
        if seen {
            return Ok(String::new());
        }
        self.with_session(sid, |s| {
            s.annotated.insert(rel.to_string());
        });
        let terms: Vec<String> = chloedb::text::analyze(rel);
        if terms.is_empty() {
            return Ok(String::new());
        }
        let kinds = vec![Kind::Error, Kind::Decision, Kind::Preference, Kind::Fact, Kind::Note, Kind::Episode];
        let filter = Filter { exclude_window: Some((sid.to_string(), w)), kinds, ..Default::default() };
        let mut where_ = filter.conds();
        where_.push("MATCH(files, :files)".into());
        let params = filter.params().set("files", Value::Str(terms.join(" AND ")));
        let rs = self.store.query_with(&format!("SELECT {} FROM memory WHERE {} ORDER BY seq DESC LIMIT 3", crate::record::COLUMNS, where_.join(" AND ")), &params)?;
        let recs: Vec<Record> = rs.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
        if recs.is_empty() {
            return Ok(String::new());
        }
        let mut s = format!("ChloeCtx notes on {rel} from earlier work (newest first):\n");
        let mut ck = self.checker();
        for r in recs.iter().take(2) {
            let snip: String = r.text.chars().take(500).collect();
            let label = if r.kind.is_knowledge() { ck.check_record(&r).label() } else { String::new() };
            s += &format!("\n{}\n", recall::fmt_record_labeled(r, &snip, now(), &label));
        }
        Ok(s)
    }

    /// The files usually edited after `rel`, each with one note not in
    /// `shown`: the newest decision, rule or fact about it, else its newest
    /// error (episodes, which name every file of a turn, are left out).
    /// Once per file and session.
    fn next_file_notes(&self, sid: &str, rel: &str, shown: &str) -> Result<String> {
        if !self.with_session(sid, |s| s.annotated.insert(format!("next:{rel}"))) {
            return Ok(String::new());
        }
        let next = {
            let mut nf = self.next_files.lock().unwrap();
            nf.update(&self.store)?;
            nf.after(rel, 2, 1.0)
        };
        let next: Vec<String> = next.into_iter().map(|(f, _)| f).filter(|f| Path::new(&self.project).join(f).exists()).collect();
        if next.is_empty() {
            return Ok(String::new());
        }
        let w = self.with_session(sid, |s| s.window_start);
        let kinds = vec![Kind::Error, Kind::Decision, Kind::Preference, Kind::Fact, Kind::Note];
        let filter = Filter { exclude_window: Some((sid.to_string(), w)), kinds, ..Default::default() };
        let mut s = format!("In earlier work, edits of {rel} were usually followed by edits of: {}.\n", next.join(", "));
        let mut shown = shown.to_string();
        let mut ck = self.checker();
        for f in &next {
            let terms: Vec<String> = chloedb::text::analyze(f);
            if terms.is_empty() {
                continue;
            }
            let mut where_ = filter.conds();
            where_.push("MATCH(files, :files)".into());
            let params = filter.params().set("files", Value::Str(terms.join(" AND ")));
            let rs = self.store.query_with(&format!("SELECT {} FROM memory WHERE {} ORDER BY seq DESC LIMIT 12", crate::record::COLUMNS, where_.join(" AND ")), &params)?;
            let recs: Vec<Record> = rs.rows.iter().filter_map(|r| crate::store::row_record(r)).filter(|r| !shown.contains(&format!("[{}]", r.id))).collect();
            if let Some(r) = recs.iter().find(|r| r.kind.is_knowledge()).or(recs.first()).cloned() {
                let snip: String = r.text.chars().take(300).collect();
                let label = if r.kind.is_knowledge() { ck.check_record(&r).label() } else { String::new() };
                let one = format!("\nChloeCtx note on {f}:\n{}\n", recall::fmt_record_labeled(&r, &snip, now(), &label));
                shown += &one;
                s += &one;
            }
        }
        Ok(s)
    }

    // ------------------------------------------------------------ tools

    fn op_search(&self, req: &Json) -> Result<Json> {
        let q = req.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let limit = req.get("limit").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 50) as usize;
        let kinds: Vec<Kind> = req.get("kinds").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|k| k.as_str().and_then(Kind::parse)).collect()).unwrap_or_default();
        let (active, w) = self.active_session();
        let mut f = Filter { kinds, ..Default::default() };
        if !req.get("include_current").and_then(|v| v.as_bool()).unwrap_or(false) && !active.is_empty() {
            f.exclude_window = Some((active.clone(), w));
        }
        if let Some(days) = req.get("since_days").and_then(|v| v.as_f64()) {
            f.since_ts = Some(now() - (days * 86_400.0) as i64);
        }
        if let Some(s) = req.get("session").and_then(|v| v.as_str()) {
            f.session = Some(s.to_string());
        }
        let store = self.store.pinned();
        let inputs = crate::receipt::Inputs::search(q, &f, limit, &self.cfg.rank);
        let out = crate::receipt::retrieve(&store, &inputs)?;
        let terms: HashSet<String> = chloedb::text::analyze_query(q).into_iter().collect();
        let mut text = String::new();
        for h in out.hits.iter().take(limit) {
            text += &recall::fmt_record(&h.record, &recall::snippet(&h.record.text, &terms, 700), now());
            text += &format!("\n(score {:.3})\n\n", h.score);
        }
        if text.is_empty() {
            text = "No matching records.".into();
        }
        let turn = self.state.lock().unwrap().sessions.get(&active).map_or(0, |s| s.turn);
        let receipt = self.write_receipt(&active, turn, "search", &store, inputs, &out, &text);
        if let Some(id) = receipt.as_ref().filter(|_| !out.ids.is_empty()) {
            text += &format!("(receipt {id}: `chloectx receipt {id}` shows why each record ranked where it did, and replays it)");
        }
        Ok(json!({"text": text, "evidence": out.ev, "ids": out.ids, "receipt": receipt}))
    }

    fn book(&self) -> Result<&crate::receipt::Book> {
        // Receipts made by an answer just sent are readable at once.
        self.save_receipts();
        self.receipts.get().ok_or_else(|| err("receipts are off for this store (`chloectx receipts on`, config.json receipts.enabled, or CHLOECTX_RECEIPTS=0)"))
    }

    /// `{"op": "receipts_switch", "on": bool, "purge"?: bool}`: turns
    /// receipts and automatic snapshots on or off for this store, in its
    /// config.json (which wins over `CHLOECTX_RECEIPTS`), from now on.
    /// With `purge`, the receipts kept so far and the automatic snapshots
    /// are removed too.
    fn op_receipts_switch(&self, req: &Json) -> Result<Json> {
        let on = req.get("on").and_then(|v| v.as_bool()).ok_or_else(|| err("receipts_switch needs on: true or false"))?;
        let purge = req.get("purge").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut cfg: Json = std::fs::read_to_string(&self.paths.config).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({}));
        if !cfg.is_object() {
            return Err(err(format!("{} is not a JSON object", self.paths.config.display())));
        }
        if !cfg.get("receipts").is_some_and(|r| r.is_object()) {
            cfg["receipts"] = json!({});
        }
        cfg["receipts"]["enabled"] = json!(on);
        let tmp = self.paths.config.with_extension("json.tmp");
        {
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(serde_json::to_string_pretty(&cfg)?.as_bytes())?;
        }
        std::fs::rename(&tmp, &self.paths.config)?;
        if on && self.receipts.get().is_none() {
            let b = crate::receipt::Book::open(&self.paths.dir, self.cfg.sync)?;
            let _ = self.receipts.set(b);
        }
        self.receipts_on.store(on, Ordering::Release);
        self.no_hook_receipts.store(true, Ordering::Release);
        let mut text = format!("Receipts and automatic snapshots are {} for this store.", if on { "on" } else { "off" });
        if purge {
            self.unsaved.lock().unwrap().clear();
            let receipts = match self.receipts.get() {
                Some(b) => b.purge()?,
                None => 0,
            };
            let (_, snaps, named) = self.snapshots.drop_automatic_after(|| ())?;
            text += &format!(" Removed {receipts} receipts and {snaps} automatic snapshots.");
            if !named.is_empty() {
                text += &format!(" The named snapshots {} are kept (`chloectx snapshot drop NAME`).", named.join(", "));
            }
        } else if !on {
            text += " The receipts and snapshots kept so far stay until their retention ends (`chloectx receipts off --purge` removes them now); a forget or redact still scrubs them.";
        }
        Ok(json!({"text": text, "on": on}))
    }

    /// `{"op": "receipts", "session"?, "limit"?}`: the newest receipts.
    fn op_receipts(&self, req: &Json) -> Result<Json> {
        let limit = req.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let rs = self.book()?.latest(req.get("session").and_then(|v| v.as_str()), limit)?;
        let mut text = String::new();
        for r in &rs {
            text += &format!(
                "{}  {}  session {} turn {:<3} {:<7} {:<6} {} shown, {} tokens · {}\n",
                r.id,
                recall::fmt_time(r.ts),
                r.session.chars().take(8).collect::<String>(),
                r.turn,
                r.source,
                r.decision.as_deref().unwrap_or("-"),
                r.shown.len(),
                r.tokens,
                r.inputs.query.chars().take(60).collect::<String>().replace('\n', " ")
            );
        }
        if text.is_empty() {
            text = "No receipts yet.".into();
        }
        let list: Vec<Json> = rs.iter().map(|r| json!({"id": r.id, "ts": r.ts, "session": r.session, "turn": r.turn, "source": r.source, "decision": r.decision, "shown": r.shown, "tokens": r.tokens, "txn": r.snapshot.txn, "query": r.inputs.query.chars().take(200).collect::<String>()})).collect();
        Ok(json!({"text": text, "receipts": list}))
    }

    /// `{"op": "receipt", "id", "replay"?}`: one receipt, and its replay on
    /// the snapshot kept at its commit and on today's store.
    fn op_receipt(&self, req: &Json) -> Result<Json> {
        let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let r = self.book()?.get(id)?.ok_or_else(|| err(format!("no receipt {id}")))?;
        let mut text = crate::receipt::fmt_receipt(&r);
        let replays = if req.get("replay").and_then(|v| v.as_bool()).unwrap_or(true) {
            let rs = crate::receipt::replay(&r, &self.snapshots, &self.store)?;
            text += "\nreplay:\n";
            text += &crate::blame::fmt_replays(&rs);
            rs
        } else {
            Vec::new()
        };
        if req.get("text").and_then(|v| v.as_bool()).unwrap_or(false) {
            text += &format!("\nwhat the agent was shown:\n{}\n", r.text);
        }
        // Each candidate's text as it is now (gone if deleted since), for a page.
        let ids: Vec<String> = r.rows.iter().map(|x| x.id.clone()).collect();
        let records: serde_json::Map<String, Json> = self
            .store
            .get(&ids)?
            .iter()
            .map(|x| (x.id.clone(), json!({"text": x.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect::<String>(), "tag": x.tag})))
            .collect();
        Ok(json!({"text": text, "receipt": r, "replays": replays, "records": records}))
    }

    /// `{"op": "blame", "target": "path[:line]", "replay"?}`.
    fn op_blame(&self, req: &Json) -> Result<Json> {
        let target = req.get("target").and_then(|v| v.as_str()).ok_or_else(|| err("blame needs a target: path or path:line"))?;
        let replay = req.get("replay").and_then(|v| v.as_bool()).unwrap_or(true);
        self.save_receipts();
        crate::blame::blame(&self.store, self.receipts.get(), &self.snapshots, &self.project, target, replay)
    }

    /// `{"op": "snapshot", "what": "keep" | "list" | "drop", "name"?}`.
    fn op_snapshot(&self, req: &Json) -> Result<Json> {
        let name = req.get("name").and_then(|v| v.as_str()).filter(|n| !n.is_empty());
        match req.get("what").and_then(|v| v.as_str()).unwrap_or("list") {
            "keep" => {
                let db = self.store.db().ok_or_else(|| err("the store is a table of a ChloeDB server"))?;
                let k = self.snapshots.keep(db, Some(name.ok_or_else(|| err("keep needs a name"))?), &self.cfg.receipts.snapshots)?;
                Ok(json!({"text": format!("kept {} at commit {} ({} records)", k.name, k.at, k.records), "snapshot": k}))
            }
            "drop" => {
                let n = name.ok_or_else(|| err("drop needs a name"))?;
                let found = self.snapshots.drop_named(n)?;
                Ok(json!({"text": if found { format!("dropped {n}") } else { format!("no snapshot {n}") }, "dropped": found}))
            }
            _ => {
                let all = self.snapshots.list();
                let mut text = String::new();
                for k in &all {
                    text += &format!("{:<28} commit {:<8} {}  {} records{}\n", k.name, k.at, recall::fmt_time(k.ts), k.records, if k.auto { "  (automatic)" } else { "" });
                }
                if text.is_empty() {
                    text = "No snapshots kept.".into();
                }
                Ok(json!({"text": text, "snapshots": all}))
            }
        }
    }

    /// After a record is forgotten or records are redacted: the receipts'
    /// copies of their text are scrubbed (whether receipts are on now or
    /// not), and the automatic snapshots, which hold the old text, are
    /// dropped. For a forget, the store's segments that held the record
    /// are first rewritten without it, with no snapshot being taken
    /// meanwhile, so that no later snapshot copies its bytes. What it did,
    /// in words.
    fn scrub_history(&self, forgotten: Option<&crate::receipt::Forgotten>) -> String {
        let mut out = Vec::new();
        self.save_receipts();
        if let Some(b) = self.receipts.get() {
            match b.scrub(forgotten) {
                Ok(0) => {}
                Ok(n) => out.push(format!("{n} receipt{} scrubbed", if n == 1 { "" } else { "s" })),
                Err(e) => out.push(format!("scrubbing the receipts failed: {e}")),
            }
        }
        let compact = || -> Option<String> {
            forgotten?;
            let db = self.store.db()?;
            match compact_deleted(db) {
                Ok(_) => None,
                Err(e) => Some(format!("compacting the store failed ({e}): the record's bytes stay in its file until a merge rewrites them")),
            }
        };
        match self.snapshots.drop_automatic_after(compact) {
            Ok((failed, dropped, named)) => {
                out.extend(failed);
                if dropped > 0 {
                    out.push(format!("{dropped} automatic snapshot{} dropped", if dropped == 1 { "" } else { "s" }));
                }
                if !named.is_empty() {
                    out.push(format!("the named snapshots {} still hold the old text (`chloectx snapshot drop NAME`)", named.join(", ")));
                }
            }
            Err(e) => out.push(format!("dropping the automatic snapshots failed: {e}")),
        }
        out.join("; ")
    }

    fn op_timeline(&self, req: &Json) -> Result<Json> {
        let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let before = req.get("before").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
        let after = req.get("after").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
        Ok(json!({"text": timeline_text(&self.store, id, before, after)?}))
    }

    fn op_sql(&self, req: &Json) -> Result<Json> {
        let sql = req.get("sql").and_then(|v| v.as_str()).unwrap_or("").trim();
        let rs = self.store.query_guarded(sql, &chloedb::query::Params::new(), &crate::sqlguard::Limits::TOOL)?;
        Ok(json!({"text": rows_text(&rs)}))
    }

    fn op_remember(&self, req: &Json) -> Result<Json> {
        let kind = req.get("kind").and_then(|v| v.as_str()).and_then(Kind::parse).unwrap_or(Kind::Note);
        let text = crate::redact::redact(req.get("text").and_then(|v| v.as_str()).unwrap_or("").trim());
        if text.is_empty() {
            return Err(err("text is empty"));
        }
        let files: Vec<String> = req.get("files").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let (sid, _) = self.active_session();
        let sid = if sid.is_empty() { "manual".to_string() } else { sid };
        let turn = self.with_session(&sid, |s| s.turn);
        let mut replaced = None;
        if let Some(old) = req.get("supersedes").and_then(|v| v.as_str()) {
            if let Some(mut r) = self.store.get(&[old.to_string()])?.pop() {
                r.tag = "superseded".into();
                r.importance *= 0.3;
                self.store.insert(&[r])?;
                replaced = Some(old.to_string());
            }
        }
        let a = anchor::make(Path::new(&self.project), &text, &files, &[]);
        let mut d = Draft::new(kind, text).files(files).tag("remembered");
        if !a.is_empty() {
            d = d.ctx(a.to_line());
        }
        // What it replaced, as a distilled replacement links it.
        if let Some(old) = replaced {
            d = d.link("supersedes", old);
        }
        let recs = self.store.append(&sid, turn, now(), vec![d])?;
        Ok(json!({"id": recs[0].id, "text": format!("Stored {} as {}.", kind.as_str(), recs[0].id)}))
    }

    /// `{"op": "forget", "id": ID}`, or `{"op": "forget", "ids": [...]}` for
    /// several at once: each record is deleted and scrubbed from the
    /// receipts, and the automatic snapshots are dropped and the store
    /// compacted once for all of them. Answers `deleted` and `missing`.
    fn op_forget(&self, req: &Json) -> Result<Json> {
        let ids: Vec<String> = match req.get("ids").and_then(|v| v.as_array()) {
            Some(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            None => vec![req.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string()],
        };
        let (mut deleted, mut missing, mut forgotten) = (Vec::new(), Vec::new(), Vec::new());
        for id in &ids {
            let rec = self.store.get(&[id.clone()])?.pop();
            if !self.store.delete(id)? {
                missing.push(id.clone());
                continue;
            }
            deleted.push(id.clone());
            forgotten.push(crate::receipt::Forgotten { id: id.clone(), prompt: rec.filter(|r| r.kind == Kind::Prompt).map(|r| r.text) });
        }
        // The receipts are scrubbed of each record; the automatic snapshots
        // are dropped and the store compacted once, after the last.
        let mut scrubbed = Vec::new();
        if let Some((last, rest)) = forgotten.split_last() {
            if !rest.is_empty() {
                self.save_receipts();
                if let Some(b) = self.receipts.get() {
                    let mut n = 0;
                    for f in rest {
                        match b.scrub(Some(f)) {
                            Ok(k) => n += k,
                            Err(e) => scrubbed.push(format!("scrubbing the receipts failed: {e}")),
                        }
                    }
                    if n > 0 {
                        scrubbed.push(format!("{n} receipt{} scrubbed of the others", if n == 1 { "" } else { "s" }));
                    }
                }
            }
            scrubbed.push(self.scrub_history(Some(last)));
        }
        let scrubbed: Vec<String> = scrubbed.into_iter().filter(|s| !s.is_empty()).collect();
        let text = match (deleted.as_slice(), missing.as_slice()) {
            ([], [m]) => format!("No record {m}."),
            ([], ms) => format!("No records {}.", ms.join(", ")),
            (ds, ms) => {
                let mut t = if ds.len() == 1 { format!("Deleted {}", ds[0]) } else { format!("Deleted {} records", ds.len()) };
                if !scrubbed.is_empty() {
                    t += &format!("; {}", scrubbed.join("; "));
                }
                if !ms.is_empty() {
                    t += &format!("; no record {}", ms.join(", "));
                }
                t + "."
            }
        };
        Ok(json!({"text": text, "deleted": deleted, "missing": missing}))
    }

    /// `{"op": "find", "text": TEXT}`: the records whose text or context
    /// holds TEXT (whatever its case), and `{"op": "find", "ids": [...]}`:
    /// those of the ids that exist; each with a snippet around the match,
    /// for `chloectx forget --everywhere` to show before it forgets them.
    fn op_find(&self, req: &Json) -> Result<Json> {
        let limit = req.get("limit").and_then(|v| v.as_u64()).unwrap_or(1000) as usize;
        let row = |r: &Record, at: Option<(&str, usize)>| {
            let (field, snippet) = match at {
                Some((f, i)) => (f, snippet_at(if f == "ctx" { &r.ctx } else { &r.text }, i, 160)),
                None => ("text", snippet_at(&r.text, 0, 160)),
            };
            json!({"id": r.id, "kind": r.kind.as_str(), "session": r.session, "ts": r.ts, "field": field, "snippet": snippet})
        };
        if let Some(ids) = req.get("ids").and_then(|v| v.as_array()) {
            let ids: Vec<String> = ids.iter().filter_map(|v| v.as_str().map(String::from)).collect();
            let found: Vec<Json> = self.store.get(&ids)?.iter().map(|r| row(r, None)).collect();
            return Ok(json!({"matches": found, "total": found.len()}));
        }
        let needle = req.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
        if needle.chars().count() < 3 {
            return Err(err("give at least 3 characters to look for"));
        }
        let rs = self.store.query(&format!("SELECT id FROM memory LIMIT {}", self.store.len() + 1000))?;
        let ids: Vec<String> = rs.rows.iter().filter_map(|r| r.first().and_then(|v| v.as_str()).map(String::from)).collect();
        let (mut out, mut total) = (Vec::new(), 0usize);
        for chunk in ids.chunks(500) {
            for r in self.store.get(chunk)? {
                let hit = find_ci(&r.text, &needle).map(|i| ("text", i)).or_else(|| find_ci(&r.ctx, &needle).map(|i| ("ctx", i)));
                if let Some(at) = hit {
                    total += 1;
                    if out.len() < limit {
                        out.push(row(&r, Some(at)));
                    }
                }
            }
        }
        Ok(json!({"matches": out, "total": total}))
    }

    /// `{"op": "redact", "dry_run": bool}`, for a store written before
    /// redaction knew a secret's shape: rewrites the records whose text or
    /// context [`crate::redact`] changes, then compacts the store, so that
    /// their old versions leave its segments and its write-ahead log;
    /// redacts the session state and events.log the same way; and, when a
    /// sync is set up, sends the rewritten records again, replacing the
    /// server's copies. Answers how many changed and the kinds of secret
    /// found, never what they were.
    fn op_redact(&self, req: &Json) -> Result<Json> {
        use crate::redact::{count_markers, redact_json, redact_record};
        use std::collections::BTreeMap;
        let dry = req.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
        let mut found = |before: &str, after: &str| {
            let (mut was, mut is) = (BTreeMap::new(), BTreeMap::new());
            count_markers(before, &mut was);
            count_markers(after, &mut is);
            for (k, n) in is {
                let more = n.saturating_sub(was.get(&k).copied().unwrap_or(0));
                if more > 0 {
                    *kinds.entry(k).or_default() += more;
                }
            }
        };

        // The records, in batches by id.
        let rs = self.store.query(&format!("SELECT id FROM memory LIMIT {}", self.store.len() + 1000))?;
        let ids: Vec<String> = rs.rows.iter().filter_map(|r| r.first().and_then(|v| v.as_str()).map(String::from)).collect();
        let mut fixed: Vec<Record> = Vec::new();
        for chunk in ids.chunks(500) {
            let mut batch = Vec::new();
            for mut r in self.store.get(chunk)? {
                let before = format!("{}\n{}", r.text, r.ctx);
                if redact_record(&mut r) {
                    found(&before, &format!("{}\n{}", r.text, r.ctx));
                    batch.push(r);
                }
            }
            if !dry {
                self.store.insert(&batch)?;
            }
            fixed.extend(batch);
        }
        let compacted = if dry || fixed.is_empty() {
            Json::Null
        } else {
            match self.store.db().map(|db| db.optimize()) {
                Some(Ok(_)) => json!(true),
                Some(Err(e)) => json!(format!("the rewritten records are stored, but compacting failed ({e}): their old versions stay in the file until merges reach them")),
                // A cluster's table: the server's merges drop the old versions.
                None => Json::Null,
            }
        };

        // The session state (prompts kept for recall after a compaction).
        let mut sessions = 0;
        {
            let mut st = self.state.lock().unwrap();
            for s in st.sessions.values_mut() {
                let Ok(mut v) = serde_json::to_value(&*s) else { continue };
                let before = v.to_string();
                if redact_json(&mut v) {
                    sessions += 1;
                    found(&before, &v.to_string());
                    if !dry {
                        if let Ok(new) = serde_json::from_value(v) {
                            *s = new;
                        }
                    }
                }
            }
        }
        if sessions > 0 && !dry {
            self.save_state();
        }

        // events.log, line by line, while nothing appends to it.
        let mut log_lines = 0;
        {
            let _one = self.logging.lock().unwrap_or_else(|e| e.into_inner());
            let path = self.paths.dir.join("events.log");
            if let Ok(text) = std::fs::read_to_string(&path) {
                let mut out = String::with_capacity(text.len());
                for line in text.split_inclusive('\n') {
                    let (body, nl) = line.strip_suffix('\n').map_or((line, ""), |b| (b, "\n"));
                    let new = match serde_json::from_str::<Json>(body) {
                        Ok(mut v) => {
                            if redact_json(&mut v) {
                                v.to_string()
                            } else {
                                body.to_string()
                            }
                        }
                        Err(_) => crate::redact::redact(body),
                    };
                    if new != body {
                        log_lines += 1;
                        found(body, &new);
                    }
                    out += &new;
                    out += nl;
                }
                if log_lines > 0 && !dry {
                    let tmp = path.with_extension("log.tmp");
                    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
                    f.write_all(out.as_bytes())?;
                    drop(f);
                    std::fs::rename(&tmp, &path)?;
                }
            }
        }

        // The server's copies.
        let sync = match crate::sync::remote(&crate::paths::home()) {
            Some(remote) if !dry && !fixed.is_empty() => {
                let key = crate::sync::project_key(&crate::paths::home(), &self.project);
                match crate::sync::push_records(&remote, &key, &fixed) {
                    Ok(n) => json!({"server": remote.url, "replaced": n}),
                    Err(e) => json!({"server": remote.url, "error": e.to_string()}),
                }
            }
            _ => Json::Null,
        };
        let old = chloedb::upgrade::old_path(&self.paths.db);
        let old_copy = old.exists().then(|| old.display().to_string());

        let verb = if dry { "would be redacted" } else { "redacted" };
        let what: Vec<String> = kinds.iter().map(|(k, n)| format!("{k} x{n}")).collect();
        let mut text = format!(
            "{} of {} records {verb}{}; {sessions} session states and {log_lines} lines of events.log {verb}.",
            fixed.len(),
            ids.len(),
            if what.is_empty() { String::new() } else { format!(" ({})", what.join(", ")) }
        );
        if let Some(e) = compacted.as_str() {
            text += &format!(" Note: {e}.");
        }
        // The receipts' copies of the text, and the snapshots that hold it.
        if !dry && (!fixed.is_empty() || sessions > 0 || log_lines > 0) {
            let scrubbed = self.scrub_history(None);
            if !scrubbed.is_empty() {
                text += &format!(" Receipts and snapshots: {scrubbed}.");
            }
        }
        match (sync.get("replaced"), sync.get("error")) {
            (Some(n), _) => text += &format!(" Replaced {n} records on {}.", sync["server"].as_str().unwrap_or("")),
            (_, Some(e)) => text += &format!(" Could not replace them on {}: {}.", sync["server"].as_str().unwrap_or(""), e.as_str().unwrap_or("")),
            _ => {}
        }
        if let Some(p) = &old_copy {
            text += &format!(" An older copy of the store, from before its format was upgraded, is at {p}: delete it once you no longer need it.");
        }
        self.log(&json!({"ts": now(), "event": "redact", "dry_run": dry, "records": fixed.len(), "sessions": sessions, "log_lines": log_lines, "kinds": kinds}));
        Ok(json!({
            "records": ids.len(), "changed": fixed.len(), "kinds": kinds, "sessions": sessions, "log_lines": log_lines,
            "dry_run": dry, "compacted": compacted, "sync": sync, "old_copy": old_copy, "text": text,
        }))
    }

    /// Stores drafts for a session (transcript back-fill, tests).
    fn op_append(&self, req: &Json) -> Result<Json> {
        let sid = req.get("session").and_then(|v| v.as_str()).unwrap_or("manual");
        let turn = req.get("turn").and_then(|v| v.as_i64()).unwrap_or(0);
        let ts = req.get("ts").and_then(|v| v.as_i64()).unwrap_or_else(now);
        let drafts: Vec<Draft> = serde_json::from_value(req.get("drafts").cloned().unwrap_or(json!([])))?;
        let recs = self.store.append(sid, turn, ts, drafts)?;
        Ok(json!({"stored": recs.len(), "last_seq": self.store.last_seq()}))
    }

    /// Copies the store to the new directory `dest` while it serves: the
    /// database as a checkpoint (a consistent copy, which copying
    /// `memory.strata` while the daemon writes is not), the session state
    /// and config as they are, and the event log up to its last whole line.
    /// Restoring is putting these files back with the daemon stopped.
    fn op_backup(&self, req: &Json) -> Result<Json> {
        let dest = PathBuf::from(req.get("dest").and_then(|v| v.as_str()).ok_or_else(|| err("backup needs a dest"))?);
        if !dest.is_absolute() {
            return Err(err(format!("{} is not an absolute path", dest.display())));
        }
        std::fs::create_dir(&dest).map_err(|e| err(format!("create {}: {e}", dest.display())))?;
        let db = self.store.db().ok_or_else(|| err("the store is a table of a ChloeDB server"))?;
        let stats = db.storage().checkpoint(&dest.join("memory.strata"), &chloedb::storage::CheckpointOptions::default())?;
        self.persist();
        for name in ["state.json", "config.json"] {
            match std::fs::copy(self.paths.dir.join(name), dest.join(name)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(err(format!("copy {name}: {e}"))),
                _ => {}
            }
        }
        if let Ok(log) = std::fs::read(self.paths.dir.join("events.log")) {
            let whole = log.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
            std::fs::write(dest.join("events.log"), &log[..whole]).map_err(|e| err(format!("write events.log: {e}")))?;
        }
        Ok(json!({"dest": dest, "records": stats.live_docs, "bytes": stats.bytes_written, "txn": stats.txn, "seconds": stats.seconds}))
    }

    /// `{"op": "capsule", "what": "list" | "cut", "title"?, "kinds"?, "since_days"?}`:
    /// the capsules cut from this store (under its `capsules/`), or a new
    /// one cut there from the store as it is now ([`crate::capsule::pack`]).
    fn op_capsule(&self, req: &Json) -> Result<Json> {
        let dir = self.paths.dir.join("capsules");
        if req.get("what").and_then(|v| v.as_str()) == Some("cut") {
            let kinds: Vec<Kind> = match req.get("kinds").and_then(|v| v.as_array()) {
                Some(a) if !a.is_empty() => a.iter().filter_map(|k| k.as_str().and_then(Kind::parse)).collect(),
                _ => crate::capsule::DEFAULT_KINDS.to_vec(),
            };
            if kinds.is_empty() {
                return Err(err("no kind of record to pack"));
            }
            let title = req.get("title").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(String::from);
            let since_ts = req.get("since_days").and_then(|v| v.as_f64()).filter(|d| *d > 0.0).map(|d| now() - (d * 86_400.0) as i64);
            let slug: String = title.as_deref().unwrap_or("capsule").chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect::<String>().split('-').filter(|w| !w.is_empty()).take(6).collect::<Vec<_>>().join("-");
            let out = dir.join(format!("{}-{}", if slug.is_empty() { "capsule".to_string() } else { slug }, crate::recall::fmt_time(now()).replace([' ', ':'], "-").chars().take(16).collect::<String>()));
            crate::paths::create_private_dir(&dir)?;
            let opts = crate::capsule::PackOptions { kinds, since_ts, title, ..Default::default() };
            let m = crate::capsule::pack(&self.store.pinned(), &self.cfg.embedder, &self.project, &out, &opts)?;
            let mut j = serde_json::to_value(&m)?;
            j["dir"] = json!(out);
            return Ok(j);
        }
        let mut list = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Ok(m) = crate::capsule::manifest(&e.path()) {
                    let mut j = serde_json::to_value(&m)?;
                    j["dir"] = json!(e.path());
                    list.push(j);
                }
            }
        }
        list.sort_by_key(|j| std::cmp::Reverse(j["created_ts"].as_i64().unwrap_or(0)));
        Ok(json!({"dir": dir, "capsules": list}))
    }

    fn op_stats(&self) -> Result<Json> {
        let db = self.store.db().ok_or_else(|| err("the store is a table of a ChloeDB server"))?;
        let fs = db.storage().file_stats();
        let kinds = self.store.query("SELECT kind, COUNT(*) FROM memory GROUP BY kind ORDER BY 2 DESC LIMIT 20")?;
        let by_kind: serde_json::Map<String, Json> = kinds.rows.iter().filter_map(|r| Some((r[0].as_str()?.to_string(), json!(r[1].as_i64()?)))).collect();
        // Entity records belong to no session.
        let sessions = self.store.query("SELECT session, COUNT(*) FROM memory WHERE kind != 'entity' GROUP BY session ORDER BY 2 DESC LIMIT 1000")?.rows.len();
        self.save_receipts();
        let (receipt_file, receipt_wal) = self.receipts.get().map_or((0, 0), |b| b.bytes());
        let snaps = self.snapshots.list();
        Ok(json!({
            "records": self.store.len(), "sessions": sessions, "by_kind": by_kind, "last_seq": self.store.last_seq(),
            "embedder": self.store.embedder.name(), "file_bytes": fs.file_bytes, "wal_bytes": fs.wal_bytes,
            "segments": fs.segments.len(), "memtable_docs": fs.memtable_docs, "store": self.paths.db,
            "receipts": {
                "on": self.receipts_on(), "count": self.receipts.get().map_or(0, |b| b.len()),
                "file_bytes": receipt_file, "wal_bytes": receipt_wal, "retention": self.cfg.receipts.retention,
            },
            "snapshots": {
                "automatic": snaps.iter().filter(|k| k.auto).count(), "named": snaps.iter().filter(|k| !k.auto).count(),
                "bytes": self.snapshots.bytes(), "retention": self.cfg.receipts.snapshots,
            },
            "total_bytes": fs.file_bytes + fs.wal_bytes + receipt_file + receipt_wal + self.snapshots.bytes(),
            // The model CLI as this daemon resolves it (its PATH is the one
            // of whichever process started it), for doctor (CTX-3).
            "claude": match crate::llm::resolve_cli(&self.distill_cfg().command) {
                Ok(p) => json!({"path": p}),
                Err(e) => json!({"error": e}),
            },
            "distill": self.distill_cfg().active(),
        }))
    }
}

/// The byte offset in `hay` where `needle` (already lowercase) starts,
/// whatever the case of `hay`.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    if hay.is_ascii() {
        return hay.to_ascii_lowercase().find(needle);
    }
    // Lowercasing can change a character's length: each lowercase
    // character keeps the offset of the one it came from.
    let mut low = String::with_capacity(hay.len());
    let mut from = Vec::with_capacity(hay.len());
    for (i, c) in hay.char_indices() {
        for l in c.to_lowercase() {
            from.push((low.len(), i));
            low.push(l);
        }
    }
    let at = low.find(needle)?;
    from.iter().rev().find(|(lo, _)| *lo <= at).map(|(_, i)| *i)
}

/// About `width` characters of `text` around byte offset `at`, on one line.
fn snippet_at(text: &str, at: usize, width: usize) -> String {
    let at = (0..=at.min(text.len())).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    let before: Vec<char> = text[..at].chars().rev().take(width / 3).collect();
    let start = at - before.iter().map(|c| c.len_utf8()).sum::<usize>();
    let body: String = text[start..].chars().take(width).collect();
    let mut s = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        s = format!("…{s}");
    }
    if start + body.len() < text.len() {
        s.push('…');
    }
    s
}

/// One hybrid search of `store`, as `memory_search` shows it: the first
/// `limit` hits with a snippet each, their ids, and the search's evidence.
pub fn search_text(store: &Store, q: &str, f: &Filter, rank: &RankParams, limit: usize) -> Result<(String, Vec<String>, Json)> {
    let (hits, ev) = recall::search(store, q, f, rank)?;
    let terms: HashSet<String> = chloedb::text::analyze_query(q).into_iter().collect();
    let mut text = String::new();
    for h in hits.iter().take(limit) {
        text += &recall::fmt_record(&h.record, &recall::snippet(&h.record.text, &terms, 700), now());
        text += &format!("\n(score {:.3})\n\n", h.score);
    }
    if text.is_empty() {
        text = "No matching records.".into();
    }
    let ids = hits.iter().take(limit).map(|h| h.record.id.clone()).collect();
    Ok((text, ids, serde_json::to_value(ev).unwrap_or(Json::Null)))
}

/// The records just before and after record `id` in its session, with
/// their text, as `memory_timeline` shows them.
pub fn timeline_text(store: &Store, id: &str, before: usize, after: usize) -> Result<String> {
    let store = store.pinned();
    let rec = store.get(&[id.to_string()])?.pop().ok_or_else(|| err(format!("no record {id}")))?;
    let near = store.range(&rec.session, rec.seq - 200, rec.seq + 200)?;
    let at = near.iter().position(|r| r.id == rec.id).unwrap_or(0);
    let lo = at.saturating_sub(before);
    let hi = (at + after + 1).min(near.len());
    let mut text = String::new();
    for r in &near[lo..hi] {
        let body: String = r.text.chars().take(1500).collect();
        text += &format!("{}{}\n\n", if r.id == rec.id { ">> " } else { "" }, recall::fmt_record(r, &body, now()));
    }
    Ok(text)
}

/// A result set as `memory_sql` shows it: a header, at most 100 rows of
/// cells cut to 400 characters, and the row count.
pub fn rows_text(rs: &chloedb::query::exec::ResultSet) -> String {
    let mut text = rs.columns.join(" | ") + "\n";
    for row in rs.rows.iter().take(100) {
        let cells: Vec<String> = row
            .iter()
            .map(|v| match v {
                chloedb::Value::Null => "NULL".into(),
                chloedb::Value::Int(x) => x.to_string(),
                chloedb::Value::Float(x) => format!("{x:.4}"),
                chloedb::Value::Str(s) => {
                    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
                    if one.chars().count() > 400 {
                        format!("{}…", one.chars().take(400).collect::<String>())
                    } else {
                        one
                    }
                }
                chloedb::Value::Vector(v) => format!("vector({})", v.len()),
                chloedb::Value::List(xs) => format!("list({})", xs.len()),
                chloedb::Value::Bool(b) => b.to_string(),
                chloedb::Value::Timestamp(t) => chloedb::timestamp::format(*t),
            })
            .collect();
        text += &cells.join(" | ");
        text += "\n";
    }
    text += &format!("({} rows)", rs.rows.len());
    text
}

// ---------------------------------------------------------------- server

/// Runs the daemon in this process until it is idle or shut down.
pub fn serve(dir: &Path, project: &str) -> Result<()> {
    let paths = StorePaths::new(dir);
    crate::paths::create_private_dir(dir)?;
    if UnixStream::connect(&paths.socket).is_ok() {
        return Err(err(format!("a daemon already listens on {}", paths.socket.display())));
    }
    let _ = std::fs::remove_file(&paths.socket);
    let daemon = Arc::new(Daemon::open(dir, project)?);
    // Which project this store records, for `chloectx ui`.
    if !project.is_empty() {
        let _ = std::fs::write(dir.join("project.txt"), format!("{project}\n"));
    }
    let listener = UnixListener::bind(&paths.socket)?;
    eprintln!("chloectx: serving {} on {} ({} records, embedder {})", project, paths.socket.display(), daemon.store.len(), daemon.store.embedder.name());
    // Retention of receipts and snapshots, beside the first requests.
    {
        let d = daemon.clone();
        std::thread::spawn(move || d.housekeep());
    }
    // Rules a store took from a harness's envelopes before they were
    // cleaned off prompts: retired once, off the hooks' path.
    {
        let d = daemon.clone();
        std::thread::spawn(move || {
            if let Some(n) = crate::envelope::retire_once(&d.paths.dir, &d.store).filter(|n| *n > 0) {
                d.log(&json!({"ts": now(), "event": "envelope_rules_retired", "count": n}));
            }
        });
    }
    // The sync to a server, when one is set up: what this store lacks on
    // start, then what it records, every PUSH_EVERY.
    if crate::sync::remote(&crate::paths::home()).is_some() {
        let d = daemon.clone();
        std::thread::spawn(move || {
            d.sync_now(true);
            loop {
                std::thread::sleep(crate::sync::PUSH_EVERY);
                d.sync_now(false);
            }
        });
    }
    {
        let d = daemon.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            if now() - d.last_activity.load(Ordering::Relaxed) > d.cfg.idle_secs as i64 {
                eprintln!("chloectx: idle, exiting");
                d.shutdown();
            }
        });
    }
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let d = daemon.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                return;
            }
            let req: Json = serde_json::from_str(&line).unwrap_or(Json::Null);
            let (resp, deferred) = d.handle(&req);
            let mut w = stream;
            let _ = writeln!(w, "{resp}");
            let _ = w.flush();
            drop(w);
            d.persist();
            if let Some(f) = deferred {
                f();
            }
        });
    }
    Ok(())
}

// ---------------------------------------------------------------- client

fn try_request(socket: &Path, req: &Json, deadline: Instant) -> std::io::Result<Json> {
    let stream = UnixStream::connect(socket)?;
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))?;
    let mut w = stream.try_clone()?;
    writeln!(w, "{req}")?;
    w.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|e| std::io::Error::other(e.to_string()))
}

/// Sends one request to the project's daemon, starting it if needed.
pub fn request(dir: &Path, project: &str, req: &Json, timeout: Duration) -> Result<Json> {
    let paths = StorePaths::new(dir);
    let deadline = Instant::now() + timeout;
    match try_request(&paths.socket, req, deadline) {
        Ok(r) => return Ok(r),
        // A daemon that died as the request reached it (CTX-16): a new one
        // is started, as for one that is not there.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe) => {
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(e) if !matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return Err(err(format!("daemon: {e}"))),
        Err(_) => {}
    }
    spawn(&paths, project)?;
    loop {
        match try_request(&paths.socket, req, deadline) {
            Ok(r) => return Ok(r),
            Err(e) if Instant::now() >= deadline => return Err(err(format!("daemon did not answer in time: {e}"))),
            Err(_) => std::thread::sleep(Duration::from_millis(15)),
        }
    }
}

/// A request to the store's daemon only when one runs: never starts one
/// (and so never creates a store).
pub fn request_running(dir: &Path, req: &Json, timeout: Duration) -> Result<Json> {
    try_request(&StorePaths::new(dir).socket, req, Instant::now() + timeout).map_err(|e| err(format!("daemon: {e}")))
}

/// Rewrites the store's segments that hold deleted records, without them
/// (after a forget): the memtable is flushed, each segment with a deleted
/// row is merged alone, and the space freed is returned to the file system.
/// Segments with no deleted row are left alone, unlike `optimize`, which
/// merges the whole table. The pages the old segments were on are free but
/// keep their bytes until a later write reuses them; `chloectx redact`
/// rewrites the whole file for that.
///
/// The store's background merger is woken by the flush and may claim the
/// segment that held the record; a merge here leaves the segments another
/// one holds alone, and until that merge is installed the old segment,
/// record and all, is the one a checkpoint copies. So this returns only
/// once no merge is running and none is left to do: a merge claimed after
/// the delete rebuilds the segment from its live rows, without the record.
pub fn compact_deleted(db: &chloedb::db::Db) -> Result<chloedb::storage::MergeStats> {
    let wait = Duration::from_secs(60);
    let s = db.storage();
    s.flush()?;
    let policy = chloedb::storage::MergePolicy { merge_factor: 1 << 40, max_deleted_ratio: 0.0, ..Default::default() };
    let deadline = Instant::now() + wait;
    let idle = || -> Result<()> {
        while s.merge_status().running > 0 {
            if Instant::now() >= deadline {
                return Err(err(format!("a background merge still holds the store's segments after {} s", wait.as_secs())));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    };
    let mut stats = chloedb::storage::MergeStats::default();
    loop {
        idle()?;
        let m = s.merge(&policy)?;
        let done = m.merges == 0;
        stats.merges += m.merges;
        stats.segments_in += m.segments_in;
        stats.docs_merged += m.docs_merged;
        stats.docs_dropped += m.docs_dropped;
        stats.bytes_written += m.bytes_written;
        stats.seconds += m.seconds;
        // Nothing left to merge, and no merge started meanwhile on a
        // segment this one then skipped.
        if done && s.merge_status().running == 0 {
            break;
        }
    }
    s.vacuum()?;
    Ok(stats)
}

/// Replaces the store's file with a fresh copy of its live rows, after a
/// `redact`: the versions it replaced are gone from every segment, but the
/// pages they were on stay in the file, free, until later writes reuse
/// them, and this copy has no free pages. It needs the file to itself: it
/// holds the lock that clients take to start a daemon (so a hook that
/// comes meanwhile finds no daemon and records nothing, for the few
/// seconds this takes), stops the project's daemon, waits up to `wait`
/// for it to let go of the file, and opens it here. The next request
/// starts a daemon on the copy. Returns the file's size before and after.
pub fn rewrite_file(dir: &Path, wait: Duration) -> Result<Json> {
    use std::os::unix::io::AsRawFd;
    let paths = StorePaths::new(dir);
    if !paths.db.exists() {
        return Err(err(format!("no store at {}", paths.db.display())));
    }
    let deadline = Instant::now() + wait;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(&paths.spawn_lock)?;
    // SAFETY: flock on an open descriptor.
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if Instant::now() >= deadline {
            return Err(err("a daemon is being started; try again"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if UnixStream::connect(&paths.socket).is_ok() {
        let _ = try_request(&paths.socket, &json!({"op": "shutdown"}), Instant::now() + Duration::from_secs(10));
    }
    let db = loop {
        match chloedb::db::Db::open(&paths.db, chloedb::storage::DbOptions::default()) {
            Ok(db) => break db,
            Err(chloedb::Error::Busy(_)) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(err(format!("open {}: {e}", paths.db.display()))),
        }
    };
    let before = std::fs::metadata(&paths.db).map_or(0, |m| m.len());
    // Without what merges dropped, then copied: the copy holds only live data.
    db.optimize()?;
    let copy = dir.join("memory.strata.redacted");
    let _ = std::fs::remove_file(&copy);
    let stats = db.storage().checkpoint(&copy, &chloedb::storage::CheckpointOptions::default())?;
    let live = db.snapshot().n_live_docs();
    drop(db);
    if stats.live_docs != live {
        let _ = std::fs::remove_file(&copy);
        return Err(err(format!("the copy holds {} records, the store {live}: the store was left as it was", stats.live_docs)));
    }
    std::fs::rename(&copy, &paths.db)?;
    // Empty since the checkpoint flushed; the copy starts its own.
    match std::fs::remove_file(dir.join("memory.strata-wal")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    drop(lock);
    let after = std::fs::metadata(&paths.db).map_or(0, |m| m.len());
    Ok(json!({"records": live, "bytes_before": before, "bytes_after": after}))
}

/// Starts `chloectx serve` in the background unless one is starting already.
fn spawn(paths: &StorePaths, project: &str) -> Result<()> {
    crate::paths::create_private_dir(&paths.dir)?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(&paths.spawn_lock)?;
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock on an open descriptor.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(()); // another client is spawning it
    }
    if UnixStream::connect(&paths.socket).is_ok() {
        return Ok(());
    }
    let exe: PathBuf = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&paths.log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["serve", "--dir"]).arg(&paths.dir).arg("--project").arg(project);
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid is async-signal-safe; it detaches the daemon from the hook's session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    // Hold the lock until the socket appears (or a short while), so other
    // clients wait instead of spawning a second daemon.
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until && UnixStream::connect(&paths.socket).is_err() {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
