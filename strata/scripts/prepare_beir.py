"""Download BEIR collections with Cohere embed-english-v3 embeddings and convert
them to the plain files the Strata loaders read.

Usage: python3 scripts/prepare_beir.py OUT_DIR [dataset ...]
       (default datasets: scifact fiqa trec-covid quora; needs pyarrow + numpy)

Per dataset, OUT_DIR/<name>/ gets
  docs.jsonl        {"id","title","text","year","category","rating","topic"} per line
  doc_vecs.f32      n x dim float32, row-major, little-endian
  queries.jsonl     {"id","text"} for the test queries
  query_vecs.f32    m x dim float32
  qrels.tsv         query_id <TAB> doc_id <TAB> grade
  meta.json         {"docs": n, "queries": m, "dim": d}

year, category and rating are synthetic and independent of content (seeded,
reproducible): year uniform in 1990..2024, category Zipf(1) over 64 values,
rating uniform in [0, 5). topic is content-correlated: the k-means cluster
(32 clusters) of the document embedding, so filters on it correlate with
queries the way real metadata does.
"""
import json
import os
import subprocess
import sys

import numpy as np
import pyarrow.parquet as pq

BASE = "https://huggingface.co/datasets/CohereLabs/beir-embed-english-v3/resolve/main"
TREE = "https://huggingface.co/api/datasets/CohereLabs/beir-embed-english-v3/tree/main/{}/corpus"


def fetch(url, path):
    if not os.path.exists(path) or os.path.getsize(path) == 0:
        subprocess.run(["curl", "-sSfL", "-o", path, url], check=True)


def corpus_files(name):
    out = subprocess.run(["curl", "-sSfL", TREE.format(name)], check=True, capture_output=True).stdout
    return sorted(f["path"] for f in json.loads(out) if f["path"].endswith(".parquet"))


def kmeans(x, k, iters=12, seed=7):
    rng = np.random.default_rng(seed)
    sample = x[rng.choice(len(x), size=min(len(x), 50_000), replace=False)]
    c = sample[rng.choice(len(sample), size=k, replace=False)].copy()
    for _ in range(iters):
        a = np.argmax(sample @ c.T, axis=1)
        for j in range(k):
            m = sample[a == j]
            if len(m):
                c[j] = m.mean(axis=0)
        c /= np.linalg.norm(c, axis=1, keepdims=True) + 1e-12
    labels = np.empty(len(x), dtype=np.int32)
    for s in range(0, len(x), 65536):
        labels[s:s + 65536] = np.argmax(x[s:s + 65536] @ c.T, axis=1)
    return labels


def prepare(out_dir, name):
    d = os.path.join(out_dir, name)
    raw = os.path.join(d, "raw")
    os.makedirs(raw, exist_ok=True)
    print(f"== {name}", flush=True)
    parts = []
    for p in corpus_files(name):
        local = os.path.join(raw, p.replace("/", "_"))
        fetch(f"{BASE}/{p}", local)
        parts.append(local)
    for p in ["queries/test.parquet", "qrels/test.parquet"]:
        fetch(f"{BASE}/{name}/{p}", os.path.join(raw, p.replace("/", "_")))

    ids, titles, texts, vecs = [], [], [], []
    for p in parts:
        t = pq.read_table(p)
        ids += t.column("_id").to_pylist()
        titles += t.column("title").to_pylist()
        texts += t.column("text").to_pylist()
        e = t.column("emb").combine_chunks()
        dim = len(e[0])
        vecs.append(np.asarray(e.values.to_numpy(zero_copy_only=False), dtype=np.float32).reshape(-1, dim))
    x = np.concatenate(vecs)
    n, dim = x.shape
    norms = np.linalg.norm(x, axis=1)
    print(f"   {n} docs, dim {dim}, |v| in [{norms.min():.3f}, {norms.max():.3f}]", flush=True)
    x.astype("<f4").tofile(os.path.join(d, "doc_vecs.f32"))

    rng = np.random.default_rng(20261012)
    years = rng.integers(1990, 2025, size=n)
    zipf = 1.0 / np.arange(1, 65)
    cats = rng.choice(64, size=n, p=zipf / zipf.sum())
    ratings = np.round(rng.random(n) * 5.0, 2)
    topics = kmeans(x / (norms[:, None] + 1e-12), 32)
    with open(os.path.join(d, "docs.jsonl"), "w") as f:
        for i in range(n):
            f.write(json.dumps({
                "id": ids[i], "title": titles[i] or "", "text": texts[i] or "",
                "year": int(years[i]), "category": f"c{int(cats[i]):02d}",
                "rating": float(ratings[i]), "topic": f"t{int(topics[i]):02d}",
            }) + "\n")

    q = pq.read_table(os.path.join(raw, "queries_test.parquet"))
    qrels = pq.read_table(os.path.join(raw, "qrels_test.parquet")).to_pylist()
    judged = {r["query_id"] for r in qrels if r["score"] > 0}
    qids, qtexts, qvecs = [], [], []
    for row in q.to_pylist():
        if row["_id"] in judged:
            qids.append(row["_id"]); qtexts.append(row["text"]); qvecs.append(row["emb"])
    np.asarray(qvecs, dtype="<f4").tofile(os.path.join(d, "query_vecs.f32"))
    with open(os.path.join(d, "queries.jsonl"), "w") as f:
        for i, t in zip(qids, qtexts):
            f.write(json.dumps({"id": i, "text": t}) + "\n")
    with open(os.path.join(d, "qrels.tsv"), "w") as f:
        for r in qrels:
            f.write(f"{r['query_id']}\t{r['corpus_id']}\t{r['score']}\n")
    json.dump({"docs": n, "queries": len(qids), "dim": dim}, open(os.path.join(d, "meta.json"), "w"))
    print(f"   {len(qids)} judged test queries, {len(qrels)} qrels", flush=True)


if __name__ == "__main__":
    out = sys.argv[1]
    for name in sys.argv[2:] or ["scifact", "fiqa", "trec-covid", "quora"]:
        prepare(out, name)
