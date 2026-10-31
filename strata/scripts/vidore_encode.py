#!/usr/bin/env python3
"""Encode a ViDoRe dataset's pages and queries with a ColPali-family model,
for `strata-multivector-bench`.

Reads one of ViDoRe's `*_tesseract` datasets (a parquet file with the page
`image`, its `image_filename`, a `query` and the page's OCR text in
`text_description`; each row's page answers its query), and writes to OUT:

  pages.f32, queries.f32   little-endian float32 vectors, one after another
  pages.off, queries.off   little-endian u64: each item's first vector, and the total
  pages.jsonl              {"id": image_filename, "text": OCR text}, one per page
  queries.jsonl            {"query": text, "page": image_filename}
  meta.json                {"dataset", "model", "dim"}

Padding is dropped: each page and query keeps the vectors of its real tokens.

    pip install torch torchvision --index-url https://download.pytorch.org/whl/cpu
    pip install colpali-engine pyarrow pillow numpy
    curl -L -o docvqa.parquet https://huggingface.co/datasets/vidore/docvqa_test_subsampled_tesseract/resolve/main/data/test-00000-of-00001.parquet
    python3 vidore_encode.py docvqa.parquet out/docvqa --dataset vidore/docvqa_test_subsampled_tesseract
"""

import argparse
import io
import json
import os
import sys
import time

import numpy as np
import pyarrow.parquet as pq
import torch
from PIL import Image


def load_model(name, dtype):
    from colpali_engine.models import ColIdefics3, ColIdefics3Processor

    model = ColIdefics3.from_pretrained(name, torch_dtype=getattr(torch, dtype), device_map="cpu").eval()
    return model, ColIdefics3Processor.from_pretrained(name)


def encode(model, batch):
    with torch.no_grad():
        out = model(**batch)
    mask = batch["attention_mask"].bool()
    return [out[i][mask[i]].float().numpy() for i in range(out.shape[0])]


def write(out, name, items):
    offs = [0]
    with open(os.path.join(out, f"{name}.f32"), "wb") as f:
        for x in items:
            f.write(np.ascontiguousarray(x, dtype="<f4").tobytes())
            offs.append(offs[-1] + x.shape[0])
    np.array(offs, dtype="<u8").tofile(os.path.join(out, f"{name}.off"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("parquet")
    ap.add_argument("out")
    ap.add_argument("--dataset", required=True)
    ap.add_argument("--model", default="vidore/colSmol-256M")
    ap.add_argument("--batch", type=int, default=4)
    ap.add_argument("--dtype", default="bfloat16", help="bfloat16 (fast on CPUs with AMX or AVX-512 BF16) or float32")
    ap.add_argument("--limit", type=int, default=0, help="first N pages only (a smoke test)")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    torch.set_num_threads(os.cpu_count() or 1)

    t = pq.read_table(a.parquet, columns=["image", "image_filename", "query", "text_description"]).to_pylist()
    pages, seen, queries = [], set(), []
    for r in t:
        if r["image_filename"] not in seen:
            seen.add(r["image_filename"])
            pages.append(r)
        if r["query"]:
            queries.append({"query": r["query"], "page": r["image_filename"]})
    if a.limit:
        pages = pages[: a.limit]
        keep = {p["image_filename"] for p in pages}
        queries = [q for q in queries if q["page"] in keep]
    print(f"{a.dataset}: {len(pages)} pages, {len(queries)} queries", file=sys.stderr)

    model, proc = load_model(a.model, a.dtype)
    # Each batch is kept in OUT/cache, so a run that stops resumes there.
    cache = os.path.join(a.out, "cache")
    os.makedirs(cache, exist_ok=True)
    t0, encoded = time.time(), 0
    vecs = []
    for i in range(0, len(pages), a.batch):
        path = os.path.join(cache, f"{i:06d}.npz")
        if os.path.exists(path):
            with np.load(path) as z:
                vecs += [z[k] for k in sorted(z.files)]
            continue
        imgs = [Image.open(io.BytesIO(p["image"]["bytes"])).convert("RGB") for p in pages[i : i + a.batch]]
        out = encode(model, proc.process_images(imgs))
        np.savez(path, **{f"{j:03d}": v for j, v in enumerate(out)})
        vecs += out
        encoded += len(imgs)
        el = time.time() - t0
        print(f"pages {i + len(imgs)}/{len(pages)}, {el / encoded:.2f} s a page, {np.mean([v.shape[0] for v in vecs]):.0f} vectors a page", file=sys.stderr, flush=True)
    page_s = (time.time() - t0) / max(encoded, 1)
    write(a.out, "pages", vecs)
    with open(os.path.join(a.out, "pages.jsonl"), "w") as f:
        for p in pages:
            f.write(json.dumps({"id": p["image_filename"], "text": p["text_description"] or ""}) + "\n")

    t0 = time.time()
    qv = []
    for i in range(0, len(queries), 16):
        qv += encode(model, proc.process_queries([q["query"] for q in queries[i : i + 16]]))
    query_s = (time.time() - t0) / len(queries)
    write(a.out, "queries", qv)
    with open(os.path.join(a.out, "queries.jsonl"), "w") as f:
        for q in queries:
            f.write(json.dumps(q) + "\n")
    meta = {"dataset": a.dataset, "model": a.model, "dtype": a.dtype, "dim": int(vecs[0].shape[1]), "encode_s_per_page": page_s, "encode_s_per_query": query_s}
    with open(os.path.join(a.out, "meta.json"), "w") as f:
        json.dump(meta, f)
    print(json.dumps(meta), file=sys.stderr)


if __name__ == "__main__":
    main()
