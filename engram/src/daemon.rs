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
    pub sync: bool,
    /// Knowledge written down by a small model after each turn.
    pub distill: crate::distill::DistillConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            embedder: std::env::var("ENGRAM_EMBEDDER").unwrap_or_else(|_| "static".into()),
            idle_secs: 1800,
            rank: RankParams::default(),
            pack: PackParams::default(),
            gate: Gate::default(),
            briefing_chars: 6000,
            knowledge: Default::default(),
            file_notes: true,
            sync: true,
            distill: Default::default(),
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
    /// Distillations run one at a time, so each sees what the last wrote.
    distilling: Mutex<()>,
    last_activity: AtomicI64,
}

/// Work to do after the response is sent (so hooks return at once).
type Deferred = Box<dyn FnOnce() + Send>;

impl Daemon {
    pub fn open(dir: &Path, project: &str) -> Result<Daemon> {
        let paths = StorePaths::new(dir);
        crate::paths::create_private_dir(dir)?;
        let cfg = Config::load(&paths.config);
        let embedder: Arc<dyn embed::Embedder> = Arc::from(embed::open(&cfg.embedder)?);
        let store = Arc::new(Store::open(&paths.db, embedder, StoreOptions { sync: cfg.sync, ..Default::default() })?);
        let state: State = std::fs::read_to_string(&paths.state).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Ok(Daemon { paths, project: project.to_string(), store, cfg, state: Mutex::new(state), dirty: Default::default(), pending: Default::default(),
            distilling: Mutex::new(()), last_activity: AtomicI64::new(now()) })
    }

