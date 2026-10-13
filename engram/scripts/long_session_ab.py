#!/usr/bin/env python3
"""Engram in one long Claude Code session that compacts.

The A/B test in ab_eval.py starts every task in a new session. This one keeps
the whole history in a single session, as a long working day does, and
compacts it after each block of work, as Claude Code does when the context
fills up. Each follow-up task then forks that session (`--resume ID
--fork-session`), so every run starts from the same state. The lanes, each
with its own history:

  compact         the history compacts after each block; no Engram
  compact+engram  the same, with Engram recording and recalling throughout
  compact+engram+graph  the same, with graph recall on (ENGRAM_GRAPH=1)
  full            the history never compacts (the whole session stays in
                  context); no Engram
  fresh           each block is its own session, and each task starts a new
                  one, as when work resumes the next day; no Engram
  fresh+engram    the same, with Engram

Runs inside a lane are sequential: a resumed session refers to the files
by path, so the repository, Engram's store and Claude Code's own auto memory
(kept outside the repository) are restored to the end of the history before
each run. What each run changed in the repository is kept in
patches/<lane>/<task>_<repeat>.patch. Lanes run in parallel.

  python3 scripts/long_session_ab.py --model <model> --scenarios stockroom slotwise --repeats 3
  python3 scripts/long_session_ab.py --model <model> --scenarios kestrel --lanes fresh fresh+engram compact compact+engram full

Results go to results/long/<scenario>/runs.jsonl (one row per run) and
history_<lane>.json (the history's turns, costs, compaction summaries and,
with Engram, what its distillation cost).
"""

import argparse
import concurrent.futures as cf
import glob
import json
import os
import random
import re
import shutil
import subprocess
import sys
import threading
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ab_eval as ab  # noqa: E402

RESULTS = os.path.join(ab.ROOT, "results", "long")
LANES = ["compact", "compact+engram", "compact+engram+graph", "full"]
ALL_LANES = LANES + ["fresh", "fresh+engram"]
# Settings of an enclosing session that would change how the runs compact.
SCRUB = ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE", "CLAUDECODE")


def claude(prompt, cwd, env, model, session=None, resume=None, fork=False, timeout=1200):
    cmd = ["claude", "-p", prompt, "--model", model, "--output-format", "json", "--allowedTools", ab.TOOLS,
           "--disallowedTools", "WebFetch,WebSearch", "--max-budget-usd", "3"]
    # Only the repository's own settings and MCP servers: the user's plugins,
    # hooks and MCP servers (another memory system among them) would add the
    # same outside context to every lane. Engram installs into
    # .claude/settings.local.json and .mcp.json, so it still loads.
    cmd += ["--setting-sources", "project,local", "--strict-mcp-config"]
    if os.path.exists(os.path.join(cwd, ".mcp.json")):
        cmd += ["--mcp-config", os.path.join(cwd, ".mcp.json")]
    if session:
        cmd += ["--session-id", session]
    if resume:
        cmd += ["--resume", resume]
    if fork:
        cmd += ["--fork-session"]
    t0 = time.time()
    try:
        p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
        out = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {"is_error": True, "result": (p.stdout + p.stderr)[-500:]}
    except subprocess.TimeoutExpired:
        out = {"is_error": True, "result": "timeout"}
    out["wall_s"] = time.time() - t0
    return out


def transcript(session):
    paths = glob.glob(os.path.expanduser(f"~/.claude/projects/*/{session}.jsonl"))
    if not paths:
        return []
    out = []
    for line in open(paths[0], encoding="utf-8"):
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return out


def summaries(session):
    """The compaction summaries written into a session, in order."""
    out = []
    for ev in transcript(session):
        if ev.get("type") == "user" and ev.get("isCompactSummary"):
            c = (ev.get("message") or {}).get("content")
            if isinstance(c, list):
                c = "\n".join(x.get("text", "") for x in c if isinstance(x, dict))
            out.append(c or "")
    return out


def context_tokens(session):
    """Input tokens of the session's last request (what the next turn starts from)."""
    last = 0
    for ev in transcript(session):
        u = ((ev.get("message") or {}).get("usage") or {}) if ev.get("type") == "assistant" else {}
        if u:
            last = u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)
    return last


def distill_cost(home):
    """What Engram's distillation (a separate model call after each turn) cost during the history."""
    out = {"turns": 0, "errors": 0, "cost_usd": 0.0, "ms": []}
    for path in glob.glob(os.path.join(home, "projects", "*", "events.log")):
        for line in open(path, encoding="utf-8"):
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("event") == "distill":
                out["turns"] += 1
                out["errors"] += "error" in ev
                out["cost_usd"] += ev.get("cost_usd") or 0
                if ev.get("ms") is not None:
                    out["ms"].append(ev["ms"])
    return out


