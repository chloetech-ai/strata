//! What the boxes offer beyond the token (SPEC.md, "Fifth build:
//! Autocomplete", the bar): proposals before the first keystroke, a ghost
//! sentence that completes the whole prompt with its done check, a
//! "because" on every suggestion, ranking that learns from what each user
//! picks and writes, and an optional model ghost when the user pauses.
//!
//! Everything but the model ghost reads ChloeDB and is deterministic.

use super::*;

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::db::Agent;
use crate::supervisor::one_line;

/// Proposals and habits are reused for this long.
const PROPOSALS_FOR: Duration = Duration::from_secs(3);
const HABITS_FOR: Duration = Duration::from_secs(30);
/// How long a model ghost may take.
const MODEL_TIMEOUT: Duration = Duration::from_secs(25);
const MODEL: &str = "haiku";

// ------------------------------------------------------------- who

/// The user a request is for: the id the browser keeps, else "you".
pub fn user_of(u: Option<&str>) -> String {
    let u: String = u.unwrap_or("").trim().chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@')).take(64).collect();
    if u.is_empty() {
        "you".into()
    } else {
        u
    }
}

// ------------------------------------------------------------- time

/// "Nov 9", in the server's time zone.
pub fn short_date(ms: i64) -> String {
    let days = (ms / 1000 + local_offset_s()).div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {d}", M[(m - 1) as usize])
}

/// The server's offset from UTC, in seconds (`date +%z`, read once).
fn local_offset_s() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let out = Command::new("date").arg("+%z").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        let (sign, rest) = match out.chars().next() {
            Some('-') => (-1, &out[1..]),
            Some('+') => (1, &out[1..]),
            _ => return 0,
        };
        let h: i64 = rest.get(..2).and_then(|x| x.parse().ok()).unwrap_or(0);
        let m: i64 = rest.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0);
        sign * (h * 3600 + m * 60)
    })
}

/// Local midnight of today, in ms.
fn midnight_ms() -> i64 {
    let off = local_offset_s() * 1000;
    (now_ms() + off).div_euclid(86_400_000) * 86_400_000 - off
}

fn span(ms: i64) -> String {
    let s = (ms / 1000).max(60);
    match s {
        0..=3599 => format!("{} min", s / 60),
        3600..=172_799 => format!("{} h", s / 3600),
        _ => format!("{} days", s / 86_400),
    }
}

// ------------------------------------------------------------- habits

/// What one user picked and passed over, by suggestion and by kind,
/// weighted by how recent (a two-week half life).
#[derive(Default)]
pub struct Habits {
    picks: HashMap<String, (f64, u32)>,
    passed: HashMap<String, u32>,
    kinds: HashMap<String, f64>,
}

fn hkey(kind: &str, label: &str) -> String {
    format!("{kind}|{}", label.trim().to_lowercase())
}

impl Habits {
    /// The points a suggestion gains for this user, and how often they picked it.
    pub fn boost(&self, kind: &str, label: &str) -> (f64, u32) {
        let k = hkey(kind, label);
        let (w, n) = self.picks.get(&k).copied().unwrap_or((0.0, 0));
        let kind_w = self.kinds.get(kind).copied().unwrap_or(0.0);
        let mut b = 12.0 * (1.0 - (-w).exp()) + 3.0 * (1.0 - (-kind_w / 4.0).exp());
        if n == 0 {
            b -= self.passed.get(&k).copied().unwrap_or(0).min(6) as f64 * 0.7;
        }
        (b, n)
    }
}

type HabitsCache = Mutex<HashMap<String, (Instant, Arc<Habits>)>>;

fn habits_cache() -> &'static HabitsCache {
    static C: OnceLock<HabitsCache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn habits(sup: &Supervisor, user: &str, repo: &str, field: &str) -> Arc<Habits> {
    let key = format!("{user}|{repo}|{field}");
    if let Some((at, h)) = habits_cache().lock().unwrap().get(&key) {
        if at.elapsed() < HABITS_FOR {
            return h.clone();
        }
    }
    let rows = q(sup, "SELECT kind, label, accepted, ts, field FROM complete_events WHERE \"user\" = :u AND repo = :r ORDER BY ts DESC LIMIT 5000", &Params::new().set("u", sv(user)).set("r", sv(repo))).unwrap_or_default();
    let now = now_ms();
    let mut h = Habits::default();
    for r in rows {
        let kind = get_str(&r[0]).unwrap_or_default();
        if kind == "typed" {
            continue;
        }
        let label = get_str(&r[1]).unwrap_or_default();
        let age_days = (now - get_ms(&r[3]).unwrap_or(now)).max(0) as f64 / 86_400_000.0;
        let w = 0.5f64.powf(age_days / 14.0) * if get_str(&r[4]).as_deref() == Some(field) { 1.0 } else { 0.5 };
        if get_int(&r[2]).unwrap_or(0) != 0 {
            let e = h.picks.entry(hkey(&kind, &label)).or_insert((0.0, 0));
            e.0 += w;
            e.1 += 1;
            *h.kinds.entry(kind).or_insert(0.0) += w;
        } else {
            *h.passed.entry(hkey(&kind, &label)).or_insert(0) += 1;
        }
    }
    let h = Arc::new(h);
    habits_cache().lock().unwrap().insert(key, (Instant::now(), h.clone()));
    h
}

#[derive(Deserialize)]
pub struct Shown {
    pub kind: Option<String>,
    pub label: Option<String>,
    pub rank: Option<i64>,
    pub source: Option<String>,
}

#[derive(Deserialize)]
pub struct EventsB {
    pub user: Option<String>,
    pub repo: Option<String>,
    pub field: Option<String>,
    pub agent: Option<String>,
    pub shown: Option<Vec<Shown>>,
    pub accepted: Option<Shown>,
    pub typed: Option<String>,
}

/// Records what a box showed, what was picked and what was passed over,
/// and what the user wrote in the end.
pub fn record(sup: &Arc<Supervisor>, b: EventsB) -> Result<Json, ApiError> {
    static N: AtomicU64 = AtomicU64::new(0);
    let user = user_of(b.user.as_deref());
    let repo = repo_for(sup, b.repo.as_deref(), b.agent.as_deref())?;
    if repo.is_empty() {
        return Err(ApiError::bad("no repository"));
    }
    let field = b.field.filter(|f| !f.is_empty()).unwrap_or_else(|| "task".into());
    let agent = b.agent.filter(|a| !a.is_empty());
    let ctx = agent.as_deref().and_then(|a| sup.agent_row(a)).map(|a| format!("{}: {}", a.name, a.last_event.unwrap_or_default())).unwrap_or_default();
    let now = now_ms();
    let mut rows: Vec<(String, String, bool, i64, String, String)> = Vec::new();
    let acc = b.accepted.as_ref().map(|a| (a.kind.clone().unwrap_or_default(), a.label.clone().unwrap_or_default()));
    let mut acc_seen = false;
    for s in b.shown.unwrap_or_default().into_iter().take(30) {
        let (kind, label) = (s.kind.unwrap_or_default(), s.label.unwrap_or_default());
        if kind.is_empty() || label.is_empty() {
            continue;
        }
        let yes = acc.as_ref().is_some_and(|(k, l)| *k == kind && *l == label);
        acc_seen |= yes;
        rows.push((kind, label, yes, s.rank.unwrap_or(-1), s.source.unwrap_or_else(|| "chloedb".into()), String::new()));
    }
    if let (Some((k, l)), false) = (&acc, acc_seen) {
        let a = b.accepted.as_ref().unwrap();
        if !k.is_empty() && !l.is_empty() {
            rows.push((k.clone(), l.clone(), true, a.rank.unwrap_or(-1), a.source.clone().unwrap_or_else(|| "chloedb".into()), String::new()));
        }
    }
    if let Some(t) = b.typed.map(|t| t.trim().to_string()).filter(|t| t.chars().count() >= 4) {
        rows.push(("typed".into(), one_line(&t, 200), true, -1, "user".into(), t));
    }
    for (i, (kind, label, yes, rank, source, text)) in rows.iter().enumerate() {
        let id = format!("{now}-{}-{i}", N.fetch_add(1, Ordering::Relaxed));
        let p = Params::new()
            .set("id", Value::Str(id))
            .set("u", sv(&user))
            .set("r", sv(&repo))
            .set("f", sv(&field))
            .set("a", agent.as_deref().map(sv).unwrap_or(Value::Null))
            .set("k", sv(kind))
            .set("l", sv(&one_line(label, 300)))
            .set("c", sv(&ctx))
            .set("t", sv(text))
            .set("y", Value::Bool(*yes))
            .set("n", Value::Int(*rank))
            .set("s", sv(source))
            .set("ts", crate::db::ts(now));
        q(sup, "INSERT INTO complete_events (id, \"user\", repo, field, agent, kind, label, ctx, text, accepted, rank, source, ts) VALUES (:id, :u, :r, :f, :a, :k, :l, :c, :t, :y, :n, :s, :ts)", &p).map_err(ApiError::internal)?;
    }
    habits_cache().lock().unwrap().retain(|k, _| !k.starts_with(&format!("{user}|{repo}|")));
    Ok(json!({ "ok": true, "rows": rows.len(), "user": user }))
}

