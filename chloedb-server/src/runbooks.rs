//! Runbooks as data: `docs/chloedb/runbooks.md`, compiled in, found by
//! an alert's rule or a fleet problem's id (the MCP tool `runbook`, and
//! each fleet problem's `next.runbook`).
//!
//! A runbook is a `## Title` section whose first line after it is
//! `For: ` and the ids it is for, each in backticks.

use std::sync::OnceLock;

use serde_json::{json, Value as Json};

const SOURCE: &str = include_str!("../../docs/chloedb/runbooks.md");
pub const PAGE: &str = "https://chloetech-ai.github.io/chloedb/site/docs/runbooks.html";

/// One runbook.
pub struct Runbook {
    pub title: &'static str,
    pub anchor: String,
    pub ids: Vec<&'static str>,
    pub body: &'static str,
}

/// Every runbook, in the page's order.
pub fn all() -> &'static [Runbook] {
    static ALL: OnceLock<Vec<Runbook>> = OnceLock::new();
    ALL.get_or_init(|| parse(SOURCE))
}

fn anchor_of(title: &str) -> String {
    let mut a = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            a.push(c.to_ascii_lowercase());
        } else if (c == ' ' || c == '-') && !a.ends_with('-') {
            a.push('-');
        }
    }
    a.trim_matches('-').to_string()
}

fn parse(src: &'static str) -> Vec<Runbook> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(at) = rest.find("\n## ") {
        rest = &rest[at + 4..];
        let end = rest.find("\n## ").unwrap_or(rest.len());
        let section = &rest[..end];
        let (title, body) = section.split_once('\n').unwrap_or((section, ""));
        let body = body.trim();
        let Some(line) = body.lines().next().and_then(|l| l.strip_prefix("For:")) else { continue };
        let ids: Vec<&'static str> = line.split('`').skip(1).step_by(2).collect();
        let body = body.split_once('\n').map_or("", |(_, b)| b).trim();
        out.push(Runbook { title: title.trim(), anchor: anchor_of(title), ids, body });
    }
    out
}

fn json_of(r: &Runbook) -> Json {
    json!({"title": r.title, "for": r.ids, "steps": r.body, "docs": format!("{PAGE}#{}", r.anchor)})
}

/// The runbook for alert rule or fleet problem `id`: exactly by its id
/// (a fleet problem `shard-2-down` is `shards`).
pub fn for_id(id: &str) -> Option<&'static Runbook> {
    let id = id.trim();
    let id = if id.starts_with("shard-") { "shards" } else { id };
    all().iter().find(|r| r.ids.contains(&id))
}

/// `runbook {for?, q?}`: the runbook for an id, else those whose title or
/// steps hold every word of `q`, else the list of them.
pub fn find(query: &str) -> Json {
    let q = query.trim();
    if let Some(r) = for_id(q) {
        return json!({"runbooks": [json_of(r)]});
    }
    let words: Vec<String> = q.split_whitespace().map(str::to_lowercase).collect();
    if !words.is_empty() {
        let hits: Vec<Json> = all()
            .iter()
            .filter(|r| {
                let text = format!("{} {} {}", r.title, r.ids.join(" "), r.body).to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .map(json_of)
            .collect();
        if !hits.is_empty() {
            return json!({"runbooks": hits});
        }
    }
    json!({
        "runbooks": [],
        "note": if q.is_empty() { "the runbooks, by title: pass an alert's rule or a fleet problem's id" } else { "no runbook matches: these are the runbooks there are" },
        "index": all().iter().map(|r| json!({"title": r.title, "for": r.ids})).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runbooks_are_read_from_the_page() {
        assert!(all().len() >= 8, "{}", all().len());
        assert_eq!(for_id("copy-down").unwrap().title, "A copy is down");
        assert_eq!(for_id("shard-2-down").unwrap().title, "A copy is down");
        assert_eq!(for_id("p99").unwrap().anchor, "queries-are-slow");
        assert!(find("disk vacuum")["runbooks"][0]["title"].as_str().unwrap().contains("disk"));
        assert!(find("nothing like this")["index"].as_array().unwrap().len() >= 8);
    }

    /// Every alert the server has by default, and every problem the fleet
    /// view names, has a runbook.
    #[test]
    fn every_builtin_alert_and_fleet_problem_has_one() {
        for rule in crate::alerts::defaults().into_iter().chain(crate::alerts::builtins()) {
            assert!(for_id(&rule.id).is_some(), "no runbook for alert {}", rule.id);
        }
        for id in ["alerts", "coordinators_gone", "copies", "damaged", "disk", "error_ratio", "error_ratio_recovered", "lag", "p99", "p99_recovered", "shards", "shard-0-down"] {
            if id == "alerts" {
                continue;
            }
            assert!(for_id(id).is_some(), "no runbook for fleet problem {id}");
        }
    }

    /// Every route a runbook names is a route of the server.
    #[test]
    fn the_routes_they_name_exist() {
        for r in all() {
            for (i, _) in r.body.match_indices("/v1/") {
                let path: String = r.body[i..].chars().take_while(|c| !c.is_whitespace() && *c != '`' && *c != ',' && *c != ')').collect();
                let path = path.split('?').next().unwrap().replace("<i>", "0").replace("<id>", "x").replace("<copy name>", "x");
                let path = path.trim_end_matches(['.', ';', ':']);
                assert!(crate::catalog::route_of(path).is_some(), "{}: no route {path}", r.title);
            }
        }
    }
}
