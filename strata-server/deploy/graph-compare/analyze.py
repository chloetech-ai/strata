"""Tables from compare.sh's reports: python analyze.py OUT [OTHER_OUT...] > tables.md

The tables are OUT's; with more passes, a last table puts every pass's
client p50 side by side.
"""

import json
import re
import sys
from pathlib import Path

ORDER = ["strata", "strata-grpc", "strata-edges", "strata-edges-grpc", "neo4j", "memgraph", "falkordb", "arangodb"]
NAMES = {
    "strata": "Strata (EDGES field)",
    "strata-grpc": "Strata (EDGES field, gRPC)",
    "strata-edges": "Strata (edge table)",
    "strata-edges-grpc": "Strata (edge table, gRPC)",
    "neo4j": "Neo4j",
    "memgraph": "Memgraph",
    "falkordb": "FalkorDB",
    "arangodb": "ArangoDB",
}
KINDS = [
    ("hops_both_1", "1 hop, both ways"),
    ("hops_both_2", "2 hops, both ways"),
    ("hops_out_3", "3 hops out"),
    ("path", "shortest path (≤ 10)"),
    ("ppr_0.0001", "PPR top 20, eps 1e-4 (GDS: defaults)"),
    ("ppr_1e-05", "PPR top 20, eps 1e-5 (GDS: 100 iterations)"),
    ("ppr_1e-06", "PPR top 20, eps 1e-6"),
]


def ms(x):
    if x is None:
        return "–"
    return f"{x:.2f}" if x < 10 else f"{x:.0f}" if x >= 100 else f"{x:.1f}"


def main():
    dirs = [Path(x) for x in sys.argv[1:]]
    out = dirs[0]
    runs = {e: json.load(open(out / f"run-{e}.json")) for e in ORDER if (out / f"run-{e}.json").exists()}
    print("## Load\n")
    print("| engine | nodes (s) | edges (s) | total (s) | memory after the queries | data on disk |")
    print("|---|---:|---:|---:|---:|---:|")
    for e, r in runs.items():
        ld = r["load"]
        mem = json.load(open(out / f"mem-{e}.json"))["mem"].split(" / ")[0] if (out / f"mem-{e}.json").exists() else "–"
        disk = "–"
        if (out / f"disk-{e}.txt").exists():
            m = re.match(r"(\d+)", (out / f"disk-{e}.txt").read_text())
            disk = f"{int(m.group(1)) / 1024:.0f} MiB" if m else "–"
        extra = f" (+ {ld['gds_project_s']:.1f} s GDS projection)" if "gds_project_s" in ld else ""
        print(f"| {NAMES[e]} | {ms(ld.get('nodes_s'))} | {ms(ld.get('edges_s'))} | {ms(ld['total_s'])}{extra} | {mem} | {disk} |")
    for title, key in [("client", "client_ms"), ("engine's own time", "server_ms")]:
        print(f"\n## Latency, {title}: p50 / p95 in ms\n")
        print("| query | " + " | ".join(NAMES[e] for e in runs) + " |")
        print("|---|" + "---:|" * len(runs))
        for k, label in KINDS:
            cells = []
            for e, r in runs.items():
                q = r["queries"].get(k)
                if q is None:
                    cells.append("not run")
                elif q.get("unavailable"):
                    cells.append("n/a")
                elif q[key] is None:
                    cells.append("–")
                else:
                    c = f"{ms(q[key]['p50'])} / {ms(q[key]['p95'])}"
                    if q["done"] < q["asked"]:
                        c += f" ({q['done']}/{q['asked']} in budget)"
                    cells.append(c)
            print(f"| {label} | " + " | ".join(cells) + " |")
    print("\n## Answers\n")
    print("| query | " + " | ".join(NAMES[e] for e in runs) + " |")
    print("|---|" + "---:|" * len(runs))
    for k, label in KINDS:
        cells = []
        for e, r in runs.items():
            q = r["queries"].get(k)
            if q is None or q.get("unavailable"):
                cells.append("–")
            elif k.startswith("ppr"):
                cells.append(f"recall@20 {q['recall_at_20']:.3f}")
            else:
                cells.append(f"{q['done'] - q['wrong']}/{q['done']} right")
        print(f"| {label} | " + " | ".join(cells) + " |")
    if len(dirs) > 1:
        passes(dirs)


def passes(dirs):
    runs = [{e: json.load(open(d / f"run-{e}.json")) for e in ORDER if (d / f"run-{e}.json").exists()} for d in dirs]
    engines = [e for e in ORDER if e in runs[0]]
    print(f"\n## Client p50 in each pass ({', '.join(d.name for d in dirs)}), ms\n")
    print("| query | " + " | ".join(NAMES[e] for e in engines) + " |")
    print("|---|" + "---:|" * len(engines))
    for k, label in KINDS:
        cells = []
        for e in engines:
            vals = []
            for r in runs:
                q = r.get(e, {}).get("queries", {}).get(k)
                vals.append("–" if not q or q.get("unavailable") or not q.get("client_ms") else ms(q["client_ms"]["p50"]))
            cells.append(" / ".join(vals))
        print(f"| {label} | " + " | ".join(cells) + " |")
    print("\n| load, total (s) | " + " | ".join(" / ".join(ms(r[e]["load"]["total_s"]) if e in r else "–" for r in runs) for e in engines) + " |")


if __name__ == "__main__":
    main()
