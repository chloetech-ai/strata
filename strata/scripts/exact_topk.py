"""Exact top-k neighbours of the probe queries among the first ROWS passages
of a directory prepared by prepare_msmarco.py (dot product on the unit
vectors, float32), the reference for strata-scale's vector recall.

Usage: python3 scripts/exact_topk.py DATA ROWS QUERIES_DIR OUT.json [--k 100] [--n 1000]

OUT.json: {"rows": ROWS, "k": k, "ids": [[doc id, ...] per query]}.
"""
import argparse
import gzip
import json
import os

import numpy as np


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("rows", type=int)
    ap.add_argument("queries")
    ap.add_argument("out")
    ap.add_argument("--k", type=int, default=100)
    ap.add_argument("--n", type=int, default=1000)
    a = ap.parse_args()
    meta = json.load(open(os.path.join(a.data, "meta.json")))
    dim = meta["dim"]
    q = np.fromfile(os.path.join(a.queries, "query_vecs.f32"), dtype="<f4").reshape(-1, dim)[: a.n]
    x = np.memmap(os.path.join(a.data, "vecs.f32"), dtype="<f4", mode="r", shape=(a.rows, dim))
    best_s = np.full((len(q), a.k), -np.inf, dtype=np.float32)
    best_i = np.zeros((len(q), a.k), dtype=np.int64)
    step = 200_000
    for s in range(0, a.rows, step):
        sc = q @ np.asarray(x[s:s + step]).T
        kk = min(a.k, sc.shape[1])
        part = np.argpartition(-sc, kk - 1, axis=1)[:, :kk]
        cs = np.concatenate([best_s, np.take_along_axis(sc, part, axis=1)], axis=1)
        ci = np.concatenate([best_i, part + s], axis=1)
        keep = np.argpartition(-cs, a.k - 1, axis=1)[:, : a.k]
        best_s, best_i = np.take_along_axis(cs, keep, axis=1), np.take_along_axis(ci, keep, axis=1)
    order = np.argsort(-best_s, axis=1, kind="stable")
    best_i = np.take_along_axis(best_i, order, axis=1)

    # Row number -> doc id, from the shards' text files.
    want = set(best_i.ravel().tolist())
    names = {}
    for sh in meta["shards"]:
        lo, hi = sh["start"], sh["start"] + sh["rows"]
        if lo >= a.rows or not any(lo <= w < hi for w in want):
            continue
        with gzip.open(os.path.join(a.data, f"text_{sh['shard']:02d}.json.gz"), "rt", encoding="utf-8") as f:
            for r, line in enumerate(f, start=lo):
                if r >= min(hi, a.rows):
                    break
                if r in want:
                    names[r] = json.loads(line)["docid"]
    ids = [[names[int(i)] for i in row] for row in best_i]
    json.dump({"rows": a.rows, "k": a.k, "ids": ids}, open(a.out, "w"))
    print(f"exact top-{a.k} of {len(q)} queries over {a.rows} rows -> {a.out}")


if __name__ == "__main__":
    main()
