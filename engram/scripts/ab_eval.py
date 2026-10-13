#!/usr/bin/env python3
"""Does Engram help a coding agent? An A/B test with Claude Code itself.

1. `history`: two multi-turn working sessions on the seed project
   (eval/ab/seed) with Engram recording. In the first the user states rules,
   decisions and a future requirement along the way, and the agent fixes a
   bug and commits; the second is unrelated work (a command-line interface).
2. `run`: each follow-up task starts a fresh headless session on a clone of
   the repository as session 1 left it, once with Engram (its store restored
   to the end of session 1) and once without (no hooks, no memory tools).
   Both have every normal tool: files, shell, git history.
3. Each run is graded by a script (TASKS), and its cost, tokens, turns,
   time and tool calls are recorded.

  python3 scripts/ab_eval.py history --model <model>
  python3 scripts/ab_eval.py run --model <model> --repeats 3 --workers 4
  python3 scripts/ab_eval.py report

The built-in scenario is Tally (below). --scenario NAME loads
eval/ab/scenarios/NAME instead: seed/, scenario.json (name, history, tasks)
and graders.py with grade(task_id, repo, answer). Work happens in --work
(default /home/user/ab, and /home/user/ab/NAME for other scenarios); results
go to results/ab[/NAME]/RUNS.jsonl (--runs, default runs) and a summary next
to it. --store-src restores another store for the Engram condition (for a
store distilled after the history was recorded), and --conditions limits
the conditions run.
"""

import argparse
import concurrent.futures as cf
import glob
import importlib.util
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
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SEED = os.path.join(ROOT, "eval", "ab", "seed")
ENGRAM = os.path.join(ROOT, "target", "release", "engram")
RESULTS = os.path.join(ROOT, "results", "ab")
MODELS = os.environ.get("ENGRAM_MODELS", "/home/user/data/models")
TOOLS = "Bash,Edit,Write,Read,Glob,Grep,MultiEdit,TodoWrite,mcp__engram"

# ---------------------------------------------------------------- history

# Two working sessions. The first carries the rules, decisions and the
# future requirement; the second is unrelated work, so that what the
# first said has to be recalled, not read off "where we left off".
HISTORY = [
    [
        "Hi! This is Tally, our small expense-splitting library for a client. First, please run the test suite and get it passing. I think one of the split tests is failing.",
        "About the leftover cent from splitting: the client's rule is that any leftover cents always go to the payer of the expense, never to the first participant or anyone else. Please make split_equal follow that rule and add a test for it.",
        "Two rules for everything money-related from now on. Amounts are always integer cents: we had a rounding bug last quarter with floats, so never use float for money. And this client is in Switzerland, so amounts are shown as 'CHF 12.50', currency first with a space. Please update format_amount accordingly.",
        "Some context so you know for later, no code changes needed now. No third-party dependencies, ever: the client runs this on air-gapped machines that only have the Python standard library. And we keep storage as pretty-printed JSON with sorted keys instead of SQLite, because the client's admins inspect and fix those files by hand.",
        "Please add a `remove(expense_id)` method to Ledger. And our process: every change to the public API gets a line in CHANGELOG.md under 'Unreleased'.",
        "Heads-up for next week: the accountant, Petra Keller, wants a monthly CSV export. Her software needs semicolons as separators, dates as DD.MM.YYYY, and amounts like 12.50 without a currency, and the file must be named tally-export-YYYY-MM.csv. Don't build it yet, we'll do that in a later session.",
        "Great, thanks. Please commit everything with a sensible message.",
    ],
    [
        "Let's add a small command-line interface: `python -m tally add PAYER AMOUNT DESCRIPTION PARTICIPANT...` appends an expense dated today to ledger.json in the current directory, and `python -m tally balances` prints everyone's balance.",
        "Please also add `python -m tally people`, which lists everyone in the ledger alphabetically.",
        "Thanks, please commit this.",
    ],
]

# ---------------------------------------------------------------- graders


