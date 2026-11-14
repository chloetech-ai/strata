//! Goals and outcomes (Mission): a goal of a repository, the 3 to 8
//! outcomes a lead agent breaks it into, the agent working on each, and
//! the outcome's state following its agent's.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chloedb::query::Params;
use chloedb::Value;
use serde::Deserialize;
use serde_json::{json, Value as Json};

use crate::db::{get_int, get_ms, get_str, iso, now_ms, opt_str, ts, Agent};
use crate::levels::{new_id, Levels};
use crate::supervisor::{one_line, ApiError, SpawnRequest, Supervisor};
use crate::trail::{self, Ran};

pub const GOAL_STATES: [&str; 3] = ["active", "done", "dropped"];
pub const OUTCOME_STATES: [&str; 5] = ["next", "active", "needs_you", "done", "dropped"];

#[derive(Clone, Debug, Default)]
pub struct Goal {
    pub id: String,
    pub repo: String,
    pub text: String,
    pub state: String,
    pub lead: Option<String>,
    pub created: i64,
    pub done: Option<i64>,
}

pub(crate) const GOAL_COLS: &str = "id, repo, text, state, lead, created, done";

impl Goal {
    pub(crate) fn of(r: &[Value]) -> Goal {
        Goal {
            id: get_str(&r[0]).unwrap_or_default(),
            repo: get_str(&r[1]).unwrap_or_default(),
            text: get_str(&r[2]).unwrap_or_default(),
            state: get_str(&r[3]).unwrap_or_default(),
            lead: get_str(&r[4]),
            created: get_ms(&r[5]).unwrap_or(0),
            done: get_ms(&r[6]),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), ApiError> {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("repo", Value::Str(self.repo.clone()))
            .set("text", Value::Str(self.text.clone()))
            .set("state", Value::Str(self.state.clone()))
            .set("lead", opt_str(&self.lead))
            .set("created", ts(self.created))
            .set("done", self.done.map_or(Value::Null, ts));
        sup.db().query(&format!("INSERT OR REPLACE INTO goals ({GOAL_COLS}) VALUES (:id, :repo, :text, :state, :lead, :created, :done)"), &p).map_err(ApiError::internal)?;
        Ok(())
    }

    /// The row as JSON, with its outcomes counted.
    pub fn to_json(&self, outcomes: &[Outcome]) -> Json {
        let live: Vec<&Outcome> = outcomes.iter().filter(|o| o.state != "dropped").collect();
        let done = live.iter().filter(|o| o.state == "done").count();
        json!({
            "id": self.id, "repo": self.repo, "text": self.text, "state": self.state, "lead": self.lead,
            "created": iso(self.created), "done": self.done.map(iso),
            "outcomes_total": live.len(), "outcomes_done": done,
            "needs_you": live.iter().filter(|o| o.state == "needs_you").count(),
            "progress": if live.is_empty() { 0 } else { live.iter().map(|o| o.progress.clamp(0, 100)).sum::<i64>() / live.len() as i64 },
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub id: String,
    pub goal: String,
    pub title: String,
    pub detail: Option<String>,
    pub state: String,
    pub agent: Option<String>,
    pub ord: i64,
    pub after: Option<String>,
    pub progress: i64,
    pub created: i64,
    pub updated: i64,
}

pub(crate) const OUTCOME_COLS: &str = "id, goal, title, detail, state, agent, ord, after, progress, created, updated";

impl Outcome {
    pub(crate) fn of(r: &[Value]) -> Outcome {
        Outcome {
            id: get_str(&r[0]).unwrap_or_default(),
            goal: get_str(&r[1]).unwrap_or_default(),
            title: get_str(&r[2]).unwrap_or_default(),
            detail: get_str(&r[3]),
            state: get_str(&r[4]).unwrap_or_default(),
            agent: get_str(&r[5]),
            ord: get_int(&r[6]).unwrap_or(0),
            after: get_str(&r[7]),
            progress: get_int(&r[8]).unwrap_or(0),
            created: get_ms(&r[9]).unwrap_or(0),
            updated: get_ms(&r[10]).unwrap_or(0),
        }
    }

    fn put(&self, sup: &Supervisor) -> Result<(), ApiError> {
        let p = Params::new()
            .set("id", Value::Str(self.id.clone()))
            .set("goal", Value::Str(self.goal.clone()))
            .set("title", Value::Str(self.title.clone()))
            .set("detail", opt_str(&self.detail))
            .set("state", Value::Str(self.state.clone()))
            .set("agent", opt_str(&self.agent))
            .set("ord", Value::Int(self.ord))
            .set("after", opt_str(&self.after))
            .set("progress", Value::Int(self.progress))
            .set("created", ts(self.created))
            .set("updated", ts(self.updated));
        sup.db()
            .query(&format!("INSERT OR REPLACE INTO outcomes ({OUTCOME_COLS}) VALUES (:id, :goal, :title, :detail, :state, :agent, :ord, :after, :progress, :created, :updated)"), &p)
            .map_err(ApiError::internal)?;
        Ok(())
    }

    /// The row as JSON, with its agent's name and state.
    pub fn to_json(&self, sup: &Supervisor) -> Json {
        let a = self.agent.as_deref().and_then(|id| sup.agent_row(id));
        json!({
            "id": self.id, "goal": self.goal, "title": self.title, "detail": self.detail, "state": self.state,
            "agent": self.agent, "agent_name": a.as_ref().map(|a| a.name.clone()), "agent_state": a.as_ref().map(|a| a.state.clone()),
            "ord": self.ord, "after": self.after, "progress": self.progress,
            "created": iso(self.created), "updated": iso(self.updated), "updated_ms": self.updated,
        })
    }
}

pub fn goal(sup: &Supervisor, id: &str) -> Result<Goal, ApiError> {
    let p = Params::new().set("id", Value::Str(id.to_string()));
    let rows = sup.db().query(&format!("SELECT {GOAL_COLS} FROM goals WHERE id = :id LIMIT 1"), &p).map_err(ApiError::internal)?;
    rows.first().map(|r| Goal::of(r)).ok_or_else(|| ApiError::not_found(format!("no goal {id}")))
}

pub fn goals(sup: &Supervisor, repo: Option<&str>) -> Vec<Goal> {
    let rows = match repo {
        Some(r) => sup.db().query(&format!("SELECT {GOAL_COLS} FROM goals WHERE repo = :repo ORDER BY created DESC LIMIT 10000"), &Params::new().set("repo", Value::Str(r.to_string()))),
        None => sup.db().query(&format!("SELECT {GOAL_COLS} FROM goals ORDER BY created DESC LIMIT 10000"), &Params::new()),
    };
    rows.unwrap_or_default().iter().map(|r| Goal::of(r)).collect()
}

pub fn outcome(sup: &Supervisor, id: &str) -> Result<Outcome, ApiError> {
    let p = Params::new().set("id", Value::Str(id.to_string()));
    let rows = sup.db().query(&format!("SELECT {OUTCOME_COLS} FROM outcomes WHERE id = :id LIMIT 1"), &p).map_err(ApiError::internal)?;
    rows.first().map(|r| Outcome::of(r)).ok_or_else(|| ApiError::not_found(format!("no outcome {id}")))
}

/// The outcomes of a goal (or of all), in their order.
pub fn outcomes(sup: &Supervisor, goal: Option<&str>) -> Vec<Outcome> {
    let rows = match goal {
        Some(g) => sup.db().query(&format!("SELECT {OUTCOME_COLS} FROM outcomes WHERE goal = :goal LIMIT 10000"), &Params::new().set("goal", Value::Str(g.to_string()))),
        None => sup.db().query(&format!("SELECT {OUTCOME_COLS} FROM outcomes LIMIT 100000"), &Params::new()),
    };
    let mut v: Vec<Outcome> = rows.unwrap_or_default().iter().map(|r| Outcome::of(r)).collect();
    v.sort_by_key(|o| (o.goal.clone(), o.ord, o.created));
    v
}

/// The state an outcome's agent implies; `done` and `dropped` once set
/// stay. `asking`: the agents with an open question.
fn derived(o: &Outcome, a: Option<&Agent>, running: bool, asking: &HashSet<String>) -> String {
    if matches!(o.state.as_str(), "done" | "dropped") {
        return o.state.clone();
    }
    let Some(a) = a else {
        return if o.agent.is_none() { "next".into() } else { o.state.clone() };
    };
    if asking.contains(&a.id) || a.state == "waiting" {
        return "needs_you".into();
    }
    if a.state == "exited" || !running {
        // Ended: done when it said so (progress 100) and exited cleanly;
        // else someone has to look.
        return if a.exit_code == Some(0) && o.progress >= 100 { "done".into() } else { "needs_you".into() };
    }
    "active".into()
}

/// The outcomes of a goal (or of all) with their state brought up to
/// their agents', written back where it changed.
pub fn refresh(sup: &Supervisor, lv: &Levels, goal: Option<&str>) -> Vec<Outcome> {
    let _w = lv.write.lock().unwrap();
    let asking = crate::questions::asking(sup);
    let agents: HashMap<String, Agent> = sup.agent_rows().into_iter().map(|a| (a.id.clone(), a)).collect();
    let mut list = outcomes(sup, goal);
    for o in list.iter_mut() {
        let a = o.agent.as_deref().and_then(|id| agents.get(id));
        let running = a.is_some_and(|a| sup.is_running(&a.id));
        let s = derived(o, a, running, &asking);
        if s != o.state {
            o.state = s;
            if o.state == "done" {
                o.progress = 100;
            }
            o.updated = now_ms();
            let _ = o.put(sup);
        }
    }
    list
}

/// Claude Code's effort levels (`--effort`).
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// What `POST /api/goals` takes. Three ways to staff a goal (sixth build):
/// `plan`, a lead that finds the outcomes and starts their agents;
/// `outcomes`, the person's own, one agent each (`start`, default true) and
/// no planner; `agents`, agents already running, each attached as an
/// outcome. `model` and `effort` are the lead's, or the outcomes' agents'.
#[derive(Debug, Default, Deserialize)]
pub struct GoalBody {
    pub repo: Option<String>,
    pub text: Option<String>,
    pub plan: Option<PlanMode>,
    pub model: Option<String>,
    pub worktree: Option<bool>,
    pub effort: Option<String>,
    pub outcomes: Option<Vec<OutcomeSpec>>,
    pub start: Option<bool>,
    pub agents: Option<Vec<String>>,
}

/// `plan`: `true` starts a lead that plans; `"draft"` only drafts the
/// parts and starts nothing (JOURNEYS.md, "A. Start a new project", Plan it).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PlanMode {
    On(bool),
    Mode(String),
}

/// An outcome the person names: its title, or the title with how to tell
/// it is done and what its agent is to do.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum OutcomeSpec {
    Title(String),
    Full { title: String, detail: Option<String>, task: Option<String>, after: Option<usize>, model: Option<String> },
}

impl OutcomeSpec {
    fn parts(&self) -> (String, Option<String>, Option<String>) {
        let clean = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        match self {
            // "Title — how to tell it is done", as the form's lines are.
            OutcomeSpec::Title(t) => match [" — ", " – ", " -- "].iter().find_map(|sep| t.split_once(sep)) {
                Some((title, done)) => (title.trim().to_string(), Some(done.trim().to_string()).filter(|d| !d.is_empty()), None),
                None => (t.trim().to_string(), None, None),
            },
            OutcomeSpec::Full { title, detail, task, .. } => (title.trim().to_string(), clean(detail), clean(task)),
        }
    }
}

/// The first prompt of an outcome's agent when the person named the
/// outcomes: the outcome first, then the goal it is part of.
pub fn outcome_task(goal: &Goal, title: &str, detail: Option<&str>, task: Option<&str>) -> String {
    let mut s = format!("{}.", title.trim_end_matches('.'));
    if let Some(t) = task {
        s.push_str("\n\n");
        s.push_str(t);
    }
    if let Some(d) = detail {
        s.push_str("\n\nDone when: ");
        s.push_str(d);
    }
    s.push_str(&format!(
        "\n\nThis is one outcome of a goal the user set in Workbench: {}. Other agents work on the other outcomes in their own worktrees. Work in this worktree; when it is done, say what you changed and how you checked it.",
        goal.text.trim().trim_end_matches('.')
    ));
    s
}

/// A name for an outcome's agent from its title: its first words, free
/// (no running agent and no `wb/` branch has it) and not in `taken`.
fn outcome_name(sup: &Supervisor, repo: &std::path::Path, title: &str, taken: &HashSet<String>) -> String {
    let mut base = String::new();
    for w in title.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).take(3) {
        let w = w.to_ascii_lowercase();
        if base.len() + w.len() + 1 > 20 {
            break;
        }
        if !base.is_empty() {
            base.push('-');
        }
        base.push_str(&w);
    }
    if base.is_empty() {
        base = "outcome".into();
    }
    let running: HashSet<String> = sup.agent_rows().into_iter().filter(|a| sup.is_running(&a.id)).map(|a| a.name).collect();
    let mut n = base.clone();
    let mut i = 2;
    while taken.contains(&n) || running.contains(&n) || crate::git::branch_exists(repo, &format!("wb/{n}")) {
        n = format!("{base}-{i}");
        i += 1;
    }
    n
}

/// The lead's first prompt.
pub fn lead_task(goal: &Goal) -> String {
    format!(
        "You lead the work on a goal the user set in Workbench, where the user watches every agent:\n\n\
GOAL ({id}): {text}\n\n\
You plan and coordinate; the agents you start do the building. Do this:\n\
1. Read enough of this repository to understand what the goal takes. Do not change code yet.\n\
2. Break the goal into 3 to 8 outcomes, each one a result the user can check, small enough for one agent, and as independent of the others as it can be. Add each with the workbench tool add_outcome: title (what will be true when it is done, under 10 words), detail (how to tell it is done), and after (the id of an outcome it must wait for, if any).\n\
3. For each outcome that waits for nothing, start an agent with spawn_agent, passing outcome (its id) and a task that stands alone: the agent does not see this conversation, so say what to do, where, how to tell it is done, and to report what it changed. Each agent works in its own git worktree and branch. Start the waiting outcomes when what they wait for is done.\n\
4. Follow the agents with wait_agent and read_agent. Answer their questions yourself with send_to_agent when you can. When only the user can decide, ask with ask_human (the question, 2 to 4 options, your recommendation): never ask in your terminal, the user is not watching it.\n\
5. Keep the outcomes true: update_outcome with progress (0 to 100) as they go, state \"done\" when an agent's work checks out, \"dropped\" when one no longer makes sense.\n\
6. When every outcome is done, say in a few sentences what was built and on which branches, and stop.",
        id = goal.id,
        text = goal.text.trim()
    )
}

/// A free name for a lead: `lead`, `lead-2`, ...
fn lead_name(sup: &Supervisor, repo: &std::path::Path) -> String {
    let running: HashSet<String> = sup.agent_rows().into_iter().filter(|a| sup.is_running(&a.id)).map(|a| a.name).collect();
    let mut i = 1;
    loop {
        let n = if i == 1 { "lead".to_string() } else { format!("lead-{i}") };
        if !running.contains(&n) && !crate::git::branch_exists(repo, &format!("wb/{n}")) {
            return n;
        }
        i += 1;
    }
}

/// `POST /api/goals`: a goal, and with `plan` its lead.
pub fn create(sup: &Arc<Supervisor>, lv: &Levels, b: GoalBody) -> Result<Json, ApiError> {
    let repo = trail::repo_dir(b.repo.as_deref())?;
    crate::git::checkout(&repo).map_err(ApiError::bad)?;
    let text = b.text.unwrap_or_default().trim().to_string();
    if text.is_empty() {
        return Err(ApiError::bad("text is empty: say what the goal is"));
    }
    let effort = b.effort.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(str::to_string);
    if let Some(e) = effort.as_deref().filter(|e| !EFFORTS.contains(e)) {
        return Err(ApiError::bad(format!("effort {e:?} is one of {}", EFFORTS.join(", "))));
    }
    let model = b.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string);
    let given = b.outcomes.unwrap_or_default();
    // A part that waits for another (the draft plan's `after`): by index
    // among the outcomes that have a title.
    let afters: Vec<Option<usize>> = given.iter().filter(|o| !o.parts().0.is_empty()).map(|o| match o { OutcomeSpec::Full { after, .. } => *after, _ => None }).collect();
    // A part's own model (the plan's chip), else the goal's (Dev: the
    // model picked on the form was not the one that ran).
    let models: Vec<Option<String>> = given
        .iter()
        .filter(|o| !o.parts().0.is_empty())
        .map(|o| match o {
            OutcomeSpec::Full { model: Some(m), .. } if !m.trim().is_empty() && m.trim() != "default" => Some(m.trim().to_string()),
            _ => None,
        })
        .collect();
    let specs: Vec<(String, Option<String>, Option<String>)> = given.iter().map(OutcomeSpec::parts).filter(|o| !o.0.is_empty()).collect();
    let attach: Vec<String> = b.agents.unwrap_or_default().into_iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
    let plan = match &b.plan {
        None | Some(PlanMode::On(false)) => false,
        Some(PlanMode::On(true)) => true,
        Some(PlanMode::Mode(m)) if m == "draft" => return Ok(draft(sup, &repo, &text, model.as_deref())),
        Some(PlanMode::Mode(m)) => return Err(ApiError::bad(format!("plan is true, false or \"draft\", not {m:?}"))),
    };
    if plan && (!specs.is_empty() || !attach.is_empty()) {
        return Err(ApiError::bad("plan starts a lead that finds the outcomes itself: with outcomes or agents given, leave plan off"));
    }
    if specs.len() > 12 {
        return Err(ApiError::bad("at most 12 outcomes at once"));
    }
    if let Some(t) = specs.iter().find(|o| o.0.len() > 300) {
        return Err(ApiError::bad(format!("an outcome's title is at most 300 bytes: {:?}", one_line(&t.0, 60))));
    }
    // The agents to attach: each one of this repository.
    let mut attached: Vec<Agent> = Vec::new();
    for id in &attach {
        let a = sup.agent_row(id).ok_or_else(|| ApiError::bad(format!("no agent {id}")))?;
        if PathBuf::from(&a.repo) != repo {
            return Err(ApiError::bad(format!("agent {} works in {}, not {}", a.name, a.repo, repo.display())));
        }
        attached.push(a);
    }
    let mut g = Goal { id: new_id("g"), repo: repo.to_string_lossy().to_string(), text, state: "active".into(), lead: None, created: now_ms(), done: None };
    g.put(sup)?;
    let mut lead = Json::Null;
    let mut started: Vec<Json> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    if plan {
        let req = SpawnRequest {
            repo: g.repo.clone(),
            task: Some(lead_task(&g)),
            name: Some(lead_name(sup, &repo)),
            worktree: b.worktree,
            model: model.clone(),
            effort: effort.clone(),
            goal: Some(g.id.clone()),
            ..SpawnRequest::default()
        };
        lead = spawn(sup, lv, req)?;
        g.lead = lead["id"].as_str().map(str::to_string);
        g.put(sup)?;
    }
    // The person's own outcomes, one agent each and no planner.
    let mut taken: HashSet<String> = HashSet::new();
    let mut oids: Vec<String> = Vec::new();
    for (k, (title, detail, task)) in specs.iter().enumerate() {
        let after = afters.get(k).copied().flatten().filter(|&i| i < k).and_then(|i| oids.get(i).cloned());
        let waits = after.is_some();
        let added = add_outcome(sup, lv, OutcomeBody { goal: Some(g.id.clone()), title: Some(title.clone()), detail: detail.clone(), after }, None)?;
        let oid = added["outcome"]["id"].as_str().unwrap_or("").to_string();
        oids.push(oid.clone());
        if !b.start.unwrap_or(true) {
            continue;
        }
        if waits {
            missing.push(format!("{title}: waits for the part before it; start it when that one is done"));
            continue;
        }
        let name = outcome_name(sup, &repo, title, &taken);
        taken.insert(name.clone());
        let req = SpawnRequest {
            repo: g.repo.clone(),
            task: Some(outcome_task(&g, title, detail.as_deref(), task.as_deref())),
            name: Some(name),
            worktree: b.worktree,
            model: models.get(k).cloned().flatten().or_else(|| model.clone()),
            effort: effort.clone(),
            outcome: Some(oid),
            ..SpawnRequest::default()
        };
        // One that cannot start (the limit of running agents) stays next.
        match spawn(sup, lv, req) {
            Ok(a) => started.push(a),
            Err(e) => missing.push(format!("{title}: not started ({}); start it from Mission when there is room", e.message)),
        }
    }
    // Agents already running, each an outcome of the goal; each is told
    // its outcome at its next hook.
    for a in &attached {
        let title = a.task.as_deref().map(|t| one_line(t, 120)).filter(|t| !t.is_empty()).unwrap_or_else(|| format!("{}'s work", a.name));
        let added = add_outcome(sup, lv, OutcomeBody { goal: Some(g.id.clone()), title: Some(title), detail: None, after: None }, None)?;
        let oid = added["outcome"]["id"].as_str().unwrap_or("").to_string();
        patch_outcome(sup, lv, &oid, OutcomePatch { agent: Some(a.id.clone()), state: Some("active".into()), ..OutcomePatch::default() })?;
        let gid = g.id.clone();
        started.push(sup.update_agent(&a.id, |x| {
            x.goal = Some(gid);
            x.outcome = Some(oid);
        })?);
    }
    let os = refresh(sup, lv, Some(&g.id));
    Ok(json!({ "goal": g.to_json(&os), "lead": lead, "outcomes": os.iter().map(|o| o.to_json(sup)).collect::<Vec<_>>(), "agents": started, "missing": missing }))
}

