//! What a space's page reads, over any [`Store`]: a team space of the
//! cluster (`ctx_<space>`, [`Store::open_remote`]) or a Claude Code store on
//! the machine the app runs on, which its daemon reads for the app (the
//! daemon's `view` op). One set of answers, so the page draws both alike:
//!
//! | what | answer |
//! |---|---|
//! | [`overview`] | the counts, the current rules, decisions, facts and open to-dos, the latest changes and sessions |
//! | [`notes`] | the notes, newest first (current ones unless `all`) |
//! | [`history`] | a note, what it replaced and what replaced it |
//! | [`sessions`] | the sessions, latest first, each with its first prompt as a title |
//! | [`records`] | a session's records in order, else the latest (of a kind), or the best matches for `q` |
//! | [`ask`] | the context an agent gets for a question, what went in, why, and what was left out |

use std::collections::{HashMap, HashSet};

use chloedb::query::Params;
use chloedb::Value;
use serde_json::{json, Value as Json};

use super::{params_for, record_json, tokens, NOTE_KINDS};
use crate::context;
use crate::record::{Kind, Record, COLUMNS};
use crate::store::{row_record, Filter, Store};
use crate::{err, Result};

/// The most records one call answers.
pub const MAX_RECORDS: usize = 500;
/// The most characters of a record's text and context an answer carries.
const MAX_TEXT: usize = 4000;
/// The most notes of each kind an overview lists.
const OVERVIEW_NOTES: usize = 12;

fn clip(s: &str, n: usize) -> (String, bool) {
    if s.chars().count() <= n {
        return (s.to_string(), false);
    }
    (s.chars().take(n).collect(), true)
}

/// A record as the Inspector's pages show one (as `memory.rs` answers the
/// synced table's rows): its whole text up to [`MAX_TEXT`], its context,
/// files and links.
pub fn record_full(r: &Record) -> Json {
    let (text, cut) = clip(&r.text, MAX_TEXT);
    let (context, _) = clip(&r.ctx, MAX_TEXT);
    json!({
        "id": r.id, "session": r.session, "kind": r.kind.as_str(), "tag": r.tag, "turn": r.turn, "seq": r.seq, "ts": r.ts,
        "importance": r.importance, "files": r.files, "text": text, "cut": cut, "context": context,
        "links": r.links.iter().map(|l| json!({"label": l.label, "to": l.to})).collect::<Vec<_>>(),
    })
}

fn current(r: &Record) -> bool {
    !crate::record::closed_tag(&r.tag) && r.tag != "done"
}

fn int(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64()).unwrap_or(0)
}

fn text(v: Option<&Value>) -> String {
    v.and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// The notes, newest first: the current ones unless `all` (replaced,
/// retired and done ones too). `kind` is a caller's name of one kind.
pub fn notes(store: &Store, kind: Option<Kind>, limit: usize, all: bool) -> Result<Json> {
    let kinds: Vec<Kind> = kind.map_or_else(|| NOTE_KINDS.to_vec(), |k| vec![k]);
    let want = limit.clamp(1, 1000);
    let got = store.latest(&kinds, if all { want } else { (want * 4).min(crate::cluster::WINDOW) }, &Filter::default())?;
    let notes: Vec<Json> = got.iter().filter(|r| all || current(r)).take(want).map(record_json).collect();
    Ok(json!({"notes": notes}))
}

/// A note and its history: the notes it replaced, back to the first, and
/// the ones that replaced it.
pub fn history(store: &Store, id: &str) -> Result<Option<Json>> {
    let Some(note) = store.get(&[id.to_string()])?.pop() else { return Ok(None) };
    let walk = |start: &Record, label: &str| -> Result<Vec<Record>> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::from([start.id.clone()]);
        let mut next: Vec<String> = start.linked(label).map(String::from).collect();
        while !next.is_empty() && out.len() < 50 {
            let got = store.get(&next)?;
            next = Vec::new();
            for r in got {
                if seen.insert(r.id.clone()) {
                    next.extend(r.linked(label).map(String::from));
                    out.push(r);
                }
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.seq));
        Ok(out)
    };
    let earlier = walk(&note, "supersedes")?;
    let mut later = walk(&note, "superseded_by")?;
    if later.is_empty() && !current(&note) {
        // A Claude Code store links only the new note to the old one: the
        // notes that replaced this one are found by their links.
        let notes = store.latest(&NOTE_KINDS, crate::cluster::WINDOW, &Filter::default())?;
        let mut want: Vec<String> = vec![note.id.clone()];
        let mut seen: HashSet<String> = HashSet::from([note.id.clone()]);
        while let Some(id) = want.pop() {
            for r in notes.iter().filter(|r| r.linked("supersedes").any(|t| t == id)) {
                if seen.insert(r.id.clone()) {
                    want.push(r.id.clone());
                    later.push(r.clone());
                }
            }
        }
        later.sort_by_key(|r| std::cmp::Reverse(r.seq));
    }
    Ok(Some(json!({"note": record_json(&note), "earlier": earlier.iter().map(record_json).collect::<Vec<_>>(), "later": later.iter().map(record_json).collect::<Vec<_>>()})))
}

