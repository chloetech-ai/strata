//! Questions and rules (Decide): what agents ask you (`ask_human`, and
//! permission prompts), what ChloeCtx found about each when it was asked
//! (precedent), your answers, and the rules made from them, which every
//! agent of the repository is then told.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, iso, now_ms, opt_int, opt_str, ts};
use crate::levels::{new_id, Levels};
use crate::screen::{Menu, MenuKind, Waits};
use crate::supervisor::{one_line, ApiError, Supervisor};
use crate::trail::{self, Ran};

#[derive(Clone, Debug, Default)]
pub struct Question {
    pub id: String,
    pub agent: Option<String>,
    pub outcome: Option<String>,
    pub repo: Option<String>,
    /// `ask` (from ask_human) or `permission`.
    pub kind: String,
    pub text: String,
    pub context: Option<String>,
    /// `[{label, detail?, matches_precedent}]`.
    pub options: Json,
    pub recommended: Option<i64>,
    pub precedent: Option<Json>,
    /// `open`, `answered` or `expired`.
    pub state: String,
    pub answer: Option<String>,
    /// `you`, `rule:<id>` or `terminal`.
    pub answered_by: Option<String>,
    pub rule: Option<String>,
    pub created: i64,
    pub answered: Option<i64>,
    /// A permission question's detail (SPEC.md, "Sixth build: Safe
    /// answering"): the whole command or the diff of the edit, the agent
    /// another agent's request is about, and `menu`, the prompt as its
    /// agent's screen showed it (its signature, options and question).
    pub detail: Option<Json>,
}

pub(crate) const Q_COLS: &str = "id, agent, outcome, repo, kind, text, context, options, recommended, precedent, state, answer, answered_by, rule, created, answered, detail";

/// The column the sixth build added.
pub fn migrate(sup: &Supervisor) {
    let _ = sup.db().query("ALTER TABLE questions ADD COLUMN IF NOT EXISTS detail TEXT", &Params::new());
}

impl Question {
    pub(crate) fn of(r: &[Value]) -> Question {
        let js = |v: &Value| get_str(v).and_then(|s| serde_json::from_str::<Json>(&s).ok());
        Question {
            id: get_str(&r[0]).unwrap_or_default(),
            agent: get_str(&r[1]),
            outcome: get_str(&r[2]),
            repo: get_str(&r[3]),
            kind: get_str(&r[4]).unwrap_or_default(),
            text: get_str(&r[5]).unwrap_or_default(),
            context: get_str(&r[6]),
            options: js(&r[7]).unwrap_or(json!([])),
            recommended: get_int(&r[8]),
            precedent: js(&r[9]),
            state: get_str(&r[10]).unwrap_or_default(),
            answer: get_str(&r[11]),
            answered_by: get_str(&r[12]),
            rule: get_str(&r[13]),
            created: get_ms(&r[14]).unwrap_or(0),
            answered: get_ms(&r[15]),
            detail: r.get(16).and_then(js),
        }
    }

    pub fn put(&self, sup: &Supervisor) -> Result<(), ApiError> {
        let mut precedent = self.precedent.as_ref().map(|p| p.to_string());
        if precedent.as_ref().is_some_and(|p| p.len() > 64 * 1024) {
            precedent = Some(json!({ "text": "", "ids": [], "agents": {}, "missing": ["what ChloeCtx found was too long to keep"] }).to_string());
        }
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("agent", opt_str(&self.agent))
            .set("outcome", opt_str(&self.outcome))
            .set("repo", opt_str(&self.repo))
            .set("kind", Value::Str(self.kind.clone()))
            .set("text", Value::Str(self.text.clone()))
            .set("context", opt_str(&self.context))
            .set("options", Value::Str(self.options.to_string()))
            .set("recommended", opt_int(self.recommended))
            .set("precedent", opt_str(&precedent))
            .set("state", Value::Str(self.state.clone()))
            .set("answer", opt_str(&self.answer))
            .set("answered_by", opt_str(&self.answered_by))
            .set("rule", opt_str(&self.rule))
            .set("created", ts(self.created))
            .set("answered", self.answered.map_or(Value::Null, ts))
            .set("detail", self.detail.as_ref().map_or(Value::Null, |d| Value::Str(d.to_string())));
        let names: Vec<String> = Q_COLS.split(", ").map(|c| format!(":{c}")).collect();
        sup.db().query(&format!("INSERT OR REPLACE INTO questions ({Q_COLS}) VALUES ({})", names.join(", ")), &p).map_err(ApiError::internal)?;
        Ok(())
    }

    /// The row as JSON, with its agent's name.
    pub fn to_json(&self, sup: &Supervisor) -> Json {
        let a = self.agent.as_deref().and_then(|id| sup.agent_row(id));
        json!({
            "id": self.id, "agent": self.agent, "agent_name": a.map(|a| a.name), "outcome": self.outcome, "repo": self.repo,
            "kind": self.kind, "text": self.text, "context": self.context, "options": self.options, "recommended": self.recommended,
            "precedent": self.precedent, "state": self.state, "answer": self.answer, "answered_by": self.answered_by, "rule": self.rule,
            "created": iso(self.created), "created_ms": self.created, "answered": self.answered.map(iso), "answered_ms": self.answered,
            "detail": self.detail,
        })
    }