/// `GET /api/goals`.
pub fn list(sup: &Supervisor, lv: &Levels, repo: Option<&str>) -> Result<Json, ApiError> {
    let repo = match repo.filter(|r| !r.is_empty()) {
        Some(r) => Some(trail::repo_dir(Some(r))?.to_string_lossy().to_string()),
        None => None,
    };
    let all = refresh(sup, lv, None);
    let list: Vec<Json> = goals(sup, repo.as_deref())
        .iter()
        .map(|g| {
            let os: Vec<Outcome> = all.iter().filter(|o| o.goal == g.id).cloned().collect();
            g.to_json(&os)
        })
        .collect();
    Ok(json!({ "goals": list }))
}

/// `GET /api/goals/:id`: the goal, its outcomes and agents, its open
/// questions, what ChloeCtx learned today, and its pulse.
pub fn detail(sup: &Supervisor, lv: &Levels, id: &str) -> Result<Json, ApiError> {
    let g = goal(sup, id)?;
    let os = refresh(sup, lv, Some(id));
    let mut agents: Vec<Agent> = sup.agent_rows().into_iter().filter(|a| a.goal.as_deref() == Some(id) || os.iter().any(|o| o.agent.as_deref() == Some(&a.id))).collect();
    agents.sort_by_key(|a| a.created);
    let ids: HashSet<String> = agents.iter().map(|a| a.id.clone()).collect();
    let open = crate::questions::all(sup).into_iter().filter(|q| q.state == "open" && q.agent.as_ref().is_some_and(|a| ids.contains(a))).count();
    let (learned, missing) = learned(sup, &g.repo, now_ms() / 1000 - 24 * 3600, 50);
    Ok(json!({
        "goal": g.to_json(&os),
        "outcomes": os.iter().map(|o| o.to_json(sup)).collect::<Vec<_>>(),
        "agents": agents.iter().filter_map(|a| sup.agent_json_of(&a.id)).collect::<Vec<_>>(),
        "open_questions": open,
        "learned": learned,
        // The map's collisions that involve this goal's agents.
        "heads_up": crate::map::collisions(sup, &g.repo).into_iter()
            .filter(|c| c["agents"].as_array().is_some_and(|xs| xs.iter().any(|x| x.as_str().is_some_and(|x| ids.contains(x)))))
            .collect::<Vec<_>>(),
        "pulse": pulse(sup, &ids, now_ms()),
        "missing": missing,
    }))
}