/// Each session's first prompt, a line of it (what the session was
/// about), and how many prompts it has: its turns.
fn titles(store: &Store) -> Result<HashMap<String, (String, i64)>> {
    let rs = store.query("SELECT session, seq, text FROM memory WHERE kind = 'prompt' ORDER BY seq ASC LIMIT 10000")?;
    let mut out: HashMap<String, (String, i64)> = HashMap::new();
    for r in &rs.rows {
        let s = text(r.first());
        if s.is_empty() {
            continue;
        }
        let e = out.entry(s).or_insert_with(|| (String::new(), 0));
        e.1 += 1;
        if e.0.is_empty() {
            let line = text(r.get(2)).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
            e.0 = clip(&line, 160).0;
        }
    }
    Ok(out)
}

/// The sessions, the latest first, with their counts and first prompt.
pub fn sessions(store: &Store) -> Result<Json> {
    let rs = store.query("SELECT session, COUNT(*), MIN(ts), MAX(ts) FROM memory WHERE kind != 'entity' GROUP BY session LIMIT 5000")?;
    let titles = titles(store)?;
    let mut sessions: Vec<Json> = rs
        .rows
        .iter()
        .map(|r| {
            let s = text(r.first());
            let (title, turns) = titles.get(&s).cloned().unwrap_or_default();
            json!({"session": s, "records": int(r.get(1)), "first": int(r.get(2)), "last": int(r.get(3)), "turns": turns, "title": (!title.is_empty()).then_some(title)})
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s["last"].as_i64().unwrap_or(0)));
    let kinds = store.query("SELECT kind, COUNT(*) FROM memory GROUP BY kind LIMIT 50")?;
    let by_kind: HashMap<String, i64> = kinds.rows.iter().map(|r| (text(r.first()), int(r.get(1)))).collect();
    Ok(json!({"sessions": sessions, "by_kind": by_kind}))
}