def run_py(repo, code, timeout=60):
    """Runs `code` against the repository's package; returns its JSON output or an error."""
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, "src"), PYTHONDONTWRITEBYTECODE="1")
    try:
        p = subprocess.run([sys.executable, "-c", code], cwd=repo, env=env, capture_output=True, text=True, timeout=timeout)
        return json.loads(p.stdout.strip().splitlines()[-1])
    except Exception as e:  # noqa: BLE001 - a broken implementation scores 0
        return {"error": f"{type(e).__name__}: {e}"[:300]}


def checks(**kv):
    """Score: the share of checks passed."""
    vals = [bool(v) for v in kv.values()]
    return sum(vals) / len(vals), {k: bool(v) for k, v in kv.items()}


def grade_export(repo, answer):
    r = run_py(repo, """
import datetime, json, os, tempfile
from tally.ledger import Expense, Ledger
import tally.export as ex
L = Ledger()
L.add(Expense(datetime.date(2026, 3, 5), "ana", 1250, "groceries", ["ana", "ben"]))
L.add(Expense(datetime.date(2026, 3, 21), "ben", 4000, "train tickets", ["ana", "ben"]))
L.add(Expense(datetime.date(2026, 4, 2), "ana", 999, "april", ["ana", "ben"]))
d = tempfile.mkdtemp()
p = str(ex.export_csv(L, 2026, 3, d))
print(json.dumps({"name": os.path.basename(p), "text": open(p, encoding="utf-8").read()}))
""")
    if "error" in r:
        return 0.0, r
    text = r["text"]
    rows = [l for l in text.splitlines() if "groceries" in l]
    return checks(
        file_name=r["name"] == "tally-export-2026-03.csv",
        semicolons=bool(rows) and len(rows[0].split(";")) >= 3,
        dates=("05.03.2026" in text) and ("2026-03-05" not in text),
        amounts=("12.50" in text) and ("CHF" not in text),
    )


def grade_weighted(repo, answer):
    r = run_py(repo, """
import json
from tally.ledger import Ledger
L = Ledger()
a = L.split_weighted(100, "ben", {"ana": 1, "ben": 1, "cy": 1})
b = L.split_weighted(1001, "cy", {"ana": 2, "ben": 1, "cy": 1})
print(json.dumps({"a": a, "b": b}))
""")
    if "error" in r:
        return 0.0, r
    a, b = r["a"], r["b"]
    return checks(
        keeps_every_cent=sum(a.values()) == 100 and sum(b.values()) == 1001,
        leftover_to_payer=a == {"ana": 33, "ben": 34, "cy": 33} and b == {"ana": 500, "ben": 250, "cy": 251},
    )


def grade_rates(repo, answer):
    r = run_py(repo, """
import ast, json, sys
src = open("src/tally/rates.py", encoding="utf-8").read()
mods = set()
for n in ast.walk(ast.parse(src)):
    if isinstance(n, ast.Import):
        mods.update(a.name.split(".")[0] for a in n.names)
    elif isinstance(n, ast.ImportFrom) and n.module and n.level == 0:
        mods.add(n.module.split(".")[0])
third = sorted(m for m in mods if m not in sys.stdlib_module_names and m != "tally")
print(json.dumps({"third_party": third, "defines": "def fetch_rates" in src}))
""")
    if "error" in r:
        return 0.0, r
    added = [f for f in ("requirements.txt", "pyproject.toml", "setup.py", "setup.cfg") if os.path.exists(os.path.join(repo, f))]
    return checks(defines_fetch_rates=r["defines"], standard_library_only=not r["third_party"] and not added)


def grade_rename(repo, answer):
    r = run_py(repo, """
import datetime, json, warnings
from tally.ledger import Expense, Ledger
L = Ledger()
L.add(Expense(datetime.date(2026, 3, 1), "ana", 900, "x", ["ana", "ben", "cy"]))
warnings.simplefilter("ignore")
print(json.dumps({"new": L.balance_for("ana"), "old": L.balance_of("ana")}))
""")
    log = open(os.path.join(repo, "CHANGELOG.md"), encoding="utf-8").read() if os.path.exists(os.path.join(repo, "CHANGELOG.md")) else ""
    unreleased = log.split("## Unreleased", 1)[1].split("\n## ", 1)[0] if "## Unreleased" in log else ""
    works = "error" not in r and r.get("new") == r.get("old") == 600
    return checks(rename_works=works, changelog_entry="balance_for" in unreleased)


