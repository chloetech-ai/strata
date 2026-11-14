#!/usr/bin/env python3
"""Whether a pull request's one-way-door box matches its diff (workflow
One-way doors, .github/workflows/doors.yml).

The doors are the sections of .github/CODEOWNERS that start with a line
`# door: <name>`: every path up to the next `# door:` or `# not a door:`
line belongs to that door. The box is the template's
(.github/pull_request_template.md):

  - [ ] No one-way door
  - [ ] Yes: <which door, and what happens to existing databases,
        replicas and clients>

The check fails when
  - the diff touches a door's path and "Yes" is not ticked (whether "No
    one-way door" is ticked or nothing is),
  - "Yes" is ticked with nothing said after it about which door,
  - both boxes are ticked.
It passes, with a notice, when "Yes" is ticked and no door's path is
touched: a door can be a behaviour outside these paths, and the notice
asks whether CODEOWNERS should name it. A pull request that touches no
door and ticks nothing (a dependency bump) passes.

It prints a receipt (the doors, their paths, the box) to stdout and, in
Actions, to the job's summary.

  door_check.py --body FILE --codeowners .github/CODEOWNERS < changed-paths
  door_check.py --codeowners .github/CODEOWNERS --check-paths   # every door's path exists
  door_check.py --self-test
"""
import argparse
import os
import re
import sys

DOOR = re.compile(r"^#\s*door:\s*(.+?)\s*$")
NOT_DOOR = re.compile(r"^#\s*not a door\b")
BOX = re.compile(r"^\s*[-*]\s*\[( |x|X)\]\s*(No one-way door|Yes)\b:?\s*(.*)$")


def doors_of(codeowners_text):
    """[(door, pattern)] in file order."""
    out, door = [], None
    for line in codeowners_text.splitlines():
        s = line.strip()
        m = DOOR.match(s)
        if m:
            door = m.group(1)
            continue
        if NOT_DOOR.match(s):
            door = None
            continue
        if not s or s.startswith("#") or door is None:
            continue
        out.append((door, s.split()[0]))
    return out


def pattern_re(pat):
    """A CODEOWNERS pattern (the subset this file uses) as a regex over
    repository paths: `/a/b` anchored, `dir/` everything under it, `*`
    within one segment; a pattern without a leading slash matches at any
    depth."""
    anchored = pat.startswith("/")
    p = pat.strip("/")
    body = "".join("[^/]*" if c == "*" else re.escape(c) for c in p)
    start = "^" if anchored else r"(^|.*/)"
    return re.compile(start + body + r"(/.*)?$")


def touched(doors, paths):
    """{door: [paths]} of the doors the paths touch."""
    compiled = [(d, pattern_re(p)) for d, p in doors]
    out = {}
    for path in paths:
        path = path.strip()
        if not path:
            continue
        for d, rx in compiled:
            if rx.match(path):
                out.setdefault(d, []).append(path)
                break
    return out


def read_box(body):
    """(no ticked, yes ticked, what was said after Yes)."""
    no = yes = False
    said = []
    lines = (body or "").replace("\r\n", "\n").split("\n")
    for i, line in enumerate(lines):
        m = BOX.match(line)
        if not m:
            continue
        ticked = m.group(1) in "xX"
        if m.group(2) == "Yes":
            yes = yes or ticked
            if ticked:
                said.append(m.group(3).strip())
                # What follows, to the next heading or box.
                for nxt in lines[i + 1:]:
                    if nxt.lstrip().startswith("#") or BOX.match(nxt):
                        break
                    said.append(nxt.strip())
        else:
            no = no or ticked
    said = " ".join(s for s in said if s).strip()
    # The template's own comment is not an answer.
    said = re.sub(r"<!--.*?-->", "", said, flags=re.S).strip()
    return no, yes, said


def judge(doors, paths, body):
    """(ok, errors, notices, receipt lines)."""
    hit = touched(doors, paths)
    no, yes, said = read_box(body)
    errors, notices = [], []
    if no and yes:
        errors.append('Both boxes are ticked: tick "No one-way door" or "Yes", not both.')
    if hit and not yes:
        names = ", ".join(sorted(hit))
        how = 'ticks "No one-way door"' if no else "ticks neither box"
        errors.append(f'This pull request touches a one-way door ({names}) and {how}. Tick "Yes:" and say which door, and what happens to existing databases, replicas and clients; it then waits for a human review.')
    if yes and not said:
        errors.append('"Yes" is ticked with nothing after it: say which door, and what happens to existing databases, replicas and clients.')
    if yes and not hit:
        notices.append('"Yes" is ticked and no path of .github/CODEOWNERS\' doors is touched. If the door is a path, add it to CODEOWNERS under its `# door:` section.')
    receipt = ["### One-way doors", ""]
    box = "both" if no and yes else "Yes" if yes else "No one-way door" if no else "neither"
    receipt.append(f"Box: **{box}**" + (f" ({said})" if yes and said else ""))
    receipt.append("")
    if hit:
        for d in sorted(hit):
            receipt.append(f"- **{d}**: " + ", ".join(f"`{p}`" for p in hit[d]))
    else:
        receipt.append("No door's path is touched.")
    receipt.append("")
    receipt.append("Result: " + ("**fails**: " + " ".join(errors) if errors else "passes"))
    return not errors, errors, notices, receipt


