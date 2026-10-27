"""Prepare MS MARCO v2.1 segmented passages with open Snowflake Arctic Embed M
v1.5 vectors for the Strata scale probe (`strata-scale`).

Usage: python3 scripts/prepare_msmarco.py OUT_DIR [--shards 0,1,2] [--dim 256]
       (needs pyarrow + numpy; curl on PATH)

Sources, both public and open:
  vectors  huggingface.co/datasets/Snowflake/msmarco-v2.1-snowflake-arctic-embed-m-v1.5
           corpus/NN.parquet: doc_id + 768-d embedding (Apache 2.0)
  text     msmarco.z22.web.core.windows.net msmarco_v2.1_doc_segmented.tar,
           member msmarco_v2.1_doc_segmented_NN.json.gz, fetched alone with an
           HTTP range request (shard NN of both holds the same passages in the
           same order; this script checks every id)

Per shard it downloads the Parquet file, cuts each vector to its first `dim`
dimensions (Matryoshka) and renormalises it, appends it to vecs.f32, deletes
the Parquet file, and keeps the shard's text as text_NN.json.gz (the loader
streams it with zcat). OUT_DIR gets
  vecs.f32      n x dim float32, row-major, little-endian, unit length
  attrs.bin     n x 8 bytes: u16 year, u8 category, u8 topic, f32 rating (LE)
  text_NN.json.gz  the shard's passages, one JSON object per line
  centroids.f32 32 x dim, the k-means centroids behind `topic`
  meta.json     {"dim", "rows", "shards": [{"shard", "rows", "start"}]}

year, category and rating are synthetic and independent of content, as in
prepare_beir.py: year uniform in 1990..2024, category Zipf(1) over 64 values,
rating uniform in [0, 5) rounded to 0.01. topic is the k-means cluster (32
clusters, trained on 50,000 vectors of the first shard) of the vector.
"""
import argparse
import gzip
import json
import os
import subprocess

import numpy as np
import pyarrow.parquet as pq

HF = "https://huggingface.co/datasets/Snowflake/msmarco-v2.1-snowflake-arctic-embed-m-v1.5/resolve/main/corpus/{:02d}.parquet"
TAR = "https://msmarco.z22.web.core.windows.net/msmarcoranking/msmarco_v2.1_doc_segmented.tar"
TOPICS = 32


def curl(args):
    subprocess.run(["curl", "-sSfL", "--retry", "5", "--retry-all-errors"] + args, check=True)


def range_bytes(a, b):
    return subprocess.run(["curl", "-sSfL", "--retry", "5", "-r", f"{a}-{b}", TAR], check=True, capture_output=True).stdout


def tar_index(out):
    """Offsets of the tar members' data, found by walking the 512-byte headers."""
    path = os.path.join(out, "tar_index.json")
    if os.path.exists(path):
        return json.load(open(path))
    idx, off = {}, 0
    while True:
        h = range_bytes(off, off + 511)
        if len(h) < 512 or h == b"\0" * 512:
            break
        name = h[:100].rstrip(b"\0").decode()
        sf = h[124:136]
        size = int.from_bytes(sf[1:], "big") if sf[0] & 0x80 else int(sf.rstrip(b"\0 ").decode() or "0", 8)
        idx[os.path.basename(name)] = [off + 512, size]
        off += 512 + (size + 511) // 512 * 512
    json.dump(idx, open(path, "w"))
    return idx


def kmeans(x, k, iters=12, seed=7):
    rng = np.random.default_rng(seed)
    c = x[rng.choice(len(x), size=k, replace=False)].copy()
    for _ in range(iters):
        a = np.argmax(x @ c.T, axis=1)
        for j in range(k):
            m = x[a == j]
            if len(m):
                c[j] = m.mean(axis=0)
        c /= np.linalg.norm(c, axis=1, keepdims=True) + 1e-12
    return c


def cut(e, dim):
    """768-d rows -> first `dim` dimensions, renormalised."""
    x = np.asarray(e.values.to_numpy(zero_copy_only=False), dtype=np.float32).reshape(len(e), -1)[:, :dim]
    return np.ascontiguousarray(x / (np.linalg.norm(x, axis=1, keepdims=True) + 1e-12))