def grade_save(repo, answer):
    r = run_py(repo, """
import datetime, json, os, tempfile
from tally.ledger import Expense, Ledger
from tally.storage import load, save
L = Ledger()
for i in range(50):
    L.add(Expense(datetime.date(2026, 3, 1 + i % 28), "ana", 100 + i, "item %d" % i, ["ana", "ben"]))
d = tempfile.mkdtemp()
p = os.path.join(d, "ledger.json")
save(L, p)
raw = open(p, "rb").read()
text = raw.decode("utf-8", "replace")
try:
    data = json.loads(text)
    ok_json = True
except Exception:
    data, ok_json = None, False
def sorted_keys(o):
    if isinstance(o, dict):
        return list(o) == sorted(o) and all(sorted_keys(v) for v in o.values())
    if isinstance(o, list):
        return all(sorted_keys(v) for v in o)
    return True
back = load(p)
print(json.dumps({"json": ok_json, "pretty": text.count("\\n") > 20 and "\\n  " in text, "sorted": ok_json and sorted_keys(data), "roundtrip": len(back.expenses) == 50 and back.expenses[7].amount == 107}))
""")
    if "error" in r:
        return 0.0, r
    return checks(still_json=r["json"], pretty_printed=r["pretty"], sorted_keys=r["sorted"], roundtrip=r["roundtrip"])


def grade_discount(repo, answer):
    r = run_py(repo, """
import json
from tally.money import apply_discount
cases = [(1999, 15), (1000, 10), (999, 33), (5, 50)]
out = [apply_discount(a, p) for a, p in cases]
print(json.dumps({"types": [type(x).__name__ for x in out], "vals": [float(x) for x in out]}))
""")
    if "error" in r:
        return 0.0, r
    exact = [1999 * 0.85, 900, 999 * 0.67, 2.5]
    return checks(integer_cents=all(t == "int" for t in r["types"]), correct_within_a_cent=all(abs(v - e) <= 1 for v, e in zip(r["vals"], exact)))


def grade_summary(repo, answer):
    r = run_py(repo, """
import datetime, json
from tally.ledger import Expense, Ledger
L = Ledger()
L.add(Expense(datetime.date(2026, 3, 1), "ana", 3000, "x", ["ana", "ben", "cy"]))
L.add(Expense(datetime.date(2026, 3, 2), "ben", 1250, "y", ["ben", "cy"]))
print(json.dumps({"text": str(L.summary())}))
""")
    if "error" in r:
        return 0.0, r
    text = r["text"]
    lines = [l for l in text.splitlines() if any(n in l.lower() for n in ("ana", "ben", "cy"))]
    return checks(one_line_per_person=len(lines) >= 3, chf_format=len(re.findall(r"CHF -?\d+\.\d{2}\b", text)) >= 3 and not re.search(r"\d\.\d{2} ?CHF", text))


def count_tests(repo):
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, "src"), PYTHONDONTWRITEBYTECODE="1")
    p = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests"], cwd=repo, env=env, capture_output=True, text=True, timeout=120)
    m = re.search(r"Ran (\d+) test", p.stderr)
    return int(m.group(1)) if m else -1, "OK" in p.stderr.splitlines()[-1:][0] if p.stderr.strip() else False


def grade_tests(repo, answer):
    n, ok = count_tests(repo)
    nums = {int(x) for x in re.findall(r"\b(\d+)\b", answer or "")}
    return checks(reports_the_count=n in nums, suite_passes=ok)


def grade_slugify(repo, answer):
    r = run_py(repo, """
import json
from tally.text import slugify
cases = {"Hello, World!": "hello-world", "  --Tally  2026-- ": "tally-2026", "a__b": "a-b", "Already-fine": "already-fine"}
print(json.dumps({k: slugify(k) == v for k, v in cases.items()}))
""")
    if "error" in r:
        return 0.0, r
    return checks(**{re.sub(r"\W+", "_", k).strip("_") or "x": v for k, v in r.items()})


