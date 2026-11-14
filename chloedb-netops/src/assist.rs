//! The search box's completions, an object's inspector, and the dashboards.
//!
//! The box above every page completes as it is typed, from the tables and
//! from what was typed before it: an object's id by `PREFIX(id, :w)` over
//! the estate's dictionary (two binary searches a segment, with typos when
//! nothing matches exactly), a site or a customer by any part of its name
//! (`ILIKE` over the substring index), a circuit by `PREFIX`, the values of
//! a `kind:` or `state:` filter by a `GROUP BY` in the place in view, and
//! the rest of a sentence the map's reader ([`crate::ask::read`]) would
//! understand: what it has not been told yet (a kind, a place, a state,
//! what happened, who did it, a reading, a place in the graph) is what is
//! offered next. Questions asked before come back by BM25 over
//! `netops_searches`. In the assistant's box, the questions offered follow
//! the object in view and the question asked before it.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::api::{bad, Answer, Q, Shared};
use crate::ask::{read, vocabulary, Plan, Vocabulary, KINDS};
use crate::estate::{EDGES, EVENTS, NODES, OPERATORS};
use crate::strata::{Row, RowExt};
use crate::telemetry::iso;

pub fn routes() -> Router<Shared> {
    Router::new().route("/v1/suggest", get(suggest)).route("/v1/map/detail", get(detail)).route("/v1/dash", get(dash))
}

static VOCAB: OnceLock<Arc<Vocabulary>> = OnceLock::new();

async fn vocab(q: &Q) -> Arc<Vocabulary> {
    if let Some(v) = VOCAB.get() {
        return v.clone();
    }
    let v = Arc::new(vocabulary(&q.e).await);
    if v.metros.is_empty() {
        return v;
    }
    VOCAB.get_or_init(|| v).clone()
}

/// Only what may sit inside a quoted literal or a parameter.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\'' | '\\' | '\0')).take(120).collect()
}

/// The column of the estate a place's id narrows: a region, a metro, a hub or a site.
async fn scope_of(q: &Q, scope: &str) -> Option<(&'static str, String, String)> {
    if scope.is_empty() {
        return None;
    }
    let rows = q.soft("The place in view", &format!("SELECT id, kind, name FROM {NODES}\nWHERE id = :id"), json!({"id": scope})).await;
    let r = rows.first()?;
    let col = match r.s("kind").as_str() {
        "region" => "region",
        "metro" => "metro",
        "hub" => "hub",
        "site" => "asite",
        _ => return None,
    };
    Some((col, r.s("id"), r.s("name")))
}

#[derive(serde::Deserialize)]
struct SuggestQ {
    #[serde(default)]
    q: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    sel: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    prev: String,
}

struct Items(Vec<Value>);

impl Items {
    fn add(&mut self, kind: &str, text: String, label: String, sub: String, score: f64, extra: Value) {
        if self.0.iter().any(|v| v["text"].as_str() == Some(text.as_str())) {
            return;
        }
        let mut v = json!({"kind": kind, "text": text, "label": label, "sub": sub, "score": score});
        if let Value::Object(m) = extra {
            for (k, x) in m {
                v[k] = x;
            }
        }
        self.0.push(v);
    }
}

const STATES: &[(&str, &str)] = &[("down", "that are down"), ("degraded", "that are degraded"), ("not ok", "that are not ok"), ("impacted", "that are behind a fault")];
const HAPPENED: &[&str] = &["changed in the last day", "changed in the last week", "that went down in the last 3 days", "that failed in the last month", "not changed in the last month"];
const ABOUT: &[&str] = &["about a firmware upgrade", "about power", "about a line card", "about QoS", "about an optic", "about a configuration push", "about fibre"];
const READINGS: &[&str] = &["with latency above 30 ms", "with loss above 2%", "with latency above twice its usual", "ordered by latency"];
const INTENTS: &[(&str, &str)] = &[
    ("wrong", "What is wrong with {x}, and why?"),
    ("changed", "What changed before it started?"),
    ("affected", "Who is affected, and how badly?"),
    ("todo", "What should I do about it?"),
    ("before", "How did faults like this end before?"),
    ("related", "Show what depends on it on the map"),
    ("watch", "Watch it and tell me when it changes"),
];

fn intent_of(text: &str) -> &'static str {
    let t = text.to_lowercase();
    for (id, pat) in INTENTS {
        let head: String = pat.to_lowercase().split("{x}").next().unwrap_or("").chars().take(14).collect();
        if !head.is_empty() && t.starts_with(head.trim_end()) {
            return id;
        }
    }
    ""
}

/// What follows a question: the next things people ask about the same trouble.
fn follow_ups(prev: &str) -> &'static [&'static str] {
    match prev {
        "wrong" => &["changed", "affected", "todo"],
        "changed" => &["todo", "before", "affected"],
        "affected" => &["todo", "related", "watch"],
        "todo" => &["before", "watch", "affected"],
        "before" => &["todo", "watch"],
        "related" => &["affected", "todo"],
        _ => &["wrong", "changed", "affected", "todo"],
    }
}

