#!/usr/bin/env python3
"""Charts for the October 2026 changes (query engine, memory systems compared).

Reads the committed results and writes PNGs into docs/assets/img:

  results/v2/reader/                    answers on LongMemEval-S from each context
  results/v2/deep/                      what the query engine found
  results/memory_bench/<scenario>/      Claude Code with each memory system

  python3 scripts/v2_charts.py
"""

import gzip
import json
import os
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
              "engram@main": "Engram\nmain", "engram+nodeep": "Engram new,\nno query engine", "engram": "Engram new\n(async)", "engram+deep": "Engram new\n(sync)", "full": "whole history\nin context"}
LANE_COLORS = {"none": GREY, "ccmem": AMBER, "transcripts": AMBER, "claude-mem": PURPLE, "mem0": PURPLE, "engram@main": BLUE, "engram+nodeep": TEAL, "engram": DARK, "engram+deep": DARK, "full": GREEN}


def lanes_chart(scenarios):
    runs = []
    for sc in scenarios:
        p = os.path.join(RES, "memory_bench", sc, "runs.jsonl")
        if os.path.exists(p):
            runs += [json.loads(l) for l in open(p)]
    if not runs:
        return
    lanes = [l for l in LANE_NAMES if any(r["lane"] == l for r in runs)]
    fig, ax = plt.subplots(figsize=(12, 4.6))
    for i, lane in enumerate(lanes):
        rs = [r for r in runs if r["lane"] == lane]
        solved = sum(1 for r in rs if r.get("score", 0) >= 1.0)
        v = solved / len(rs)
        ax.bar(i, v, 0.7, color=LANE_COLORS[lane])
        ax.text(i, v + 0.01, f"{solved}/{len(rs)}", ha="center", fontsize=10)
    ax.set_xticks(range(len(lanes)), [LANE_NAMES[l] for l in lanes], fontsize=10)
    ax.set_ylim(0, 1.1)
    ax.set_yticks([0, 0.25, 0.5, 0.75, 1.0], ["0%", "25%", "50%", "75%", "100%"])
    ax.set_ylabel("runs solved")
    ax.grid(axis="y", color="#e5e7eb")
    ax.set_axisbelow(True)
    ax.set_title(f"Claude Code with each memory system: follow-up tasks solved ({', '.join(scenarios)})", loc="left", fontweight="bold")
    fig.text(0.01, -0.06, "Each task run 3 times per lane in a fresh headless session after the lane recorded the project's history. Agent and memory systems: Haiku. Cloud VM, 2026-10-15.", fontsize=9, color="#555")
    save(fig, "chart-engram-v2-lanes.png")


if __name__ == "__main__":
    answers_chart()
    evidence_chart()
    lanes_chart(["bakehouse", "stockroom", "slotwise", "kestrel"])
