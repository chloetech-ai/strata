#!/usr/bin/env python3
"""Draws the two latency charts of the Engram showcase from Strata's results:
docs/showcase/search-latency.png (Quora workload, Strata against Tantivy and
SQLite) and docs/showcase/graph-latency.png (arXiv traversals, Strata against
SQLite). Titles and labels are computed from the numbers.

  python3 docs/scripts/showcase_charts.py        (needs matplotlib)
"""
import json
import math
import pathlib

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.transforms import blended_transform_factory  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parents[2]
RESULTS = ROOT / "strata" / "results"
OUT = ROOT / "docs" / "showcase"

BLUE, GREY, INK, INK2, GRID = "#2b78d6", "#c3c2b8", "#1a1a1a", "#555555", "#e4e4e4"
plt.rcParams.update({"font.family": ["Arial", "DejaVu Sans"], "font.size": 12})


def load(path):
    return json.loads((RESULTS / path).read_text())


def ms(v):
    if v >= 100:
        return f"{v:,.0f} ms"
    if v >= 1:
        return f"{v:.1f} ms"
    return f"{v:.2f} ms"


def log_ticks(lo, hi):
    a, b = math.floor(math.log10(lo)), math.ceil(math.log10(hi))
    return [10.0 ** e for e in range(a, b + 1)]


def tick_label(v):
    return f"{v:,.0f} ms" if v >= 1 else f"{v:g} ms"


def draw(groups, title, subtitle, out, lo, hi):
    """groups: [(heading, sub, [(system, value or None, note, highlight)])].
    Without a sub, a heading takes its own row above its bars; with one, the
    heading and sub sit to the left of the bars."""
    rows = sum(len(b) + (0 if sub else 1) + 0.5 for _, sub, b in groups)
    height = 0.36 * rows + 1.5
    fig = plt.figure(figsize=(9.2, height), dpi=150)
    ax = fig.add_axes([0.36, 0.02, 0.44, 1 - 1.5 / height])
    ax.set_xscale("log")
    ax.set_xlim(lo, hi)
    left = blended_transform_factory(fig.transFigure, ax.transData)
    y = 0
    for heading, sub, bars in groups:
        ax.text(0.03, y if sub else y - 0.1, heading, transform=left, ha="left", va="center", fontsize=12.5, fontweight="bold", color=INK)
        if sub:
            ax.text(0.03, y + 0.8, sub, transform=left, ha="left", va="center", fontsize=11, color=INK2)
        else:
            y += 1
        for name, v, note, hl in bars:
            ax.text(-0.02, y, name, transform=ax.get_yaxis_transform(), ha="right", va="center", fontsize=11.5, color=INK2)
            if v is None:
                ax.text(lo * 1.15, y, note, ha="left", va="center", fontsize=11.5, color=INK2)
            else:
                ax.barh(y, v - lo, left=lo, height=0.66, color=BLUE if hl else GREY, zorder=2)
                ax.text(v * 1.12, y, ms(v) + (f" {note}" if note else ""), ha="left", va="center", fontsize=11.5,
                        fontweight="bold" if hl else "normal", color=INK, clip_on=False)
            y += 1
        y += 0.5
    ax.set_ylim(y - 0.5, -0.8)
    ax.set_yticks([])
    ticks = [t for t in log_ticks(lo, hi) if lo <= t <= hi]
    ax.set_xticks(ticks)
    ax.set_xticklabels([tick_label(t) for t in ticks], color=INK2)
    ax.xaxis.tick_top()
    ax.tick_params(axis="x", length=0, pad=6)
    ax.minorticks_off()
    ax.grid(axis="x", color=GRID, zorder=0)
    for sp in ax.spines.values():
        sp.set_visible(False)
    fig.text(0.03, 1 - 0.35 / height, title, fontsize=16, fontweight="bold", color=INK, va="center")
    fig.text(0.03, 1 - 0.68 / height, subtitle, fontsize=12, color=INK2, va="center")
    fig.savefig(out, dpi=150, facecolor="white")
    plt.close(fig)
    print("wrote", out)


def search_latency():
    sysd = {s: load(f"{d}/{s}_quora.json") for s, d in (("strata", "strata"), ("tantivy", "baselines"), ("sqlite", "baselines"))}
    n = sysd["strata"]["n_docs"]

    def warm(s, kind):
        for k in sysd[s]["kinds"]:
            if k["kind"] == kind and k.get("warm"):
                return k["warm"]["p50_ms"]
        return None

    kinds = [("bm25", "Keyword search (BM25)"), ("vector", "Vector search (1,024 dims)"), ("hybrid", "Hybrid: keywords + vectors"),
             ("filter", "Structured filter"), ("facet", "Facet counts")]
    names = [("strata", "Strata"), ("tantivy", "Tantivy"), ("sqlite", "SQLite + FTS5")]
    groups, vals = [], []
    for kind, heading in kinds:
        bars = []
        for s, name in names:
            v = warm(s, kind)
            bars.append((name, v, "no vector support" if v is None else "", s == "strata"))
            if v is not None:
                vals.append((s, v))
        groups.append((heading, None, bars))
    strata_max = max(v for s, v in vals if s == "strata")
    sqlite_max = max(v for s, v in vals if s == "sqlite")
    bound = next(b for b in (0.5, 1, 2, 5, 10, 20, 50, 100) if strata_max < b)
    worst = ms(sqlite_max) if sqlite_max < 700 else f"{sqlite_max / 1000:.1f} s"
    title = f"Strata answers every search in under {bound:g} ms; SQLite takes up to {worst}"
    lo = 10 ** math.floor(math.log10(min(v for _, v in vals)))
    hi = max(v for _, v in vals) * 1.6
    draw(groups, title, f"Median warm latency per query on Quora ({round(n, -3):,} documents), log scale", OUT / "search-latency.png", lo, hi)


def graph_latency():
    d = load("graph/latency_arxiv.json")
    r = d["results"]
    queries = [("reach_out_1", "1 hop out"), ("reach_out_2", "2 hops out"), ("reach_out_3", "3 hops out"), ("reach_both_2", "2 hops, both directions")]
    groups, ratios, vals = [], [], []
    for key, heading in queries:
        q = r[key]
        st, sq = q["strata_ms"]["p95"], q["sqlite_ms"]["p95"]
        ratio = sq / st
        ratios.append(ratio)
        vals += [st, sq]
        rows = q.get("mean_rows") or q.get("rows") or q.get("mean_results")
        note = f"({ratio:.1f}× slower)" if ratio >= 1.5 else ""
        groups.append((heading, f"{rows:,.0f} rows on average" if rows else None,
                       [("Strata", st, "", True), ("SQLite", sq, note, False)]))
    best = max(ratios)
    title = f"Past one hop, Strata walks the graph up to {best:.0f}× faster than SQLite"
    lo = 10 ** math.floor(math.log10(min(vals)))
    hi = max(vals) * 1.6
    sub = f"95th-percentile latency, arXiv: {round(d['rows'], -3):,} papers, {d['edges'] / 1e6:.2f} million citations; SQLite uses a recursive query; log scale"
    draw(groups, title, sub, OUT / "graph-latency.png", lo, hi)


if __name__ == "__main__":
    search_latency()
    graph_latency()