/// Whether the ranking learns: the rank of what was picked, early against late.
pub fn stats(sup: &Supervisor, user: &str, repo: Option<&str>) -> Json {
    let (sql, p) = match repo.filter(|r| !r.is_empty()) {
        Some(r) => ("SELECT kind, accepted, rank, ts, field FROM complete_events WHERE \"user\" = :u AND repo = :r ORDER BY ts ASC LIMIT 50000", Params::new().set("u", sv(user)).set("r", sv(r))),
        None => ("SELECT kind, accepted, rank, ts, field FROM complete_events WHERE \"user\" = :u ORDER BY ts ASC LIMIT 50000", Params::new().set("u", sv(user))),
    };
    let rows = q(sup, sql, &p).unwrap_or_default();
    let mut ranks: Vec<i64> = Vec::new();
    let (mut shown, mut typed) = (0usize, 0usize);
    for r in &rows {
        match get_str(&r[0]).as_deref() {
            Some("typed") => typed += 1,
            _ => {
                shown += 1;
                if get_int(&r[1]).unwrap_or(0) != 0 {
                    if let Some(n) = get_int(&r[2]).filter(|n| *n >= 0) {
                        ranks.push(n);
                    }
                }
            }
        }
    }
    let part = |rs: &[i64]| -> Json {
        if rs.is_empty() {
            return json!({ "n": 0 });
        }
        let mrr = rs.iter().map(|r| 1.0 / (*r as f64 + 1.0)).sum::<f64>() / rs.len() as f64;
        let top1 = rs.iter().filter(|r| **r == 0).count() as f64 / rs.len() as f64;
        json!({ "n": rs.len(), "mrr": (mrr * 1000.0).round() / 1000.0, "top1": (top1 * 1000.0).round() / 1000.0 })
    };
    let half = ranks.len() / 2;
    json!({
        "user": user,
        "shown": shown,
        "accepted": ranks.len(),
        "typed": typed,
        "acceptance": if shown == 0 { json!(null) } else { json!(((ranks.len() as f64 / shown as f64) * 1000.0).round() / 1000.0) },
        "all": part(&ranks),
        "first_half": part(&ranks[..half]),
        "second_half": part(&ranks[half..]),
    })
}

// ------------------------------------------------------------- the user's own words

/// The user's own earlier texts of this field like the one being typed
/// (BM25 over what they wrote), and ChloeCtx's record of their prompts.
/// A typed text as BM25 terms (its last word a prefix that survives
/// stemming), and how many must match.
pub fn terms_of(typed: &str) -> (String, usize) {
    let mut terms: Vec<String> = words(typed).into_iter().map(|w| w.0).collect();
    if typed.chars().last().is_some_and(|c| c.is_alphanumeric()) {
        if let Some(l) = terms.last_mut() {
            let n = l.chars().count();
            if n > 4 {
                *l = l.chars().take((n - 2).max(4)).collect();
            }
            l.push('*');
        }
    }
    let need = terms.len().min(2);
    (terms.join(" "), need)
}

pub(super) fn mine(sup: &Supervisor, user: &str, repo: &str, field: &str, text: &str, qs: &str, need: usize) -> Vec<Sugg> {
    let mut out = Vec::new();
    let typed_l = text.trim().to_lowercase();
    let mut seen = HashSet::new();
    let p = Params::new().set("u", sv(user)).set("r", sv(repo)).set("f", sv(field)).set("q", sv(qs));
    let sql = format!("SELECT text, ts, agent, score() FROM complete_events WHERE \"user\" = :u AND repo = :r AND field = :f AND kind = 'typed' ORDER BY bm25(text, :q, min => {need}) DESC LIMIT 6");
    for r in q(sup, &sql, &p).unwrap_or_default() {
        let t = get_str(&r[0]).unwrap_or_default();
        if float(&r[3]) <= 0.0 || t.trim().to_lowercase() == typed_l || !seen.insert(t.trim().to_lowercase()) {
            continue;
        }
        let when = get_ms(&r[1]).map(super::ago).unwrap_or_default();
        let to = get_str(&r[2]).and_then(|a| sup.agent_row(&a)).map(|a| format!(" to {}", a.name)).unwrap_or_default();
        let mut s = sugg("mine", 23.0 + float(&r[3]).min(5.0) * 0.2, &one_line(&t, 200), &t, "you wrote this", json!({ "ts": get_ms(&r[1]).map(iso) }), 0, text.len());
        s.json["because"] = json!(format!("you wrote this{to} {when}"));
        out.push(s);
    }
    if matches!(field, "task" | "goal" | "steer") {
        let sql = format!("SELECT label, ins, refj, score() FROM complete_items WHERE repo = :r AND kind = 'prompt' ORDER BY bm25(body, :q, min => {need}) DESC LIMIT 4");
        for r in q(sup, &sql, &p).unwrap_or_default() {
            let t = get_str(&r[1]).unwrap_or_default();
            if float(&r[3]) <= 0.0 || t.trim().to_lowercase() == typed_l || !seen.insert(t.trim().to_lowercase()) {
                continue;
            }
            let refj: Json = get_str(&r[2]).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Json::Null);
            let when = refj["ts_ms"].as_i64().map(super::ago).unwrap_or_default();
            let mut s = sugg("prompt", 20.0 + float(&r[3]).min(5.0) * 0.2, &get_str(&r[0]).unwrap_or_default(), &t, "a prompt you gave Claude", refj, 0, text.len());
            s.json["because"] = json!(format!("you asked Claude this {when} (ChloeCtx)").replace("  ", " "));
            out.push(s);
        }
    }
    out
}

/// ChloeCtx's record of what the user typed: their prompts in this
/// repository's sessions, as rows of kind `prompt`.
pub(super) fn fetch_prompts(sup: &Supervisor, dir: &Path) -> Option<Vec<Item>> {
    let sql = "SELECT id, kind, text, ts FROM memory WHERE kind = 'prompt' ORDER BY ts DESC LIMIT 300";
    let out = match trail::run_chloectx(&sup.cfg.chloectx_bin, dir, &["sql", "--json", sql], Duration::from_secs(20)) {
        Ran::Out(v) => v,
        _ => return None,
    };
    let cols: Vec<String> = out["columns"].as_array().into_iter().flatten().map(|c| c.as_str().or_else(|| c["name"].as_str()).unwrap_or("").to_string()).collect();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for row in out["rows"].as_array().into_iter().flatten().filter_map(Json::as_array) {
        let col = |name: &str| cols.iter().position(|c| c == name).and_then(|i| row.get(i).cloned()).unwrap_or(Json::Null);
        if col("kind").as_str() != Some("prompt") {
            continue;
        }
        let text = col("text").as_str().unwrap_or("").trim().to_string();
        // Not a command, not a pasted wall, not a repeat.
        if text.chars().count() < 12 || text.chars().count() > 600 || text.starts_with('/') || text.starts_with('<') || !seen.insert(text.to_lowercase()) {
            continue;
        }
        let id = col("id").as_str().map(str::to_string).unwrap_or_else(|| col("id").to_string());
        let mut it = Item::new("prompt", &id, one_line(&text, 200));
        it.ins = text.clone();
        it.body = text;
        it.refj = json!({ "id": id, "ts_ms": trail::ctx_ms(&col("ts")) });
        items.push(it);
    }
    Some(items)
}

// ------------------------------------------------------------- because

