//! Apps from a space's memory: ChloeCtx suggests apps a space could become,
//! and the cluster's app engine (chloedb-apps, the app `apps` beside the
//! cluster) builds the one picked, its first rows the notes the person
//! approved.
//!
//! 1. `POST /v1/spaces/:s/apps/suggest`: one model call reads the space's
//!    live notes (and its latest prompts) and proposes two or three apps,
//!    each with the request for the app engine and the note kinds it is
//!    loaded with. Nothing is stored.
//! 2. `GET /v1/spaces/:s/apps/estimate`: what a build costs, measured from
//!    the first builds the engine made on this cluster (`sa_requests`), and
//!    what its spending cap leaves.
//! 3. `POST /v1/spaces/:s/apps`: the build, only with `confirm: true` (the
//!    page asks first, with the estimate). The engine builds the app from
//!    the request; once it is built, a model call turns each approved note
//!    into a row of the app's tables, and the rows are inserted through the
//!    engine. Notes not approved never leave the space: the app gets copies
//!    of the approved ones, and no access to the space's table.
//! 4. `GET /v1/spaces/:s/apps`: the builds, from `ctx_builds`;
//!    `POST /v1/spaces/:s/apps/:id/open`: a one-time link into the studio.
//!
//! The engine is reached through the coordinator (`/v1/apps/apps/...`) with
//! the app's own key, as the command center reaches it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use super::{bad, kind_name, note_kind, now, record_json, Answer, App, Fail, NOTE_KINDS};
use crate::cluster;
use crate::record::Record;
use crate::store::Filter;
use crate::Result;

/// The app engine, through the coordinator.
pub const ENGINE: &str = "/v1/apps/apps";
/// The builds' table.
pub const BUILDS: &str = "ctx_builds";
const CREATE_BUILDS: &str = "CREATE TABLE IF NOT EXISTS ctx_builds (id KEYWORD PRIMARY KEY, space KEYWORD, app KEYWORD, request KEYWORD, title TEXT, description TEXT, kinds KEYWORD, status KEYWORD, items INT, loaded INT, skipped INT, error TEXT, cost FLOAT, created INT, updated INT)";

/// The most notes one build is loaded with.
pub const MAX_ITEMS: usize = 300;
/// Notes a model maps to rows at a time.
const CHUNK: usize = 30;
/// How long a build may take before it counts as failed.
const BUILD_LIMIT: Duration = Duration::from_secs(20 * 60);

const SUGGEST_SYSTEM: &str = "You read what a team's long-term memory holds (decisions, rules, facts, plans and notes written down from their conversations with agents) and suggest small apps that would put it to use. An app builder will build the one they pick, and its first rows will be these notes. Output JSON only.";

const SUGGEST_PROMPT: &str = r#"The memory space "{space}" ({about}) holds {counts}.

Its current notes, newest first (kind, date, text):
{notes}

What people asked lately:
{prompts}

Suggest {n} different apps someone working with this memory would use every week, each one useful from its first day because these notes are its first rows: a tracker of the decisions and their owners, a checklist of the rules, a board of the open plans, a glossary of the facts, a timeline, a calculator for a rule's numbers, and so on. Fit the app to what the notes are about (their subject, their numbers, their people), not to notes in general.

Answer with one JSON object:
{"apps": [{"title": "a short name", "pitch": "one sentence for the person: what they do with it", "why": "one sentence: which notes make it fit", "kinds": ["the note kinds it is loaded with: decision, rule, fact, todo, note"], "description": "the request to the app builder, 3 to 6 sentences in plain words: what the app is for, its screens, the tables it keeps and their columns, and that its rows start as records loaded from this memory (each with its text, kind and date)"}]}"#;

const ROWS_SYSTEM: &str = "You load records from a team's memory into the tables of an app that was just built for them. Output JSON only.";

const ROWS_PROMPT: &str = r#"The app "{title}" was built from this request:
{description}

Its tables (name, description, columns):
{tables}

Records to load (id, kind, date, text):
{records}

Turn each record into a row of the table it belongs in: fill each column the record's words give (a date as YYYY-MM-DD, a number as a number, a keyword as one of the values the column expects), and leave out the columns it does not give, the id and the created time. A record that fits no table, or would repeat a row already given here, is left out. Never write a record that is not in the list.

Answer with one JSON object: {"rows": [{"from": "the record's id", "table": "a table's name", "fields": {"column": "value"}}]}"#;

