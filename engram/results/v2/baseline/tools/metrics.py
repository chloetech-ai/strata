#!/usr/bin/env python3
"""Flattens Engram benchmark outputs into named numbers and compares two sets.

A source is either a run directory written by run_baseline.sh, or
`recorded:<engram checkout>` for the files committed under engram/results/,
or `recorded-vm:<dir>` for the cloud-VM files taken from git history
(see run_baseline.sh: recorded_vm/).

  metrics.py flatten SRC [--json out.json]
  metrics.py compare A B [--md out.md] [--key] [--label-a NAME] [--label-b NAME]

--key keeps only the rows quoted in the docs (headline numbers); without it
every metric both sources have is listed. Reader accuracies are computed from
the verdict files (verdicts_<judge>.jsonl joined with answers.jsonl), so a
run and the committed results are graded the same way.
"""
import argparse
import collections
import gzip
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gate_policy  # noqa: E402

LME_TYPES = ["knowledge-update", "multi-session", "single-session-assistant", "single-session-preference",
             "single-session-user", "temporal-reasoning"]
LOCOMO_TYPES = ["multi-hop", "open-domain", "single-hop", "temporal"]
METHODS = ["bm25", "dense", "hybrid_0.3", "hybrid_0.5", "hybrid_0.7", "hybrid_0.3_priors", "hybrid_0.5_priors"]
RET_KEYS = [("recall_any@1", "R@1"), ("recall_any@5", "R@5"), ("recall_any@10", "R@10"), ("recall_any@20", "R@20"),
            ("recall_all@10", "all@10"), ("ndcg@10", "nDCG@10"), ("session_all@5", "sess_all@5"),
            ("pack4k", "pack4k"), ("pack8k", "pack8k"), ("pack_brief", "pack_brief")]
READER_CONFIGS = ["none", "win400", "oracle"]


def load(path):
    if path is None or not os.path.exists(path):
        return None
    op = gzip.open if path.endswith(".gz") else open
    with op(path, "rt") as f:
        return json.load(f)


def jsonl(path):
    if path is None or not os.path.exists(path):
        return []
    op = gzip.open if path.endswith(".gz") else open
    with op(path, "rt") as f:
        return [json.loads(l) for l in f if l.strip()]


def size_label(n):
    for lim, lab in ((1_000_000, "1M"), (100_000, "100k"), (10_000, "10k")):
        if n >= lim * 0.9:
            return lab
    return str(n)


def paths(src):
    """Where each piece lives for a source."""
    if src.startswith("recorded:"):
        r = os.path.join(src.split(":", 1)[1], "engram", "results")
        return {
            "lme": f"{r}/retrieval/longmemeval_s_static_request_c400.json",
            "locomo": f"{r}/retrieval/locomo_static_request_c400.json",
            "gate_fit": f"{r}/gate_static_c400.json",
            "gate_code": None,
            "gate_code_from": (os.path.join(src.split(":", 1)[1], "engram"),
                               [f"{r}/retrieval/longmemeval_s_static_request_c400.rows.jsonl", f"{r}/retrieval/locomo_static_request_c400.rows.jsonl"]),
            "structure_lme": f"{r}/structure/longmemeval_s.json",
            "structure_locomo": f"{r}/structure/locomo.json",
            "scale": f"{r}/scale/scale_static.json",
            "latency": f"{r}/latency/latency_static.json",
            "latency_par": f"{r}/latency/knowledge_par/latency_static.json",
            "reader": f"{r}/reader",
            "tests": None,
            "recorded_tests": {"strata": {"passed": 221, "ignored": 14, "failed": 0}, "engram": {"passed": 22, "failed": 0}},
        }
    if src.startswith("recorded-vm:"):
        d = src.split(":", 1)[1]
        return {"latency": f"{d}/latency__latency_static.json", "latency_par": f"{d}/latency__knowledge_par__latency_static.json",
                "scale": f"{d}/scale__scale_static.json"}
    d = src
    return {
        "lme": f"{d}/retrieval/longmemeval_s_static_request_c400.json",
        "locomo": f"{d}/retrieval/locomo_static_request_c400.json",
        "gate_fit": f"{d}/gate/gate_static_c400.json",
        "gate_code": f"{d}/gate/code_gate_policy.json",
        "structure_lme": f"{d}/structure/longmemeval_s.json",
        "structure_locomo": f"{d}/structure/locomo.json",
        "scale": f"{d}/scale/scale_static.json",
        "latency": f"{d}/latency/latency_static.json",
        "latency_par": None,
        "reader": f"{d}/reader",
        "tests": f"{d}/tests/counts.json",
    }


