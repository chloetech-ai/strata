//! Autoscaling: a cluster's rules (`spec.autoscale`), each a sentence, that
//! change its copies, shards or volumes from how busy and how full its
//! servers are.
//!
//! - **grow**: when the busiest shard's copies are more than `busyAbove`
//!   busy (the CPU they use of their cores) for `forMinutes`, add `add`
//!   copies to every shard, up to `max`;
//! - **shrink**: when they are all less than `busyBelow` busy for
//!   `forMinutes`, remove a copy from every shard, down to `min`;
//! - **volumes**: when the fullest volume is more than `fullAbove` full,
//!   grow every volume by half, or add a shard (the slots even out);
//! - **freeze**: weekly windows (UTC) in which nothing changes, or only
//!   copies are added;
//! - **cooldown**: a change waits `cooldownMinutes` after the one before,
//!   and for the cluster to be ready (every new copy caught up).
//!
//! [`decide`] is the whole of it, called by the operator on the live
//! numbers ([`Mode::On`] changes the spec, [`Mode::Suggest`] puts the
//! change in the status for a person to apply) and by [`replay`] on the
//! load the servers kept of a past week, so that the rules are tried
//! before they run.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

/// What the rules do when one fires.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Nothing: the rules are kept, and can be replayed.
    #[default]
    Off,
    /// The change waits in the status for a person to apply it.
    Suggest,
    /// The operator changes the spec itself.
    On,
}

/// `spec.autoscale`. A rule left out does nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Autoscale {
    pub mode: Mode,
    pub grow: Option<Grow>,
    pub shrink: Option<Shrink>,
    pub volumes: Option<Volumes>,
    pub freeze: Vec<Freeze>,
    /// Minutes after a change before the next one.
    pub cooldown_minutes: u64,
}