def check_paths(doors, root):
    """Door patterns that match nothing in the checkout (a file moved and
    CODEOWNERS was not told)."""
    files = []
    for dp, dns, fns in os.walk(root):
        dns[:] = [d for d in dns if d not in (".git", "target", "node_modules", ".venv")]
        for f in fns:
            files.append(os.path.relpath(os.path.join(dp, f), root).replace(os.sep, "/"))
    return [f"{d}: {p}" for d, p in doors if not any(pattern_re(p).match(f) for f in files)]


def self_test():
    co = """# header
/.github/ @x
# door: format
/a/storage/wal.rs @x
/a/segment/ @x
/a/fixtures/format*/ @x
# door: auth
/s/auth.rs @x
# not a door: gate
/.github/ @x
"""
    doors = doors_of(co)
    assert doors == [("format", "/a/storage/wal.rs"), ("format", "/a/segment/"), ("format", "/a/fixtures/format*/"), ("auth", "/s/auth.rs")], doors
    assert touched(doors, ["a/storage/wal.rs", "a/segment/x/y.rs", "a/fixtures/format3/f.bin", "a/storage/wal.rs.bak", "s/auth.rs", ".github/x", "b/a/segment/z"]) == {
        "format": ["a/storage/wal.rs", "a/segment/x/y.rs", "a/fixtures/format3/f.bin"], "auth": ["s/auth.rs"]}
    tpl = "## One-way doors\n\nDoes this...\n\n- [{n}] No one-way door\n- [{y}] Yes:{said}\n\n## Tests\n\nran it\n"
    body = lambda n=" ", y=" ", said="": tpl.format(n=n, y=y, said=said)
    ok = lambda paths, b: judge(doors, paths, b)[0]
    # No door touched.
    assert ok(["x.rs"], body(n="x"))
    assert ok(["x.rs"], body())                       # a dependency bump ticks nothing
    assert ok(["x.rs"], "")                           # nor has a body
    assert ok(["x.rs"], body(y="x", said=" the rollup layout")) and judge(doors, ["x.rs"], body(y="x", said=" r"))[2]
    # A door touched.
    assert not ok(["s/auth.rs"], body(n="x"))
    assert not ok(["s/auth.rs"], body())
    assert not ok(["s/auth.rs"], "")
    assert not ok(["s/auth.rs"], body(y="x"))          # Yes with nothing said
    assert ok(["s/auth.rs"], body(y="X", said=" authentication: keys of a new kind; older servers refuse them"))
    assert ok(["s/auth.rs"], body(y="x").replace("- [x] Yes:\n", "- [x] Yes:\n  authentication, said on the next line\n"))
    assert not ok(["s/auth.rs"], body(y="x", said=" <!-- which -->"))
    # Both ticked.
    assert not ok(["x.rs"], body(n="x", y="x", said=" auth"))
    # CRLF bodies, as GitHub sends some.
    assert ok(["s/auth.rs"], body(y="x", said=" auth").replace("\n", "\r\n"))
    # The repository's own CODEOWNERS parses into doors, and every door's path exists.
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(os.path.dirname(here))
    real = doors_of(open(os.path.join(root, ".github", "CODEOWNERS")).read())
    assert {d for d, _ in real} >= {"on-disk format", "shard protocol and replication", "authentication"}, real
    assert not any(p == "/.github/" for _, p in real), "the merge gate is not a door"
    missing = check_paths(real, root)
    assert not missing, missing
    print("ok")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--body", help="a file holding the pull request's description")
    ap.add_argument("--codeowners", default=".github/CODEOWNERS")
    ap.add_argument("--check-paths", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()
    if a.self_test:
        self_test()
        return 0
    doors = doors_of(open(a.codeowners).read())
    if not doors:
        print(f"::error::{a.codeowners} names no door (no `# door:` section)")
        return 1
    if a.check_paths:
        missing = check_paths(doors, ".")
        for m in missing:
            print(f"::error file={a.codeowners}::a door's path matches no file: {m}")
        return 1 if missing else 0
    body = open(a.body, encoding="utf-8").read() if a.body else ""
    ok, errors, notices, receipt = judge(doors, sys.stdin.read().splitlines(), body)
    print("\n".join(receipt))
    for e in errors:
        print(f"::error::{e}")
    for n in notices:
        print(f"::notice::{n}")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write("\n".join(receipt) + "\n")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
