"""Embed queries with Snowflake Arctic Embed M v1.5 (ONNX, CPU) for the Strata
scale probe, the model behind the passage vectors of prepare_msmarco.py.

Usage: python3 scripts/embed_queries.py MODEL_DIR QUERIES_TSV OUT_DIR [--dim 256] [--n 1000]
       (needs onnxruntime, tokenizers, numpy; MODEL_DIR holds model.onnx and
       tokenizer.json from huggingface.co/Snowflake/snowflake-arctic-embed-m-v1.5)

As the model card says: the query prefix "Represent this sentence for
searching relevant passages: ", the CLS token's output, unit length; then the
first `dim` dimensions (Matryoshka), renormalised. The first `n` queries of
QUERIES_TSV (id <TAB> text) are kept. OUT_DIR gets queries.jsonl
{"id","text"} and query_vecs.f32 (n x dim float32).

With --check VECS TEXT_GZ it instead embeds the first passages of a prepared
shard and prints their cosine with the dataset's vectors, to confirm the
model, pooling and truncation match.
"""
import argparse
import gzip
import json
import os

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

PREFIX = "Represent this sentence for searching relevant passages: "


class Model:
    def __init__(self, d):
        self.tok = Tokenizer.from_file(os.path.join(d, "tokenizer.json"))
        self.tok.enable_truncation(512)
        self.tok.enable_padding(pad_id=0, pad_token="[PAD]")
        self.sess = ort.InferenceSession(os.path.join(d, "model.onnx"), providers=["CPUExecutionProvider"])
        self.inputs = {i.name for i in self.sess.get_inputs()}

    def embed(self, texts, dim, batch=32):
        out = []
        for s in range(0, len(texts), batch):
            enc = self.tok.encode_batch(texts[s:s + batch])
            feed = {
                "input_ids": np.array([e.ids for e in enc], dtype=np.int64),
                "attention_mask": np.array([e.attention_mask for e in enc], dtype=np.int64),
                "token_type_ids": np.array([e.type_ids for e in enc], dtype=np.int64),
            }
            h = self.sess.run(None, {k: v for k, v in feed.items() if k in self.inputs})[0]
            out.append(h[:, 0, :])
        x = np.concatenate(out).astype(np.float32)
        x /= np.linalg.norm(x, axis=1, keepdims=True)
        x = x[:, :dim]
        return x / np.linalg.norm(x, axis=1, keepdims=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("model")
    ap.add_argument("queries", nargs="?")
    ap.add_argument("out", nargs="?")
    ap.add_argument("--dim", type=int, default=256)
    ap.add_argument("--n", type=int, default=1000)
    ap.add_argument("--check", nargs=2, metavar=("VECS", "TEXT_GZ"))
    a = ap.parse_args()
    m = Model(a.model)
    if a.check:
        vecs = np.fromfile(a.check[0], dtype="<f4", count=64 * a.dim).reshape(64, a.dim)
        rows = [json.loads(l) for _, l in zip(range(64), gzip.open(a.check[1], "rt"))]
        for name, f in [("segment", lambda r: r["segment"]),
                        ("title + segment", lambda r: r["title"] + "\n" + r["segment"]),
                        ("title + headings + segment", lambda r: r["title"] + "\n" + r["headings"] + "\n" + r["segment"])]:
            x = m.embed([f(r) for r in rows], a.dim)
            cos = np.sum(x * vecs, axis=1)
            print(f"{name:28s} cosine mean {cos.mean():.4f} min {cos.min():.4f}")
        return
    ids, texts = [], []
    for line in open(a.queries, encoding="utf-8"):
        i, t = line.rstrip("\n").split("\t", 1)
        ids.append(i)
        texts.append(t.strip())
        if len(ids) == a.n:
            break
    x = m.embed([PREFIX + t for t in texts], a.dim)
    os.makedirs(a.out, exist_ok=True)
    x.astype("<f4").tofile(os.path.join(a.out, "query_vecs.f32"))
    with open(os.path.join(a.out, "queries.jsonl"), "w") as f:
        for i, t in zip(ids, texts):
            f.write(json.dumps({"id": i, "text": t}) + "\n")
    print(f"{len(ids)} queries, dim {a.dim}")


if __name__ == "__main__":
    main()