impl Default for Autoscale {
    fn default() -> Autoscale {
        Autoscale { mode: Mode::Off, grow: None, shrink: None, volumes: None, freeze: Vec::new(), cooldown_minutes: 20 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Grow {
    /// The share of their cores the busiest shard's copies use, 0 to 1.
    pub busy_above: f64,
    pub for_minutes: u64,
    /// Copies added to every shard at once.
    pub add: usize,
    /// Copies of each shard at most.
    pub max: usize,
}

impl Default for Grow {
    fn default() -> Grow {
        Grow { busy_above: 0.7, for_minutes: 10, add: 1, max: 5 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Shrink {
    pub busy_below: f64,
    pub for_minutes: u64,
    /// Copies of each shard at least.
    pub min: usize,
}

impl Default for Shrink {
    fn default() -> Shrink {
        Shrink { busy_below: 0.35, for_minutes: 60, min: 2 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VolumeAction {
    /// Every volume grows by half (its claim grows in place).
    #[default]
    Grow,
    /// One more shard, which takes its share of the slots.
    AddShard,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Volumes {
    /// The fullest volume's used share, 0 to 1.
    pub full_above: f64,
    pub action: VolumeAction,
}

impl Default for Volumes {
    fn default() -> Volumes {
        Volumes { full_above: 0.8, action: VolumeAction::Grow }
    }
}

/// A weekly window, in UTC: `from` and `to` like `Fri 16:00`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Freeze {
    pub from: String,
    pub to: String,
    /// Copies may still be added in it.
    pub allow_grow: bool,
}

impl Default for Freeze {
    fn default() -> Freeze {
        Freeze { from: "Fri 16:00".into(), to: "Mon 08:00".into(), allow_grow: true }
    }
}

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The minute of the week (Monday 00:00 UTC is 0) of `Fri 16:00`.
fn week_minute(s: &str) -> Result<u64, String> {
    let bad = || format!("a freeze's from and to are a day and a time in UTC, like \"Fri 16:00\", not {s:?}");
    let (day, time) = s.trim().split_once(' ').ok_or_else(bad)?;
    let d = DAYS.iter().position(|x| x.eq_ignore_ascii_case(&day[..day.len().min(3)])).ok_or_else(bad)?;
    let (h, m) = time.trim().split_once(':').ok_or_else(bad)?;
    let (h, m): (u64, u64) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(d as u64 * 1440 + h * 60 + m)
}

/// `ms` as a day of the week and a time, in UTC: `Tue 14:20`.
pub fn when(ms: u64) -> String {
    let t = minute_of_week(ms);
    format!("{} {:02}:{:02}", DAYS[(t / 1440) as usize], t % 1440 / 60, t % 60)
}

/// The minute of the week of `ms` (1970-01-01 was a Thursday).
fn minute_of_week(ms: u64) -> u64 {
    let minutes = ms / 60_000;
    let days = minutes / 1440;
    ((days + 3) % 7) * 1440 + minutes % 1440
}

impl Freeze {
    fn holds(&self, ms: u64) -> bool {
        let (Ok(a), Ok(b)) = (week_minute(&self.from), week_minute(&self.to)) else { return false };
        let t = minute_of_week(ms);
        if a <= b {
            t >= a && t < b
        } else {
            t >= a || t < b
        }
    }
}

/// `spec.autoscale` in the resource definition.
pub fn schema() -> Json {
    let share = |doc: &str| json!({"type": "number", "minimum": 0, "maximum": 1, "description": doc});
    let int = |min: u64, doc: &str| json!({"type": "integer", "minimum": min, "description": doc});
    json!({"type": "object", "description": "Rules that change the copies, shards or volumes from how busy and full the servers are.", "properties": {
        "mode": {"type": "string", "enum": ["off", "suggest", "on"], "description": "off: nothing (the rules can be replayed); suggest: a change waits in status.autoscale for a person; on: the operator makes it (default off)."},
        "grow": {"type": "object", "description": "Add copies when the busiest shard's copies are busy.", "properties": {
            "busyAbove": share("The share of their cores the copies use (default 0.7)."),
            "forMinutes": int(1, "For this long (default 10)."),
            "add": int(1, "Copies added to every shard (default 1)."),
            "max": int(1, "Copies of each shard at most (default 5)."),
        }},
        "shrink": {"type": "object", "description": "Remove a copy when every shard's copies are idle.", "properties": {
            "busyBelow": share("The share of their cores the busiest shard's copies use (default 0.35)."),
            "forMinutes": int(1, "For this long (default 60)."),
            "min": int(1, "Copies of each shard at least (default 2)."),
        }},
        "volumes": {"type": "object", "description": "When the fullest volume fills.", "properties": {
            "fullAbove": share("Its used share (default 0.8)."),
            "action": {"type": "string", "enum": ["grow", "addShard"], "description": "grow: every volume by half; addShard: one more shard (default grow)."},
        }},
        "freeze": {"type": "array", "description": "Weekly windows, in UTC, in which nothing changes.", "items": {"type": "object", "properties": {
            "from": {"type": "string", "description": "A day and a time, like Fri 16:00."},
            "to": {"type": "string", "description": "A day and a time, like Mon 08:00."},
            "allowGrow": {"type": "boolean", "description": "Copies may still be added in it (default true)."},
        }}},
        "cooldownMinutes": int(0, "Minutes after a change before the next (default 20)."),
    }})
}

impl Autoscale {
    /// The rules in a sentence.
    pub fn said(&self) -> String {
        let mut parts = Vec::new();
        if let Some(g) = &self.grow {
            parts.push(format!("{} cop{} a shard when its copies are over {} busy for {}, up to {}", g.add, if g.add == 1 { "y" } else { "ies" }, pct(g.busy_above), minutes(g.for_minutes), g.max));
        }
        if let Some(s) = &self.shrink {
            parts.push(format!("one fewer when under {} for {}, down to {}", pct(s.busy_below), minutes(s.for_minutes), s.min));
        }
        if let Some(v) = &self.volumes {
            parts.push(match v.action {
                VolumeAction::Grow => format!("volumes half as big again when one is over {} full", pct(v.full_above)),
                VolumeAction::AddShard => format!("a shard more when a volume is over {} full", pct(v.full_above)),
            });
        }
        for f in &self.freeze {
            parts.push(format!("nothing{} from {} to {} (UTC)", if f.allow_grow { " but growth" } else { "" }, f.from, f.to));
        }
        if parts.is_empty() {
            return "no rules".into();
        }
        let mode = match self.mode {
            Mode::Off => "off (the rules are kept)",
            Mode::Suggest => "a person is asked",
            Mode::On => "the operator acts",
        };
        format!("{}; {} between changes; {mode}", parts.join("; "), minutes(self.cooldown_minutes))
    }

    /// What is wrong with the rules, if anything.
    pub fn check(&self) -> Result<(), String> {
        let share = |name: &str, v: f64| if v.is_finite() && v > 0.0 && v <= 1.0 { Ok(()) } else { Err(format!("autoscale.{name} is a share, more than 0 and at most 1, not {v}")) };
        if let Some(g) = &self.grow {
            share("grow.busyAbove", g.busy_above)?;
            if g.add == 0 || g.max == 0 || g.max > 64 {
                return Err("autoscale.grow.add is 1 or more, and max 1 to 64".into());
            }
        }
        if let Some(s) = &self.shrink {
            share("shrink.busyBelow", s.busy_below)?;
            if s.min == 0 {
                return Err("autoscale.shrink.min is 1 or more".into());
            }
            if let Some(g) = &self.grow {
                if s.busy_below >= g.busy_above {
                    return Err(format!("autoscale.shrink.busyBelow ({}) is under grow.busyAbove ({}), or the rules would undo each other", s.busy_below, g.busy_above));
                }
                if s.min > g.max {
                    return Err(format!("autoscale.shrink.min ({}) is at most grow.max ({})", s.min, g.max));
                }
            }
        }
        if let Some(v) = &self.volumes {
            share("volumes.fullAbove", v.full_above)?;
        }
        for f in &self.freeze {
            week_minute(&f.from)?;
            week_minute(&f.to)?;
        }
        Ok(())
    }

    /// The freeze window `at` is in, if any.
    fn frozen(&self, at: u64) -> Option<&Freeze> {
        self.freeze.iter().find(|f| f.holds(at))
    }
}

/// What the rules read at a moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub at_ms: u64,
    /// The busiest shard's copies: the share of their cores they use.
    pub busy: Option<f64>,
    /// The fullest volume's used share.
    pub full: Option<f64>,
}

/// The sizes the rules change.
#[derive(Clone, Debug, PartialEq)]
pub struct Sizes {
    pub replicas: usize,
    pub shards: usize,
    /// Each copy's volume, as in the spec (`10Gi`).
    pub volume: String,
}

/// What the rules keep from one reading to the next (`status.autoscale`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub mode: Mode,
    pub busy: Option<f64>,
    pub full: Option<f64>,
    pub at_ms: u64,
    /// Since when the copies are over `grow.busyAbove`, and under
    /// `shrink.busyBelow`.
    pub busy_since_ms: Option<u64>,
    pub idle_since_ms: Option<u64>,
    pub last_change_ms: Option<u64>,
    /// A change waiting for a person ([`Mode::Suggest`]).
    pub suggestion: Option<Change>,
    /// The suggestion a person dismissed last (its `atMs`).
    pub dismissed_ms: Option<u64>,
    /// Why nothing changes now, when a rule would fire.
    pub waiting: Option<String>,
    /// The latest changes made or suggested, the newest last.
    pub log: Vec<Entry>,
}

/// A change the rules make: a merge patch of the spec, and why.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Change {
    pub at_ms: u64,
    /// `grow`, `shrink` or `volumes`.
    pub rule: String,
    pub patch: Json,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Entry {
    pub at_ms: u64,
    pub text: String,
}

const LOG: usize = 30;

impl State {
    pub fn note(&mut self, at_ms: u64, text: impl Into<String>) {
        self.log.push(Entry { at_ms, text: text.into() });
        let over = self.log.len().saturating_sub(LOG);
        self.log.drain(..over);
    }
}

fn pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

fn minutes(m: u64) -> String {
    if m >= 60 && m % 60 == 0 {
        format!("{} h", m / 60)
    } else {
        format!("{m} min")
    }
}

/// A volume's size half as big again: `10Gi` → `15Gi` (in Mi below 1Gi).
pub fn grown(size: &str) -> Option<String> {
    let s = size.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let mi = match unit {
        "Ti" => n * 1024.0 * 1024.0,
        "Gi" => n * 1024.0,
        "Mi" => n,
        "T" => n * 1e12 / 1048576.0,
        "G" => n * 1e9 / 1048576.0,
        "M" => n * 1e6 / 1048576.0,
        _ => return None,
    } * 1.5;
    Some(if mi >= 1024.0 { format!("{}Gi", (mi / 1024.0).ceil() as u64) } else { format!("{}Mi", mi.ceil() as u64) })
}

/// The rules on `r`, after `before`: the state to keep, and the change
/// they make now, if any. `settled`: the cluster is as its spec says
/// (a change under way, or a copy still catching up, waits).
pub fn decide(a: &Autoscale, before: &State, r: &Reading, sizes: &Sizes, settled: bool) -> (State, Option<Change>) {
    let now = r.at_ms;
    let mut s = State { mode: a.mode, busy: r.busy, full: r.full, at_ms: now, waiting: None, ..before.clone() };
    // A clock runs from when its condition began, or from the latest
    // change if that came after.
    let clock = |on: bool, was: Option<u64>| on.then(|| was.filter(|w| before.last_change_ms.is_none_or(|t| *w >= t)).unwrap_or(now));
    s.busy_since_ms = clock(a.grow.as_ref().zip(r.busy).is_some_and(|(g, b)| b > g.busy_above), before.busy_since_ms);
    s.idle_since_ms = clock(a.shrink.as_ref().zip(r.busy).is_some_and(|(sh, b)| b < sh.busy_below), before.idle_since_ms);
    let held = |since: Option<u64>, m: u64| since.is_some_and(|t| now.saturating_sub(t) >= m * 60_000);

    // What would fire now, in order: grow, volumes, shrink.
    let mut fire: Option<Change> = None;
    if let Some(g) = &a.grow {
        if held(s.busy_since_ms, g.for_minutes) && sizes.replicas < g.max {
            let to = (sizes.replicas + g.add).min(g.max);
            let text = format!("the copies were {} busy for {}, over {}: {} → {to} copies of each shard", pct(r.busy.unwrap_or(0.0)), minutes(g.for_minutes), pct(g.busy_above), sizes.replicas);
            fire = Some(Change { at_ms: now, rule: "grow".into(), patch: json!({"replicas": to}), text });
        }
    }
    if fire.is_none() {
        if let (Some(v), Some(full)) = (&a.volumes, r.full) {
            if full > v.full_above {
                fire = match v.action {
                    VolumeAction::Grow => grown(&sizes.volume).map(|to| Change {
                        at_ms: now,
                        rule: "volumes".into(),
                        patch: json!({"storage": {"size": to}}),
                        text: format!("the fullest volume was {} full, over {}: every volume {} → {to}", pct(full), pct(v.full_above), sizes.volume),
                    }),
                    VolumeAction::AddShard => Some(Change {
                        at_ms: now,
                        rule: "volumes".into(),
                        patch: json!({"shards": sizes.shards + 1}),
                        text: format!("the fullest volume was {} full, over {}: {} → {} shards (the slots even out)", pct(full), pct(v.full_above), sizes.shards, sizes.shards + 1),
                    }),
                };
            }
        }
    }
    if fire.is_none() {
        if let Some(sh) = &a.shrink {
            if held(s.idle_since_ms, sh.for_minutes) && sizes.replicas > sh.min {
                let text = format!("the copies were under {} busy ({}) for {}: {} → {} copies of each shard", pct(sh.busy_below), pct(r.busy.unwrap_or(0.0)), minutes(sh.for_minutes), sizes.replicas, sizes.replicas - 1);
                fire = Some(Change { at_ms: now, rule: "shrink".into(), patch: json!({"replicas": sizes.replicas - 1}), text });
            }
        }
    }
    let Some(change) = fire else {
        s.suggestion = None;
        return (s, None);
    };

    // The guards.
    let wait = if !settled {
        Some("the cluster is still changing (a copy catching up, or a step under way)".to_string())
    } else if let Some(t) = before.last_change_ms.max(before.dismissed_ms).filter(|t| now.saturating_sub(*t) < a.cooldown_minutes * 60_000) {
        Some(format!("{} have not passed since the last change ({} left)", minutes(a.cooldown_minutes), minutes((a.cooldown_minutes * 60_000).saturating_sub(now.saturating_sub(t)).div_ceil(60_000))))
    } else {
        a.frozen(now).filter(|f| !(f.allow_grow && change.rule == "grow")).map(|f| format!("changes are frozen from {} to {} (UTC)", f.from, f.to))
    };
    if let Some(w) = wait {
        s.waiting = Some(format!("{} waits: {w}", change.text));
        // A suggestion already made stays while it still holds.
        s.suggestion = before.suggestion.clone().filter(|x| x.patch == change.patch);
        return (s, None);
    }
    // A suggestion that stands keeps its time (and is not asked again).
    if let Some(x) = before.suggestion.as_ref().filter(|x| x.patch == change.patch) {
        s.suggestion = Some(x.clone());
        return (s, None);
    }
    s.suggestion = None;
    (s, Some(change))
}

/// The operator's step: the rules on `r`, from the state it kept
/// (`status.autoscale`), after the cluster's latest change (`latest_ms`,
/// its latest version's time, whoever made it) and a person's dismissal
/// (`dismissed`, the `atMs` of the suggestion dismissed). Answers the
/// state to keep and, with [`Mode::On`], the change to make now; with
/// [`Mode::Suggest`] the change waits in the state, and with
/// [`Mode::Off`] the state says what the rules would do.
pub fn step(a: &Autoscale, mut before: State, latest_ms: Option<u64>, dismissed: Option<&str>, r: &Reading, sizes: &Sizes, settled: bool) -> (State, Option<Change>) {
    let now = r.at_ms;
    before.last_change_ms = before.last_change_ms.max(latest_ms);
    if let Some(s) = before.suggestion.clone().filter(|s| dismissed == Some(s.at_ms.to_string().as_str())) {
        before.suggestion = None;
        before.dismissed_ms = Some(now);
        before.note(now, format!("dismissed: {}", s.text));
    }
    let (mut s, change) = decide(a, &before, r, sizes, settled);
    let Some(ch) = change else { return (s, None) };
    match a.mode {
        Mode::Off => {
            s.waiting = Some(format!("autoscaling is off; on, it would now make this change: {}", ch.text));
            (s, None)
        }
        Mode::Suggest => {
            s.note(now, format!("suggested: {}", ch.text));
            s.suggestion = Some(ch);
            (s, None)
        }
        Mode::On => (s, Some(ch)),
    }
}

/// One step of a past week: the busiest shard's copies, how busy, how
/// many answered, and the fullest volume.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub at_ms: u64,
    pub busy: f64,
    pub copies: f64,
    pub full: Option<f64>,
}

/// A copy counts this long after it is added: it is catching up.
pub const CATCH_UP_MS: u64 = 20 * 60_000;
/// How busy is too busy, for the replay's summary.
pub const SATURATED: f64 = 0.85;

/// How long a person takes to apply a suggestion, in the replay of
/// [`Mode::Suggest`]: 20 minutes in office hours (Monday to Friday, 09:00
/// to 18:00 UTC), 2 hours otherwise.
pub fn answer_ms(at_ms: u64) -> u64 {
    let t = minute_of_week(at_ms);
    let (day, minute) = (t / 1440, t % 1440);
    if day < 5 && (9 * 60..18 * 60).contains(&minute) {
        20 * 60_000
    } else {
        120 * 60_000
    }
}

/// What a change does to the replay's cluster.
struct Replayed {
    sizes: Sizes,
    /// Copies added and not caught up yet: (when they will be, how many).
    catching: Vec<(u64, usize)>,
    /// What a volume holds of what it held at the start.
    volume_scale: f64,
}

impl Replayed {
    fn apply(&mut self, c: &Change, at_ms: u64) {
        if let Some(n) = c.patch["replicas"].as_u64() {
            let n = n as usize;
            if n > self.sizes.replicas {
                self.catching.push((at_ms + CATCH_UP_MS, n - self.sizes.replicas));
            }
            self.sizes.replicas = n;
        }
        if let Some(n) = c.patch["shards"].as_u64() {
            self.sizes.shards = n as usize;
        }
        if let Some(v) = c.patch["storage"]["size"].as_str() {
            self.volume_scale /= 1.5;
            self.sizes.volume = v.to_string();
        }
    }
}

/// The rules on a past week of `steps`, from `start`: how busy the
/// copies would have been, how many there would have been, and each
/// change. The work is the load of each step, kept: `busy × copies`
/// shared by the copies the rules would have had (a new one only once it
/// caught up, [`CATCH_UP_MS`]); a volume fills as its share of the data.
/// With [`Mode::Suggest`] a change is made once a person would have
/// applied it ([`answer_ms`]); [`Mode::Off`] replays as [`Mode::On`].
pub fn replay(a: &Autoscale, steps: &[Step], start: &Sizes) -> Json {
    let acting = Autoscale { mode: Mode::On, ..a.clone() };
    let mut c = Replayed { sizes: start.clone(), catching: Vec::new(), volume_scale: 1.0 };
    let mut state = State::default();
    // A suggestion waiting for a person: when they apply it, and it.
    let mut asked: Option<(u64, Change)> = None;
    let mut points = Vec::with_capacity(steps.len());
    let mut events = Vec::new();
    let (mut over, mut over_fixed, mut copy_ms, mut changes, mut waited) = (0u64, 0u64, 0f64, 0usize, 0u64);
    let mut was_frozen = false;
    let step_ms = steps.windows(2).map(|w| w[1].at_ms.saturating_sub(w[0].at_ms)).filter(|d| *d > 0).min().unwrap_or(600_000);
    // `kind`: the rule that made a change, or suggested, applied,
    // caught-up or freeze; `rule`: the rule of a change, suggestion or
    // application.
    let event = |at: u64, kind: &str, rule: Option<&str>, text: String, c: &Replayed| json!({"atMs": at, "kind": kind, "rule": rule, "text": text, "replicas": c.sizes.replicas, "shards": c.sizes.shards});
    for st in steps {
        let now = st.at_ms;
        // Copies that caught up.
        let (done, left): (Vec<_>, Vec<_>) = c.catching.iter().partition(|p| p.0 <= now);
        c.catching = left;
        for (_, n) in done {
            events.push(event(now, "caught-up", None, format!("{} caught up: {} copies of each shard serve", if n == 1 { "the new copy".to_string() } else { format!("{n} new copies") }, c.sizes.replicas), &c));
        }
        // A suggestion a person would have applied by now.
        if let Some((_, ch)) = asked.take_if(|(at, _)| *at <= now) {
            c.apply(&ch, now);
            state.last_change_ms = Some(now);
            changes += 1;
            waited += now - ch.at_ms;
            events.push(event(now, "applied", Some(&ch.rule), format!("a person applied it: {}", ch.text), &c));
        }
        let catching: usize = c.catching.iter().map(|p| p.1).sum();
        let serving = c.sizes.replicas.saturating_sub(catching).max(1);
        let work = st.busy * st.copies.max(1.0);
        let busy = work / serving as f64;
        let fixed = work / start.replicas.max(1) as f64;
        let full = st.full.map(|f| f * c.volume_scale * start.shards as f64 / c.sizes.shards.max(1) as f64);
        let frozen = a.frozen(now);
        if let Some(f) = frozen.filter(|_| !was_frozen) {
            events.push(event(now, "freeze", None, format!("no changes{} until {} (UTC)", if f.allow_grow { " but adding copies" } else { "" }, f.to), &c));
        }
        was_frozen = frozen.is_some();
        let r = Reading { at_ms: now, busy: Some(busy), full };
        let (next, change) = decide(&acting, &state, &r, &c.sizes, catching == 0 && asked.is_none());
        state = next;
        if let Some((at, ch)) = &asked {
            state.waiting = Some(format!("waits for a person to apply it (by {} UTC): {}", when(*at), ch.text));
        }
        if let Some(ch) = change {
            state.busy_since_ms = None;
            state.idle_since_ms = None;
            if a.mode == Mode::Suggest {
                events.push(event(now, "suggested", Some(&ch.rule), format!("suggested: {}", ch.text), &c));
                asked = Some((now + answer_ms(now), ch));
            } else {
                c.apply(&ch, now);
                state.last_change_ms = Some(now);
                changes += 1;
                events.push(event(now, ch.rule.as_str(), Some(&ch.rule), ch.text.clone(), &c));
            }
        }
        if busy > SATURATED {
            over += step_ms;
        }
        if fixed > SATURATED {
            over_fixed += step_ms;
        }
        copy_ms += (c.sizes.replicas * c.sizes.shards) as f64 * step_ms as f64;
        points.push(json!({
            "atMs": now, "busy": busy, "fixed": fixed, "replicas": c.sizes.replicas, "serving": serving, "shards": c.sizes.shards,
            "full": full, "frozen": frozen.is_some(), "waiting": state.waiting,
        }));
    }
    let span = steps.len() as f64 * step_ms as f64;
    json!({
        "stepMs": step_ms,
        "points": points,
        "events": events,
        "summary": {
            "changes": changes,
            "saturated": SATURATED,
            "overMinutes": over / 60_000,
            "fixedOverMinutes": over_fixed / 60_000,
            "copyHours": copy_ms / 3_600_000.0,
            "fixedCopyHours": (start.replicas * start.shards) as f64 * span / 3_600_000.0,
            "waitedMinutes": waited / 60_000,
            "end": {"replicas": c.sizes.replicas, "shards": c.sizes.shards, "volume": c.sizes.volume},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;
    /// Monday 2026-11-02 00:00 UTC.
    const MONDAY: u64 = 1_793_577_600_000;

    fn rules() -> Autoscale {
        Autoscale {
            mode: Mode::On,
            grow: Some(Grow::default()),
            shrink: Some(Shrink { min: 2, ..Shrink::default() }),
            volumes: Some(Volumes::default()),
            freeze: vec![Freeze::default()],
            cooldown_minutes: 20,
        }
    }

    fn sizes(replicas: usize) -> Sizes {
        Sizes { replicas, shards: 4, volume: "10Gi".into() }
    }

    #[test]
    fn the_week_is_counted_from_monday_utc() {
        assert_eq!(minute_of_week(MONDAY), 0);
        assert_eq!(minute_of_week(MONDAY + 4 * 1440 * MIN + 16 * 60 * MIN), week_minute("Fri 16:00").unwrap());
        let f = Freeze::default();
        assert!(f.holds(MONDAY + (4 * 1440 + 17 * 60) * MIN)); // Friday 17:00
        assert!(f.holds(MONDAY + (6 * 1440 + 12 * 60) * MIN)); // Sunday noon
        assert!(f.holds(MONDAY + 7 * 60 * MIN)); // Monday 07:00
        assert!(!f.holds(MONDAY + 9 * 60 * MIN)); // Monday 09:00
        assert!(week_minute("Fri 25:00").is_err() && week_minute("Someday 10:00").is_err() && week_minute("16:00").is_err());
    }

    #[test]
    fn copies_grow_once_busy_for_long_enough_then_wait() {
        let a = rules();
        let tue = MONDAY + 1440 * MIN + 10 * 60 * MIN;
        let at = |m: u64, busy: f64| Reading { at_ms: tue + m * MIN, busy: Some(busy), full: Some(0.4) };
        let (s, c) = decide(&a, &State::default(), &at(0, 0.82), &sizes(3), true);
        assert!(c.is_none() && s.busy_since_ms == Some(tue));
        let (s, c) = decide(&a, &s, &at(9, 0.9), &sizes(3), true);
        assert!(c.is_none());
        // A dip starts the clock again.
        let (s2, _) = decide(&a, &s, &at(9, 0.5), &sizes(3), true);
        assert_eq!(s2.busy_since_ms, None);
        let (s, c) = decide(&a, &s, &at(10, 0.9), &sizes(3), true);
        let c = c.expect("a change");
        assert_eq!((c.rule.as_str(), &c.patch), ("grow", &json!({"replicas": 4})));
        assert_eq!(c.text, "the copies were 90% busy for 10 min, over 70%: 3 → 4 copies of each shard");
        // Not settled, then in the cooldown: it waits, and says why.
        let (w, c) = decide(&a, &s, &at(11, 0.9), &sizes(3), false);
        assert!(c.is_none() && w.waiting.unwrap().ends_with("waits: the cluster is still changing (a copy catching up, or a step under way)"));
        // A change starts the clocks again: busy since before it counts
        // from it.
        let after = State { last_change_ms: Some(tue + 10 * MIN), ..s.clone() };
        let (w, c) = decide(&a, &after, &at(12, 0.9), &sizes(4), true);
        assert!(c.is_none() && w.busy_since_ms == Some(tue + 12 * MIN), "{w:?}");
        let (w, c) = decide(&a, &w, &at(25, 0.9), &sizes(4), true);
        assert!(c.is_none() && w.waiting.unwrap().contains("20 min have not passed since the last change (5 min left)"), "{c:?}");
        // At the most: nothing.
        let (_, c) = decide(&a, &State::default(), &at(0, 0.99), &sizes(5), true);
        assert!(c.is_none());
    }

    #[test]
    fn a_freeze_holds_all_but_growth_when_it_allows_it() {
        let a = rules();
        let friday = MONDAY + (4 * 1440 + 18 * 60) * MIN;
        let busy = State { busy_since_ms: Some(friday - 30 * MIN), idle_since_ms: Some(friday - 120 * MIN), ..State::default() };
        let (_, c) = decide(&a, &busy, &Reading { at_ms: friday, busy: Some(0.9), full: None }, &sizes(3), true);
        assert_eq!(c.unwrap().rule, "grow");
        let (s, c) = decide(&a, &busy, &Reading { at_ms: friday, busy: Some(0.1), full: None }, &sizes(3), true);
        assert!(c.is_none() && s.waiting.unwrap().contains("frozen from Fri 16:00 to Mon 08:00"));
        let strict = Autoscale { freeze: vec![Freeze { allow_grow: false, ..Freeze::default() }], ..a };
        let (_, c) = decide(&strict, &busy, &Reading { at_ms: friday, busy: Some(0.9), full: None }, &sizes(3), true);
        assert!(c.is_none());
    }

    #[test]
    fn copies_shrink_and_volumes_grow_or_split() {
        let a = rules();
        let wed = MONDAY + (2 * 1440 + 3 * 60) * MIN;
        let idle = State { idle_since_ms: Some(wed - 60 * MIN), ..State::default() };
        let (_, c) = decide(&a, &idle, &Reading { at_ms: wed, busy: Some(0.2), full: Some(0.5) }, &sizes(3), true);
        let c = c.unwrap();
        assert_eq!((c.rule.as_str(), &c.patch), ("shrink", &json!({"replicas": 2})));
        let (_, c) = decide(&a, &idle, &Reading { at_ms: wed, busy: Some(0.2), full: Some(0.5) }, &sizes(2), true);
        assert!(c.is_none(), "at the least");
        // A full volume comes before shrinking.
        let (_, c) = decide(&a, &idle, &Reading { at_ms: wed, busy: Some(0.2), full: Some(0.85) }, &sizes(3), true);
        let c = c.unwrap();
        assert_eq!((c.rule.as_str(), &c.patch), ("volumes", &json!({"storage": {"size": "15Gi"}})));
        assert_eq!(c.text, "the fullest volume was 85% full, over 80%: every volume 10Gi → 15Gi");
        let split = Autoscale { volumes: Some(Volumes { action: VolumeAction::AddShard, ..Volumes::default() }), ..a };
        let (_, c) = decide(&split, &idle, &Reading { at_ms: wed, busy: Some(0.2), full: Some(0.85) }, &sizes(3), true);
        assert_eq!(c.unwrap().patch, json!({"shards": 5}));
        assert_eq!((grown("10Gi").as_deref(), grown("512Mi").as_deref(), grown("1Ti").as_deref(), grown("ten")), (Some("15Gi"), Some("768Mi"), Some("1536Gi"), None));
    }

    #[test]
    fn a_suggestion_stands_until_its_reason_goes() {
        let a = Autoscale { mode: Mode::Suggest, ..rules() };
        let tue = MONDAY + (1440 + 10 * 60) * MIN;
        let s0 = State { busy_since_ms: Some(tue - 15 * MIN), ..State::default() };
        let r = Reading { at_ms: tue, busy: Some(0.9), full: None };
        let (mut s, c) = decide(&a, &s0, &r, &sizes(3), true);
        let c = c.unwrap();
        s.suggestion = Some(c.clone());
        // The same reading again: it stands, and is not made again.
        let (s, again) = decide(&a, &s, &Reading { at_ms: tue + MIN, ..r.clone() }, &sizes(3), true);
        assert!(again.is_none() && s.suggestion.as_ref() == Some(&c));
        // The copies calm down: it goes.
        let (s, _) = decide(&a, &s, &Reading { at_ms: tue + 2 * MIN, busy: Some(0.5), full: None }, &sizes(3), true);
        assert!(s.suggestion.is_none());
    }

    #[test]
    fn rules_that_undo_each_other_are_refused() {
        assert!(rules().check().is_ok());
        let bad = Autoscale { shrink: Some(Shrink { busy_below: 0.8, ..Shrink::default() }), ..rules() };
        assert!(bad.check().unwrap_err().contains("would undo each other"));
        let bad = Autoscale { grow: Some(Grow { busy_above: 1.5, ..Grow::default() }), ..rules() };
        assert!(bad.check().unwrap_err().contains("is a share"));
        let bad = Autoscale { freeze: vec![Freeze { from: "Friday".into(), ..Freeze::default() }], ..rules() };
        assert!(bad.check().is_err());
    }

    #[test]
    fn the_operator_acts_asks_or_only_says() {
        let tue = MONDAY + (1440 + 10 * 60) * MIN;
        let busy = State { busy_since_ms: Some(tue - 15 * MIN), ..State::default() };
        let r = Reading { at_ms: tue, busy: Some(0.9), full: None };
        let on = rules();
        let (s, c) = step(&on, busy.clone(), Some(tue - 60 * MIN), None, &r, &sizes(3), true);
        assert_eq!(c.unwrap().patch, json!({"replicas": 4}));
        assert!(s.suggestion.is_none());
        // A person's change 5 minutes ago starts the clock again.
        let (s, c) = step(&on, busy.clone(), Some(tue - 5 * MIN), None, &r, &sizes(3), true);
        assert!(c.is_none() && s.busy_since_ms == Some(tue), "{s:?}");
        let ask = Autoscale { mode: Mode::Suggest, ..rules() };
        let (s, c) = step(&ask, busy.clone(), None, None, &r, &sizes(3), true);
        let suggested = s.suggestion.clone().unwrap();
        assert!(c.is_none() && s.log.last().unwrap().text.starts_with("suggested: the copies were 90% busy"));
        // Dismissed by its time: gone, noted, and the cooldown counts from it.
        let at = suggested.at_ms.to_string();
        let r2 = Reading { at_ms: tue + MIN, ..r.clone() };
        let (s, c) = step(&ask, s, None, Some(at.as_str()), &r2, &sizes(3), true);
        assert!(c.is_none() && s.suggestion.is_none() && s.dismissed_ms == Some(tue + MIN), "{s:?}");
        assert!(s.log.last().unwrap().text.starts_with("dismissed: "));
        assert!(s.waiting.as_deref().unwrap_or_default().contains("have not passed since the last change"), "{s:?}");
        let off = Autoscale { mode: Mode::Off, ..rules() };
        let (s, c) = step(&off, busy, None, None, &r, &sizes(3), true);
        assert!(c.is_none() && s.suggestion.is_none() && s.waiting.unwrap().starts_with("autoscaling is off; on, it would now make this change: the copies were 90% busy"));
        assert_eq!(when(tue), "Tue 10:00");
        assert_eq!((answer_ms(tue), answer_ms(tue + 9 * 60 * MIN), answer_ms(MONDAY + 5 * 1440 * MIN + 12 * 60 * MIN)), (20 * MIN, 120 * MIN, 120 * MIN));
        assert_eq!(rules().said(), "1 copy a shard when its copies are over 70% busy for 10 min, up to 5; one fewer when under 35% for 1 h, down to 2; \
             volumes half as big again when one is over 80% full; nothing but growth from Fri 16:00 to Mon 08:00 (UTC); 20 min between changes; the operator acts");
    }

    #[test]
    fn a_week_is_replayed_with_the_work_kept() {
        // Tuesday from 09:00: 3 copies 60% busy, then two hours at 95%,
        // then calm.
        let tue = MONDAY + (1440 + 9 * 60) * MIN;
        let steps: Vec<Step> = (0..36)
            .map(|i| {
                let busy = if (6..18).contains(&i) { 0.95 } else { 0.6 };
                Step { at_ms: tue + i * 10 * MIN, busy, copies: 3.0, full: Some(0.5) }
            })
            .collect();
        let a = Autoscale { shrink: None, ..rules() };
        let r = replay(&a, &steps, &sizes(3));
        let events = r["events"].as_array().unwrap();
        // Busy at 60 min, 10 min over at 70 min: one copy; caught up 20 min
        // later the busy share is 95% × 3 / 4 = 71%, still over: after the
        // cooldown, a fifth.
        assert_eq!(events[0]["atMs"], tue + 70 * MIN);
        assert_eq!(events[0]["replicas"], 4);
        assert_eq!(events.last().unwrap()["replicas"], 5, "{events:?}");
        let p = &r["points"].as_array().unwrap()[8];
        assert_eq!(p["replicas"], 4);
        assert!((p["busy"].as_f64().unwrap() - 0.95).abs() < 1e-9, "the new copy is still catching up: {p}");
        let s = &r["summary"];
        assert!(s["overMinutes"].as_u64().unwrap() < s["fixedOverMinutes"].as_u64().unwrap(), "{s}");
        assert_eq!(s["fixedOverMinutes"], 120);
        assert!(s["copyHours"].as_f64().unwrap() > s["fixedCopyHours"].as_f64().unwrap());
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["grow", "caught-up", "grow", "caught-up"], "{events:?}");

        // Asking a person: the copy comes once one would have applied it,
        // 20 minutes later in office hours; nothing else is asked
        // meanwhile.
        let ask = Autoscale { mode: Mode::Suggest, shrink: None, ..rules() };
        let r = replay(&ask, &steps, &sizes(3));
        let events = r["events"].as_array().unwrap();
        assert_eq!((events[0]["kind"].as_str(), &events[0]["atMs"]), (Some("suggested"), &json!(tue + 70 * MIN)));
        assert_eq!((events[1]["kind"].as_str(), &events[1]["atMs"], &events[1]["replicas"]), (Some("applied"), &json!(tue + 90 * MIN), &json!(4)));
        let p = &r["points"].as_array().unwrap()[8];
        assert!(p["waiting"].as_str().unwrap().starts_with("waits for a person to apply it (by Tue 10:30 UTC)"), "{p}");
        // Two suggestions, each applied 20 minutes after it was made.
        assert_eq!(r["summary"]["waitedMinutes"], 40);
        assert!(r["summary"]["overMinutes"].as_u64().unwrap() > replay(&Autoscale { shrink: None, ..rules() }, &steps, &sizes(3))["summary"]["overMinutes"].as_u64().unwrap());
    }
}