/// A session's records in the order they were recorded; else the latest
/// (of `kind`), or the best matches for `q` by BM25. `before` (a `seq`)
/// pages back through the latest.
pub fn records(store: &Store, session: Option<&str>, kind: Option<&str>, q: Option<&str>, limit: usize, before: Option<i64>) -> Result<Json> {
    let limit = limit.clamp(1, MAX_RECORDS);
    let mut conds = Vec::new();
    let mut params = Params::new();
    let session = session.filter(|s| !s.is_empty());
    if let Some(s) = session {
        conds.push("session = :s".to_string());
        params = params.set("s", Value::Str(s.to_string()));
    }
    if let Some(k) = kind.filter(|k| !k.is_empty()) {
        let k = Kind::parse(k).ok_or_else(|| err(format!("no kind {k:?}")))?;
        conds.push("kind = :k".into());
        params = params.set("k", Value::Str(k.as_str().to_string()));
    }
    if let Some(b) = before {
        conds.push("seq < :b".into());
        params = params.set("b", Value::Int(b));
    }
    let wher = if conds.is_empty() { String::new() } else { format!("WHERE {}", conds.join(" AND ")) };
    let q = q.map(str::trim).filter(|q| !q.is_empty());
    let sql = match q {
        Some(_) => format!("SELECT {COLUMNS} FROM memory {wher} ORDER BY bm25(body, :q, syntax => 'plain') DESC LIMIT {limit}"),
        None if session.is_some() => format!("SELECT {COLUMNS} FROM memory {wher} ORDER BY seq ASC LIMIT {limit}"),
        None => format!("SELECT {COLUMNS} FROM memory {wher} ORDER BY seq DESC LIMIT {limit}"),
    };
    if let Some(q) = q {
        params = params.set("q", Value::Str(q.to_string()));
    }
    let rs = store.query_with(&sql, &params)?;
    let records: Vec<Json> = rs.rows.iter().filter_map(|r| row_record(r)).map(|r| record_full(&r)).collect();
    let more = records.len() == limit;
    Ok(json!({"session": session, "records": records, "more": more}))
}

/// What a space knows now, for the top of its page: its counts, its
/// current rules, decisions, facts and open to-dos (newest first), the
/// latest notes that replaced others (with what they replaced), and its
/// latest sessions.
pub fn overview(store: &Store) -> Result<Json> {
    let kinds = store.query("SELECT kind, COUNT(*) FROM memory GROUP BY kind LIMIT 50")?;
    let by_kind: HashMap<String, i64> = kinds.rows.iter().map(|r| (text(r.first()), int(r.get(1)))).collect();
    let span = store.query("SELECT MIN(ts), MAX(ts), COUNT(*) FROM memory WHERE kind != 'entity'")?;
    let (first, last, records) = span.rows.first().map_or((0, 0, 0), |r| (int(r.first()), int(r.get(1)), int(r.get(2))));
    let all = store.latest(&NOTE_KINDS, crate::cluster::WINDOW, &Filter::default())?;
    let mut live: HashMap<&'static str, i64> = HashMap::new();
    let mut lists: HashMap<&'static str, Vec<Json>> = HashMap::new();
    let by_id: HashMap<&str, &Record> = all.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut changes = Vec::new();
    for r in &all {
        if !current(r) {
            continue;
        }
        let name = super::kind_name(r);
        *live.entry(name).or_default() += 1;
        let l = lists.entry(name).or_default();
        if l.len() < OVERVIEW_NOTES {
            l.push(record_json(r));
        }
        let was: Vec<Json> = r.linked("supersedes").filter_map(|id| by_id.get(id)).map(|o| json!({"id": o.id, "text": o.text, "ts": o.ts})).collect();
        if !was.is_empty() && changes.len() < 8 {
            let mut j = record_json(r);
            j["was"] = json!(was);
            changes.push(j);
        }
    }
    let ss = sessions(store)?;
    let recent: Vec<Json> = ss["sessions"].as_array().map(|a| a.iter().take(6).cloned().collect()).unwrap_or_default();
    let count = ss["sessions"].as_array().map_or(0, |a| a.len());
    let list = |k: &str| lists.get(k).cloned().unwrap_or_default();
    Ok(json!({
        "records": records, "sessions": count, "first": first, "last": last, "by_kind": by_kind, "live": live,
        "rules": list("rule"), "decisions": list("decision"), "facts": list("fact"), "todos": list("todo"), "notes": list("note"),
        "changes": changes, "recent_sessions": recent,
    }))
}

