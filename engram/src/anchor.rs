//! Notes anchored to the code they describe, and a check, at the moment a
//! note is shown, of whether that code has changed since.
//!
//! A note written down from a turn records the commit and branch the
//! repository was on, the files it is about with a hash of their content at
//! that moment, and the identifiers it names that occurred in those files.
//! Before the note is injected, the files are hashed again: a note whose
//! file is gone, or whose identifiers no longer occur in it, is labelled as
//! possibly stale; one whose file merely changed says so. The anchor lives
//! in the record's context as one `Anchor:` line, so stores need no new
//! column.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::embed::fnv;

/// Files larger than this are not hashed.
const MAX_FILE: u64 = 4 << 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    /// Abbreviated commit the repository was at.
    pub head: String,
    pub branch: String,
    /// (path relative to the project, content hash) of the files the note is about.
    pub files: Vec<(String, String)>,
    /// Identifiers the note names that occurred in those files.
    pub idents: Vec<String>,
}

impl Anchor {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.head.is_empty()
    }

    /// `Anchor: head=… branch=… files=a.py#h1,b.py#h2 idents=x,y`
    pub fn to_line(&self) -> String {
        let files: Vec<String> = self.files.iter().map(|(p, h)| format!("{p}#{h}")).collect();
        let mut parts = vec![format!("head={}", self.head), format!("branch={}", self.branch)];
        if !files.is_empty() {
            parts.push(format!("files={}", files.join(",")));
        }
        if !self.idents.is_empty() {
            parts.push(format!("idents={}", self.idents.join(",")));
        }
        format!("Anchor: {}", parts.join(" "))
    }

    /// The anchor in a record's context, if any.
    pub fn parse(ctx: &str) -> Option<Anchor> {
        let line = ctx.lines().find(|l| l.starts_with("Anchor: "))?;
        let mut a = Anchor::default();
        for part in line["Anchor: ".len()..].split(' ') {
            let Some((k, v)) = part.split_once('=') else { continue };
            match k {
                "head" => a.head = v.to_string(),
                "branch" => a.branch = v.to_string(),
                "files" => a.files = v.split(',').filter(|x| !x.is_empty()).filter_map(|x| x.rsplit_once('#').map(|(p, h)| (p.to_string(), h.to_string()))).collect(),
                "idents" => a.idents = v.split(',').filter(|x| !x.is_empty()).map(String::from).collect(),
                _ => {}
            }
        }
        Some(a)
    }

    /// Joins anchors of notes merged into one: the newest note's commit and
    /// branch, every file (the newest hash of each) and every identifier.
    pub fn merge(anchors: &[Anchor]) -> Anchor {
        let mut out = anchors.last().cloned().unwrap_or_default();
        let mut files: Vec<(String, String)> = Vec::new();
        for a in anchors.iter().rev() {
            for (p, h) in &a.files {
                if !files.iter().any(|(q, _)| q == p) {
                    files.push((p.clone(), h.clone()));
                }
            }
        }
        let idents: BTreeSet<String> = anchors.iter().flat_map(|a| a.idents.iter().cloned()).collect();
        out.files = files.into_iter().take(6).collect();
        out.idents = idents.into_iter().take(8).collect();
        out
    }
}

/// The repository's `.git` directory for `project` (walking up), and the
/// common directory holding refs (they differ for a worktree).
fn git_dirs(project: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = Some(project);
    while let Some(d) = dir {
        let g = d.join(".git");
        if g.is_dir() {
            return Some((g.clone(), g));
        }
        if g.is_file() {
            let text = std::fs::read_to_string(&g).ok()?;
            let rel = text.trim().strip_prefix("gitdir:")?.trim();
            let gd = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { d.join(rel) };
            let common = std::fs::read_to_string(gd.join("commondir")).ok().map(|c| {
                let c = c.trim();
                if Path::new(c).is_absolute() {
                    PathBuf::from(c)
                } else {
                    gd.join(c)
                }
            });
            return Some((gd.clone(), common.unwrap_or(gd)));
        }
        dir = d.parent();
    }
    None
}

