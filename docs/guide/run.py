#!/usr/bin/env python3
"""Run the code on the documentation's guide pages, as a reader would.

Every guide page on the site (the "guide" pages of build_docs.py's NAV:
docs/guide/*.md and docs/quickstart.md) is a script: its code blocks run
from the top, in one shell, in a new empty directory, with this checkout's
programs on PATH. A page passes when every block succeeds and every output
block matches what the block before it printed.

    python3 docs/guide/run.py                # every guide page
    python3 docs/guide/run.py search rag     # some of them, by page name
    python3 docs/guide/run.py --list         # what would run, block by block
    python3 docs/guide/run.py --keep rag     # keep the directory it ran in
    python3 docs/guide/run.py --update rag   # write what blocks printed into the
                                             # page's output blocks (review the diff)

What a block's info string means (```lang words...):

    sh, bash, console     run in the page's shell (cd and variables carry to
                          later blocks; `cmd &` starts a server, which is
                          stopped when the page ends)
    sh, first line "# install"
                          shown, not run: installs a reader does once (the
                          checkout's own builds are on PATH instead);
                          scripts/check-doc-urls.py checks that what they
                          download exists
    python, js            run as a script (python3, node) in the page's
                          directory
    sql db=FILE           run against the database FILE (in-process, with
                          chloedb-py), printing the rows
    any lang file=NAME    written to NAME in the page's directory, not run
    output                compared with what the block before printed:
                          a line "..." stands for any lines, and a line
                          ending in "..." for any rest of that line
    norun                 shown, not run (say why in the text around it)

A page can say <!-- run: skip REASON --> to be left out (it is listed as
skipped), and <!-- run: needs NAME... --> for programs it needs
(chloedb-server, chloedb, chloedb-crawl, chloedb-sheets, chloectx, node):
a page whose programs are missing fails, unless --allow-missing, which
skips it.

The programs come from each crate's target/{ci,release,debug} (the newest
build), or from the directory CHLOEDB_DOCS_BIN names. Python is this
interpreter, so run it from the virtualenv chloedb-py is installed in.
Each page starts with an empty models folder of its own (CHLOEDB_MODELS in
its directory), as a reader who opens it first has, so a page that uses a
model must fetch it itself. --shared-models DIR keeps the models in DIR
between pages and runs instead, which is quicker and hides a missing fetch.
"""
import argparse
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.dont_write_bytecode = True  # no __pycache__ beside build_docs.py
sys.path.insert(0, str(ROOT / "docs" / "site" / "scripts"))
from build_docs import FENCE, NAV, parse_info, present  # noqa: E402

PROGRAMS = {
    "chloedb-server": "chloedb-server",
    "chloedb": "chloedb",
    "chloedb-crawl": "chloedb-crawl",
    "chloedb-sheets": "chloedb-sheets",
    "chloectx": "chloectx",
}
RUN_LANGS = {"sh", "bash", "console", "shell", "python", "py", "js", "javascript", "sql"}


def guide_pages():
    for _, items in NAV:
        for page, _title, source, kind in items:
            if kind == "guide" and present(page, source):
                yield page, ROOT / source


def line_of(text, pos):
    return text.count("\n", 0, pos) + 1


def steps(source):
    """The page's blocks, as (kind, line, lang, attrs, body)."""
    text = source.read_text()
    out = []
    for m in FENCE.finditer(text):
        lang, attrs, flags = parse_info(m.group("info"))
        indent = m.group("indent")
        body = re.sub(r"^" + re.escape(indent), "", m.group("body"), flags=re.M) if indent else m.group("body")
        line = line_of(text, m.start())
        if "output" in flags:
            out.append(("output", line, lang, attrs, body))
        elif "file" in attrs:
            out.append(("file", line, lang, attrs, body))
        elif "norun" in flags or lang not in RUN_LANGS:
            out.append(("show", line, lang, attrs, body))
        elif lang in ("sh", "bash", "console", "shell") and body.startswith("# install\n"):
            out.append(("install", line, lang, attrs, body))
        else:
            out.append(("run", line, lang, attrs, body))
    return out


