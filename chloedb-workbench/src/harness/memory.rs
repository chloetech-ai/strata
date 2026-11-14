//! The memory part of the harness (HARNESS.md, "Editor: Memory"): ChloeCtx's
//! recall settings and pins, ChloeCtx's receipts of the window's recalls,
//! every recall recomputed under a draft, and the memory a session missed
//! at the turn it mattered.
//!
//! A receipt records the recall's gate score G, the thresholds it was
//! compared with, and every candidate with its score, tokens and decision
//! (`chloectx receipt <id> --json`). So a change of the thresholds is
//! exact on the decision (G against the new `hi` and `lo`); which records a
//! new decision packs is estimated from the candidates in rank order, and
//! said so. A pin is exact under Workbench's delivery: the pinned records
//! are given at the start of every session of the version.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::supervisor::Supervisor;
use crate::trail::{self, Ran};

/// A record given whenever an agent of the version starts, for a file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pin {
    pub file: String,
    pub record: String,
    pub text: String,
}

/// The memory part: ChloeCtx's recall settings (`None`: ChloeCtx's own, as
/// its receipts record them) and the pins.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemEdits {
    /// The gate's score from which a recall is given in full, and in brief.
    pub gate_hi: Option<f64>,
    pub gate_lo: Option<f64>,
    /// A knowledge record (decision, rule, fact) qualifies at this cosine.
    pub knowledge_min_cos: Option<f64>,
    /// Characters a recall may give, and the briefing at session start.
    pub budget_chars: Option<usize>,
    pub briefing_chars: Option<usize>,
    pub pins: Vec<Pin>,
}

impl MemEdits {
    pub fn settings_set(&self) -> bool {
        self.gate_hi.is_some() || self.gate_lo.is_some() || self.knowledge_min_cos.is_some() || self.budget_chars.is_some() || self.briefing_chars.is_some()
    }
    /// The settings as the session's environment carries them.
    pub fn env_json(&self) -> Json {
        let mut m = serde_json::Map::new();
        for (k, v) in [("gate_hi", self.gate_hi.map(|x| json!(x))), ("gate_lo", self.gate_lo.map(|x| json!(x))), ("knowledge_min_cos", self.knowledge_min_cos.map(|x| json!(x))), ("budget_chars", self.budget_chars.map(|x| json!(x))), ("briefing_chars", self.briefing_chars.map(|x| json!(x)))] {
            if let Some(v) = v {
                m.insert(k.into(), v);
            }
        }
        Json::Object(m)
    }
    /// What a session of the version is told at its start of its pins.
    pub fn pins_text(&self) -> Option<String> {
        if self.pins.is_empty() {
            return None;
        }
        let mut s = String::from("Decisions the user pinned for this repository (from Workbench's harness): follow them when you work on the file each names.\n");
        for p in &self.pins {
            s.push_str(&format!("- {}: {}\n", p.file, crate::supervisor::one_line(&p.text, 400)));
        }
        Some(s)
    }
}

/// ChloeCtx's defaults (chloectx/src/recall.rs), for a store whose
/// receipts do not say.
pub const DEFAULT_HI: f64 = 6.650;
pub const DEFAULT_LO: f64 = 4.508;
pub const DEFAULT_MIN_COS: f64 = 0.40;
pub const DEFAULT_BUDGET: usize = 4000;
pub const DEFAULT_BRIEFING: usize = 6000;

/// One candidate of a receipt.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RcRow {
    pub id: String,
    pub kind: String,
    pub tag: String,
    pub rank: i64,
    pub score: f64,
    pub cos: Option<f64>,
    pub tokens: i64,
    pub decision: String,
    pub text: String,
}

impl RcRow {
    pub fn shown(&self) -> bool {
        matches!(self.decision.as_str(), "take" | "excerpt" | "knowledge" | "listed" | "briefing")
    }
}

/// One receipt, from the list (`full: false`: only the rows it showed) or
/// from `chloectx receipt` (`full: true`: every candidate, G, the inputs).
#[derive(Clone, Debug, Default, Serialize)]
pub struct Rc {
    pub id: String,
    pub ts: i64,
    pub session: String,
    pub turn: i64,
    pub agent: String,
    pub name: String,
    pub query: String,
    pub decision: Option<String>,
    pub gate: Option<f64>,
    pub hi: Option<f64>,
    pub lo: Option<f64>,
    pub min_cos: Option<f64>,
    pub budget: Option<usize>,
    pub force: bool,
    pub rows: Vec<RcRow>,
    pub full: bool,
}