/// The context an agent gets for `question` within `budget` tokens, built
/// by search alone ([`context::build`]): each record in it with its
/// section, tokens and scores, and what the searches found and left out.
pub fn ask(store: &Store, question: &str, budget: usize) -> Result<Json> {
    if question.trim().is_empty() {
        return Err(err("ask needs a question (q)"));
    }
    let budget = budget.clamp(200, 50_000);
    let built = context::build(store, question, &Filter::default(), &params_for(budget))?;
    let mut items = Vec::new();
    let mut shown: HashSet<String> = HashSet::new();
    for (r, p) in built.records.iter().zip(&built.picks) {
        shown.insert(r.id.clone());
        let mut j = record_json(r);
        j["section"] = json!(p.section);
        j["chars"] = json!(p.chars);
        j["tokens"] = json!(tokens(p.chars));
        j["bm25"] = json!(p.bm25);
        j["cos"] = json!(p.cos);
        j["replaced"] = json!(p.replaced);
        items.push(j);
    }
    // The versions shown notes replaced: in the context as a reminder
    // of what changed, not as notes of their own.
    let older: Vec<String> = built.records.iter().flat_map(|r| r.linked("supersedes").map(String::from)).filter(|id| !shown.contains(id)).collect();
    let mut left: Vec<Json> = store
        .get(&older)?
        .iter()
        .map(|r| {
            let mut j = record_json(r);
            j["why"] = json!("superseded");
            j
        })
        .collect();
    for (r, why) in &built.left_out {
        if shown.contains(&r.id) {
            continue;
        }
        let mut j = record_json(r);
        j["why"] = json!(why);
        left.push(j);
    }
    left.truncate(30);
    let chars = built.text.len();
    Ok(json!({
        "question": question, "budget": budget, "context": built.text, "chars": chars, "tokens": tokens(chars),
        "ms": (built.ms * 100.0).round() / 100.0, "model_calls": 0, "items": items, "left_out": left,
    }))
}

/// A note as the graph draws it: from a store's records, or from the rows
/// `chloectx sync` copied (`memory.rs`).
pub struct GraphNote {
    pub id: String,
    /// As callers name it: `rule` for a preference.
    pub kind: String,
    pub tag: String,
    pub text: String,
    pub ts: i64,
    pub session: String,
    /// `(label, to)`: `about` an entity (`e:…`), `touches` a file (`f:…`),
    /// `supersedes` an older note.
    pub links: Vec<(String, String)>,
}

/// The most notes the graph draws, the newest first.
pub const GRAPH_NOTES: usize = 1500;

/// How a space's memory is connected: its notes, the subjects (entities)
/// and files they are about, the sessions they came from, and which note
/// replaced which. `names` are the entities' own names, `titles` each
/// session's first prompt. Every node and edge is answered: the page picks
/// what to draw (replaced notes, files, sessions).
pub fn graph_of(notes: &[GraphNote], names: &HashMap<String, String>, titles: &HashMap<String, String>) -> Json {
    let notes = &notes[..notes.len().min(GRAPH_NOTES)];
    let ids: HashSet<&str> = notes.iter().map(|n| n.id.as_str()).collect();
    // A copy synced before its note was replaced may not say so: the note
    // that replaced it does.
    let replaced: HashSet<&str> = notes.iter().flat_map(|n| n.links.iter().filter(|(l, _)| l == "supersedes").map(|(_, t)| t.as_str())).collect();
    let mut nodes: Vec<Json> = Vec::new();
    let mut edges: Vec<Json> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |id: &str, node: Json, nodes: &mut Vec<Json>| {
        if seen.insert(id.to_string()) {
            nodes.push(node);
        }
    };
    for n in notes {
        let current = !crate::record::closed_tag(&n.tag) && n.tag != "done" && !replaced.contains(n.id.as_str());
        add(&n.id, json!({"id": n.id, "type": "note", "kind": n.kind, "label": clip(&n.text, 140).0, "text": clip(&n.text, 600).0, "tag": n.tag, "current": current, "ts": n.ts, "session": n.session}), &mut nodes);
        for (label, to) in &n.links {
            match label.as_str() {
                "about" if to.starts_with("e:") => {
                    let name = names.get(to).cloned().unwrap_or_else(|| to[2..].to_string());
                    add(to, json!({"id": to, "type": "entity", "label": name}), &mut nodes);
                }
                "touches" if to.starts_with("f:") => add(to, json!({"id": to, "type": "file", "label": &to[2..]}), &mut nodes),
                "supersedes" if ids.contains(to.as_str()) => {}
                _ => continue,
            }
            edges.push(json!({"s": n.id, "t": to, "label": label}));
        }
        if !n.session.is_empty() && n.session != "-" {
            let sid = format!("s:{}", n.session);
            let title = titles.get(&n.session).cloned().unwrap_or_else(|| format!("Session {}", n.session.chars().take(8).collect::<String>()));
            add(&sid, json!({"id": sid, "type": "session", "label": title, "session": n.session}), &mut nodes);
            edges.push(json!({"s": n.id, "t": sid, "label": "from"}));
        }
    }
    json!({"nodes": nodes, "edges": edges})
}