/// A line on each suggestion that says why it is offered.
pub fn because(sup: &Supervisor, s: &mut Json, editors: &HashMap<String, (String, String)>) {
    if s.get("because").is_some_and(|b| b.is_string()) {
        return;
    }
    let r = s["ref"].clone();
    let kind = s["kind"].as_str().unwrap_or("").to_string();
    let line: Option<String> = match kind.as_str() {
        // Its state is in the detail already.
        "agent" => r["id"].as_str().and_then(|id| sup.agent_row(id)).map(|a| {
            let d = doing_now(sup, &a);
            d.strip_prefix(&format!("{} · ", a.state.replace('_', " "))).map(str::to_string).unwrap_or(d)
        }),
        "file" => s["label"].as_str().and_then(|f| editors.get(f)).map(|(n, w)| format!("{n} edited it {w}")),
        "symbol" => r["path"].as_str().and_then(|f| editors.get(f).map(|(n, w)| format!("{n} edited {f} {w}"))),
        "decision" | "preference" | "fact" => {
            let when = r["ts"].as_str().and_then(chloedb_ts_ms).map(short_date);
            let by = r["by"].as_str().map(|b| format!(" by {b}")).unwrap_or_default();
            Some(if r["superseded"] == true {
                format!("◆ superseded; decided {}{by}", when.unwrap_or_default())
            } else {
                format!("◆ {} {}{by}", if kind == "decision" { "decided" } else { "noted" }, when.unwrap_or_else(|| "earlier".into()))
            })
        }
        "rule" => Some(format!("your rule{}; every agent is told it", r["ts"].as_str().and_then(chloedb_ts_ms).map(|t| format!(" since {}", short_date(t))).unwrap_or_default())),
        "test" => {
            let last = r["last"].as_str().unwrap_or("");
            let when = r["last_ms"].as_i64().map(super::ago).unwrap_or_default();
            match (last, r["failed"].as_i64().unwrap_or(0)) {
                ("failed", _) => Some(format!("failed {when}")),
                (_, f) if f > 0 => Some(format!("flaky: failed {f} of {} runs", r["runs"].as_i64().unwrap_or(0))),
                _ if !when.is_empty() => Some(format!("passed {when}")),
                _ => None,
            }
        }
        "task" => r["agent"].as_str().and_then(|id| sup.agent_row(id)).map(|a| format!("like what you are typing: {}'s task, {}", a.name, super::ago(a.created))),
        "goal" => Some("an earlier goal of this repository".into()),
        "trace" => Some("asks Trace: git, the agents' edits and ◆ decisions".into()),
        _ => None,
    };
    if let Some(l) = line.filter(|l| !l.trim().is_empty()) {
        s["because"] = json!(l);
    }
}

fn chloedb_ts_ms(s: &str) -> Option<i64> {
    trail::ctx_ms(&json!(s))
}

/// What an agent is doing now: its last tool call and since when.
pub fn doing_now(sup: &Supervisor, a: &Agent) -> String {
    let rows = q(sup, "SELECT tool, summary, ts FROM events WHERE agent = :a AND tool IS NOT NULL ORDER BY seq DESC LIMIT 1", &Params::new().set("a", sv(&a.id))).unwrap_or_default();
    let running = sup.state(&a.id).is_some_and(|(_, r)| r);
    if !running {
        return format!("exited{}", a.ended.map(|e| format!(" {}", super::ago(e))).unwrap_or_default());
    }
    match rows.first() {
        Some(r) => format!("{} · now: {} · {}", a.state.replace('_', " "), one_line(&get_str(&r[1]).unwrap_or_default(), 60), get_ms(&r[2]).map(super::ago).unwrap_or_default()),
        None => format!("{} · {}", a.state.replace('_', " "), a.last_event.as_deref().map(|e| one_line(e, 60)).unwrap_or_else(|| "starting".into())),
    }
}

// ------------------------------------------------------------- done checks

/// Whether a text says how its agent will know it is done.
pub fn has_done(text: &str) -> bool {
    let text_l = text.to_lowercase();
    words(text).iter().any(|w| DONE_WORDS.contains(&w.0.as_str())) || text_l.contains("done when") || text_l.contains("so that") || text_l.contains(" == ") || text_l.contains("exit 0")
}

/// The command of the tests that cover these files, and why it is that one.
pub fn covering(sup: &Supervisor, repo: &str, main: &Path, files: &[String]) -> Option<(String, String)> {
    let all = mem().lock().unwrap().get(repo).map(|e| e.files.clone()).unwrap_or_default();
    let has = |f: &str| main.join(f).exists();
    let pkg = std::fs::read_to_string(main.join("package.json")).unwrap_or_default();
    for f in files {
        let base = f.rsplit('/').next().unwrap_or(f);
        let (stem, ext) = base.rsplit_once('.').unwrap_or((base, ""));
        let is_test = |p: &str| {
            let b = p.rsplit('/').next().unwrap_or(p).to_lowercase();
            b.contains("test") || b.contains("spec")
        };
        let stem_l = stem.to_lowercase().trim_start_matches("test_").trim_end_matches("_test").trim_end_matches(".test").trim_end_matches(".spec").to_string();
        if stem_l.len() < 3 {
            continue;
        }
        let test = if is_test(f) {
            Some(f.clone())
        } else {
            all.iter().filter(|p| *p != f && is_test(p)).filter(|p| p.rsplit('/').next().unwrap_or(p).to_lowercase().contains(&stem_l)).min_by_key(|p| p.len()).cloned()
        };
        let cmd = match (ext, &test) {
            ("py", Some(t)) => Some(format!("pytest {t} -q")),
            ("ts" | "tsx" | "js" | "jsx" | "mjs", Some(t)) if pkg.contains("vitest") => Some(format!("npx vitest run {t}")),
            ("ts" | "tsx" | "js" | "jsx" | "mjs", Some(t)) if pkg.contains("jest") => Some(format!("npx jest {t}")),
            ("ts" | "tsx" | "js" | "jsx" | "mjs", Some(t)) if !pkg.is_empty() => Some(format!("npm test -- {t}")),
            ("go", _) => Some(format!("go test ./{}", f.rsplit_once('/').map(|x| x.0).unwrap_or("."))),
            ("rs", Some(t)) if t.starts_with("tests/") || t.contains("/tests/") => Some(format!("cargo test --test {}", t.rsplit('/').next().unwrap_or(t).trim_end_matches(".rs"))),
            ("rs", _) if has("Cargo.toml") || f.contains("src/") => Some(format!("cargo test {stem_l}")),
            ("rb", Some(t)) => Some(format!("bundle exec rspec {t}")),
            _ => None,
        };
        if let Some(c) = cmd {
            let why = match &test {
                Some(t) if t != f => format!("{t} covers {f}"),
                _ => format!("the tests of {f}"),
            };
            return Some((c, why));
        }
        // The tests that ran, named like the file.
        let rows = q(sup, "SELECT command, test FROM test_runs WHERE repo = :repo ORDER BY ts DESC LIMIT 2000", &Params::new().set("repo", sv(repo))).unwrap_or_default();
        if let Some(r) = rows.iter().find(|r| get_str(&r[1]).unwrap_or_default().to_lowercase().contains(&stem_l)) {
            let cmd = get_str(&r[0]).unwrap_or_default();
            if !cmd.is_empty() {
                return Some((one_line(&cmd, 80), format!("{} ran for tests named like {stem_l}", one_line(&cmd, 40))));
            }
        }
    }
    None
}