fn f(v: &Json) -> Option<f64> {
    v.as_f64()
}

fn row_of(v: &Json, texts: &Json) -> RcRow {
    let id = v["id"].as_str().unwrap_or("").to_string();
    let text = v["text"].as_str().map(str::to_string).or_else(|| texts[&id]["text"].as_str().map(str::to_string)).unwrap_or_default();
    RcRow {
        kind: v["kind"].as_str().unwrap_or("").into(),
        tag: v["tag"].as_str().or(texts[&id]["tag"].as_str()).unwrap_or("").into(),
        rank: v["rank"].as_i64().unwrap_or(0),
        score: f(&v["score"]).unwrap_or(0.0),
        cos: f(&v["cos"]),
        tokens: v["tokens"].as_i64().unwrap_or_else(|| (text.len() / 4) as i64),
        decision: v["decision"].as_str().unwrap_or("").into(),
        text,
        id,
    }
}

/// A receipt as `chloectx receipt --json` answers it (`{receipt, records}`),
/// or as the list gives it.
pub fn rc_of(v: &Json, agent: &str, name: &str) -> Rc {
    let (r, texts) = if v.get("receipt").is_some() { (&v["receipt"], &v["records"]) } else { (v, &Json::Null) };
    let inputs = &r["inputs"];
    let full = r.get("gate").is_some() && inputs.is_object();
    Rc {
        id: r["id"].as_str().unwrap_or("").into(),
        ts: trail::ctx_ms(&r["ts"]).unwrap_or(0),
        session: r["session"].as_str().unwrap_or("").into(),
        turn: r["turn"].as_i64().unwrap_or(0),
        agent: agent.into(),
        name: name.into(),
        query: r["query"].as_str().or(inputs["query"].as_str()).unwrap_or("").chars().take(300).collect(),
        decision: r["decision"].as_str().map(str::to_string),
        gate: f(&r["gate"]),
        hi: f(&inputs["gate"]["hi"]),
        lo: f(&inputs["gate"]["lo"]),
        min_cos: f(&inputs["knowledge"]["min_cos"]),
        budget: inputs["pack"]["budget_chars"].as_u64().map(|x| x as usize),
        force: inputs["force"].as_bool().unwrap_or(false),
        rows: r["rows"].as_array().map(|a| a.iter().map(|x| row_of(x, texts)).collect()).unwrap_or_default(),
        full,
    }
}

/// Full receipts already read, by id (receipts do not change).
fn cache() -> &'static Mutex<HashMap<String, Json>> {
    static C: OnceLock<Mutex<HashMap<String, Json>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The most full receipts read in one request.
const FULL_MAX: usize = 150;

/// The receipts of the agents' sessions in the window, oldest first, and
/// why some could not be read.
pub fn receipts(sup: &Supervisor, agents: &[crate::db::Agent], since_ms: i64) -> (Vec<Rc>, Vec<String>) {
    let mut out = Vec::new();
    let mut missing = Vec::new();
    let mut fulls = 0;
    for a in agents {
        let dir = if Path::new(&a.cwd).is_dir() { a.cwd.clone() } else { a.repo.clone() };
        let mut sessions: Vec<String> = a.session_id.iter().cloned().collect();
        if let Ok(evs) = sup.all_events(&a.id) {
            for ev in evs.iter().filter(|e| e.kind == "SessionStart") {
                if let Some(s) = trail::data_of(ev).and_then(|d| d["session_id"].as_str().map(str::to_string)) {
                    if !sessions.contains(&s) {
                        sessions.push(s);
                    }
                }
            }
        }
        for sid in sessions {
            match trail::run_chloectx(&sup.cfg.chloectx_bin, Path::new(&dir), &["receipts", "--session", &sid, "--limit", "500", "--json"], Duration::from_secs(20)) {
                Ran::Out(v) => {
                    for r in v["receipts"].as_array().into_iter().flatten() {
                        if r["session"].as_str().is_some_and(|s| !s.is_empty() && !sid.starts_with(s) && !s.starts_with(&sid)) {
                            continue;
                        }
                        let mut rc = rc_of(r, &a.id, &a.name);
                        if rc.ts < since_ms {
                            continue;
                        }
                        if !rc.full && fulls < FULL_MAX && !rc.id.is_empty() {
                            let hit = cache().lock().unwrap().get(&rc.id).cloned();
                            let full = hit.or_else(|| match trail::run_chloectx(&sup.cfg.chloectx_bin, Path::new(&dir), &["receipt", &rc.id, "--json", "--no-replay"], Duration::from_secs(20)) {
                                Ran::Out(v) if v.get("receipt").is_some() => {
                                    cache().lock().unwrap().insert(rc.id.clone(), v.clone());
                                    Some(v)
                                }
                                _ => None,
                            });
                            fulls += 1;
                            if let Some(v) = full {
                                let mut full = rc_of(&v, &a.id, &a.name);
                                if full.full {
                                    full.id = rc.id.clone();
                                    rc = full;
                                }
                            }
                        }
                        out.push(rc);
                    }
                }
                Ran::Missing => {
                    missing.push(trail::not_installed(&sup.cfg.chloectx_bin));
                    return (out, missing);
                }
                Ran::Failed(e) => {
                    if !missing.contains(&e) {
                        missing.push(e);
                    }
                }
            }
        }
    }
    out.sort_by_key(|r| r.ts);
    out.dedup_by(|a, b| a.id == b.id && !a.id.is_empty());
    (out, missing)
}

