#!/usr/bin/env python3
"""Throwaway TEI /embed stand-in for the loading phase only: a fixed random unit vector per text (384 dims,
as bge-small-en-v1.5). Every bank is exported and re-imported into an instance with the real model, which
re-embeds every fact and rebuilds the links, so these vectors never reach a recall."""
import hashlib, json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import numpy as np

def vec(t):
    rng = np.random.default_rng(int.from_bytes(hashlib.sha256(t.encode("utf-8", "replace")).digest()[:8], "little"))
    v = rng.standard_normal(384)
    return (v / np.linalg.norm(v)).tolist()

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, obj):
        b = json.dumps(obj).encode(); self.send_response(code)
        self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        self._send(200, {"model_id": "hash-standin", "model_type": "embed", "max_input_length": 512, "model_dtype": "float32"} if self.path.startswith("/info") else {})
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        x = req.get("inputs"); x = [x] if isinstance(x, str) else list(x)
        self._send(200, [vec(t) for t in x])

ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
