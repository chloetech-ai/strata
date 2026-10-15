"""Compares an engram-bench retrieval summary JSON against a recorded one.
Prints every metric of every method, overall and by question type, with the
difference. Usage: compare_retrieval.py RECORDED.json REPRODUCED.json [--methods bm25,dense,...] [--max-diff-only]"""
import json, sys
rec = json.load(open(sys.argv[1])); rep = json.load(open(sys.argv[2]))
methods = None
if "--methods" in sys.argv:
    methods = sys.argv[sys.argv.index("--methods") + 1].split(",")
keys = ["recall_any@1", "recall_any@5", "recall_any@10", "recall_any@20", "recall_all@10", "ndcg@10", "session_all@5", "pack4k", "pack8k", "pack_brief"]
print(f"recorded {sys.argv[1]} ({rec.get('embedder')}, {rec.get('eligible')} eligible, {rec.get('seconds', 0):.0f} s)")
print(f"reproduced {sys.argv[2]} ({rep.get('embedder')}, {rep.get('eligible')} eligible, {rep.get('seconds', 0):.0f} s)")
maxd = {}
def block(name, a, b):
    print(f"== {name} (n={a.get('n')} / {b.get('n')})")
    for m in (methods or sorted(a)):
        if m == "n" or m not in a or m not in b:
            continue
        cells = []
        for k in keys:
            x, y = a[m].get(k), b[m].get(k)
            if x is None or y is None:
                continue
            d = y - x
            maxd[m] = max(maxd.get(m, 0.0), abs(d))
            cells.append(f"{k} {x:.3f}/{y:.3f}" + (f" ({d:+.3f})" if abs(d) >= 0.0005 else ""))
        print(f"  {m:<18} " + "  ".join(cells))
block("all", rec["all"], rep["all"])
for t in sorted(rec.get("by_type", {})):
    if t in rep.get("by_type", {}):
        block(t, rec["by_type"][t], rep["by_type"][t])
print("max |difference| per method over all metrics and types:", {m: round(v, 4) for m, v in maxd.items()})
