"""Assemble report/report.html from the template, the narrative fragments and
results/*.json. Usage: python3 scripts/build_report.py"""
import datetime
import json
import pathlib
import subprocess

root = pathlib.Path(__file__).resolve().parent.parent
results = root / "results"
report = root / "report"


def load(name):
    p = results / f"{name}.json"
    return json.loads(p.read_text()) if p.exists() else None


data = {
    "clustered": load("clustered"),
    "uniform": load("uniform"),
    "wikitext": load("wikitext"),
    "kernels": load("kernels"),
    "terms": {k: load(f"terms_{k}") for k in range(2, 7) if load(f"terms_{k}")},
    "generated": datetime.date.today().isoformat(),
}
try:
    data["rustc"] = subprocess.run(["rustc", "--version"], capture_output=True, text=True).stdout.split()[1]
except (OSError, IndexError):
    pass

html = (report / "template.html").read_text()
html = html.replace("/*__DATA__*/null", json.dumps(data, separators=(",", ":")))
html = html.replace("/*__SCRIPT__*/", (report / "report.js").read_text())
for marker, fragment in [("<!--RESULTS-->", "results.html"), ("<!--TAKEAWAYS-->", "takeaways.html")]:
    f = report / fragment
    html = html.replace(marker, f.read_text() if f.exists() else "")
(report / "report.html").write_text(html)
print(f"wrote {report / 'report.html'} ({len(html) / 1024:.0f} KiB)")


# ---- designs page -------------------------------------------------------
designs = results / "designs"


def load_design(name):
    p = designs / f"{name}.json"
    return json.loads(p.read_text()) if p.exists() else None


ddata = {
    "clustered": load_design("clustered"),
    "uniform": load_design("uniform"),
    "wikitext": load_design("wikitext"),
    "selective": load_design("selective"),
    "dense": load_design("dense"),
    "terms": {k: load_design(f"terms_{k}") for k in range(2, 7) if load_design(f"terms_{k}")},
    "generated": data["generated"],
}
tpl = report / "designs_template.html"
if tpl.exists():
    style = (report / "template.html").read_text()
    style = style[style.index("<style>") + len("<style>"):style.index("</style>")]
    page = tpl.read_text().replace("/*__STYLE__*/", style)
    page = page.replace("/*__DATA__*/null", json.dumps(ddata, separators=(",", ":")))
    page = page.replace("/*__CHARTS__*/", (report / "charts.js").read_text())
    page = page.replace("/*__SCRIPT__*/", (report / "designs.js").read_text())
    for marker, fragment in [
        ("<!--FINDINGS-->", "designs_findings.html"),
        ("<!--PYRAMID-->", "designs_pyramid.html"),
        ("<!--RESULTS-->", "designs_results.html"),
        ("<!--VERDICT-->", "designs_verdict.html"),
    ]:
        f = report / fragment
        page = page.replace(marker, f.read_text() if f.exists() else "")
    (report / "designs.html").write_text(page)
    print(f"wrote {report / 'designs.html'} ({len(page) / 1024:.0f} KiB)")
