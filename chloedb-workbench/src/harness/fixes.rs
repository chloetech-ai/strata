//! Fix the harness: every problem the agents caused in the window, joined
//! into one ranked list of changes to the harness.
//!
//! A finding is one problem (a storm of prompts, a record left out before a
//! failing test, the same correction typed to two agents). A fix is the
//! change that answers one or more of them. Several problems that call for
//! the same edit are one fix, so "pin the shipping decision to cart.py"
//! counts every turn it would have saved.
//!
//! Two kinds of fix can be applied now, because the harness has an editor for
//! them: an allow rule (permissions) and a pin (memory). The rest are listed
//! as ideas, saying which editor they wait for. The list is ranked by what
//! each fix saves, in the unit its evidence measures (time first), then by
//! how strong the evidence is. The best fix that does not widen what agents
//! may do is the recommended one, and the only one ticked. Applying puts the
//! ticked fixes in a draft, never in the current version; promoting stays a
//! separate step with the widening check. Nothing here sends an agent
//! anything: sending an agent back is about one agent's work, a fix is about
//! every agent to come.

use super::*;

/// The fixes and ideas of the window, worst first.
pub(super) fn build(sup: &Supervisor, w: &Win, cur_v: &Version, cur_e: &Edits, found: &Found) -> Json {
    let rules = rules_under(sup, &w.repo, cur_v, cur_e);
    let base = baseline(sup, &w.repo);
    let mut fixes: Vec<Json> = Vec::new();

    // Permission storms: one allow rule each, as agents type the command.
    for (key, n) in &found.storms {
        let rule = format!("Bash({key}:*)");
        let mut e = cur_e.permissions.clone();
        if !e.allow.contains(&rule) {
            e.allow.push(rule.clone());
        }
        let after = perms::effective(&base, &e, "draft");
        let imp = perms::impact(&rules, &after, &w.calls);
        let wid = perms::widening(&rules, &after, &w.calls, &w.cwd());
        let mine: Vec<&perms::Call> = w.calls.iter().filter(|c| c.prompted && c.probe.tool == "Bash" && c.probe.command.as_deref().and_then(storm_key).as_deref() == Some(key.as_str())).collect();
        let wait: i64 = mine.iter().map(|c| c.wait_ms).sum();
        let saved = imp["saved"].as_i64().unwrap_or(0);
        let agents: Vec<String> = names_of(&mine.iter().map(|c| c.name.clone()).collect::<HashSet<String>>());
        fixes.push(json!({
            "id": hash(&format!("allow\n{rule}")), "part": "permissions", "title": format!("Allow `{key}` as agents type it"),
            "diff": [["+", format!("allow  {rule}")]],
            "edit": { "permissions": { "allow": [rule] } },
            "answers": [{ "kind": "storm", "title": format!("Permission storm: {key}"), "detail": format!("{n} prompts; {} of them in {} session{}", mine.len(), agents.len(), if agents.len() == 1 { "" } else { "s" }), "agents": agents }],
            "problems": 1, "cost_ms": wait, "cost": if wait > 0 { fmt_dur(wait) } else { format!("{n} prompts") },
            "impact": format!("−{saved} of {} prompts · {} denies changed", imp["prompts_before"], imp["denies_changed"]),
            "numbers": { "prompts": saved, "records": 0, "misses": 0, "ms": wait },
            "level": "counted", "widens": !wid.is_empty(),
            "alt": "Or make agents need no prefix (a change to the repository, through Review).",
        }));
    }

    // Missed memory: one pin per record, whichever turns it was missed at.
    let mut by_record: BTreeMap<&str, Vec<&memory::Miss>> = BTreeMap::new();
    for m in &found.misses {
        by_record.entry(m.record.as_str()).or_default().push(m);
    }
    for (record, ms) in by_record {
        let first = ms[0];
        if cur_e.memory.pins.iter().any(|p| p.record == record) {
            continue;
        }
        let pin = Pin { file: first.file.clone(), record: record.to_string(), text: first.text.clone() };
        let mut d = cur_e.memory.clone();
        d.pins.push(pin.clone());
        let rc = memory::recompute(&found.rcs, &cur_e.memory, &d, &found.misses);
        let fixed_here = rc["misses_fixed"].as_array().map_or(0, |a| a.iter().filter(|x| x["record"] == json!(record)).count());
        let cost: i64 = ms.iter().map(|m| m.cost_ms).sum();
        let failed: i64 = ms.iter().map(|m| m.failed).sum();
        fixes.push(json!({
            "id": hash(&format!("pin\n{record}")), "part": "memory", "title": format!("Pin “{}” to {}", one_line(&first.text, 50), first.file),
            "diff": [["+", format!("pin    {} → “{}”", first.file, one_line(&first.text, 60))]],
            "edit": { "memory": { "pins": [pin] } },
            "answers": ms.iter().map(|m| json!({
                "kind": "missed_memory", "agent": m.agent, "name": m.name, "turn": m.turn, "file": m.file,
                "title": format!("{}'s turn {} left it out", m.name, m.turn),
                "detail": format!("scored {:.2} and was {}; {} edited {} and {} test run{} failed after", m.score, m.decision, m.name, m.file, m.failed, if m.failed == 1 { "" } else { "s" }),
                "href": format!("#/debug/{}/{}/wrong", m.agent, m.turn),
            })).collect::<Vec<_>>(),
            "problems": ms.len(), "cost_ms": cost, "cost": if cost > 0 { fmt_dur(cost) } else { format!("{failed} failed run{}", if failed == 1 { "" } else { "s" }) },
            "impact": format!("+{} records in {} sessions · {fixed_here} miss{} fixed", rc["added"], rc["sessions_changed"].as_array().map_or(0, |a| a.len()), if fixed_here == 1 { "" } else { "es" }),
            "numbers": { "prompts": 0, "records": rc["added"].as_i64().unwrap_or(0), "misses": fixed_here, "ms": cost },
            "level": "recomputed", "widens": false,
        }));
    }

    let recommended = rank(&mut fixes);

    // The problems no editor of this build can change yet.
    let mut ideas: Vec<Json> = Vec::new();
    for f in &found.findings {
        let kind = f["kind"].as_str().unwrap_or("");
        let title = f["title"].as_str().unwrap_or("");
        let (idea, needs, link) = match kind {
            "unused" => (format!("Leave out {}", title.trim_start_matches("Unused ")), "the tools editor", None),
            "slow_hook" => ("Lengthen the timeout or warm up the hook".to_string(), "the hooks editor", None),
            "corrected" => ("Make what you keep correcting a rule".to_string(), "", Some("#/more/rules")),
            "redo" => ("Write a rule for the loop".to_string(), "", Some("#/more/rules")),
            "auto_mode" => ("Allow what auto mode blocked, where it is safe".to_string(), "", Some("#/harness/edit/permissions")),
            _ => continue,
        };
        ideas.push(json!({
            "id": hash(&format!("idea\n{kind}\n{title}")), "kind": kind, "title": idea, "problem": title,
            "cost_ms": f["cost_ms"], "cost": f["cost"], "level": f["level"],
            "needs": if needs.is_empty() { Json::Null } else { json!(needs) }, "link": link,
            "applicable": false,
        }));
    }

    let problems = found.findings.len();
    let answered: usize = fixes.iter().map(|f| f["problems"].as_u64().unwrap_or(0) as usize).sum();
    let ms: i64 = fixes.iter().map(|f| f["cost_ms"].as_i64().unwrap_or(0)).sum();
    // The agents behind the problems, for a separate "send back" step.
    let mut agents: BTreeMap<String, Json> = BTreeMap::new();
    for f in &fixes {
        for a in f["answers"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (a["agent"].as_str(), a["name"].as_str()) {
                agents.entry(id.to_string()).or_insert_with(|| json!({ "id": id, "name": name, "turn": a["turn"] }));
            }
        }
    }
    json!({
        "repo": w.repo,
        "current": cur_v.to_json(),
        "fixes": fixes,
        "ideas": ideas,
        "recommended": recommended.map(|i| fixes[i]["id"].clone()),
        "summary": { "problems": problems, "answered": answered, "fixes": fixes.len(), "ideas": ideas.len(), "cost_ms": ms },
        "agents": agents.into_values().collect::<Vec<_>>(),
    })
}


