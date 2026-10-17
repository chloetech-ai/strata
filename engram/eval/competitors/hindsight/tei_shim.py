#!/usr/bin/env python3
"""A small stand-in for Hugging Face's text-embeddings-inference (TEI) server,
for Hindsight's `tei` embeddings and reranker providers. It runs the ONNX
exports of Hindsight's two default models with onnxruntime on the CPU:

  python tei_shim.py embed  --model DIR --port 8080   # BAAI/bge-small-en-v1.5: CLS token, unit length
  python tei_shim.py rerank --model DIR --port 8081   # cross-encoder/ms-marco-MiniLM-L-6-v2: sigmoid of the logit

DIR holds the model repository's tokenizer.json and onnx/model.onnx. Both are
truncated at 512 tokens, as sentence-transformers truncates them.

Endpoints as TEI's: GET /info; POST /embed {"inputs": [...]} -> [[...]];
POST /rerank {"query", "texts"} -> [{"index", "score"}], highest score first.

Why: Hindsight runs these models with sentence-transformers by default, which
needs torch; this container can install neither torch (PyTorch's index is
blocked) nor models from Hugging Face, so the same models' ONNX files, copied
in from another machine, run here behind Hindsight's documented TEI provider.
"""

import argparse
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer


class Model:
    def __init__(self, kind, path, threads):
        self.kind = kind
        find = lambda name: next(p for p in (os.path.join(path, name), os.path.join(path, "onnx", name)) if os.path.exists(p))  # noqa: E731
        self.tok = Tokenizer.from_file(find("tokenizer.json"))
        self.tok.enable_truncation(max_length=512)
        pad = self.tok.token_to_id("[PAD]") or 0
        self.tok.enable_padding(pad_id=pad, pad_token="[PAD]")
        so = ort.SessionOptions()
        so.intra_op_num_threads = threads
        self.sess = ort.InferenceSession(find("model.onnx"), so, providers=["CPUExecutionProvider"])
        self.inputs = {i.name for i in self.sess.get_inputs()}
        self.lock = threading.Semaphore(2)

    def _run(self, enc):
        feed = {"input_ids": np.array([e.ids for e in enc], dtype=np.int64),
                "attention_mask": np.array([e.attention_mask for e in enc], dtype=np.int64)}
        if "token_type_ids" in self.inputs:
            feed["token_type_ids"] = np.array([e.type_ids for e in enc], dtype=np.int64)
        with self.lock:
            return self.sess.run(None, feed)[0]

    def embed(self, texts, batch=32):
        out = []
        for i in range(0, len(texts), batch):
            hidden = self._run(self.tok.encode_batch(texts[i:i + batch]))
            v = hidden[:, 0, :]
            v = v / np.clip(np.linalg.norm(v, axis=1, keepdims=True), 1e-12, None)
            out.extend(v.astype(float).tolist())
        return out

    def rerank(self, query, texts, batch=32):
        scores = []
        for i in range(0, len(texts), batch):
            logits = self._run(self.tok.encode_batch([(query, t) for t in texts[i:i + batch]]))
            scores.extend((1.0 / (1.0 + np.exp(-logits[:, 0]))).astype(float).tolist())
        return sorted(({"index": i, "score": s} for i, s in enumerate(scores)), key=lambda r: -r["score"])


def serve(model, model_id, port):
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def _send(self, code, obj):
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path.startswith("/info"):
                self._send(200, {"model_id": model_id, "model_type": model.kind, "max_input_length": 512, "model_dtype": "float32"})
            elif self.path.startswith("/health"):
                self._send(200, {})
            else:
                self._send(404, {"error": "not found"})

        def do_POST(self):
            try:
                req = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
                if self.path.startswith("/embed") and model.kind == "embed":
                    inputs = req.get("inputs")
                    self._send(200, model.embed([inputs] if isinstance(inputs, str) else list(inputs)))
                elif self.path.startswith("/rerank") and model.kind == "rerank":
                    self._send(200, model.rerank(req["query"], list(req["texts"])))
                else:
                    self._send(404, {"error": "not found"})
            except Exception as e:  # noqa: BLE001 - reported to the client
                self._send(500, {"error": str(e)})

    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("kind", choices=["embed", "rerank"])
    ap.add_argument("--model", required=True)
    ap.add_argument("--model-id", default="")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--threads", type=int, default=2)
    args = ap.parse_args()
    serve(Model(args.kind, args.model, args.threads), args.model_id or os.path.basename(args.model.rstrip("/")), args.port)


if __name__ == "__main__":
    main()
