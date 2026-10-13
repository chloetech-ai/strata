"""Assemble report/graph.html (graph indexes and graph queries in Strata)
from report/graph/, the shared style and chart helpers of posting_bench's
reports, and results/graph/.
Usage: python3 scripts/build_graph_report.py"""
import datetime
import json
import pathlib

root = pathlib.Path(__file__).resolve().parent.parent
results = root / "results" / "graph"
src = root / "report" / "graph"
shared = root.parent / "posting_bench" / "report"


def load(name):
    p = results / name
    return json.loads(p.read_text()) if p.exists() else None


def strip(r):
    return {k: v for k, v in r.items() if k != "sizes"} if r else None


arxiv, arxiv_v1 = load("latency_arxiv.json"), load("latency_arxiv_v1.json")
synth, synth_v1 = load("synthetic_1000000.json"), load("synthetic_1000000_v1.json")
data = {
    "generated": datetime.date.today().isoformat(),
    "load": load("load_arxiv.json"),
    "speed": {"arxiv": arxiv, "synthetic": strip(synth)},
    "history": {
        "arxiv": {"before": arxiv_v1 and arxiv_v1["results"], "after": arxiv and arxiv["results"]},
        "synthetic": {"before": synth_v1 and synth_v1["results"], "after": synth and synth["results"]},
    },
    "quality": load("quality_stats.json"),
}
style = (shared / "template.html").read_text()
style = style[style.index("<style>") + len("<style>"):style.index("</style>")]
page = (src / "template.html").read_text().replace("/*__STYLE__*/", style)
page = page.replace("/*__DATA__*/null", json.dumps(data, separators=(",", ":")))
page = page.replace("/*__CHARTS__*/", (shared / "charts.js").read_text() + "\n" + (src / "diagrams.js").read_text())
page = page.replace("/*__SCRIPT__*/", (src / "report.js").read_text())
for marker, fragment in [("<!--FINDINGS-->", "findings.html"), ("<!--BODY-->", "body.html")]:
    page = page.replace(marker, (src / fragment).read_text())
out = root / "report" / "graph.html"
out.write_text(page)
print(f"wrote {out} ({len(page) / 1024:.0f} KiB)")