    fn labels(&self) -> Vec<String> {
        self.options.as_array().into_iter().flatten().map(|o| o["label"].as_str().unwrap_or("").to_string()).collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Rule {
    pub id: String,
    pub repo: String,
    pub text: String,
    pub question: Option<String>,
    pub ctx_id: Option<String>,
    pub active: bool,
    pub created: i64,
}

pub(crate) const R_COLS: &str = "id, repo, text, question, ctx_id, active, created";

impl Rule {
    pub(crate) fn of(r: &[Value]) -> Rule {
        Rule {
            id: get_str(&r[0]).unwrap_or_default(),
            repo: get_str(&r[1]).unwrap_or_default(),
            text: get_str(&r[2]).unwrap_or_default(),
            question: get_str(&r[3]),
            ctx_id: get_str(&r[4]),
            active: get_int(&r[5]).unwrap_or(0) != 0,
            created: get_ms(&r[6]).unwrap_or(0),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), ApiError> {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("repo", Value::Str(self.repo.clone()))
            .set("text", Value::Str(self.text.clone()))
            .set("question", opt_str(&self.question))
            .set("ctx_id", opt_str(&self.ctx_id))
            .set("active", Value::Bool(self.active))
            .set("created", ts(self.created));
        sup.db().query(&format!("INSERT OR REPLACE INTO rules ({R_COLS}) VALUES (:id, :repo, :text, :question, :ctx_id, :active, :created)"), &p).map_err(ApiError::internal)?;
        Ok(())
    }

    pub fn to_json(&self) -> Json {
        json!({ "id": self.id, "repo": self.repo, "text": self.text, "question": self.question, "ctx_id": self.ctx_id, "active": self.active, "created": iso(self.created), "created_ms": self.created })
    }
}

pub fn get(sup: &Supervisor, id: &str) -> Result<Question, ApiError> {
    let p = Params::new().set("id", Value::Str(id.to_string()));
    let rows = sup.db().query(&format!("SELECT {Q_COLS} FROM questions WHERE id = :id LIMIT 1"), &p).map_err(ApiError::internal)?;
    rows.first().map(|r| Question::of(r)).ok_or_else(|| ApiError::not_found(format!("no question {id}")))
}

/// Every question, newest first.
pub fn all(sup: &Supervisor) -> Vec<Question> {
    let rows = sup.db().query(&format!("SELECT {Q_COLS} FROM questions ORDER BY created DESC LIMIT 100000"), &Params::new()).unwrap_or_default();
    rows.iter().map(|r| Question::of(r)).collect()
}

fn open_of(sup: &Supervisor, agent: &str) -> Vec<Question> {
    let p = Params::new().set("agent", Value::Str(agent.to_string()));
    let rows = sup.db().query(&format!("SELECT {Q_COLS} FROM questions WHERE agent = :agent AND state = 'open' LIMIT 1000"), &p).unwrap_or_default();
    rows.iter().map(|r| Question::of(r)).collect()
}

/// The agents with an open question.
pub fn asking(sup: &Supervisor) -> HashSet<String> {
    let rows = sup.db().query("SELECT agent FROM questions WHERE state = 'open' LIMIT 100000", &Params::new()).unwrap_or_default();
    rows.iter().filter_map(|r| get_str(&r[0])).collect()
}

/// The rules of a repository (or all), oldest first; only active ones
/// with `active_only`.
pub fn rules(sup: &Supervisor, repo: Option<&str>, active_only: bool) -> Vec<Rule> {
    let rows = match repo {
        Some(r) => sup.db().query(&format!("SELECT {R_COLS} FROM rules WHERE repo = :repo ORDER BY created ASC LIMIT 10000"), &Params::new().set("repo", Value::Str(r.to_string()))),
        None => sup.db().query(&format!("SELECT {R_COLS} FROM rules ORDER BY created ASC LIMIT 10000"), &Params::new()),
    };
    rows.unwrap_or_default().iter().map(|r| Rule::of(r)).filter(|r| !active_only || r.active).collect()
}

/// Open questions whose agent no longer runs expire.
pub fn expire_stale(sup: &Supervisor, lv: &Levels) {
    let _w = lv.write.lock().unwrap();
    let rows = sup.db().query(&format!("SELECT {Q_COLS} FROM questions WHERE state = 'open' LIMIT 100000"), &Params::new()).unwrap_or_default();
    for mut q in rows.iter().map(|r| Question::of(r)) {
        if q.agent.as_deref().is_some_and(|a| !sup.is_running(a)) {
            q.state = "expired".into();
            q.answered = Some(now_ms());
            if q.put(sup).is_ok() {
                if let Some(a) = &q.agent {
                    let mut open = lv.open_perm.lock().unwrap();
                    if open.get(a) == Some(&q.id) {
                        open.remove(a);
                    }
                }
                lv.changed(&q.id);
            }
        }
    }
}

/// `GET /api/questions`.
pub fn list(sup: &Supervisor, lv: &Levels, repo: Option<&str>, state: Option<&str>, agent: Option<&str>) -> Result<Json, ApiError> {
    expire_stale(sup, lv);
    let repo = match repo.filter(|r| !r.is_empty()) {
        Some(r) => Some(trail::repo_dir(Some(r))?.to_string_lossy().to_string()),
        None => None,
    };
    let qs: Vec<Json> = all(sup)
        .into_iter()
        .filter(|q| repo.as_ref().is_none_or(|r| q.repo.as_ref() == Some(r)))
        .filter(|q| state.filter(|s| !s.is_empty()).is_none_or(|s| q.state == s))
        .filter(|q| agent.filter(|s| !s.is_empty()).is_none_or(|a| q.agent.as_deref() == Some(a)))
        .map(|q| q.to_json(sup))
        .collect();
    Ok(json!({ "questions": qs }))
}

/// Words of a label or text: lower case, three letters or more, not the
/// commonest words.
fn words(s: &str) -> Vec<String> {
    const COMMON: [&str; 24] = ["the", "and", "for", "with", "not", "but", "you", "your", "this", "that", "use", "one", "all", "any", "are", "was", "yes", "its", "it's", "from", "into", "then", "than", "per"];
    s.to_lowercase().split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| w.chars().count() >= 3 && !COMMON.contains(w)).map(str::to_string).collect()
}

/// Whether `text` holds every word of `label` (at least one).
pub fn label_matches(label: &str, text: &str) -> bool {
    let want = words(label);
    let have: HashSet<String> = words(text).into_iter().collect();
    !want.is_empty() && want.iter().all(|w| have.contains(w))
}

/// A question's text for comparing: lower case, words only.
fn norm(s: &str) -> String {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// What `POST /api/questions` (ask_human) takes.
#[derive(Debug, Default, Deserialize)]
pub struct AskBody {
    pub agent: Option<String>,
    #[serde(alias = "text")]
    pub question: Option<String>,
    #[serde(default)]
    pub options: Vec<Json>,
    pub recommended: Option<i64>,
    pub context: Option<String>,
}

fn option_of(v: &Json) -> Option<Json> {
    match v {
        Json::String(s) if !s.trim().is_empty() => Some(json!({ "label": s.trim(), "matches_precedent": false })),
        Json::Object(o) => {
            let label = o.get("label").and_then(Json::as_str).map(str::trim).filter(|l| !l.is_empty())?;
            let mut out = json!({ "label": label, "matches_precedent": false });
            if let Some(d) = o.get("detail").and_then(Json::as_str).filter(|d| !d.trim().is_empty()) {
                out["detail"] = json!(d.trim());
            }
            Some(out)
        }
        _ => None,
    }
}

/// `POST /api/questions`: an agent asks you. Its state is `waiting` until
/// you answer; when a rule made from the same question answers it, it is
/// answered at once.
pub fn create_ask(sup: &Arc<Supervisor>, lv: &Arc<Levels>, agent: &str, b: AskBody) -> Result<Json, ApiError> {
    let a = sup.agent_row(agent).ok_or_else(|| ApiError::not_found(format!("no agent {agent}")))?;
    let text = b.question.unwrap_or_default().trim().to_string();
    if text.is_empty() || text.len() > 4000 {
        return Err(ApiError::bad("question is required, at most 4000 bytes"));
    }
    if b.options.len() > 8 {
        return Err(ApiError::bad("at most 8 options"));
    }
    let options: Vec<Json> = b.options.iter().map(|o| option_of(o).ok_or_else(|| ApiError::bad("each option is a label, or {label, detail?}"))).collect::<Result<_, _>>()?;
    if let Some(r) = b.recommended {
        if r < 0 || r as usize >= options.len() {
            return Err(ApiError::bad("recommended is the index of an option, from 0"));
        }
    }
    let mut q = Question {
        id: new_id("q"),
        agent: Some(a.id.clone()),
        outcome: a.outcome.clone(),
        repo: Some(a.repo.clone()),
        kind: "ask".into(),
        text,
        context: b.context.filter(|c| !c.trim().is_empty()),
        options: Json::Array(options),
        recommended: b.recommended,
        precedent: None,
        state: "open".into(),
        created: now_ms(),
        ..Question::default()
    };
    // A rule made from your answer to the same question answers it.
    if let Some((rule, answer)) = rule_answer(sup, &q) {
        q.state = "answered".into();
        q.answer = Some(answer.clone());
        q.answered_by = Some(format!("rule:{}", rule.id));
        q.rule = Some(rule.id.clone());
        q.answered = Some(q.created);
        q.put(sup)?;
        let _ = sup.note(&a.id, "answered", format!("answered by a rule: {}", one_line(&answer, 200)), Some(json!({ "question": q.id, "by": format!("rule:{}", rule.id) }).to_string()), |_| {});
        lv.changed(&q.id);
        return Ok(json!({ "question": q.to_json(sup) }));
    }
    q.put(sup)?;
    let waiting = one_line(&q.text, 500);
    sup.note(&a.id, "question", format!("question: {}", one_line(&q.text, 200)), Some(json!({ "question": q.id }).to_string()), |ag| {
        Supervisor::set_agent_state(ag, "waiting");
        ag.waiting_for = Some(waiting.clone());
        ag.narration = Some(one_line(&waiting, crate::narrate::MAX_CHARS));
        ag.narration_ts = Some(now_ms());
    })?;
    lv.changed(&q.id);
    find_precedent_later(sup, lv, &q.id);
    Ok(json!({ "question": q.to_json(sup) }))
}

/// The active rule made from an answer to the same question, and that
/// answer, when it fits the new question's options.
fn rule_answer(sup: &Supervisor, q: &Question) -> Option<(Rule, String)> {
    let repo = q.repo.as_deref()?;
    let want = norm(&q.text);
    let labels = q.labels();
    for r in rules(sup, Some(repo), true).into_iter().rev() {
        let Some(from) = r.question.as_deref().and_then(|id| get(sup, id).ok()) else { continue };
        if from.kind != "ask" || norm(&from.text) != want {
            continue;
        }
        let Some(ans) = from.answer.clone() else { continue };
        let first = ans.lines().next().unwrap_or("").to_string();
        if labels.is_empty() || labels.iter().any(|l| l == &first) {
            return Some((r, ans));
        }
    }
    None
}

/// Whether a Notification asks permission to use a tool.
pub fn is_permission(hook: &Json) -> bool {
    match hook.get("notification_type").and_then(Json::as_str) {
        Some(t) => t == "permission_prompt",
        None => hook.get("message").and_then(Json::as_str).is_some_and(|m| m.to_lowercase().contains("permission")),
    }
}

/// A permission prompt becomes a question (one open at a time per agent).
/// `pre` is the `PreToolUse` hook it asks about, when the session's last
/// hook was one: the whole command, or the edit's diff, come from it. Its
/// options are the ones its agent's screen shows (none until Workbench has
/// read them there; the screen watcher fills them in).
pub fn create_permission(sup: &Arc<Supervisor>, lv: &Arc<Levels>, agent: &str, hook: &Json, pre: Option<Json>) {
    if lv.open_perm.lock().unwrap().contains_key(agent) {
        return;
    }
    let Some(a) = sup.agent_row(agent) else { return };
    let text = one_line(hook.get("message").and_then(Json::as_str).unwrap_or("Claude needs your permission"), 500);
    let mut q = Question {
        id: new_id("q"),
        agent: Some(a.id.clone()),
        outcome: a.outcome.clone(),
        repo: Some(a.repo.clone()),
        kind: "permission".into(),
        text,
        context: None,
        options: json!([]),
        recommended: None,
        state: "open".into(),
        created: now_ms(),
        ..Question::default()
    };
    set_tool(sup, &mut q, pre.as_ref());
    if let crate::screen::Waits::Menu(m) = sup.waits(agent) {
        if m.kind != MenuKind::Trust {
            attach_menu(&mut q, &m);
        }
    }
    open_question(sup, lv, q, None);
}

/// The question's context and detail from the tool call it asks about.
fn set_tool(sup: &Supervisor, q: &mut Question, pre: Option<&Json>) {
    let Some(pre) = pre else { return };
    let d = detail_of(sup, pre);
    q.context = d["action"].as_str().map(str::to_string).or_else(|| d["summary"].as_str().map(str::to_string));
    q.detail = Some(d);
}

/// Saves a new open question, marks it its agent's open prompt and tells
/// the screens; `note` is the event that says so (none for one a
/// Notification stands for).
fn open_question(sup: &Arc<Supervisor>, lv: &Arc<Levels>, q: Question, note: Option<String>) {
    {
        let _w = lv.write.lock().unwrap();
        if q.put(sup).is_err() {
            return;
        }
    }
    let agent = q.agent.clone().unwrap_or_default();
    lv.open_perm.lock().unwrap().insert(agent.clone(), q.id.clone());
    let line = one_line(&q.text, crate::narrate::MAX_CHARS);
    let waiting = one_line(&q.text, 500);
    let set = |ag: &mut crate::db::Agent| {
        ag.narration = Some(line.clone());
        ag.narration_ts = Some(now_ms());
    };
    match note {
        Some(n) => {
            let _ = sup.note(&agent, "screen", n, Some(json!({ "question": q.id }).to_string()), |ag| {
                set(ag);
                if ag.state != "exited" {
                    Supervisor::set_agent_state(ag, "waiting");
                    ag.waiting_for = Some(waiting.clone());
                }
            });
        }
        None => {
            let _ = sup.update_agent(&agent, set);
        }
    }
    lv.changed(&q.id);
    find_precedent_later(sup, lv, &q.id);
}

/// The options of a menu as a question keeps them: the label the screen
/// shows, its number, and what it grants beyond this one call.
fn options_of(m: &Menu) -> Json {
    let no = crate::screen::no_option(m);
    Json::Array(
        m.options
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let mut v = json!({ "label": o.label, "n": o.n, "no": no == Some(i), "matches_precedent": false });
                if let Some(g) = crate::screen::grants_more(&o.label) {
                    v["grants"] = json!(g);
                }
                v
            })
            .collect(),
    )
}

/// Gives a question the prompt its agent's screen shows.
fn attach_menu(q: &mut Question, m: &Menu) {
    q.options = options_of(m);
    let mut d = q.detail.clone().unwrap_or_else(|| json!({}));
    d["menu"] = json!({ "kind": m.kind.as_str(), "question": m.question, "header": m.header, "footer": m.footer, "sig": m.signature(), "labels": m.labels() });
    q.detail = Some(d);
}

/// The signature of the prompt a question was read from, if it was.
fn menu_sig(q: &Question) -> Option<String> {
    q.detail.as_ref()?.get("menu")?.get("sig")?.as_str().map(str::to_string)
}

/// What a question shows of the tool call a `PreToolUse` hook made: its
/// one line (`summary`), the whole command, the diff of an edit, or, for a
/// request about another agent, that agent by name and state (`action`).
pub fn detail_of(sup: &Supervisor, pre: &Json) -> Json {
    let tool = pre.get("tool_name").and_then(Json::as_str).unwrap_or("");
    let input = pre.get("tool_input").cloned().unwrap_or(Json::Null);
    let s = |k: &str| input.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let mut d = json!({ "tool": tool, "summary": crate::supervisor::summarize("PreToolUse", pre) });
    let cwd = pre.get("cwd").and_then(Json::as_str).unwrap_or("");
    match tool {
        "Bash" => {
            d["command"] = json!(cap(&s("command"), 64 * 1024));
            if !s("description").is_empty() {
                d["description"] = json!(s("description"));
            }
        }
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => {
            let file = if s("file_path").is_empty() { s("notebook_path") } else { s("file_path") };
            d["file"] = json!(file);
            if !cwd.is_empty() {
                if let Ok(rel) = std::path::Path::new(&file).strip_prefix(cwd) {
                    d["rel"] = json!(rel.to_string_lossy());
                    d["summary"] = json!(format!("{tool}: {}", rel.to_string_lossy()));
                }
            }
            let (diff, lines) = edit_diff(tool, &file, &input);
            d["diff"] = json!(diff);
            d["diff_lines"] = json!(lines);
        }
        t if t.starts_with("mcp__workbench__") => {
            let short = &t["mcp__workbench__".len()..];
            let id = s("id");
            let other = (!id.is_empty()).then(|| sup.agent_row(&id)).flatten();
            let who = other.as_ref().map(|o| {
                let running = sup.is_running(&o.id);
                let state = if !running { "exited".to_string() } else if o.state == "waiting" { "waiting for you".into() } else { o.state.clone() };
                let oc = o.outcome.as_deref().and_then(|x| crate::goals::outcome(sup, x).ok());
                d["agent"] = json!({ "id": o.id, "name": o.name, "state": o.state, "running": running, "outcome": oc.as_ref().map(|x| x.title.clone()), "outcome_state": oc.as_ref().map(|x| x.state.clone()) });
                let mut how = state;
                if let Some(oc) = &oc {
                    how.push_str(&format!(", its outcome {}", oc.state.replace('_', " ")));
                }
                format!("{} ({how})", o.name)
            });
            let who = who.unwrap_or_else(|| if id.is_empty() { String::new() } else { format!("agent {id}, which Workbench does not know") });
            let action = match short {
                "stop_agent" => format!("Stop {who}"),
                "send_to_agent" => format!("Type into {who}: “{}”", one_line(&s("text"), 200)),
                "wait_agent" => format!("Wait for {who}"),
                "read_agent" => format!("Read {who}'s events"),
                "spawn_agent" => format!("Start an agent{}: {}", if s("name").is_empty() { String::new() } else { format!(" named {}", s("name")) }, one_line(&s("task"), 200)),
                "blast_radius" | "prepare_pr" if !who.is_empty() => format!("{} for {who}", short.replace('_', " ")),
                other => format!("Use Workbench's {}", other.replace('_', " ")),
            };
            d["action"] = json!(action);
            d["input"] = json!(cap(&input.to_string(), 8 * 1024));
        }
        _ if !input.is_null() => d["input"] = json!(cap(&input.to_string(), 8 * 1024)),
        _ => {}
    }
    d
}

fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The most lines a question's diff keeps.
const DIFF_MAX: usize = 600;

/// The diff an edit would make, as lines starting with ` `, `-` or `+`
/// (`@@ line N` before each hunk when the file shows where), and the number
/// of lines it changes.
fn edit_diff(tool: &str, file: &str, input: &Json) -> (String, usize) {
    let on_disk = std::fs::read_to_string(file).ok();
    let s = |v: &Json, k: &str| v.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let mut out: Vec<String> = Vec::new();
    let mut changed = 0;
    let mut hunk = |old: &str, new: &str, at: Option<usize>, out: &mut Vec<String>| {
        if let Some(n) = at {
            out.push(format!("@@ line {n}"));
        }
        for l in line_diff(old, new) {
            if !l.starts_with(' ') {
                changed += 1;
            }
            out.push(l);
        }
    };
    let line_of = |old: &str| on_disk.as_deref().and_then(|t| if old.is_empty() { None } else { t.find(old) }).map(|i| on_disk.as_deref().unwrap()[..i].matches('\n').count() + 1);
    match tool {
        "Edit" => hunk(&s(input, "old_string"), &s(input, "new_string"), line_of(&s(input, "old_string")), &mut out),
        "MultiEdit" => {
            for e in input.get("edits").and_then(Json::as_array).into_iter().flatten() {
                hunk(&s(e, "old_string"), &s(e, "new_string"), line_of(&s(e, "old_string")), &mut out);
            }
        }
        "Write" => match &on_disk {
            Some(old) => hunk(old, &s(input, "content"), None, &mut out),
            None => {
                out.push("(a new file)".into());
                hunk("", &s(input, "content"), None, &mut out);
            }
        },
        _ => {
            let cell = s(input, "cell_id");
            out.push(format!("({} cell{})", if s(input, "edit_mode").is_empty() { "replaces a" } else { "edits a" }, if cell.is_empty() { String::new() } else { format!(" {cell}") }));
            hunk("", &s(input, "new_source"), None, &mut out);
        }
    }
    if out.len() > DIFF_MAX {
        let more = out.len() - DIFF_MAX;
        out.truncate(DIFF_MAX);
        out.push(format!("… {more} more lines"));
    }
    (out.join("\n"), changed)
}

/// A line diff of `old` and `new`: unchanged lines start with a space,
/// removed ones with `-`, added ones with `+`; at most 3 unchanged lines
/// around a change, `…` for those left out.
pub fn line_diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = if old.is_empty() { Vec::new() } else { old.lines().collect() };
    let b: Vec<&str> = if new.is_empty() { Vec::new() } else { new.lines().collect() };
    let mut ops: Vec<(char, &str)> = Vec::new();
    if a.len() * b.len() > 4_000_000 {
        ops.extend(a.iter().map(|l| ('-', *l)));
        ops.extend(b.iter().map(|l| ('+', *l)));
    } else {
        // Longest common subsequence, from the end.
        let (n, m) = (a.len(), b.len());
        let mut t = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * (m + 1) + j] = if a[i] == b[j] { t[(i + 1) * (m + 1) + j + 1] + 1 } else { t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && a[i] == b[j] {
                ops.push((' ', a[i]));
                i += 1;
                j += 1;
            } else if j < m && (i == n || t[i * (m + 1) + j + 1] >= t[(i + 1) * (m + 1) + j]) {
                ops.push(('+', b[j]));
                j += 1;
            } else {
                ops.push(('-', a[i]));
                i += 1;
            }
        }
        // Removals before additions within a change.
        let mut k = 0;
        while k < ops.len() {
            if ops[k].0 == ' ' {
                k += 1;
                continue;
            }
            let end = (k..ops.len()).find(|&x| ops[x].0 == ' ').unwrap_or(ops.len());
            ops[k..end].sort_by_key(|o| o.0 != '-');
            k = end;
        }
    }
    let near = |i: usize| (i.saturating_sub(3)..(i + 4).min(ops.len())).any(|x| ops[x].0 != ' ');
    let mut out = Vec::new();
    let mut skipped = false;
    for (i, (op, l)) in ops.iter().enumerate() {
        if *op == ' ' && !near(i) {
            skipped = true;
            continue;
        }
        if skipped {
            out.push("…".to_string());
            skipped = false;
        }
        out.push(format!("{op}{l}"));
    }
    out
}

