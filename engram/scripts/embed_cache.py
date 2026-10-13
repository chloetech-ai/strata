"""Embeds texts dumped by `engram-bench retrieval --dump-missing` with
BGE-small (CLS pooling, unit length; queries get BGE's instruction prefix)
and appends them to the bench's embedding cache (u64 key + f32 vector).
The daemon computes the same vectors with candle (feature `bert`); this
path only makes benchmark-scale embedding feasible on a CPU.
Usage: python3 scripts/embed_cache.py MISSING.jsonl CACHE.bin [MODEL_DIR]"""
import json
import struct
import sys
import time

import numpy as np
import torch
from sentence_transformers import SentenceTransformer

missing, cache = sys.argv[1], sys.argv[2]
model_dir = sys.argv[3] if len(sys.argv) > 3 else "/home/user/data/models/bge-small-en-v1.5"
PREFIX = "Represent this sentence for searching relevant passages: "
torch.set_num_threads(4)
model = SentenceTransformer("BAAI/bge-small-en-v1.5", device="cpu")
model.max_seq_length = 512
rows = [json.loads(l) for l in open(missing)]
texts = [(PREFIX + r["text"]) if r["query"] else r["text"] for r in rows]
order = sorted(range(len(texts)), key=lambda i: len(texts[i]))
t0 = time.time()
out = open(cache, "ab")
B = 256
for s in range(0, len(order), B):
    idx = order[s:s + B]
    vecs = model.encode([texts[i] for i in idx], batch_size=64, normalize_embeddings=True, convert_to_numpy=True)
    for i, v in zip(idx, vecs):
        out.write(struct.pack("<Q", int(rows[i]["key"])) + np.asarray(v, dtype="<f4").tobytes())
    if (s // B) % 40 == 0:
        print(f"{s + len(idx)}/{len(texts)} in {time.time() - t0:.0f} s", flush=True)
out.close()
print(f"embedded {len(texts)} texts in {time.time() - t0:.0f} s")
