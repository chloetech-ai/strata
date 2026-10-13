"""Assemble report/strata.html from the template, the narrative fragments,
the shared style and chart helpers of posting_bench's reports, and results/.
Usage: python3 scripts/build_report.py"""
import datetime
import json
import pathlib

root = pathlib.Path(__file__).resolve().parent.parent
results = root / "results"
report = root / "report"
shared = root.parent / "posting_bench" / "report"
COLLECTIONS = ["scifact", "fiqa", "trec-covid", "quora"]


def load(path):
    p = results / path
    return json.loads(p.read_text()) if p.exists() else None


def slim_system(d):
    """Build and per-kind latency of a workload run (no per-query outputs)."""
    if not d:
        return None
    keep = {k: d.get(k) for k in ("system", "collection", "n_docs", "build", "config")}
    keep["kinds"] = [{k: v for k, v in r.items() if k in ("kind", "queries", "warm", "cold", "cold_pages", "open_pages", "ndcg10", "mean_results")} for r in d.get("kinds", [])]
    return keep


def slim_planner(d, rows=True):
    """Planner summary per kind; per-query rows only where a chart needs them."""
    if not d:
        return None
    out = {}
    for kind, k in d["kinds"].items():
        out[kind] = {x: k[x] for x in ("queries", "regret_geomean", "regret_p90", "within_1_25", "auto_recall_mean", "total_ms", "oracle_ms")}
        if rows:
            out[kind]["rows"] = [{"target": r["target"], "plans": {p["plan"]: [round(p["ms"], 4), round(p["recall"], 3)] for p in r["plans"]}, "regret": r["regret"]} for r in k["rows"]]
    return out


def slim_vector(d):
    if not d:
        return None
    return {
        "n": d.get("n"),
        "ndcg10_exact": d.get("ndcg10_exact"),
        "kernels": d.get("kernels"),
        "indexes": [{k: v for k, v in ix.items() if k in ("kind", "build", "curve", "cold", "info", "sections", "warm_hints")} for ix in d.get("indexes", [])],
        "filtered": d.get("filtered"),
    }


def slim_text(d):
    if not d:
        return None
    keep = {k: d.get(k) for k in ("documents", "postings", "positions", "terms", "ndcg@10")}
    keep["formats"] = [
        {
            "format": f.get("format"),
            "build_secs": f.get("build_secs"),
            "sizes": {"bits_per_posting": f["sizes"]["bits_per_posting"], "bits_per_position": f["sizes"].get("bits_per_position")},
            "warm": {"boolean": f["warm"].get("boolean"), "bm25": [{k: b.get(k) for k in ("algo", "filter", "mean_us", "p50_us", "p99_us")} for b in f["warm"].get("bm25", [])]},
        }
        for f in d.get("formats", [])
    ]
    return keep


data = {"generated": datetime.date.today().isoformat(), "collections": {}}
for c in COLLECTIONS:
    data["collections"][c] = {
        "strata": slim_system(load(f"strata/strata_{c}.json")),
        "strata_ivf": slim_system(load(f"strata/strata_ivf_{c}.json")),
        "sqlite": slim_system(load(f"baselines/sqlite_{c}.json")),
        "tantivy": slim_system(load(f"baselines/tantivy_{c}.json")),
        "hybrid": load(f"strata/hybrid_{c}.json"),
        "planner": slim_planner(load(f"strata/planner_{c}.json")),
        "planner_ivf": slim_planner(load(f"strata/planner_ivf_{c}.json"), rows=False),
        "vector": slim_vector(load(f"vector/{c}.json")),
        "text": slim_text(load(f"text/{c}.json")),
        "readahead": slim_system(load(f"strata/strata_readahead_{c}.json")),
        "noreadahead": slim_system(load(f"strata/strata_noreadahead_{c}.json")),
        "showcase": load(f"strata/showcase_{c}.json"),
    }
data["storage"] = {n: load(f"storage/{n}.json") for n in ("wal", "bulk", "open", "flush", "merge", "space", "pk", "snapshot")}

style = (shared / "template.html").read_text()
style = style[style.index("<style>") + len("<style>"):style.index("</style>")]
page = (report / "template.html").read_text().replace("/*__STYLE__*/", style)
page = page.replace("/*__DATA__*/null", json.dumps(data, separators=(",", ":")))
page = page.replace("/*__CHARTS__*/", (shared / "charts.js").read_text() + "\n" + (report / "diagrams.js").read_text())
page = page.replace("/*__SCRIPT__*/", (report / "report.js").read_text() if (report / "report.js").exists() else "")
for marker, fragment in [("<!--FINDINGS-->", "findings.html"), ("<!--RESULTS-->", "results.html"), ("<!--VERDICT-->", "verdict.html")]:
    f = report / fragment
    page = page.replace(marker, f.read_text() if f.exists() else "")
(report / "strata.html").write_text(page)
print(f"wrote {report / 'strata.html'} ({len(page) / 1024:.0f} KiB)")