/// Events per minute of `agents` for the 60 minutes before `now`:
/// `[[minute_ms, count]]`, oldest first.
pub fn pulse(sup: &Supervisor, agents: &HashSet<String>, now: i64) -> Vec<[i64; 2]> {
    let end = now - now.rem_euclid(60_000) + 60_000;
    let start = end - 60 * 60_000;
    let mut counts = vec![0i64; 60];
    let p = Params::new().set("since", ts(start));
    for r in sup.db().query("SELECT agent, ts FROM events WHERE ts >= :since LIMIT 1000000", &p).unwrap_or_default() {
        if get_str(&r[0]).is_some_and(|a| agents.contains(&a)) {
            if let Some(t) = get_ms(&r[1]) {
                let i = (t - start) / 60_000;
                if (0..60).contains(&i) {
                    counts[i as usize] += 1;
                }
            }
        }
    }
    counts.iter().enumerate().map(|(i, c)| [start + i as i64 * 60_000, *c]).collect()
}

/// ChloeCtx's decision, preference, fact and error-fix records of a
/// repository since `since_s` (seconds), newest first, with their agents;
/// and what could not be found.
pub fn learned(sup: &Supervisor, repo: &str, since_s: i64, limit: usize) -> (Vec<Json>, Vec<String>) {
    let sql = format!(
        "SELECT id, kind, tag, ts, session, text, files FROM memory WHERE ts >= {since_s} AND (kind = 'decision' OR kind = 'preference' OR kind = 'fact' OR (kind = 'error' AND tag = 'fix')) ORDER BY ts DESC LIMIT {limit}"
    );
    let dir = PathBuf::from(repo);
    let out = match trail::run_chloectx(&sup.cfg.chloectx_bin, &dir, &["sql", "--json", &sql], Duration::from_secs(30)) {
        Ran::Out(v) => v,
        Ran::Missing => return (Vec::new(), vec![trail::not_installed(&sup.cfg.chloectx_bin)]),
        Ran::Failed(e) => return (Vec::new(), vec![e]),
    };
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let col = |row: &Vec<Json>, name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
    let s = trail::Sessions::load(sup);
    let mut list = Vec::new();
    for row in out["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
        let kind = col(row, "kind");
        let tag = col(row, "tag");
        let kind = if kind == "error" && tag == "fix" { json!("fix") } else { kind };
        let session = col(row, "session");
        list.push(json!({
            "id": col(row, "id"), "kind": kind, "text": col(row, "text"),
            "ts": trail::ctx_time(&col(row, "ts")), "ts_ms": trail::ctx_ms(&col(row, "ts")),
            "session": session,
            "agent": session.as_str().and_then(|sid| s.agent(sid)).map(|a| trail::agent_json(sup, a)),
            "files": trail::files_of(&col(row, "files")),
        }));
    }
    (list, Vec::new())
}