def directives(source):
    text = source.read_text()
    skip = re.search(r"<!--\s*run:\s*skip\s+(.*?)\s*-->", text)
    needs = set()
    for m in re.finditer(r"<!--\s*run:\s*needs\s+(.*?)\s*-->", text):
        needs.update(m.group(1).split())
    return (skip.group(1) if skip else None), needs


def find_programs():
    """A directory of links to this checkout's newest build of each program."""
    found = {}
    explicit = os.environ.get("CHLOEDB_DOCS_BIN")
    for name in PROGRAMS:
        if explicit:
            p = pathlib.Path(explicit) / name
            if p.exists():
                found[name] = p
            continue
        # Each crate's own target directory, or another's (a build with a
        # shared CARGO_TARGET_DIR puts every program in one).
        cands = [ROOT / crate / "target" / prof / name for crate in PROGRAMS for prof in ("ci", "release", "debug")]
        cands = [c for c in cands if c.exists()]
        if cands:
            found[name] = max(cands, key=lambda c: c.stat().st_mtime)
    return found


def compare(expected, actual):
    """Whether output `actual` matches `expected`, with ... as a wildcard."""
    exp = [l.rstrip() for l in expected.rstrip("\n").split("\n")]
    act = [l.rstrip() for l in actual.rstrip("\n").split("\n")]
    pat = ""
    for l in exp:
        if l.strip() == "...":
            pat += r"(?:[^\n]*\n)*?"
        elif l.endswith("..."):
            pat += re.escape(l[:-3]) + r"[^\n]*\n"
        else:
            pat += re.escape(l) + r"\n"
    return re.fullmatch(pat, "\n".join(act) + "\n") is not None


SQL_RUNNER = r'''
import sys, chloedb
db = chloedb.open(sys.argv[1], read_only=True)
sql = open(sys.argv[2]).read()
for stmt in [s.strip() for s in sql.split(";") if s.strip()]:
    r = db.query(stmt)
    cols = list(r.columns)
    rows = [["" if v is None else (f"{v:.4g}" if isinstance(v, float) else str(v)) for v in row] for row in r.rows]
    w = [max([len(c)] + [len(row[i]) for row in rows]) for i, c in enumerate(cols)]
    print(" | ".join(c.ljust(w[i]) for i, c in enumerate(cols)).rstrip())
    print("-+-".join("-" * x for x in w))
    for row in rows:
        print(" | ".join(v.ljust(w[i]) for i, v in enumerate(row)).rstrip())
'''