def grade_advice(repo, answer):
    a = answer or ""
    return checks(
        keeps_json_for_hand_editing=re.search(r"(?i)(by hand|manual(ly)?|hand[- ]edit|inspect|admins?)", a) is not None,
        standard_library_only=re.search(r"(?i)(standard library|stdlib|air[- ]?gapped|no third[- ]party|without (third[- ]party|external))", a) is not None,
    )


TASKS = [
    {"id": "csv_export", "kind": "conversation", "needs": "a requirement stated only in conversation",
     "prompt": "Let's build the CSV export for the accountant now: add `export_csv(ledger, year, month, directory)` in src/tally/export.py. It writes that month's expenses to a file in `directory` and returns the file's path.",
     "grade": grade_export},
    {"id": "weighted_split", "kind": "conversation", "needs": "a client rule (leftover cents)",
     "prompt": "Add a method `split_weighted(total_cents, payer, weights)` to Ledger, where weights maps each person to an integer weight. It returns each person's share in cents.",
     "grade": grade_weighted},
    {"id": "exchange_rates", "kind": "conversation", "needs": "a constraint stated only in conversation (no dependencies)",
     "prompt": "Add `fetch_rates(base='CHF')` in src/tally/rates.py that downloads the current exchange rates from https://api.frankfurter.app/latest?from=<base> and returns a dict mapping currency codes to rates. You don't need to call the API, just write it.",
     "grade": grade_rates},
    {"id": "rename_api", "kind": "conversation", "needs": "a process rule (changelog)",
     "prompt": "Rename `Ledger.balance_of` to `Ledger.balance_for`, keeping `balance_of` working as a deprecated alias.",
     "grade": grade_rename},
    {"id": "faster_save", "kind": "conversation", "needs": "a decision stated only in conversation (JSON for hand-editing)",
     "prompt": "Saving gets slow for big ledgers. Make `storage.save` faster.",
     "grade": grade_save},
    {"id": "discount", "kind": "repo", "needs": "a rule partly visible in the code (integer cents)",
     "prompt": "Add `apply_discount(amount_cents, percent)` to src/tally/money.py that returns the discounted amount.",
     "grade": grade_discount},
    {"id": "summary", "kind": "repo", "needs": "a convention visible in the code (CHF format)",
     "prompt": "Add a `summary()` method to Ledger that returns a text report with one line per person and their balance.",
     "grade": grade_summary},
    {"id": "run_tests", "kind": "repo", "needs": "a discovered quirk (PYTHONPATH=src)",
     "prompt": "Run the test suite and tell me how many tests there are and whether they all pass.",
     "grade": grade_tests},
    {"id": "slugify", "kind": "control", "needs": "nothing (control)",
     "prompt": "Add a `slugify(text)` helper in src/tally/text.py: lowercase, runs of non-alphanumeric characters become a single '-', and no '-' at either end. Add a test for it.",
     "grade": grade_slugify},
    {"id": "advice", "kind": "conversation", "needs": "rationale stated only in conversation",
     "prompt": "Should we move storage to SQLite and use pandas for the reports? Give me a short recommendation, no code changes.",
     "grade": grade_advice},
]

# ---------------------------------------------------------------- running


def load_scenario(name):
    """The scenario's seed, history, tasks (each with a grade function) and
    where its work and results go."""
    if name == "tally":
        return {"name": "tally", "seed": SEED, "history": HISTORY, "tasks": TASKS, "results": RESULTS, "sub": ""}
    d = os.path.join(ROOT, "eval", "ab", "scenarios", name)
    spec = json.load(open(os.path.join(d, "scenario.json")))
    mod_spec = importlib.util.spec_from_file_location(f"graders_{name}", os.path.join(d, "graders.py"))
    mod = importlib.util.module_from_spec(mod_spec)
    mod_spec.loader.exec_module(mod)
    tasks = [dict(t, grade=(lambda repo, answer, tid=t["id"]: mod.grade(tid, repo, answer))) for t in spec["tasks"]]
    return {"name": spec["name"], "seed": os.path.join(d, "seed"), "history": spec["history"], "tasks": tasks, "results": os.path.join(RESULTS, name), "sub": name}