def prepare_shard(out, shard, dim, start, idx, meta):
    pq_path = os.path.join(out, f"raw_{shard:02d}.parquet")
    text_path = os.path.join(out, f"text_{shard:02d}.json.gz")
    if not os.path.exists(text_path):
        off, size = idx[f"msmarco_v2.1_doc_segmented_{shard:02d}.json.gz"]
        curl(["-r", f"{off}-{off + size - 1}", "-o", text_path + ".part", TAR])
        os.rename(text_path + ".part", text_path)
    if not os.path.exists(pq_path):
        curl(["-C", "-", "-o", pq_path + ".part", HF.format(shard)])
        os.rename(pq_path + ".part", pq_path)
    pf = pq.ParquetFile(pq_path)
    n = pf.metadata.num_rows
    print(f"   shard {shard:02d}: {n} rows", flush=True)

    cpath = os.path.join(out, "centroids.f32")
    if os.path.exists(cpath):
        cents = np.fromfile(cpath, dtype="<f4").reshape(TOPICS, dim)
    else:
        first = next(pf.iter_batches(batch_size=50_000, columns=["embedding"]))
        cents = kmeans(cut(first.column(0), dim), TOPICS)
        cents.astype("<f4").tofile(cpath)

    rng = np.random.default_rng(20261012 + shard)
    zipf = 1.0 / np.arange(1, 65)
    attrs = np.zeros(n, dtype=[("year", "<u2"), ("cat", "u1"), ("topic", "u1"), ("rating", "<f4")])
    attrs["year"] = rng.integers(1990, 2025, size=n)
    attrs["cat"] = rng.choice(64, size=n, p=zipf / zipf.sum())
    attrs["rating"] = np.round(rng.random(n) * 5.0, 2)

    # Drop what an interrupted run of this shard appended.
    for name, row_bytes in [("vecs.f32", dim * 4), ("attrs.bin", attrs.itemsize)]:
        p = os.path.join(out, name)
        if os.path.exists(p):
            os.truncate(p, start * row_bytes)
    text = gzip.open(text_path, "rt", encoding="utf-8")
    vf = open(os.path.join(out, "vecs.f32"), "ab")
    done = 0
    for b in pf.iter_batches(batch_size=65536, columns=["doc_id", "embedding"]):
        ids = b.column(0).to_pylist()
        for i in ids:
            got = json.loads(text.readline())["docid"]
            if got != i:
                raise SystemExit(f"shard {shard}: row {done}: vector id {i} but text id {got}")
            done += 1
        x = cut(b.column(1), dim)
        attrs["topic"][done - len(ids):done] = np.argmax(x @ cents.T, axis=1)
        vf.write(x.astype("<f4").tobytes())
    if text.readline():
        raise SystemExit(f"shard {shard}: text has more rows than vectors")
    vf.close()
    with open(os.path.join(out, "attrs.bin"), "ab") as f:
        f.write(attrs.tobytes())
    os.remove(pq_path)
    meta["shards"].append({"shard": shard, "rows": n, "start": start})
    meta["rows"] = start + n
    json.dump(meta, open(os.path.join(out, "meta.json"), "w"), indent=1)
    return n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--shards", default="0,1,2")
    ap.add_argument("--dim", type=int, default=256)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    mpath = os.path.join(a.out, "meta.json")
    meta = json.load(open(mpath)) if os.path.exists(mpath) else {"dim": a.dim, "rows": 0, "shards": []}
    if meta["dim"] != a.dim:
        raise SystemExit(f"{a.out} holds {meta['dim']}-d vectors")
    idx = tar_index(a.out)
    have = {s["shard"] for s in meta["shards"]}
    for s in [int(x) for x in a.shards.split(",")]:
        if s not in have:
            prepare_shard(a.out, s, a.dim, meta["rows"], idx, meta)
    print(f"   {meta['rows']} rows in {a.out}", flush=True)


if __name__ == "__main__":
    main()
