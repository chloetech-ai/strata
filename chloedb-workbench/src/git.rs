//! Worktrees: where a session runs when it has a checkout of its own.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value as Json};

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("git {} failed", args.join(" ")) } else { err })
    }
}

/// A checkout, read back to its repository.
#[derive(Clone, Debug)]
pub struct Checkout {
    /// The repository's main checkout.
    pub main: PathBuf,
}

/// The checkout `path` is in, and its repository's main checkout; an error
/// when it is not in a git checkout.
pub fn checkout(path: &Path) -> Result<Checkout, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a folder", path.display()));
    }
    let top = git(path, &["rev-parse", "--show-toplevel"]).map_err(|_| format!("{} is not in a git checkout", path.display()))?;
    let top = PathBuf::from(top);
    let common = git(path, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = PathBuf::from(common);
    let main = match common.file_name().and_then(|n| n.to_str()) {
        Some(".git") => common.parent().map(Path::to_path_buf).unwrap_or_else(|| top.clone()),
        _ => top.clone(),
    };
    let main = main.canonicalize().unwrap_or(main);
    Ok(Checkout { main })
}

/// The commit `rev` names in `dir`, in full.
pub fn rev_parse(dir: &Path, rev: &str) -> Option<String> {
    git(dir, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).ok().filter(|s| !s.is_empty())
}

/// The branch checked out at `dir`, or `None` on a detached HEAD.
pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok().filter(|b| !b.is_empty())
}

/// Whether the local branch `name` exists.
pub fn branch_exists(repo: &Path, name: &str) -> bool {
    git(repo, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_ok()
}

/// A short, stable hash of a path (FNV-1a), as 8 hex digits.
pub fn hash8(path: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in path.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

/// The folder the worktrees of `repo` go in.
pub fn worktrees_dir(data_dir: &Path, repo: &Path) -> PathBuf {
    let name = repo.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "repo".into());
    data_dir.join("worktrees").join(format!("{name}-{}", hash8(repo)))
}

/// Adds a worktree of `repo` at `path` on `branch`: a new branch from
/// `base` (the repository's HEAD when `None`), or the branch as it is when
/// it exists already.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base: Option<&str>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let p = path.to_string_lossy().to_string();
    if branch_exists(repo, branch) {
        git(repo, &["worktree", "add", &p, branch])?;
    } else {
        let base = base.unwrap_or("HEAD");
        git(repo, &["worktree", "add", &p, "-b", branch, base])?;
    }
    Ok(())
}

/// The commit of line `line` of `rel` in `dir` (`git blame`): its sha, the
/// author's time (seconds) and subject; `None` when it is not committed
/// or git cannot say.
pub fn blame_line(dir: &Path, rel: &str, line: usize) -> Option<(String, i64, String)> {
    let out = git(dir, &["blame", "--porcelain", "-L", &format!("{line},{line}"), "--", rel]).ok()?;
    let sha = out.split_whitespace().next()?.to_string();
    if sha.chars().all(|c| c == '0') {
        return None;
    }
    let field = |k: &str| out.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
    Some((sha, field("author-time ")?.parse().ok()?, field("summary ").unwrap_or_default()))
}

/// Whether commit `a` is `b` or one of its ancestors.
pub fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", a, b]).is_ok()
}