/// What `PATCH /api/goals/:id` takes.
#[derive(Debug, Default, Deserialize)]
pub struct GoalPatch {
    pub text: Option<String>,
    pub state: Option<String>,
}

pub fn patch(sup: &Supervisor, lv: &Levels, id: &str, b: GoalPatch) -> Result<Json, ApiError> {
    let _w = lv.write.lock().unwrap();
    let mut g = goal(sup, id)?;
    if let Some(t) = b.text.filter(|t| !t.trim().is_empty()) {
        g.text = t.trim().to_string();
    }
    if let Some(s) = b.state {
        if !GOAL_STATES.contains(&s.as_str()) {
            return Err(ApiError::bad(format!("state is one of {}", GOAL_STATES.join(", "))));
        }
        g.done = if s == "active" { None } else { g.done.or(Some(now_ms())) };
        g.state = s;
    }
    g.put(sup)?;
    let os = outcomes(sup, Some(id));
    Ok(json!({ "goal": g.to_json(&os) }))
}

/// What `POST /api/outcomes` takes.
#[derive(Debug, Default, Deserialize)]
pub struct OutcomeBody {
    pub goal: Option<String>,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub after: Option<String>,
}

/// `POST /api/outcomes`; `by` (the calling agent) stands for its goal when
/// none is given.
pub fn add_outcome(sup: &Supervisor, lv: &Levels, b: OutcomeBody, by: Option<&str>) -> Result<Json, ApiError> {
    let goal_id = match b.goal.filter(|g| !g.is_empty()) {
        Some(g) => g,
        None => by
            .and_then(|a| sup.agent_row(a))
            .and_then(|a| a.goal)
            .ok_or_else(|| ApiError::bad("goal is required (an agent's own goal stands for it when it has one)"))?,
    };
    let g = goal(sup, &goal_id)?;
    let title = b.title.unwrap_or_default().trim().to_string();
    if title.is_empty() || title.len() > 300 {
        return Err(ApiError::bad("title is required, at most 300 bytes"));
    }
    let _w = lv.write.lock().unwrap();
    let existing = outcomes(sup, Some(&g.id));
    if let Some(after) = b.after.as_deref().filter(|a| !a.is_empty()) {
        if !existing.iter().any(|o| o.id == after) {
            return Err(ApiError::bad(format!("after: goal {} has no outcome {after}", g.id)));
        }
    }
    let now = now_ms();
    let o = Outcome {
        id: new_id("o"),
        goal: g.id.clone(),
        title,
        detail: b.detail.filter(|d| !d.trim().is_empty()),
        state: "next".into(),
        agent: None,
        ord: existing.iter().map(|o| o.ord).max().unwrap_or(0) + 1,
        after: b.after.filter(|a| !a.is_empty()),
        progress: 0,
        created: now,
        updated: now,
    };
    o.put(sup)?;
    Ok(json!({ "outcome": o.to_json(sup) }))
}