/// The settings the window's receipts were made with (the newest full one).
pub fn observed(rcs: &[Rc]) -> Json {
    let last = rcs.iter().rev().find(|r| r.full);
    json!({
        "gate_hi": last.and_then(|r| r.hi).unwrap_or(DEFAULT_HI),
        "gate_lo": last.and_then(|r| r.lo).unwrap_or(DEFAULT_LO),
        "knowledge_min_cos": last.and_then(|r| r.min_cos).unwrap_or(DEFAULT_MIN_COS),
        "budget_chars": last.and_then(|r| r.budget).unwrap_or(DEFAULT_BUDGET),
        "briefing_chars": DEFAULT_BRIEFING,
        "from_receipts": last.is_some(),
    })
}

/// A receipt's shown records under `m`: `(ids, exact)`.
pub fn shown_under(rc: &Rc, m: &MemEdits) -> (Vec<String>, bool) {
    let was: Vec<String> = rc.rows.iter().filter(|r| r.shown()).map(|r| r.id.clone()).collect();
    if !rc.full {
        return (was, !m.settings_set());
    }
    let hi = m.gate_hi.or(rc.hi).unwrap_or(DEFAULT_HI);
    let lo = m.gate_lo.or(rc.lo).unwrap_or(DEFAULT_LO);
    let min_cos = m.knowledge_min_cos.or(rc.min_cos).unwrap_or(DEFAULT_MIN_COS);
    let budget = m.budget_chars.or(rc.budget).unwrap_or(DEFAULT_BUDGET);
    let mut exact = true;
    // Knowledge: kept at the new threshold; a lower one may let in records
    // the receipt never listed.
    let mut ids: Vec<String> = rc.rows.iter().filter(|r| r.decision == "knowledge" && r.cos.unwrap_or(1.0) >= min_cos).map(|r| r.id.clone()).collect();
    if min_cos < rc.min_cos.unwrap_or(DEFAULT_MIN_COS) - 1e-9 {
        exact = false;
    }
    let g = rc.gate.unwrap_or(0.0);
    let was_d = rc.decision.clone().unwrap_or_else(|| "Skip".into());
    let now_d = if rc.force && rc.rows.iter().any(|r| r.rank > 0) {
        "Inject".to_string()
    } else if g >= hi {
        "Inject".into()
    } else if g >= lo {
        "Brief".into()
    } else {
        "Skip".into()
    };
    let hits = rc.rows.iter().filter(|r| r.rank > 0 && r.decision != "knowledge");
    if now_d == "Skip" {
        // Nothing of the search is given.
    } else if now_d == was_d && (now_d == "Brief" || budget == rc.budget.unwrap_or(DEFAULT_BUDGET)) {
        ids.extend(hits.filter(|r| r.shown()).map(|r| r.id.clone()));
    } else {
        // A new decision or budget: packed again from the candidates in
        // rank order, by their tokens; per-turn caps are not in the receipt.
        exact = false;
        let (cap, max) = if now_d == "Brief" { (1200usize, 3usize) } else { (budget, 12) };
        let mut used = 0usize;
        let mut n = 0;
        let mut ranked: Vec<&RcRow> = hits.filter(|r| r.tag != "uncertain" && !matches!(r.decision.as_str(), "skip: shown before" | "skip: same question" | "skip: shown as knowledge")).collect();
        ranked.sort_by_key(|r| r.rank);
        for r in ranked {
            let c = (r.tokens.max(1) as usize) * 4 + 40;
            if n >= max || used + c > cap {
                break;
            }
            used += c;
            n += 1;
            ids.push(r.id.clone());
        }
    }
    (ids, exact)
}