def script_for(page, source, blocks, work):
    """The bash script that runs a page's blocks, writing its files first."""
    run = work / ".run"
    run.mkdir()
    (run / "sql.py").write_text(SQL_RUNNER)
    rel = source.relative_to(ROOT).as_posix()
    # .run is named by its full path: a block may cd elsewhere.
    sh = ["set -eo pipefail", f"RUN={sh_quote(str(run))}", 'exec 3>&1 4>&2', 'step=""',
          'on_exit() { st=$?; exec 1>&3 2>&4; if [ $st -ne 0 ] && [ -n "$step" ]; then echo "--- output of the failed block ---"; cat "$RUN/out-$step"; echo "--- $where failed (exit $st)"; fi; exit $st; }',
          "trap on_exit EXIT"]
    last = None
    for i, (kind, line, lang, attrs, body) in enumerate(blocks):
        where = f"{rel}:{line}"
        if kind == "file":
            dest = work / attrs["file"]
            dest.parent.mkdir(parents=True, exist_ok=True)
            # Written when the page reaches it, as a reader would.
            (run / f"file-{i}").write_text(body)
            sh.append(f'mkdir -p "$(dirname {sh_quote(attrs["file"])})" && cp "$RUN/file-{i}" {sh_quote(attrs["file"])}')
        elif kind == "run":
            if lang in ("python", "py"):
                (run / f"block-{i}.py").write_text(body)
                # From stdin, as pasted: imports find the page's files.
                code = f'python3 - < "$RUN/block-{i}.py"'
            elif lang in ("js", "javascript"):
                (run / f"block-{i}.mjs").write_text(body)
                code = f'node "$RUN/block-{i}.mjs"'
            elif lang == "sql":
                if "db" not in attrs:
                    raise SystemExit(f"{where}: a sql block runs against db=FILE, or says norun")
                (run / f"block-{i}.sql").write_text(body)
                code = f'python3 "$RUN/sql.py" {sh_quote(attrs["db"])} "$RUN/block-{i}.sql"'
            else:
                code = body
            sh.append(f'step={i}; where={sh_quote(where)}; echo "▶ {where}" >&4; exec >"$RUN/out-{i}" 2>&1')
            sh.append(code)
            sh.append(f'exec 1>&3 2>&4; cat "$RUN/out-{i}"; step=""')
            last = i
        elif kind == "output":
            if last is None:
                raise SystemExit(f"{where}: an output block with no block before it")
            (run / f"expect-{i}").write_text(body)
            sh.append(f'python3 {sh_quote(str(HERE / "run.py"))} --compare "$RUN/expect-{i}" "$RUN/out-{last}" {sh_quote(where)}')
    return "\n".join(sh) + "\n"


def sh_quote(s):
    return "'" + str(s).replace("'", "'\\''") + "'"


def run_page(page, source, programs, args):
    skip, needs = directives(source)
    rel = source.relative_to(ROOT).as_posix()
    if skip:
        return "skip", f"{rel}: {skip}"
    missing = sorted(n for n in needs if n not in programs and not (n == "node" and shutil.which("node")))
    if missing:
        return ("skip" if args.allow_missing else "fail"), f"{rel}: needs {' '.join(missing)}, not built"
    blocks = steps(source)
    work = pathlib.Path(tempfile.mkdtemp(prefix=f"chloedb-docs-{page}-"))
    bindir = work / ".bin"
    bindir.mkdir()
    for name, path in programs.items():
        (bindir / name).symlink_to(path)
    env = dict(os.environ)
    env["PATH"] = os.pathsep.join([str(bindir), str(pathlib.Path(sys.executable).parent), env.get("PATH", "")])
    # A models folder of the page's own, empty, unless --shared-models: a page
    # that uses a model fetches it, as a reader who starts there must.
    env["CHLOEDB_MODELS"] = str(pathlib.Path(args.shared_models).expanduser() if args.shared_models else work / ".models")
    env.pop("STRATA_MODELS", None)
    pathlib.Path(env["CHLOEDB_MODELS"]).mkdir(parents=True, exist_ok=True)
    env["CHLOEDB_DOCS_RUN"] = "1"
    env.pop("ANTHROPIC_API_KEY", None)  # pages print the prompt they would send, not a model's answer
    if args.update:
        env["CHLOEDB_DOCS_UPDATE"] = "1"
    script = script_for(page, source, blocks, work)
    (work / ".run" / "page.sh").write_text(script)
    t0 = time.time()
    # Output to a file, not a pipe: a server a page starts in the background
    # keeps the page's descriptors, and would hold a pipe open after the page ends.
    log = work / ".run" / "page.log"
    with open(log, "w") as f:
        proc = subprocess.Popen(["bash", ".run/page.sh"], cwd=work, env=env, start_new_session=True,
                                stdin=subprocess.DEVNULL, stdout=f, stderr=subprocess.STDOUT)
    try:
        status = proc.wait(timeout=args.timeout)
        out = log.read_text(errors="replace")
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
        out = log.read_text(errors="replace") + f"\n--- {rel} took longer than {args.timeout}s"
        status = -1
    finally:
        # The servers a page started, and their children.
        try:
            os.killpg(proc.pid, signal.SIGTERM)
            time.sleep(0.5)
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    took = time.time() - t0
    if args.update:
        update_outputs(source, blocks, work)
    if args.verbose or status != 0:
        print(out)
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)
    else:
        print(f"kept {work}")
    n_run = sum(1 for b in blocks if b[0] in ("run", "output"))
    n_show = sum(1 for b in blocks if b[0] in ("show", "install"))
    return ("pass" if status == 0 else "fail"), f"{rel}: {n_run} blocks run, {n_show} shown only, {took:.1f}s"