/// The files of the repository a text names: paths, or bare file names.
pub fn files_named(repo: &str, text: &str) -> Vec<String> {
    let all = mem().lock().unwrap().get(repo).map(|e| e.files.clone()).unwrap_or_default();
    let mut out = Vec::new();
    for w in text.split(|c: char| c.is_whitespace() || matches!(c, '`' | ',' | ';' | '(' | ')' | '"' | '\'')) {
        let w = w.trim_end_matches(['.', ':', '!', '?']);
        if !(w.contains('.') || w.contains('/')) || w.len() < 3 {
            continue;
        }
        let hit = if all.iter().any(|f| f == w) { Some(w.to_string()) } else { all.iter().filter(|f| f.ends_with(&format!("/{w}"))).min_by_key(|f| f.len()).cloned() };
        if let Some(f) = hit {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    out
}

fn agent_files(sup: &Supervisor, a: &Agent) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in q(sup, "SELECT file FROM file_edits WHERE agent = :a ORDER BY ts DESC LIMIT 60", &Params::new().set("a", sv(&a.id))).unwrap_or_default() {
        if let Some(f) = get_str(&r[0]) {
            if !out.contains(&f) {
                out.push(f);
            }
        }
    }
    out
}

/// A test as a sentence names it: its last part, or a whole run (a command)
/// in backticks.
fn test_name(t: &str) -> String {
    let short = t.rsplit(" › ").next().unwrap_or(t);
    if short.contains(' ') {
        format!("`{short}`")
    } else {
        short.to_string()
    }
}

/// A test command narrowed to one test, where the runner allows it.
fn test_cmd(cmd: &str, short: &str) -> String {
    let c = one_line(cmd, 80);
    let name = short.rsplit("::").next().unwrap_or(short);
    if c.starts_with("cargo test") && !c.contains(name) {
        format!("cargo test {name}")
    } else if c.starts_with("pytest") && !c.contains(name) {
        format!("pytest -k {name} -q")
    } else if c.is_empty() {
        name.to_string()
    } else {
        c
    }
}

// ------------------------------------------------------------- proposals

/// One thing an empty box proposes.
#[derive(Clone)]
pub struct Prop {
    sub: &'static str,
    text: String,
    because: String,
    prio: f64,
    go: Option<String>,
    refj: Json,
}

type PropCache = Mutex<HashMap<String, (Instant, Arc<Vec<Prop>>)>>;

fn prop_cache() -> &'static PropCache {
    static C: OnceLock<PropCache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What an empty box proposes, best first, for this user: up to `n`.
pub(super) fn proposals(sup: &Supervisor, user: &str, repo: &str, field: &str, agent: Option<&Agent>, n: usize) -> (Vec<Sugg>, Json) {
    let key = format!("{repo}|{field}|{}", agent.map(|a| a.id.as_str()).unwrap_or(""));
    let cached = prop_cache().lock().unwrap().get(&key).filter(|(at, _)| at.elapsed() < PROPOSALS_FOR).map(|(_, p)| p.clone());
    let props = match cached {
        Some(p) => p,
        None => {
            let p = Arc::new(build_proposals(sup, user, repo, field, agent));
            prop_cache().lock().unwrap().insert(key, (Instant::now(), p.clone()));
            p
        }
    };
    let h = habits(sup, user, repo, field);
    // The user's own corrections are theirs: asked each time, not cached.
    let mut all: Vec<Prop> = props.iter().cloned().collect();
    if field == "steer" {
        if let Some(a) = agent {
            all.extend(said_before(sup, user, repo, a));
        }
    }
    if matches!(field, "task" | "goal") {
        all.extend(again(sup, user, repo, field));
    }
    let mut out: Vec<Sugg> = Vec::new();
    let mut seen = HashSet::new();
    for p in all {
        if !seen.insert(p.text.to_lowercase()) {
            continue;
        }
        let kind = format!("proposal:{}", p.sub);
        let (b, picked) = h.boost(&kind, &p.text);
        let mut s = sugg("proposal", p.prio + b, &p.text, &p.text, p.sub, p.refj.clone(), 0, 0);
        s.json["sub"] = json!(p.sub);
        s.json["because"] = json!(p.because);
        s.json["source"] = json!("chloedb");
        s.json["picked"] = json!(picked);
        if let Some(g) = &p.go {
            s.json["go"] = json!(g);
        }
        out.push(s);
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.truncate(n);
    let context = match agent {
        Some(a) => json!({ "agent": a.name, "id": a.id, "now": doing_now(sup, a) }),
        None => Json::Null,
    };
    (out, context)
}

fn build_proposals(sup: &Supervisor, user: &str, repo: &str, field: &str, agent: Option<&Agent>) -> Vec<Prop> {
    let main = PathBuf::from(repo);
    match field {
        "task" | "goal" => {
            let mut out = failing_tests(sup, repo, None);
            out.extend(open_errors(sup, repo));
            out.extend(next_outcomes(sup, repo));
            let hot = hotspots(sup.db(), repo);
            if let Some((path, (_, why))) = hot.iter().max_by(|a, b| a.1 .0.total_cmp(&b.1 .0)) {
                let done = covering(sup, repo, &main, std::slice::from_ref(path)).map(|c| format!("; done when `{}` passes", c.0)).unwrap_or_default();
                out.push(Prop { sub: "hotspot", text: format!("Add tests around {path} before more agents change it{done}."), because: format!("a hotspot: {why}"), prio: 6.0, go: None, refj: json!({ "path": path }) });
            }
            let _ = user;
            out
        }
        "steer" => match agent {
            Some(a) => steer_proposals(sup, repo, &main, a),
            None => Vec::new(),
        },
        "trace" | "quick" | "why" => today_questions(sup, repo, &main, field),
        _ => Vec::new(),
    }
}

/// Tests failing or flaky in the last day (of one agent, or of all).
fn failing_tests(sup: &Supervisor, repo: &str, agent: Option<&str>) -> Vec<Prop> {
    let since = now_ms() - 24 * 3600 * 1000;
    let p = Params::new().set("repo", sv(repo)).set("since", crate::db::ts(since)).set("a", agent.map(sv).unwrap_or(Value::Null));
    let sql = if agent.is_some() {
        "SELECT test, outcome, ts, command, agent FROM test_runs WHERE repo = :repo AND agent = :a AND ts >= :since ORDER BY ts DESC LIMIT 5000"
    } else {
        "SELECT test, outcome, ts, command, agent FROM test_runs WHERE repo = :repo AND ts >= :since ORDER BY ts DESC LIMIT 5000"
    };
    // test -> (runs, fails, last outcome, last fail ms, first ms, command, agent of the last fail)
    let mut by: Vec<(String, (u32, u32, String, i64, i64, String, Option<String>))> = Vec::new();
    for r in q(sup, sql, &p).unwrap_or_default() {
        let Some(t) = get_str(&r[0]) else { continue };
        let outcome = get_str(&r[1]).unwrap_or_default();
        let ts = get_ms(&r[2]).unwrap_or(0);
        let i = by.iter().position(|x| x.0 == t).unwrap_or_else(|| {
            by.push((t.clone(), (0, 0, outcome.clone(), 0, ts, get_str(&r[3]).unwrap_or_default(), None)));
            by.len() - 1
        });
        let e = &mut by[i].1;
        e.0 += 1;
        e.4 = e.4.min(ts);
        if outcome == "failed" {
            e.1 += 1;
            if e.3 == 0 {
                e.3 = ts;
                e.6 = get_str(&r[4]);
            }
        }
    }
    by.retain(|x| x.1 .1 > 0);
    by.sort_by_key(|x| (x.1 .2 != "failed", std::cmp::Reverse(x.1 .1), std::cmp::Reverse(x.1 .3)));
    by.into_iter()
        .take(2)
        .map(|(t, (n, f, last, fail_ms, first_ms, cmd, who))| {
            let short = test_name(&t);
            let c = test_cmd(&cmd, t.rsplit(" › ").next().unwrap_or(&t));
            let by = who.and_then(|a| sup.agent_row(&a)).map(|a| format!(" ({})", a.name)).unwrap_or_default();
            if f < n && last != "failed" || f < n && n >= 3 {
                Prop {
                    sub: "test",
                    text: format!("Fix {short}: flaky, failed {f} of {n} runs in the last {}; done when `{c}` passes 5 times in a row.", span(now_ms() - first_ms)),
                    because: format!("test_runs: last failed {}{by}", super::ago(fail_ms)),
                    prio: 10.0 + f as f64 * 0.3,
                    go: None,
                    refj: json!({ "test": t, "runs": n, "failed": f, "command": c }),
                }
            } else {
                Prop {
                    sub: "test",
                    text: format!("Fix {short}: it started failing {}; done when `{c}` passes.", super::ago(first_ms)),
                    because: format!("test_runs: failed {f} of {n} runs{by}"),
                    prio: 11.0,
                    go: None,
                    refj: json!({ "test": t, "runs": n, "failed": f, "command": c }),
                }
            }
        })
        .collect()
}

fn open_errors(sup: &Supervisor, repo: &str) -> Vec<Prop> {
    let rows = q(sup, "SELECT message, file, line, hits, first_ms, agent FROM runtime_errors WHERE repo = :repo AND state = 'open' ORDER BY hits DESC LIMIT 2", &Params::new().set("repo", sv(repo))).unwrap_or_default();
    rows.iter()
        .map(|r| {
            let msg = one_line(&get_str(&r[0]).unwrap_or_default(), 80);
            let at = match (get_str(&r[1]), get_int(&r[2])) {
                (Some(f), Some(l)) if !f.is_empty() => format!(" at {f}:{l}"),
                (Some(f), _) if !f.is_empty() => format!(" at {f}"),
                _ => String::new(),
            };
            let hits = get_int(&r[3]).unwrap_or(1);
            let by = get_str(&r[5]).and_then(|a| sup.agent_row(&a)).map(|a| format!(", in {}'s preview", a.name)).unwrap_or_default();
            Prop {
                sub: "error",
                text: format!("Fix “{msg}”{at}; done when the preview loads without it."),
                because: format!("runtime error: {hits} hit{} since {}{by}", if hits == 1 { "" } else { "s" }, get_int(&r[4]).map(super::ago).unwrap_or_default()),
                prio: 9.0 + (hits as f64).ln().min(3.0),
                go: None,
                refj: json!({ "message": msg }),
            }
        })
        .collect()
}

fn next_outcomes(sup: &Supervisor, repo: &str) -> Vec<Prop> {
    let goals = q(sup, "SELECT id, text, state FROM goals WHERE repo = :repo LIMIT 50", &Params::new().set("repo", sv(repo))).unwrap_or_default();
    let mut out = Vec::new();
    for g in goals.iter().filter(|g| !matches!(get_str(&g[2]).as_deref(), Some("done" | "dropped"))) {
        let gid = get_str(&g[0]).unwrap_or_default();
        let rows = q(sup, "SELECT title, detail, state, agent, ord FROM outcomes WHERE goal = :g ORDER BY ord ASC LIMIT 100", &Params::new().set("g", sv(&gid))).unwrap_or_default();
        if let Some(o) = rows.iter().find(|o| !matches!(get_str(&o[2]).as_deref(), Some("done" | "dropped" | "running" | "working")) && get_str(&o[3]).filter(|a| !a.is_empty()).is_none()) {
            let title = get_str(&o[0]).unwrap_or_default();
            let detail = get_str(&o[1]).filter(|d| !d.trim().is_empty()).map(|d| format!(": {}", one_line(&d, 120))).unwrap_or_default();
            out.push(Prop {
                sub: "outcome",
                text: format!("{title}{detail}"),
                because: format!("the next step of “{}”, not started", one_line(&get_str(&g[1]).unwrap_or_default(), 60)),
                prio: 8.0,
                go: None,
                refj: json!({ "goal": gid }),
            });
            if out.len() >= 2 {
                break;
            }
        }
    }
    out
}

/// The user's own recent task, offered again when it did not finish.
fn again(sup: &Supervisor, user: &str, repo: &str, field: &str) -> Vec<Prop> {
    let rows = q(sup, "SELECT text, ts FROM complete_events WHERE \"user\" = :u AND repo = :r AND field = :f AND kind = 'typed' ORDER BY ts DESC LIMIT 20", &Params::new().set("u", sv(user)).set("r", sv(repo)).set("f", sv(field))).unwrap_or_default();
    let mut n: HashMap<String, (u32, i64, String)> = HashMap::new();
    for r in &rows {
        let t = get_str(&r[0]).unwrap_or_default();
        let e = n.entry(t.trim().to_lowercase()).or_insert((0, get_ms(&r[1]).unwrap_or(0), t.clone()));
        e.0 += 1;
    }
    n.into_values()
        .max_by_key(|e| (e.0, e.1))
        .map(|(k, ts, t)| Prop { sub: "mine", text: t, because: if k > 1 { format!("you wrote this {k} times, last {}", super::ago(ts)) } else { format!("you wrote this {}", super::ago(ts)) }, prio: 4.0 + k as f64 * 0.5, go: None, refj: Json::Null })
        .into_iter()
        .collect()
}

fn steer_proposals(sup: &Supervisor, repo: &str, main: &Path, a: &Agent) -> Vec<Prop> {
    let mut out = Vec::new();
    let files = agent_files(sup, a);
    // A test it broke.
    if let Some(p) = failing_tests(sup, repo, Some(&a.id)).into_iter().next() {
        let short = test_name(p.refj["test"].as_str().unwrap_or(""));
        let cmd = p.refj["command"].as_str().unwrap_or("").to_string();
        let edit = files.first().map(|f| format!(" since your edit of {f}")).unwrap_or_default();
        out.push(Prop { sub: "test", text: format!("{short} fails{edit}: fix it before you go on, and run `{cmd}`."), because: p.because.clone(), prio: 10.0, go: None, refj: p.refj.clone() });
    }
    // The ◆ rule or decision about what it is changing.
    let recs = mem().lock().unwrap().get(repo).map(|e| e.recs.clone()).unwrap_or_default();
    let task_stems: HashSet<String> = words(a.task.as_deref().unwrap_or("")).into_iter().map(|w| stem(&w.0)).collect();
    let best = recs
        .iter()
        .filter(|r| !r.superseded)
        .map(|r| {
            let on_file = r.files.iter().any(|f| files.iter().any(|g| g == f || g.ends_with(&format!("/{f}"))));
            let shared = words(&r.text).iter().filter(|w| task_stems.contains(&stem(&w.0))).count();
            (r, on_file as usize * 3 + shared + (r.kind == "rule") as usize)
        })
        .filter(|(_, s)| *s >= 2)
        .max_by_key(|(r, s)| (*s, r.ts_ms.unwrap_or(0)));
    if let Some((r, _)) = best {
        let mut why = match r.files.iter().find(|f| files.iter().any(|g| g.ends_with(f.as_str()))) {
            Some(f) => vec![format!("{} is editing {f}", a.name)],
            None => vec![format!("about {}'s task", a.name)],
        };
        if let Some(d) = r.ts_ms.map(short_date) {
            why.push(if r.kind == "rule" { format!("your rule since {d}") } else { format!("◆ decided {d}{}", r.by.as_deref().map(|b| format!(" by {b}")).unwrap_or_default()) });
        }
        out.push(Prop { sub: "rule", text: format!("Keep to ◆{}: {}", r.id, r.text), because: why.join(", "), prio: 8.0, go: None, refj: r.to_json() });
    }
    // How it shows it is done.
    if let Some((cmd, why)) = covering(sup, repo, main, &files) {
        out.push(Prop { sub: "done", text: format!("When you are done, run `{cmd}` and show me the output."), because: why, prio: 5.0, go: None, refj: json!({ "command": cmd }) });
    }
    out
}

/// What this user said to an agent in a spot like this one before.
fn said_before(sup: &Supervisor, user: &str, repo: &str, a: &Agent) -> Vec<Prop> {
    let files = agent_files(sup, a);
    let mut qtext: Vec<String> = words(a.task.as_deref().unwrap_or("")).into_iter().map(|w| w.0).collect();
    qtext.extend(words(a.last_event.as_deref().unwrap_or("")).into_iter().map(|w| w.0));
    for f in files.iter().take(5) {
        qtext.push(f.rsplit('/').next().unwrap_or(f).to_lowercase());
    }
    qtext.retain(|w| w.len() >= 3);
    if qtext.is_empty() {
        return Vec::new();
    }
    let p = Params::new().set("u", sv(user)).set("r", sv(repo)).set("q", sv(&qtext.join(" ")));
    let rows = q(sup, "SELECT text, ts, agent, ctx, score() FROM complete_events WHERE \"user\" = :u AND repo = :r AND field = 'steer' AND kind = 'typed' ORDER BY bm25(text, :q, syntax => 'plain') DESC LIMIT 5", &p).unwrap_or_default();
    let mut out = Vec::new();
    for r in rows.iter().filter(|r| float(&r[4]) > 0.0).take(1) {
        let t = get_str(&r[0]).unwrap_or_default();
        let who = get_str(&r[2]).filter(|x| x != &a.id).and_then(|x| sup.agent_row(&x)).map(|x| format!(" to {}", x.name)).unwrap_or_default();
        let ctx = get_str(&r[3]).filter(|c| !c.is_empty()).map(|c| format!(", when it was at “{}”", one_line(c.split_once(": ").map(|x| x.1).unwrap_or(&c), 40))).unwrap_or_default();
        out.push(Prop { sub: "said", text: t, because: format!("you said this{who} {}{ctx}", get_ms(&r[1]).map(super::ago).unwrap_or_default()), prio: 9.0 + float(&r[4]).min(4.0) * 0.25, go: None, refj: Json::Null });
    }
    out
}

/// Questions about what changed today, for Trace and the quick search.
fn today_questions(sup: &Supervisor, repo: &str, main: &Path, field: &str) -> Vec<Prop> {
    let mut out = Vec::new();
    let go = |q: &str| (field == "quick").then(|| format!("#/trace?q={}", urlencode(q)));
    // The files agents changed most today.
    let rows = q(sup, "SELECT file, agent, ts FROM file_edits WHERE repo = :repo AND ts >= :since ORDER BY ts DESC LIMIT 5000", &Params::new().set("repo", sv(repo)).set("since", crate::db::ts(midnight_ms()))).unwrap_or_default();
    let mut by: Vec<(String, u32, Vec<String>, i64)> = Vec::new();
    for r in &rows {
        let (Some(f), a) = (get_str(&r[0]), get_str(&r[1]).and_then(|a| sup.agent_row(&a)).map(|a| a.name)) else { continue };
        let i = by.iter().position(|x| x.0 == f).unwrap_or_else(|| {
            by.push((f.clone(), 0, Vec::new(), get_ms(&r[2]).unwrap_or(0)));
            by.len() - 1
        });
        by[i].1 += 1;
        if let Some(a) = a {
            if !by[i].2.contains(&a) {
                by[i].2.push(a);
            }
        }
    }
    by.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (f, n, who, last) in by.into_iter().take(2) {
        let names = if who.is_empty() { "an agent".to_string() } else { who.join(" and ") };
        let qn = format!("Why did {names} change {f} today?");
        out.push(Prop { sub: "question", go: go(&qn), text: qn, because: format!("{n} edit{} today, last {}", if n == 1 { "" } else { "s" }, super::ago(last)), prio: 8.0 + (n as f64).ln(), refj: json!({ "file": f }) });
    }
    // Today's commits, every branch.
    let log = Command::new("git").arg("-C").arg(main).args(["log", "--all", "--since=midnight", "-n", "5", "--format=%h%x1f%an%x1f%s%x1f%ct"]).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).output();
    if let Ok(o) = log {
        for line in String::from_utf8_lossy(&o.stdout).lines().take(2) {
            let parts: Vec<&str> = line.split('\u{1f}').collect();
            if parts.len() < 4 {
                continue;
            }
            let qn = format!("Why “{}”, and what did it change?", one_line(parts[2], 70));
            let when = parts[3].parse::<i64>().map(|s| super::ago(s * 1000)).unwrap_or_default();
            out.push(Prop { sub: "question", go: go(&qn), text: qn, because: format!("commit {} by {}, {when}", parts[0], parts[1]), prio: 7.5, refj: json!({ "sha": parts[0] }) });
        }
    }
    for p in failing_tests(sup, repo, None).into_iter().take(1) {
        let short = test_name(p.refj["test"].as_str().unwrap_or(""));
        let qn = format!("When did {short} start failing, and what changed then?");
        out.push(Prop { sub: "question", go: go(&qn), text: qn, because: p.because, prio: 8.5, refj: p.refj });
    }
    out
}

// ------------------------------------------------------------- ghost

/// The rest of the sentence after the cursor, from what this box knows:
/// a text the user (or an agent's task, or a proposal) already began this
/// way, else the word being typed, the ◆ rule that applies, and a done
/// check from the tests that cover the files it names.
#[allow(clippy::too_many_arguments)]
pub fn ghost(sup: &Supervisor, user: &str, repo: &str, field: &str, text: &str, tok: &Token, agent: Option<&Agent>, top: Option<&Json>) -> Option<Json> {
    if text.trim().is_empty() || !matches!(field, "task" | "goal" | "steer" | "answer" | "trace" | "quick" | "why" | "persona" | "change") || repo.is_empty() {
        return None;
    }
    if let Some(g) = continued(sup, user, repo, field, text, agent) {
        return Some(g);
    }
    if !matches!(field, "task" | "goal" | "steer") {
        return None;
    }
    let mut parts: Vec<Json> = Vec::new();
    let mut ghost = String::new();
    let mut because: Vec<String> = Vec::new();
    // The word being typed.
    let mid_word = text.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '/');
    if mid_word {
        let t = top.filter(|s| matches!(s["kind"].as_str(), Some("file" | "symbol" | "agent" | "test" | "decision" | "rule" | "preference" | "fact")))?;
        let ins = t["insert"].as_str().unwrap_or("").trim_end();
        let tl = tok.text.to_lowercase();
        if !ins.to_lowercase().starts_with(&tl) || ins.len() <= tok.text.len() || !ins.is_char_boundary(tok.text.len()) {
            return None;
        }
        let rest = &ins[tok.text.len()..];
        ghost.push_str(rest);
        parts.push(json!({ "kind": "word", "text": rest }));
    }
    let full = format!("{text}{ghost}");
    let words_n = words(&full).len();
    if words_n < 3 {
        return (!ghost.is_empty()).then(|| json!({ "text": ghost, "parts": parts, "because": "completes the word", "source": "chloedb" }));
    }
    let mut files = files_named(repo, &full);
    if field == "steer" {
        if let Some(a) = agent {
            for f in agent_files(sup, a).into_iter().take(3) {
                if !files.contains(&f) {
                    files.push(f);
                }
            }
        }
    }
    // The ◆ rule or decision that applies.
    let recs = mem().lock().unwrap().get(repo).map(|e| e.recs.clone()).unwrap_or_default();
    let full_l = full.to_lowercase();
    let stems: HashSet<String> = words(&full).into_iter().map(|w| stem(&w.0)).collect();
    let tchoices = choices(&full_l);
    let rule = recs
        .iter()
        .filter(|r| !r.superseded && !full.contains(&format!("◆{}", r.id)) && matches!(r.kind.as_str(), "rule" | "decision"))
        .filter(|r| {
            // Not one the text already goes against (the check says that).
            let rc = choices(&r.text.to_lowercase());
            !rc.iter().any(|(g, rpos, _, _)| tchoices.iter().any(|(tg, tpos, _, _)| tg == g && !tpos.is_empty() && !rpos.is_empty() && tpos.is_disjoint(rpos)))
        })
        .map(|r| {
            let on_file = r.files.iter().any(|f| files.iter().any(|g| g == f || g.ends_with(&format!("/{f}"))));
            let shared = words(&r.text).iter().filter(|w| stems.contains(&stem(&w.0))).count();
            (r, on_file as usize * 2 + shared)
        })
        .filter(|(_, s)| *s >= 2)
        .max_by_key(|(r, s)| (*s, r.kind == "rule", r.ts_ms.unwrap_or(0)))
        .map(|(r, _)| r.clone());
    let done = if matches!(field, "task" | "goal") && !has_done(&full) {
        let main = PathBuf::from(repo);
        covering(sup, repo, &main, &files).or_else(|| {
            let refs: Vec<(usize, usize, String)> = files.iter().map(|f| (0, 0, f.clone())).collect();
            done_command(sup, repo, &main, &refs).map(|c| (c, "the command the tests ran with here".to_string()))
        })
    } else {
        None
    };
    if rule.is_none() && done.is_none() {
        return (!ghost.is_empty()).then(|| json!({ "text": ghost, "parts": parts, "because": "completes the word", "source": "chloedb" }));
    }
    let end = full.trim_end();
    let sentence_over = end.ends_with(['.', '!', '?']);
    let spaced = full.ends_with(char::is_whitespace);
    let mut add = String::new();
    if let Some(r) = &rule {
        let clause = if sentence_over { format!(" Keep to ◆{}.", r.id) } else if spaced { format!("per ◆{}", r.id) } else { format!(", per ◆{}", r.id) };
        add.push_str(&clause);
        parts.push(json!({ "kind": "rule", "text": clause, "ref": r.to_json() }));
        because.push(format!("◆ {} applies", if r.kind == "rule" { "your rule" } else { "a decision" }));
    }
    if let Some((cmd, why)) = &done {
        let clause = if sentence_over || add.ends_with('.') {
            format!(" Done when `{cmd}` passes.")
        } else if spaced && add.is_empty() {
            format!("so that `{cmd}` passes.")
        } else {
            format!("; done when `{cmd}` passes.")
        };
        add.push_str(&clause);
        parts.push(json!({ "kind": "done", "text": clause, "command": cmd }));
        because.push(format!("done check: {why}"));
    } else if rule.is_some() && !sentence_over {
        add.push('.');
    }
    ghost.push_str(&add);
    Some(json!({ "text": ghost, "parts": parts, "because": because.join(" · "), "source": "chloedb" }))
}