/// What `PATCH /api/outcomes/:id` takes.
#[derive(Debug, Default, Deserialize)]
pub struct OutcomePatch {
    pub title: Option<String>,
    pub detail: Option<String>,
    pub state: Option<String>,
    pub progress: Option<i64>,
    pub agent: Option<String>,
}

pub fn patch_outcome(sup: &Supervisor, lv: &Levels, id: &str, b: OutcomePatch) -> Result<Json, ApiError> {
    {
        let _w = lv.write.lock().unwrap();
        let mut o = outcome(sup, id)?;
        if let Some(t) = b.title.filter(|t| !t.trim().is_empty()) {
            o.title = t.trim().to_string();
        }
        if let Some(d) = b.detail {
            o.detail = Some(d).filter(|d| !d.trim().is_empty());
        }
        if let Some(p) = b.progress {
            o.progress = p.clamp(0, 100);
        }
        if let Some(a) = b.agent.filter(|a| !a.is_empty()) {
            if sup.agent_row(&a).is_none() {
                return Err(ApiError::bad(format!("no agent {a}")));
            }
            o.agent = Some(a);
        }
        if let Some(s) = b.state {
            if !OUTCOME_STATES.contains(&s.as_str()) {
                return Err(ApiError::bad(format!("state is one of {}", OUTCOME_STATES.join(", "))));
            }
            if s == "done" {
                o.progress = 100;
            }
            o.state = s;
        }
        o.updated = now_ms();
        o.put(sup)?;
    }
    let goal_id = outcome(sup, id)?.goal;
    let o = refresh(sup, lv, Some(&goal_id)).into_iter().find(|o| o.id == id).ok_or_else(|| ApiError::not_found(format!("no outcome {id}")))?;
    Ok(json!({ "outcome": o.to_json(sup) }))
}

