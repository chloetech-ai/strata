#!/usr/bin/env python3
"""Charts for the October 2026 changes (query engine, memory systems compared).

Reads the committed results and writes PNGs into docs/assets/img:

  results/v2/reader/                    answers on LongMemEval-S from each context
  results/v2/deep/                      what the query engine found
  results/memory_bench/<scenario>/      Claude Code with each memory system

and the memory systems' results pooled over scenarios into
results/memory_bench/pooled.{md,json}.

  python3 scripts/v2_charts.py
"""

import gzip
import json
import os
import random
import sys
from collections import defaultdict

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RES = os.path.join(ROOT, "results")
OUT = os.path.join(os.path.dirname(ROOT), "docs", "assets", "img")

GREY, BLUE, DARK, GREEN, AMBER, TEAL, PURPLE = "#9ca3af", "#2d6cd6", "#1e3a8a", "#16a34a", "#d97706", "#0d9488", "#7c3aed"

plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 12, "axes.spines.top": False, "axes.spines.right": False})


def save(fig, name):
    os.makedirs(OUT, exist_ok=True)
    fig.savefig(os.path.join(OUT, name), dpi=160, bbox_inches="tight", facecolor="white")
    plt.close(fig)
    print("wrote", os.path.join(OUT, name))


def answers_chart():
    s = json.load(open(os.path.join(RES, "v2", "reader", "summary.json")))["configs"]
    rows = [
        ("no memory", "none", GREY),
        ("Engram main: 4,000-character recall", "win400", BLUE),
        ("query engine's picks only", "deep_haiku", TEAL),
        ("Engram main + query engine", "win400_deep_haiku", DARK),
        ("the evidence sessions, whole (oracle)", "oracle", GREEN),
    ]
    fig, ax = plt.subplots(figsize=(10, 3.9))
    ys = list(range(len(rows)))[::-1]
    for y, (label, key, color) in zip(ys, rows):
        c = s[key]
        ax.barh(y, c["accuracy"], color=color, height=0.6)
        ax.text(c["accuracy"] + 0.01, y, f"{c['accuracy']:.1%}   ({c['context_chars']:,.0f} chars)", va="center", fontsize=11)
    ax.set_yticks(ys, [r[0] for r in rows])
    ax.set_xlim(0, 1.18)
    ax.set_xticks([0, 0.25, 0.5, 0.75, 1.0], ["0%", "25%", "50%", "75%", "100%"])
    ax.grid(axis="x", color="#e5e7eb")
    ax.set_axisbelow(True)
    ax.set_title("LongMemEval-S: questions answered correctly, by context", loc="left", fontweight="bold")
    fig.text(0.01, -0.04, "500 questions; reader and judge Haiku (LongMemEval's grading prompts). Average context size in brackets. Cloud VM, 2026-10-15.", fontsize=9, color="#555")
    save(fig, "chart-engram-v2-answers.png")


def evidence_chart():
    rows = [json.loads(l) for l in gzip.open(os.path.join(RES, "v2", "deep", "longmemeval_s_deep_haiku.rows.jsonl.gz"), "rt")]
    rows = [r for r in rows if not r["abstention"] and r["n_evidence"] > 0]
    by = defaultdict(list)
    for r in rows:
        by[r["qtype"]].append(r)
    by["all"] = rows
    order = ["all", "multi-session", "temporal-reasoning", "knowledge-update", "single-session-user", "single-session-assistant", "single-session-preference"]
    names = {"all": "all", "multi-session": "multi-\nsession", "temporal-reasoning": "temporal", "knowledge-update": "knowledge\nupdate",
             "single-session-user": "user\nfact", "single-session-assistant": "assistant\nfact", "single-session-preference": "preference"}
    series = [("plain search, top 10 records", "base_r_all@10", BLUE), ("query engine, 30 candidates", "pool_all", PURPLE), ("query engine, its picks (2.6 records)", "pick_all", DARK)]
    fig, ax = plt.subplots(figsize=(11, 4.3))
    w = 0.26
    for i, (label, key, color) in enumerate(series):
        xs = [j + (i - 1) * w for j in range(len(order))]
        vals = [sum(r[key] for r in by[t]) / len(by[t]) for t in order]
        ax.bar(xs, vals, w, color=color, label=label)
        for x, v in zip(xs, vals):
            ax.text(x, v + 0.01, f"{v:.2f}", ha="center", fontsize=8)
    ax.set_xticks(range(len(order)), [f"{names[t]}\n(n={len(by[t])})" for t in order], fontsize=10)
    ax.set_ylim(0, 1.12)
    ax.set_ylabel("all evidence found")
    ax.grid(axis="y", color="#e5e7eb")
    ax.set_axisbelow(True)
    ax.legend(loc="upper left", bbox_to_anchor=(0, 1.16), ncol=3, frameon=False, fontsize=10)
    ax.set_title("LongMemEval-S: questions whose every evidence turn was found", loc="left", fontweight="bold", pad=34)
    save(fig, "chart-engram-v2-evidence.png")