/// (abbreviated HEAD commit, branch) of the repository at `project`, read
/// from `.git` without running git; empty strings outside a repository.
pub fn git_head(project: &Path) -> (String, String) {
    let Some((gd, common)) = git_dirs(project) else { return (String::new(), String::new()) };
    let head = std::fs::read_to_string(gd.join("HEAD")).unwrap_or_default();
    let head = head.trim();
    let Some(r) = head.strip_prefix("ref:").map(str::trim) else {
        return (head.chars().take(10).collect(), "(detached)".into());
    };
    let branch = r.strip_prefix("refs/heads/").unwrap_or(r).to_string();
    let sha = std::fs::read_to_string(common.join(r)).ok().map(|s| s.trim().to_string()).or_else(|| {
        let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
        packed.lines().find_map(|l| l.strip_suffix(r).map(|s| s.trim().to_string()))
    });
    (sha.unwrap_or_default().chars().take(10).collect(), branch)
}

/// A file's content, if it exists and is not too large.
fn read(project: &Path, rel: &str) -> Option<String> {
    let p = project.join(rel);
    let meta = std::fs::metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    let bytes = std::fs::read(&p).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn hash(text: &str) -> String {
    format!("{:016x}", fnv(text))
}

/// Identifiers a note names: words in backticks, and words shaped like code
/// (snake_case, camelCase, dotted names, calls). File paths are left out.
pub fn identifiers(text: &str) -> Vec<String> {
    static TICK: OnceLock<Regex> = OnceLock::new();
    static CODE: OnceLock<Regex> = OnceLock::new();
    let tick = TICK.get_or_init(|| Regex::new(r"`([^`\n]{3,60})`").unwrap());
    let code = CODE.get_or_init(|| Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*(?:\(\))?").unwrap());
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim().trim_end_matches("()").trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
        // The last component of a dotted name (`Ledger.balance_of` → balance_of) is what occurs in the file.
        let s = s.rsplit('.').next().unwrap_or(s);
        if s.len() >= 4 && s.len() <= 60 && s.chars().all(|c| c.is_alphanumeric() || c == '_') && !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };
    for c in tick.captures_iter(text) {
        let inner = &c[1];
        if inner.contains('/') || crate::capture::paths_in(inner).first().is_some_and(|p| p == inner) {
            continue;
        }
        for w in inner.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == '=') {
            if w.chars().any(|c| c.is_alphabetic()) {
                push(w);
            }
        }
    }
    for m in code.find_iter(text) {
        let w = m.as_str();
        let looks = w.contains('_') || w.ends_with("()") || (w.contains('.') && !crate::capture::paths_in(w).first().is_some_and(|p| p == w))
            || (w.chars().skip(1).any(|c| c.is_uppercase()) && w.chars().any(|c| c.is_lowercase()));
        if looks {
            push(w);
        }
    }
    out
}

/// The anchor of a note about `files_named` (paths in its text), written
/// during a turn that changed `turn_files`: those of the named files that
/// exist, and the turn's files that contain an identifier the note names.
pub fn make(project: &Path, text: &str, files_named: &[String], turn_files: &[String]) -> Anchor {
    let (head, branch) = git_head(project);
    let idents = identifiers(text);
    let mut files: Vec<(String, String)> = Vec::new();
    let mut found: Vec<String> = Vec::new();
    let mut consider = |rel: &str, need_ident: bool, files: &mut Vec<(String, String)>| {
        if files.len() >= 4 || files.iter().any(|(p, _)| p == rel) {
            return;
        }
        let Some(content) = read(project, rel) else { return };
        let here: Vec<&String> = idents.iter().filter(|i| contains_word(&content, i)).collect();
        if need_ident && here.is_empty() {
            return;
        }
        for i in here {
            if !found.contains(i) {
                found.push(i.clone());
            }
        }
        files.push((rel.to_string(), hash(&content)));
    };
    for f in files_named {
        consider(f, false, &mut files);
    }
    for f in turn_files {
        consider(f, true, &mut files);
    }
    found.truncate(8);
    Anchor { head, branch, files, idents: found }
}

