#!/usr/bin/env python3
"""Grades the distiller on the fixed test set in eval/distill/cases.jsonl.

  ./target/release/engram-bench distill --model haiku --repeats 3 --out results/distill/outputs.jsonl
  python3 scripts/distill_eval.py eval/distill/cases.jsonl results/distill/outputs.jsonl

Each case says what a correct output holds, checked by case-insensitive
regular expressions on the notes' text:
  expect        for each entry, some note of one of `kind` matches every pattern
                in `all` (and replaces `replaces`, if given);
  forbid        no note matches any of these;
  forbid_kinds  no note of that kind matches any of its patterns;
  done          these noted requirements are marked done;
  empty         nothing is written down.
An output passes when every check holds. Prints the pass rate per case and
writes a summary next to the outputs.
"""

import json
import os
import re
import sys
from collections import defaultdict


def check(case, out):
    items, fails = out["items"], []
    if out.get("error"):
        return [f"error: {out['error'][:120]}"]
    for e in case.get("expect", []):
        ok = any(
            (not e.get("kind") or it["kind"] in e["kind"])
            and all(re.search(p, it["text"], re.I) for p in e["all"])
            and (not e.get("replaces") or e["replaces"] in it.get("replaces", []))
            for it in items
        )
        if not ok:
            fails.append(f"missing {e.get('kind')} with {e['all']}" + (f" replacing {e['replaces']}" if e.get("replaces") else ""))
    for p in case.get("forbid", []):
        hit = [it["text"] for it in items if re.search(p, it["text"], re.I)]
        if hit:
            fails.append(f"forbidden /{p}/ in: {hit[0][:120]}")
    for kind, pats in case.get("forbid_kinds", {}).items():
        for p in pats:
            hit = [it["text"] for it in items if it["kind"] == kind and re.search(p, it["text"], re.I)]
            if hit:
                fails.append(f"forbidden {kind} /{p}/: {hit[0][:120]}")
    for d in case.get("done", []):
        if d not in out.get("done", []):
            fails.append(f"{d} not marked done")
    if case.get("empty") and items:
        fails.append(f"{len(items)} notes written: {items[0]['text'][:120]}")
    return fails


def main():
    cases_path, outs_path = sys.argv[1], sys.argv[2]
    cases = {c["id"]: c for c in map(json.loads, open(cases_path))}
    outs = [json.loads(l) for l in open(outs_path)]
    by = defaultdict(list)
    for o in outs:
        by[o["id"]].append((o, check(cases[o["id"]], o)))
    total = sum(len(v) for v in by.values())
    passed = sum(1 for v in by.values() for _, f in v if not f)
    summary = {"outputs": total, "passed": passed, "cost_usd": sum(o.get("cost_usd") or 0 for o in outs), "cases": {}}
    print(f"{'case':24} pass  first failure")
    for cid in cases:
        rs = by.get(cid, [])
        ok = sum(1 for _, f in rs if not f)
        first = next((f[0] for _, f in rs if f), "")
        summary["cases"][cid] = {"runs": len(rs), "passed": ok, "about": cases[cid].get("about"), "failures": [f for _, f in rs if f]}
        print(f"{cid:24} {ok}/{len(rs)}   {first[:110]}")
    print(f"\n{passed}/{total} outputs pass; ${summary['cost_usd']:.4f}")
    json.dump(summary, open(os.path.join(os.path.dirname(outs_path), "summary.json"), "w"), indent=1)


if __name__ == "__main__":
    main()