/// A text that begins as this one does: the user's own, a proposal, an
/// agent's task, a prompt ChloeCtx kept. The rest of it is the ghost.
fn continued(sup: &Supervisor, user: &str, repo: &str, field: &str, text: &str, agent: Option<&Agent>) -> Option<Json> {
    let tl = text.to_lowercase();
    if tl.len() != text.len() || text.trim().chars().count() < 3 {
        return None;
    }
    let (qs, _) = terms_of(text);
    let mut cands: Vec<(f64, String, String)> = Vec::new();
    // Proposals of this box.
    let (props, _) = proposals(sup, user, repo, field, agent, 6);
    for p in &props {
        cands.push((3.0, p.json["insert"].as_str().unwrap_or("").to_string(), p.json["because"].as_str().unwrap_or("").to_string()));
    }
    if !qs.is_empty() {
        let p = Params::new().set("u", sv(user)).set("r", sv(repo)).set("f", sv(field)).set("q", sv(&qs));
        for r in q(sup, "SELECT text, ts, score() FROM complete_events WHERE \"user\" = :u AND repo = :r AND field = :f AND kind = 'typed' ORDER BY bm25(text, :q) DESC LIMIT 10", &p).unwrap_or_default() {
            if float(&r[2]) > 0.0 {
                cands.push((4.0 + float(&r[2]).min(5.0) * 0.1, get_str(&r[0]).unwrap_or_default(), format!("you wrote this {}", get_ms(&r[1]).map(super::ago).unwrap_or_default())));
            }
        }
        if matches!(field, "task" | "goal") {
            for r in q(sup, "SELECT id, score() FROM agents WHERE repo = :r ORDER BY bm25(task, :q) DESC LIMIT 6", &p).unwrap_or_default() {
                if let Some(a) = get_str(&r[0]).and_then(|id| sup.agent_row(&id)).filter(|_| float(&r[1]) > 0.0) {
                    let (_, how) = outcome_of(sup, &a);
                    cands.push((2.0, a.task.clone().unwrap_or_default(), format!("{}'s task, {}: {how}", a.name, super::ago(a.created))));
                }
            }
        }
        for r in q(sup, "SELECT ins, refj, score() FROM complete_items WHERE repo = :r AND kind = 'prompt' ORDER BY bm25(body, :q) DESC LIMIT 6", &p).unwrap_or_default() {
            if float(&r[2]) > 0.0 {
                cands.push((1.5, get_str(&r[0]).unwrap_or_default(), "you asked Claude this (ChloeCtx)".into()));
            }
        }
    }
    cands
        .into_iter()
        .filter(|(_, c, _)| c.len() > text.len() + 1 && c.to_lowercase().starts_with(&tl) && c.is_char_boundary(text.len()))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c, why)| {
            let rest = c[text.len()..].to_string();
            json!({ "text": rest, "parts": [{ "kind": "continued", "text": rest }], "because": why, "source": "chloedb" })
        })
}