/// The agent did something after its permission prompt: it was answered
/// in its terminal.
pub fn answered_in_terminal(sup: &Supervisor, lv: &Levels, agent: &str) {
    let Some(qid) = lv.open_perm.lock().unwrap().remove(agent) else { return };
    let _w = lv.write.lock().unwrap();
    if let Ok(mut q) = get(sup, &qid) {
        if q.state == "open" {
            q.state = "answered".into();
            q.answered_by = Some("terminal".into());
            q.answer = Some("answered in the terminal".into());
            q.answered = Some(now_ms());
            let _ = q.put(sup);
            lv.changed(&q.id);
        }
    }
}

/// Closes an open question whose prompt is gone (`expired`, by `screen`,
/// the answer saying why), and says so in its agent's events.
fn close_question(sup: &Supervisor, lv: &Levels, qid: &str, why: &str) {
    let agent = {
        let _w = lv.write.lock().unwrap();
        match get(sup, qid) {
            Ok(mut q) if q.state == "open" => {
                q.state = "expired".into();
                q.answered_by = Some("screen".into());
                q.answer = Some(why.to_string());
                q.answered = Some(now_ms());
                let _ = q.put(sup);
                q.agent
            }
            _ => None,
        }
    };
    let Some(agent) = agent else { return };
    {
        let mut open = lv.open_perm.lock().unwrap();
        if open.get(&agent).map(String::as_str) == Some(qid) {
            open.remove(&agent);
        }
    }
    let _ = sup.note(&agent, "screen", format!("question closed: {}", one_line(why, 200)), Some(json!({ "question": qid }).to_string()), |_| {});
    lv.changed(qid);
}

