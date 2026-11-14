#!/usr/bin/env python3
"""Turn a scheduled workflow run on main into an issue that someone sees.

Run by .github/workflows/nightly-report.yml after a nightly run of CI, Chaos,
Fuzz, Supply chain, Perf, Docs URLs or Demos completes. One open issue per workflow, titled
"Nightly <workflow> is failing on main" and labelled `nightly-failure`:

- a failed run opens it, or comments on it when it is already open, with the
  failed jobs and the names of the tests that failed in each (read from the
  jobs' logs);
- a run that passed closes it, saying which run passed.

Needs `gh` and a token with `issues: write` and `actions: read` in GH_TOKEN.
Environment: REPO, RUN_ID, WORKFLOW, CONCLUSION, HEAD_SHA, RUN_URL.
DRY_RUN=1 prints what it would do instead of doing it. `--parse FILE` prints
the failures it finds in one saved job log, for trying the patterns out.
"""

import json
import os
import re
import subprocess
import sys

LABEL = "nightly-failure"
MAX_PER_JOB = 25

# A job log's lines start with a timestamp: "2026-11-11T03:41:07.1234567Z ".
STAMP = re.compile(r"^\d{4}-\d\d-\d\dT[\d:.]+Z ")
PATTERNS = [
    # cargo test
    re.compile(r"^test (\S+)(?: - should panic)? \.\.\. FAILED$"),
    # node --test (spec reporter) and the app engine's tests
    re.compile(r"^\s*✖ (.+?) \(\d[\d.]*m?s\)$"),
    re.compile(r"^not ok \d+ - (.+)$"),
    # pytest
    re.compile(r"^FAILED (\S+)"),
    # Maven Surefire
    re.compile(r"^\[ERROR\]\s+(\S+)\s.*<<< (?:FAILURE|ERROR)!"),
    # chloedb-sim: a seed whose run broke an invariant (replay it with
    # `chloedb-sim --seed N --trace`)
    re.compile(r"^(seed \d+: \d+ violations?)"),
    # cargo fuzz: the crash file it saved
    re.compile(r"^(artifact_prefix=.*|Test unit written to \S+)$"),
    # cargo deny
    re.compile(r"^error\[([a-z-]+)\]: (.+)$"),
]


ERROR = re.compile(r"^##\[error\](.+)$")


def failures_in_log(text):
    """The failing tests (or other named failures) in one job's log, in order, once each.

    A log that names none gives its first error annotation instead (a build
    that did not compile, a step that timed out)."""
    found = []
    first_error = None
    for raw in text.splitlines():
        line = STAMP.sub("", raw).rstrip()
        if first_error is None and (m := ERROR.match(line)):
            first_error = m.group(1)
        for pattern in PATTERNS:
            m = pattern.search(line)
            if m:
                name = ": ".join(g for g in m.groups() if g)
                if name not in found:
                    found.append(name)
                break
    if not found and first_error:
        found.append(first_error)
    return found


def gh(*args, input=None):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True, input=input).stdout


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--parse":
        with open(sys.argv[2], encoding="utf-8", errors="replace") as f:
            for name in failures_in_log(f.read()):
                print(name)
        return

    repo = os.environ["REPO"]
    run_id = os.environ["RUN_ID"]
    workflow = os.environ["WORKFLOW"]
    conclusion = os.environ["CONCLUSION"]
    sha = os.environ["HEAD_SHA"]
    run_url = os.environ["RUN_URL"]
    dry = os.environ.get("DRY_RUN") == "1"

    title = f"Nightly {workflow} is failing on main"
    issues = json.loads(gh("issue", "list", "--repo", repo, "--state", "open", "--label", LABEL,
                           "--search", f'"{title}" in:title', "--json", "number,title"))
    issue = next((i["number"] for i in issues if i["title"] == title), None)

    if conclusion == "success":
        if issue is None:
            print(f"{workflow} passed and no issue is open.")
            return
        body = f"The nightly {workflow} run passed again on {sha[:10]}: {run_url}. Closing."
        print(f"close #{issue}: {body}")
        if not dry:
            gh("issue", "close", str(issue), "--repo", repo, "--comment", body)
        return

    if conclusion not in ("failure", "timed_out"):
        # cancelled, skipped, neutral, action_required: nothing was learned.
        print(f"{workflow} concluded {conclusion}: no report.")
        return

    raw = gh("api", "--paginate", f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100",
             "--jq", '.jobs[] | select(.conclusion == "failure" or .conclusion == "timed_out") | @json')
    jobs = [json.loads(line) for line in raw.splitlines() if line.strip()]

    lines = [f"The nightly **{workflow}** run on main failed: {run_url}", "",
             f"Commit: {sha}", ""]
    if not jobs:
        lines.append("No job is marked failed (the run itself failed: see its page).")
    for job in jobs:
        lines.append(f"- [{job['name']}]({job['html_url']}) ({job['conclusion']})")
        try:
            log = gh("api", f"repos/{repo}/actions/jobs/{job['id']}/logs")
        except subprocess.CalledProcessError as e:
            lines.append(f"  - (its log could not be read: {e.stderr.strip()[:200]})")
            continue
        names = failures_in_log(log)
        for name in names[:MAX_PER_JOB]:
            lines.append(f"  - `{name}`")
        if len(names) > MAX_PER_JOB:
            lines.append(f"  - and {len(names) - MAX_PER_JOB} more")
        if not names:
            lines.append("  - no failing test named in its log (a build, a step or a timeout: see the job)")
    lines += ["", "This issue closes itself when a nightly run passes. Each failed night adds a comment."]
    body = "\n".join(lines)

    if dry:
        print(f"{'comment on #' + str(issue) if issue else 'open: ' + title}\n\n{body}")
        return
    if issue is None:
        gh("label", "create", LABEL, "--repo", repo, "--color", "B60205",
           "--description", "A scheduled run on main failed", "--force")
        url = gh("issue", "create", "--repo", repo, "--title", title, "--label", LABEL, "--body-file", "-", input=body)
        print(f"opened {url.strip()}")
    else:
        gh("issue", "comment", str(issue), "--repo", repo, "--body-file", "-", input=body)
        print(f"commented on #{issue}")


if __name__ == "__main__":
    main()