// ------------------------------------------------------------- the model ghost

#[derive(Deserialize)]
pub struct GhostB {
    pub text: Option<String>,
    pub field: Option<String>,
    pub repo: Option<String>,
    pub agent: Option<String>,
    pub user: Option<String>,
    /// The deterministic ghost the box shows now.
    pub ghost: Option<String>,
}

fn claude_found(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

type GhostCache = Mutex<HashMap<String, String>>;

fn ghost_cache() -> &'static GhostCache {
    static C: OnceLock<GhostCache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A better ghost from a fast model, when the user pauses. Off when the
/// claude binary is missing. Cached by text, and reused while the user
/// types what it said.
pub fn model_ghost(sup: &Arc<Supervisor>, b: GhostB) -> Result<Json, ApiError> {
    let started = Instant::now();
    let text = b.text.unwrap_or_default();
    let field = b.field.filter(|f| !f.is_empty()).unwrap_or_else(|| "task".into());
    let repo = repo_for(sup, b.repo.as_deref(), b.agent.as_deref())?;
    let bin = sup.cfg.claude_bin.clone();
    if !claude_found(&bin) {
        return Ok(json!({ "available": false, "why": format!("{bin} is not found: the model ghost is off, the ChloeDB one stays") }));
    }
    if text.trim().chars().count() < 6 || repo.is_empty() {
        return Ok(json!({ "available": true, "ghost": null, "why": "too short to guess" }));
    }
    let key = format!("{repo}|{field}|");
    // The cache: this text, or an earlier one whose answer the user is typing.
    {
        let c = ghost_cache().lock().unwrap();
        for (k, g) in c.iter() {
            if let Some(t0) = k.strip_prefix(&key) {
                if text.starts_with(t0) && text.len() <= t0.len() + g.len() {
                    let typed_more = &text[t0.len()..];
                    if g.starts_with(typed_more) && g.len() > typed_more.len() && g.is_char_boundary(typed_more.len()) {
                        let rest = g[typed_more.len()..].to_string();
                        return Ok(json!({ "available": true, "cached": true, "took_ms": 0, "ghost": { "text": rest, "source": "model", "model": MODEL, "checked": true, "because": "a fast model, from this box's context; every name in it is in the code or the memory" } }));
                    }
                }
            }
        }
    }
    let agent = b.agent.as_deref().filter(|a| !a.is_empty()).and_then(|a| sup.agent_row(a));
    let user = user_of(b.user.as_deref());
    super::fresh(sup, &repo);
    let prompt = pack(sup, &user, &repo, &field, &text, agent.as_ref(), b.ghost.as_deref());
    let system = "You complete what a developer is typing into a box of Workbench, an IDE where they direct coding agents. Answer with only the characters that come after their cursor: no quotes, no explanation, one line, at most 30 words. Begin with a space when a new word begins. Keep their style and words. Name files, functions, ◆ records, tests and services only from the context, which is this repository's real code and memory: never invent a component it does not show (a payment processor, a bank, a queue, a service). When you do not know, complete with fewer words. A task that changes code ends with a done check: a command in backticks that passes.";
    let args: Vec<String> = ["-p", "--output-format", "json", "--tools", "", "--strict-mcp-config", "--settings", "{\"disableAllHooks\":true}", "--no-session-persistence", "--model", MODEL, "--system-prompt", system]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(prompt.clone()))
        .collect();
    let out = run_for(&bin, &args, Path::new(&repo), MODEL_TIMEOUT);
    let raw = match out {
        Ok(o) => o,
        Err(e) => return Ok(json!({ "available": true, "ghost": null, "why": e, "took_ms": started.elapsed().as_millis() as u64 })),
    };
    let res = raw.lines().rev().find_map(|l| serde_json::from_str::<Json>(l.trim()).ok().filter(|v| v.get("result").is_some())).or_else(|| serde_json::from_str::<Json>(raw.trim()).ok());
    let Some(mut g) = res.and_then(|v| v["result"].as_str().map(str::to_string)).filter(|g| !g.trim().is_empty()) else {
        return Ok(json!({ "available": true, "ghost": null, "why": "the model answered nothing", "took_ms": started.elapsed().as_millis() as u64 }));
    };
    g = g.lines().next().unwrap_or("").trim_end().trim_matches('"').to_string();
    if g.to_lowercase().starts_with(&text.to_lowercase()) && g.is_char_boundary(text.len()) {
        g = g[text.len()..].to_string();
    }
    // After a space typed, no second one.
    if text.ends_with(char::is_whitespace) {
        g = g.trim_start().to_string();
    }
    let g: String = g.chars().take(240).collect();
    // Grounded, or dropped (SPEC.md, "Sixth build: E"): every file,
    // function and thing it names is in the code, the memory or the context.
    let (files, recs) = mem().lock().unwrap().get(&repo).map(|e| (e.files.clone(), e.recs.clone())).unwrap_or_default();
    let known = format!("{text}\n{prompt}\n{}", recs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("\n"));
    let unknown = ungrounded(&g, &known, &files, &mut |w| in_code(&repo, w));
    if !unknown.is_empty() {
        let names = unknown.iter().take(4).map(|w| format!("“{w}”")).collect::<Vec<_>>().join(", ");
        return Ok(json!({
            "available": true, "cached": false, "ghost": null, "took_ms": started.elapsed().as_millis() as u64,
            "dropped": { "text": g, "names": unknown },
            "why": format!("the model's ghost named {names}, which this repository's code and memory do not have: it was dropped"),
        }));
    }
    {
        let mut c = ghost_cache().lock().unwrap();
        if c.len() > 500 {
            c.clear();
        }
        c.insert(format!("{key}{text}"), g.clone());
    }
    Ok(json!({ "available": true, "cached": false, "took_ms": started.elapsed().as_millis() as u64, "ghost": { "text": g, "source": "model", "model": MODEL, "checked": true, "because": "a fast model, from this box's context; every name in it is in the code or the memory" } }))
}