/// Words of a record that name a file (`cart.py`, `src/money.rs`).
pub fn files_named(text: &str) -> Vec<String> {
    const EXT: [&str; 22] = ["py", "rs", "js", "ts", "tsx", "jsx", "go", "java", "rb", "md", "toml", "json", "yaml", "yml", "css", "html", "sql", "sh", "c", "h", "cpp", "kt"];
    let mut out = Vec::new();
    for w in text.split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ',' | ';' | ':' | '“' | '”')) {
        let w = w.trim_end_matches(['.', ',', ')']);
        if let Some((stem, ext)) = w.rsplit_once('.') {
            if !stem.is_empty() && EXT.contains(&ext) && stem.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/' | '.')) && !out.contains(&w.to_string()) {
                out.push(w.to_string());
            }
        }
    }
    out
}

/// A memory missed at the turn it mattered.
#[derive(Clone, Debug, Serialize)]
pub struct Miss {
    pub receipt: String,
    pub agent: String,
    pub name: String,
    pub turn: i64,
    pub ts: i64,
    pub record: String,
    pub kind: String,
    pub text: String,
    pub score: f64,
    pub decision: String,
    pub file: String,
    pub failed: i64,
    pub cost_ms: i64,
    pub level: String,
}

/// The misses of the window: a record of a decision, rule or fact that a
/// recall left out, which names a file the agent then edited, with a test
/// failing after it. `edits` are (agent, file, ts); `tests` (agent, passed, ts).
pub fn misses(rcs: &[Rc], edits: &[(String, String, i64)], tests: &[(String, bool, i64)], last_event: &HashMap<String, i64>) -> Vec<Miss> {
    let mut out: Vec<Miss> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for rc in rcs.iter().filter(|r| r.full) {
        for row in rc.rows.iter().filter(|r| !r.shown() && matches!(r.kind.as_str(), "decision" | "preference" | "rule" | "fact")) {
            if seen.contains(&(rc.agent.clone(), row.id.clone())) {
                continue;
            }
            // Shown later in the session: not missed for long.
            for file in files_named(&row.text) {
                let edited = edits.iter().find(|(a, f, t)| *a == rc.agent && *t >= rc.ts && (f == &file || f.ends_with(&format!("/{file}"))));
                let Some(edit) = edited else { continue };
                let fails: Vec<i64> = tests.iter().filter(|(a, p, t)| *a == rc.agent && !p && *t >= edit.2).map(|x| x.2).collect();
                if fails.is_empty() {
                    continue;
                }
                let first_fail = *fails.iter().min().unwrap();
                let fixed = tests.iter().filter(|(a, p, t)| *a == rc.agent && *p && *t > first_fail).map(|x| x.2).min();
                let end = fixed.or_else(|| last_event.get(&rc.agent).copied()).unwrap_or(first_fail);
                seen.insert((rc.agent.clone(), row.id.clone()));
                out.push(Miss {
                    receipt: rc.id.clone(),
                    agent: rc.agent.clone(),
                    name: rc.name.clone(),
                    turn: rc.turn,
                    ts: rc.ts,
                    record: row.id.clone(),
                    kind: row.kind.clone(),
                    text: row.text.clone(),
                    score: row.score,
                    decision: row.decision.clone(),
                    file,
                    failed: fails.len() as i64,
                    cost_ms: (end - rc.ts).max(0),
                    level: "counted".into(),
                });
                break;
            }
        }
    }
    out
}