def flatten(src):
    p = paths(src)
    m = collections.OrderedDict()
    # 1. retrieval
    for ds, types in (("lme", LME_TYPES), ("locomo", LOCOMO_TYPES)):
        d = load(p.get(ds))
        if not d:
            continue
        m[f"retrieval.{ds}.eligible"] = d.get("eligible")
        m[f"retrieval.{ds}.seconds"] = d.get("seconds")
        for meth in METHODS:
            for k, lab in RET_KEYS:
                v = d["all"].get(meth, {}).get(k)
                if v is not None:
                    m[f"retrieval.{ds}.{meth}.{lab}"] = v
        for t in types:
            bt = d.get("by_type", {}).get(t)
            if not bt:
                continue
            m[f"retrieval.{ds}.type.{t}.n"] = bt.get("n")
            for meth in METHODS:
                for k, lab in RET_KEYS:
                    v = bt.get(meth, {}).get(k)
                    if v is not None:
                        m[f"retrieval.{ds}.type.{t}.{meth}.{lab}"] = v
    # gate: refit by scripts/fit_gate.py, and the checkout's own default gate
    g = load(p.get("gate_fit"))
    if g:
        for path, pol in g.get("policy", {}).items():
            ds = "lme" if "longmemeval" in path else "locomo"
            for k in ("inject_rate", "precision", "coverage", "chars_per_prompt", "always_precision", "always_coverage"):
                m[f"gate.refit.{ds}.{k}"] = pol[k]
        for k, v in g.get("weights", {}).items():
            m[f"gate.refit.weight.{k}"] = v
        m["gate.refit.weight.hi"] = g.get("hi")
        m["gate.refit.weight.lo"] = g.get("lo")
    g = load(p.get("gate_code"))
    if not g and p.get("gate_code_from"):
        # The committed rows under the checkout's own default gate.
        eng, rows = p["gate_code_from"]
        if all(os.path.exists(x) for x in rows):
            try:
                w = gate_policy.gate_weights(eng)
                g = {x: gate_policy.policy(w, x) for x in rows}
            except ValueError as e:
                print(f"warning: {e}", file=sys.stderr)
    if g:
        for path, pol in g.items():
            if path == "gate":
                continue
            ds = "lme" if "longmemeval" in path else "locomo"
            for k in ("inject_rate", "full_rate", "precision", "coverage", "chars_per_prompt", "always_precision", "always_coverage"):
                m[f"gate.code.{ds}.{k}"] = pol[k]
    # 2. structure
    for ds in ("lme", "locomo"):
        d = load(p.get(f"structure_{ds}"))
        if not d:
            continue
        m[f"structure.{ds}.seconds"] = d.get("seconds")
        for k, v in d["all"]["rank"].items():
            lay, how = k.rsplit("|", 1)
            for mk in ("r1", "r5", "r10", "mrr"):
                m[f"structure.{ds}.rank.{lay}.{how}.{mk}"] = v[mk]
        for k, v in d["all"]["pack"].items():
            lay, pres, b = k.rsplit("|", 2)
            if b in ("2000", "4000"):
                m[f"structure.{ds}.pack.{lay}.{pres}.{b}.ev"] = v["ev"]
                m[f"structure.{ds}.pack.{lay}.{pres}.{b}.ans"] = v["ans"]
        for k, v in d.get("rows_per_unit", {}).items():
            m[f"structure.{ds}.rows_per_unit.{k}"] = v
    # 3. scale and latency
    d = load(p.get("scale"))
    for r in d or []:
        s = size_label(r["records"])
        for k, v in r.items():
            if isinstance(v, (int, float)):
                m[f"scale.{s}.{k}"] = v
    for key, name in (("latency", "latency"), ("latency_par", "latency_par")):
        d = load(p.get(key))
        for r in d or []:
            s = size_label(r["records"])
            pre = f"{name}.{s}"
            m[f"{pre}.records"] = r["records"]
            for k in ("daemon_start_ms", "embedder_open_ms", "table_read_ms"):
                if k in r:
                    m[f"{pre}.{k}"] = r[k]
            for h, st in r.get("hooks", {}).items():
                m[f"{pre}.hooks.{h}.p50"] = st["p50"]
                m[f"{pre}.hooks.{h}.p99"] = st["p99"]
            for ph, st in r.get("phases", {}).items():
                m[f"{pre}.phases.{ph}.p50"] = st["p50"]
            for c, st in r.get("stall", {}).items():
                for k in ("append", "rebuild", "query_after_write", "query_after_rebuild"):
                    m[f"{pre}.stall.{c}.{k}.p50"] = st[k]["p50"]
                    m[f"{pre}.stall.{c}.{k}.p99"] = st[k]["p99"]
    # 4. reader, from the verdict files
    rd = p.get("reader")
    if rd and os.path.isdir(rd):
        answers = {(a["config"], a["qid"]): a for a in jsonl(os.path.join(rd, "answers.jsonl"))}
        for vf in sorted(os.listdir(rd)):
            mm = re.match(r"verdicts_(\w+)\.jsonl$", vf)
            if not mm:
                continue
            judge = mm.group(1)
            acc = collections.defaultdict(list)
            cost_j = collections.defaultdict(float)
            for v in jsonl(os.path.join(rd, vf)):
                a = answers.get((v["config"], v["qid"]))
                if a is None:
                    continue
                t = "abstention" if a["abstention"] else a["qtype"]
                acc[(v["config"], "all")].append(v["correct"])
                acc[(v["config"], t)].append(v["correct"])
                cost_j[v["config"]] += v.get("cost_judge", 0.0)
            for (c, t), xs in sorted(acc.items()):
                if t == "all":
                    m[f"reader.{judge}.{c}.n"] = len(xs)
                    m[f"reader.{judge}.{c}.accuracy"] = sum(xs) / len(xs)
                    m[f"reader.{judge}.{c}.cost_judge_usd"] = cost_j[c]
                else:
                    m[f"reader.{judge}.{c}.type.{t}"] = sum(xs) / len(xs)
        per = collections.defaultdict(list)
        for (c, _), a in answers.items():
            per[c].append(a)
        for c, xs in sorted(per.items()):
            m[f"reader.answers.{c}.n"] = len(xs)
            m[f"reader.answers.{c}.context_chars"] = sum(a["context_chars"] for a in xs) / len(xs)
            m[f"reader.answers.{c}.reader_tokens"] = sum(a["reader_tokens"] for a in xs) / len(xs)
            m[f"reader.answers.{c}.cost_reader_usd"] = sum(a.get("cost_reader", 0.0) for a in xs)
    # 5. tests
    t = load(p.get("tests")) if p.get("tests") else p.get("recorded_tests")
    for crate, c in (t or {}).items():
        for k, v in c.items():
            m[f"tests.{crate}.{k}"] = v
    return m