class Lane:
    def __init__(self, args, sc, name):
        self.args, self.sc, self.name = args, sc, name
        self.dir = os.path.join(args.work, sc["name"], name.replace("+", "_"))
        self.repo = os.path.join(self.dir, sc["name"])
        self.home = os.path.join(self.dir, "engram")
        self.engram = "engram" in name
        self.compact = name.startswith("compact")
        self.fresh = name.startswith("fresh")
        self.hist_path = os.path.join(sc["results"], f"history_{name.replace('+', '_')}.json")
        # Claude Code's own auto memory for this repository. It lives outside
        # the repository, so it is reset with the history and restored with
        # the repository before each run; otherwise what one run writes there
        # reaches the runs after it.
        self.cc_memory = os.path.expanduser(os.path.join("~/.claude/projects", re.sub(r"[^A-Za-z0-9]", "-", os.path.abspath(self.repo)), "memory"))
        self.patches = os.path.join(sc["results"], "patches", name.replace("+", "_"))

    def env(self, distill):
        env = ab.base_env(self.home, self.engram, distill=distill)
        for k in SCRUB:
            env.pop(k, None)
        env["ENGRAM_DISTILL_MODEL"] = self.args.distill_model or self.args.model
        if "graph" in self.name:
            env["ENGRAM_GRAPH"] = "1"
        return env

    def history(self):
        if os.path.exists(self.hist_path):
            return json.load(open(self.hist_path))
        shutil.rmtree(self.dir, ignore_errors=True)
        shutil.rmtree(self.cc_memory, ignore_errors=True)
        os.makedirs(self.dir)
        shutil.copytree(self.sc["seed"], self.repo)
        for c in (["init", "-q"], ["add", "-A"], ["-c", "user.name=Dana", "-c", "user.email=dana@example.com", "commit", "-qm", "initial version"]):
            subprocess.run(["git"] + c, cwd=self.repo, check=True)
        if self.engram:
            ab.install(self.repo, self.home)
        env = self.env(distill=True)
        session = str(uuid.uuid4())
        sessions = [session]
        turns = []
        first = True
        for k, prompts in enumerate(self.sc["history"]):
            if self.fresh and k:
                session = str(uuid.uuid4())
                sessions.append(session)
                first = True
            for prompt in prompts:
                out = claude(prompt, self.repo, env, self.args.model, session=session if first else None, resume=None if first else session)
                first = False
                turns.append({"block": k, "prompt": prompt, "result": out.get("result"), "cost": out.get("total_cost_usd"), "steps": out.get("num_turns"), "error": out.get("is_error"), "wall_s": out["wall_s"]})
                self.log(f"history block {k + 1}: {out.get('num_turns')} steps, ${out.get('total_cost_usd') or 0:.3f}, {out['wall_s']:.0f} s")
            if self.compact:
                before = context_tokens(session)
                out = claude("/compact", self.repo, env, self.args.model, resume=session)
                turns.append({"block": k, "prompt": "/compact", "cost": out.get("total_cost_usd"), "error": out.get("is_error"), "wall_s": out["wall_s"], "context_before": before})
                self.log(f"compacted after block {k + 1} (context was {before} tokens)")
        if self.engram:
            ab.stop_daemon(self.repo, self.home, wait=300)
        for src in (self.repo, self.home):
            if os.path.exists(src):
                shutil.copytree(src, src + ".snap", symlinks=True)
        if os.path.exists(self.cc_memory):
            shutil.copytree(self.cc_memory, self.mem_snap, symlinks=True)
        h = {
            "lane": self.name, "session": session, "sessions": sessions, "turns": turns, "summaries": summaries(session), "context_tokens": context_tokens(session),
            "cost_usd": sum(t["cost"] or 0 for t in turns), "git_log": subprocess.run(["git", "log", "--oneline"], cwd=self.repo, capture_output=True, text=True).stdout,
        }
        if self.engram:
            h["distill"] = distill_cost(self.home)
        h["claude_memory_files"] = self.memory_files()
        os.makedirs(self.sc["results"], exist_ok=True)
        json.dump(h, open(self.hist_path, "w"), indent=1)
        return h

    @property
    def mem_snap(self):
        return os.path.join(self.dir, "claude_memory.snap")

    def memory_files(self):
        """Claude Code's own memory files at the end of the history (name -> size)."""
        if not os.path.isdir(self.mem_snap):
            return {}
        return {f: os.path.getsize(os.path.join(self.mem_snap, f)) for f in sorted(os.listdir(self.mem_snap))}

    def restore(self):
        for dst, snap in ((self.repo, self.repo + ".snap"), (self.home, self.home + ".snap"), (self.cc_memory, self.mem_snap)):
            shutil.rmtree(dst, ignore_errors=True)
            if os.path.exists(snap):
                shutil.copytree(snap, dst, symlinks=True)

    def save_patch(self, task, rep):
        """What the run changed in the repository, for regrading and reading later."""
        os.makedirs(self.patches, exist_ok=True)
        p = subprocess.run(["git", "diff", "--no-index", "--", self.repo + ".snap", self.repo], capture_output=True, text=True, errors="replace")
        keep = []
        for block in re.split(r"(?m)^(?=diff --git )", p.stdout):
            if "/.git/" in block.split("\n", 1)[0] or "__pycache__" in block.split("\n", 1)[0]:
                continue
            keep.append(block)
        with open(os.path.join(self.patches, f"{task}_{rep}.patch"), "w", encoding="utf-8") as f:
            f.write("".join(keep).replace(self.repo + ".snap", "").replace(self.repo, ""))

    def run(self, task, rep, hist, lock):
        self.restore()
        env = self.env(distill=False)
        if self.fresh:
            out = claude(task["prompt"], self.repo, env, self.args.model)
        else:
            out = claude(task["prompt"], self.repo, env, self.args.model, resume=hist["session"], fork=True)
        if self.engram:
            ab.stop_daemon(self.repo, self.home)
        score, detail = task["grade"](self.repo, out.get("result") or "")
        self.save_patch(task["id"], rep)
        sid = out.get("session_id")
        tools = ab.transcript_tools(sid) if sid else []
        u = out.get("usage") or {}
        row = {
            "scenario": self.sc["name"], "lane": self.name, "task": task["id"], "kind": task.get("kind"), "repeat": rep, "score": score, "solved": score >= 1.0, "checks": detail,
            "cost_usd": out.get("total_cost_usd"), "steps": out.get("num_turns"), "wall_s": out["wall_s"],
            "input_tokens": u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0),
            "cache_read_tokens": u.get("cache_read_input_tokens", 0), "output_tokens": u.get("output_tokens", 0),
            "error": out.get("is_error"), "answer": (out.get("result") or "")[:3000], "tools": tools,
            "memory_tool_calls": sum(1 for t in tools if t.startswith("mcp__engram")),
            "engram": ab.engram_events(self.home) if self.engram else None,
        }
        with lock:
            with open(os.path.join(self.sc["results"], "runs.jsonl"), "a") as f:
                f.write(json.dumps(row) + "\n")
        return row

    def log(self, msg):
        print(f"[{self.sc['name']}/{self.name}] {msg}", file=sys.stderr, flush=True)


