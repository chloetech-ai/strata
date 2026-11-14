//! The Inspector page's API: the memory Claude Code sessions recorded, as
//! `chloectx sync` copies it into the cluster: a table per project
//! (`chloectx_mem_<project>`, [`crate::sync::table_for`]), or, from before
//! 0.3.4, the table projects shared, [`TABLE`] (`chloectx_memory`); one
//! row per record, keyed `<project>:<id>`. Read the way `chloectx ui`
//! reads a local store: by project, by session in the order it was
//! recorded, by kind, and by search.
//!
//! | call | answer |
//! |---|---|
//! | `GET /v1/memory` | `{table, exists, projects: [{project, records, sessions, last}], create}` (`create`: the table's CREATE TABLE) |
//! | `GET /v1/memory/sessions?project=` | `{sessions: [{session, records, first, last, turns}]}`, the latest first |
//! | `GET /v1/memory/records?project=&session=&kind=&q=&limit=&before=` | `{records}`: a session's in the order recorded, else the latest first (`before`: a `seq` to page back from), or the best matches for `q` |
//!
//! Only reads: the page changes nothing, and a key that may read a
//! project's table is enough for that project; the list shows the projects
//! whose tables the caller reads.

use std::collections::HashMap;

use chloedb::query::Params;
use chloedb::Value;
use serde_json::{json, Value as Json};

use super::{bad, Answer, App};
use crate::sync::TABLE;

/// The most records one call answers.
const MAX_RECORDS: usize = 500;
/// The most characters of a record's text and context an answer carries.
const MAX_TEXT: usize = 4000;

const COLUMNS: &str = "rid, session, kind, tag, turn, seq, ts, importance, files, content, context, links_json";