/// Starts an agent (`POST /api/agents`) with its goal and outcome: an
/// outcome's agent works toward its goal and the outcome becomes active;
/// an agent started by an agent with a goal works toward that goal too.
pub fn spawn(sup: &Arc<Supervisor>, lv: &Levels, mut req: SpawnRequest) -> Result<Json, ApiError> {
    req.outcome = req.outcome.filter(|o| !o.is_empty());
    req.goal = req.goal.filter(|g| !g.is_empty());
    let o = match req.outcome.as_deref() {
        Some(oid) => {
            let o = outcome(sup, oid).map_err(|_| ApiError::bad(format!("no outcome {oid}")))?;
            if let Some(a) = o.agent.as_deref().filter(|a| sup.is_running(a)) {
                return Err(ApiError::conflict(format!("outcome {oid} has a running agent, {a}")));
            }
            req.goal = Some(o.goal.clone());
            Some(o)
        }
        None => None,
    };
    if req.goal.is_none() {
        req.goal = req.parent.as_deref().and_then(|p| sup.agent_row(p)).and_then(|p| p.goal);
    }
    if let Some(g) = req.goal.as_deref() {
        goal(sup, g).map_err(|_| ApiError::bad(format!("no goal {g}")))?;
    }
    let agent = sup.spawn(req)?;
    if let Some(mut o) = o {
        let _w = lv.write.lock().unwrap();
        o.agent = agent["id"].as_str().map(str::to_string);
        if matches!(o.state.as_str(), "next" | "needs_you") {
            o.state = "active".into();
        }
        o.updated = now_ms();
        o.put(sup)?;
    }
    Ok(agent)
}


