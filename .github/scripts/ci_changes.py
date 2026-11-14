#!/usr/bin/env python3
"""Which of CI's jobs a change needs (ci.yml's job `changes`).

Reads the changed paths, one per line, on stdin (or none, with --all, for a
run that tests everything) and prints one line `run=<json>` for
$GITHUB_OUTPUT. The JSON names each group of jobs and whether it runs:

  any        some job besides the documentation site's check runs
  full       the whole matrix: ChloeDB, chloedb-server, the workflows or
             any path this file does not know, since everything depends
             on them
  crates     the crates `test` tests (a list, for its matrix), and test
             whether there is any
  server     a chloedb-server binary is needed (job `server-bin`)
  smoke      the smoke test runs, and which of its parts: smoke_crawl,
             smoke_sheets, smoke_ctx, smoke_apps
  python, javascript, java, docs
             those jobs

A change only under one of the apps or packages runs that one's jobs, plus
the jobs that use it: chloectx, chloedb-crawl and chloedb-sheets are run by
the smoke test and by the guide's pages (job `docs`), and the Python package
by the guide's pages too. The documentation site, Markdown files and
benchmark results run nothing, as before (see the top of ci.yml).

The demos (chloedb-netops, chloedb-swarm, examples/feature-store,
examples/parley) are on a release train of their own: CI and its required
check `CI result` do not test them, and a change only under them runs
nothing here. The workflow Demos (demos.yml) tests them, on a pull request
that touches one and every night, and a failure there does not hold a
ChloeDB release (docs/compatibility.md, "Demos").

`python3 .github/scripts/ci_changes.py --self-test` checks the rules.
"""
import json
import re
import sys

# Directories whose changes only reach the jobs listed; anything else (the
# engine, the server, .github/, scripts/, files at the root) runs it all.
# Each maps to (crates `test` tests, other groups of jobs).
APPS = {
    "chloectx/": ({"chloectx"}, {"smoke_ctx", "docs"}),
    "chloedb-crawl/": ({"chloedb-crawl"}, {"smoke_crawl", "docs"}),
    "chloedb-sheets/": ({"chloedb-sheets"}, {"smoke_sheets", "docs"}),
    "chloedb-apps/": (set(), {"smoke_apps"}),
    "chloedb-py/": (set(), {"python", "docs"}),
    "chloedb-js/": (set(), {"javascript"}),
    "chloedb-java/": (set(), {"java"}),
}
CRATES = ["chloedb", "chloectx", "chloedb-server", "chloedb-crawl", "chloedb-sheets"]
SMOKE = ["smoke_crawl", "smoke_sheets", "smoke_ctx", "smoke_apps"]
# The demo train (demos.yml): nothing in CI.
DEMOS = ("chloedb-netops/", "chloedb-swarm/", "examples/feature-store/", "examples/parley/")
JOBS = ["python", "javascript", "java", "docs"]
# The guide's pages run as scripts: a change to them or to the site's
# generator runs the docs job even though it is documentation.
GUIDE = re.compile(r"^docs/(guide/|quickstart\.md$|site/scripts/build_docs\.py$)")
# Benchmark results nothing builds or reads, except the Python scripts there
# (a job runs chloedb/results/sequence/gate.py) and the workloads
# tests/workload_gen.rs checks.
RESULTS = re.compile(r"^(chloedb|chloedb-server|chloectx)/results/")
RESULTS_CODE = re.compile(r"^(chloedb|chloedb-server|chloectx)/results/.*\.py$|^chloedb/results/workloads/")


def plan(paths, everything=False):
    full = everything
    crates, groups = set(), set()
    for p in paths:
        p = p.strip()
        if not p:
            continue
        if GUIDE.match(p):
            groups.add("docs")
            continue
        if p.startswith("docs/") or p.endswith(".md"):
            continue
        if RESULTS.match(p) and not RESULTS_CODE.match(p):
            continue
        if p.startswith(DEMOS):
            continue
        for prefix, (cs, gs) in APPS.items():
            if p.startswith(prefix):
                crates |= cs
                groups |= gs
                break
        else:
            full = True
    if full:
        crates, groups = set(CRATES), set(SMOKE) | set(JOBS)
    out = {"full": full, "crates": [c for c in CRATES if c in crates]}
    for g in SMOKE + JOBS:
        out[g] = g in groups
    out["smoke"] = any(out[g] for g in SMOKE)
    out["server"] = out["smoke"] or out["python"] or out["javascript"] or out["docs"]
    out["test"] = bool(out["crates"])
    out["any"] = full or bool(crates) or any(out[g] for g in SMOKE + JOBS)
    return out


def self_test():
    def on(paths, everything=False):
        r = plan(paths, everything)
        return ({k for k, v in r.items() if v is True} - {"test"}) | {f"test:{c}" for c in r["crates"]}

    every = {"any", "full", "server", "smoke", *SMOKE, *JOBS, *(f"test:{c}" for c in CRATES)}
    assert on([], everything=True) == every
    assert on(["chloedb/src/lib.rs"]) == every
    assert on(["chloedb-server/src/main.rs"]) == every
    assert on([".github/workflows/ci.yml"]) == every
    assert on(["install.sh"]) == every
    assert on(["README.md", "docs/chloedb/server.md", "chloedb/results/x/run.json"]) == set()
    assert on(["docs/guide/search.md"]) == {"any", "docs", "server"}
    assert on(["chloedb/results/sequence/gate.py"]) == every
    assert on(["chloedb-sheets/src/lib.rs"]) == {"any", "test:chloedb-sheets", "smoke", "smoke_sheets", "docs", "server"}
    assert on(["chloedb-netops/src/lib.rs"]) == set()
    assert on(["examples/parley/parley/app.py", "examples/feature-store/featurestore.py"]) == set()
    assert on(["chloedb-netops/src/lib.rs", "chloedb-sheets/src/lib.rs"]) == {"any", "test:chloedb-sheets", "smoke", "smoke_sheets", "docs", "server"}
    assert on(["chloedb-crawl/src/lib.rs"]) == {"any", "test:chloedb-crawl", "smoke", "smoke_crawl", "docs", "server"}
    assert on(["chloectx/src/app.rs"]) == {"any", "test:chloectx", "smoke", "smoke_ctx", "docs", "server"}
    assert on(["chloedb-apps/src/x.js"]) == {"any", "smoke", "smoke_apps", "server"}
    assert on(["chloedb-swarm/x.js"]) == set()
    assert on(["chloedb-py/python/chloedb/x.py"]) == {"any", "python", "docs", "server"}
    assert on(["chloedb-js/client.js"]) == {"any", "javascript", "server"}
    assert on(["chloedb-java/pom.xml"]) == {"any", "java"}
    assert on(["chloedb-java/pom.xml", "chloedb-js/client.js"]) == {"any", "java", "javascript", "server"}
    assert on(["chloedb-java/pom.xml", "chloedb/src/x.rs"]) == every
    assert plan(["chloectx/src/a.rs", "chloedb-crawl/src/b.rs"])["crates"] == ["chloectx", "chloedb-crawl"]
    print("ok")


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        self_test()
    else:
        everything = "--all" in sys.argv
        print("run=" + json.dumps(plan([] if everything else sys.stdin, everything), separators=(",", ":")))