fn str_of(v: Option<&Value>) -> String {
    v.and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn int_of(v: Option<&Value>) -> i64 {
    v.and_then(|v| v.as_i64()).unwrap_or(0)
}

fn clip(s: String) -> (String, bool) {
    if s.chars().count() <= MAX_TEXT {
        return (s, false);
    }
    (s.chars().take(MAX_TEXT).collect(), true)
}

fn record(row: &[Value]) -> Json {
    let (text, cut) = clip(str_of(row.get(9)));
    let (context, _) = clip(str_of(row.get(10)));
    let links: Json = serde_json::from_str(&str_of(row.get(11))).unwrap_or(json!([]));
    json!({
        "id": str_of(row.first()), "session": str_of(row.get(1)), "kind": str_of(row.get(2)), "tag": str_of(row.get(3)),
        "turn": int_of(row.get(4)), "seq": int_of(row.get(5)), "ts": int_of(row.get(6)),
        "importance": row.get(7).and_then(|v| v.as_f64()).unwrap_or(0.0), "files": str_of(row.get(8)),
        "text": text, "cut": cut, "context": context, "links": links,
    })
}

impl App {
    /// The table project `project` is read from: its own, else the shared
    /// one of before 0.3.4; `None` when neither exists.
    pub fn memory_table(&self, project: &str) -> Result<Option<String>, super::Fail> {
        let tables = self.cluster.tables()?;
        let own = crate::sync::table_for(project);
        Ok(if tables.iter().any(|t| *t == own) {
            Some(own)
        } else if tables.iter().any(|t| t == TABLE) {
            Some(TABLE.to_string())
        } else {
            None
        })
    }

    /// `GET /v1/memory`: the projects synced to the cluster whose tables
    /// `caller` reads, each with its table (`table`) and `create`, the
    /// CREATE TABLE of a project's table (with `{project}` for its name's
    /// place: [`crate::sync::table_for`]).
    pub fn memory(&self, caller: &super::Caller) -> Answer {
        let tables: Vec<String> = self.cluster.tables()?.into_iter().filter(|t| t == TABLE || t.starts_with(crate::sync::PREFIX)).collect();
        let mut by: HashMap<String, Json> = HashMap::new();
        for t in tables.iter().filter(|t| caller.reads_table(t)) {
            let rs = self.cluster.query(&format!("SELECT project, COUNT(*), COUNT(DISTINCT session), MAX(ts) FROM {t} GROUP BY project LIMIT 1000"), &Params::new())?;
            for r in &rs.rows {
                let project = str_of(r.first());
                // A project with a table of its own is read from it; its
                // rows in the shared table are older copies.
                let own = crate::sync::table_for(&project);
                if (t == TABLE && tables.contains(&own)) || (t != TABLE && *t != own) {
                    continue;
                }
                by.insert(project.clone(), json!({"project": project, "table": t, "records": int_of(r.get(1)), "sessions": int_of(r.get(2)), "last": int_of(r.get(3))}));
            }
        }
        let mut projects: Vec<Json> = by.into_values().collect();
        projects.sort_by_key(|p| std::cmp::Reverse(p["last"].as_i64().unwrap_or(0)));
        Ok(json!({"exists": !tables.is_empty(), "projects": projects, "prefix": crate::sync::PREFIX, "create": crate::sync::create_for("{project}")}))
    }

    /// `GET /v1/memory/sessions?project=`: a project's sessions, the
    /// latest first.
    pub fn memory_sessions(&self, project: &str) -> Answer {
        if project.is_empty() {
            return Err(bad("which project? (?project=owner/name)"));
        }
        let Some(t) = self.memory_table(project)? else {
            return Ok(json!({"sessions": []}));
        };
        let rs = self.cluster.query(
            &format!("SELECT session, COUNT(*), MIN(ts), MAX(ts) FROM {t} WHERE project = :p AND kind != 'entity' GROUP BY session LIMIT 5000"),
            &Params::new().set("p", Value::Str(project.to_string())),
        )?;
        let mut sessions: Vec<Json> = rs
            .rows
            .iter()
            .map(|r| json!({"session": str_of(r.first()), "records": int_of(r.get(1)), "first": int_of(r.get(2)), "last": int_of(r.get(3))}))
            .collect();
        // Each session's first prompt, a line of it, as its title.
        let prompts = self.cluster.query(
            &format!("SELECT session, seq, content FROM {t} WHERE project = :p AND kind = 'prompt' ORDER BY seq ASC LIMIT 10000"),
            &Params::new().set("p", Value::Str(project.to_string())),
        )?;
        let mut titles: HashMap<String, String> = HashMap::new();
        let mut turns: HashMap<String, i64> = HashMap::new();
        for r in &prompts.rows {
            *turns.entry(str_of(r.first())).or_default() += 1;
            let line = str_of(r.get(2)).lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").chars().take(160).collect::<String>();
            if !line.is_empty() {
                titles.entry(str_of(r.first())).or_insert(line);
            }
        }
        for s in &mut sessions {
            let id = s["session"].as_str().unwrap_or("").to_string();
            s["title"] = json!(titles.get(&id));
            s["turns"] = json!(turns.get(&id).copied().unwrap_or(0));
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s["last"].as_i64().unwrap_or(0)));
        // What each kind of the project counts, for the page's filters.
        let kinds = self.cluster.query(&format!("SELECT kind, COUNT(*) FROM {t} WHERE project = :p GROUP BY kind LIMIT 50"), &Params::new().set("p", Value::Str(project.to_string())))?;
        let by_kind: HashMap<String, i64> = kinds.rows.iter().map(|r| (str_of(r.first()), int_of(r.get(1)))).collect();
        Ok(json!({"project": project, "sessions": sessions, "by_kind": by_kind}))
    }

    /// `GET /v1/memory/records`: a session's records in the order they were
    /// recorded; else the project's latest (of a kind), or its best matches
    /// for `q`.
    pub fn memory_records(&self, project: &str, session: Option<&str>, kind: Option<&str>, q: Option<&str>, limit: usize, before: Option<i64>) -> Answer {
        if project.is_empty() {
            return Err(bad("which project? (?project=owner/name)"));
        }
        let Some(t) = self.memory_table(project)? else {
            return Ok(json!({"records": []}));
        };
        let limit = limit.clamp(1, MAX_RECORDS);
        let mut conds = vec!["project = :p".to_string()];
        let mut params = Params::new().set("p", Value::Str(project.to_string()));
        if let Some(s) = session.filter(|s| !s.is_empty()) {
            conds.push("session = :s".into());
            params = params.set("s", Value::Str(s.to_string()));
        }
        if let Some(k) = kind.filter(|k| !k.is_empty()) {
            if crate::record::Kind::parse(k).is_none() {
                return Err(bad(format!("no kind {k:?}")));
            }
            conds.push("kind = :k".into());
            params = params.set("k", Value::Str(k.to_string()));
        }
        if let Some(b) = before {
            conds.push("seq < :b".into());
            params = params.set("b", Value::Int(b));
        }
        let wher = conds.join(" AND ");
        let q = q.map(str::trim).filter(|q| !q.is_empty());
        let sql = match q {
            Some(_) => format!("SELECT {COLUMNS} FROM {t} WHERE {wher} ORDER BY bm25(content, :q, syntax => 'plain') DESC LIMIT {limit}"),
            None if session.is_some_and(|s| !s.is_empty()) => format!("SELECT {COLUMNS} FROM {t} WHERE {wher} ORDER BY seq ASC LIMIT {limit}"),
            None => format!("SELECT {COLUMNS} FROM {t} WHERE {wher} ORDER BY seq DESC LIMIT {limit}"),
        };
        if let Some(q) = q {
            params = params.set("q", Value::Str(q.to_string()));
        }
        let rs = self.cluster.query(&sql, &params)?;
        let records: Vec<Json> = rs.rows.iter().map(|r| record(r)).collect();
        Ok(json!({"project": project, "session": session, "records": records, "more": records.len() == limit}))
    }

    /// `GET /v1/memory/notes?project=&kind=&all=1`: a synced project's
    /// notes, newest first, as a space's are answered; the current ones
    /// unless `all`.
    pub fn memory_notes(&self, project: &str, kind: Option<&str>, all: bool) -> Answer {
        if project.is_empty() {
            return Err(bad("which project? (?project=owner/name)"));
        }
        let Some(t) = self.memory_table(project)? else {
            return Ok(json!({"notes": []}));
        };
        let kinds = match kind.filter(|k| !k.is_empty()) {
            Some("rule" | "preference") => "'preference'".to_string(),
            Some(k @ ("decision" | "fact" | "todo" | "note")) => format!("'{k}'"),
            Some(_) => return Err(bad("kind is decision, rule, fact, todo or note")),
            None => "'decision', 'preference', 'fact', 'todo', 'note'".to_string(),
        };
        let rs = self.cluster.query(
            &format!("SELECT {COLUMNS} FROM {t} WHERE project = :p AND kind IN ({kinds}) ORDER BY seq DESC LIMIT 2000"),
            &Params::new().set("p", Value::Str(project.to_string())),
        )?;
        let mut notes: Vec<Json> = rs.rows.iter().map(|r| note(r)).collect();
        // A copy synced before its note was replaced may not say so: the
        // note that replaced it does.
        let replaced: std::collections::HashMap<String, String> = notes
            .iter()
            .flat_map(|n| n["supersedes"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(|o| (o.to_string(), n["id"].as_str().unwrap_or("").to_string())))
            .collect();
        for n in &mut notes {
            if let Some(by) = replaced.get(n["id"].as_str().unwrap_or("")) {
                n["tag"] = json!("superseded");
                if n["superseded_by"].as_array().is_none_or(|a| a.is_empty()) {
                    n["superseded_by"] = json!([by]);
                }
            }
        }
        if !all {
            notes.retain(|n| n["tag"] != "superseded" && n["tag"] != "done");
        }
        Ok(json!({"notes": notes}))
    }

    /// `GET /v1/memory/overview?project=`: what a synced project knows now,
    /// as [`super::view::overview`] answers a space: its counts, current
    /// rules, decisions, facts and open to-dos, the latest notes that
    /// replaced others, and its latest sessions.
    pub fn memory_overview(&self, project: &str) -> Answer {
        if project.is_empty() {
            return Err(bad("which project? (?project=owner/name)"));
        }
        let Some(t) = self.memory_table(project)? else {
            return Ok(json!({"records": 0, "sessions": 0, "by_kind": {}, "live": {}, "rules": [], "decisions": [], "facts": [], "todos": [], "notes": [], "changes": [], "recent_sessions": []}));
        };
        let p = || Params::new().set("p", Value::Str(project.to_string()));
        let span = self.cluster.query(&format!("SELECT MIN(ts), MAX(ts), COUNT(*) FROM {t} WHERE project = :p AND kind != 'entity'"), &p())?;
        let (first, last, records) = span.rows.first().map_or((0, 0, 0), |r| (int_of(r.first()), int_of(r.get(1)), int_of(r.get(2))));
        let rs = self.cluster.query(
            &format!("SELECT {COLUMNS} FROM {t} WHERE project = :p AND kind IN ('decision', 'preference', 'fact', 'todo', 'note') ORDER BY seq DESC LIMIT 5000"),
            &p(),
        )?;
        let notes: Vec<Json> = rs.rows.iter().map(|r| note(r)).collect();
        let by_id: HashMap<String, &Json> = notes.iter().map(|n| (n["id"].as_str().unwrap_or("").to_string(), n)).collect();
        let mut live: HashMap<String, i64> = HashMap::new();
        let mut lists: HashMap<String, Vec<Json>> = HashMap::new();
        let mut changes = Vec::new();
        // A copy synced before its note was replaced may not say so: the
        // note that replaced it does.
        let replaced: std::collections::HashSet<&str> = notes.iter().flat_map(|n| n["supersedes"].as_array().into_iter().flatten().filter_map(|v| v.as_str())).collect();
        for n in &notes {
            let tag = n["tag"].as_str().unwrap_or("");
            if tag == "superseded" || tag == "done" || replaced.contains(n["id"].as_str().unwrap_or("")) {
                continue;
            }
            let k = n["kind"].as_str().unwrap_or("note").to_string();
            *live.entry(k.clone()).or_default() += 1;
            let l = lists.entry(k).or_default();
            if l.len() < 12 {
                l.push(n.clone());
            }
            let was: Vec<Json> = n["supersedes"].as_array().into_iter().flatten().filter_map(|id| by_id.get(id.as_str().unwrap_or(""))).map(|o| json!({"id": o["id"], "text": o["text"], "ts": o["ts"]})).collect();
            if !was.is_empty() && changes.len() < 8 {
                let mut j = n.clone();
                j["was"] = json!(was);
                changes.push(j);
            }
        }
        let ss = self.memory_sessions(project)?;
        let recent: Vec<Json> = ss["sessions"].as_array().map(|a| a.iter().take(6).cloned().collect()).unwrap_or_default();
        let list = |k: &str| lists.get(k).cloned().unwrap_or_default();
        Ok(json!({
            "project": project, "records": records, "sessions": ss["sessions"].as_array().map_or(0, |a| a.len()), "first": first, "last": last,
            "by_kind": ss["by_kind"], "live": live,
            "rules": list("rule"), "decisions": list("decision"), "facts": list("fact"), "todos": list("todo"), "notes": list("note"),
            "changes": changes, "recent_sessions": recent,
        }))
    }
}

impl App {
    /// `GET /v1/memory/graph?project=`: how a synced project's memory is
    /// connected, as [`super::view::graph_of`] answers a space's.
    pub fn memory_graph(&self, project: &str) -> Answer {
        if project.is_empty() {
            return Err(bad("which project? (?project=owner/name)"));
        }
        let Some(t) = self.memory_table(project)? else {
            return Ok(json!({"nodes": [], "edges": []}));
        };
        let p = || Params::new().set("p", Value::Str(project.to_string()));
        let rs = self.cluster.query(
            &format!("SELECT {COLUMNS} FROM {t} WHERE project = :p AND kind IN ('decision', 'preference', 'fact', 'todo', 'note') ORDER BY seq DESC LIMIT {}", super::view::GRAPH_NOTES),
            &p(),
        )?;
        let notes: Vec<super::view::GraphNote> = rs
            .rows
            .iter()
            .map(|r| {
                let links: Vec<Json> = serde_json::from_str(&str_of(r.get(11))).unwrap_or_default();
                super::view::GraphNote {
                    id: str_of(r.first()),
                    kind: match str_of(r.get(2)).as_str() {
                        "preference" => "rule".to_string(),
                        k => k.to_string(),
                    },
                    tag: str_of(r.get(3)),
                    text: str_of(r.get(9)),
                    ts: int_of(r.get(6)),
                    session: str_of(r.get(1)),
                    links: links.iter().filter_map(|l| Some((l["label"].as_str()?.to_string(), l["to"].as_str()?.to_string()))).collect(),
                }
            })
            .collect();
        let names: HashMap<String, String> = self
            .cluster
            .query(&format!("SELECT rid, content FROM {t} WHERE project = :p AND kind = 'entity' LIMIT 10000"), &p())?
            .rows
            .iter()
            .map(|r| (str_of(r.first()), str_of(r.get(1))))
            .collect();
        let ss = self.memory_sessions(project)?;
        let titles: HashMap<String, String> = ss["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| Some((s["session"].as_str()?.to_string(), s["title"].as_str()?.to_string())))
            .collect();
        Ok(super::view::graph_of(&notes, &names, &titles))
    }
}

/// A synced note as a space's notes are answered (`kind` as callers name
/// it: `rule` for a preference), with the notes it replaced and that
/// replaced it from its links.
fn note(row: &[Value]) -> Json {
    let links: Vec<Json> = serde_json::from_str(&str_of(row.get(11))).unwrap_or_default();
    let linked = |label: &str| -> Vec<Json> { links.iter().filter(|l| l["label"] == label).map(|l| l["to"].clone()).collect() };
    let kind = match str_of(row.get(2)).as_str() {
        "preference" => "rule".to_string(),
        k => k.to_string(),
    };
    json!({
        "id": str_of(row.first()), "kind": kind, "tag": str_of(row.get(3)), "text": str_of(row.get(9)), "ts": int_of(row.get(6)),
        "session": str_of(row.get(1)), "seq": int_of(row.get(5)), "supersedes": linked("supersedes"), "superseded_by": linked("superseded_by"),
    })
}