// The draft plan (`plan: "draft"`).

const DRAFT_SYSTEM: &str = "You split what a developer wants built into the parts that coding agents will build, one agent per part. Answer with JSON only, no prose: {\"parts\":[{\"title\":\"...\",\"done\":\"how to tell it is done: a command that passes, in backticks, or what you will see\",\"after\":[indexes of parts it waits for, from 0],\"model\":\"haiku|sonnet|opus\"}]}. Two to six parts, the smallest first; a part waits for another only when it needs that one's code. Use the developer's words and names; invent no service they did not name.";

/// Runs `claude -p` with a timeout; its stdout.
fn run_print(bin: &str, args: &[String], cwd: &std::path::Path, timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    let mut child = std::process::Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {bin}: {e}"))?;
    let mut out = child.stdout.take().ok_or("no output")?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the model took over {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
    reader.join().map_err(|_| "the output was lost".to_string())
}

/// The parts a model drafted, from its answer: `None` when it is not the JSON asked for.
fn parts_of_answer(raw: &str) -> Option<Vec<Json>> {
    let res = raw.lines().rev().find_map(|l| serde_json::from_str::<Json>(l.trim()).ok().filter(|v| v.get("result").is_some())).or_else(|| serde_json::from_str::<Json>(raw.trim()).ok())?;
    let text = res["result"].as_str()?.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim().to_string();
    let v: Json = serde_json::from_str(&text).ok()?;
    let parts = v["parts"].as_array()?;
    let n = parts.len();
    let out: Vec<Json> = parts
        .iter()
        .filter_map(|p| {
            let title = p["title"].as_str()?.trim();
            if title.is_empty() {
                return None;
            }
            let after: Vec<usize> = p["after"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()).map(|x| x as usize).filter(|&x| x < n).collect();
            let model = p["model"].as_str().filter(|m| ["haiku", "sonnet", "opus"].contains(m));
            let done = p["done"].as_str().map(|d| one_line(d.trim(), 300)).filter(|d| !d.is_empty());
            Some(json!({ "title": one_line(title, 300), "done": done, "after": after, "model": model }))
        })
        .take(8)
        .collect();
    (!out.is_empty()).then_some(out)
}

/// The parts, from the person's own words: one per line or sentence.
fn parts_of_words(text: &str) -> Vec<Json> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['-', '*', '•']).trim();
        for s in line.split_inclusive(['.', ';']).map(|s| s.trim().trim_end_matches(['.', ';']).trim()) {
            if s.split_whitespace().count() >= 2 && out.len() < 6 {
                out.push(json!({ "title": one_line(s, 300), "done": null, "after": [], "model": null }));
            }
        }
    }
    if out.is_empty() {
        out.push(json!({ "title": one_line(text.trim(), 300), "done": null, "after": [], "model": null }));
    }
    out
}