/// Confirms misses by replaying their receipts on the snapshot ChloeCtx
/// kept (`chloectx receipt <id> --json`): the same decision makes the
/// miss `recomputed`.
pub fn confirm(sup: &Supervisor, dir: &Path, ms: &mut [Miss]) {
    for m in ms.iter_mut().take(10) {
        if let Ran::Out(v) = trail::run_chloectx(&sup.cfg.chloectx_bin, dir, &["receipt", &m.receipt, "--json"], Duration::from_secs(30)) {
            let kept = v["replays"].as_array().and_then(|a| a.iter().find(|r| r["on"] == "kept snapshot"));
            if let Some(k) = kept {
                let same = k["rows"].as_array().into_iter().flatten().find(|r| r["id"] == json!(m.record)).is_some_and(|r| r["decision"] == json!(m.decision));
                if same {
                    m.level = "recomputed".into();
                }
            }
        }
    }
}

/// Every recall of the window under `draft` against `base`, and which
/// misses it fixes.
pub fn recompute(rcs: &[Rc], base: &MemEdits, draft: &MemEdits, ms: &[Miss]) -> Json {
    let mut changed = 0;
    let mut added = 0i64;
    let mut dropped = 0i64;
    let mut tokens = 0i64;
    let mut exact = 0;
    let mut sessions: HashSet<String> = HashSet::new();
    let mut by_session: BTreeMap<String, Vec<&Rc>> = BTreeMap::new();
    for rc in rcs {
        by_session.entry(format!("{}/{}", rc.agent, rc.session)).or_default().push(rc);
        let (a, ea) = shown_under(rc, base);
        let (b, eb) = shown_under(rc, draft);
        if ea && eb {
            exact += 1;
        }
        let tok = |id: &String| rc.rows.iter().find(|r| &r.id == id).map_or(0, |r| r.tokens);
        let plus: Vec<&String> = b.iter().filter(|x| !a.contains(x)).collect();
        let minus: Vec<&String> = a.iter().filter(|x| !b.contains(x)).collect();
        if !plus.is_empty() || !minus.is_empty() {
            changed += 1;
            sessions.insert(rc.name.clone());
        }
        added += plus.len() as i64;
        dropped += minus.len() as i64;
        tokens += plus.iter().map(|x| tok(x)).sum::<i64>() - minus.iter().map(|x| tok(x)).sum::<i64>();
    }
    // Pins: given at the start of each session that did not see the record.
    let new_pins: Vec<&Pin> = draft.pins.iter().filter(|p| !base.pins.iter().any(|q| q.record == p.record && q.file == p.file)).collect();
    for p in &new_pins {
        for (key, list) in &by_session {
            if !list.iter().any(|rc| rc.rows.iter().any(|r| r.id == p.record && r.shown())) {
                added += 1;
                tokens += (p.text.len() / 4) as i64 + 12;
                sessions.insert(key.split('/').next().and_then(|a| list.first().map(|r| r.name.clone()).filter(|_| !a.is_empty())).unwrap_or_default());
            }
        }
    }
    let fixed: Vec<Json> = ms
        .iter()
        .filter(|m| {
            let pinned = draft.pins.iter().any(|p| p.record == m.record) && !base.pins.iter().any(|p| p.record == m.record);
            let shown = rcs.iter().find(|r| r.id == m.receipt).is_some_and(|rc| shown_under(rc, draft).0.contains(&m.record));
            pinned || shown
        })
        .map(|m| json!({ "agent": m.name, "turn": m.turn, "record": m.record, "text": m.text }))
        .collect();
    let all_exact = exact == rcs.len() && rcs.iter().all(|r| r.full || !draft.settings_set());
    json!({
        "level": if rcs.is_empty() { "none" } else { "recomputed" },
        "exact": all_exact,
        "recalls": rcs.len(),
        "recalls_exact": exact,
        "sessions": by_session.len(),
        "changed": changed,
        "added": added,
        "dropped": dropped,
        "tokens": tokens,
        "sessions_changed": sessions.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "misses_fixed": fixed,
        "note": if all_exact { "Exact: each recall's decision from its own gate score, and pins as Workbench gives them." } else { "Decisions are exact from each receipt's gate score; which records a changed decision or budget gives is estimated from the candidates in rank order (the receipt does not hold the per-turn caps)." },
    })
}

