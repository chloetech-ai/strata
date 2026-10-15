#!/usr/bin/env python3
"""Compares memory systems for coding agents on multi-session scenarios.

For each lane (a memory system, or none), Claude Code works through the
scenario's history with that memory system active: each history block is a
new session, as separate working days are. Teammate commits the scenario
lists land between blocks without the agent being told. The lane's state
(repository and the whole HOME, which holds ~/.claude, Claude Code's auto
memory, plugins and every memory system's data) is then snapshotted. Each
follow-up task restores that snapshot and starts a new session with the
task's prompt; the 'full' lane instead forks the one long history session.
Each run is graded by the scenario's grader and its cost, steps, time,
tokens, tool calls and memory-system activity are recorded.

Lanes (LANES below; add rows there, or derive one on the command line: a
name 'BASE+anything' is BASE with the --lane-env/--lane-bin given for it):

  none          no memory: auto memory off, no plugins, no hooks
  ccmem         Claude Code's built-in auto memory, on (its default)
  transcripts   history as in 'none'; runs get the history sessions'
                transcripts as markdown in a folder (--add-dir) and a system
                prompt saying they can be grepped
  claude-mem    the claude-mem plugin (eval/competitors/claude_mem)
  mem0          mem0 behind a small MCP server (eval/competitors/mem0)
  engram@main   Engram, the binary built from main
  engram        Engram, the development build (binary is a lane parameter);
                its query engine in its default mode (ENGRAM_DEEP=async)
  engram+deep   the same with ENGRAM_DEEP=sync (also engram+async and
                engram+nodeep, ENGRAM_DEEP=off)
  full          upper bound: the whole history in one session, each task a
                fork of it

Isolation: every `claude` call (and so every hook, daemon and worker it
starts) runs with only PATH, HOME=<the lane's own home>,
ANTHROPIC_BASE_URL and the lane's own variables. PATH is the lane's
~/.local/bin, a directory of links to claude/node/npm/bun/uv
(<work>/bin) and /usr/local/bin:/usr/bin:/bin. Lanes run in parallel;
runs within a lane run one after another (each restores the snapshot in
place).

  python3 scripts/memory_bench.py all --scenario stockroom --lanes none ccmem engram@main --repeats 3
  python3 scripts/memory_bench.py history --scenario stockroom --lanes claude-mem
  python3 scripts/memory_bench.py run --scenario bakehouse --lanes engram+tool --lane-env engram+tool=ENGRAM_DEEP=tool
  python3 scripts/memory_bench.py report --scenario stockroom

`run` records the history first where it is missing; `all` is `run` then
`report`. Results go to --results (default results/memory_bench/<scenario>/):
runs.jsonl (one row per run), history_<lane>.json, patches/<lane>/,
summary.json and summary.md. Work (repositories, homes, snapshots, logs) goes
to --work/<scenario>/<lane>/.
"""

import argparse
import concurrent.futures as cf
import glob
import hashlib
import importlib.util
import json
import os
import random
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.request
import uuid
import zlib
from collections import defaultdict
from datetime import datetime

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import ab_eval as ab  # noqa: E402
import ab_stats  # noqa: E402

ROOT = ab.ROOT
COMPETITORS = os.path.join(ROOT, "eval", "competitors")
BASE_TOOLS = ["Bash", "Edit", "Write", "Read", "Glob", "Grep", "MultiEdit", "TodoWrite"]
LINKED_TOOLS = ("claude", "node", "npm", "npx", "bun", "bunx", "uv", "uvx")
SYSTEM_PATH = "/usr/local/bin:/usr/bin:/bin"

TRANSCRIPTS_PROMPT = (
    "Transcripts of earlier work sessions on this project are in {dir} (one markdown file per session, "
    "oldest first: the user's requests, your replies, tool calls and short results). They can be searched "
    "with grep (or the Grep tool) when earlier requests, decisions or conventions may matter."
)

# kind: plain | transcripts | full | engram | claude-mem | mem0
# auto_memory: leave Claude Code's auto memory on (off otherwise)
# env / history_env / run_env: extra variables (all calls / history only / task runs only)
LANES = {
    "none": {"kind": "plain"},
    "ccmem": {"kind": "plain", "auto_memory": True},
    "transcripts": {"kind": "transcripts"},
    "claude-mem": {"kind": "claude-mem", "version": "13.39.0", "mcp": ["mcp__plugin_claude-mem_mcp-search"]},
    "mem0": {"kind": "mem0", "mcp": ["mcp__mem0"]},
    "engram@main": {"kind": "engram", "binary": "/home/claude/engram-main/engram/target/release/engram", "mcp": ["mcp__engram"]},
    "engram": {"kind": "engram", "binary": "/home/claude/engram/engram/target/release/engram", "mcp": ["mcp__engram"]},
    # The development build's query engine ("deep" recall): async is its default mode.
    "engram+deep": {"kind": "engram", "binary": "/home/claude/engram/engram/target/release/engram", "mcp": ["mcp__engram"], "env": {"ENGRAM_DEEP": "sync"}},
    "engram+async": {"kind": "engram", "binary": "/home/claude/engram/engram/target/release/engram", "mcp": ["mcp__engram"], "env": {"ENGRAM_DEEP": "async"}},
    "engram+nodeep": {"kind": "engram", "binary": "/home/claude/engram/engram/target/release/engram", "mcp": ["mcp__engram"], "env": {"ENGRAM_DEEP": "off"}},
    "full": {"kind": "full"},
}
DEFAULT_LANES = ["none", "ccmem", "transcripts", "claude-mem", "mem0", "engram@main", "engram", "engram+deep", "full"]


def slug(name):
    return re.sub(r"[^A-Za-z0-9]+", "_", name).strip("_")


def log(lane, msg):
    print(f"{time.strftime('%H:%M:%S')} [{lane}] {msg}", file=sys.stderr, flush=True)


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# ---------------------------------------------------------------- scenario