fn clip(s: &str, n: usize) -> String {
    crate::llm::clip(&crate::redact::redact(s), n)
}

fn day_of(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The note kinds a body names, by their callers' names; all of them when
/// it names none.
pub fn kinds_of(b: &Json) -> std::result::Result<Vec<String>, Fail> {
    let mut out = Vec::new();
    for k in b["kinds"].as_array().into_iter().flatten() {
        let k = k.as_str().unwrap_or("");
        let (kind, _) = note_kind(k).ok_or_else(|| bad(format!("kinds are decision, rule, fact, todo and note, not {k:?}")))?;
        let name = super::name_of_kind(kind).to_string();
        if !out.contains(&name) {
            out.push(name);
        }
    }
    if out.is_empty() {
        out = NOTE_KINDS.iter().map(|k| super::name_of_kind(*k).to_string()).collect();
    }
    Ok(out)
}

/// The median of the engine's first builds, from its requests: for each app,
/// its first request and the look at its first version that follows.
pub fn first_build_costs(rows: &[(String, String, f64)]) -> Vec<f64> {
    let mut per: Vec<(String, f64, bool)> = Vec::new();
    for (app, source, cost) in rows {
        match per.iter_mut().find(|p| &p.0 == app) {
            None if source == "user" => per.push((app.clone(), *cost, false)),
            Some(p) if source == "look" && !p.2 => {
                p.1 += cost;
                p.2 = true;
            }
            _ => {}
        }
    }
    per.into_iter().map(|p| p.1).collect()
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// Rows a model gave for the records it was given: each from one of them,
/// into one of the app's tables, with fields.
pub fn rows_of(reply: &str, ids: &[String], tables: &[String]) -> Vec<(String, String, Json)> {
    let Some(j) = crate::llm::json_object(reply) else { return Vec::new() };
    let mut out = Vec::new();
    for r in j["rows"].as_array().into_iter().flatten() {
        let (Some(from), Some(table)) = (r["from"].as_str(), r["table"].as_str()) else { continue };
        if !ids.iter().any(|i| i == from) || !tables.iter().any(|t| t == table) || !r["fields"].is_object() {
            continue;
        }
        let mut fields = r["fields"].clone();
        if let Some(o) = fields.as_object_mut() {
            o.remove("id");
            o.remove("created");
        }
        out.push((from.to_string(), table.to_string(), fields));
    }
    out
}

impl App {
    /// The app engine beside the cluster, as the coordinator lists it, and
    /// what it may still spend.
    fn engine(&self) -> std::result::Result<Json, Fail> {
        let apps = self.cluster.get("/v1/apps").map_err(|f| Fail(502, format!("the coordinator's apps: {f}")))?;
        let up = apps["apps"].as_array().into_iter().flatten().find(|a| a["name"] == "apps");
        match up {
            None => return Err(Fail(503, "no app engine beside this cluster: an app is built by chloedb-apps, the app `apps` (spec.apps of a StrataCluster)".into())),
            Some(a) if a["up"] == false => return Err(Fail(503, format!("the app engine does not answer: {}", a["error"].as_str().unwrap_or("down")))),
            Some(_) => {}
        }
        let status = self.cluster.get(&format!("{ENGINE}/api/status")).map_err(|f| Fail(502, format!("the app engine: {f}")))?;
        Ok(status)
    }

    /// The live notes of a space of the kinds named, the newest first.
    fn approved(&self, space: &str, kinds: &[String], exclude: &[String], limit: usize) -> std::result::Result<Vec<Record>, Fail> {
        let store = self.space(space)?;
        let ks: Vec<_> = kinds.iter().filter_map(|k| note_kind(k).map(|x| x.0)).collect();
        let got = store.latest(&ks, (limit * 3).min(cluster::WINDOW), &Filter::default())?;
        Ok(got.into_iter().filter(|r| !crate::record::closed_tag(&r.tag) && r.tag != "done" && !exclude.contains(&r.id)).take(limit).collect())
    }

    /// What a build costs here, measured, and what the engine may spend.
    pub fn estimate(&self) -> Answer {
        let status = self.engine()?;
        let rs = self.cluster.query("SELECT app, source, cost FROM sa_requests WHERE status = 'done' ORDER BY created ASC LIMIT 5000", &Default::default());
        let rows: Vec<(String, String, f64)> = rs
            .map(|rs| {
                rs.rows
                    .iter()
                    .map(|r| (r.first().and_then(|v| v.as_str()).unwrap_or("").to_string(), r.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string(), r.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0)))
                    .collect()
            })
            .unwrap_or_default();
        let costs = first_build_costs(&rows);
        let n = costs.len();
        let usd = median(costs).map(|m| (m * 100.0).round() / 100.0);
        let spend = &status["spend"];
        let left = spend["cap"].as_f64().map(|cap| ((cap - spend["usd"].as_f64().unwrap_or(0.0)) * 100.0).round() / 100.0);
        Ok(json!({"usd": usd, "builds": n, "engine_model": status["model"], "loading_model": self.model.json(), "cap": spend["cap"], "spent": spend["usd"], "left": left}))
    }

    /// Two or three apps the space could become.
    pub fn suggest(&self, space: &str, b: &Json) -> Answer {
        let store = self.space(space)?;
        let row = self.space_row(space)?.unwrap_or(Json::Null);
        let notes = self.approved(space, &kinds_of(&json!({}))?, &[], 150)?;
        if notes.is_empty() {
            return Err(bad("the space has no notes yet: an app is suggested from its decisions, rules, facts, plans and notes"));
        }
        let mut counts: Vec<(String, usize)> = Vec::new();
        for r in &notes {
            let k = kind_name(r).to_string();
            match counts.iter_mut().find(|c| c.0 == k) {
                Some(c) => c.1 += 1,
                None => counts.push((k, 1)),
            }
        }
        let mut lines = String::new();
        for r in &notes {
            if lines.len() > 24_000 {
                break;
            }
            lines.push_str(&format!("- {}, {}: {}\n", kind_name(r), day_of(r.ts), clip(&r.text, 400)));
        }
        let prompts: Vec<String> = store.latest(&[crate::record::Kind::Prompt], 12, &Filter::default())?.iter().map(|r| format!("- {}", clip(&r.text, 240))).collect();
        let n = b["n"].as_u64().unwrap_or(3).clamp(2, 3);
        let about = [row["title"].as_str().unwrap_or(space), row["description"].as_str().unwrap_or("")].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(": ");
        let prompt = SUGGEST_PROMPT
            .replace("{space}", space)
            .replace("{about}", &about)
            .replace("{counts}", &counts.iter().map(|(k, n)| format!("{n} {k}")).collect::<Vec<_>>().join(", "))
            .replace("{notes}", &lines)
            .replace("{prompts}", &if prompts.is_empty() { "(none kept)".to_string() } else { prompts.join("\n") })
            .replace("{n}", &n.to_string());
        let reply = self.model.complete(SUGGEST_SYSTEM, &prompt).map_err(|e| Fail(503, e.0))?;
        let j = crate::llm::json_object(&reply.text).ok_or_else(|| Fail(502, "the model's suggestions were not JSON".into()))?;
        let have: Vec<&str> = counts.iter().map(|c| c.0.as_str()).collect();
        let apps: Vec<Json> = j["apps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["title"].as_str().is_some_and(|t| !t.trim().is_empty()) && a["description"].as_str().is_some_and(|d| d.trim().len() >= 20))
            .take(3)
            .map(|a| {
                // The kinds it names that the space has (a kind it made up is dropped).
                let mut kinds: Vec<String> = Vec::new();
                for k in a["kinds"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).filter_map(note_kind) {
                    let name = super::name_of_kind(k.0).to_string();
                    if have.contains(&name.as_str()) && !kinds.contains(&name) {
                        kinds.push(name);
                    }
                }
                let kinds = if kinds.is_empty() { have.iter().map(|k| k.to_string()).collect() } else { kinds };
                let items: usize = counts.iter().filter(|c| kinds.contains(&c.0)).map(|c| c.1).sum();
                json!({"title": clip(a["title"].as_str().unwrap_or(""), 80), "pitch": clip(a["pitch"].as_str().unwrap_or(""), 300), "why": clip(a["why"].as_str().unwrap_or(""), 300),
                    "description": clip(a["description"].as_str().unwrap_or(""), 3000), "kinds": kinds, "items": items})
            })
            .collect();
        if apps.is_empty() {
            return Err(Fail(502, "the model suggested no app".into()));
        }
        Ok(json!({"space": space, "apps": apps, "counts": counts.iter().map(|(k, n)| (k.clone(), json!(n))).collect::<serde_json::Map<_, _>>(), "notes": notes.len(), "usage": reply.usage.json()}))
    }

    /// The notes a build would be loaded with, for the person to look over.
    pub fn build_items(&self, space: &str, b: &Json) -> Answer {
        let kinds = kinds_of(b)?;
        let notes = self.approved(space, &kinds, &[], MAX_ITEMS)?;
        Ok(json!({"kinds": kinds, "notes": notes.iter().map(record_json).collect::<Vec<_>>(), "max": MAX_ITEMS}))
    }

    fn put_build(&self, b: &Json) -> Result<()> {
        let mut doc = b.clone();
        doc["updated"] = json!(now());
        self.cluster.write(vec![json!({"op": "upsert", "table": BUILDS, "doc": doc})])
    }

    fn get_build(&self, id: &str) -> Result<Option<Json>> {
        Ok(self.builds_where(&format!("id = {}", cluster::lit(id)))?.pop())
    }

    fn builds_where(&self, cond: &str) -> Result<Vec<Json>> {
        const COLS: [&str; 15] = ["id", "space", "app", "request", "title", "description", "kinds", "status", "items", "loaded", "skipped", "error", "cost", "created", "updated"];
        let rs = match self.cluster.query(&format!("SELECT {} FROM {BUILDS} WHERE {cond} ORDER BY created DESC LIMIT 100", COLS.join(", ")), &Default::default()) {
            Ok(rs) => rs,
            // No build yet: no table.
            Err(e) if e.0.contains(BUILDS) || e.0.contains("no table") || e.0.contains("unknown table") => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(rs
            .rows
            .iter()
            .map(|r| {
                let mut o = serde_json::Map::new();
                for (i, c) in COLS.iter().enumerate() {
                    o.insert(c.to_string(), r.get(i).map(cluster::value_json).unwrap_or(Json::Null));
                }
                Json::Object(o)
            })
            .collect())
    }

    /// The builds made from a space, the newest first.
    pub fn builds(&self, space: &str) -> Answer {
        self.space(space)?;
        Ok(json!({"builds": self.builds_where(&format!("space = {}", cluster::lit(space)))?}))
    }

    /// Builds an app from a space: only with `confirm`, since it spends the
    /// app engine's model budget.
    pub fn build(self: &Arc<Self>, space: &str, b: &Json) -> Answer {
        self.space(space)?;
        let title = b["title"].as_str().unwrap_or("").trim().to_string();
        let description = b["description"].as_str().unwrap_or("").trim().to_string();
        if title.is_empty() || description.len() < 20 {
            return Err(bad("a build needs a \"title\" and a \"description\" of a sentence or more"));
        }
        let kinds = kinds_of(b)?;
        let exclude: Vec<String> = b["exclude"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect();
        let items = self.approved(space, &kinds, &exclude, MAX_ITEMS)?;
        if items.is_empty() {
            return Err(bad("no notes of those kinds are left to load: pick other kinds, or keep some notes in"));
        }
        let estimate = self.estimate()?;
        if b["confirm"] != true {
            return Err(Fail(409, format!("building spends the app engine's model budget (about ${} a build here): send \"confirm\": true once the person agreed", estimate["usd"].as_f64().map_or("?".into(), |u| format!("{u:.2}")))));
        }
        if estimate["left"].as_f64().is_some_and(|l| l <= 0.0) {
            return Err(Fail(402, "the app engine has spent its model budget (spec.apps.spendCapUsd)".into()));
        }
        cluster::check_name(space)?;
        self.cluster.sql(CREATE_BUILDS)?;
        let request = format!(
            "{description}\n\nIts first rows are {} records from the team's memory (ChloeCtx space \"{space}\": {}), each a note with its text, kind and date, loaded into its tables once it is built: leave out example rows.",
            items.len(),
            kinds.join(", ")
        );
        let made = self.cluster.post(&format!("{ENGINE}/api/apps"), &json!({"description": request, "name": title})).map_err(|f| Fail(502, format!("the app engine: {f}")))?;
        // The engine answers `{app: "<id>", request}`; an object with an id is taken too.
        let app_id = made["app"].as_str().or_else(|| made["app"]["id"].as_str()).unwrap_or("").to_string();
        if app_id.is_empty() {
            return Err(Fail(502, format!("the app engine made no app: {made}")));
        }
        let doc = json!({
            "id": format!("b{}{app_id}", now()), "space": space, "app": app_id, "request": made["request"], "title": title, "description": description, "kinds": kinds.join(","),
            "status": "building", "items": items.len(), "loaded": 0, "skipped": 0, "error": "", "cost": 0.0, "created": now(),
        });
        self.put_build(&doc)?;
        let (me, b, ids) = (self.clone(), doc.clone(), items.iter().map(|r| r.id.clone()).collect::<Vec<_>>());
        std::thread::spawn(move || me.follow(b, Some(ids)));
        Ok(json!({"build": doc, "estimate": estimate}))
    }

    /// Follows a build to its end: the engine's build, then the rows. With no
    /// `ids` (after a restart), the notes are the space's of its kinds again.
    fn follow(self: Arc<Self>, mut b: Json, ids: Option<Vec<String>>) {
        let r = self.follow_inner(&mut b, ids);
        if let Err(e) = r {
            b["status"] = json!("failed");
            b["error"] = json!(crate::llm::clip(&e, 2000));
        }
        if let Err(e) = self.put_build(&b) {
            eprintln!("chloectx app: build {}: {}", b["id"], e.0);
        }
    }

    fn follow_inner(&self, b: &mut Json, ids: Option<Vec<String>>) -> std::result::Result<(), String> {
        let app_id = b["app"].as_str().unwrap_or("").to_string();
        let request = b["request"].as_str().unwrap_or("").to_string();
        let t0 = Instant::now();
        let state = loop {
            if t0.elapsed() > BUILD_LIMIT {
                return Err(format!("the app engine did not finish building within {} minutes", BUILD_LIMIT.as_secs() / 60));
            }
            std::thread::sleep(Duration::from_secs(3));
            let s = match self.cluster.get(&format!("{ENGINE}/api/apps/{app_id}")) {
                Ok(s) => s,
                Err(f) if f.status == 404 => return Err("the app engine no longer has the app".into()),
                Err(_) => continue,
            };
            let req = s["requests"].as_array().into_iter().flatten().find(|r| r["id"] == request.as_str()).cloned().unwrap_or(Json::Null);
            match req["status"].as_str() {
                Some("done") => {
                    b["cost"] = json!(req["cost"].as_f64().unwrap_or(0.0));
                    break s;
                }
                Some("failed") => return Err(format!("the app engine could not build it: {}", req["error"].as_str().unwrap_or("failed"))),
                _ => {}
            }
        };
        b["status"] = json!("loading");
        self.put_build(b).map_err(|e| e.0)?;
        let space = b["space"].as_str().unwrap_or("").to_string();
        let kinds: Vec<String> = b["kinds"].as_str().unwrap_or("").split(',').filter(|s| !s.is_empty()).map(str::to_string).collect();
        let store = self.store(&space).map_err(|e| e.0)?;
        let items: Vec<Record> = match ids {
            Some(ids) => store.get(&ids).map_err(|e| e.0)?,
            None => self.approved(&space, &kinds, &[], MAX_ITEMS).map_err(|f| f.1)?,
        };
        let tables: Vec<&Json> = state["tables"].as_array().into_iter().flatten().collect();
        let names: Vec<String> = tables.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect();
        if names.is_empty() {
            return Err("the app was built with no tables to load the notes into".into());
        }
        let about = tables
            .iter()
            .map(|t| format!("- {}: {}\n  columns: {}", t["name"].as_str().unwrap_or(""), t["description"].as_str().unwrap_or(""), crate::llm::clip(&t["columns"].to_string(), 3000)))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut loaded, mut skipped, mut failures) = (0usize, 0usize, Vec::new());
        for chunk in items.chunks(CHUNK) {
            let records = chunk.iter().map(|r| format!("- {}, {}, {}: {}", r.id, kind_name(r), day_of(r.ts), clip(&r.text, 600))).collect::<Vec<_>>().join("\n");
            let prompt = ROWS_PROMPT
                .replace("{title}", b["title"].as_str().unwrap_or(""))
                .replace("{description}", &clip(b["description"].as_str().unwrap_or(""), 3000))
                .replace("{tables}", &about)
                .replace("{records}", &records);
            let reply = self.model.complete(ROWS_SYSTEM, &prompt).map_err(|e| format!("loading the notes: {}", e.0))?;
            let ids: Vec<String> = chunk.iter().map(|r| r.id.clone()).collect();
            let rows = rows_of(&reply.text, &ids, &names);
            let used: std::collections::HashSet<&str> = rows.iter().map(|r| r.0.as_str()).collect();
            skipped += chunk.len() - used.len();
            for (_, table, fields) in &rows {
                match self.cluster.post(&format!("{ENGINE}/api/apps/{app_id}/insert"), &json!({"table": table, "row": fields})) {
                    Ok(_) => loaded += 1,
                    Err(f) => {
                        if failures.len() < 3 {
                            failures.push(f.message.clone());
                        }
                    }
                }
            }
            b["loaded"] = json!(loaded);
            b["skipped"] = json!(skipped);
            let _ = self.put_build(b);
        }
        b["status"] = json!("ready");
        if !failures.is_empty() {
            b["error"] = json!(format!("some rows were refused: {}", failures.join("; ")));
        }
        Ok(())
    }

    /// Builds a restart left unfinished: followed again.
    pub fn resume_builds(self: &Arc<Self>) {
        let Ok(open) = self.builds_where("status IN ('building', 'loading')") else { return };
        for b in open {
            let me = self.clone();
            std::thread::spawn(move || me.follow(b, None));
        }
    }

    /// A one-time link into the studio, at the built app.
    pub fn open_build(&self, space: &str, id: &str) -> Answer {
        let b = self.get_build(id)?.filter(|b| b["space"] == space).ok_or_else(|| Fail(404, format!("no build {id} of {space}")))?;
        let app = b["app"].as_str().unwrap_or("");
        let link = self.cluster.post(&format!("{ENGINE}/api/handoff"), &json!({"next": format!("#/app/{app}")})).map_err(|f| Fail(if f.status == 409 { 409 } else { 502 }, format!("the studio: {}", f.message)))?;
        Ok(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_build_is_its_first_request_and_the_look_after_it() {
        let rows = [("a", "user", 0.10), ("a", "look", 0.03), ("a", "user", 0.05), ("a", "look", 0.02), ("b", "look", 9.0), ("b", "user", 0.20), ("c", "user", 0.12)];
        let rows: Vec<(String, String, f64)> = rows.iter().map(|(a, s, c)| (a.to_string(), s.to_string(), *c)).collect();
        let mut got = first_build_costs(&rows);
        got.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(got.len(), 3);
        assert!((got[0] - 0.12).abs() < 1e-9 && (got[1] - 0.13).abs() < 1e-9 && (got[2] - 0.20).abs() < 1e-9, "{got:?}");
        assert_eq!(median(got), Some(0.13));
        assert_eq!(median(vec![]), None);
    }

    #[test]
    fn rows_come_only_from_the_records_given_into_the_apps_tables() {
        let reply = r#"Here: {"rows": [
            {"from": "r1", "table": "decisions", "fields": {"title": "Refunds", "id": "x", "created": "2020"}},
            {"from": "r9", "table": "decisions", "fields": {"title": "invented"}},
            {"from": "r2", "table": "secrets", "fields": {"title": "wrong table"}},
            {"from": "r2", "table": "rules", "fields": "not an object"},
            {"from": "r2", "table": "rules", "fields": {"text": "Invoices in the customer's currency"}}]}"#;
        let ids = vec!["r1".to_string(), "r2".to_string()];
        let tables = vec!["decisions".to_string(), "rules".to_string()];
        let rows = rows_of(reply, &ids, &tables);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].2, json!({"title": "Refunds"}), "no id or created of its own");
        assert_eq!((rows[1].0.as_str(), rows[1].1.as_str()), ("r2", "rules"));
        assert!(rows_of("no json", &ids, &tables).is_empty());
    }

    #[test]
    fn kinds_are_named_as_callers_name_them() {
        assert_eq!(kinds_of(&json!({"kinds": ["decision", "preference", "rule", "todo"]})).ok().unwrap(), ["decision", "rule", "todo"]);
        assert_eq!(kinds_of(&json!({})).ok().unwrap().len(), 5);
        assert!(kinds_of(&json!({"kinds": ["mood"]})).is_err());
    }

    #[test]
    fn days_are_civil_dates() {
        assert_eq!(day_of(0), "1970-01-01");
        assert_eq!(day_of(1_794_268_800), "2026-11-10");
        assert_eq!(day_of(951_782_400), "2000-02-29");
    }
}