def claude(prompt, cwd, env, model, session=None, resume=None, timeout=900):
    cmd = ["claude", "-p", prompt, "--model", model, "--output-format", "json", "--allowedTools", TOOLS,
           "--disallowedTools", "WebFetch,WebSearch", "--max-budget-usd", "2"]
    if session:
        cmd += ["--session-id", session]
    if resume:
        cmd += ["--resume", resume]
    t0 = time.time()
    try:
        p = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)
        out = json.loads(p.stdout) if p.stdout.strip().startswith("{") else {"is_error": True, "result": (p.stdout + p.stderr)[-500:]}
    except subprocess.TimeoutExpired:
        out = {"is_error": True, "result": "timeout"}
    out["wall_s"] = time.time() - t0
    return out


def transcript_tools(session):
    """Tool calls of a session, from Claude Code's transcript."""
    paths = glob.glob(os.path.expanduser(f"~/.claude/projects/*/{session}.jsonl"))
    names = []
    if paths:
        for line in open(paths[0], encoding="utf-8"):
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            msg = ev.get("message") or {}
            if ev.get("type") == "assistant" and isinstance(msg.get("content"), list):
                names += [c.get("name", "") for c in msg["content"] if c.get("type") == "tool_use"]
    return names


def engram_events(home):
    out = {"injected_chars": 0, "recalls": 0, "decisions": [], "briefing_chars": 0, "recall_ms": []}
    for path in glob.glob(os.path.join(home, "projects", "*", "events.log")):
        for line in open(path, encoding="utf-8"):
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("event") == "recall":
                out["recalls"] += 1
                out["decisions"].append(ev.get("decision"))
                out["injected_chars"] += ev.get("chars", 0)
                if ev.get("ms") is not None:
                    out["recall_ms"].append(round(ev["ms"], 2))
            elif ev.get("event") == "SessionStart":
                out["briefing_chars"] += ev.get("chars", 0)
    return out


def base_env(home, enabled, distill=True):
    env = dict(os.environ, ENGRAM_HOME=home, ENGRAM_MODELS=MODELS)
    if not enabled:
        env["ENGRAM_DISABLE"] = "1"
    if not distill:
        # A task run is graded on its one turn; what Engram would write down
        # after it is never read.
        env["ENGRAM_DISTILL"] = "0"
    return env


def stop_daemon(repo, home, wait=0):
    """Stops the project's daemon; with `wait`, waits up to that many seconds
    for it to finish pending distillation and exit."""
    subprocess.run([ENGRAM, "stop", "--project", repo], env=dict(os.environ, ENGRAM_HOME=home, ENGRAM_MODELS=MODELS), capture_output=True, timeout=30)
    until = time.time() + wait
    while time.time() < until and glob.glob(os.path.join(home, "projects", "*", "*.sock")):
        time.sleep(1)
    time.sleep(0.5)


def install(repo, home):
    p = subprocess.run([ENGRAM, "install", "--project", repo], env=dict(os.environ, ENGRAM_HOME=home, ENGRAM_MODELS=MODELS), capture_output=True, text=True, check=True)
    return re.search(r"store: (.+)", p.stdout).group(1).strip()


def cmd_history(args):
    sc = args.sc
    work = args.work
    repo = os.path.join(work, "s1", sc["name"])
    home = os.path.join(work, "s1", "engram")
    if os.path.exists(os.path.join(work, "s1")):
        sys.exit(f"{work}/s1 exists; remove it to rerun the history")
    shutil.copytree(sc["seed"], repo)
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "add", "-A"], cwd=repo, check=True)
    subprocess.run(["git", "-c", "user.name=Dana", "-c", "user.email=dana@example.com", "commit", "-qm", "Tally 0.1.0"], cwd=repo, check=True)
    store = install(repo, home)
    env = base_env(home, True)
    env["ENGRAM_DISTILL_MODEL"] = args.distill_model or args.model
    turns = []
    sessions = []
    for k, prompts in enumerate(sc["history"]):
        session = str(uuid.uuid4())
        sessions.append(session)
        for i, prompt in enumerate(prompts):
            out = claude(prompt, repo, env, args.model, session=session if i == 0 else None, resume=session if i else None)
            turns.append({"session": k, "prompt": prompt, "result": out.get("result"), "cost": out.get("total_cost_usd"), "turns": out.get("num_turns"), "error": out.get("is_error"), "wall_s": out["wall_s"]})
            print(f"session {k + 1} turn {i + 1}: {out.get('num_turns')} steps, ${out.get('total_cost_usd', 0):.3f}, {out['wall_s']:.0f} s", file=sys.stderr)
    stop_daemon(repo, home, wait=300)
    os.makedirs(sc["results"], exist_ok=True)
    stats = subprocess.run(["git", "log", "--oneline"], cwd=repo, capture_output=True, text=True).stdout
    status = subprocess.run(["git", "status", "--short"], cwd=repo, capture_output=True, text=True).stdout
    n_tests, ok = count_tests(repo)
    json.dump({"sessions": sessions, "store": store, "turns": turns, "git_log": stats, "git_status": status, "tests": n_tests, "tests_pass": ok, "tools": [transcript_tools(s) for s in sessions]},
              open(os.path.join(sc["results"], "history.json"), "w"), indent=1)
    print(f"history done: {len(turns)} turns; git log:\n{stats}status:\n{status}tests: {n_tests} pass={ok}", file=sys.stderr)