KEY_PATTERNS = [
    r"^retrieval\.(lme|locomo)\.(eligible|seconds)$",
    r"^retrieval\.(lme|locomo)\.(bm25|dense|hybrid_0\.3_priors|hybrid_0\.5_priors)\.(R@5|R@10|nDCG@10|pack4k)$",
    r"^retrieval\.(lme|locomo)\.type\.[^.]+\.(bm25|hybrid_0\.3_priors)\.R@5$",
    r"^gate\.(refit|code)\.(lme|locomo)\.(inject_rate|precision|coverage|chars_per_prompt|always_precision|always_coverage)$",
    r"^structure\.(lme|locomo)\.rank\.(turn|turn\+req|win400\+req|win400\+req/vec-text|session)\.hybrid\.(r10|mrr)$",
    r"^structure\.(lme|locomo)\.pack\.(turn|turn\+req|win400\+req/vec-text|session)\.snip600\.4000\.(ev|ans)$",
    r"^structure\.(lme|locomo)\.seconds$",
    r"^scale\.(10k|100k|1M)\.(records|query_ms_p50|query_ms_p99|hook_ms_p50|hook_ms_p99|append_ms_p50|append_ms_p99|bytes_per_record|recall10|pack4k|embed_records_per_s|bulk_load_s|evidence_records)$",
    r"^latency(_par)?\.(10k|100k|1M)\.hooks\.(process_noop|process_prompt|process_tool|client_prompt)\.(p50|p99)$",
    r"^latency(_par)?\.(10k|100k|1M)\.phases\.search_total\.p50$",
    r"^latency(_par)?\.(10k|100k|1M)\.stall\.chunk1024_flat\.(append|query_after_write)\.(p50|p99)$",
    r"^latency(_par)?\.(10k|100k|1M)\.(daemon_start_ms|embedder_open_ms)$",
    r"^reader\.\w+\.(none|win400|oracle)\.(n|accuracy)$",
    r"^reader\.answers\.(none|win400|oracle)\.(context_chars|reader_tokens|cost_reader_usd)$",
    r"^tests\.",
]