def load_scenario(arg):
    """A scenario by name (eval/ab/scenarios/NAME) or directory: its seed,
    history blocks, tasks with grade functions and teammate commits."""
    d = arg if os.path.isdir(arg) else os.path.join(ROOT, "eval", "ab", "scenarios", arg)
    spec = json.load(open(os.path.join(d, "scenario.json")))
    graders = load_module(os.path.join(d, "graders.py"), f"graders_{slug(spec['name'])}")
    tasks = [dict(t, grade=(lambda repo, answer, tid=t["id"]: graders.grade(tid, repo, answer))) for t in spec["tasks"]]
    for c in spec.get("teammate_commits", []):
        if not 1 <= c["after_block"] <= len(spec["history"]):
            sys.exit(f"teammate commit after_block {c['after_block']} is outside 1..{len(spec['history'])}")
    return {"name": spec["name"], "dir": d, "seed": os.path.join(d, "seed"), "history": spec["history"], "tasks": tasks,
            "teammate_commits": spec.get("teammate_commits", [])}


# ---------------------------------------------------------------- files and processes


def sync_tree(src, dst):
    """Makes dst an exact copy of src, copying only files whose size or
    modification time differ (a restore of a large, mostly unchanged home is
    fast). Symlinks are copied as links; sockets and the like are dropped."""
    if not os.path.isdir(src):
        shutil.rmtree(dst, ignore_errors=True)
        return
    os.makedirs(dst, exist_ok=True)
    have = {e.name: e for e in os.scandir(dst)}
    for e in os.scandir(src):
        d = os.path.join(dst, e.name)
        old = have.pop(e.name, None)
        if e.is_symlink():
            if old is not None and not (old.is_symlink() and os.readlink(d) == os.readlink(e.path)):
                _remove(d, old)
                old = None
            if old is None:
                os.symlink(os.readlink(e.path), d)
        elif e.is_dir(follow_symlinks=False):
            if old is not None and (old.is_symlink() or not old.is_dir(follow_symlinks=False)):
                _remove(d, old)
            sync_tree(e.path, d)
        elif e.is_file(follow_symlinks=False):
            if old is not None:
                if old.is_file(follow_symlinks=False) and not old.is_symlink():
                    s, t = e.stat(follow_symlinks=False), old.stat(follow_symlinks=False)
                    if s.st_size == t.st_size and s.st_mtime_ns == t.st_mtime_ns:
                        continue
                _remove(d, old)
            shutil.copy2(e.path, d, follow_symlinks=False)
    for name, e in have.items():
        _remove(os.path.join(dst, name), e)


def _remove(path, entry):
    if entry.is_dir(follow_symlinks=False) and not entry.is_symlink():
        shutil.rmtree(path, ignore_errors=True)
    else:
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass


def home_procs(home):
    """PIDs of processes whose environment has HOME=home."""
    key = b"HOME=" + home.encode() + b"\0"
    me = {os.getpid(), os.getppid()}
    out = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit() or int(pid) in me:
            continue
        try:
            env = open(f"/proc/{pid}/environ", "rb").read()
        except OSError:
            continue
        if (b"\0" + env).find(b"\0" + key) >= 0:
            out.append(int(pid))
    return out


def cmdline(pid):
    try:
        return open(f"/proc/{pid}/cmdline", "rb").read().replace(b"\0", b" ").decode("utf-8", "replace")
    except OSError:
        return ""


def kill_home_procs(home, grace=3.0):
    """Stops whatever the lane left running (hook children, MCP servers,
    workers): everything with the lane's HOME."""
    pids = home_procs(home)
    for sig in (signal.SIGTERM, signal.SIGKILL):
        for pid in pids:
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        until = time.time() + grace
        while time.time() < until and (pids := [p for p in pids if os.path.exists(f"/proc/{p}") and not _zombie(p)]):
            time.sleep(0.2)
        if not pids:
            return []
    return pids


def _zombie(pid):
    try:
        return open(f"/proc/{pid}/stat").read().split(") ", 1)[1].startswith("Z")
    except (OSError, IndexError):
        return True


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def git(repo, *args, env=None, check=True):
    return subprocess.run(["git", *args], cwd=repo, env=env, capture_output=True, text=True, check=check).stdout


# ---------------------------------------------------------------- transcripts


def transcript_path(home, session):
    paths = glob.glob(os.path.join(home, ".claude", "projects", "*", f"{session}.jsonl"))
    return paths[0] if paths else None


def transcript_events(home, session, since=None):
    """The session's transcript events (with `since`, only those written at
    or after that time: a forked session starts with a copy of its parent)."""
    path = transcript_path(home, session) if session else None
    out = []
    if not path:
        return out
    for line in open(path, encoding="utf-8"):
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if since is not None:
            ts = ev.get("timestamp")
            try:
                if ts and datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp() < since:
                    continue
            except ValueError:
                pass
        out.append(ev)
    return out


def transcript_stats(events, memory_paths=()):
    """Tool calls, context injected by hooks, auto-memory attachments and
    compactions, from transcript events."""
    tools, path_hits, hook_ctx, attach, compactions, ctx = [], 0, defaultdict(int), defaultdict(int), 0, 0
    for ev in events:
        msg = ev.get("message") or {}
        if ev.get("type") == "assistant" and isinstance(msg.get("content"), list):
            for c in msg["content"]:
                if c.get("type") == "tool_use":
                    tools.append(c.get("name", ""))
                    blob = json.dumps(c.get("input"))
                    path_hits += any(p in blob for p in memory_paths)
            u = msg.get("usage") or {}
            if u:
                ctx = u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)
        elif ev.get("type") == "user" and ev.get("isCompactSummary"):
            compactions += 1
        elif ev.get("type") == "attachment":
            a = ev.get("attachment") or {}
            t = a.get("type", "")
            content = a.get("content")
            size = len(content if isinstance(content, str) else json.dumps(content or ""))
            if t == "hook_additional_context":
                hook_ctx[a.get("hookEvent") or a.get("hookName") or "?"] += size
            elif t == "hook_system_message":
                hook_ctx["system_message"] += size
            elif "memory" in t.lower() or "claude_md" in t.lower() or "nested" in t.lower():
                attach[t] += len(json.dumps(a))
    return {"tools": tools, "memory_path_tool_calls": path_hits, "hook_context_chars": dict(hook_ctx), "memory_attachments": dict(attach),
            "compactions": compactions, "context_tokens": ctx}


