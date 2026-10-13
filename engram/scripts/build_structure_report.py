"""Assemble report/structure.html (what a memory record should be) from
report/structure/, the shared style and chart helpers of posting_bench's
reports, and results/.
Usage: python3 scripts/build_structure_report.py"""
import datetime
import gzip
import json
import pathlib
from collections import defaultdict

root = pathlib.Path(__file__).resolve().parent.parent
results = root / "results"
src = root / "report" / "structure"
shared = root.parent / "posting_bench" / "report"


def load(path):
    p = results / path
    return json.loads(p.read_text()) if p.exists() else None


def structure(name):
    d = load(f"structure/{name}.json")
    if not d:
        return None
    a = d["all"]
    by_type = {t: {"n": v["n"], "pack": {k: m for k, m in v["pack"].items() if k.endswith("|4000")}, "rank": v["rank"]} for t, v in d.get("by_type", {}).items()}
    return {
        "units": d["units"], "questions": d["questions"], "budgets": d["budgets"], "layouts": d["layouts"], "presentations": d["presentations"],
        "rows_per_unit": d["rows_per_unit"], "n": a["n"], "n_verbatim": a["n_verbatim"], "n_multi": a["n_multi"],
        "pack": a["pack"], "rank": a["rank"], "by_type": by_type,
    }


def transcript():
    d = load("structure/transcript.json")
    if not d:
        return None
    # Pool the three probe sets per (point, fresh): weighted by eligible probes.
    pooled = defaultdict(lambda: defaultdict(lambda: [0.0, 0]))
    sets = defaultdict(dict)
    for r in d["results"]:
        key = f"{r['point']}|{'fresh' if r['fresh'] else 'compacted'}"
        n = r["eligible"]
        for k, v in r["pack"].items():
            acc = pooled[key][k]
            acc[0] += v * n
            acc[1] += n
        rank = pooled[key][f"{r['layout']}|rank10"]
        rank[0] += r["rank10"] * n
        rank[1] += n
        sets[key][f"{r['set']}|{r['layout']}"] = {"eligible": n, "rank1": r["rank1"], "rank10": r["rank10"], "pack": {k: v for k, v in r["pack"].items() if k.endswith(("|2000", "|4000"))}}
    out = {k: {kk: vv[0] / max(1, vv[1]) for kk, vv in v.items()} for k, v in pooled.items()}
    n = {}
    for r in d["results"]:
        key = f"{r['point']}|{'fresh' if r['fresh'] else 'compacted'}"
        n.setdefault(key, {})[r["set"]] = r["eligible"]
    return {"pooled": out, "eligible": n, "sets": sets, "budgets": d["budgets"]}


def reader(out_dir, judge="small"):
    base = results / out_dir
    if not (base / "answers.jsonl").exists():
        return None
    answers = {}
    for line in (base / "answers.jsonl").read_text().splitlines():
        r = json.loads(line)
        answers[(r["config"], r["qid"])] = r
    verdicts = {}
    vpath = base / f"verdicts_{judge}.jsonl"
    if vpath.exists():
        for line in vpath.read_text().splitlines():
            v = json.loads(line)
            verdicts[(v["config"], v["qid"])] = v["correct"]
    rows = defaultdict(list)
    for k, r in answers.items():
        if k in verdicts:
            rows[k[0]].append({"qid": k[1], "type": "abstention" if r["abstention"] else r["qtype"], "ok": verdicts[k], "chars": r["context_chars"], "tokens": r["reader_tokens"], "cost": r["cost_reader"]})
    return rows


def summarize(rows, qids=None):
    out = {}
    for cfg, rs in rows.items():
        if qids is not None:
            rs = [r for r in rs if r["qid"] in qids]
        if not rs:
            continue
        by = defaultdict(list)
        for r in rs:
            by[r["type"]].append(r["ok"])
        out[cfg] = {
            "n": len(rs), "acc": sum(r["ok"] for r in rs) / len(rs),
            "by_type": {t: {"n": len(v), "acc": sum(v) / len(v)} for t, v in by.items()},
            "chars": sum(r["chars"] for r in rs) / len(rs), "tokens": sum(r["tokens"] for r in rs) / len(rs),
            "cost": sum(r["cost"] for r in rs) / len(rs),
        }
    return out