// The screen watcher.

/// What the watcher remembers of an agent's screen.
struct Watched {
    /// What the screen waits on, as a string, and since when.
    reading: String,
    since: Instant,
    /// The trust prompts it has answered (by signature).
    trusted: HashSet<String>,
    /// Workbench set the agent waiting from its screen.
    set_waiting: bool,
}

/// How long a reading must hold before the watcher acts on it.
const SETTLE: Duration = Duration::from_millis(600);
const GONE_AFTER: Duration = Duration::from_millis(1500);
const IDLE_AFTER: Duration = Duration::from_millis(2500);

fn reading_of(w: &Waits) -> String {
    match w {
        Waits::Menu(m) => format!("menu:{}:{}", m.signature(), m.selected),
        Waits::Input { interrupted } => format!("input:{interrupted}"),
        Waits::Busy => "busy".into(),
        Waits::Unknown => "unknown".into(),
    }
}

/// Watches every running agent's screen (SPEC.md, "Sixth build: Safe
/// answering"): a menu that waits on a key becomes a question (a
/// Notification may come late, or never: the trust prompt, a dialog), the
/// trust prompt of a worktree Workbench made is answered, a question whose
/// prompt left the screen is closed, and an agent that sits at its prompt
/// (after an interrupt, say) is idle, so that a steer reaches it.
pub fn watch_screens(sup: Arc<Supervisor>, lv: Arc<Levels>) {
    let _ = std::thread::Builder::new().name("screens".into()).spawn(move || {
        let mut seen: HashMap<String, Watched> = HashMap::new();
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let ids: Vec<String> = sup.agent_rows().into_iter().map(|a| a.id).filter(|id| sup.is_running(id)).collect();
            seen.retain(|id, _| ids.contains(id));
            for id in ids {
                if lv.typing.lock().unwrap().contains(&id) {
                    continue;
                }
                let w = sup.waits(&id);
                let r = reading_of(&w);
                let st = seen.entry(id.clone()).or_insert_with(|| Watched { reading: String::new(), since: Instant::now(), trusted: HashSet::new(), set_waiting: false });
                if st.reading != r {
                    st.reading = r;
                    st.since = Instant::now();
                }
                let held = st.since.elapsed();
                act(&sup, &lv, &id, w, held, st);
            }
        }
    });
}

