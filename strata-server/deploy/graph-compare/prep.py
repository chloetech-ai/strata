"""Prepares the graph engine comparison: the arXiv citation graph as files
each engine loads, the queries, and their exact answers.

    python prep.py --arxiv arxiv.jsonl --out DIR

Writes to DIR:
  nodes.csv, edges.csv     id / src,dst (no self-loops, no repeated edges)
  papers.ndjson            {"id", "cites"} rows for Strata's EDGES field
  links.ndjson             {"id", "src", "dst"} rows for Strata's edge table
  queries.json             seeds and pairs (seeded, so every run asks the same)
  reference.json           exact answers: neighbour counts, path lengths, top PPR

Neighbour counts are distinct nodes within k hops, the seed excluded. Paths
and "both" directions treat the graph as undirected; a pair of papers citing
each other is two edges between them, as every engine stores it. PPR is
personalized PageRank on that undirected graph (restart 0.15), solved by
power iteration to 1e-12.
"""

import argparse
import json
from pathlib import Path

import numpy as np
from scipy.sparse import csr_matrix
from scipy.sparse.csgraph import shortest_path

ALPHA = 0.15
N_QUERIES = 200
MAX_PATH = 10


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arxiv", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--seed", type=int, default=7)
    a = ap.parse_args()
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)

    cites = {}
    with open(a.arxiv) as f:
        for line in f:
            p = json.loads(line)
            cites[p["id"]] = sorted({c for c in p["cites"] if c != p["id"]})
    n = max(cites) + 1
    assert sorted(cites) == list(range(n)), "ids are 0..n-1"
    src = np.fromiter((s for s, ts in cites.items() for _ in ts), dtype=np.int64)
    dst = np.fromiter((t for ts in cites.values() for t in ts), dtype=np.int64)
    m = len(src)
    print(f"{n} nodes, {m} edges")

    with open(out / "nodes.csv", "w") as f:
        f.write("id\n")
        f.writelines(f"{i}\n" for i in range(n))
    with open(out / "edges.csv", "w") as f:
        f.write("src,dst\n")
        f.writelines(f"{s},{t}\n" for s, t in zip(src.tolist(), dst.tolist()))
    with open(out / "papers.ndjson", "w") as f:
        f.writelines(json.dumps({"id": i, "cites": json.dumps(cites[i])}) + "\n" for i in range(n))
    with open(out / "links.ndjson", "w") as f:
        f.writelines(json.dumps({"id": f"{s}>{t}", "src": s, "dst": t}) + "\n" for s, t in zip(src.tolist(), dst.tolist()))

    A = csr_matrix((np.ones(m), (src, dst)), shape=(n, n))
    U = (A + A.T).tocsr()  # undirected, mutual citations count twice

    rng = np.random.default_rng(a.seed)
    seeds = rng.choice(n, N_QUERIES, replace=False).tolist()
    pairs = [(int(x), int(y)) for x, y in zip(rng.choice(n, N_QUERIES), rng.choice(n, N_QUERIES))]

    def within(G, s, k):
        seen = np.zeros(n, dtype=bool)
        seen[s] = True
        frontier = np.array([s])
        for _ in range(k):
            nxt = np.unique(np.concatenate([G.indices[G.indptr[u]:G.indptr[u + 1]] for u in frontier] or [np.array([], dtype=np.int32)]))
            nxt = nxt[~seen[nxt]]
            seen[nxt] = True
            frontier = nxt
        return int(seen.sum()) - 1

    hops = {
        "both_1": [within(U, s, 1) for s in seeds],
        "both_2": [within(U, s, 2) for s in seeds],
        "out_3": [within(A, s, 3) for s in seeds],
    }
    for k, v in hops.items():
        print(f"hops {k}: mean {np.mean(v):.0f}, max {max(v)}")

    d = shortest_path(U, unweighted=True, directed=False, indices=[p[0] for p in pairs])
    lengths = [None if not np.isfinite(d[i, b]) or d[i, b] > MAX_PATH else int(d[i, b]) for i, (_, b) in enumerate(pairs)]
    found = [x for x in lengths if x is not None]
    print(f"paths: {len(found)} found, mean length {np.mean(found):.2f}")

    deg = np.asarray(U.sum(axis=1)).ravel()
    inv = np.where(deg > 0, 1.0 / np.maximum(deg, 1), 0.0)
    P = csr_matrix(U.multiply(inv[:, None]))  # row-stochastic where deg > 0
    X0 = np.zeros((N_QUERIES, n))
    X0[np.arange(N_QUERIES), seeds] = 1.0
    X = X0.copy()
    for it in range(1000):
        stay = X * (deg == 0)  # mass at a node without edges stays there
        Xn = ALPHA * X0 + (1 - ALPHA) * (np.asarray((P.T @ X.T).T) + stay)
        delta = np.abs(Xn - X).sum(axis=1).max()
        X = Xn
        if delta < 1e-12:
            break
    print(f"ppr: {it + 1} iterations")
    top = np.argsort(-X, axis=1)[:, :50]
    ppr = [{"ids": top[i].tolist(), "scores": X[i, top[i]].tolist()} for i in range(N_QUERIES)]

    json.dump({"nodes": n, "edges": m, "seed": a.seed, "seeds": seeds, "pairs": pairs, "max_path": MAX_PATH, "alpha": ALPHA}, open(out / "queries.json", "w"))
    json.dump({"hops": hops, "path_len": lengths, "ppr": ppr}, open(out / "reference.json", "w"))


if __name__ == "__main__":
    main()