def result_cost_log(path, offset=0):
    """Cost of the Claude Code result messages appended to `path` after
    `offset` (cumulative per session, so each session counts its increase)."""
    out = {"calls": 0, "sessions": 0, "cost_usd": 0.0, "models": {}}
    if not path or not os.path.exists(path):
        return out
    last = {}
    with open(path, "rb") as f:
        f.seek(offset)
        for raw in f:
            try:
                ev = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if ev.get("type") != "result":
                continue
            out["calls"] += 1
            sid, cost = ev.get("session_id"), ev.get("total_cost_usd") or 0.0
            prev = last.get(sid)
            out["cost_usd"] += cost - prev if prev is not None and cost >= prev else cost
            last[sid] = cost
            for m, u in (ev.get("modelUsage") or {}).items():
                out["models"][m] = out["models"].get(m, 0) + 1
    out["sessions"] = len(last)
    return out


def fsize(path):
    try:
        return os.path.getsize(path)
    except OSError:
        return 0


# ---------------------------------------------------------------- lanes


def lane_def(name, args):
    if name in LANES:
        d = json.loads(json.dumps(LANES[name]))
    elif "+" in name and name.split("+", 1)[0] in LANES:
        d = json.loads(json.dumps(LANES[name.split("+", 1)[0]]))
    else:
        sys.exit(f"unknown lane {name} (known: {', '.join(LANES)}; or BASE+suffix with --lane-env)")
    for spec in args.lane_env or []:
        lane, kv = spec.split("=", 1)
        if lane == name:
            k, v = kv.split("=", 1)
            d.setdefault("env", {})[k] = v
    for spec in args.lane_bin or []:
        lane, path = spec.split("=", 1)
        if lane == name:
            d["binary"] = path
    return d