LANE_NAMES = {"none": "no memory", "ccmem": "Claude Code\nauto memory", "transcripts": "past\ntranscripts", "claude-mem": "claude-mem", "mem0": "mem0",
              "engram@main": "Engram\nmain", "engram+v5": "Engram new\n(default)", "engram+rank": "Engram new,\nranker only", "full": "whole history\nin context",
              "engram+nodeep": "first build,\nno query engine", "engram": "first build\n(async)", "engram+deep": "first build\n(sync)"}
LANE_COLORS = {"none": GREY, "ccmem": AMBER, "transcripts": AMBER, "claude-mem": PURPLE, "mem0": PURPLE, "engram@main": BLUE, "engram+v5": DARK, "engram+rank": TEAL,
               "engram+nodeep": TEAL, "engram": DARK, "engram+deep": DARK, "full": GREEN}
# Every task ran four times in these lanes; the first build's lanes (the
# query engine as first proposed) ran three times, before two bakehouse tasks
# existed, and are reported in pooled.md only.
CHART_LANES = ["none", "ccmem", "transcripts", "claude-mem", "mem0", "engram@main", "engram+v5", "engram+rank", "full"]


def lanes_chart(scenarios):
    data = {}
    for sc in scenarios:
        p = os.path.join(RES, "memory_bench", sc, "summary.json")
        if os.path.exists(p):
            data[sc] = [json.loads(l) for l in open(os.path.join(RES, "memory_bench", sc, "runs.jsonl"))]
    if not data:
        return
    lanes = [l for l in CHART_LANES if any(r["lane"] == l for rs in data.values() for r in rs)]
    fig, axes = plt.subplots(1, len(data), figsize=(3.3 * len(data) + 2.2, 0.42 * len(lanes) + 1.4), sharey=True, squeeze=False)
    ys = list(range(len(lanes)))[::-1]
    for ax, (sc, runs) in zip(axes[0], data.items()):
        for y, lane in zip(ys, lanes):
            rs = [r for r in runs if r["lane"] == lane]
            if not rs:
                continue
            solved = sum(1 for r in rs if r.get("score", 0) >= 1.0)
            ax.barh(y, solved / len(rs), 0.7, color=LANE_COLORS[lane])
            ax.text(solved / len(rs) + 0.02, y, f"{solved}/{len(rs)}", va="center", fontsize=9)
        ax.set_xlim(0, 1.3)
        ax.set_xticks([0, 0.5, 1.0], ["0%", "50%", "100%"])
        ax.grid(axis="x", color="#e5e7eb")
        ax.set_axisbelow(True)
        ax.set_title(sc, fontsize=12, fontweight="bold")
    axes[0][0].set_yticks(ys, [LANE_NAMES[l].replace("\n", " ") for l in lanes], fontsize=10)
    fig.suptitle("Claude Code with each memory system: follow-up runs solved", x=0.01, ha="left", fontweight="bold")
    fig.text(0.01, -0.03, "Each task run 4 times per lane in a new headless session, after the lane recorded the project's history. Agent and memory systems: Haiku. Cloud VM, 2026-10-15.", fontsize=9, color="#555")
    fig.tight_layout()
    save(fig, "chart-engram-v2-lanes.png")


def pooled(scenarios):
    """Every lane's runs over all scenarios, against none and Engram main
    (tasks are strata, so the permutation test shuffles within a task)."""
    sys.path.insert(0, HERE)
    import memory_bench as mb  # noqa: E402

    rows, done = [], []
    for sc in scenarios:
        p = os.path.join(RES, "memory_bench", sc, "summary.json")
        if os.path.exists(p):
            done.append(sc)
            for line in open(os.path.join(RES, "memory_bench", sc, "runs.jsonl")):
                r = json.loads(line)
                r["task"] = f"{sc}/{r['task']}"
                rows.append(r)
    if not rows:
        return
    rng = random.Random(1)
    lanes = [l for l in LANE_NAMES if any(r["lane"] == l for r in rows)]
    out = {"scenarios": done, "lanes": {}}
    md = [f"# Memory bench: {', '.join(done)} pooled", "",
          "Solved = score 1.0. p: two-sided; Fisher = Fisher's exact test on solved runs; perm = permutation test of the "
          "solved-rate difference with lane labels shuffled within each task (20,000 shuffles).", "",
          "| lane | runs | solved | needs earlier sessions: solved | $/run (agent + memory) | p vs none (Fisher / perm) | p vs engram@main (Fisher / perm) |",
          "|---|---|---|---|---|---|---|"]
    for lane in lanes:
        rs = [r for r in rows if r["lane"] == lane]
        need = [r for r in rs if r.get("kind") not in ("control", "repo")]
        solved = sum(r["solved"] for r in rs)
        cost = sum(r.get("cost_usd") or 0 for r in rs) / len(rs)
        mem = sum(r.get("memory_cost_usd") or 0 for r in rs) / len(rs)
        vs = {ref: mb.compare(rows, lane, ref, 20000, rng) for ref in ("none", "engram@main") if ref != lane}
        out["lanes"][lane] = {"runs": len(rs), "solved": solved, "need_runs": len(need), "need_solved": sum(r["solved"] for r in need),
                              "cost_usd": cost, "memory_cost_usd": mem, "vs": vs}
        cell = lambda v: "– / –" if not v else f"{v['fisher_solved_p']:.2g} / {v['perm_solved_p']:.2g}"  # noqa: E731
        md.append(f"| {lane} | {len(rs)} | {solved}/{len(rs)} ({solved / len(rs):.0%}) | {out['lanes'][lane]['need_solved']}/{len(need)} | "
                  f"{cost:.3f} + {mem:.3f} | {cell(vs.get('none'))} | {cell(vs.get('engram@main'))} |")
    base = os.path.join(RES, "memory_bench", "pooled")
    json.dump(out, open(base + ".json", "w"), indent=1)
    open(base + ".md", "w").write("\n".join(md) + "\n")
    print("wrote", base + ".md")


