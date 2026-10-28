"""Tables of the scale probe (results/scale/steps.jsonl) as Markdown.

Usage: python3 scripts/scale_report.py [results/scale/steps.jsonl] > results/scale/README.tables.md
"""
import json
import sys

KINDS = ["bm25", "bm25_filtered", "vector", "vector_ef128", "vector_ef256", "vector_filtered", "hybrid_rrf", "hybrid_weighted"]


def gib(b):
    return f"{b / 2**30:.2f}"


def heap(s):
    """Peak anonymous memory; steps run before it was sampled show '-'."""
    return f"{s['peak_anon_mib']:,.0f}" if "peak_anon_mib" in s else "-"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "results/scale/steps.jsonl"
    steps = [json.loads(l) for l in open(path) if l.strip()]
    if steps:
        m = steps[0]["machine"]
        print(f"Machine: {m['cpu']}, {m['cores']} cores, {m['mem_gib']:.1f} GiB RAM, {m['host']}.\n")

    print("## Loads\n")
    print("| step | rows loaded | segments of | seconds | docs/s | HNSW s | text s | doc store s | peak RSS MiB | segments after | live GiB | bytes/doc |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for s in steps:
        if s["step"] != "load":
            continue
        b, a = s["build_seconds"], s["after"]
        print(f"| {s['label']} | {s['docs_loaded']:,} | {s['segment_docs']:,} | {s['seconds']:.0f} | {s['docs_per_second']:,.0f} | "
              f"{b.get('emb (vector)', 0):.0f} | {b.get('body (text)', 0):.0f} | {b.get('doc store', 0):.0f} | {s['peak_rss_mib']:,.0f} | "
              f"{a['segments']} | {gib(a['live_pages'] * 4096)} | {a['bytes_per_doc']:,.0f} |")

    print("\n## Vacuums\n")
    print("| step | seconds | GiB returned | file GiB before → after |")
    print("|---|---|---|---|")
    for s in steps:
        if s["step"] == "vacuum":
            print(f"| {s['label']} | {s['seconds']:.1f} | {gib(s['bytes_returned'])} | {gib(s['before']['file_bytes'])} → {gib(s['after']['file_bytes'])} |")

    print("\n## Merges\n")
    print("Peak RSS counts the table's file pages that the step read through its mapping; peak heap is anonymous memory only.\n")
    print("| step | policy | merges | docs rewritten | seconds | rewritten docs/s | HNSW s | text s | reading inputs s | peak RSS MiB | peak heap MiB | segments before → after | file GiB after |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for s in steps:
        if s["step"] != "merge":
            continue
        m, b, p = s["merge"], s["build_seconds"], s["policy"]
        rate = m["docs_merged"] / m["seconds"] if m["seconds"] > 0 else 0
        print(f"| {s['label']} | factor {p['merge_factor']}, ≤ {p['max_merged_docs']:,} | {m['merges']} | {m['docs_merged']:,} | {m['seconds']:.0f} | {rate:,.0f} | "
              f"{b.get('emb (vector)', 0):.0f} | {b.get('body (text)', 0):.0f} | {s['rebuild_docs_seconds']:.0f} | {s['peak_rss_mib']:,.0f} | "
              f"{heap(s)} | {s['before']['segments']} → {s['after']['segments']} | {gib(s['after']['file_bytes'])} |")

    queries = [s for s in steps if s["step"] == "query"]
    print("\n## Warm latency, one query at a time (p50 / p99 ms)\n")
    print("| step | docs | segments | " + " | ".join(KINDS) + " |")
    print("|---|---|---|" + "---|" * len(KINDS))
    for s in queries:
        cells = []
        for k in KINDS:
            d = s["kinds"].get(k)
            cells.append(f"{d['latency']['p50_ms']:.2f} / {d['latency']['p99_ms']:.2f}" if d else "-")
        print(f"| {s['label']} | {s['docs']:,} | {s['segments']} | " + " | ".join(cells) + " |")

    print("\n## Vector recall@10 against exact search\n")
    print("| step | segments | ef 64 | ef 128 | ef 256 | filtered (vs prefilter scan) |")
    print("|---|---|---|---|---|---|")
    for s in queries:
        k = s["kinds"]
        r = lambda n, f="recall_at_10": f"{k[n][f]:.3f}" if n in k and f in k[n] else "-"
        print(f"| {s['label']} | {s['segments']} | {r('vector')} | {r('vector_ef128')} | {r('vector_ef256')} | {r('vector_filtered', 'recall_at_10_vs_prefilter')} |")

    print("\n## Segment-parallel prototype (4 threads, segments split by size) vs serial, p50 / p99 ms\n")
    print("| step | segments | kind | serial | parallel | p50 speed-up | agreement with serial top-10 |")
    print("|---|---|---|---|---|---|---|")
    for s in queries:
        for k, d in s["parallel"]["kinds"].items():
            if k.startswith("_"):
                continue
            ser = s["kinds"][k]["latency"]
            par = d["latency"]
            print(f"| {s['label']} | {s['segments']} | {k} | {ser['p50_ms']:.2f} / {ser['p99_ms']:.2f} | {par['p50_ms']:.2f} / {par['p99_ms']:.2f} | "
                  f"{ser['p50_ms'] / par['p50_ms']:.2f}x | {d['agreement_with_serial']:.3f} |")

    print("\n## Throughput with 4 concurrent callers (serial queries)\n")
    print("| step | segments | kind | queries/s | p50 ms | p99 ms |")
    print("|---|---|---|---|---|---|")
    for s in queries:
        for k, d in s["throughput"].items():
            print(f"| {s['label']} | {s['segments']} | {k} | {d['queries_per_second']:,.0f} | {d['latency']['p50_ms']:.2f} | {d['latency']['p99_ms']:.2f} |")

    last = [s for s in steps if s["step"] in ("stats", "merge", "load")]
    if last:
        a = last[-1].get("after", last[-1])
        n = a["docs"]
        print(f"\n## Bytes per passage by component ({last[-1]['label']}, {n:,} passages)\n")
        print("| component | bytes/passage | GiB |")
        print("|---|---|---|")
        for c, b in sorted(a["components"].items(), key=lambda x: -x[1]):
            if b / n >= 1:
                print(f"| {c} | {b / n:,.0f} | {gib(b)} |")


if __name__ == "__main__":
    main()
