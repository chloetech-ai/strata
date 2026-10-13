"""Assemble report/engram.html from the template, the shared style and chart
helpers of posting_bench's reports, and results/.
Usage: python3 scripts/build_report.py"""
import datetime
import json
import pathlib

root = pathlib.Path(__file__).resolve().parent.parent
results = root / "results"
report = root / "report"
shared = root.parent / "posting_bench" / "report"


def load(path):
    p = results / path
    return json.loads(p.read_text()) if p.exists() else None


def slim_retrieval(d):
    if not d:
        return None
    return {k: d[k] for k in ("dataset", "embedder", "ctx", "units", "questions", "eligible", "seconds", "all", "by_type") if k in d}


def probes(name):
    rows = load(f"{name}/merged_static.json")
    if not rows:
        return None
    keep = ("point", "fresh", "probe", "q", "stored", "in_context", "in_summary", "inject_hit", "auto_hit", "pull8_hit", "decision", "auto_chars", "first_rank")
    return [{k: r.get(k) for k in keep} for r in rows]


def probes_bge(name):
    rows = load(f"{name}/merged_bert.json")
    if not rows:
        return None
    keep = ("point", "fresh", "probe", "stored", "in_context", "inject_hit", "auto_hit", "pull8_hit")
    return [{k: r.get(k) for k in keep} for r in rows]


data = {
    "generated": datetime.date.today().isoformat(),
    "retrieval": {
        "lme_static": slim_retrieval(load("retrieval/longmemeval_s_static_round.json")),
        "lme_static_sub": slim_retrieval(load("retrieval/longmemeval_s_static_round_every5.json")),
        "lme_bge_sub": slim_retrieval(load("retrieval/longmemeval_s_bge-small_round_every5.json")),
        "locomo_static": slim_retrieval(load("retrieval/locomo_static_round.json")),
        "locomo_bge": slim_retrieval(load("retrieval/locomo_bge-small_round.json")),
        "lme_static_none": slim_retrieval(load("retrieval/longmemeval_s_static_none.json")),
        "lme_static_request": slim_retrieval(load("retrieval/longmemeval_s_static_request.json")),
        "locomo_static_none": slim_retrieval(load("retrieval/locomo_static_none.json")),
    },
    "probes": {"recurring": probes("probes"), "lost": probes("probes_lost"), "vague": probes("probes_vague")},
    "probes_bge": {"recurring": probes_bge("probes"), "lost": probes_bge("probes_lost"), "vague": probes_bge("probes_vague")},
    "gate": load("gate_static.json"),
    "gate_bge": load("gate_bge.json"),
    "scale": load("scale/scale_static.json"),
    "live": load("live/live_runs.json"),
    "live_qa": load("live/live_transcript_qa.json"),
    "session": load("session_stats.json"),
}

style = (shared / "template.html").read_text()
style = style[style.index("<style>") + len("<style>"):style.index("</style>")]
page = (report / "template.html").read_text().replace("/*__STYLE__*/", style)
page = page.replace("/*__DATA__*/null", json.dumps(data, separators=(",", ":")))
page = page.replace("/*__CHARTS__*/", (shared / "charts.js").read_text() + "\n" + (report / "diagrams.js").read_text())
page = page.replace("/*__SCRIPT__*/", (report / "report.js").read_text())
for marker, fragment in [("<!--FINDINGS-->", "findings.html"), ("<!--RESULTS-->", "results.html"), ("<!--VERDICT-->", "verdict.html")]:
    f = report / fragment
    page = page.replace(marker, f.read_text() if f.exists() else "")
(report / "engram.html").write_text(page)
print(f"wrote {report / 'engram.html'} ({len(page) / 1024:.0f} KiB)")