/// A C-quoted path as git prints one (`"a\tb"`), unquoted; others as they are.
fn unquote(p: &str) -> String {
    let Some(inner) = p.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else { return p.to_string() };
    let b = inner.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'0'..=b'7' if i + 2 < b.len() => {
                    let o = std::str::from_utf8(&b[i..i + 3]).ok().and_then(|s| u8::from_str_radix(s, 8).ok()).unwrap_or(b'?');
                    out.push(o);
                    i += 2;
                }
                c => out.push(c),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path of a `---` or `+++` line without its `a/` or `b/`, `None` for
/// `/dev/null`.
fn side(p: &str) -> Option<String> {
    let p = p.strip_suffix('\t').unwrap_or(p);
    if p == "/dev/null" {
        return None;
    }
    let p = unquote(p);
    Some(p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(&p).to_string())
}

/// The path of a `diff --git a/X b/X` header when both sides are the same.
fn header_path(h: &str) -> String {
    if h.starts_with('"') {
        if let Some(end) = h[1..].find("\" ").map(|i| i + 2) {
            return side(&h[end + 1..]).unwrap_or_default();
        }
    }
    let l = h.len().saturating_sub(5) / 2;
    match h.get(2..2 + l) {
        Some(p) => p.to_string(),
        None => h.to_string(),
    }
}

/// `@@ -a,b +c,d @@`: `(a, c)`.
fn hunk_starts(l: &str) -> Option<(i64, i64)> {
    let mut it = l.split_whitespace().skip(1);
    let num = |s: &str| s[1..].split(',').next().and_then(|n| n.parse::<i64>().ok());
    Some((num(it.next()?)?, num(it.next()?)?))
}

/// A unified diff (`git diff`'s output) as files of hunks of lines.
pub fn parse_diff(text: &str) -> Vec<Json> {
    let mut files: Vec<Json> = Vec::new();
    let (mut old_no, mut new_no) = (0i64, 0i64);
    for l in text.lines() {
        if let Some(h) = l.strip_prefix("diff --git ") {
            files.push(json!({ "path": header_path(h), "status": "modified", "hunks": [] }));
            continue;
        }
        let Some(f) = files.last_mut() else { continue };
        let in_hunk = f["hunks"].as_array().is_some_and(|h| !h.is_empty());
        if !in_hunk {
            if l.starts_with("new file mode") {
                f["status"] = json!("added");
            } else if l.starts_with("deleted file mode") {
                f["status"] = json!("deleted");
            } else if let Some(p) = l.strip_prefix("rename from ") {
                f["status"] = json!("renamed");
                f["old_path"] = json!(unquote(p));
            } else if let Some(p) = l.strip_prefix("rename to ") {
                f["path"] = json!(unquote(p));
            } else if l.starts_with("Binary files ") {
                f["binary"] = json!(true);
            } else if let Some(p) = l.strip_prefix("--- ") {
                if let Some(p) = side(p) {
                    f["old_path"] = json!(p);
                }
            } else if let Some(p) = l.strip_prefix("+++ ") {
                match side(p) {
                    Some(p) => f["path"] = json!(p),
                    None => f["status"] = json!("deleted"),
                }
                if f["old_path"] == f["path"] {
                    f.as_object_mut().unwrap().remove("old_path");
                }
            }
        }
        if l.starts_with("@@ ") {
            if let Some((o, n)) = hunk_starts(l) {
                (old_no, new_no) = (o, n);
                f["hunks"].as_array_mut().unwrap().push(json!({ "old_start": o, "new_start": n, "lines": [] }));
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        let lines = f["hunks"].as_array_mut().unwrap().last_mut().unwrap()["lines"].as_array_mut().unwrap();
        match l.as_bytes().first() {
            Some(b'+') => {
                lines.push(json!({ "kind": "+", "text": &l[1..], "new_line": new_no, "old_line": null }));
                new_no += 1;
            }
            Some(b'-') => {
                lines.push(json!({ "kind": "-", "text": &l[1..], "new_line": null, "old_line": old_no }));
                old_no += 1;
            }
            Some(b' ') => {
                lines.push(json!({ "kind": " ", "text": &l[1..], "new_line": new_no, "old_line": old_no }));
                old_no += 1;
                new_no += 1;
            }
            None => {
                // An empty context line, as some tools write it.
                lines.push(json!({ "kind": " ", "text": "", "new_line": new_no, "old_line": old_no }));
                old_no += 1;
                new_no += 1;
            }
            _ => {} // "\ No newline at end of file"
        }
    }
    for f in &mut files {
        if f.get("binary").is_none() {
            f["binary"] = json!(false);
        }
    }
    files
}

/// The most of an untracked file the diff shows as added lines.
const UNTRACKED_MAX: u64 = 1 << 20;

/// What changed in `dir` since `base`, committed or not, untracked files
/// as added: `[{path, status, binary, hunks}]`, by path.
pub fn diff(dir: &Path, base: &str) -> Result<Vec<Json>, String> {
    let text = git(
        dir,
        &["-c", "core.quotePath=false", "diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M", "--src-prefix=a/", "--dst-prefix=b/", base, "--"],
    )?;
    let mut files = parse_diff(&text);
    let untracked = git(dir, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for p in untracked.split('\0').filter(|p| !p.is_empty()) {
        let full = dir.join(p);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        let mut f = json!({ "path": p, "status": "added", "untracked": true, "binary": false, "hunks": [] });
        if meta.len() > UNTRACKED_MAX {
            f["too_large"] = json!(true);
        } else {
            let body = std::fs::read(&full).unwrap_or_default();
            if body.contains(&0) {
                f["binary"] = json!(true);
            } else if !body.is_empty() {
                let text = String::from_utf8_lossy(&body);
                let lines: Vec<Json> = text.lines().enumerate().map(|(i, l)| json!({ "kind": "+", "text": l, "new_line": i + 1, "old_line": null })).collect();
                f["hunks"] = json!([{ "old_start": 0, "new_start": 1, "lines": lines }]);
            }
        }
        files.push(f);
    }
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    Ok(files)
}

/// What a worktree changed that its repository's main checkout does not
/// have yet (the journeys' "ready to review", JOURNEYS.md): files and
/// lines added and removed since the merge base of the two, committed or
/// not, untracked files counted as added. `None` when git cannot say.
pub fn change_stat(worktree: &Path, main: &Path) -> Option<(usize, i64, i64)> {
    let head = rev_parse(main, "HEAD")?;
    let base = git(worktree, &["merge-base", &head, "HEAD"]).ok().filter(|s| !s.is_empty())?;
    let num = git(worktree, &["diff", "--numstat", "--no-color", "--no-ext-diff", "--no-textconv", &base, "--"]).ok()?;
    let (mut files, mut add, mut del) = (0usize, 0i64, 0i64);
    for l in num.lines() {
        let mut it = l.split('\t');
        let (a, d) = (it.next().unwrap_or("0"), it.next().unwrap_or("0"));
        files += 1;
        add += a.parse::<i64>().unwrap_or(0);
        del += d.parse::<i64>().unwrap_or(0);
    }
    let untracked = git(worktree, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default();
    for p in untracked.split('\0').filter(|p| !p.is_empty()) {
        files += 1;
        let full = worktree.join(p);
        if std::fs::metadata(&full).map(|m| m.is_file() && m.len() <= UNTRACKED_MAX).unwrap_or(false) {
            add += std::fs::read(&full).map(|b| if b.contains(&0) { 0 } else { String::from_utf8_lossy(&b).lines().count() as i64 }).unwrap_or(0);
        }
    }
    Some((files, add, del))
}

/// The local branches under `prefix` (such as `wb/`) whose commits the
/// checked-out branch of `repo` does not have, newest first.
pub fn unmerged_branches(repo: &Path, prefix: &str) -> Vec<String> {
    let pattern = format!("refs/heads/{prefix}*");
    git(repo, &["for-each-ref", "--sort=-committerdate", "--no-merged", "HEAD", "--format=%(refname:short)", &pattern])
        .map(|s| s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_unified_diff() {
        let d = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,3 +1,3 @@ fn x\n keep\n-old\n+new\n\\ No newline at end of file\ndiff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+hi\ndiff --git a/gone b/gone\ndeleted file mode 100644\n--- a/gone\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\ndiff --git a/old name b/new name\nsimilarity index 90%\nrename from old name\nrename to new name\ndiff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\n";
        let f = parse_diff(d);
        assert_eq!(f.len(), 5);
        assert_eq!(f[0]["path"], "src/a.rs");
        assert_eq!(f[0]["status"], "modified");
        let lines = &f[0]["hunks"][0]["lines"];
        assert_eq!(lines[0], json!({"kind": " ", "text": "keep", "new_line": 1, "old_line": 1}));
        assert_eq!(lines[1], json!({"kind": "-", "text": "old", "new_line": null, "old_line": 2}));
        assert_eq!(lines[2], json!({"kind": "+", "text": "new", "new_line": 2, "old_line": null}));
        assert_eq!(lines.as_array().unwrap().len(), 3);
        assert_eq!(f[1]["status"], "added");
        assert_eq!(f[1]["hunks"][0]["lines"][0]["new_line"], 1);
        assert_eq!(f[2]["status"], "deleted");
        assert_eq!(f[2]["path"], "gone");
        assert_eq!(f[3]["status"], "renamed");
        assert_eq!(f[3]["path"], "new name");
        assert_eq!(f[3]["old_path"], "old name");
        assert_eq!(f[4]["binary"], true);
        assert_eq!(f[4]["path"], "img.png");
        assert_eq!(unquote("\"a\\tb\\303\\251\""), "a\tbé");
    }
}
