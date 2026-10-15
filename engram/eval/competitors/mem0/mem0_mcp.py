#!/usr/bin/env python3
"""mem0 (github.com/mem0ai/mem0, the open-source library) behind a small MCP server.

Tools: add_memory(text), search_memory(query, limit), list_memories().
All memories belong to one user id per project (MEM0_BENCH_USER, default
the working directory's name).

There is no API key in the benchmark container, only an authenticated
`claude` CLI, so mem0's two model dependencies are replaced:
- LLM: ClaudeCliLLM runs `claude -p --model <MEM0_BENCH_MODEL> --tools ""
  --no-session-persistence --output-format json --system-prompt ...` with an
  isolated environment (PATH, HOME, ANTHROPIC_BASE_URL only) and appends
  each result message (with its cost) to $MEM0_BENCH_DIR/llm-results.jsonl;
- embedder: OnnxMiniLM runs all-MiniLM-L6-v2 (Chroma's ONNX export, 384
  dimensions) on the CPU with onnxruntime.
The vector store is mem0's default, Qdrant, in local on-disk mode under
$MEM0_BENCH_DIR/qdrant, and the history database is $MEM0_BENCH_DIR/history.db.

mem0's config validators accept only known provider names, so the factories'
"openai" entries are pointed at these two classes before Memory is built.

  MEM0_BENCH_DIR=... MEM0_BENCH_ONNX=.../all-MiniLM-L6-v2 python mem0_mcp.py          # MCP server on stdio
  MEM0_BENCH_DIR=... MEM0_BENCH_ONNX=... python mem0_mcp.py dump                       # print all memories as JSON
"""

import json
import os
import subprocess
import sys
import time

os.environ.setdefault("MEM0_TELEMETRY", "False")

from mem0 import Memory  # noqa: E402
from mem0.configs.llms.base import BaseLlmConfig  # noqa: E402
from mem0.embeddings.base import EmbeddingBase  # noqa: E402
from mem0.llms.base import LLMBase  # noqa: E402
from mem0.utils.factory import EmbedderFactory, LlmFactory  # noqa: E402

STATE = os.environ.get("MEM0_BENCH_DIR") or os.path.join(os.path.expanduser("~"), ".mem0-bench")
ONNX_DIR = os.environ.get("MEM0_BENCH_ONNX", "")
MODEL = os.environ.get("MEM0_BENCH_MODEL", "haiku")
CLAUDE = os.environ.get("MEM0_BENCH_CLAUDE", "claude")
USER = os.environ.get("MEM0_BENCH_USER") or os.path.basename(os.getcwd()) or "project"
LLM_LOG = os.path.join(STATE, "llm-results.jsonl")


class ClaudeCliLLM(LLMBase):
    def __init__(self, config=None):
        super().__init__(config)

    def generate_response(self, messages, tools=None, tool_choice="auto", response_format=None, **kwargs):
        system = "\n\n".join(m["content"] for m in messages if m.get("role") == "system")
        rest = [m for m in messages if m.get("role") != "system"]
        prompt = rest[0]["content"] if len(rest) == 1 else "\n\n".join(f"{m['role'].upper()}: {m['content']}" for m in rest)
        if response_format and response_format.get("type") == "json_object":
            system += "\n\nRespond with a single valid JSON object and nothing else (no code fences, no commentary)."
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": os.environ.get("HOME", STATE), "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1"}
        if os.environ.get("ANTHROPIC_BASE_URL"):
            env["ANTHROPIC_BASE_URL"] = os.environ["ANTHROPIC_BASE_URL"]
        cmd = [CLAUDE, "-p", prompt, "--model", self.config.model or MODEL, "--tools", "", "--no-session-persistence",
               "--output-format", "json", "--system-prompt", system or "You are a helpful assistant."]
        os.makedirs(STATE, exist_ok=True)
        t0 = time.time()
        p = subprocess.run(cmd, cwd=STATE, env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=300)
        try:
            out = json.loads(p.stdout)
        except json.JSONDecodeError:
            raise RuntimeError(f"claude -p failed ({p.returncode}): {(p.stdout + p.stderr)[-500:]}")
        out["wall_s"] = round(time.time() - t0, 2)
        with open(LLM_LOG, "a") as f:
            f.write(json.dumps({k: out.get(k) for k in ("type", "session_id", "total_cost_usd", "modelUsage", "usage", "is_error", "wall_s")}) + "\n")
        if out.get("is_error"):
            raise RuntimeError(f"claude -p error: {str(out.get('result'))[:300]}")
        return out.get("result") or ""