/// Words a ghost may use without the repository having them: the language
/// of tasks.
const GENERIC: &[&str] = &[
    "about", "above", "accept", "across", "actual", "add", "after", "again", "against", "agent", "all", "allow", "already", "also", "always", "amount", "answer", "any", "app",
    "apply", "around", "ask", "back", "bad", "based", "before", "begin", "behaviour", "behavior", "being", "below", "best", "better", "between", "both", "break", "broken", "bug", "build",
    "bullet", "call", "case", "cause", "change", "check", "clean", "clear", "close", "code", "come", "command", "comment", "commit", "compare", "complete", "correct", "could", "count",
    "cover", "current", "data", "default", "describe", "detail", "different", "does", "done", "down", "each", "edge", "edit", "else", "empty", "end", "ensure", "enough", "error", "even",
    "every", "example", "exist", "expect", "explain", "fail", "false", "fast", "few", "field", "file", "find", "first", "fix", "flow", "follow", "found", "from", "function", "get",
    "give", "goes", "good", "handle", "happen", "have", "here", "how", "idea", "input", "inside", "instead", "into", "issue", "item", "just", "keep", "kind", "know", "last", "later",
    "leave", "less", "line", "list", "little", "long", "look", "make", "many", "match", "mean", "method", "might", "missing", "more", "most", "move", "much", "must", "name", "need",
    "never", "new", "next", "none", "note", "nothing", "now", "number", "old", "once", "only", "other", "output", "over", "own", "part", "pass", "path", "place", "point", "print",
    "problem", "project", "put", "read", "reason", "record", "remove", "rename", "repo", "report", "result", "return", "right", "run", "same", "say", "see", "seem", "send", "set",
    "should", "show", "side", "simple", "since", "small", "some", "start", "state", "step", "still", "stop", "sure", "take", "task", "tell", "test", "text", "than", "that", "their",
    "them", "then", "there", "these", "thing", "this", "those", "through", "time", "today", "together", "top", "true", "try", "turn", "under", "until", "update", "use", "user", "using",
    "value", "very", "want", "way", "well", "what", "when", "where", "which", "while", "who", "whole", "why", "will", "with", "without", "work", "would", "write", "wrong",
    "yes", "you", "your", "goes", "comes", "rather", "summary", "points",
];