/// A draft plan: the parts and their order, from a fast model or from the
/// person's words. Nothing is created and no agent starts.
pub fn draft(sup: &Supervisor, repo: &std::path::Path, text: &str, model: Option<&str>) -> Json {
    let m = model.unwrap_or("haiku").to_string();
    let args: Vec<String> = ["-p", "--output-format", "json", "--tools", "", "--strict-mcp-config", "--settings", "{\"disableAllHooks\":true}", "--no-session-persistence", "--model", &m, "--system-prompt", DRAFT_SYSTEM]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(text.to_string()))
        .collect();
    let (parts, by, missing) = match run_print(&sup.cfg.claude_bin, &args, repo, Duration::from_secs(45)) {
        Ok(raw) => match parts_of_answer(&raw) {
            Some(p) => (p, "model", Vec::<String>::new()),
            None => (parts_of_words(text), "words", vec!["the model's answer was not a plan: these parts are your own sentences".to_string()]),
        },
        Err(e) => (parts_of_words(text), "words", vec![format!("{e}: these parts are your own sentences")]),
    };
    json!({ "draft": true, "repo": repo.to_string_lossy(), "text": text, "parts": parts, "drafted_by": by, "model": if by == "model" { Some(m) } else { None }, "missing": missing })
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outcome_follows_its_agent() {
        let o = Outcome { agent: Some("a1".into()), state: "active".into(), ..Outcome::default() };
        let mut a = Agent { id: "a1".into(), state: "working".into(), ..Agent::default() };
        let none = HashSet::new();
        assert_eq!(derived(&o, Some(&a), true, &none), "active");
        a.state = "waiting".into();
        assert_eq!(derived(&o, Some(&a), true, &none), "needs_you");
        a.state = "idle".into();
        assert_eq!(derived(&o, Some(&a), true, &["a1".to_string()].into()), "needs_you");
        a.state = "exited".into();
        a.exit_code = Some(0);
        assert_eq!(derived(&o, Some(&a), false, &none), "needs_you");
        let o2 = Outcome { progress: 100, ..o.clone() };
        assert_eq!(derived(&o2, Some(&a), false, &none), "done");
        let o3 = Outcome { state: "dropped".into(), ..o.clone() };
        assert_eq!(derived(&o3, Some(&a), true, &none), "dropped");
        let o4 = Outcome { agent: None, state: "next".into(), ..Outcome::default() };
        assert_eq!(derived(&o4, None, false, &none), "next");
    }
}