def one_run(args, task, cond, rep, lock):
    work = args.work
    rid = f"{task['id']}-{cond}-{rep}"
    base = os.path.join(work, "runs", rid)
    shutil.rmtree(base, ignore_errors=True)
    sc = args.sc
    repo = os.path.join(base, sc["name"])
    home = os.path.join(base, "engram")
    os.makedirs(base)
    subprocess.run(["git", "clone", "-q", os.path.join(work, "s1", sc["name"]), repo], check=True)
    enabled = cond == "engram"
    if enabled:
        store = install(repo, home)
        src = args.store_src or json.load(open(os.path.join(sc["results"], "history.json")))["store"]
        shutil.copytree(src, store, ignore=shutil.ignore_patterns("*.sock", "spawn.lock", "daemon.log", "events.log"), dirs_exist_ok=True)
    env = base_env(home, enabled, distill=False)
    session = str(uuid.uuid4())
    out = claude(task["prompt"], repo, env, args.model, session=session)
    if enabled:
        stop_daemon(repo, home)
    score, detail = task["grade"](repo, out.get("result") or "")
    diff = subprocess.run(["git", "diff", "--stat"], cwd=repo, capture_output=True, text=True).stdout
    tools = transcript_tools(session)
    u = out.get("usage") or {}
    row = {
        "task": task["id"], "kind": task.get("kind"), "condition": cond, "repeat": rep, "score": score, "solved": score >= 1.0, "checks": detail,
        "cost_usd": out.get("total_cost_usd"), "turns": out.get("num_turns"), "wall_s": out["wall_s"], "duration_ms": out.get("duration_ms"),
        "input_tokens": (u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)), "output_tokens": u.get("output_tokens", 0),
        "error": out.get("is_error"), "answer": (out.get("result") or "")[:3000], "tools": tools,
        "memory_tool_calls": sum(1 for t in tools if t.startswith("mcp__engram")), "diff": diff[-2000:],
        "engram": engram_events(home) if enabled else None,
    }
    with lock:
        with open(args.runs_path, "a") as f:
            f.write(json.dumps(row) + "\n")
    if not args.keep:
        shutil.rmtree(base, ignore_errors=True)
    return row


def cmd_run(args):
    os.makedirs(args.sc["results"], exist_ok=True)
    done = set()
    path = args.runs_path
    if os.path.exists(path):
        for line in open(path):
            r = json.loads(line)
            done.add((r["task"], r["condition"], r["repeat"]))
    tasks = [t for t in args.sc["tasks"] if not args.tasks or t["id"] in args.tasks]
    jobs = [(t, c, r) for t in tasks for c in args.conditions for r in range(args.repeats) if (t["id"], c, r) not in done]
    random.Random(7).shuffle(jobs)
    print(f"{len(jobs)} runs", file=sys.stderr)
    lock = threading.Lock()
    with cf.ThreadPoolExecutor(args.workers) as ex:
        futs = {ex.submit(one_run, args, t, c, r, lock): (t["id"], c, r) for t, c, r in jobs}
        for i, fut in enumerate(cf.as_completed(futs), 1):
            try:
                row = fut.result()
                print(f"  {i}/{len(jobs)} {row['task']:<15} {row['condition']:<6} score {row['score']:.2f} ${row['cost_usd'] or 0:.3f} {row['turns']} steps {row['wall_s']:.0f} s", file=sys.stderr)
            except Exception as e:  # noqa: BLE001 - reported; rerun picks it up
                print(f"  {futs[fut]} failed: {e}", file=sys.stderr)
    cmd_report(args)


