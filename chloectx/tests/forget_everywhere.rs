//! `chloectx forget --everywhere`: one command finds a fact in every store
//! `chloectx stores` lists, by its text (in a record's text or its context,
//! whatever the case) or by record id, lists the matches, asks first (or
//! takes --yes; --dry-run only lists), and forgets them with each store's
//! own forget. The real program, with stores in a home of the test's own
//! and the hash embedder.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Homes {
    t: tempfile::TempDir,
}

impl Homes {
    fn new() -> Homes {
        let t = tempfile::tempdir().unwrap();
        for d in ["ctx", "a", "b", "c"] {
            std::fs::create_dir_all(t.path().join(d)).unwrap();
        }
        Homes { t }
    }

    fn project(&self, p: &str) -> PathBuf {
        self.t.path().join(p).canonicalize().unwrap()
    }

    fn run(&self, project: &str, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_chloectx"))
            .args(args)
            .current_dir(self.project(project))
            .env("HOME", self.t.path())
            .env("CHLOECTX_HOME", self.t.path().join("ctx"))
            .env("CHLOECTX_EMBEDDER", "hash:64")
            .env("CHLOECTX_DISTILL", "0")
            .env("CHLOECTX_DEEP", "off")
            .env("CHLOECTX_RECEIPTS", "0")
            .env_remove("CLAUDE_PROJECT_DIR")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn remember(&self, project: &str, text: &str) -> String {
        let (code, out, err) = self.run(project, &["remember", "fact", text]);
        assert_eq!(code, 0, "{out}{err}");
        // "Stored fact as <id>."
        out.split_whitespace().last().unwrap().trim_end_matches('.').to_string()
    }

    fn count(&self, project: &str, needle: &str) -> usize {
        let (_, out, _) = self.run(project, &["sql", &format!("SELECT id FROM memory WHERE text LIKE '%{needle}%'")]);
        out.lines().filter(|l| l.contains(':') && !l.contains("id")).count()
    }
}

impl Drop for Homes {
    fn drop(&mut self) {
        let _ = self.run("a", &["stop", "--all"]);
    }
}

#[test]
fn a_fact_is_found_in_every_store_and_forgotten_once_asked() {
    let h = Homes::new();
    h.remember("a", "The staging codeword is Zebra-Quartz-77, changed monthly");
    h.remember("a", "Unrelated: the build uses cargo nextest");
    h.remember("b", "The demo banner still shows zebra-quartz-77 too");
    let keep = h.remember("c", "Nothing to see in project c");

    // Too short to look for.
    assert_eq!(h.run("a", &["forget", "--everywhere", "ze"]).0, 2);
    // Listed, whatever the case, in both stores; nothing changed.
    let (code, out, err) = h.run("c", &["forget", "--everywhere", "--dry-run", "ZEBRA-quartz-77"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains(&h.project("a").display().to_string()) && out.contains(&h.project("b").display().to_string()), "{out}");
    assert!(!out.contains(&format!("{} (", h.project("c").display())), "c holds no match: {out}");
    assert!(out.contains("Would forget 2 records in 2 stores. Nothing was changed."), "{out}");
    assert_eq!(h.count("a", "Zebra"), 1);
    // Without a terminal and without --yes it does not ask, and changes nothing.
    let (_, out, _) = h.run("c", &["forget", "--everywhere", "zebra-quartz-77"]);
    assert!(out.contains("Run it again with --yes"), "{out}");
    assert_eq!(h.count("b", "zebra"), 1);

    let (code, out, err) = h.run("c", &["forget", "--everywhere", "--yes", "zebra-quartz-77"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("Forgot 2 records."), "{out}");
    assert_eq!(h.count("a", "Zebra"), 0);
    assert_eq!(h.count("b", "zebra"), 0);
    assert_eq!(h.count("a", "nextest"), 1, "the other records stay");
    let (_, out, _) = h.run("a", &["forget", "--everywhere", "--dry-run", "zebra-quartz-77"]);
    assert!(out.contains("No record in any of the stores holds"), "{out}");

    // By id, wherever its store is.
    let (code, out, _) = h.run("a", &["forget", "--everywhere", "--yes", &keep]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("Deleted {keep}")), "{out}");
    assert_eq!(h.count("c", "project c"), 0);
    assert_eq!(h.count("a", "nextest"), 1, "ids repeat across stores; only the one that exists went");
    // Every store is still there; none was created for a folder without one.
    let (_, out, _) = h.run("a", &["stores"]);
    assert!(out.contains("\n3 stores, "), "{out}");
    assert!(Path::new(env!("CARGO_BIN_EXE_chloectx")).exists());
}