/// Best first: what a fix saves in time, then how many problems it answers,
/// then how strong the evidence is; a fix that widens what agents may do is
/// never first. Marks the rank, which is recommended (the best one that does
/// not widen) and that each can be applied. Answers the recommended index.
fn rank(fixes: &mut [Json]) -> Option<usize> {
    let strength = |f: &Json| if f["level"] == json!("counted") { 0 } else { 1 };
    fixes.sort_by(|a, b| {
        let key = |f: &Json| (f["widens"].as_bool().unwrap_or(false), std::cmp::Reverse(f["cost_ms"].as_i64().unwrap_or(0)), std::cmp::Reverse(f["problems"].as_i64().unwrap_or(0)), strength(f));
        key(a).cmp(&key(b))
    });
    let recommended = fixes.iter().position(|f| !f["widens"].as_bool().unwrap_or(false));
    for (i, f) in fixes.iter_mut().enumerate() {
        f["rank"] = json!(i + 1);
        f["recommended"] = json!(Some(i) == recommended);
        f["applicable"] = json!(true);
    }
    recommended
}

/// `POST /api/harness/fixes/apply {repo, ids}`: the chosen fixes, in one
/// draft. Nothing is promoted and no agent is sent anything.
#[derive(Deserialize, Default)]
pub(super) struct ApplyB {
    pub repo: Option<String>,
    #[serde(default)]
    pub ids: Vec<String>,
}