class Lane:
    def __init__(self, args, sc, name):
        self.args, self.sc, self.name = args, sc, name
        self.d = lane_def(name, args)
        self.kind = self.d["kind"]
        self.dir = os.path.join(args.work, slug(sc["name"]), slug(name))
        self.home = os.path.join(self.dir, "home")
        self.repo = os.path.join(self.dir, sc["name"])
        self.extra = os.path.join(self.dir, "transcripts")
        self.snap = os.path.join(self.dir, "snap")
        self.logs = os.path.join(self.dir, "logs")
        self.bin = os.path.join(self.dir, "bin")
        self.engram_home = os.path.join(self.home, "engram")
        self.hist_path = os.path.join(args.results, f"history_{slug(name)}.json")
        self.patches = os.path.join(args.results, "patches", slug(name))
        self.meta_path = os.path.join(self.dir, "lane.json")
        self.meta = json.load(open(self.meta_path)) if os.path.exists(self.meta_path) else {}

    # -- environment

    def env(self, phase):
        env = {"PATH": os.pathsep.join([os.path.join(self.home, ".local", "bin"), self.args.bench_bin, SYSTEM_PATH]), "HOME": self.home}
        if os.environ.get("ANTHROPIC_BASE_URL"):
            env["ANTHROPIC_BASE_URL"] = os.environ["ANTHROPIC_BASE_URL"]
        if not self.d.get("auto_memory"):
            env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"
        if self.kind == "engram":
            env.update(ENGRAM_HOME=self.engram_home, ENGRAM_MODELS=self.args.engram_models, ENGRAM_DISTILL_MODEL=self.args.distill_model)
            if phase == "run":
                # A run is graded on its one turn; what Engram would write down after it is never read.
                env["ENGRAM_DISTILL"] = "0"
        env.update(self.meta.get("env", {}))
        env.update(self.d.get("env", {}))
        env.update(self.d.get(f"{phase}_env", {}))
        return env

    def claude_args(self, phase):
        out = ["--allowedTools", ",".join(BASE_TOOLS + self.d.get("mcp", [])), "--disallowedTools", "WebFetch,WebSearch"]
        if self.kind == "transcripts" and phase == "run":
            out += ["--add-dir", self.extra, "--append-system-prompt", TRANSCRIPTS_PROMPT.format(dir=self.extra)]
        if self.kind == "mem0":
            out += ["--mcp-config", self.meta["mcp_config"], "--append-system-prompt", self.meta["system_prompt"]]
        return out

    def memory_paths(self):
        if self.kind == "transcripts":
            return [self.extra]
        if self.d.get("auto_memory"):
            return [os.path.join(self.home, ".claude", "projects")]
        return []

    # -- claude

    def claude(self, prompt, phase, label, session=None, resume=None, fork=False):
        cmd = [os.path.join(self.args.bench_bin, "claude"), "-p", prompt, "--model", self.args.model, "--output-format", "json",
               "--max-budget-usd", str(self.args.budget)] + self.claude_args(phase)
        if session:
            cmd += ["--session-id", session]
        if resume:
            cmd += ["--resume", resume]
        if fork:
            cmd += ["--fork-session"]
        os.makedirs(self.logs, exist_ok=True)
        out_path, err_path = os.path.join(self.logs, f"{label}.json"), os.path.join(self.logs, f"{label}.err")
        t0 = time.time()
        with open(out_path, "w") as fo, open(err_path, "w") as fe:
            p = subprocess.Popen(cmd, cwd=self.repo, env=self.env(phase), stdin=subprocess.DEVNULL, stdout=fo, stderr=fe, start_new_session=True)
            try:
                p.wait(timeout=self.args.timeout)
                timed_out = False
            except subprocess.TimeoutExpired:
                os.killpg(p.pid, signal.SIGKILL)
                p.wait()
                timed_out = True
        text = open(out_path).read()
        try:
            out = json.loads(text) if text.strip().startswith("{") else None
        except json.JSONDecodeError:
            out = None
        if out is None:
            out = {"is_error": True, "result": "timeout" if timed_out else (text + open(err_path).read())[-800:]}
        out["wall_s"] = time.time() - t0
        out["t0"] = t0
        return out

    # -- memory systems

    def setup(self):
        """A fresh repository from the seed and the lane's memory system."""
        shutil.rmtree(self.dir, ignore_errors=True)
        os.makedirs(self.home)
        with open(os.path.join(self.home, ".gitconfig"), "w") as f:
            f.write("[user]\n\tname = Dana\n\temail = dana@example.com\n[init]\n\tdefaultBranch = main\n")
        shutil.copytree(self.sc["seed"], self.repo)
        genv = dict(PATH=SYSTEM_PATH, HOME=self.home)
        git(self.repo, "init", "-q", env=genv)
        git(self.repo, "add", "-A", env=genv)
        git(self.repo, "commit", "-qm", "initial version", env=genv)
        meta = {"lane": self.name, "def": self.d, "env": {}}
        if self.kind == "engram":
            src = self.d["binary"]
            if not os.path.exists(src):
                raise RuntimeError(f"engram binary {src} does not exist")
            os.makedirs(self.bin, exist_ok=True)
            binary = os.path.join(self.bin, "engram")
            shutil.copy2(src, binary)
            meta["engram"] = {"source": src, "sha256": sha256(binary), "version": subprocess.run([binary, "--version"], capture_output=True, text=True).stdout.strip(),
                              "source_rev": subprocess.run(["git", "-C", os.path.dirname(src), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
                              "source_dirty": bool(subprocess.run(["git", "-C", os.path.dirname(src), "status", "--porcelain", "--untracked-files=no"], capture_output=True, text=True).stdout.strip())}
            self.meta = meta
            p = subprocess.run([binary, "install", "--project", self.repo], env=self.env("history"), capture_output=True, text=True)
            if p.returncode:
                raise RuntimeError(f"engram install failed: {p.stdout}{p.stderr}")
            with open(os.path.join(self.repo, ".git", "info", "exclude"), "a") as f:
                f.write(".claude/settings.local.json\n.mcp.json\n")
        elif self.kind == "claude-mem":
            setup = load_module(os.path.join(COMPETITORS, "claude_mem", "setup.py"), "claude_mem_setup")
            info = setup.install(self.home, version=self.d.get("version", setup.VERSION), model=self.args.distill_model,
                                 shared=self.args.shared, path_dirs=[self.args.bench_bin], real_claude=os.path.realpath(os.path.join(self.args.bench_bin, "claude")))
            meta["claude_mem"] = info
            meta["env"].update(info["env"])
        elif self.kind == "mem0":
            setup = load_module(os.path.join(COMPETITORS, "mem0", "setup.py"), "mem0_setup")
            info = setup.install(self.home, shared=self.args.shared, model=self.args.distill_model, bench_bin=self.args.bench_bin)
            meta["mem0"] = info
            meta["env"].update(info["env"])
            meta["mcp_config"], meta["system_prompt"] = info["mcp_config"], info["system_prompt"]
        meta["claude_version"] = subprocess.run([os.path.join(self.args.bench_bin, "claude"), "--version"], capture_output=True, text=True).stdout.strip()
        self.meta = meta
        json.dump(meta, open(self.meta_path, "w"), indent=1)

    def settle(self, phase):
        """Lets the memory system finish what it does after a session (Engram's
        distillation, claude-mem's observer queue), stops it, and stops
        anything else the session left running."""
        info = {}
        if self.kind == "engram":
            t0 = time.time()
            subprocess.run([os.path.join(self.bin, "engram"), "stop", "--project", self.repo], env=self.env(phase), capture_output=True, timeout=60)
            until = time.time() + (self.args.settle_timeout if phase == "history" else 60)
            while time.time() < until and glob.glob(os.path.join(self.engram_home, "projects", "*", "*.sock")):
                time.sleep(1)
            info["engram_stop_s"] = round(time.time() - t0, 1)
        elif self.kind == "claude-mem":
            info.update(self.claude_mem_drain(self.args.settle_timeout if phase == "history" else 120))
            subprocess.run([os.path.join(self.home, ".local", "bin", "claude-mem"), "stop"], env=self.env(phase), cwd=self.repo,
                           stdin=subprocess.DEVNULL, capture_output=True, timeout=120)
        until = time.time() + 30
        while time.time() < until and [p for p in home_procs(self.home) if not _zombie(p)]:
            time.sleep(0.5)
        left = [p for p in home_procs(self.home) if not _zombie(p)]
        if left:
            info["killed"] = [cmdline(p)[:120] for p in left]
            kill_home_procs(self.home)
        return info

    def claude_mem_db(self):
        return os.path.join(self.home, ".claude-mem", "claude-mem.db")

    def claude_mem_query(self, sql):
        try:
            c = sqlite3.connect(f"file:{self.claude_mem_db()}?mode=ro", uri=True, timeout=5)
            try:
                return c.execute(sql).fetchall()
            finally:
                c.close()
        except sqlite3.Error:
            return None

    def claude_mem_status(self):
        """The worker's own view of its queue (/api/processing-status), or None."""
        port = self.meta["claude_mem"]["port"]
        try:
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(f"http://127.0.0.1:{port}/api/processing-status", timeout=5) as r:
                return json.loads(r.read())
        except Exception:  # noqa: BLE001 - worker not running
            return None

    def claude_mem_drain(self, timeout, quiet_s=10):
        """Waits until claude-mem's hooks have exited, its queue (database and
        the worker's in-memory buffer) is empty and its observer has not
        answered anything for quiet_s seconds."""
        t0 = time.time()
        log_path = self.meta["claude_mem"]["observer_log"]
        last_size, last_change = fsize(log_path), time.time()
        while time.time() - t0 < timeout:
            size = fsize(log_path)
            if size != last_size:
                last_size, last_change = size, time.time()
            hooks = [p for p in home_procs(self.home) if " hook " in cmdline(p)]
            pending = self.claude_mem_query("select count(*) from pending_messages where status in ('pending', 'processing')")
            st = self.claude_mem_status()
            busy = bool(hooks) or (pending is not None and pending[0][0] > 0) or (st is not None and (st.get("queueDepth") or st.get("isProcessing")))
            if busy:
                last_change = time.time()
            elif time.time() - last_change >= quiet_s:
                break
            time.sleep(1)
        st = self.claude_mem_status()
        return {"claude_mem_drain_s": round(time.time() - t0, 1), "claude_mem_queue_left": st.get("queueDepth") if st else None}

    def memory_offsets(self):
        offs = {"engram": {p: fsize(p) for p in glob.glob(os.path.join(self.engram_home, "projects", "*", "events.log"))}}
        if self.kind == "claude-mem":
            offs["claude_mem"] = fsize(self.meta["claude_mem"]["observer_log"])
        if self.kind == "mem0":
            offs["mem0"] = fsize(self.meta["mem0"]["llm_log"])
        return offs

    def memory_activity(self, offs, phase):
        """What the memory system did (and cost) since `offs`."""
        out = {"cost_usd": 0.0}
        if self.kind == "engram":
            ev = {"injected_chars": 0, "recalls": 0, "decisions": [], "briefing_chars": 0, "distills": 0, "distill_errors": 0, "events": defaultdict(int),
                  "cost_by_event": defaultdict(float), "errors": defaultdict(int)}
            for path in glob.glob(os.path.join(self.engram_home, "projects", "*", "events.log")):
                with open(path, "rb") as f:
                    f.seek(offs["engram"].get(path, 0))
                    for raw in f:
                        try:
                            e = json.loads(raw)
                        except json.JSONDecodeError:
                            continue
                        name = e.get("event")
                        ev["events"][name] += 1
                        if e.get("error"):
                            ev["errors"][name] += 1
                        # Every model call Engram makes (distill, consolidate, deep, ask, ...) logs its cost.
                        if isinstance(e.get("cost_usd"), (int, float)):
                            ev["cost_by_event"][name] += e["cost_usd"]
                            out["cost_usd"] += e["cost_usd"]
                        if name == "recall":
                            ev["recalls"] += 1
                            ev["decisions"].append(e.get("decision"))
                            ev["injected_chars"] += e.get("chars", 0)
                        elif name == "SessionStart":
                            ev["briefing_chars"] += e.get("chars", 0)
                        elif name == "distill":
                            ev["distills"] += 1
                            ev["distill_errors"] += bool(e.get("error"))
                        elif name == "deep":
                            # The development build's query engine: did it find anything, and how long did it take.
                            ev["deep"] = ev.get("deep") or {"runs": 0, "with_picks": 0, "ms": 0, "modes": {}}
                            ev["deep"]["runs"] += 1
                            ev["deep"]["with_picks"] += bool(e.get("picks"))
                            ev["deep"]["ms"] += e.get("ms") or 0
                            ev["deep"]["modes"][str(e.get("mode"))] = ev["deep"]["modes"].get(str(e.get("mode")), 0) + 1
            for k in ("events", "cost_by_event", "errors"):
                ev[k] = dict(ev[k])
            out["engram"] = ev
        elif self.kind == "claude-mem":
            c = result_cost_log(self.meta["claude_mem"]["observer_log"], offs.get("claude_mem", 0))
            out["cost_usd"] = c["cost_usd"]
            out["claude_mem"] = c
        elif self.kind == "mem0":
            c = result_cost_log(self.meta["mem0"]["llm_log"], offs.get("mem0", 0))
            out["cost_usd"] = c["cost_usd"]
            out["mem0"] = c
        return out

    def memory_state(self):
        """What the memory system holds at the end of the history."""
        st = {"auto_memory_files": {}}
        for path in sorted(glob.glob(os.path.join(self.home, ".claude", "projects", "*", "memory", "**", "*"), recursive=True)):
            if os.path.isfile(path):
                st["auto_memory_files"][os.path.relpath(path, os.path.join(self.home, ".claude", "projects"))] = open(path, encoding="utf-8", errors="replace").read()[:4000]
        if self.kind == "engram":
            p = subprocess.run([os.path.join(self.bin, "engram"), "stats", "--project", self.repo], env=self.env("history"), capture_output=True, text=True, timeout=60)
            st["engram_stats"] = (p.stdout + p.stderr)[-3000:]
            subprocess.run([os.path.join(self.bin, "engram"), "stop", "--project", self.repo], env=self.env("history"), capture_output=True, timeout=60)
        elif self.kind == "claude-mem":
            q = self.claude_mem_query
            st["claude_mem"] = {
                "observations": (q("select count(*) from observations") or [[None]])[0][0],
                "summaries": (q("select count(*) from session_summaries") or [[None]])[0][0],
                "prompts": (q("select count(*) from user_prompts") or [[None]])[0][0],
                "observation_titles": [r[0] for r in (q("select title from observations order by id") or [])],
                "summary_learned": [r[0] for r in (q("select learned from session_summaries order by id") or [])],
                "models": [r[0] for r in (q("select distinct generated_by_model from observations") or [])],
            }
            st["claude_mem"]["chroma_dir_bytes"] = sum(os.path.getsize(p) for p in glob.glob(os.path.join(self.home, ".claude-mem", "chroma", "**", "*"), recursive=True) if os.path.isfile(p))
        elif self.kind == "mem0":
            setup = load_module(os.path.join(COMPETITORS, "mem0", "setup.py"), "mem0_setup")
            st["mem0"] = setup.dump(self.home, self.meta["mem0"])
        return st

    # -- history

    def apply_teammate_commits(self, block):
        done = []
        for c in self.sc["teammate_commits"]:
            if c["after_block"] != block:
                continue
            genv = dict(PATH=SYSTEM_PATH, HOME=self.home, GIT_AUTHOR_NAME=c["author"], GIT_AUTHOR_EMAIL=c["email"],
                        GIT_COMMITTER_NAME=c["author"], GIT_COMMITTER_EMAIL=c["email"])
            for rel, content in c["files"].items():
                path = os.path.join(self.repo, rel)
                if content is None:
                    if os.path.lexists(path):
                        os.remove(path)
                    git(self.repo, "rm", "-q", "--cached", "--ignore-unmatch", "--", rel, env=genv)
                else:
                    os.makedirs(os.path.dirname(path) or self.repo, exist_ok=True)
                    with open(path, "w", encoding="utf-8") as f:
                        f.write(content)
                    git(self.repo, "add", "--", rel, env=genv)
            changed = [p for p in git(self.repo, "diff", "--cached", "--name-only", "--", *c["files"], env=genv).splitlines() if p]
            if changed:
                # Only the teammate's paths: whatever the agent left uncommitted stays as it was.
                git(self.repo, "commit", "-q", "-m", c["message"], "--", *changed, env=genv)
            done.append({"after_block": block, "author": c["author"], "files": changed, "commit": git(self.repo, "rev-parse", "HEAD", env=genv).strip()})
            log(self.name, f"teammate commit by {c['author']} after block {block}: {', '.join(changed) or 'nothing changed'}")
        return done

    def history(self):
        if os.path.exists(self.hist_path) and os.path.exists(os.path.join(self.snap, "complete")):
            return json.load(open(self.hist_path))
        t_start = time.time()
        self.setup()
        full = self.kind == "full"
        sessions, turns, settles, commits = [], [], [], []
        offs = self.memory_offsets()
        session = None
        # Claude Code reports total_cost_usd cumulatively over a session (it grows with each --resume),
        # so a turn's own cost is the difference from the previous turn of the same session.
        session_cost = {}
        for k, prompts in enumerate(self.sc["history"]):
            first = False
            if not full or k == 0:
                session = str(uuid.uuid4())
                sessions.append(session)
                first = True
            for i, prompt in enumerate(prompts):
                out = self.claude(prompt, "history", f"history_b{k + 1}_p{i + 1}", session=session if first else None, resume=None if first else session)
                first = False
                cum = out.get("total_cost_usd")
                cost = None if cum is None else max(0.0, cum - session_cost.get(session, 0.0))
                if cum is not None:
                    session_cost[session] = max(cum, session_cost.get(session, 0.0))
                turns.append({"block": k + 1, "session": session, "prompt": prompt, "result": (out.get("result") or "")[:2000], "cost_usd": cost,
                              "session_cost_usd": cum, "steps": out.get("num_turns"), "error": out.get("is_error"), "wall_s": round(out["wall_s"], 1),
                              "models": list((out.get("modelUsage") or {}).keys())})
                log(self.name, f"history block {k + 1} prompt {i + 1}: {out.get('num_turns')} steps, ${cost or 0:.3f}, {out['wall_s']:.0f} s{' ERROR ' + str(out.get('result'))[:200] if out.get('is_error') else ''}")
            settles.append(self.settle("history"))
            commits += self.apply_teammate_commits(k + 1)
        if self.kind == "transcripts":
            exporter = load_module(os.path.join(COMPETITORS, "transcripts", "export_transcripts.py"), "export_transcripts")
            paths = [transcript_path(self.home, s) for s in sessions]
            exporter.export([p for p in paths if p], self.extra)
        activity = self.memory_activity(offs, "history")
        state = self.memory_state()
        settles.append(self.settle("history"))
        ev = transcript_events(self.home, session)
        tstats = transcript_stats(ev)
        h = {
            "lane": self.name, "scenario": self.sc["name"], "model": self.args.model, "meta": self.meta, "sessions": sessions, "session": session,
            "turns": turns, "settles": settles, "teammate_commits": commits,
            "cost_usd": sum(t["cost_usd"] or 0 for t in turns), "final_session_cost_usd": session_cost.get(session, 0.0), "agent_wall_s": sum(t["wall_s"] for t in turns), "wall_s": round(time.time() - t_start, 1),
            "errors": sum(bool(t["error"]) for t in turns), "memory": activity, "memory_state": state,
            "last_session_compactions": tstats["compactions"], "last_session_context_tokens": tstats["context_tokens"],
            "git_log": git(self.repo, "log", "--format=%h %an: %s", env=dict(PATH=SYSTEM_PATH, HOME=self.home)),
            "git_status": git(self.repo, "status", "--short", env=dict(PATH=SYSTEM_PATH, HOME=self.home)),
            "transcripts": sorted(os.listdir(self.extra)) if os.path.isdir(self.extra) else None,
        }
        self.snapshot()
        os.makedirs(self.args.results, exist_ok=True)
        json.dump(h, open(self.hist_path, "w"), indent=1)
        log(self.name, f"history done: ${h['cost_usd']:.3f} agent + ${activity['cost_usd']:.3f} memory, {h['wall_s']:.0f} s, {h['errors']} errors")
        return h

    def snapshot(self):
        shutil.rmtree(self.snap, ignore_errors=True)
        for src, name in ((self.repo, "repo"), (self.home, "home"), (self.extra, "transcripts")):
            if os.path.exists(src):
                sync_tree(src, os.path.join(self.snap, name))
        open(os.path.join(self.snap, "complete"), "w").close()

    def restore(self):
        for dst, name in ((self.repo, "repo"), (self.home, "home"), (self.extra, "transcripts")):
            sync_tree(os.path.join(self.snap, name), dst)

    # -- runs

    def save_patch(self, task, rep):
        os.makedirs(self.patches, exist_ok=True)
        snap = os.path.join(self.snap, "repo")
        p = subprocess.run(["git", "diff", "--no-index", "--", snap, self.repo], capture_output=True, text=True, errors="replace")
        keep = [b for b in re.split(r"(?m)^(?=diff --git )", p.stdout) if "/.git/" not in b.split("\n", 1)[0] and "__pycache__" not in b.split("\n", 1)[0]]
        with open(os.path.join(self.patches, f"{task}_{rep}.patch"), "w", encoding="utf-8") as f:
            f.write("".join(keep).replace(snap, "").replace(self.repo, ""))

    def run(self, task, rep, hist, lock):
        if home_procs(self.home):
            kill_home_procs(self.home)
        self.restore()
        offs = self.memory_offsets()
        label = f"run_{task['id']}_{rep}"
        if self.kind == "full":
            out = self.claude(task["prompt"], "run", label, resume=hist["session"], fork=True)
        else:
            out = self.claude(task["prompt"], "run", label, session=str(uuid.uuid4()))
        settle = self.settle("run")
        score, detail = task["grade"](self.repo, out.get("result") or "")
        self.save_patch(task["id"], rep)
        sid = out.get("session_id")
        ts = transcript_stats(transcript_events(self.home, sid, since=out["t0"] - 5 if self.kind == "full" else None), self.memory_paths())
        u = out.get("usage") or {}
        mem = self.memory_activity(offs, "run")
        raw_cost = out.get("total_cost_usd")
        cost = raw_cost
        if self.kind == "full" and raw_cost is not None:
            # A fork's total_cost_usd includes everything its parent session spent.
            cost = max(0.0, raw_cost - (hist.get("final_session_cost_usd") or 0.0))
        row = {
            "scenario": self.sc["name"], "lane": self.name, "task": task["id"], "kind": task.get("kind"), "repeat": rep,
            "score": score, "solved": score >= 1.0, "checks": detail,
            "cost_usd": cost, "cost_usd_reported": raw_cost, "steps": out.get("num_turns"), "wall_s": round(out["wall_s"], 1),
            "input_tokens": u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0),
            "cache_read_tokens": u.get("cache_read_input_tokens", 0), "output_tokens": u.get("output_tokens", 0),
            "models": list((out.get("modelUsage") or {}).keys()), "error": out.get("is_error"), "answer": (out.get("result") or "")[:3000],
            "tools": ts["tools"], "memory_tool_calls": sum(1 for t in ts["tools"] if t.startswith("mcp__")),
            "memory_tools": sorted({t for t in ts["tools"] if t.startswith("mcp__")}), "memory_path_tool_calls": ts["memory_path_tool_calls"],
            "hook_context_chars": ts["hook_context_chars"], "memory_attachments": ts["memory_attachments"], "compactions": ts["compactions"],
            "memory_cost_usd": mem["cost_usd"], "memory": {k: v for k, v in mem.items() if k != "cost_usd"}, "settle": settle, "session_id": sid,
        }
        with lock:
            with open(os.path.join(self.args.results, "runs.jsonl"), "a") as f:
                f.write(json.dumps(row) + "\n")
        return row


def lane_main(args, sc, name, lock, do_runs):
    lane = Lane(args, sc, name)
    try:
        hist = lane.history()
    except Exception as e:  # noqa: BLE001 - reported; the other lanes go on
        log(name, f"history failed: {type(e).__name__}: {e}")
        kill_home_procs(lane.home)
        return
    if not do_runs:
        return
    done = set()
    path = os.path.join(args.results, "runs.jsonl")
    if os.path.exists(path):
        for line in open(path):
            r = json.loads(line)
            done.add((r["lane"], r["task"], r["repeat"]))
    tasks = [t for t in sc["tasks"] if not args.tasks or t["id"] in args.tasks]
    jobs = [(t, r) for r in range(args.repeats) for t in tasks if (name, t["id"], r) not in done]
    random.Random(zlib.crc32(name.encode())).shuffle(jobs)
    for i, (t, r) in enumerate(jobs, 1):
        try:
            row = lane.run(t, r, hist, lock)
            log(name, f"{i}/{len(jobs)} {t['id']:<22} score {row['score']:.2f} ${row['cost_usd'] or 0:.3f} (+${row['memory_cost_usd']:.3f} memory) "
                      f"{row['steps']} steps {row['wall_s']:.0f} s, {row['memory_tool_calls']} memory tool calls")
        except Exception as e:  # noqa: BLE001 - reported; a rerun picks it up
            log(name, f"{t['id']} repeat {r} failed: {type(e).__name__}: {e}")
            kill_home_procs(lane.home)


# ---------------------------------------------------------------- report


def compare(rows, lane, ref, iters, rng):
    """Lane vs ref: Fisher's exact test on pooled solved counts and the
    task-stratified permutation test (ab_stats) on score and solved."""
    by = defaultdict(lambda: {"engram": [], "none": []})
    for r in rows:
        if r["lane"] in (lane, ref):
            by[r["task"]]["engram" if r["lane"] == lane else "none"].append(r)
    st = {k: v for k, v in by.items() if v["engram"] and v["none"]}
    if not st:
        return None
    a = [r for v in st.values() for r in v["engram"]]
    b = [r for v in st.values() for r in v["none"]]
    sa, sb = sum(r["solved"] for r in a), sum(r["solved"] for r in b)
    two, _ = ab_stats.fisher(sa, len(a) - sa, sb, len(b) - sb)
    for v in st.values():
        for r in v["engram"] + v["none"]:
            r["solved_f"] = float(r["solved"])
    d_score, p_score, _ = ab_stats.permutation(st, "score", iters, rng)
    d_solved, p_solved, _ = ab_stats.permutation(st, "solved_f", iters, rng)
    return {"tasks": len(st), "runs": [len(a), len(b)], "solved": [sa, sb], "fisher_solved_p": two,
            "score_diff": d_score, "perm_score_p": p_score, "solved_diff": d_solved, "perm_solved_p": p_solved}


def cmd_report(args, sc):
    path = os.path.join(args.results, "runs.jsonl")
    rows = [json.loads(l) for l in open(path)] if os.path.exists(path) else []
    if args.tasks:
        rows = [r for r in rows if r["task"] in args.tasks]
    rng = random.Random(1)
    mean = lambda xs: sum(xs) / len(xs) if xs else None  # noqa: E731
    lanes = list(dict.fromkeys([r["lane"] for r in rows]))
    order = [l for l in list(LANES) + lanes if l in lanes]
    lanes = list(dict.fromkeys(order))
    kinds = sorted({r.get("kind") or "?" for r in rows})
    out = {"scenario": sc["name"], "lanes": {}, "tasks": {}}
    for lane in lanes:
        rs = [r for r in rows if r["lane"] == lane]
        hp = os.path.join(args.results, f"history_{slug(lane)}.json")
        h = json.load(open(hp)) if os.path.exists(hp) else {}
        d = {"runs": len(rs), "solved": sum(r["solved"] for r in rs), "solved_rate": mean([r["solved"] for r in rs]), "score": mean([r["score"] for r in rs]),
             "cost_usd": mean([r["cost_usd"] or 0 for r in rs]), "memory_cost_usd": mean([r.get("memory_cost_usd") or 0 for r in rs]),
             "steps": mean([r["steps"] or 0 for r in rs]), "wall_s": mean([r["wall_s"] for r in rs]),
             "memory_tool_calls": mean([r["memory_tool_calls"] for r in rs]), "errors": sum(bool(r.get("error")) for r in rs),
             "history": {"cost_usd": h.get("cost_usd"), "memory_cost_usd": (h.get("memory") or {}).get("cost_usd"), "wall_s": h.get("wall_s"), "errors": h.get("errors")},
             "by_kind": {}, "vs": {}}
        for k in kinds:
            ks = [r for r in rs if (r.get("kind") or "?") == k]
            d["by_kind"][k] = {"runs": len(ks), "solved": sum(r["solved"] for r in ks), "score": mean([r["score"] for r in ks])}
        for ref in ("none", "engram@main"):
            if ref != lane and ref in lanes:
                d["vs"][ref] = {"all": compare(rows, lane, ref, args.iters, rng)}
                for k in kinds:
                    d["vs"][ref][k] = compare([r for r in rows if (r.get("kind") or "?") == k], lane, ref, args.iters, rng)
        out["lanes"][lane] = d
    for t in sc["tasks"]:
        out["tasks"][t["id"]] = {"kind": t.get("kind"), "needs": t.get("needs"),
                                 **{lane: mean([r["score"] for r in rows if r["lane"] == lane and r["task"] == t["id"]]) for lane in lanes}}
    os.makedirs(args.results, exist_ok=True)
    json.dump(out, open(os.path.join(args.results, "summary.json"), "w"), indent=1)
    f2 = lambda v, p=2: "–" if v is None else f"{v:.{p}f}"  # noqa: E731
    pv = lambda c, k: "–" if not c else f"{c[k]:.3f}"  # noqa: E731
    md = [f"# Memory bench: {sc['name']}", "", f"Model {args.model}; {len(rows)} runs. Solved = score 1.0. Costs in USD per run; "
          "memory $ = the memory system's own model calls during the run; history $ = agent + memory system while recording the history.", "",
          "| lane | runs | solved | score | " + " | ".join(f"{k} solved" for k in kinds) + " | $/run | memory $/run | steps | wall s | memory tool calls | history $ (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |",
          "|" + "---|" * (14 + len(kinds) - 2)]
    for lane, d in out["lanes"].items():
        vn, ve = (d["vs"].get("none") or {}).get("all"), (d["vs"].get("engram@main") or {}).get("all")
        hist = d["history"]
        md.append(f"| {lane} | {d['runs']} | {d['solved']}/{d['runs']} | {f2(d['score'])} | "
                  + " | ".join(f"{d['by_kind'][k]['solved']}/{d['by_kind'][k]['runs']}" for k in kinds)
                  + f" | {f2(d['cost_usd'], 3)} | {f2(d['memory_cost_usd'], 3)} | {f2(d['steps'], 1)} | {f2(d['wall_s'], 0)} | {f2(d['memory_tool_calls'], 1)} | "
                  f"{f2(hist['cost_usd'])} + {f2(hist['memory_cost_usd'])} | {pv(vn, 'fisher_solved_p')} / {pv(vn, 'perm_score_p')} | {pv(ve, 'fisher_solved_p')} / {pv(ve, 'perm_score_p')} |")
    md += ["", "Per task (mean score):", "", "| task | kind | " + " | ".join(lanes) + " |", "|" + "---|" * (2 + len(lanes))]
    for tid, t in out["tasks"].items():
        md.append(f"| {tid} | {t['kind']} | " + " | ".join(f2(t.get(l)) for l in lanes) + " |")
    md += ["", "p: two-sided. Fisher = Fisher's exact test on solved runs pooled over tasks; perm = permutation test of the mean score "
           "difference with lane labels shuffled within each task (scripts/ab_stats.py)."]
    open(os.path.join(args.results, "summary.md"), "w").write("\n".join(md) + "\n")
    print("\n".join(md))


# ---------------------------------------------------------------- main


def make_bench_bin(work):
    """<work>/bin: links to the tools a lane may run, so PATH needs nothing else from this machine."""
    d = os.path.join(work, "bin")
    os.makedirs(d, exist_ok=True)
    for t in LINKED_TOOLS:
        src = shutil.which(t)
        dst = os.path.join(d, t)
        if src and not os.path.lexists(dst):
            os.symlink(os.path.realpath(src) if t == "claude" else src, dst)
    if not os.path.exists(os.path.join(d, "claude")):
        sys.exit("claude not found on PATH")
    return d


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["history", "run", "report", "all"])
    ap.add_argument("--scenario", required=True, help="name under eval/ab/scenarios, or a scenario directory")
    ap.add_argument("--lanes", nargs="+", default=None, help=f"default: {' '.join(DEFAULT_LANES)}")
    ap.add_argument("--model", default="haiku")
    ap.add_argument("--distill-model", default="haiku", help="model the memory systems use for their own calls (Engram, claude-mem, mem0)")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--tasks", nargs="*")
    ap.add_argument("--work", default="/home/claude/bench/work")
    ap.add_argument("--results", default=None)
    ap.add_argument("--shared", default="/home/claude/bench/shared", help="downloads shared by lanes (packages, uv cache, models)")
    ap.add_argument("--engram-models", default="/home/claude/data/models")
    ap.add_argument("--lane-env", action="append", metavar="LANE=K=V", help="extra environment for a lane (repeatable)")
    ap.add_argument("--lane-bin", action="append", metavar="LANE=PATH", help="Engram binary for a lane")
    ap.add_argument("--budget", type=float, default=3.0, help="--max-budget-usd per claude call")
    ap.add_argument("--timeout", type=int, default=1500, help="seconds per claude call")
    ap.add_argument("--settle-timeout", type=int, default=600, help="seconds to wait for a memory system after a history session")
    ap.add_argument("--iters", type=int, default=10000, help="permutation test iterations")
    args = ap.parse_args()
    sc = load_scenario(args.scenario)
    args.results = os.path.abspath(args.results or os.path.join(ROOT, "results", "memory_bench", slug(sc["name"])))
    args.work = os.path.abspath(args.work)
    if args.cmd == "report":
        args.lanes = args.lanes or []
        cmd_report(args, sc)
        return
    args.lanes = args.lanes or DEFAULT_LANES
    for name in args.lanes:
        lane_def(name, args)
    args.bench_bin = make_bench_bin(args.work)
    os.makedirs(args.results, exist_ok=True)
    lock = threading.Lock()
    with cf.ThreadPoolExecutor(len(args.lanes)) as ex:
        for f in cf.as_completed([ex.submit(lane_main, args, sc, name, lock, args.cmd != "history") for name in args.lanes]):
            f.result()
    if args.cmd == "all":
        cmd_report(args, sc)


if __name__ == "__main__":
    main()