def update_outputs(source, blocks, work):
    """Write what each block printed into the output block under it, where they differ."""
    lines = source.read_text().split("\n")
    changed = 0
    # From the end, so that earlier line numbers stay put.
    for i in reversed(range(len(blocks))):
        kind, line, lang, attrs, body = blocks[i]
        printed = work / ".run" / f"expect-{i}.printed"
        if kind != "output" or not printed.exists():
            continue
        start = line  # the fence line is line `line` (1-based), its body starts after it
        end = start + body.count("\n")
        new = printed.read_text().rstrip("\n").split("\n")
        lines[start:end] = new
        changed += 1
    if changed:
        source.write_text("\n".join(lines))
        print(f"updated {changed} output blocks of {source.relative_to(ROOT)}")


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "--compare":
        expected, actual, where = sys.argv[2], sys.argv[3], sys.argv[4]
        e, a = open(expected).read(), open(actual).read()
        if os.environ.get("CHLOEDB_DOCS_UPDATE") and (not compare(e, a) or e.strip() == "..."):
            # --update: what the block printed, for run_page to put on the page
            with open(expected + ".printed", "w") as f:
                f.write(a)
        elif not compare(e, a):
            print(f"--- {where}: the output differs from the page's\n--- expected:\n{e}--- printed:\n{a}", file=sys.stderr)
            sys.exit(1)
        return
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("pages", nargs="*", help="page names (as in docs/site/docs/NAME.html); default all")
    ap.add_argument("--list", action="store_true", help="list each page's blocks and what is done with them")
    ap.add_argument("--keep", action="store_true", help="keep each page's directory")
    ap.add_argument("--verbose", "-v", action="store_true", help="print each page's output")
    ap.add_argument("--allow-missing", action="store_true", help="skip pages whose programs are not built")
    ap.add_argument("--update", action="store_true", help="write what each block printed into the page's output blocks that differ (review the diff)")
    ap.add_argument("--timeout", type=int, default=900, help="seconds a page may take (default 900)")
    ap.add_argument("--shared-models", metavar="DIR", help="keep the models in DIR between pages (quicker; hides a page that does not fetch its model)")
    args = ap.parse_args()
    pages = list(guide_pages())
    if args.pages:
        known = {p for p, _ in pages}
        for p in args.pages:
            if p not in known:
                raise SystemExit(f"no guide page {p}: {', '.join(sorted(known))}")
        pages = [(p, s) for p, s in pages if p in args.pages]
    if args.list:
        for page, source in pages:
            skip, needs = directives(source)
            print(f"{source.relative_to(ROOT)}" + (f"  (skipped: {skip})" if skip else "") + (f"  needs {' '.join(sorted(needs))}" if needs else ""))
            for kind, line, lang, attrs, body in steps(source):
                first = body.strip().split("\n")[0][:70]
                print(f"  {line:5d}  {kind:8s} {lang or '-':7s} {attrs.get('file', attrs.get('db', '')):18s} {first}")
        return
    programs = find_programs()
    results = []
    for page, source in pages:
        status, msg = run_page(page, source, programs, args)
        print(f"{status.upper():5s} {msg}", flush=True)
        results.append(status)
    failed = results.count("fail")
    print(f"{results.count('pass')} passed, {failed} failed, {results.count('skip')} skipped")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