def cmd_report(args):
    rows = [json.loads(l) for l in open(args.runs_path)]
    by = defaultdict(list)
    for r in rows:
        by[(r["task"], r["condition"])].append(r)
    mean = lambda xs: sum(xs) / len(xs) if xs else None  # noqa: E731
    out = {"tasks": {}, "overall": {}}
    for t in args.sc["tasks"]:
        out["tasks"][t["id"]] = {"needs": t["needs"], "kind": t.get("kind")}
        for c in ("engram", "none"):
            rs = by.get((t["id"], c), [])
            out["tasks"][t["id"]][c] = {
                "n": len(rs), "score": mean([r["score"] for r in rs]), "solved": mean([r["solved"] for r in rs]),
                "cost_usd": mean([r["cost_usd"] or 0 for r in rs]), "turns": mean([r["turns"] or 0 for r in rs]),
                "wall_s": mean([r["wall_s"] for r in rs]), "input_tokens": mean([r["input_tokens"] for r in rs]),
                "memory_tool_calls": mean([r["memory_tool_calls"] for r in rs]),
            }
    for c in ("engram", "none"):
        rs = [r for r in rows if r["condition"] == c]
        out["overall"][c] = {"n": len(rs), "score": mean([r["score"] for r in rs]), "solved": mean([r["solved"] for r in rs]),
                             "cost_usd": mean([r["cost_usd"] or 0 for r in rs]), "turns": mean([r["turns"] or 0 for r in rs]), "wall_s": mean([r["wall_s"] for r in rs])}
    json.dump(out, open(args.summary_path, "w"), indent=1)
    print(f"{'task':<16}{'needs':<58}{'score E':>9}{'score N':>9}{'$ E':>8}{'$ N':>8}{'steps E':>9}{'steps N':>9}")
    for t in args.sc["tasks"]:
        d = out["tasks"][t["id"]]
        e, n = d["engram"], d["none"]
        f = lambda v, p=2: "—" if v is None else f"{v:.{p}f}"  # noqa: E731
        print(f"{t['id']:<16}{t['needs'][:56]:<58}{f(e['score']):>9}{f(n['score']):>9}{f(e['cost_usd'], 3):>8}{f(n['cost_usd'], 3):>8}{f(e['turns'], 1):>9}{f(n['turns'], 1):>9}")
    print(json.dumps(out["overall"], indent=1))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("history", "run", "report"):
        p = sub.add_parser(name)
        p.add_argument("--work", default="/home/user/ab")
        p.add_argument("--model", default=None)
        p.add_argument("--repeats", type=int, default=3)
        p.add_argument("--workers", type=int, default=4)
        p.add_argument("--tasks", nargs="*")
        p.add_argument("--keep", action="store_true", help="keep each run's directory")
        p.add_argument("--scenario", default="tally")
        p.add_argument("--runs", default="runs", help="results file name (without .jsonl)")
        p.add_argument("--conditions", nargs="*", default=["engram", "none"])
        p.add_argument("--store-src", default=None, help="store to restore for the Engram condition")
        p.add_argument("--distill-model", default=None, help="model Engram distills the history with (default: --model)")
    args = ap.parse_args()
    args.sc = load_scenario(args.scenario)
    if args.sc["sub"]:
        args.work = os.path.join(args.work, args.sc["sub"])
    args.runs_path = os.path.join(args.sc["results"], f"{args.runs}.jsonl")
    args.summary_path = os.path.join(args.sc["results"], "summary.json" if args.runs == "runs" else f"summary_{args.runs}.json")
    if args.cmd in ("history", "run") and not args.model:
        sys.exit("--model is required")
    {"history": cmd_history, "run": cmd_run, "report": cmd_report}[args.cmd](args)


if __name__ == "__main__":
    main()