fn contains_word(hay: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(i) = hay[start..].find(word) {
        let a = start + i;
        let b = a + word.len();
        let before = hay[..a].chars().next_back().map_or(true, |c| !(c.is_alphanumeric() || c == '_'));
        let after = hay[b..].chars().next().map_or(true, |c| !(c.is_alphanumeric() || c == '_'));
        if before && after {
            return true;
        }
        start = b;
    }
    false
}

/// What the check found about a note's anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// Nothing anchored, or nothing changed.
    Current,
    /// These files changed since the note (its identifiers still occur).
    Changed(Vec<String>),
    /// The note probably describes code that is no longer there.
    Stale(String),
    /// A rule or requirement whose code moved away from what it was stated
    /// against: the rule still holds, the code may now break it.
    Drifted(String),
}

impl Freshness {
    /// The label shown after the note ("" when current).
    pub fn label(&self) -> String {
        match self {
            Freshness::Current => String::new(),
            Freshness::Changed(f) => format!(" [{} changed since this note; check the current code]", f.join(", ")),
            Freshness::Stale(why) => format!(" [may be out of date: {why}; check the current code before relying on it]"),
            Freshness::Drifted(why) => format!(" [the code changed since this was stated ({why}); check that it still follows it]"),
        }
    }
    pub fn is_stale(&self) -> bool {
        matches!(self, Freshness::Stale(_))
    }
}

/// Checks anchors against the working tree, reading each file once.
pub struct Checker {
    pub project: PathBuf,
    branch: Option<String>,
    files: HashMap<String, Option<(String, String)>>,
}

impl Checker {
    pub fn new(project: &Path) -> Checker {
        Checker { project: project.to_path_buf(), branch: None, files: HashMap::new() }
    }

    fn file(&mut self, rel: &str) -> Option<(String, String)> {
        if let Some(v) = self.files.get(rel) {
            return v.clone();
        }
        let v = read(&self.project, rel).map(|c| (hash(&c), c));
        if self.files.len() < 64 {
            self.files.insert(rel.to_string(), v.clone());
        }
        v
    }

    /// How a note stands against the current code. Rules and requirements
    /// never go stale with the code: a change that removed what they were
    /// stated against is reported as drift.
    pub fn check_record(&mut self, r: &crate::record::Record) -> Freshness {
        match self.check_ctx(&r.ctx) {
            Freshness::Stale(why) if matches!(r.kind, crate::record::Kind::Preference | crate::record::Kind::Todo) => Freshness::Drifted(why),
            f => f,
        }
    }

    /// How a record with context `ctx` stands against the current code.
    pub fn check_ctx(&mut self, ctx: &str) -> Freshness {
        match Anchor::parse(ctx) {
            Some(a) => self.check(&a),
            None => Freshness::Current,
        }
    }