class OnnxMiniLM(EmbeddingBase):
    """all-MiniLM-L6-v2 (mean pooling, unit length) with onnxruntime."""

    def __init__(self, config=None):
        super().__init__(config)
        import numpy as np
        import onnxruntime as ort
        from tokenizers import Tokenizer

        d = os.path.join(ONNX_DIR, "onnx") if os.path.isdir(os.path.join(ONNX_DIR, "onnx")) else ONNX_DIR
        self.np = np
        self.tok = Tokenizer.from_file(os.path.join(d, "tokenizer.json"))
        self.tok.enable_truncation(max_length=256)
        self.tok.enable_padding(pad_id=0, pad_token="[PAD]")
        so = ort.SessionOptions()
        so.intra_op_num_threads = 2
        self.sess = ort.InferenceSession(os.path.join(d, "model.onnx"), so, providers=["CPUExecutionProvider"])
        self.inputs = {i.name for i in self.sess.get_inputs()}

    def embed_batch(self, texts, memory_action="add"):
        np = self.np
        texts = [t if isinstance(t, str) else json.dumps(t) for t in texts]
        enc = self.tok.encode_batch(texts)
        ids = np.array([e.ids for e in enc], dtype=np.int64)
        mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
        feed = {"input_ids": ids, "attention_mask": mask}
        if "token_type_ids" in self.inputs:
            feed["token_type_ids"] = np.zeros_like(ids)
        hidden = self.sess.run(None, feed)[0]
        m = mask[..., None].astype(np.float32)
        v = (hidden * m).sum(1) / np.clip(m.sum(1), 1e-9, None)
        v /= np.clip(np.linalg.norm(v, axis=1, keepdims=True), 1e-12, None)
        return v.tolist()

    def embed(self, text, memory_action=None):
        return self.embed_batch([text], memory_action)[0]


def build():
    LlmFactory.provider_to_class["openai"] = (f"{__name__}.ClaudeCliLLM", BaseLlmConfig)
    EmbedderFactory.provider_to_class["openai"] = f"{__name__}.OnnxMiniLM"
    os.makedirs(STATE, exist_ok=True)
    return Memory.from_config({
        "llm": {"provider": "openai", "config": {"model": MODEL}},
        "embedder": {"provider": "openai", "config": {"model": "all-MiniLM-L6-v2", "embedding_dims": 384}},
        "vector_store": {"provider": "qdrant", "config": {"collection_name": "memories", "path": os.path.join(STATE, "qdrant"), "embedding_model_dims": 384, "on_disk": True}},
        "history_db_path": os.path.join(STATE, "history.db"),
    })


def _items(res):
    return res.get("results", res) if isinstance(res, dict) else res


def main():
    from mcp.server.fastmcp import FastMCP

    mem = build()
    server = FastMCP("mem0")

    @server.tool()
    def add_memory(text: str) -> str:
        """Save something worth remembering in later sessions on this project: a requirement, rule, decision (and why), preference, constraint or fact. mem0 extracts the facts from the text and merges them with what it already holds."""
        res = _items(mem.add([{"role": "user", "content": text}], user_id=USER))
        return json.dumps(res, default=str)

    @server.tool()
    def search_memory(query: str, limit: int = 8) -> str:
        """Search the project's long-term memory (facts, rules and decisions saved in earlier sessions) by meaning."""
        res = _items(mem.search(query, filters={"user_id": USER}, top_k=limit))
        return json.dumps([{"memory": r.get("memory"), "score": r.get("score"), "created_at": r.get("created_at")} for r in res], default=str)

    @server.tool()
    def list_memories() -> str:
        """List everything in the project's long-term memory."""
        res = _items(mem.get_all(filters={"user_id": USER}, top_k=500))
        return json.dumps([{"memory": r.get("memory"), "created_at": r.get("created_at")} for r in res], default=str)

    server.run()


def dump():
    mem = build()
    users = sys.argv[2:] or [USER]
    out = {}
    for u in users:
        out[u] = [r.get("memory") for r in _items(mem.get_all(filters={"user_id": u}, top_k=1000))]
    print(json.dumps(out))


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "dump":
        dump()
    else:
        main()