fn act(sup: &Arc<Supervisor>, lv: &Arc<Levels>, id: &str, w: Waits, held: Duration, st: &mut Watched) {
    let Some(a) = sup.agent_row(id) else { return };
    let open = lv.open_perm.lock().unwrap().get(id).cloned().and_then(|q| get(sup, &q).ok()).filter(|q| q.state == "open");
    match w {
        Waits::Menu(m) => {
            if held < SETTLE {
                return;
            }
            let sig = m.signature();
            if let Some(q) = &open {
                match menu_sig(q) {
                    Some(s) if s == sig => return,
                    None if m.kind != MenuKind::Trust => {
                        // A Notification's question: now with the options.
                        let _w = lv.write.lock().unwrap();
                        if let Ok(mut q) = get(sup, &q.id) {
                            attach_menu(&mut q, &m);
                            if q.put(sup).is_ok() {
                                lv.changed(&q.id);
                            }
                        }
                        return;
                    }
                    _ => close_question(sup, lv, &q.id, &format!("{}'s screen shows another prompt now", a.name)),
                }
            }
            // A Workbench worktree, or a folder of a repository the person
            // trusted when they opened it (POST /api/repos with trust).
            let trusted = workbench_worktree(sup, &a.cwd) || (std::path::Path::new(&a.cwd).starts_with(&a.repo) && crate::repos::trusted(sup, &a.repo));
            if m.kind == MenuKind::Trust && trusted && st.trusted.insert(sig.clone()) {
                trust_later(sup, lv, id, m);
                return;
            }
            let mut q = Question {
                id: new_id("q"),
                agent: Some(a.id.clone()),
                outcome: a.outcome.clone(),
                repo: Some(a.repo.clone()),
                kind: "permission".into(),
                text: match m.kind {
                    MenuKind::Trust => "Trust this folder?".into(),
                    _ if !m.question.is_empty() => one_line(&m.question, 500),
                    _ => "Claude Code waits on a choice".into(),
                },
                options: json!([]),
                state: "open".into(),
                created: now_ms(),
                ..Question::default()
            };
            if m.kind == MenuKind::Permission {
                set_tool(sup, &mut q, lv.last_pre.lock().unwrap().get(id));
            }
            if m.kind == MenuKind::Trust {
                q.context = Some(format!("Claude Code asks whether to trust {}", a.cwd));
            }
            attach_menu(&mut q, &m);
            st.set_waiting = true;
            let note = format!("waits on a prompt: {}", one_line(&q.text, 200));
            open_question(sup, lv, q, Some(note));
        }
        other => {
            let input = matches!(other, Waits::Input { .. });
            if let Some(q) = &open {
                if menu_sig(q).is_some() && held >= GONE_AFTER {
                    let why = match other {
                        Waits::Input { interrupted: true } => format!("The prompt left {}'s screen: it was interrupted (Esc) in its terminal and waits for what to do instead.", a.name),
                        Waits::Input { .. } => format!("The prompt left {}'s screen: it was answered or cancelled in its terminal.", a.name),
                        Waits::Busy => format!("The prompt left {}'s screen: it was answered in its terminal, and {} works on.", a.name, a.name),
                        _ => format!("The prompt is no longer on {}'s screen.", a.name),
                    };
                    close_question(sup, lv, &q.id, &why);
                }
            }
            // ask_human keeps its agent in a tool call (busy): an agent at
            // its text prompt is not waiting on one.
            if input && held >= IDLE_AFTER && matches!(a.state.as_str(), "working" | "starting" | "waiting") && !asking_ask(sup, id) {
                let interrupted = matches!(other, Waits::Input { interrupted: true });
                st.set_waiting = false;
                let summary = if interrupted { "interrupted: waits at its prompt for what to do instead" } else { "waits at its prompt" };
                let _ = sup.note(id, "screen", summary.into(), None, |ag| Supervisor::set_agent_state(ag, "idle"));
                crate::levels::type_pending_steers(sup, id);
            } else if !input && st.set_waiting && held >= GONE_AFTER && a.state == "waiting" && open.is_none() {
                st.set_waiting = false;
                let _ = sup.update_agent(id, |ag| Supervisor::set_agent_state(ag, "working"));
            }
        }
    }
}