main = reader("reader", judge="large") or {}
full = reader("reader_full", judge="large") or {}
facts = reader("reader_facts", judge="large") or {}
full_q = {r["qid"] for r in full.get("full", [])}
facts_q = {r["qid"] for rs in facts.values() for r in rs}
# The small judge against the large one, on every answer graded by both.
agree = None
pairs = []
for d in ("reader", "reader_facts", "reader_full"):
    small, large = reader(d, judge="small") or {}, reader(d, judge="large") or {}
    for cfg, rs in large.items():
        sm = {r["qid"]: r["ok"] for r in small.get(cfg, [])}
        pairs += [(sm[r["qid"]], r["ok"]) for r in rs if r["qid"] in sm]
if pairs:
    agree = {"n": len(pairs), "agree": sum(a == b for a, b in pairs) / len(pairs), "small_acc": sum(a for a, _ in pairs) / len(pairs), "large_acc": sum(b for _, b in pairs) / len(pairs)}

# Accuracy when the packed context does or does not hold the evidence.
conditional = {}
rows_path = results / "structure" / "longmemeval_s.rows.jsonl.gz"
if rows_path.exists():
    qrows = {json.loads(l)["qid"]: json.loads(l) for l in gzip.open(rows_path, "rt")}
    for cfg, key in [("today", "turn+req|snip600|4000"), ("turn", "turn|snip600|4000"), ("win400", "win400+req/vec-text|snip600|4000")]:
        ok = {r["qid"]: r["ok"] for r in main.get(cfg, [])}
        out = {}
        for metric in ("ev", "ans"):
            g = defaultdict(list)
            for q, r in qrows.items():
                m = r["pack"].get(key, {})
                if q in ok and m.get(metric) is not None:
                    g[int(m[metric])].append(ok[q])
            out[metric] = {str(k): {"n": len(v), "acc": sum(v) / len(v)} for k, v in g.items()}
        conditional[cfg] = out

