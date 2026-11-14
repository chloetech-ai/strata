//! What `chloectx install` writes and what `fetch-model` keeps: B-11 (hook
//! commands that a shell runs whatever the path), B-15 (settings rewritten
//! whole, in their order, with a backup) and B-14 (a model checked against
//! its pinned SHA-256, never left half downloaded).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use chloectx::install;
use serde_json::{json, Value as Json};

fn read(path: &Path) -> Json {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A stand-in chloectx in `dir` that answers `--version` as clap does.
fn fake_chloectx(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let exe = dir.join("chloectx");
    std::fs::write(&exe, "#!/bin/sh\n[ \"$1\" = --version ] && echo 'chloectx 0.2.0'\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe.to_string_lossy().into_owned()
}

/// B-11: `./install.sh --prefix "/Users/jo/My Tools"`, or a home with a
/// space or a quote in it: every hook command still runs the binary.
#[test]
fn hook_commands_run_from_a_path_with_spaces_and_quotes() {
    let t = tempfile::tempdir().unwrap();
    for sub in ["My Tools/bin", "jo's (work) $HOME/bin", "plain/bin"] {
        let exe = fake_chloectx(&t.path().join(sub));
        for user in [false, true] {
            let hooks = install::hooks(&exe, user);
            for (event, entries) in hooks.as_object().unwrap() {
                let cmd = entries[0]["hooks"][0]["command"].as_str().unwrap();
                assert_eq!(install::hook_exe(cmd).as_deref(), Some(exe.as_str()), "{event}: {cmd}");
                chloectx::doctor::runs_in_a_shell(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"));
                // And the shell passes the event as the hook's last word.
                let probe = cmd.replacen(" hook", " --version #", 1);
                let out = std::process::Command::new("sh").arg("-c").arg(&probe).output().unwrap();
                assert!(out.status.success(), "{probe}: {}", String::from_utf8_lossy(&out.stderr));
            }
        }
    }
    // The commands of an install before the quoting, still recognised (so
    // a new install replaces them), and still reported as broken.
    let old = format!("{} hook Stop", fake_chloectx(&t.path().join("Old Tools/bin")));
    assert!(install::hook_exe(&old).is_some());
    assert!(chloectx::doctor::runs_in_a_shell(&old).is_err(), "{old} ran");
}

/// B-15: a project install keeps the settings' own order, backs up what it
/// rewrites, leaves no temporary file behind, and keeps a link a link.
#[test]
fn a_project_install_rewrites_settings_whole_in_their_order_with_a_backup() {
    let t = tempfile::tempdir().unwrap();
    let project = t.path().join("proj");
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    // The settings live elsewhere (a dotfiles folder) and are linked in.
    let real = t.path().join("dotfiles/settings.local.json");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    let original = "{\n  \"zeta\": 1,\n  \"permissions\": {\"deny\": [\"Bash(rm:*)\"], \"allow\": [\"Bash(ls:*)\"]},\n  \"alpha\": {\"y\": 1, \"x\": 2}\n}\n";
    std::fs::write(&real, original).unwrap();
    let link = project.join(".claude/settings.local.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::fs::write(project.join(".mcp.json"), "{\"mcpServers\": {\"zz\": {\"command\": \"z\"}, \"aa\": {\"command\": \"a\"}}}\n").unwrap();

    let msg = install::install(&project, "/bin/chloectx", false, None).unwrap();

    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link was replaced by a file");
    let text = std::fs::read_to_string(&real).unwrap();
    let at = |k: &str| text.find(&format!("\"{k}\"")).unwrap_or_else(|| panic!("{k} missing: {text}"));
    assert!(at("zeta") < at("permissions") && at("permissions") < at("alpha") && at("alpha") < at("hooks"), "keys re-sorted: {text}");
    assert!(at("deny") < at("allow") && at("y") < at("x"), "nested keys re-sorted: {text}");
    let mcp = std::fs::read_to_string(project.join(".mcp.json")).unwrap();
    assert!(mcp.find("\"zz\"").unwrap() < mcp.find("\"aa\"").unwrap() && mcp.find("\"aa\"").unwrap() < mcp.find("\"chloectx\"").unwrap(), "{mcp}");

    // Both files were backed up before they changed, and the message says where.
    let backups = |dir: &Path, prefix: &str| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix) && p.extension().map_or(false, |e| e == "bak")).collect()
    };
    let saved = backups(&project.join(".claude"), "settings.local.json.chloectx-");
    assert_eq!(saved.len(), 1, "{msg}");
    assert_eq!(std::fs::read_to_string(&saved[0]).unwrap(), original);
    assert_eq!(backups(&project, ".mcp.json.chloectx-").len(), 1, "{msg}");
    assert!(msg.contains("previous versions saved as"), "{msg}");
    // Nothing temporary left over, next to the link or the file.
    for dir in [project.join(".claude"), real.parent().unwrap().to_path_buf(), project.clone()] {
        for e in std::fs::read_dir(&dir).unwrap() {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            assert!(!n.ends_with(".tmp"), "{n} left in {}", dir.display());
        }
    }
    // The hooks went in, and uninstall takes them out, backing up again.
    assert_eq!(read(&real)["permissions"]["allow"], json!(["Bash(ls:*)", "mcp__chloectx"]));
    let msg = install::uninstall(&project).unwrap();
    assert!(msg.contains("previous versions saved as"), "{msg}");
    let after = read(&real);
    assert_eq!(after, serde_json::from_str::<Json>(original).unwrap());
}

/// B-14: a download cut short, or a file changed upstream, is refused and
/// never stands where the model is read; the pinned bytes are kept.
#[test]
fn a_model_is_kept_only_with_its_pinned_bytes() {
    use chloectx::embed::{fetch_pinned, file_sha256, Fetched};
    use chloedb::model_pins::Pin;

    let t = tempfile::tempdir().unwrap();
    let site = t.path().join("site");
    let rev = "0123456789abcdef";
    let served = site.join("owner/model/resolve").join(rev);
    std::fs::create_dir_all(&served).unwrap();
    std::fs::write(served.join("config.json"), "{\"dim\": 4}").unwrap();
    std::fs::write(served.join("model.safetensors"), vec![7u8; 200_000]).unwrap();
    let good = |f: &str| file_sha256(&served.join(f)).unwrap();
    let pin = |weights: String| Pin {
        repo: "owner/model",
        revision: rev,
        files: Box::leak(vec![("config.json", &*Box::leak(good("config.json").into_boxed_str())), ("model.safetensors", &*Box::leak(weights.into_boxed_str()))].into_boxed_slice()),
    };
    let base = format!("file://{}", site.display());
    let dir = t.path().join("models/model");
    let files = ["config.json", "model.safetensors"];

    // Bytes that are not the pinned ones (changed upstream, or cut short).
    let wrong = pin("00".repeat(32));
    let e = fetch_pinned(&base, "owner/model", Some(&wrong), &dir, &files).unwrap_err();
    assert!(e.to_string().contains("SHA-256"), "{e}");
    assert!(!dir.join("model.safetensors").exists(), "a file that failed its check is where the model is read");
    assert!(!dir.join("model.safetensors.part").exists());

    // A partial file left by an earlier download is replaced.
    std::fs::write(dir.join("model.safetensors"), vec![7u8; 60_000]).unwrap();
    let right = pin(good("model.safetensors"));
    let done = fetch_pinned(&base, "owner/model", Some(&right), &dir, &files).unwrap();
    assert_eq!(done, vec![Fetched::Kept("config.json".into()), Fetched::Downloaded("model.safetensors".into())]);
    assert_eq!(std::fs::read(dir.join("model.safetensors")).unwrap().len(), 200_000);

    // Run again: nothing to download.
    let done = fetch_pinned(&base, "owner/model", Some(&right), &dir, &files).unwrap();
    assert!(done.iter().all(|f| matches!(f, Fetched::Kept(_))), "{done:?}");

    // The model chloectx fetches is pinned, every file of it.
    for &(_, repo, _, files) in chloectx::embed::FETCHED {
        let p = chloedb::model_pins::pin(repo).unwrap_or_else(|| panic!("{repo} has no pin"));
        assert!(files.iter().all(|f| p.sha256(f).is_some()), "{repo}");
    }
}

/// CTX-9: a mistyped project path is refused, not created. CTX-12: in a git
/// repository, the two files that name this machine's binary are kept out
/// of `git add` (in `.git/info/exclude`), and a tracked `.mcp.json` is
/// called out.
#[test]
fn a_project_install_checks_the_folder_and_keeps_its_files_out_of_git() {
    let t = tempfile::tempdir().unwrap();
    let missing = t.path().join("no/such/dir");
    let e = install::install(&missing, "/bin/chloectx", false, None).unwrap_err().to_string();
    assert!(e.contains("is not a folder") && e.contains(&t.path().display().to_string()), "{e}");
    assert!(!t.path().join("no").exists(), "the mistyped path was created");

    let git = |dir: &Path, args: &[&str]| {
        let st = std::process::Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").output();
        st.map(|o| (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned())).unwrap_or((false, String::new()))
    };
    let repo = t.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    if !git(&repo, &["init", "-q"]).0 {
        return; // no git here
    }
    std::fs::write(repo.join("README"), "x\n").unwrap();
    let msg = install::install(&repo, "/bin/chloectx", false, None).unwrap();
    assert!(msg.contains("info/exclude"), "{msg}");
    let (_, status) = git(&repo, &["status", "--porcelain", "--untracked-files=all"]);
    assert!(!status.contains(".mcp.json") && !status.contains("settings.local.json"), "{status}");
    assert!(status.contains("README"), "{status}");
    // Again: nothing added twice.
    install::install(&repo, "/bin/chloectx", false, None).unwrap();
    let ex = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert_eq!(ex.matches("/.mcp.json").count(), 1, "{ex}");

    // A tracked .mcp.json is said to be tracked.
    let repo2 = t.path().join("repo2");
    std::fs::create_dir_all(&repo2).unwrap();
    git(&repo2, &["init", "-q"]);
    std::fs::write(repo2.join(".mcp.json"), "{\"mcpServers\": {}}\n").unwrap();
    git(&repo2, &["add", ".mcp.json"]);
    git(&repo2, &["commit", "-q", "-m", "mcp"]);
    let msg = install::install(&repo2, "/bin/chloectx", false, None).unwrap();
    assert!(msg.contains(".mcp.json is tracked by git"), "{msg}");
}