    fn save_state(&self, st: &State) {
        let mut st2 = State { sessions: st.sessions.clone(), active: st.active.clone() };
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
            let st = self.state.lock().unwrap();
            self.save_state(&st);
        }
    }

    fn log(&self, line: &Json) {
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
            "hook" => self.hook(req.get("event").and_then(|v| v.as_str()).unwrap_or(""), req.get("payload").unwrap_or(&Json::Null)),
            "search" => self.op_search(req).map(|r| (r, None)),
            "timeline" => self.op_timeline(req).map(|r| (r, None)),
            "sql" => self.op_sql(req).map(|r| (r, None)),
            "remember" => self.op_remember(req).map(|r| (r, None)),
            "forget" => self.op_forget(req).map(|r| (r, None)),
            "append" => self.op_append(req).map(|r| (r, None)),
            "stats" => self.op_stats().map(|r| (r, None)),
            // Read-only views for the demo inspector (demo/server.py).
            "inspect_query" => crate::inspect::query(&self.store, req).map(|r| (r, None)),
            "inspect_layout" => crate::inspect::layout(&self.store).map(|r| (r, None)),
            "inspect_embed" => crate::inspect::embed(&self.store, req).map(|r| (r, None)),
            "distill_backfill" => self.distill_backfill().map(|(t, n)| (json!({"turns": t, "items": n}), None)),
            "briefing" => {
                let sid = req.get("session").and_then(|v| v.as_str()).unwrap_or("");
                recall::briefing(&self.store, sid, self.cfg.briefing_chars, now()).map(|(t, _)| (json!({"text": t}), None))
            }
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
        while self.pending.load(Ordering::Acquire) > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = self.store.flush();
        let st = self.state.lock().unwrap();
        self.save_state(&st);
        let _ = std::fs::remove_file(&self.paths.socket);
        std::process::exit(0);
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
                let (mut text, shown) = recall::briefing(&self.store, &sid, self.cfg.briefing_chars, now())?;
                self.with_session(&sid, |s| s.injected.extend(shown));
                if source == "compact" && !last_prompts.is_empty() {
                    let q: Vec<String> = last_prompts.iter().rev().take(2).cloned().collect();
                    if let Some(r) = self.recall_for(&sid, &q.join("\n"), true)? {
                        text += "\n\nRecords relevant to the work in progress, which the compaction summary may have left out:\n";
                        text += &r;
                    }
                }
                self.log(&json!({"ts": now(), "event": event, "session": sid, "source": source, "chars": text.len()}));
                Ok((ctx_out("SessionStart", text), None))
            }
            "UserPromptSubmit" => {
                let prompt = p.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let turn = self.with_session(&sid, |s| {
                    s.turn += 1;
                    s.episode = EpisodeAcc { prompt: prompt.clone(), ..Default::default() };
                    s.episode_stored = false;
                    s.last_prompts.push_back(prompt.chars().take(1000).collect());
                    while s.last_prompts.len() > 4 {
                        s.last_prompts.pop_front();
                    }
                    s.turn
                });
                // Recall first: the prompt is in the live window anyway, and
                // storing it afterwards keeps the write (and the memtable
                // rebuild it causes) off the prompt's critical path.
                let text = self.recall_for(&sid, &prompt, false)?.unwrap_or_default();
                let me = self.clone();
                Ok((
                    ctx_out("UserPromptSubmit", text),
                    Some(Box::new(move || {
                        if let Err(e) = me.store.append(&sid, turn, now(), capture::prompt_drafts(&prompt)) {
                            me.log(&json!({"ts": now(), "error": e.to_string(), "event": "UserPromptSubmit"}));
                        }
                        me.store.warm();
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
                let text = self.file_notes(&sid, &rel)?;
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
                let (turn, ctx) = self.with_session(&sid, |s| {
                    s.episode.add_tool(&tool, &input, &resp, &project);
                    (s.turn, format!("Turn {} request: {}", s.turn, s.episode.prompt.chars().take(200).collect::<String>()))
                });
                let me = self.clone();
                Ok((
                    json!({"output": null}),
                    Some(Box::new(move || {
                        let drafts: Vec<Draft> = capture::tool_drafts(&tool, &input, &resp, &project).into_iter().map(|d| d.ctx(ctx.clone())).collect();
                        if let Err(e) = me.store.append(&sid, turn, now(), drafts) {
                            me.log(&json!({"ts": now(), "error": e.to_string(), "event": "PostToolUse"}));
                        }
                        me.store.warm();
                    })),
                ))
            }
            "Stop" => {
                let answer = p.get("last_assistant_message").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let (turn, ep, first) = self.with_session(&sid, |s| {
                    let first = !s.episode_stored;
                    s.episode_stored = true;
                    (s.turn, s.episode.clone(), first)
                });
                if !first {
                    return Ok((json!({"output": null}), None));
                }
                let me = self.clone();
                let tpath = p.get("transcript_path").and_then(|v| v.as_str()).map(String::from);
                Ok((
                    json!({"output": null}),
                    Some(Box::new(move || {
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
                        me.distill_turn(&sid, turn, now(), &ep.prompt, &answer, &ep.commands);
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
                let me = self.clone();
                let tpath = p.get("transcript_path").and_then(|v| v.as_str()).map(String::from);
                let (turn, prompt) = self.with_session(&sid, |s| (s.turn, s.episode.prompt.clone()));
                Ok((
                    json!({"output": null}),
                    Some(Box::new(move || {
                        if let Some(t) = &tpath {
                            let ctx = format!("Turn {turn} request: {}", prompt.chars().take(200).collect::<String>());
                            me.sync_narration(&sid, turn, t, "", &ctx);
                        }
                    })),
                ))
            }
            "SessionEnd" => {
                let me = self.clone();
                Ok((json!({"output": null}), Some(Box::new(move || {
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
    pub fn distill_turn(&self, sid: &str, turn: i64, ts: i64, prompt: &str, answer: &str, commands: &[String]) {
        if !self.cfg.distill.active() || (prompt.trim().is_empty() && answer.trim().is_empty()) {
            return;
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        let _one = self.distilling.lock().unwrap_or_else(|e| e.into_inner());
        let known: Vec<Record> = self
            .store
            .latest(&recall::KNOWLEDGE, 80, &Filter::default())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.tag != "superseded" && r.tag != "done" && (r.kind != Kind::Todo || r.tag == "planned"))
            .take(40)
            .collect();
        let listed: Vec<(String, Kind, String)> = known.iter().rev().map(|r| (r.id.clone(), r.kind, r.text.clone())).collect();
        let t0 = Instant::now();
        match crate::distill::distill(&self.cfg.distill, prompt, answer, commands, &listed) {
            Ok(d) => {
                let is_known = |id: &String| known.iter().any(|r| &r.id == id);
                let replaced: Vec<String> = d.replaced.into_iter().filter(|id| is_known(id)).collect();
                let done: Vec<String> = d.done.into_iter().filter(|id| known.iter().any(|r| &r.id == id && r.kind == Kind::Todo)).collect();
                // Near-copies of what is noted (other than what they replace) add nothing.
                let terms = |t: &str| -> HashSet<String> { strata::text::analyze(t).into_iter().collect() };
                let mut seen: Vec<HashSet<String>> = known.iter().filter(|r| !replaced.contains(&r.id)).map(|r| terms(&r.text)).collect();
                let ctx = format!("Turn {turn} request: {}", prompt.chars().take(200).collect::<String>());
                // Each note links to the turn it was written from.
                let source = self
                    .store
                    .query(&format!("SELECT id FROM memory WHERE session = '{}' AND turn = {turn} AND kind = 'prompt' ORDER BY seq ASC LIMIT 1", sid.replace('\'', "''")))
                    .ok()
                    .and_then(|rs| rs.rows.first().and_then(|r| r[0].as_str().map(String::from)));
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
                        let mut x = x.ctx(ctx.clone());
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
                    "replaced": replaced, "done": done, "cost_usd": d.cost_usd, "ms": t0.elapsed().as_millis() as u64}));
            }
            Err(e) => self.log(&json!({"ts": now(), "event": "distill", "session": sid, "turn": turn, "error": e.to_string()})),
        }
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }

    /// Distills every past turn of the store that has a prompt (for stores
    /// recorded before distillation existed). Returns (turns, items).
    fn distill_backfill(&self) -> Result<(usize, usize)> {
        let rs = self.store.query(&format!("SELECT {} FROM memory WHERE kind IN ('prompt', 'answer', 'episode') ORDER BY seq ASC LIMIT 1000000", crate::record::COLUMNS))?;
        let recs: Vec<Record> = rs.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
        // (session, turn) -> (prompt, answer, commands)
        let mut turns: Vec<((String, i64), (String, String, Vec<String>, i64))> = Vec::new();
        for r in recs {
            let key = (r.session.clone(), r.turn);
            if turns.last().map_or(true, |(k, _)| *k != key) {
                turns.push((key.clone(), Default::default()));
            }
            let (_, (p, a, c, ts)) = turns.last_mut().unwrap();
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
                }
                _ => {}
            }
        }
        let before = self.store.len();
        let n = turns.len();
        for ((sid, turn), (p, a, c, ts)) in turns {
            if !p.trim().is_empty() {
                self.distill_turn(&sid, turn, ts, p.trim(), a.trim(), &c);
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
    fn recall_for(&self, sid: &str, prompt: &str, force: bool) -> Result<Option<String>> {
        let (w, mut skip) = {
            let st = self.state.lock().unwrap();
            let s = st.sessions.get(sid).cloned().unwrap_or_default();
            (s.window_start, s.injected)
        };
        let query: String = prompt.chars().take(2000).collect();
        let filter = Filter { exclude_window: Some((sid.to_string(), w)), ..Default::default() };
        let t0 = Instant::now();
        // Knowledge that matches the prompt on its own text is shown even
        // when the gate holds back the rest: a request that does not sound
        // like earlier work can still be bound by what was agreed.
        // The two searches are independent (only packing needs both), so
        // they run side by side.
        let (known, searched) = std::thread::scope(|sc| {
            let k = sc.spawn(|| if self.cfg.knowledge.enabled { recall::knowledge(&self.store, &query, &filter, &skip, &self.cfg.knowledge) } else { Ok(Vec::new()) });
            let m = recall::search(&self.store, &query, &filter, &self.cfg.rank);
            (k.join().unwrap_or_else(|_| Err(crate::err("knowledge search panicked"))), m)
        });
        let known = known?;
        let (hits, ev) = searched?;
        let mut ids: Vec<String> = known.iter().map(|(r, _, _)| r.id.clone()).collect();
        skip.extend(ids.iter().cloned());
        let g = self.cfg.gate.score(&ev);
        let decision = if force && !hits.is_empty() { Decision::Inject } else { self.cfg.gate.decide(&ev) };
        let mut parts = Vec::new();
        if !known.is_empty() {
            parts.push(recall::fmt_knowledge(&known, now()));
        }
        match decision {
            _ if hits.is_empty() => {}
            Decision::Inject | Decision::Brief => {
                let weak = decision == Decision::Brief;
                let params = if weak { PackParams::brief() } else { self.cfg.pack.clone() };
                let items = recall::pack(&hits, &query, &skip, &params);
                if !items.is_empty() {
                    let packed: Vec<String> = items.iter().map(|(r, _)| r.id.clone()).collect();
                    let shown: HashSet<String> = packed.iter().cloned().chain(skip.iter().cloned()).collect();
                    let mut t = recall::fmt_recall_as(&items, now(), weak);
                    t += &recall::fmt_more(&hits, &shown, if weak { 5 } else { 3 });
                    parts.push(t);
                    ids.extend(packed);
                }
            }
            Decision::Skip => {}
        }
        let text = if parts.is_empty() { None } else { Some(parts.join("\n")) };
        if !ids.is_empty() {
            let mut st = self.state.lock().unwrap();
            if let Some(s) = st.sessions.get_mut(sid) {
                s.injected.extend(ids.iter().cloned());
            }
            self.dirty.store(true, Ordering::Release);
        }
        let known: Vec<Json> = known.iter().map(|(r, c, v)| json!({"id": r.id, "cos": c, "cov": v})).collect();
        self.log(&json!({
            "ts": now(), "event": "recall", "session": sid, "decision": format!("{decision:?}"), "gate": g,
            "evidence": ev, "candidates": hits.len(), "injected": ids, "knowledge": known, "chars": text.as_ref().map_or(0, |t| t.len()),
            "ms": t0.elapsed().as_secs_f64() * 1e3,
        }));
        Ok(text)
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
        let terms: Vec<String> = strata::text::analyze(rel);
        if terms.is_empty() {
            return Ok(String::new());
        }
        let m = terms.iter().map(|t| t.replace('\'', "")).collect::<Vec<_>>().join(" AND ");
        let filter = Filter { exclude_window: Some((sid.to_string(), w)), ..Default::default() };
        let mut where_ = filter_sql(&filter);
        where_.push(format!("MATCH(files, '{m}')"));
        where_.push("kind IN ('error', 'decision', 'preference', 'fact', 'note', 'episode')".into());
        let rs = self.store.query(&format!("SELECT {} FROM memory WHERE {} ORDER BY seq DESC LIMIT 3", crate::record::COLUMNS, where_.join(" AND ")))?;
        let recs: Vec<Record> = rs.rows.iter().filter_map(|r| crate::store::row_record(r)).collect();
        if recs.is_empty() {
            return Ok(String::new());
        }
        let mut s = format!("Engram notes on {rel} from earlier work (newest first):\n");
        for r in recs.iter().take(2) {
            let snip: String = r.text.chars().take(500).collect();
            s += &format!("\n{}\n", recall::fmt_record(r, &snip, now()));
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
            f.exclude_window = Some((active, w));
        }
        if let Some(days) = req.get("since_days").and_then(|v| v.as_f64()) {
            f.since_ts = Some(now() - (days * 86_400.0) as i64);
        }
        if let Some(s) = req.get("session").and_then(|v| v.as_str()) {
            f.session = Some(s.to_string());
        }
        let (hits, ev) = recall::search(&self.store, q, &f, &self.cfg.rank)?;
        let terms: HashSet<String> = strata::text::analyze_query(q).into_iter().collect();
        let mut text = String::new();
        for h in hits.iter().take(limit) {
            text += &recall::fmt_record(&h.record, &recall::snippet(&h.record.text, &terms, 700), now());
            text += &format!("\n(score {:.3})\n\n", h.score);
        }
        if text.is_empty() {
            text = "No matching records.".into();
        }
        Ok(json!({"text": text, "evidence": ev, "ids": hits.iter().take(limit).map(|h| h.record.id.clone()).collect::<Vec<_>>()}))
    }

    fn op_timeline(&self, req: &Json) -> Result<Json> {
        let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let before = req.get("before").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
        let after = req.get("after").and_then(|v| v.as_u64()).unwrap_or(8) as usize;
        let rec = self.store.get(&[id.to_string()])?.pop().ok_or_else(|| err(format!("no record {id}")))?;
        let near = self.store.range(&rec.session, rec.seq - 200, rec.seq + 200)?;
        let at = near.iter().position(|r| r.id == rec.id).unwrap_or(0);
        let lo = at.saturating_sub(before);
        let hi = (at + after + 1).min(near.len());
        let mut text = String::new();
        for r in &near[lo..hi] {
            let body: String = r.text.chars().take(1500).collect();
            text += &format!("{}{}\n\n", if r.id == rec.id { ">> " } else { "" }, recall::fmt_record(r, &body, now()));
        }
        Ok(json!({"text": text}))
    }

    fn op_sql(&self, req: &Json) -> Result<Json> {
        let sql = req.get("sql").and_then(|v| v.as_str()).unwrap_or("").trim();
        if !sql.get(..6).map_or(false, |s| s.eq_ignore_ascii_case("select")) {
            return Err(err("only SELECT statements over the memory table are allowed"));
        }
        let rs = self.store.query(sql)?;
        let mut text = rs.columns.join(" | ") + "\n";
        for row in rs.rows.iter().take(100) {
            let cells: Vec<String> = row
                .iter()
                .map(|v| match v {
                    strata::Value::Null => "NULL".into(),
                    strata::Value::Int(x) => x.to_string(),
                    strata::Value::Float(x) => format!("{x:.4}"),
                    strata::Value::Str(s) => {
                        let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
                        if one.chars().count() > 400 {
                            format!("{}…", one.chars().take(400).collect::<String>())
                        } else {
                            one
                        }
                    }
                    strata::Value::Vector(v) => format!("vector({})", v.len()),
                })
                .collect();
            text += &cells.join(" | ");
            text += "\n";
        }
        text += &format!("({} rows)", rs.rows.len());
        Ok(json!({"text": text}))
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
        if let Some(old) = req.get("supersedes").and_then(|v| v.as_str()) {
            if let Some(mut r) = self.store.get(&[old.to_string()])?.pop() {
                r.tag = "superseded".into();
                r.importance *= 0.3;
                self.store.insert(&[r])?;
            }
        }
        let recs = self.store.append(&sid, turn, now(), vec![Draft::new(kind, text).files(files).tag("remembered")])?;
        Ok(json!({"id": recs[0].id, "text": format!("Stored {} as {}.", kind.as_str(), recs[0].id)}))
    }

    fn op_forget(&self, req: &Json) -> Result<Json> {
        let id = req.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let found = self.store.delete(id)?;
        Ok(json!({"text": if found { format!("Deleted {id}.") } else { format!("No record {id}.") }}))
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

    fn op_stats(&self) -> Result<Json> {
        let fs = self.store.db.storage().file_stats();
        let kinds = self.store.query("SELECT kind, COUNT(*) FROM memory GROUP BY kind ORDER BY 2 DESC LIMIT 20")?;
        let by_kind: serde_json::Map<String, Json> = kinds.rows.iter().filter_map(|r| Some((r[0].as_str()?.to_string(), json!(r[1].as_i64()?)))).collect();
        // Entity records belong to no session.
        let sessions = self.store.query("SELECT session, COUNT(*) FROM memory WHERE kind != 'entity' GROUP BY session ORDER BY 2 DESC LIMIT 1000")?.rows.len();
        Ok(json!({
            "records": self.store.len(), "sessions": sessions, "by_kind": by_kind, "last_seq": self.store.last_seq(),
            "embedder": self.store.embedder.name(), "file_bytes": fs.file_bytes, "wal_bytes": fs.wal_bytes,
            "segments": fs.segments.len(), "memtable_docs": fs.memtable_docs, "store": self.paths.db,
        }))
    }
}

fn filter_sql(f: &Filter) -> Vec<String> {
    let mut v = Vec::new();
    if let Some((s, w)) = &f.exclude_window {
        v.push(format!("NOT (session = '{}' AND seq >= {w})", s.replace('\'', "''")));
    }
    v
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
    let listener = UnixListener::bind(&paths.socket)?;
    eprintln!("engram: serving {} on {} ({} records, embedder {})", project, paths.socket.display(), daemon.store.len(), daemon.store.embedder.name());
    {
        let d = daemon.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            if now() - d.last_activity.load(Ordering::Relaxed) > d.cfg.idle_secs as i64 {
                eprintln!("engram: idle, exiting");
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

/// Starts `engram serve` in the background unless one is starting already.
fn spawn(paths: &StorePaths, project: &str) -> Result<()> {
    crate::paths::create_private_dir(&paths.dir)?;
    let lock = std::fs::OpenOptions::new().create(true).write(true).mode(0o600).open(&paths.spawn_lock)?;
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