pub(super) fn apply(sup: &Supervisor, b: ApplyB) -> Result<Json, ApiError> {
    if b.ids.is_empty() {
        return Err(ApiError::bad("tick at least one fix"));
    }
    let repo = repo_or_default(sup, b.repo)?;
    let (from, to) = window_of(Some(WINDOW_DAYS));
    let w = Win::load(sup, &repo, from, to);
    let cur = current(sup, &repo);
    let cur_e = edits_of(sup, &cur.id);
    let rules = rules_under(sup, &repo, &cur, &cur_e);
    let found = find(sup, &w, &rules, true);
    let all = build(sup, &w, &cur, &cur_e, &found);
    let chosen: Vec<&Json> = all["fixes"].as_array().into_iter().flatten().filter(|f| b.ids.iter().any(|i| f["id"] == json!(i))).collect();
    if chosen.len() != b.ids.len() {
        return Err(ApiError::new(409, "gone", "a ticked fix is not in this week's list any more: reload the list"));
    }
    // From the newest draft when there is one, else from the current version.
    let draft = versions(sup, &repo).into_iter().find(|v| v.kind == "config" && v.state == "draft");
    let parent = draft.unwrap_or_else(|| cur.clone());
    let mut e = edits_of(sup, &parent.id);
    let mut widens = false;
    for f in &chosen {
        widens |= f["widens"].as_bool().unwrap_or(false);
        for r in f["edit"]["permissions"]["allow"].as_array().into_iter().flatten().filter_map(|r| r.as_str()) {
            if !e.permissions.allow.iter().any(|x| x == r) {
                e.permissions.allow.push(r.to_string());
            }
        }
        for p in f["edit"]["memory"]["pins"].as_array().into_iter().flatten() {
            if let Ok(pin) = serde_json::from_value::<Pin>(p.clone()) {
                if !e.memory.pins.iter().any(|x| x.record == pin.record) {
                    e.memory.pins.push(pin);
                }
            }
        }
    }
    let titles: Vec<String> = chosen.iter().map(|f| f["title"].as_str().unwrap_or("").to_string()).collect();
    let mut parts = serde_json::Map::new();
    parts.insert("permissions".into(), serde_json::to_value(&e.permissions).unwrap_or_default());
    parts.insert("memory".into(), serde_json::to_value(&e.memory).unwrap_or_default());
    let note = format!("From Fix the harness: {}", titles.join("; "));
    let mut v = new_version(sup, NewVersionB { repo: Some(repo), parent: Some(parent.id.clone()), name: None, note: Some(one_line(&note, 400)), parts })?;
    v["applied"] = json!(titles);
    v["widens"] = json!(widens);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(id: &str, cost_ms: i64, problems: i64, level: &str, widens: bool) -> Json {
        json!({ "id": id, "cost_ms": cost_ms, "problems": problems, "level": level, "widens": widens })
    }

    #[test]
    fn the_best_fix_that_does_not_widen_is_recommended_and_the_only_one() {
        let mut v = vec![
            fix("storm", 900_000, 1, "counted", true),
            fix("pin-small", 60_000, 1, "recomputed", false),
            fix("pin-big", 300_000, 3, "recomputed", false),
        ];
        let r = rank(&mut v);
        let ids: Vec<&str> = v.iter().map(|f| f["id"].as_str().unwrap()).collect();
        // The storm saves the most but widens, so it is last; of the others, the larger saving is first.
        assert_eq!(ids, ["pin-big", "pin-small", "storm"]);
        assert_eq!(r, Some(0));
        assert_eq!(v.iter().filter(|f| f["recommended"] == json!(true)).count(), 1);
        assert_eq!(v[0]["rank"], 1);
        assert!(v.iter().all(|f| f["applicable"] == json!(true)));
    }

    #[test]
    fn when_every_fix_widens_none_is_recommended() {
        let mut v = vec![fix("a", 10, 1, "counted", true), fix("b", 20, 1, "counted", true)];
        assert_eq!(rank(&mut v), None);
        assert!(v.iter().all(|f| f["recommended"] == json!(false)));
        assert_eq!(v[0]["id"], "b");
    }

    #[test]
    fn on_equal_savings_more_problems_answered_then_counted_evidence_win() {
        let mut v = vec![fix("one", 100, 1, "counted", false), fix("two", 100, 2, "recomputed", false), fix("two-counted", 100, 2, "counted", false)];
        rank(&mut v);
        let ids: Vec<&str> = v.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["two-counted", "two", "one"]);
    }
}
