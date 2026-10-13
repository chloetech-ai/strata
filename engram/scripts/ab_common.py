"""Helpers for the A/B graders (scripts/ab_eval.py and eval/ab/scenarios/*/graders.py)."""

import json
import os
import re
import subprocess
import sys


def run_py(repo, code, pythonpath="src", timeout=60):
    """Runs `code` with the repository's sources importable; returns the JSON
    object the code prints last, or {"error": ...}."""
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, pythonpath), PYTHONDONTWRITEBYTECODE="1")
    try:
        p = subprocess.run([sys.executable, "-c", code], cwd=repo, env=env, capture_output=True, text=True, timeout=timeout)
        return json.loads(p.stdout.strip().splitlines()[-1])
    except Exception as e:  # noqa: BLE001 - a broken implementation scores 0
        return {"error": f"{type(e).__name__}: {e}"[:300]}


def checks(**kv):
    """Score: the share of checks passed, and the checks."""
    vals = [bool(v) for v in kv.values()]
    return sum(vals) / len(vals), {k: bool(v) for k, v in kv.items()}


def count_tests(repo, pythonpath="src", start="tests"):
    """(number of tests run, whether the suite passed) with unittest discovery."""
    env = dict(os.environ, PYTHONPATH=os.path.join(repo, pythonpath), PYTHONDONTWRITEBYTECODE="1")
    p = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", start], cwd=repo, env=env, capture_output=True, text=True, timeout=120)
    m = re.search(r"Ran (\d+) test", p.stderr)
    last = p.stderr.strip().splitlines()[-1] if p.stderr.strip() else ""
    return (int(m.group(1)) if m else -1), last.startswith("OK")


def read(repo, rel):
    """A file of the repository, or '' if it does not exist."""
    path = os.path.join(repo, rel)
    return open(path, encoding="utf-8").read() if os.path.exists(path) else ""


def third_party_imports(repo, rel):
    """Top-level modules imported by a file that are not in the standard library."""
    r = run_py(repo, f"""
import ast, json, sys
src = open({rel!r}, encoding="utf-8").read()
mods = set()
for n in ast.walk(ast.parse(src)):
    if isinstance(n, ast.Import):
        mods.update(a.name.split(".")[0] for a in n.names)
    elif isinstance(n, ast.ImportFrom) and n.module and n.level == 0:
        mods.add(n.module.split(".")[0])
print(json.dumps(sorted(m for m in mods if m not in sys.stdlib_module_names)))
""")
    return r if isinstance(r, list) else ["<error>"]