def ab_set(rows, tasks):
    """Per task and condition, and overall, for one A/B run set."""
    med = lambda xs: sorted(xs)[len(xs) // 2] if xs else None  # noqa: E731

    def agg(rs):
        return {"n": len(rs), "solved": sum(r["solved"] for r in rs), "score": sum(r["score"] for r in rs) / max(1, len(rs)),
                "cost": med([r["cost_usd"] or 0 for r in rs]), "steps": med([r["turns"] or 0 for r in rs]), "wall": med([r["wall_s"] for r in rs]),
                "memory_tools": sum(r["memory_tool_calls"] for r in rs)}
    out = {}
    for t in tasks:
        rs = [r for r in rows if r["task"] == t["id"]]
        if not rs:
            continue
        o = {"needs": t["needs"], "kind": t.get("kind")}
        for c in ("engram", "none"):
            o[c] = agg([r for r in rs if r["condition"] == c])
        o["engram"]["gate"] = [d for r in rs if r["condition"] == "engram" for d in (r.get("engram") or {}).get("decisions", [])]
        # A grader that could not run the implementation reports {"error": ...}.
        fails = lambda r: [k for k, v in (r.get("checks") or {}).items() if v is False] or (["crashed on the grader's input"] if "error" in (r.get("checks") or {}) else [])  # noqa: E731
        o["failed_checks"] = {c: sorted({k for r in rs if r["condition"] == c and not r["solved"] for k in fails(r)}) for c in ("engram", "none")}
        out[t["id"]] = o
    order = [t["id"] for t in tasks if t["id"] in out]
    return {"order": order, "tasks": out, "overall": {c: agg([r for r in rows if r["condition"] == c]) for c in ("engram", "none")}}


def ab_summary():
    """The with/without Engram tests (scripts/ab_eval.py): the first round and
    the rerun on Tally, and the two held-out scenarios."""
    import sys
    sys.path.insert(0, str(root / "scripts"))
    import ab_eval
    rd = lambda p: [json.loads(l) for l in (results / "ab" / p).read_text().splitlines()] if (results / "ab" / p).exists() else []  # noqa: E731
    first = rd("runs.jsonl")
    if not first:
        return None
    sets = {"tally_r1": ab_set(first, ab_eval.TASKS),
            "tally_dev": ab_set(rd("runs_v2.jsonl") + [r for r in first if r["condition"] == "none"], ab_eval.TASKS)}
    hist = {"tally": load("ab/history.json") or {}}
    for sc in ("stockroom", "slotwise"):
        rows = rd(f"{sc}/runs.jsonl")
        if rows:
            sets[sc] = ab_set(rows, ab_eval.load_scenario(sc)["tasks"])
            hist[sc] = load(f"ab/{sc}/history.json") or {}
    # A resumed session reports its running total: a session costs its last turn's figure.
    cost = lambda h: sum(max([t.get("cost") or 0 for t in h.get("turns", []) if t["session"] == k] or [0]) for k in range(len(h.get("sessions", []))))  # noqa: E731
    history = {k: {"turns": len(h.get("turns", [])), "sessions": len(h.get("sessions", [])), "cost": cost(h)} for k, h in hist.items()}
    return {"sets": sets, "stats": load("ab/stats.json"), "history": history, "distill": load("ab/distill_cost.json")}


data = {
    "generated": datetime.date.today().isoformat(),
    "lme": structure("longmemeval_s"),
    "locomo": structure("locomo"),
    "facts": structure("longmemeval_s_facts"),
    "coding": transcript(),
    "reader": summarize(main),
    "reader_full_subset": {**summarize(main, full_q), **summarize(full)},
    "reader_facts_subset": {**summarize(main, facts_q), **summarize(facts)},
    "judge_agreement": agree,
    "conditional": conditional,
    # Before: 1,500-byte records. LoCoMo's "round" context is the previous
    # turn for every turn, the same as Engram's request context there.
    "retrieval": {k: load(f"retrieval/{k}.json") for k in ["longmemeval_s_static_request", "longmemeval_s_static_request_c400", "locomo_static_round", "locomo_static_request_c400"]},
    "gate_before": load("gate_static.json"),
    "gate_after": load("gate_static_c400.json"),
    "probes_before": {k: load(f"{k}/merged_static.json") for k in ["probes", "probes_lost", "probes_vague"]},
    "probes_after": {k: load(f"probes_after/{k}/merged_static.json") for k in ["probes", "probes_lost", "probes_vague"]},
    "latency": load("latency/latency_static.json"),
    # The same run before the daemon moved its state write after the reply.
    "latency_pre": load("latency/latency_static_pre_persist.json"),
    "facts_cost": load("structure/facts_cost.json"),
    "ab": ab_summary(),
}
for k in ("longmemeval_s_static_request", "longmemeval_s_static_request_c400", "locomo_static_round", "locomo_static_request_c400"):
    d = data["retrieval"][k]
    if d:
        data["retrieval"][k] = {"all": d["all"], "eligible": d["eligible"]}

style = (shared / "template.html").read_text()
style = style[style.index("<style>") + len("<style>"):style.index("</style>")]
page = (src / "template.html").read_text().replace("/*__STYLE__*/", style)
page = page.replace("/*__DATA__*/null", json.dumps(data, separators=(",", ":")))
page = page.replace("/*__CHARTS__*/", (shared / "charts.js").read_text() + "\n" + (src / "diagrams.js").read_text())
page = page.replace("/*__SCRIPT__*/", (src / "report.js").read_text())
for marker, fragment in [("<!--FINDINGS-->", "findings.html"), ("<!--RESULTS-->", "results.html"), ("<!--DESIGN-->", "design.html")]:
    f = src / fragment
    page = page.replace(marker, f.read_text() if f.exists() else "")
out = root / "report" / "structure.html"
out.write_text(page)
print(f"wrote {out} ({len(page) / 1024:.0f} KiB)")
