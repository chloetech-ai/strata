#!/usr/bin/env python3
"""Were the files named before an edit the ones edited next? Reads the
runs' transcripts in the bench's work directory and writes edits.json:

- of the files the hook named after an edit of A, how many the run then
  edited, and how many the lane without next files edited after its own
  first edit of A in the same task (whether the hint changed anything);
- the kinds of the notes shown with them.

Run from engram/:

  python3 results/memory_bench_next/edits.py /home/user/bench/work
"""

import collections
import glob
import gzip
import json
import os
import re
import sys

DIR = "results/memory_bench_next"
HINT = re.compile(r"edits of (\S+) were usually followed by edits of: (.*?)\.\n")
NOTE = re.compile(r"Engram note on .*?:\n\[[^\]]*\] [^\n]*?· (\w+)( \(\w+\))? ·")


def parse(path):
    """The run's hints (A, [B, ...]), its edits in order, and its note kinds."""
    root, events, kinds = None, [], collections.Counter()
    for line in gzip.open(path, "rt"):
        try:
            d = json.loads(line)
        except ValueError:
            continue
        root = root or d.get("cwd")
        a = d.get("attachment") or {}
        if a.get("type") == "hook_additional_context":
            for c in a.get("content", []):
                for m in HINT.finditer(c):
                    events.append(("hint", (m.group(1), [x.strip() for x in m.group(2).split(",")])))
                # The notes after the hint are the next files' own.
                i = c.find("usually followed")
                if i >= 0:
                    kinds.update(m.group(1) for m in NOTE.finditer(c, i))
        msg = d.get("message") or {}
        if d.get("type") == "assistant" and isinstance(msg.get("content"), list):
            for b in msg["content"]:
                if b.get("type") == "tool_use" and b.get("name") in ("Edit", "MultiEdit", "Write"):
                    p = b["input"].get("file_path", "")
                    if root and p.startswith(root):
                        p = p[len(root):].lstrip("/")
                    events.append(("edit", p))
    return events, kinds


def edited_after(events, i):
    return {e[1] for e in events[i + 1:] if e[0] == "edit"}


work = sys.argv[1]
out = {"named": 0, "edited": 0, "without_named": 0, "without_edited": 0, "note_kinds": collections.Counter(), "by_project": {}}
for sc in ["bakehouse", "kestrel", "slotwise", "stockroom"]:
    logs = os.path.join(work, sc, "engram", "logs")
    p = {"named": 0, "edited": 0, "without_named": 0, "without_edited": 0}
    for f in sorted(glob.glob(f"{logs}/run_engram_next_*.transcript.jsonl.gz")):
        task = re.search(r"run_engram_next_(.*)_\d+\.transcript", f).group(1)
        without = [parse(g)[0] for g in glob.glob(f"{logs}/run_{task}_*.transcript.jsonl.gz") if re.fullmatch(rf"run_{re.escape(task)}_\d+\.transcript\.jsonl\.gz", os.path.basename(g))]
        events, kinds = parse(f)
        out["note_kinds"].update(kinds)
        for i, (k, v) in enumerate(events):
            if k != "hint":
                continue
            a, named = v
            for b in named:
                p["named"] += 1
                p["edited"] += b in edited_after(events, i)
                for w in without:
                    first = next((j for j, e in enumerate(w) if e == ("edit", a)), None)
                    if first is not None:
                        p["without_named"] += 1
                        p["without_edited"] += b in edited_after(w, first)
    out["by_project"][sc] = p
    for k in p:
        out[k] += p[k]
out["edited_share"] = out["edited"] / out["named"]
out["without_edited_share"] = out["without_edited"] / out["without_named"]
json.dump(out, open(f"{DIR}/edits.json", "w"), indent=1)
print(json.dumps(out, indent=1))