/// Whether the agent has an open `ask_human` question.
fn asking_ask(sup: &Supervisor, agent: &str) -> bool {
    open_of(sup, agent).iter().any(|q| q.kind == "ask")
}

/// Whether a folder is a worktree Workbench made (under its data dir).
fn workbench_worktree(sup: &Supervisor, cwd: &str) -> bool {
    let root = sup.cfg.data_dir.join("worktrees");
    let root = root.canonicalize().unwrap_or(root);
    std::path::Path::new(cwd).starts_with(&root)
}

/// Answers Claude Code's trust prompt for a worktree Workbench made, off
/// the watcher's thread; when it cannot, the prompt becomes a question.
fn trust_later(sup: &Arc<Supervisor>, lv: &Arc<Levels>, id: &str, m: Menu) {
    let (sup, lv, id) = (sup.clone(), lv.clone(), id.to_string());
    let _ = std::thread::Builder::new().name(format!("trust-{id}")).spawn(move || {
        let Some(yes) = m.options.iter().find(|o| o.label.to_lowercase().starts_with("yes") && o.label.to_lowercase().contains("trust")).map(|o| o.label.clone()) else { return };
        let sig = m.signature();
        let done = {
            let _t = Typing::start(&lv, &id);
            match _t {
                Some(_) => choose(&sup, &id, &sig, Pick::Label(&yes)),
                None => return,
            }
        };
        let cwd = sup.agent_row(&id).map(|a| a.cwd).unwrap_or_default();
        match done {
            Ok(_) => {
                let _ = sup.note(&id, "screen", format!("Workbench trusted {cwd} for it: a worktree Workbench made (Claude Code's trust prompt, answered “{yes}”)"), Some(json!({ "trusted": cwd }).to_string()), |_| {});
            }
            Err(e) => {
                let _ = sup.note(&id, "screen", format!("Workbench could not answer the trust prompt: {}", e.reason), None, |_| {});
            }
        }
    });
}

/// An agent being typed into by Workbench: the watcher leaves it alone
/// meanwhile, and a second answer waits its turn.
pub struct Typing<'a> {
    lv: &'a Levels,
    id: String,
}

