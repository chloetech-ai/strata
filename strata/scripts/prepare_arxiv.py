#!/usr/bin/env python3
"""Turns ogbn-arxiv (169,343 arXiv CS papers and 1.17M citations, from the
Open Graph Benchmark) plus the papers' titles and abstracts into one JSON
line per paper, for strata-graph-bench:

  {"id": 0, "year": 2013, "category": "cs.na", "title": ..., "abstract": ...,
   "cites": [411, 640, ...], "split": "train" | "valid" | "test"}

Edges in OGB's edge list run from the citing paper to the cited one; the
script checks that against publication years.

  python3 -I scripts/prepare_arxiv.py /home/user/data/graph/arxiv
"""

import csv
import gzip
import io
import json
import os
import sys
import tarfile


def rows(path):
    with gzip.open(path, "rt") as f:
        yield from csv.reader(f)


def main():
    base = sys.argv[1]
    raw = os.path.join(base, "raw", "arxiv")
    paper_id = [int(r[1]) for r in list(rows(os.path.join(raw, "mapping", "nodeidx2paperid.csv.gz")))[1:]]
    n = len(paper_id)
    year = [int(r[0]) for r in rows(os.path.join(raw, "raw", "node_year.csv.gz"))]
    label = [int(r[0]) for r in rows(os.path.join(raw, "raw", "node-label.csv.gz"))]
    cats = {int(r[0]): r[1].replace("arxiv ", "").replace(" ", ".") for r in list(rows(os.path.join(raw, "mapping", "labelidx2arxivcategeory.csv.gz")))[1:]}
    split = ["train"] * n
    for name in ("valid", "test"):
        for r in rows(os.path.join(raw, "split", "time", f"{name}.csv.gz")):
            split[int(r[0])] = name
    cites = [[] for _ in range(n)]
    forward = backward = 0
    for r in rows(os.path.join(raw, "raw", "edge.csv.gz")):
        s, t = int(r[0]), int(r[1])
        cites[s].append(t)
        forward += year[s] >= year[t]
        backward += year[s] < year[t]
    print(f"{n} papers, {forward + backward} edges; citing paper not older in {forward / (forward + backward):.1%}", file=sys.stderr)
    text = {}
    with tarfile.open(os.path.join(base, "titleabs.tsv.gz"), "r:gz") as tar:
        for m in tar.getmembers():
            f = tar.extractfile(m)
            if f is None:
                continue
            for line in io.TextIOWrapper(f, encoding="utf-8", errors="replace"):
                parts = line.rstrip("\n").split("\t")
                if len(parts) >= 3 and parts[0].isdigit():
                    text[int(parts[0])] = (parts[1].strip(), parts[2].strip())
    missing = 0
    with open(os.path.join(base, "arxiv.jsonl"), "w") as out:
        for i in range(n):
            t = text.get(paper_id[i])
            missing += t is None
            title, abstract = t if t else ("", "")
            out.write(json.dumps({"id": i, "year": year[i], "category": cats[label[i]], "title": title, "abstract": abstract,
                                  "cites": sorted(set(cites[i])), "split": split[i]}) + "\n")
    print(f"wrote {n} papers ({missing} without text) to {base}/arxiv.jsonl", file=sys.stderr)


if __name__ == "__main__":
    main()