/// Whether each word typed starts a word of `text`, in order (the last one may be half typed).
fn fits(text: &str, typed: &[String]) -> bool {
    let words: Vec<String> = text.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.').filter(|w| !w.is_empty()).map(str::to_string).collect();
    let mut at = 0;
    for t in typed {
        match words[at..].iter().position(|w| w.starts_with(t.as_str())) {
            Some(p) => at += p + 1,
            None => return false,
        }
    }
    true
}

fn plural(kind: &str) -> String {
    match kind {
        "olt" => "OLTs".into(),
        "ont" => "ONTs".into(),
        "cpe" => "customer routers".into(),
        _ => KINDS.iter().find(|(k, _)| *k == kind).and_then(|(_, names)| names.iter().find(|n| n.ends_with('s') && !n.contains('-'))).map_or_else(|| "objects".into(), |n| n.to_string()),
    }
}

async fn suggest(State(svc): State<Shared>, Query(a): Query<SuggestQ>) -> Answer {
    let q = Q::new(&svc);
    let text = a.q.chars().take(300).collect::<String>();
    let ask_mode = a.mode == "ask";
    // The word being typed, and what was typed before it.
    let (head, w) = match text.rfind(' ') {
        Some(i) => (text[..i].trim_end().to_string(), text[i + 1..].to_string()),
        None => (String::new(), text.clone()),
    };
    let wl = clean(&w.to_lowercase());
    let prev_word = head.rsplit(' ').next().unwrap_or("").to_lowercase();
    let join = |s: &str| if head.is_empty() { s.to_string() } else { format!("{head} {s}") };
    let scope = scope_of(&q, &clean(&a.scope)).await;
    let (scope_sql, scope_p) = match &scope {
        Some((col, id, _)) => (format!(" AND {col} = :scope"), json!(id)),
        None => (String::new(), json!("")),
    };
    let mut items = Items(Vec::new());
    let v = vocab(&q).await;

    // A filter's values: kind:, state:, vendor:, in:.
    if let Some((field, part)) = w.split_once(':') {
        let part_l = part.to_lowercase();
        match field.to_lowercase().as_str() {
            "kind" => {
                let rows = q.soft("Kinds in view, by how many (GROUP BY)", &format!("SELECT kind, COUNT(*) AS n FROM {NODES}\nWHERE layer != 'container'{scope_sql}\nGROUP BY kind ORDER BY n DESC LIMIT 30"), json!({"scope": scope_p})).await;
                for r in rows.iter().filter(|r| r.s("kind").starts_with(&part_l)) {
                    items.add("filter", join(&format!("kind:{}", r.s("kind"))), format!("kind:{}", r.s("kind")), format!("{} {}", r.i("n"), plural(&r.s("kind"))), 50.0 + r.f("n").log10(), json!({"n": r.i("n")}));
                }
            }
            "state" => {
                let rows = q.soft("States in view (GROUP BY)", &format!("SELECT state, COUNT(*) AS n FROM {NODES}\nWHERE layer != 'container'{scope_sql}\nGROUP BY state ORDER BY n DESC LIMIT 10"), json!({"scope": scope_p})).await;
                for r in rows.iter().filter(|r| r.s("state").starts_with(&part_l)) {
                    items.add("filter", join(&format!("state:{}", r.s("state"))), format!("state:{}", r.s("state")), format!("{} objects", r.i("n")), 50.0 + if r.s("state") == "ok" { 0.0 } else { 5.0 }, json!({"n": r.i("n")}));
                }
            }
            "vendor" => {
                for vd in v.vendors.iter().filter(|x| x.to_lowercase().starts_with(&part_l)) {
                    items.add("filter", join(&format!("vendor:{vd}")), format!("vendor:{vd}"), "vendor".into(), 50.0, json!({}));
                }
            }
            "in" => {
                for (id, names) in v.metros.iter().chain(v.regions.iter()) {
                    if names.iter().any(|n| n.starts_with(&part_l)) {
                        items.add("filter", join(&format!("in:{}", names[0].replace(' ', "-"))), format!("in:{}", names[0]), id.clone(), 50.0, json!({"id": id}));
                    }
                }
            }
            _ => {}
        }
        return finish(q, items, &text, scope, ask_mode);
    }
    if w.is_empty() && head.is_empty() && !ask_mode {
        for f in ["kind:", "state:", "vendor:", "in:"] {
            items.add("filter", f.to_string(), f.to_string(), "filter by".into(), 10.0, json!({"partial": true}));
        }
    }

    // Objects, places, customers and circuits whose names start with the word.
    let wants_object = !matches!(prev_word.as_str(), "by" | "about" | "with" | "above" | "below");
    if wl.len() >= 2 && wants_object {
        // A name matches where one of its words starts with what is typed.
        let p = json!({"w": wl, "scope": scope_p, "pre": format!("{wl}%"), "mid": format!("% {wl}%")});
        let mut rows = q.soft("Objects by the start of their id (PREFIX over the dictionary)", &format!("SELECT id, kind, name, state, reason, metro, hub FROM {NODES}\nWHERE PREFIX(id, :w) AND layer != 'container'{scope_sql}\nORDER BY CASE WHEN state = 'ok' THEN 0 ELSE 1 END DESC, tier, id LIMIT 6"), p.clone()).await;
        let mut fuzzy = false;
        if rows.is_empty() && wl.len() >= 4 {
            rows = q.soft("Objects by the start of their id, with a typo (PREFIX, fuzzy)", &format!("SELECT id, kind, name, state, reason, metro, hub FROM {NODES}\nWHERE PREFIX(id, :w, fuzzy => 'auto') AND layer != 'container'{scope_sql}\nORDER BY tier, id LIMIT 5"), p.clone()).await;
            fuzzy = true;
        }
        for r in &rows {
            let st = r.s("state");
            let sub = [crate::assist::kind_name(&r.s("kind")).to_string(), if st == "ok" { String::new() } else { st.clone() }, r.s("hub"), r.s("metro").trim_start_matches("m-").to_string()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            items.add("object", join(&r.s("id")), r.s("id"), sub, if st == "ok" { 40.0 } else { 45.0 } - if fuzzy { 5.0 } else { 0.0 }, json!({"id": r.s("id"), "state": st, "fuzzy": fuzzy}));
        }
        if wl.len() >= 3 {
            let places = q.soft("Places by any part of their name (ILIKE over the substring index)", &format!("SELECT id, kind, name, children FROM {NODES}\nWHERE (name ILIKE :pre OR name ILIKE :mid) AND layer = 'container'{scope_sql}\nORDER BY tier, id LIMIT 5"), p.clone()).await;
            for r in &places {
                // A metro or a region is said by its name; a hub or a site by its hub's id, which the reader knows.
                let word = match r.s("kind").as_str() {
                    "hub" => r.s("id"),
                    "site" => r.s("id").split_once('-').map(|x| x.1.to_string()).unwrap_or_default(),
                    _ => r.s("name"),
                };
                let sub = if word == r.s("name") { format!("{} · {} objects", kind_name(&r.s("kind")), r.i("children")) } else { format!("{} · {} · {} objects", r.s("name"), kind_name(&r.s("kind")), r.i("children")) };
                items.add("place", join(&word), word, sub, 38.0 + r.f("children").max(1.0).log10() * 0.1, json!({"id": r.s("id")}));
            }
            let cust = if !head.is_empty() && prev_word != "customer" && prev_word != "for" { Vec::new() } else { q.soft("Customers by any part of their name (ILIKE over the substring index)", &format!("SELECT customer, COUNT(*) AS n FROM {NODES}\nWHERE (customer ILIKE :pre OR customer ILIKE :mid){scope_sql}\nGROUP BY customer ORDER BY n DESC LIMIT 4"), p.clone()).await };
            for r in &cust {
                items.add("customer", join(&if prev_word == "customer" { r.s("customer") } else { format!("customer {}", r.s("customer")) }), r.s("customer"), format!("customer · {} sites", r.i("n")), 36.0, json!({"customer": r.s("customer")}));
            }
            if scope.is_none() {
                let up = json!({"w": w.to_uppercase()});
                let circ = q.soft("Circuits by the start of their id (PREFIX)", "SELECT id, circuit, provider, plane, status FROM netops_links\nWHERE PREFIX(circuit, :w) LIMIT 4", up).await;
                for r in &circ {
                    items.add("circuit", join(&r.s("circuit")), r.s("circuit"), format!("{} circuit · {} · plane {}", r.s("provider"), r.s("id"), r.s("plane")), 42.0, json!({"id": r.s("id")}));
                }
            }
        }
    }
    // Who did it.
    if prev_word == "by" {
        for o in OPERATORS.iter().filter(|o| o.starts_with(&wl)) {
            items.add("phrase", join(o), o.to_string(), "made changes".into(), 48.0, json!({}));
        }
    }

    if ask_mode {
        ask_items(&q, &mut items, &text, &a, scope.as_ref()).await;
        return finish(q, items, &text, scope, ask_mode);
    }

    // A word that asks for a certain next one: "that are", "not", "in", "about", "with", "changed".
    let lead: Option<Vec<String>> = match prev_word.as_str() {
        "not" => Some(["ok", "down", "degraded"].map(String::from).to_vec()),
        "is" | "are" => Some(["down", "degraded", "not ok", "ok", "behind a fault"].map(String::from).to_vec()),
        "that" | "which" => Some(["are down", "are degraded", "are not ok", "went down in the last 3 days", "failed in the last month", "changed in the last week"].map(String::from).to_vec()),
        "in" => {
            let timed = ["changed", "failed", "down", "upgraded"].iter().any(|t| head.to_lowercase().contains(t));
            let mut l: Vec<String> = match &scope {
                Some((_, _, name)) => vec![name.clone()],
                None => v.metros.iter().map(|(_, n)| n[0].split(' ').map(|p| p[..1].to_uppercase() + &p[1..]).collect::<Vec<_>>().join(" ")).collect(),
            };
            let times = ["the last day", "the last week", "the last month"].map(String::from);
            if timed { l.splice(0..0, times); } else { l.extend(times); }
            Some(l)
        }
        "about" => Some(ABOUT.iter().map(|s| s.trim_start_matches("about ").to_string()).collect()),
        "with" => Some(["latency above 30 ms", "loss above 2%", "latency above twice its usual"].map(String::from).to_vec()),
        "changed" | "failed" | "down" | "upgraded" => Some(["in the last day", "in the last week", "in the last month", "in the last 3 days"].map(String::from).to_vec()),
        "by" if wl.is_empty() => Some(OPERATORS.iter().map(|o| o.to_string()).collect()),
        "behind" | "under" if wl.is_empty() => Some(if a.sel.is_empty() { Vec::new() } else { vec![clean(&a.sel)] }),
        _ => None,
    };
    if let Some(words) = lead {
        for w2 in words.into_iter().filter(|x| x.to_lowercase().starts_with(&wl)) {
            items.add("phrase", join(&w2), w2, String::new(), 55.0, json!({}));
        }
        return finish(q, items, &text, scope, ask_mode);
    }

    // The rest of a sentence: what the reader has not been told yet.
    let known = |_: &str| false;
    let plan: Plan = if head.is_empty() { Plan::default() } else { read(&head, &v, &known) };
    let mut next: Vec<(String, f64)> = Vec::new();
    if plan.kinds.is_empty() {
        let rows = q.soft("Kinds in view, by how many (GROUP BY)", &format!("SELECT kind, COUNT(*) AS n FROM {NODES}\nWHERE layer != 'container'{scope_sql}\nGROUP BY kind ORDER BY n DESC LIMIT 20"), json!({"scope": scope_p})).await;
        for r in rows.iter().filter(|r| r.s("kind") != "core") {
            let name = plural(&r.s("kind"));
            let cap = name[..1].to_uppercase() + &name[1..];
            next.push((if head.is_empty() { cap } else { name }, 30.0 + 0.001 * r.f("n").log10()));
        }
    }
    if plan.metro.is_empty() && plan.region.is_empty() && plan.hub.is_empty() && !head.is_empty() {
        match &scope {
            Some((_, _, name)) => next.push((format!("in {name}"), 29.0)),
            None => {
                for (_, names) in v.metros.iter().take(10) {
                    let n = &names[0];
                    next.push((format!("in {}", n.split(' ').map(|p| p[..1].to_uppercase() + &p[1..]).collect::<Vec<_>>().join(" ")), 26.0));
                }
            }
        }
    }
    if !head.is_empty() {
        if plan.states.is_empty() {
            for (_, s) in STATES {
                next.push((s.to_string(), 28.0));
            }
        }
        match &plan.happened {
            None => next.extend(HAPPENED.iter().map(|s| (s.to_string(), 27.0))),
            Some(hp) => {
                if hp.about.is_empty() && hp.cause.is_empty() {
                    next.extend(ABOUT.iter().map(|s| (s.to_string(), 31.0)));
                }
                if hp.author.is_empty() && hp.event != "fault" {
                    next.extend(OPERATORS.iter().take(4).map(|o| (format!("by {o}"), 30.5)));
                }
            }
        }
        if plan.readings.is_empty() {
            next.extend(READINGS.iter().map(|s| (s.to_string(), 26.5)));
        }
        if plan.walk.is_none() {
            if !a.sel.is_empty() {
                next.push((format!("behind {}", clean(&a.sel)), 29.5));
            }
            next.push(("behind ".into(), 25.0));
        }
    }
    // How many each state would find, for the kind and the place already said: one GROUP BY.
    let mut counts: Vec<(String, String)> = Vec::new();
    if !head.is_empty() && plan.states.is_empty() && (!plan.kinds.is_empty() || !plan.metro.is_empty() || scope.is_some()) {
        let mut cond = vec!["layer != 'container'".to_string()];
        if !plan.kinds.is_empty() {
            cond.push(format!("kind IN ({})", plan.kinds.iter().map(|k| format!("'{}'", clean(k))).collect::<Vec<_>>().join(", ")));
        }
        if !plan.metro.is_empty() {
            cond.push(format!("metro = '{}'", clean(&plan.metro)));
        } else if !plan.region.is_empty() {
            cond.push(format!("region = '{}'", clean(&plan.region)));
        }
        let rows = q.soft("What the sentence so far finds, by state (GROUP BY)", &format!("SELECT state, COUNT(*) AS n FROM {NODES}\nWHERE {}{scope_sql}\nGROUP BY state LIMIT 10", cond.join(" AND ")), json!({"scope": scope_p})).await;
        let n = |st: &str| rows.iter().filter(|r| (st == "not ok" && r.s("state") != "ok") || r.s("state") == st).map(|r| r.i("n")).sum::<i64>();
        for (st, phrase) in STATES {
            let k = n(st);
            counts.push((phrase.to_string(), format!("{k} {}", if k == 1 { "object" } else { "objects" })));
        }
    }
    for (phrase, score) in next {
        if phrase.to_lowercase().starts_with(&wl) || wl.is_empty() {
            let partial = phrase.ends_with(' ');
            let t = join(&phrase);
            let label = phrase.clone();
            let sub = counts.iter().find(|(p, _)| *p == phrase).map(|(_, n)| format!("{n} now")).unwrap_or_default();
            items.add("phrase", t, label, sub, score + if wl.is_empty() { 0.0 } else { 10.0 }, json!({"partial": partial}));
        }
    }
    // Asked before.
    if text.trim().len() >= 3 {
        let rows = q.soft("Asked before, by words (BM25 over netops_searches)", "SELECT asked, found, score() AS score FROM netops_searches\nORDER BY bm25(asked, :q) DESC LIMIT 4", json!({"q": text})).await;
        let typed: Vec<String> = text.to_lowercase().split_whitespace().map(str::to_string).collect();
        for r in rows.iter().filter(|r| r.f("score") > 0.0 && fits(&r.s("asked"), &typed)) {
            items.add("history", r.s("asked"), r.s("asked"), format!("asked before · {} found", r.i("found")), 35.0, json!({}));
        }
    }
    finish(q, items, &text, scope, ask_mode)
}

/// The questions the assistant offers: about the object or the place in
/// view, following the question asked before.
async fn ask_items(q: &Q, items: &mut Items, text: &str, a: &SuggestQ, scope: Option<&(&'static str, String, String)>) {
    let typed: Vec<String> = text.to_lowercase().split_whitespace().map(str::to_string).collect();
    let sel = clean(&a.sel);
    let prev = intent_of(&a.prev);
    let mut qs: Vec<(String, f64, &str)> = Vec::new();
    if !sel.is_empty() {
        let order = follow_ups(prev);
        for (rank, id) in order.iter().enumerate() {
            if let Some((_, pat)) = INTENTS.iter().find(|(i, _)| i == id) {
                qs.push((pat.replace("{x}", &sel), 60.0 - rank as f64, id));
            }
        }
        for (id, pat) in INTENTS {
            qs.push((pat.replace("{x}", &sel), 47.0, id));
        }
    }
    let place = scope.map(|s| s.2.clone()).unwrap_or_else(|| "the network".into());
    qs.push((format!("What is broken in {place} right now?"), 45.0, "broken"));
    qs.push((format!("Which customers are affected in {place}?"), 44.0, "customers"));
    qs.push((format!("What changed in {place} in the last day?"), 43.0, "recent"));
    qs.push(("Which incidents are open?".into(), 42.0, "incidents"));
    for (t, score, id) in qs {
        if typed.is_empty() || fits(&t, &typed) {
            items.add("question", t.clone(), t, if id == prev { "asked just now".into() } else { String::new() }, score - if id == prev { 30.0 } else { 0.0 }, json!({"intent": id}));
        }
    }
    if !typed.is_empty() {
        let rows = q.soft("Asked before, by words (BM25 over netops_searches)", "SELECT asked, found, score() AS score FROM netops_searches\nORDER BY bm25(asked, :q) DESC LIMIT 3", json!({"q": text})).await;
        for r in rows.iter().filter(|r| r.f("score") > 0.0 && fits(&r.s("asked"), &typed)) {
            items.add("history", r.s("asked"), r.s("asked"), "asked before".into(), 34.0, json!({}));
        }
    }
}

fn finish(q: Q, mut items: Items, text: &str, scope: Option<(&'static str, String, String)>, ask_mode: bool) -> Answer {
    items.0.sort_by(|a, b| b["score"].as_f64().unwrap_or(0.0).partial_cmp(&a["score"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    items.0.truncate(if ask_mode { 9 } else { 10 });
    q.done(json!({"q": text, "items": items.0, "scope": scope.map(|(c, id, name)| json!({"column": c, "id": id, "name": name}))}))
}

pub fn kind_name(k: &str) -> &'static str {
    match k {
        "region" => "Region",
        "metro" => "Metro",
        "hub" => "Hub",
        "site" => "Site",
        "core" => "Core router",
        "border" => "Border router",
        "agg" => "Aggregation router",
        "firewall" => "Firewall",
        "spine" => "Spine switch",
        "leaf" => "Leaf switch",
        "hub-router" => "Hub router",
        "power" => "Power plant",
        "olt" => "OLT",
        "cell" => "Cell site router",
        "switch" => "Access switch",
        "ont" => "ONT",
        "radio" => "Radio",
        "cpe" => "Customer router",
        _ => "Object",
    }
}

// ---- one object, for the inspector's tabs

#[derive(serde::Deserialize)]
struct DetailQ {
    id: String,
}

/// What the inspector's tabs show of one object: its events by day, the
/// series and logs and configuration the backbone keeps for its devices,
/// the incidents that name it, what depends on it (for the map to light),
/// the customers behind it and the runbooks that read like its trouble.
async fn detail(State(svc): State<Shared>, Query(a): Query<DetailQ>) -> Answer {
    let q = Q::new(&svc);
    let id = clean(&a.id);
    let found = q.rows("The object", &format!("SELECT * FROM {NODES}\nWHERE id = :id"), json!({"id": id})).await?;
    let Some(n) = found.first().cloned() else { return Err(bad(format!("no object {id}"))) };
    let now = q.last();
    let p = json!({"id": id, "since": iso(now - 60 * 86_400), "since6h": iso(now - 6 * 3600), "since3h": iso(now - 3 * 3600)});
    let events = q.rows("What happened to it, sixty days", &format!("SELECT ts, event, severity, cause, text, author, ticket, minutes FROM {EVENTS}\nWHERE node = :id AND ts >= :since\nORDER BY ts DESC LIMIT 400"), p.clone()).await?;
    let mut by_day: BTreeMap<String, [i64; 2]> = BTreeMap::new();
    for e in &events {
        let day: String = e.s("ts").chars().take(10).collect();
        let slot = by_day.entry(day).or_default();
        match e.s("event").as_str() {
            "fault" => slot[0] += 1,
            "change" => slot[1] += 1,
            _ => {}
        }
    }
    let days: Vec<Value> = by_day.iter().map(|(d, [f, c])| json!({"day": d, "faults": f, "changes": c})).collect();
    let core = n.s("layer") == "core";
    let mut out = json!({"node": n, "events": events, "days": days, "core": core});
    if core {
        out["metrics"] = json!(q.soft("Its readings, six hours (a point a minute)", "SELECT ts, cpu, mem, temp_c, bgp_up, sessions FROM netops_device_metrics\nWHERE device = :id AND ts >= :since6h\nORDER BY ts LIMIT 2000", p.clone()).await);
        let links = q.soft("Its links", "SELECT id, src, dst, circuit, provider, plane, capacity_mbps, status, latency_ms FROM netops_links\nWHERE src = :id OR dst = :id LIMIT 60", p.clone()).await;
        if !links.is_empty() {
            let list = links.iter().map(|l| format!("'{}'", clean(&l.s("id")))).collect::<Vec<_>>().join(", ");
            out["link_metrics"] = json!(q.soft("Its links' readings, three hours", &format!("SELECT ts, link, util, loss, rx_dbm, latency_ms, up FROM netops_link_metrics\nWHERE link IN ({list}) AND ts >= :since3h\nORDER BY ts LIMIT 9000"), p.clone()).await);
        }
        out["links"] = json!(links);
        out["logs"] = json!(q.soft("Its log lines, newest first", "SELECT ts, severity, program, message FROM netops_syslog\nWHERE device = :id\nORDER BY ts DESC LIMIT 80", p.clone()).await);
        out["config"] = json!(q.soft("Its configuration, the latest version", "SELECT version, ts, author, change_id, lines, body FROM netops_configs\nWHERE device = :id\nORDER BY version DESC LIMIT 1", p.clone()).await);
        out["incidents"] = json!(q.soft("Incidents that name it (a multi-valued field)", "SELECT id, title, severity, status, started, resolved, root_cause FROM netops_incidents\nWHERE devices = :id\nORDER BY started DESC LIMIT 12", p.clone()).await);
    }
    // What depends on it, for the map to light; and who is behind it.
    let walk = format!("REACHABLE({EDGES}, '{id}', hops => 6, dir => 'out', through => layer != 'core') AND layer != 'core'");
    let at = q.e.estate.as_ref().and_then(|est| est.find(&id).map(|i| (est, i)));
    match at.filter(|_| !q.e.walks_graphs()) {
        // A coordinator does not walk graphs: the app walks its copy of the
        // estate, and reads the states of what it reached that are not ok.
        Some((est, i)) => {
            let below: Vec<u32> = est.below(i, 200_000).into_iter().filter(|j| est.nodes[*j as usize].layer != "core").collect();
            out["below"] = json!(below.iter().take(4000).map(|j| est.nodes[*j as usize].id.clone()).collect::<Vec<_>>());
            let firms: Vec<u32> = below.iter().copied().filter(|j| !est.nodes[*j as usize].customer.is_empty()).collect();
            let hurt = crate::estate::among(&q, "The customers' objects behind it that are not ok", est, &firms, "id", "state != 'ok'", json!({})).await.map(|a| a.rows).unwrap_or_default();
            let hurt: std::collections::HashSet<String> = hurt.iter().map(|r| r.s("id")).collect();
            let mut by: HashMap<&str, (i64, i64)> = HashMap::new();
            for j in &firms {
                let n = &est.nodes[*j as usize];
                let c = by.entry(n.customer.as_str()).or_default();
                c.0 += 1;
                c.1 += i64::from(hurt.contains(&n.id));
            }
            let mut rows: Vec<(&str, i64, i64)> = by.into_iter().map(|(c, (n, h))| (c, n, h)).collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            out["customers"] = json!(rows.into_iter().take(20).map(|(c, n, h)| json!({"customer": c, "n": n, "hurt": h})).collect::<Vec<_>>());
        }
        None => {
            out["below"] = json!(q.soft("What depends on it, for the map (REACHABLE along the edges)", &format!("SELECT id FROM {NODES}\nWHERE {walk}\nLIMIT 4000"), json!({})).await.iter().map(|r| r.s("id")).collect::<Vec<_>>());
            out["customers"] = json!(q.soft("The customers behind it (REACHABLE, grouped)", &format!("SELECT customer, COUNT(*) AS n, SUM(CASE WHEN state = 'ok' THEN 0 ELSE 1 END) AS hurt FROM {NODES}\nWHERE {walk} AND customer != ''\nGROUP BY customer ORDER BY n DESC LIMIT 20"), json!({})).await);
        }
    }
    let words = [n.s("reason"), events.iter().find(|e| e.s("event") == "fault").map(|e| e.s("text")).unwrap_or_default()].into_iter().find(|s| !s.is_empty());
    if let Some(w) = words {
        let w = w.trim_start_matches("Behind ").to_string();
        let semantic = q.e.st().semantic;
        let rank = if semantic { "rrf(bm25(body, :q), similarity(emb, :q))" } else { "bm25(body, :q)" };
        out["runbooks"] = json!(q.soft(&format!("Runbooks that read like its trouble ({})", if semantic { "words and meaning" } else { "words" }), &format!("SELECT id, title, category, uses, wins, score() AS score FROM netops_runbooks\nORDER BY {rank} DESC LIMIT 4"), json!({"q": w})).await);
        out["words"] = json!(w);
    }
    q.done(out)
}

// ---- dashboards

#[derive(serde::Deserialize)]
struct DashQ {
    #[serde(default)]
    scope: String,
}

/// The panels of the dashboards page: the backbone's links and devices
/// over three hours, the services, and the estate by place, state and day.
async fn dash(State(svc): State<Shared>, Query(a): Query<DashQ>) -> Answer {
    let q = Q::new(&svc);
    let now = q.last();
    let scope = scope_of(&q, &clean(&a.scope)).await;
    let (scope_sql, scope_p) = match &scope {
        Some((col, id, _)) => (format!(" AND {col} = :scope"), json!(id)),
        None => (String::new(), json!("")),
    };
    let p = json!({"since": iso(now - 3 * 3600), "last": iso(now), "scope": scope_p});
    let top = q.rows("The busiest links over three hours", "SELECT link, MAX(util) AS peak, AVG(util) AS mean FROM netops_link_metrics\nWHERE ts >= :since\nGROUP BY link ORDER BY peak DESC LIMIT 6", p.clone()).await?;
    let lossy = q.rows("The links that lost the most", "SELECT link, MAX(loss) AS worst, MIN(rx_dbm) AS rx FROM netops_link_metrics\nWHERE ts >= :since\nGROUP BY link ORDER BY worst DESC LIMIT 4", p.clone()).await?;
    let mut ids: Vec<String> = top.iter().chain(lossy.iter()).map(|r| r.s("link")).collect();
    ids.sort();
    ids.dedup();
    let list = ids.iter().map(|l| format!("'{}'", clean(l))).collect::<Vec<_>>().join(", ");
    let series = if ids.is_empty() { Vec::new() } else { q.rows("Their readings, a point a minute", &format!("SELECT ts, link, util, loss, rx_dbm FROM netops_link_metrics\nWHERE link IN ({list}) AND ts >= :since\nORDER BY ts LIMIT 9000"), p.clone()).await? };
    let hot_dev = q.rows("The busiest devices", "SELECT device, MAX(cpu) AS peak FROM netops_device_metrics\nWHERE ts >= :since\nGROUP BY device ORDER BY peak DESC LIMIT 4", p.clone()).await?;
    let dl = hot_dev.iter().map(|r| format!("'{}'", clean(&r.s("device")))).collect::<Vec<_>>().join(", ");
    let dev_series = if hot_dev.is_empty() { Vec::new() } else { q.rows("Their CPU, a point a minute", &format!("SELECT ts, device, cpu, temp_c FROM netops_device_metrics\nWHERE device IN ({dl}) AND ts >= :since\nORDER BY ts LIMIT 9000"), p.clone()).await? };
    let svc_top = q.soft("Services by their worst latency", "SELECT service, MAX(latency_p95) AS worst FROM netops_service_metrics\nWHERE ts >= :since\nGROUP BY service ORDER BY worst DESC LIMIT 4", p.clone()).await;
    let sl = svc_top.iter().map(|r| format!("'{}'", clean(&r.s("service")))).collect::<Vec<_>>().join(", ");
    let svc_series = if svc_top.is_empty() { Vec::new() } else { q.soft("Their p95 latency and errors", &format!("SELECT ts, service, latency_p95, requests, errors FROM netops_service_metrics\nWHERE service IN ({sl}) AND ts >= :since\nORDER BY ts LIMIT 9000"), p.clone()).await };
    let states = q.rows("What is not ok, by metro and state", &format!("SELECT metro, state, COUNT(*) AS n FROM {NODES}\nWHERE layer != 'container' AND state != 'ok'{scope_sql}\nGROUP BY metro, state LIMIT 500"), p.clone()).await?;
    let kinds = q.rows("What is not ok, by kind", &format!("SELECT kind, state, COUNT(*) AS n FROM {NODES}\nWHERE layer != 'container' AND state != 'ok'{scope_sql}\nGROUP BY kind, state LIMIT 200"), p.clone()).await?;
    // Faults a day by cause, two weeks: one grouped query a day.
    let mut heat = Vec::new();
    let day0 = now - now.rem_euclid(86_400);
    for d in (0..14).rev() {
        let (t0, t1) = (day0 - d * 86_400, day0 - (d - 1) * 86_400);
        let rows = q.soft(&format!("Faults {}, by cause (one GROUP BY a day)", if d == 0 { "today".to_string() } else { format!("{d} days ago") }), &format!("SELECT cause, COUNT(*) AS n FROM {EVENTS}\nWHERE event = 'fault' AND ts >= :t0 AND ts < :t1{scope_sql}\nGROUP BY cause LIMIT 20"), json!({"t0": iso(t0), "t1": iso(t1), "scope": scope_p})).await;
        let m: HashMap<String, i64> = rows.iter().map(|r| (r.s("cause"), r.i("n"))).collect();
        heat.push(json!({"day": iso(t0).chars().take(10).collect::<String>(), "by": m}));
    }
    let incidents = q.soft("Incidents in thirty days, by cause", "SELECT category, COUNT(*) AS n, AVG(mttr_min) AS mttr FROM netops_incidents\nWHERE started >= :month\nGROUP BY category ORDER BY n DESC LIMIT 12", json!({"month": iso(now - 30 * 86_400)})).await;
    let group = |rows: &[Row], key: &str, val: &str, only: &[Row], by: &str| -> BTreeMap<String, Vec<Value>> {
        let keep: Vec<String> = only.iter().map(|r| r.s(by)).collect();
        let mut m: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for r in rows.iter().filter(|r| keep.contains(&r.s(key))) {
            m.entry(r.s(key)).or_default().push(json!({"t": r.s("ts"), "v": r.get(val).cloned().unwrap_or(Value::Null)}));
        }
        m
    };
    q.done(json!({"at": iso(now), "scope": scope.map(|(c, id, name)| json!({"column": c, "id": id, "name": name})),
        "links": {"top": top, "lossy": lossy, "util": group(&series, "link", "util", &top, "link"), "loss": group(&series, "link", "loss", &lossy, "link"), "rx": group(&series, "link", "rx_dbm", &lossy, "link")},
        "devices": {"top": hot_dev, "cpu": group(&dev_series, "device", "cpu", &hot_dev, "device")},
        "services": {"top": svc_top, "p95": group(&svc_series, "service", "latency_p95", &svc_top, "service")},
        "estate": {"by_metro": states, "by_kind": kinds, "heat": heat}, "incidents": incidents}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_typed_question_fits_the_questions_it_starts() {
        let t = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        assert!(fits("What changed before it started?", &t("what cha")));
        assert!(fits("What is wrong with switch-s07-h16-lon, and why?", &t("wrong swi")));
        assert!(!fits("What changed before it started?", &t("who")));
        assert_eq!(intent_of("What changed before it started?"), "changed");
        assert_eq!(intent_of("What is wrong with cr2-ams, and why?"), "wrong");
        assert_eq!(follow_ups("wrong")[0], "changed");
        assert_eq!(plural("olt"), "OLTs");
        assert_eq!(plural("switch"), "switches");
    }
}