/// A store's graph, as [`graph_of`] draws it.
pub fn graph(store: &Store) -> Result<Json> {
    let got = store.latest(&NOTE_KINDS, GRAPH_NOTES, &Filter::default())?;
    let notes: Vec<GraphNote> = got
        .iter()
        .map(|r| GraphNote {
            id: r.id.clone(),
            kind: super::kind_name(r).to_string(),
            tag: r.tag.clone(),
            text: r.text.clone(),
            ts: r.ts,
            session: r.session.clone(),
            links: r.links.iter().map(|l| (l.label.clone(), l.to.clone())).collect(),
        })
        .collect();
    let mut want: Vec<String> = notes.iter().flat_map(|n| n.links.iter().filter(|(l, t)| l == "about" && t.starts_with("e:")).map(|(_, t)| t.clone())).collect();
    want.sort();
    want.dedup();
    let names: HashMap<String, String> = store.get(&want)?.into_iter().map(|r| (r.id, r.text)).collect();
    let titles: HashMap<String, String> = titles(store)?.into_iter().map(|(s, (t, _))| (s, t)).collect();
    Ok(graph_of(&notes, &names, &titles))
}

/// One of the answers above, by name, with its arguments in a JSON object:
/// the daemon's `view` op, which `chloectx app --local-stores` calls to
/// read a store on its machine.
pub fn by_name(store: &Store, req: &Json) -> Result<Json> {
    let s = |k: &str| req.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty());
    let n = |k: &str, d: u64| req.get(k).and_then(|v| v.as_u64()).unwrap_or(d) as usize;
    match s("what").unwrap_or("") {
        "overview" => overview(store),
        "sessions" => sessions(store),
        "records" => records(store, s("session"), s("kind"), s("q"), n("limit", 200), req.get("before").and_then(|v| v.as_i64())),
        "notes" => {
            let kind = match s("kind") {
                Some(k) => Some(super::note_kind(k).ok_or_else(|| err("kind is decision, rule, fact, todo or note"))?.0),
                None => None,
            };
            notes(store, kind, n("limit", 100), req.get("all").and_then(|v| v.as_bool()).unwrap_or(false))
        }
        "history" => history(store, s("id").unwrap_or(""))?.ok_or_else(|| err(format!("no note {}", s("id").unwrap_or("")))),
        "ask" => ask(store, s("question").unwrap_or(""), n("budget", 4000)),
        "graph" => graph(store),
        w => Err(err(format!("no view {w:?}: overview, sessions, records, notes, history, ask or graph"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::record::Draft;
    use crate::store::StoreOptions;
    use std::sync::Arc;

    const DAY: i64 = 86_400;

    /// Two sessions, a decision replaced by a later one and a rule: what
    /// the inspector's views make of them.
    #[test]
    fn views_of_a_store() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("v.strata"), Arc::new(HashEmbedder { dim: 256 }), StoreOptions { sync: false, ..Default::default() }).unwrap();
        s.append("s1", 0, DAY, vec![Draft::new(Kind::Prompt, "Who approves large refunds?\nThe finance team asked.")]).unwrap();
        let old = s.append("s1", 0, DAY + 30, vec![Draft::new(Kind::Decision, "Refunds over 500 EUR need a second approver.").tag("superseded")]).unwrap();
        s.append("s1", 0, DAY + 60, vec![Draft::new(Kind::Answer, "A second approver from finance, over 500 EUR.")]).unwrap();
        s.append("s2", 0, 3 * DAY, vec![Draft::new(Kind::Prompt, "Finance raised the refund limit to 1000 EUR.")]).unwrap();
        s.append("s2", 0, 3 * DAY + 30, vec![Draft::new(Kind::Decision, "Refunds over 1000 EUR need a second approver.").tag("distilled").link("supersedes", old[0].id.clone())]).unwrap();
        s.append("s2", 1, 3 * DAY + 90, vec![Draft::new(Kind::Prompt, "commit")]).unwrap();
        s.append("s2", 1, 3 * DAY + 95, vec![Draft::new(Kind::Preference, "Never log card numbers.").tag("distilled")]).unwrap();
        s.flush().unwrap();
        let view = |req: Json| by_name(&s, &req).unwrap();

        let ov = view(json!({"what": "overview"}));
        assert_eq!((ov["sessions"].as_i64(), ov["live"]["decision"].as_i64(), ov["live"]["rule"].as_i64()), (Some(2), Some(1), Some(1)), "{ov}");
        assert_eq!(ov["decisions"].as_array().unwrap().len(), 1, "{ov}");
        assert!(ov["decisions"][0]["text"].as_str().unwrap().contains("1000 EUR"), "{ov}");
        assert!(ov["changes"][0]["was"][0]["text"].as_str().unwrap().contains("500 EUR"), "{ov}");

        let ss = view(json!({"what": "sessions"}));
        let ss = ss["sessions"].as_array().unwrap();
        assert_eq!(ss[0]["session"], "s2", "the latest first");
        assert_eq!((ss[0]["turns"].as_i64(), ss[1]["turns"].as_i64()), (Some(2), Some(1)));
        assert_eq!(ss[1]["title"], "Who approves large refunds?", "a session's title is its first prompt's first line");

        let rs = view(json!({"what": "records", "session": "s1"}));
        let kinds: Vec<&str> = rs["records"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["prompt", "decision", "answer"], "in the order recorded");
        let found = view(json!({"what": "records", "q": "card numbers"}));
        assert_eq!(found["records"][0]["kind"], "preference", "{found}");

        assert_eq!(view(json!({"what": "notes"}))["notes"].as_array().unwrap().len(), 2, "current notes only");
        assert_eq!(view(json!({"what": "notes", "all": true}))["notes"].as_array().unwrap().len(), 3);
        let h = view(json!({"what": "history", "id": old[0].id}));
        assert!(h["later"][0]["text"].as_str().unwrap().contains("1000 EUR"), "{h}");

        let a = view(json!({"what": "ask", "question": "Who approves a refund of 1200 EUR?"}));
        let items = a["items"].as_array().unwrap();
        assert!(items.iter().any(|i| i["text"].as_str().unwrap().contains("1000 EUR")), "{a}");
        assert!(!items.iter().any(|i| i["id"] == json!(old[0].id)), "a replaced decision is left out: {a}");
        // The graph: the notes, the session each came from, and which replaced which.
        let g = view(json!({"what": "graph"}));
        let node = |id: &str| g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id).cloned();
        let old_node = node(&old[0].id).unwrap_or_else(|| panic!("{g}"));
        assert_eq!(old_node["current"], false, "{g}");
        assert_eq!(node("s:s1").unwrap()["label"], "Who approves large refunds?", "{g}");
        let edges = g["edges"].as_array().unwrap();
        assert!(edges.iter().any(|e| e["label"] == "supersedes" && e["t"] == json!(old[0].id)), "{g}");
        assert!(edges.iter().any(|e| e["label"] == "from" && e["t"] == "s:s2"), "{g}");
        assert!(by_name(&s, &json!({"what": "nothing"})).is_err());
    }
}