    pub fn check(&mut self, a: &Anchor) -> Freshness {
        if a.files.is_empty() {
            return Freshness::Current;
        }
        let mut changed = Vec::new();
        let mut gone = Vec::new();
        let mut now_text = String::new();
        for (p, h) in &a.files {
            match self.file(p) {
                None => gone.push(p.clone()),
                Some((h2, content)) => {
                    if &h2 != h {
                        changed.push(p.clone());
                    }
                    now_text.push_str(&content);
                    now_text.push('\n');
                }
            }
        }
        if !gone.is_empty() && gone.len() == a.files.len() {
            return Freshness::Stale(format!("{} no longer exist{}", gone.join(", "), if gone.len() == 1 { "s" } else { "" }));
        }
        if changed.is_empty() && gone.is_empty() {
            return Freshness::Current;
        }
        let missing: Vec<&String> = a.idents.iter().filter(|i| !contains_word(&now_text, i)).collect();
        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().take(3).map(|i| format!("`{i}`")).collect();
            let files: Vec<String> = a.files.iter().map(|(p, _)| p.clone()).collect();
            return Freshness::Stale(format!("{} no longer appear{} in {}", names.join(", "), if missing.len() == 1 { "s" } else { "" }, files.join(", ")));
        }
        changed.extend(gone);
        Freshness::Changed(changed)
    }

    /// The current branch, read once.
    pub fn branch(&mut self) -> String {
        if self.branch.is_none() {
            self.branch = Some(git_head(&self.project).1);
        }
        self.branch.clone().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_look_like_code() {
        let ids = identifiers("Use `format_amount()` in money.py; Ledger.balance_of stays as an alias. The CLI uses argparse and parseConfig. Plain words stay out.");
        assert!(ids.contains(&"format_amount".to_string()), "{ids:?}");
        assert!(ids.contains(&"balance_of".to_string()), "{ids:?}");
        assert!(ids.contains(&"parseConfig".to_string()), "{ids:?}");
        assert!(!ids.iter().any(|i| i == "money" || i == "py" || i == "Plain" || i == "argparse"), "{ids:?}");
    }

    #[test]
    fn anchors_round_trip_and_go_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join(".git/refs/heads")).unwrap();
        std::fs::write(p.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(p.join(".git/refs/heads/main"), "0123456789abcdef0123456789abcdef01234567\n").unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/client.py"), "DEFAULT_TIMEOUT = 30\n\ndef fetch_page(url):\n    pass\n").unwrap();
        std::fs::write(p.join("src/other.py"), "x = 1\n").unwrap();
        let a = make(p, "The client's DEFAULT_TIMEOUT is 30 s because the depot links are slow; fetch_page retries twice.", &[], &["src/client.py".into(), "src/other.py".into()]);
        assert_eq!(a.head, "0123456789");
        assert_eq!(a.branch, "main");
        assert_eq!(a.files.len(), 1, "{a:?}");
        assert_eq!(a.idents, vec!["DEFAULT_TIMEOUT".to_string(), "fetch_page".to_string()]);
        let back = Anchor::parse(&format!("Turn 3 request: x\n{}", a.to_line())).unwrap();
        assert_eq!(back, a);
        assert_eq!(Checker::new(p).check(&a), Freshness::Current);
        // A change that keeps the identifiers.
        std::fs::write(p.join("src/client.py"), "DEFAULT_TIMEOUT = 10\n\ndef fetch_page(url):\n    pass\n").unwrap();
        assert_eq!(Checker::new(p).check(&a), Freshness::Changed(vec!["src/client.py".into()]));
        // A rename.
        std::fs::write(p.join("src/client.py"), "TIMEOUT_S = 10\n\ndef fetch_page(url):\n    pass\n").unwrap();
        let f = Checker::new(p).check(&a);
        assert!(f.is_stale() && f.label().contains("`DEFAULT_TIMEOUT`"), "{f:?}");
        // A deletion.
        std::fs::remove_file(p.join("src/client.py")).unwrap();
        assert!(Checker::new(p).check(&a).label().contains("no longer exists"));
    }

    #[test]
    fn rules_drift_instead_of_going_stale() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/pay.py"), "MODE = ROUND_HALF_UP\n").unwrap();
        let a = make(p, "Totals use ROUND_HALF_UP", &["src/pay.py".to_string()], &[]);
        assert!(!a.to_line().contains("idents= "));
        std::fs::write(p.join("src/pay.py"), "MODE = ROUND_HALF_EVEN\n").unwrap();
        let rule = crate::record::Record::from_draft(crate::record::Draft::new(crate::record::Kind::Preference, "Totals use ROUND_HALF_UP").ctx(&a.to_line()), "s", 1, 1, 1);
        let fact = crate::record::Record { kind: crate::record::Kind::Fact, ..rule.clone() };
        let mut ck = Checker::new(p);
        assert!(ck.check_record(&fact).is_stale());
        let f = ck.check_record(&rule);
        assert!(!f.is_stale() && f.label().contains("still follows"), "{f:?}");
    }

    #[test]
    fn merged_anchors_keep_old_hashes() {
        let a = Anchor { head: "a".into(), branch: "main".into(), files: vec![("x.py".into(), "1".into())], idents: vec!["foo".into()] };
        let b = Anchor { head: "b".into(), branch: "main".into(), files: vec![("x.py".into(), "2".into()), ("y.py".into(), "3".into())], idents: vec!["bar".into()] };
        let m = Anchor::merge(&[a, b]);
        assert_eq!(m.head, "b");
        assert_eq!(m.files, vec![("x.py".to_string(), "2".to_string()), ("y.py".to_string(), "3".to_string())]);
        assert_eq!(m.idents, vec!["bar".to_string(), "foo".to_string()]);
    }
}