def efficiency(scenarios):
    """What each lane's memory costs a run: the context it adds, the agent's
    input, the memory system's own model calls, money and time. From the runs
    that logged where memory reached the agent (the fourth repeat, and every
    run of the lanes added with it)."""
    rows = []
    for sc in scenarios:
        p = os.path.join(RES, "memory_bench", sc, "runs.jsonl")
        if os.path.exists(p):
            rows += [json.loads(l) for l in open(p) if '"memory_chars"' in l]
    if not rows:
        return

    def q(xs, f):
        xs = sorted(xs)
        return xs[min(len(xs) - 1, int(f * len(xs)))]

    out, md = {}, ["# Memory bench: what memory costs a run", "",
                   "Runs that logged where memory reached the agent: the fourth repeat of every task, and all four in the lanes added with it. "
                   "Memory context = characters that hooks injected, memory tools returned, or the agent read from the memory's files, per run (mean). "
                   "Agent input = tokens the agent read per run, cached or not (median). Memory model tokens = the memory system's own model calls per run "
                   "(input + output, mean). For `full` the history is the forked session's own context, so it shows in agent input, not memory context. "
                   "Wall time includes load from the lanes running beside it.", "",
                   "| lane | runs | solved | memory context, chars | agent input, tokens (p50) | memory model tokens | $ per run, agent + memory | wall s p50 / p95 |",
                   "|---|---|---|---|---|---|---|---|"]
    for lane in [l for l in LANE_NAMES if any(r["lane"] == l for r in rows)]:
        rs = [r for r in rows if r["lane"] == lane]
        mem_chars = sum(sum((r.get("memory_chars") or {}).values()) for r in rs) / len(rs)
        mt = [(r.get("memory_tokens") or {}) for r in rs]
        mem_tok = sum(t.get("input", 0) + t.get("cache_read", 0) + t.get("cache_write", 0) + t.get("output", 0) for t in mt) / len(rs)
        agent_in = q([(r.get("input_tokens") or 0) for r in rs], 0.5)
        cost = sum(r.get("cost_usd") or 0 for r in rs) / len(rs)
        mem_cost = sum(r.get("memory_cost_usd") or 0 for r in rs) / len(rs)
        wall = [r.get("wall_s") or 0 for r in rs]
        solved = sum(r["solved"] for r in rs)
        out[lane] = {"runs": len(rs), "solved": solved, "memory_chars": mem_chars, "agent_input_tokens_p50": agent_in, "memory_model_tokens": mem_tok,
                     "cost_usd": cost, "memory_cost_usd": mem_cost, "wall_s_p50": q(wall, 0.5), "wall_s_p95": q(wall, 0.95)}
        md.append(f"| {lane} | {len(rs)} | {solved}/{len(rs)} ({solved / len(rs):.0%}) | {mem_chars:,.0f} | {agent_in:,} | {mem_tok:,.0f} | "
                  f"{cost:.4f} + {mem_cost:.4f} | {q(wall, 0.5):.0f} / {q(wall, 0.95):.0f} |")
    base = os.path.join(RES, "memory_bench", "efficiency")
    json.dump(out, open(base + ".json", "w"), indent=1)
    open(base + ".md", "w").write("\n".join(md) + "\n")
    print("wrote", base + ".md")


if __name__ == "__main__":
    answers_chart()
    evidence_chart()
    lanes_chart(["bakehouse", "stockroom", "slotwise", "kestrel"])
    pooled(["bakehouse", "stockroom", "slotwise", "kestrel"])
    efficiency(["bakehouse", "stockroom", "slotwise", "kestrel"])