def lane_main(args, sc, name, lock):
    lane = Lane(args, sc, name)
    hist = lane.history()
    path = os.path.join(sc["results"], "runs.jsonl")
    done = set()
    if os.path.exists(path):
        for line in open(path):
            r = json.loads(line)
            done.add((r["lane"], r["task"], r["repeat"]))
    tasks = [t for t in sc["tasks"] if not args.tasks or t["id"] in args.tasks]
    jobs = [(t, r) for r in range(args.repeats) for t in tasks if (name, t["id"], r) not in done]
    random.Random(hash(name) & 0xFFFF).shuffle(jobs)
    for i, (t, r) in enumerate(jobs, 1):
        try:
            row = lane.run(t, r, hist, lock)
            lane.log(f"{i}/{len(jobs)} {t['id']:<22} score {row['score']:.2f} ${row['cost_usd'] or 0:.3f} {row['steps']} steps {row['wall_s']:.0f} s")
        except Exception as e:  # noqa: BLE001 - reported; a rerun picks it up
            lane.log(f"{t['id']} rep {r} failed: {e}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True)
    ap.add_argument("--distill-model", default=None)
    ap.add_argument("--scenarios", nargs="+", default=["stockroom", "slotwise"])
    ap.add_argument("--lanes", nargs="+", default=LANES, choices=ALL_LANES)
    ap.add_argument("--tasks", nargs="*")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--work", default="/home/user/long")
    args = ap.parse_args()
    lock = threading.Lock()
    jobs = []
    for name in args.scenarios:
        sc = ab.load_scenario(name)
        sc["results"] = os.path.join(RESULTS, sc["name"])
        os.makedirs(sc["results"], exist_ok=True)
        jobs += [(sc, lane) for lane in args.lanes]
    with cf.ThreadPoolExecutor(len(jobs)) as ex:
        for f in cf.as_completed([ex.submit(lane_main, args, sc, lane, lock) for sc, lane in jobs]):
            f.result()


if __name__ == "__main__":
    main()
