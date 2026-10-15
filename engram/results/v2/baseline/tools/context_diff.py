"""Compares two engram-structure --dump context files (JSONL, .gz allowed) by
question: how many contexts are identical, and how the lengths differ.
Usage: context_diff.py A B"""
import gzip, json, sys
def load(p):
    op = gzip.open if p.endswith(".gz") else open
    with op(p, "rt") as f:
        return {r["qid"]: r for r in map(json.loads, f) if r}
a, b = load(sys.argv[1]), load(sys.argv[2])
common = sorted(set(a) & set(b))
same = sum(a[q]["context"] == b[q]["context"] for q in common)
la = sum(len(a[q]["context"]) for q in common) / max(1, len(common))
lb = sum(len(b[q]["context"]) for q in common) / max(1, len(common))
print(f"{len(a)} vs {len(b)} questions, {len(common)} in both; identical contexts {same}/{len(common)}; mean length {la:.0f} vs {lb:.0f} chars")
