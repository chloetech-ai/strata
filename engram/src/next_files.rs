//! Files that usually come next. B follows A when an edit of B comes within
//! [`GAP`] edits after one of A in a session (consecutive edits of one file
//! count once), weighted by one over the gap: counted transitions, as
//! `strata.next_items` keeps them for items. They are counted from the edits
//! the store already holds as tool calls, so no write is needed and any
//! store works; the daemon keeps the counts and reads only what was stored
//! since. Before an edit of A, the PreToolUse hook names the files that
//! usually come next, with a short note on each. An experiment, off unless
//! `ENGRAM_NEXT_FILES=1`.

use std::collections::{HashMap, VecDeque};

use strata::query::Params;
use strata::Value;

use crate::store::Store;
use crate::Result;

/// How many edits after one of A an edit of B still counts.
pub const GAP: usize = 3;

/// The tools whose calls are edits of one file.
pub const EDITS: [&str; 3] = ["Edit", "MultiEdit", "Write"];

#[derive(Default)]
pub struct NextFiles {
    /// For each file, the files edited after it, with their weights.
    next: HashMap<String, HashMap<String, f64>>,
    /// Each session's last [`GAP`] files edited, newest last.
    recent: HashMap<String, VecDeque<String>>,
    /// The newest tool call counted.
    seq: i64,
}

impl NextFiles {
    /// Counts an edit of `file` in `session`.
    pub fn edit(&mut self, session: &str, file: &str) {
        let r = self.recent.entry(session.to_string()).or_default();
        if r.back().is_some_and(|f| f == file) {
            return;
        }
        // The nearest edit of each earlier file gives its gap.
        let mut seen: Vec<&str> = Vec::new();
        for (g, a) in r.iter().rev().enumerate() {
            if a != file && !seen.contains(&a.as_str()) {
                seen.push(a);
                *self.next.entry(a.clone()).or_default().entry(file.to_string()).or_default() += 1.0 / (g + 1) as f64;
            }
        }
        r.push_back(file.to_string());
        if r.len() > GAP {
            r.pop_front();
        }
    }

    /// Counts the edits stored since the last call.
    pub fn update(&mut self, store: &Store) -> Result<()> {
        let tools: Vec<String> = EDITS.iter().map(|t| format!("'{t}'")).collect();
        let sql = format!("SELECT session, seq, files FROM memory WHERE kind = 'tool_call' AND tag IN ({}) AND seq > :seq ORDER BY seq ASC LIMIT 1000000000", tools.join(", "));
        let rs = store.query_with(&sql, &Params::new().set("seq", Value::Int(self.seq)))?;
        for r in &rs.rows {
            let (Some(session), Some(seq), Some(file)) = (r[0].as_str(), r[1].as_i64(), r[2].as_str()) else { continue };
            // An edit's record names its one file.
            if !file.is_empty() && !file.contains(' ') {
                self.edit(session, file);
            }
            self.seq = self.seq.max(seq);
        }
        Ok(())
    }

    /// The `n` files edited after `file` with the most weight, at least
    /// `min`, heaviest first.
    pub fn after(&self, file: &str, n: usize, min: f64) -> Vec<(String, f64)> {
        let mut out: Vec<(String, f64)> = self.next.get(file).map(|m| m.iter().filter(|(_, w)| **w >= min).map(|(f, w)| (f.clone(), *w)).collect()).unwrap_or_default();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.truncate(n);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::record::{Draft, Kind};
    use crate::store::StoreOptions;
    use std::sync::Arc;

    #[test]
    fn counts_files_edited_next() {
        let mut nf = NextFiles::default();
        // a, a (once), b, c, d: b after a at gap 1, c at 2, d at 3 and
        // past it nothing.
        for f in ["a", "a", "b", "c", "d", "e"] {
            nf.edit("s1", f);
        }
        assert_eq!(nf.after("a", 5, 0.0), vec![("b".into(), 1.0), ("c".into(), 0.5), ("d".into(), 1.0 / 3.0)]);
        // A file edited twice in the window counts at its nearest gap.
        for f in ["x", "y", "x", "z"] {
            nf.edit("s2", f);
        }
        assert_eq!(nf.after("x", 5, 0.0), vec![("y".into(), 1.0), ("z".into(), 1.0)]);
        assert_eq!(nf.after("y", 5, 0.0), vec![("x".into(), 1.0), ("z".into(), 0.5)]);
        // Sessions do not run into each other; weights add up across them.
        nf.edit("s3", "b");
        assert_eq!(nf.after("e", 5, 0.0), vec![]);
        nf.edit("s3", "a");
        nf.edit("s4", "a");
        nf.edit("s4", "b");
        assert_eq!(nf.after("a", 1, 0.0), vec![("b".into(), 2.0)]);
        assert_eq!(nf.after("a", 5, 0.6), vec![("b".into(), 2.0)]);
    }

    #[test]
    fn counts_edits_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("m.strata"), Arc::new(HashEmbedder { dim: 32 }), StoreOptions { sync: false, ..Default::default() }).unwrap();
        let call = |tool: &str, file: &str| Draft::new(Kind::ToolCall, format!("{tool} {file}")).tag(tool).files(vec![file.into()]);
        s.append("s1", 1, 100, vec![call("Edit", "src/a.rs"), call("Read", "src/z.rs"), call("Edit", "src/b.rs"), call("Bash", "src/a.rs")]).unwrap();
        let mut nf = NextFiles::default();
        nf.update(&s).unwrap();
        assert_eq!(nf.after("src/a.rs", 2, 0.0), vec![("src/b.rs".into(), 1.0)]);
        // Only what was stored since is read.
        s.append("s1", 2, 200, vec![call("Write", "docs/a.md"), call("MultiEdit", "src/a.rs")]).unwrap();
        nf.update(&s).unwrap();
        assert_eq!(nf.after("src/a.rs", 2, 0.0), vec![("src/b.rs".into(), 1.0), ("docs/a.md".into(), 0.5)]);
        assert_eq!(nf.after("docs/a.md", 2, 0.0), vec![("src/a.rs".into(), 1.0)]);
        nf.update(&s).unwrap();
        assert_eq!(nf.after("src/a.rs", 2, 0.0).len(), 2);
    }
}