/// The names in a model's ghost that nothing backs: a file not among the
/// repository's, an identifier the code does not hold, a word that is in
/// neither the code (`in_code`), nor `known` (what was typed, the context
/// pack, the memory's records), nor the language of tasks.
pub fn ungrounded(ghost: &str, known: &str, files: &[String], in_code: &mut dyn FnMut(&str) -> bool) -> Vec<String> {
    let known_stems: HashSet<String> = known.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(|w| stem(&w.to_lowercase())).collect();
    let mut out: Vec<String> = Vec::new();
    for raw in ghost.split_whitespace() {
        let tok = raw.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\'' | '`' | '“' | '”' | '‘' | '’' | '*'));
        let tok = tok.strip_suffix("'s").or_else(|| tok.strip_suffix("’s")).unwrap_or(tok);
        if tok.chars().count() < 3 || tok.starts_with('◆') || tok.starts_with('@') || tok.chars().all(|c| !c.is_alphabetic()) {
            continue;
        }
        let ext = tok.rsplit_once('.').filter(|(a, e)| !a.is_empty() && (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()) && e.chars().any(|c| c.is_ascii_alphabetic()));
        let ok = if tok.contains('/') || ext.is_some() {
            files.iter().any(|f| f == tok || f.ends_with(&format!("/{tok}")) || f.starts_with(&format!("{}/", tok.trim_end_matches('/')))) || known.contains(tok)
        } else if tok.contains('_') || tok.ends_with("()") || tok.chars().zip(tok.chars().skip(1)).any(|(a, b)| a.is_lowercase() && b.is_uppercase()) {
            let id = tok.trim_end_matches("()");
            known.contains(id) || in_code(id)
        } else {
            let w = tok.to_lowercase();
            let s = stem(&w);
            w.chars().count() < 4 || STOP.contains(&w.as_str()) || GENERIC.contains(&w.as_str()) || GENERIC.contains(&s.as_str()) || known_stems.contains(&s) || known_stems.contains(&w) || in_code(&w)
        };
        if !ok && !out.contains(&tok.to_string()) {
            out.push(tok.to_string());
        }
    }
    out
}

type CodeCache = Mutex<HashMap<String, bool>>;

fn code_cache() -> &'static CodeCache {
    static C: OnceLock<CodeCache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether the repository's tracked files hold a word (as a whole word,
/// or a form of it: "settles" for "settle"), case aside: `git grep`.
fn in_code(repo: &str, word: &str) -> bool {
    let w: String = word.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
    if w.is_empty() {
        return false;
    }
    let key = format!("{repo}\u{1}{}", w.to_lowercase());
    if let Some(v) = code_cache().lock().unwrap().get(&key) {
        return *v;
    }
    let s = stem(&w.to_lowercase());
    let forms = [w.clone(), s.clone(), format!("{s}e"), format!("{s}s"), format!("{s}es"), format!("{s}ed"), format!("{s}ing")];
    let re = format!("({})", forms.join("|"));
    let hit = Command::new("git")
        .args(["-C", repo, "grep", "-q", "-I", "-i", "-w", "-E", "-e", &re, "--"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let mut c = code_cache().lock().unwrap();
    if c.len() > 5000 {
        c.clear();
    }
    c.insert(key, hit);
    hit
}

/// The context a model ghost is given: a pack of what the box knows.
fn pack(sup: &Supervisor, user: &str, repo: &str, field: &str, text: &str, agent: Option<&Agent>, ghost: Option<&str>) -> String {
    let mut lines = vec![format!("Box: {field} in {}", Path::new(repo).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())];
    if let Some(a) = agent {
        lines.push(format!("Agent {}: task “{}”; {}", a.name, one_line(a.task.as_deref().unwrap_or(""), 160), doing_now(sup, a)));
        let files = agent_files(sup, a);
        if !files.is_empty() {
            lines.push(format!("Its files: {}", files.iter().take(6).cloned().collect::<Vec<_>>().join(", ")));
        }
    }
    let files = files_named(repo, text);
    if let Some((cmd, why)) = covering(sup, repo, Path::new(repo), &files) {
        lines.push(format!("Done check: `{cmd}` ({why})"));
    }
    let recs = mem().lock().unwrap().get(repo).map(|e| e.recs.clone()).unwrap_or_default();
    let stems: HashSet<String> = words(text).into_iter().map(|w| stem(&w.0)).collect();
    let mut near: Vec<&Rec> = recs.iter().filter(|r| !r.superseded && words(&r.text).iter().any(|w| stems.contains(&stem(&w.0)))).collect();
    near.truncate(4);
    for r in near {
        lines.push(format!("◆{} ({}): {}", r.id, r.kind, one_line(&r.text, 140)));
    }
    for p in failing_tests(sup, repo, agent.map(|a| a.id.as_str())).into_iter().take(1) {
        lines.push(format!("Failing: {} ({})", p.refj["test"].as_str().unwrap_or(""), p.because));
    }
    let mine = q(sup, "SELECT text FROM complete_events WHERE \"user\" = :u AND repo = :r AND field = :f AND kind = 'typed' ORDER BY ts DESC LIMIT 3", &Params::new().set("u", sv(user)).set("r", sv(repo)).set("f", sv(field))).unwrap_or_default();
    for r in mine {
        lines.push(format!("They wrote before: “{}”", one_line(&get_str(&r[0]).unwrap_or_default(), 160)));
    }
    if let Some(g) = ghost.filter(|g| !g.is_empty()) {
        lines.push(format!("The box suggests now: “{g}”"));
    }
    // The repository's real code: its files, and the code nearest the text
    // (code search's chunks, when it is indexed).
    let files = mem().lock().unwrap().get(repo).map(|e| e.files.clone()).unwrap_or_default();
    if !files.is_empty() {
        lines.push(format!("Files ({}): {}", files.len(), files.iter().take(60).cloned().collect::<Vec<_>>().join(", ")));
    }
    let qtext: String = words(text).iter().map(|w| w.0.clone()).collect::<Vec<_>>().join(" ");
    if !qtext.is_empty() {
        let rows = q(sup, "SELECT path, start_line, symbol, text FROM code_chunks WHERE repo = :r AND tree = 'main' AND MATCH(text, :q, min => 1) ORDER BY bm25(text, :q) DESC LIMIT 4", &Params::new().set("r", sv(repo)).set("q", sv(&qtext))).unwrap_or_default();
        for r in rows {
            let body: String = get_str(&r[3]).unwrap_or_default().lines().take(6).collect::<Vec<_>>().join(" ⏎ ");
            lines.push(format!("Code {}:{} {}: {}", get_str(&r[0]).unwrap_or_default(), get_int(&r[1]).unwrap_or(0), get_str(&r[2]).unwrap_or_default(), one_line(&body, 300)));
        }
    }
    lines.push(format!("They have typed: “{text}”"));
    lines.push("Continue it.".into());
    lines.join("\n")
}

/// Runs a program with a deadline: its stdout, or why not.
fn run_for(program: &str, args: &[String], cwd: &Path, timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    let mut child = Command::new(program).args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("cannot run {program}: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let end = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(30)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the model did not answer in {} s", timeout.as_secs()));
            }
        }
    }
    reader.join().map_err(|_| "the model's output was lost".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_users_and_done_words() {
        assert_eq!(user_of(Some(" ana;drop ")), "anadrop");
        assert_eq!(user_of(None), "you");
        assert!(short_date(1_762_700_000_000).len() >= 5);
        assert!(has_done("Fix it; done when `pytest` passes."));
        assert!(!has_done("Fix the rounding in money.py"));
        assert_eq!(test_cmd("cargo test", "totals::coupon"), "cargo test coupon");
        assert_eq!(test_cmd("pytest tests", "test_money.py::test_round"), "pytest -k test_round -q");
        assert_eq!(test_name("api › totals::coupon"), "totals::coupon");
        assert_eq!(test_name("cargo test -p calc"), "`cargo test -p calc`");
        // A ghost is grounded in the code and the memory, or its names are told.
        let files = vec!["money.py".to_string(), "cart.py".to_string(), "web/index.html".to_string()];
        let code = ["checkout", "cents", "payment", "tax"];
        let mut in_code = |w: &str| code.contains(&w.to_lowercase().as_str());
        let lee = " the money comes in at checkout and goes to the shop's payment processor, then settles to the shop's bank account";
        assert_eq!(ungrounded(lee, "Read-only: explain how money flows through this shop, from ", &files, &mut in_code), vec!["processor", "settles", "bank", "account"]);
        assert!(ungrounded(" and keep every total in integer cents.", "Fix the rounding in money.py\nPrices are integer cents; totals round down", &files, &mut in_code).is_empty());
        assert_eq!(ungrounded(" in billing.py and money.py", "Fix the rounding", &files, &mut in_code), vec!["billing.py"]);
        assert_eq!(ungrounded(" by calling apply_coupon()", "", &files, &mut in_code), vec!["apply_coupon"]);
        // No claude: the model ghost is off.
        assert!(!claude_found("/nonexistent/claude"));
        assert!(!claude_found("no-such-claude-binary-here"));
    }
}