/// One turn's recall under `base` and `draft`, record by record.
pub fn preview(rc: &Rc, base: &MemEdits, draft: &MemEdits) -> Json {
    let (a, ea) = shown_under(rc, base);
    let (b, eb) = shown_under(rc, draft);
    let pinned = |m: &MemEdits, id: &str| m.pins.iter().any(|p| p.record == id);
    let word = |shown: bool, pin: bool| if pin { "pinned" } else if shown { "given" } else { "left out" };
    let mut rows: Vec<Json> = rc
        .rows
        .iter()
        .map(|r| {
            let before = word(a.contains(&r.id), pinned(base, &r.id));
            let after = word(b.contains(&r.id), pinned(draft, &r.id));
            json!({ "id": r.id, "kind": r.kind, "tag": r.tag, "text": r.text, "score": r.score, "cos": r.cos, "rank": r.rank, "decision": r.decision, "before": before, "after": after, "changed": before != after })
        })
        .collect();
    for p in draft.pins.iter().filter(|p| !rc.rows.iter().any(|r| r.id == p.record)) {
        rows.push(json!({ "id": p.record, "kind": "decision", "text": p.text, "score": Json::Null, "before": if pinned(base, &p.record) { "pinned" } else { "not a candidate" }, "after": "pinned", "changed": !pinned(base, &p.record) }));
    }
    rows.sort_by_key(|r| (!r["changed"].as_bool().unwrap_or(false), r["before"] != json!("given")));
    json!({
        "receipt": rc.id, "agent": rc.agent, "name": rc.name, "turn": rc.turn, "ts": crate::db::iso(rc.ts), "query": rc.query,
        "gate": rc.gate, "decision": rc.decision, "full": rc.full, "exact": ea && eb, "rows": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc() -> Rc {
        let row = |id: &str, kind: &str, rank: i64, d: &str, cos: Option<f64>| RcRow { id: id.into(), kind: kind.into(), tag: String::new(), rank, score: 0.5, cos, tokens: 50, decision: d.into(), text: format!("{id} about cart.py") };
        Rc {
            id: "r1".into(),
            ts: 1000,
            agent: "a1".into(),
            name: "freeship".into(),
            decision: Some("Brief".into()),
            gate: Some(5.0),
            hi: Some(6.65),
            lo: Some(4.5),
            min_cos: Some(0.4),
            budget: Some(4000),
            rows: vec![row("k1", "decision", 0, "knowledge", Some(0.45)), row("h1", "turn", 1, "take", None), row("h2", "decision", 2, "skip: record cap", None), row("h3", "turn", 3, "skip: record cap", None)],
            full: true,
            ..Rc::default()
        }
    }

    #[test]
    fn thresholds_are_recomputed_from_the_gate_score() {
        let r = rc();
        let (same, exact) = shown_under(&r, &MemEdits::default());
        assert_eq!((same, exact), (vec!["k1".to_string(), "h1".to_string()], true));
        let (more, exact) = shown_under(&r, &MemEdits { gate_hi: Some(4.9), ..MemEdits::default() });
        assert!(!exact && more.len() == 4, "{more:?}");
        let (none, exact) = shown_under(&r, &MemEdits { gate_lo: Some(5.5), knowledge_min_cos: Some(0.5), ..MemEdits::default() });
        assert!(exact && none.is_empty(), "{none:?}");
    }

    #[test]
    fn a_skipped_decision_naming_an_edited_file_before_a_failure_is_a_miss() {
        let r = rc();
        let edits = vec![("a1".to_string(), "shop/cart.py".to_string(), 2000)];
        let tests = vec![("a1".to_string(), false, 3000), ("a1".to_string(), true, 9000)];
        let ms = misses(&[r.clone()], &edits, &tests, &HashMap::new());
        assert_eq!(ms.len(), 1);
        assert_eq!((ms[0].record.as_str(), ms[0].file.as_str(), ms[0].cost_ms), ("h2", "cart.py", 8000));
        let pin = MemEdits { pins: vec![Pin { file: "cart.py".into(), record: "h2".into(), text: "h2".into() }], ..MemEdits::default() };
        let v = recompute(&[r], &MemEdits::default(), &pin, &ms);
        assert_eq!(v["misses_fixed"].as_array().unwrap().len(), 1);
        assert_eq!(files_named("Shipping is not taxed (cart.py), see `src/money.rs`."), vec!["cart.py", "src/money.rs"]);
    }
}