def fmt(v):
    if v is None:
        return "–"
    if isinstance(v, bool):
        return str(v)
    if isinstance(v, int):
        return f"{v:,}"
    a = abs(v)
    if a >= 1000:
        return f"{v:,.0f}"
    if a >= 100:
        return f"{v:.1f}"
    if a >= 10:
        return f"{v:.2f}"
    return f"{v:.3f}"


def compare(a, b, key, la, lb):
    keys = list(a) + [k for k in b if k not in a]
    if key:
        keys = [k for k in keys if any(re.search(p, k) for p in KEY_PATTERNS)]
    out = [f"| metric | {la} | {lb} | difference |", "|---|---|---|---|"]
    n_same = n_diff = 0
    for k in keys:
        x, y = a.get(k), b.get(k)
        if x is None and y is None:
            continue
        if isinstance(x, (int, float)) and isinstance(y, (int, float)) and not isinstance(x, bool):
            d = y - x
            rel = f" ({d / x * 100:+.0f}%)" if x and abs(x) > 1e-9 and (k.split(".")[0] in ("scale", "latency", "latency_par") or k.endswith("seconds")) else ""
            ds = "=" if abs(d) < 5e-4 else f"{d:+.3f}{rel}" if abs(d) < 100 else f"{d:+,.0f}{rel}"
            n_same += ds == "="
            n_diff += ds != "="
        else:
            ds = ""
        out.append(f"| {k} | {fmt(x)} | {fmt(y)} | {ds} |")
    out.append("")
    out.append(f"{n_same} rows equal (to 3 decimals), {n_diff} differ.")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("flatten")
    f.add_argument("src")
    f.add_argument("--json")
    c = sub.add_parser("compare")
    c.add_argument("a")
    c.add_argument("b")
    c.add_argument("--md")
    c.add_argument("--key", action="store_true")
    c.add_argument("--label-a", default="A")
    c.add_argument("--label-b", default="B")
    args = ap.parse_args()
    if args.cmd == "flatten":
        m = flatten(args.src)
        s = json.dumps(m, indent=1)
        if args.json:
            open(args.json, "w").write(s)
        else:
            print(s)
    else:
        t = compare(flatten(args.a), flatten(args.b), args.key, args.label_a, args.label_b)
        if args.md:
            open(args.md, "w").write(t + "\n")
        print(t)


if __name__ == "__main__":
    main()
