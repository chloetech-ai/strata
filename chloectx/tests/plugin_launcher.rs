//! The plugin's launcher (`plugin/scripts/chloectx-launch`), run as Claude
//! Code runs it, against a release laid out as the release workflow lays
//! it out (in a folder, read through `file://`), with a stand-in chloectx:
//! it installs the prebuilt program only when its SHA-256 matches, fetches
//! the model, runs the program with the hook's arguments and its settings,
//! prefers a chloectx already on PATH, says why it could not set up, and
//! replaces a program it downloaded when the plugin moves to another
//! version. Then the real program: the plugin's hooks step aside where the
//! user settings hold `chloectx install --user`'s.

#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn launcher() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin/scripts/chloectx-launch")
}

/// This machine's archive name, as the launcher and the release name it.
fn platform() -> String {
    let os = if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let arch = match std::env::consts::ARCH {
        "aarch64" if os == "macos" => "arm64",
        "aarch64" => "aarch64",
        a => a,
    };
    format!("{os}-{arch}")
}

fn write_exe(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stand-in chloectx that says how it was run.
fn fake_chloectx(label: &str, log: &Path) -> String {
    format!(
        "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'chloectx {label}' ;;\n  fetch-model) echo 'fetch-model {label}' >> '{}' ;;\n  *) cat > /dev/null; echo \"{label} ran: $* plugin=$CHLOECTX_PLUGIN model=$CHLOECTX_DISTILL_MODEL\" ;;\nesac\n",
        log.display()
    )
}

/// A release folder holding chloectx-<platform>.tar.gz of version `ver`,
/// with its .sha256 (or a wrong one).
fn release(dir: &Path, ver: &str, program: &str, right_sum: bool) {
    let name = format!("chloectx-{}", platform());
    let stage = dir.join("stage").join(ver);
    write_exe(&stage.join(&name).join("chloectx"), program);
    let rel = dir.join(format!("chloedb-server-v{ver}"));
    std::fs::create_dir_all(&rel).unwrap();
    let tgz = rel.join(format!("{name}.tar.gz"));
    assert!(Command::new("tar").arg("-C").arg(&stage).arg("-czf").arg(&tgz).arg(&name).status().unwrap().success());
    let sum = if right_sum { sha256(&tgz) } else { "0".repeat(64) };
    std::fs::write(rel.join(format!("{name}.tar.gz.sha256")), format!("{sum}  {name}.tar.gz\n")).unwrap();
}

fn sha256(p: &Path) -> String {
    let out = Command::new("sh").arg("-c").arg("sha256sum \"$1\" 2>/dev/null || shasum -a 256 \"$1\"").arg("sh").arg(p).output().unwrap();
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().to_string()
}

struct Env {
    _t: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    releases: PathBuf,
    log: PathBuf,
    path: String,
}

fn env() -> Env {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let data = t.path().join("plugin-data");
    let releases = t.path().join("releases");
    let log = t.path().join("calls.log");
    // The system's tools, and no chloectx (nor cargo, in most places; the
    // build is turned off anyway).
    let path = "/usr/bin:/bin:/usr/sbin:/sbin".to_string();
    Env { home, data, releases, log, path, _t: t }
}

impl Env {
    fn run(&self, args: &[&str], stdin: &str, extra: &[(&str, &str)]) -> (i32, String, String) {
        let mut c = Command::new(launcher());
        c.args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .env("CLAUDE_PLUGIN_DATA", &self.data)
            .env("CHLOECTX_RELEASES_URL", format!("file://{}", self.releases.display()))
            .env("CHLOECTX_BUILD", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn installed(&self) -> PathBuf {
        self.home.join(".local/bin/chloectx")
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, f: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}; setup.log: {}", std::fs::read_to_string(self.data.join("setup.log")).unwrap_or_default());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn a_download_whose_sha256_is_wrong_is_not_installed() {
    let e = env();
    release(&e.releases, VERSION, &fake_chloectx("released", &e.log), false);
    let (code, _, err) = e.run(&["setup"], "", &[]);
    assert_ne!(code, 0);
    assert!(err.contains("SHA-256"), "{err}");
    assert!(!e.installed().exists(), "nothing installed");
    let failed = std::fs::read_to_string(e.data.join("failed")).unwrap();
    assert!(failed.contains("no prebuilt chloectx") && failed.contains("chloectx-setup.html"), "{failed}");
    assert!(!e.data.join("setup.lock").exists(), "the lock is released");
}

#[test]
fn the_release_is_installed_checked_and_run_with_the_plugins_settings() {
    let e = env();
    release(&e.releases, VERSION, &fake_chloectx("released", &e.log), true);
    let (code, out, err) = e.run(&["setup"], "", &[]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(e.installed().is_file());
    assert!(e.calls().contains("fetch-model released"), "the model is fetched once set up");
    assert_eq!(e.run(&["where"], "", &[]).1.trim(), e.installed().to_string_lossy());
    let ready = std::fs::read_to_string(e.data.join("ready")).unwrap();
    assert_eq!(ready.trim(), format!("downloaded {VERSION} {VERSION} {}", e.installed().display()));

    // A hook runs the program with its arguments, CHLOECTX_PLUGIN=1 and
    // the distillation model's default, or the session's own.
    let (code, out, _) = e.run(&["hook", "--user", "Stop"], "{\"hook_event_name\":\"Stop\"}", &[]);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "released ran: hook --user Stop plugin=1 model=haiku");
    let (_, out, _) = e.run(&["hook", "--user", "Stop"], "{}", &[("CHLOECTX_DISTILL_MODEL", "sonnet")]);
    assert_eq!(out.trim(), "released ran: hook --user Stop plugin=1 model=sonnet");
    let (_, out, _) = e.run(&["mcp"], "", &[]);
    assert_eq!(out.trim(), "released ran: mcp plugin=1 model=haiku");
}

#[test]
fn a_chloectx_on_path_is_used_and_nothing_is_downloaded() {
    let mut e = env();
    let own = e.home.join("tools");
    write_exe(&own.join("chloectx"), &fake_chloectx("own", &e.log));
    e.path = format!("{}:{}", own.display(), e.path);
    // No release at all: it must not be needed.
    let (code, out, _) = e.run(&["hook", "--user", "SessionStart"], "{}", &[]);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "own ran: hook --user SessionStart plugin=1 model=haiku", "the session start waits for a setup that only fetches the model");
    assert!(!e.installed().exists());
    assert_eq!(e.calls().trim(), "fetch-model own");
    assert!(std::fs::read_to_string(e.data.join("ready")).unwrap().starts_with("found "));
}

#[test]
fn until_it_is_set_up_hooks_record_nothing_and_the_session_start_says_why() {
    let e = env();
    // No release has this program (and building is off).
    let (code, out, _) = e.run(&["hook", "--user", "SessionStart"], "{}", &[]);
    assert_eq!(code, 0, "a hook never fails the session");
    let msg: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{e}: {out}"));
    let text = msg["systemMessage"].as_str().unwrap();
    assert!(text.contains("could not set itself up") && text.contains("not recorded"), "{text}");
    // Other hooks say nothing and do not try again.
    let (code, out, _) = e.run(&["hook", "--user", "PostToolUse"], "{}", &[]);
    assert_eq!((code, out.as_str()), (0, ""));
    assert!(!e.data.join("setup.lock").exists());
    // The MCP server fails, saying why.
    let (code, _, err) = e.run(&["mcp"], "", &[("CHLOECTX_PLUGIN_WAIT", "2")]);
    assert_eq!(code, 1);
    assert!(err.contains("could not set itself up"), "{err}");

    // Once the release is there, the next session's start sets it up.
    release(&e.releases, VERSION, &fake_chloectx("released", &e.log), true);
    let (_, out, _) = e.run(&["hook", "--user", "SessionStart"], "{}", &[]);
    assert_eq!(out.trim(), "released ran: hook --user SessionStart plugin=1 model=haiku");
    assert!(!e.data.join("failed").exists());
}

#[test]
fn a_downloaded_program_follows_the_plugins_version() {
    let e = env();
    release(&e.releases, "0.0.1", &fake_chloectx("old", &e.log), true);
    assert_eq!(e.run(&["setup"], "", &[("CHLOECTX_VERSION", "0.0.1")]).0, 0);
    let (_, out, _) = e.run(&["hook", "--user", "Stop"], "{}", &[("CHLOECTX_VERSION", "0.0.1")]);
    assert!(out.starts_with("old ran"), "{out}");

    // The plugin is at VERSION now: the old program runs while the new one
    // is downloaded in the background.
    release(&e.releases, VERSION, &fake_chloectx("new", &e.log), true);
    let (_, out, _) = e.run(&["hook", "--user", "Stop"], "{}", &[]);
    assert!(out.starts_with("old ran"), "{out}");
    e.wait_for("the new version", || std::fs::read_to_string(e.data.join("ready")).is_ok_and(|r| r.starts_with(&format!("downloaded {VERSION} "))));
    let (_, out, _) = e.run(&["hook", "--user", "Stop"], "{}", &[]);
    assert!(out.starts_with("new ran"), "{out}");
}

#[test]
fn without_a_prebuilt_program_it_is_built_from_a_checkout() {
    let mut e = env();
    let checkout = e.home.join("checkout");
    std::fs::create_dir_all(checkout.join("chloectx")).unwrap();
    std::fs::write(checkout.join("chloectx/Cargo.toml"), "").unwrap();
    // A stand-in for the repository's install.sh: `--no-wire --prefix DIR`.
    let built = e.home.join("built-chloectx");
    write_exe(&built, &fake_chloectx("built", &e.log));
    write_exe(&checkout.join("install.sh"), &format!("#!/bin/sh\necho \"install.sh $*\" >> '{}'\nmkdir -p \"$3/bin\" && cp '{}' \"$3/bin/chloectx\"\n", e.log.display(), built.display()));
    let tools = e.home.join("tools");
    write_exe(&tools.join("cargo"), "#!/bin/sh\n");
    e.path = format!("{}:{}", tools.display(), e.path);
    let source = checkout.to_string_lossy().into_owned();
    let (code, out, err) = e.run(&["setup"], "", &[("CHLOECTX_BUILD", "1"), ("CHLOECTX_SOURCE", &source)]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("could not download") && err.contains("building chloectx from"), "{err}");
    assert!(e.calls().contains(&format!("install.sh --no-wire --prefix {}", e.home.join(".local").display())), "{}", e.calls());
    let (_, out, _) = e.run(&["hook", "--user", "Stop"], "{}", &[]);
    assert_eq!(out.trim(), "built ran: hook --user Stop plugin=1 model=haiku");
}

/// The real program, run by the launcher's environment: it steps aside where
/// the user settings hold the hooks of `chloectx install --user`, and records
/// where they do not.
#[test]
fn the_plugins_hooks_step_aside_for_a_user_install() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let project = t.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let ctx_home = t.path().join("ctx");
    let exe = env!("CARGO_BIN_EXE_chloectx");
    let hook = |event: &str| {
        let mut c = Command::new(exe);
        c.args(["hook", "--user", event])
            .env("HOME", &home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env("CHLOECTX_HOME", &ctx_home)
            .env("CHLOECTX_EMBEDDER", "hash:64")
            .env("CHLOECTX_DISTILL", "0")
            .env("CHLOECTX_DEEP", "off")
            .env("CHLOECTX_PLUGIN", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let payload = serde_json::json!({"hook_event_name": event, "session_id": "plug-1", "cwd": project, "source": "startup"});
        child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let store = ctx_home.join("projects").join(chloectx::paths::project_slug(&project));

    // `chloectx install --user` in place: the plugin's hook does nothing.
    let mut settings = serde_json::json!({});
    chloectx::install::add_to_settings(&mut settings, "/opt/chloectx", true, None);
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/settings.json"), settings.to_string()).unwrap();
    let out = hook("SessionStart");
    assert!(out.stdout.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(!store.exists(), "no store for a hook that stepped aside");

    // Without it, the plugin's hook records.
    std::fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    let out = hook("SessionStart");
    assert!(out.status.success());
    assert!(store.join("memory.strata").exists(), "{}", String::from_utf8_lossy(&out.stderr));
    let _ = Command::new(exe).args(["stop"]).current_dir(&project).env("CHLOECTX_HOME", &ctx_home).env("CLAUDE_PROJECT_DIR", &project).output();
}