impl<'a> Typing<'a> {
    /// `None` when something else types into it now.
    pub fn start(lv: &'a Levels, id: &str) -> Option<Typing<'a>> {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if lv.typing.lock().unwrap().insert(id.to_string()) {
                return Some(Typing { lv, id: id.to_string() });
            }
            if Instant::now() > until {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Typing<'_> {
    fn drop(&mut self) {
        self.lv.typing.lock().unwrap().remove(&self.id);
    }
}

/// What to choose in a menu.
pub enum Pick<'a> {
    /// The option with this label.
    Label(&'a str),
    /// Esc: cancel it (a permission prompt reads it as no).
    Esc,
}

/// Why an answer was not typed; `gone` when the prompt is not on the
/// screen (or is another one), so that its question closes.
#[derive(Debug)]
pub struct NotTyped {
    pub reason: String,
    pub gone: bool,
}

/// Moves the cursor of the menu with signature `sig` on `id`'s screen to
/// `pick` and presses Enter (or Esc), reading the screen before each key:
/// a key is typed only into that prompt, and Enter only with the cursor on
/// the option chosen. Answers what was chosen.
pub fn choose(sup: &Supervisor, id: &str, sig: &str, pick: Pick) -> Result<String, NotTyped> {
    let name = sup.agent_row(id).map(|a| a.name).unwrap_or_else(|| id.to_string());
    let gone = |reason: String| NotTyped { reason, gone: true };
    let failed = |reason: String| NotTyped { reason, gone: false };
    let read = || match sup.waits(id) {
        Waits::Menu(m) => Some(m),
        _ => None,
    };
    let Some(mut m) = read() else {
        return Err(gone(match sup.waits(id) {
            Waits::Input { interrupted: true } => format!("the prompt is no longer on {name}'s screen: it was interrupted (Esc) and waits at its text prompt, where a key would be a new message"),
            Waits::Input { .. } => format!("the prompt is no longer on {name}'s screen: it waits at its text prompt, where a key would be a new message"),
            _ => format!("the prompt is no longer on {name}'s screen"),
        }));
    };
    if m.signature() != sig {
        return Err(gone(format!("{name}'s screen shows another prompt now")));
    }
    let wait_gone = |what: String| -> Result<String, NotTyped> {
        let until = Instant::now() + Duration::from_secs(4);
        while Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
            if read().is_none_or(|n| n.signature() != sig) {
                return Ok(what);
            }
        }
        Err(failed(format!("Workbench chose “{what}”, but the prompt is still on {name}'s screen")))
    };
    let target = match pick {
        Pick::Esc => {
            sup.type_raw(id, b"\x1b").map_err(|e| failed(e.message))?;
            return wait_gone("Esc".into());
        }
        Pick::Label(l) => {
            let at: Vec<usize> = m.options.iter().enumerate().filter(|(_, o)| o.label == l).map(|(i, _)| i).collect();
            match at.as_slice() {
                [one] => *one,
                _ => return Err(gone(format!("{name}'s prompt has no option “{l}” now"))),
            }
        }
    };
    let mut keys = 0;
    while m.selected != target {
        keys += 1;
        if keys > m.options.len() * 2 + 4 {
            return Err(failed(format!("Workbench could not move the cursor of {name}'s prompt to “{}”", m.options[target].label)));
        }
        let before = m.selected;
        sup.arrow(id, before > target).map_err(|e| failed(e.message))?;
        let until = Instant::now() + Duration::from_millis(800);
        loop {
            std::thread::sleep(Duration::from_millis(30));
            match read() {
                Some(n) if n.signature() == sig => {
                    m = n;
                    if m.selected != before {
                        break;
                    }
                }
                _ => return Err(gone(format!("the prompt on {name}'s screen changed while Workbench moved its cursor"))),
            }
            if Instant::now() > until {
                // The key was lost (it happens with the first key a
                // program reads): the loop types it again.
                break;
            }
        }
    }
    // The cursor is on it, on this prompt: Enter.
    let label = m.options[target].label.clone();
    sup.type_raw(id, b"\r").map_err(|e| failed(e.message))?;
    wait_gone(label)
}

/// `ask_human` returned without an answer (it timed out): its question
/// expires.
pub fn expire_asks_of(sup: &Supervisor, lv: &Levels, agent: &str) {
    let _w = lv.write.lock().unwrap();
    for mut q in open_of(sup, agent).into_iter().filter(|q| q.kind == "ask") {
        q.state = "expired".into();
        q.answered = Some(now_ms());
        let _ = q.put(sup);
        lv.changed(&q.id);
    }
}

/// Asks ChloeCtx about the question off the request path, and marks the
/// options its answer or a rule of the repository speaks for.
fn find_precedent_later(sup: &Arc<Supervisor>, lv: &Arc<Levels>, qid: &str) {
    let (sup, lv, qid) = (sup.clone(), lv.clone(), qid.to_string());
    let _ = std::thread::Builder::new().name(format!("precedent-{qid}")).spawn(move || {
        let Ok(q) = get(&sup, &qid) else { return };
        let Some(repo) = q.repo.clone() else { return };
        let ask = if q.kind == "permission" { format!("{} {}", q.text, q.context.as_deref().unwrap_or("")) } else { q.text.clone() };
        let mut p = trail::ask(&sup, Some(&repo), &ask).unwrap_or_else(|e| json!({ "text": "", "ids": [], "agents": {}, "missing": [e.message] }));
        let rs = rules(&sup, Some(&repo), true);
        let _w = lv.write.lock().unwrap();
        let Ok(mut q) = get(&sup, &qid) else { return };
        let text = p["text"].as_str().unwrap_or("").to_string();
        let mut matched_rules = Vec::new();
        if q.kind == "ask" {
            if let Some(opts) = q.options.as_array_mut() {
                for o in opts.iter_mut() {
                    let label = o["label"].as_str().unwrap_or("").to_string();
                    let by_rule: Vec<&Rule> = rs.iter().filter(|r| label_matches(&label, &r.text)).collect();
                    let by_record = text.lines().any(|l| label_matches(&label, l));
                    o["matches_precedent"] = json!(!by_rule.is_empty() || by_record);
                    matched_rules.extend(by_rule.into_iter().map(|r| json!({ "id": r.id, "text": r.text, "option": label })));
                }
            }
        }
        p["rules"] = Json::Array(matched_rules);
        q.precedent = Some(p);
        if q.put(&sup).is_ok() {
            lv.changed(&q.id);
        }
    });
}

/// What `POST /api/questions/:id/answer` takes: `option` (from 0), `text`
/// (said with it, or instead), `rule` (a rule made from the answer); for a
/// permission question, `cancel` (Esc) instead of an option.
#[derive(Debug, Default, Deserialize)]
pub struct AnswerBody {
    pub option: Option<usize>,
    pub text: Option<String>,
    pub rule: Option<String>,
    pub cancel: Option<bool>,
    /// The label the card showed for `option`: when given, it must be the
    /// prompt's label at that index, or nothing is typed.
    pub label: Option<String>,
}

/// `POST /api/questions/:id/answer`.
pub fn answer(sup: &Arc<Supervisor>, lv: &Arc<Levels>, id: &str, b: AnswerBody) -> Result<Json, ApiError> {
    if get(sup, id)?.kind == "permission" {
        return answer_prompt(sup, lv, id, b);
    }
    let text = b.text.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    let q = {
        let _w = lv.write.lock().unwrap();
        let mut q = get(sup, id)?;
        if q.state != "open" {
            return Err(ApiError::conflict(format!("question {id} is {}{}", q.state, q.answered_by.as_deref().map(|b| format!(" (by {b})")).unwrap_or_default())));
        }
        let labels = q.labels();
        let label = match b.option {
            Some(n) => Some(labels.get(n).cloned().ok_or_else(|| ApiError::bad(format!("option is an index from 0 to {}", labels.len().saturating_sub(1))))?),
            None => None,
        };
        let answer = match (&label, &text) {
            (Some(l), Some(t)) => format!("{l}\n{t}"),
            (Some(l), None) => l.clone(),
            (None, Some(t)) => t.clone(),
            (None, None) => return Err(ApiError::bad("option or text is required")),
        };
        q.state = "answered".into();
        q.answer = Some(answer);
        q.answered_by = Some("you".into());
        q.answered = Some(now_ms());
        q.put(sup)?;
        q
    };
    let agent = q.agent.clone().unwrap_or_default();
    let _ = sup.note(&agent, "answered", format!("answered: {}", one_line(q.answer.as_deref().unwrap_or(""), 200)), Some(json!({ "question": q.id, "by": "you" }).to_string()), |a| {
        if a.state == "waiting" {
            Supervisor::set_agent_state(a, "working");
        }
    });
    finish_answer(sup, lv, q, b.rule.as_deref())
}

/// The rule made from an answer, if one was asked for, and the answer.
fn finish_answer(sup: &Supervisor, lv: &Levels, mut q: Question, rule: Option<&str>) -> Result<Json, ApiError> {
    let mut missing = Vec::new();
    let mut rule_json = Json::Null;
    if let Some(r) = rule.map(str::trim).filter(|r| !r.is_empty()) {
        let repo = q.repo.clone().unwrap_or_default();
        let (rule, m) = add_rule(sup, &repo, r, Some(&q.id));
        missing = m;
        rule_json = rule.to_json();
        let _w = lv.write.lock().unwrap();
        q.rule = Some(rule.id.clone());
        q.put(sup)?;
    }
    lv.changed(&q.id);
    Ok(json!({ "question": q.to_json(sup), "rule": rule_json, "missing": missing }))
}

/// A permission question (a prompt on its agent's screen) is answered by
/// choosing its option on that screen: the option with the label you
/// chose, the cursor moved to it and checked there before Enter, and only
/// while that same prompt is on the screen. When it is not, nothing is
/// typed and the question closes, saying why.
fn answer_prompt(sup: &Arc<Supervisor>, lv: &Arc<Levels>, id: &str, b: AnswerBody) -> Result<Json, ApiError> {
    let q = get(sup, id)?;
    if q.state != "open" {
        return Err(ApiError::conflict(format!("question {id} is {}{}", q.state, q.answer.as_deref().filter(|_| q.answered_by.as_deref() == Some("screen")).map(|w| format!(": {w}")).unwrap_or_default())));
    }
    let agent = q.agent.clone().unwrap_or_default();
    let name = sup.agent_row(&agent).map(|a| a.name).unwrap_or_else(|| agent.clone());
    let labels = q.labels();
    let Some(sig) = menu_sig(&q) else {
        return unread_prompt(sup, lv, &q, &name, b);
    };
    let pick = match (b.option, b.cancel) {
        (Some(n), _) => Some(labels.get(n).cloned().ok_or_else(|| ApiError::bad(format!("option is an index from 0 to {}, as the prompt lists them", labels.len().saturating_sub(1))))?),
        (None, Some(true)) => None,
        _ => return Err(ApiError::bad("a permission question is answered with option (the index of one of its options, as its prompt lists them) or cancel: true (Esc)")),
    };
    if let (Some(want), Some(have)) = (b.label.as_deref(), pick.as_deref()) {
        if want != have {
            return Err(ApiError::new(409, "not_typed", format!("Nothing was typed: option {} of this prompt is “{have}”, not “{want}”", b.option.unwrap_or(0) + 1)));
        }
    }
    let typed = {
        let Some(_t) = Typing::start(lv, &agent) else {
            return Err(ApiError::conflict(format!("Workbench is typing into {name} already; try again in a moment")));
        };
        choose(sup, &agent, &sig, match &pick {
            Some(l) => Pick::Label(l),
            None => Pick::Esc,
        })
    };
    if let Err(e) = typed {
        if e.gone {
            close_question(sup, lv, id, &format!("Not typed: {}.", e.reason));
            return Err(ApiError::new(409, "prompt_gone", format!("Nothing was typed: {}. The question is closed.", e.reason)));
        }
        return Err(ApiError::new(409, "not_typed", format!("{}. Answer it in {name}'s terminal.", e.reason)));
    }
    let answer = pick.clone().unwrap_or_else(|| "Cancelled (Esc)".into());
    let q = {
        let _w = lv.write.lock().unwrap();
        let mut q = get(sup, id)?;
        q.state = "answered".into();
        q.answer = Some(answer.clone());
        q.answered_by = Some("you".into());
        q.answered = Some(now_ms());
        q.put(sup)?;
        q
    };
    {
        let mut open = lv.open_perm.lock().unwrap();
        if open.get(&agent).map(String::as_str) == Some(id) {
            open.remove(&agent);
        }
    }
    let n = b.option.map(|i| i + 1);
    let _ = sup.note(&agent, "input", format!("input: chose “{}” in its prompt", one_line(&answer, 160)), Some(json!({ "question": id, "by": "you", "option": n }).to_string()), |a| {
        if a.state == "waiting" {
            Supervisor::set_agent_state(a, "working");
        }
    });
    finish_answer(sup, lv, q, b.rule.as_deref())
}

/// A permission question whose prompt Workbench has not read off the
/// screen (no menu it knows was drawn there). Only the two answers that
/// mean the same in every Claude Code prompt are typed: the first option,
/// its plain "Yes", by its number, and Esc, which cancels. And only while
/// the screen shows neither Claude Code's text prompt (where "1" would be a
/// message) nor a working spinner (the prompt has gone).
fn unread_prompt(sup: &Arc<Supervisor>, lv: &Arc<Levels>, q: &Question, name: &str, b: AnswerBody) -> Result<Json, ApiError> {
    let agent = q.agent.clone().unwrap_or_default();
    let esc = b.cancel == Some(true) && b.option.is_none();
    let answer = match b.option {
        Some(0) if b.label.as_deref().is_none_or(|l| l == "Yes") => "Yes",
        None if esc => "Cancelled (Esc)",
        _ => {
            return Err(ApiError::conflict(format!(
                "Workbench has not read this prompt's options from {name}'s screen, so it types only its first option (Yes) or Esc: answer it in {name}'s terminal"
            )))
        }
    };
    {
        let Some(_t) = Typing::start(lv, &agent) else {
            return Err(ApiError::conflict(format!("Workbench is typing into {name} already; try again in a moment")));
        };
        let not = match sup.waits(&agent) {
            Waits::Input { .. } => Some(format!("{name} is at its text prompt, where a key would be a new message")),
            Waits::Busy => Some(format!("{name} is working again: its prompt has gone")),
            Waits::Menu(_) => Some(format!("{name} shows a prompt Workbench has not matched to this question yet; try again in a moment")),
            Waits::Unknown => None,
        };
        if let Some(why) = not {
            return Err(ApiError::new(409, "not_typed", format!("Nothing was typed: {why}.")));
        }
        if esc {
            sup.type_raw(&agent, b"\x1b")?;
        } else {
            sup.type_raw(&agent, b"1")?;
            std::thread::sleep(Duration::from_millis(150));
            sup.type_raw(&agent, b"\r")?;
        }
    }
    let q = {
        let _w = lv.write.lock().unwrap();
        let mut q = get(sup, &q.id)?;
        q.state = "answered".into();
        q.answer = Some(answer.into());
        q.answered_by = Some("you".into());
        q.answered = Some(now_ms());
        q.put(sup)?;
        q
    };
    {
        let mut open = lv.open_perm.lock().unwrap();
        if open.get(&agent) == Some(&q.id) {
            open.remove(&agent);
        }
    }
    let _ = sup.note(&agent, "input", format!("input: chose “{answer}” in its prompt (typed as {})", if esc { "Esc" } else { "1" }), Some(json!({ "question": q.id, "by": "you", "option": b.option.map(|i| i + 1) }).to_string()), |a| {
        if a.state == "waiting" {
            Supervisor::set_agent_state(a, "working");
        }
    });
    finish_answer(sup, lv, q, b.rule.as_deref())
}

/// A rule of a repository, kept in ChloeCtx too (`chloectx remember
/// decision`) when it is installed; what could not be done is said.
pub fn add_rule(sup: &Supervisor, repo: &str, text: &str, question: Option<&str>) -> (Rule, Vec<String>) {
    let mut missing = Vec::new();
    let dir = PathBuf::from(repo);
    let ctx_id = match trail::run_raw(&sup.cfg.chloectx_bin, &dir, &["remember", "decision", text], Duration::from_secs(30)) {
        Ran::Out(Json::String(out)) => stored_id(&out),
        Ran::Out(_) => None,
        Ran::Missing => {
            missing.push(trail::not_installed(&sup.cfg.chloectx_bin) + "; the rule is kept in Workbench only.");
            None
        }
        Ran::Failed(e) => {
            missing.push(format!("{e}; the rule is kept in Workbench only."));
            None
        }
    };
    let r = Rule { id: new_id("r"), repo: repo.to_string(), text: text.to_string(), question: question.map(str::to_string), ctx_id, active: true, created: now_ms() };
    if let Err(e) = r.put(sup) {
        missing.push(e.message);
    }
    (r, missing)
}

/// The record id in what `chloectx remember` prints: "Stored decision as <id>."
fn stored_id(out: &str) -> Option<String> {
    let line = out.lines().find(|l| l.contains(" as "))?;
    let id = line.rsplit_once(" as ")?.1.trim().trim_end_matches('.');
    (!id.is_empty() && !id.contains(' ')).then(|| id.to_string())
}

/// What `PATCH /api/rules/:id` takes.
#[derive(Debug, Default, Deserialize)]
pub struct RulePatch {
    pub active: Option<bool>,
    pub text: Option<String>,
}

pub fn patch_rule(sup: &Supervisor, lv: &Levels, id: &str, b: RulePatch) -> Result<Json, ApiError> {
    let _w = lv.write.lock().unwrap();
    let p = Params::new().set("id", Value::Str(id.to_string()));
    let rows = sup.db().query(&format!("SELECT {R_COLS} FROM rules WHERE id = :id LIMIT 1"), &p).map_err(ApiError::internal)?;
    let mut r = rows.first().map(|r| Rule::of(r)).ok_or_else(|| ApiError::not_found(format!("no rule {id}")))?;
    if let Some(a) = b.active {
        r.active = a;
    }
    if let Some(t) = b.text.filter(|t| !t.trim().is_empty()) {
        r.text = t.trim().to_string();
    }
    r.put(sup)?;
    Ok(json!({ "rule": r.to_json() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_words_and_ids_are_read() {
        assert!(label_matches("Round half up", "Totals round half up, always."));
        assert!(!label_matches("Round half even", "Totals round half up"));
        assert!(!label_matches("Yes", "yes we do"), "too common a word to match on");
        assert_eq!(stored_id("Stored decision as 1c2e9a0b:58.\n"), Some("1c2e9a0b:58".into()));
        assert_eq!(stored_id("nothing"), None);
        assert_eq!(norm("Round half UP?"), "round half up");
        assert!(is_permission(&json!({ "notification_type": "permission_prompt", "message": "x" })));
        assert!(!is_permission(&json!({ "notification_type": "idle_prompt", "message": "permission" })));
        assert!(is_permission(&json!({ "message": "Claude needs your permission to use Bash" })));
    }
}
