#!/usr/bin/env python3
"""Sets up the mem0 lane for one isolated HOME (used by scripts/memory_bench.py).

- a shared virtualenv (<shared>/mem0-venv) with mem0ai, mcp, onnxruntime,
  tokenizers and numpy, built once with uv;
- Chroma's ONNX export of all-MiniLM-L6-v2 as the embedding model (shared,
  downloaded once by ../claude_mem/setup.py's chroma_model());
- HOME/.mem0-bench/mcp.json, the MCP config the lane passes to Claude Code
  with --mcp-config, which runs mem0_mcp.py with the venv's Python.

  python3 eval/competitors/mem0/setup.py HOME
"""

import importlib.util
import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SHARED = "/home/claude/bench/shared"
PACKAGES = ["mem0ai==2.2.1", "mcp<2", "onnxruntime", "tokenizers", "numpy"]
SYSTEM_PROMPT = (
    "You have a long-term memory for this project, kept between sessions by mem0: the tools mcp__mem0__search_memory, "
    "mcp__mem0__add_memory and mcp__mem0__list_memories. Before you start on a request, search the memory for anything "
    "relevant to it (requirements, rules, decisions, conventions, constraints, earlier plans). When the user states something "
    "that will matter in later sessions (a requirement, rule, decision and its reason, preference, constraint or fact about "
    "the project or its environment), save it with add_memory as a short self-contained statement."
)


def _claude_mem_setup():
    spec = importlib.util.spec_from_file_location("claude_mem_setup", os.path.join(HERE, "..", "claude_mem", "setup.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def venv(shared=SHARED):
    v = os.path.join(shared, "mem0-venv")
    py = os.path.join(v, "bin", "python")
    # mem0 writes ~/.mem0/config.json on import: keep it out of the caller's home.
    check_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": shared, "MEM0_TELEMETRY": "False"}
    if not os.path.exists(py) or subprocess.run([py, "-c", "import mem0, onnxruntime, tokenizers; from mcp.server.fastmcp import FastMCP"], env=check_env, capture_output=True).returncode:
        cm = _claude_mem_setup()
        env = cm.net_env(dict(os.environ, UV_NATIVE_TLS="1"))
        subprocess.run(["uv", "venv", "-q", "--python", sys.executable, v], env=env, check=True)
        subprocess.run(["uv", "pip", "install", "-q", *PACKAGES], env=dict(env, VIRTUAL_ENV=v), check=True)
    return py


def install(home, shared=SHARED, model="haiku", bench_bin=None):
    py = venv(shared)
    onnx = _claude_mem_setup().chroma_model(shared)
    state = os.path.join(home, ".mem0-bench")
    os.makedirs(state, exist_ok=True)
    claude = os.path.join(bench_bin, "claude") if bench_bin else shutil.which("claude")
    cfg = {"mcpServers": {"mem0": {"type": "stdio", "command": py, "args": [os.path.join(HERE, "mem0_mcp.py")],
                                   "env": {"MEM0_BENCH_DIR": state, "MEM0_BENCH_ONNX": onnx, "MEM0_BENCH_MODEL": model,
                                           "MEM0_BENCH_CLAUDE": claude, "MEM0_BENCH_USER": "project", "MEM0_TELEMETRY": "False"}}}}
    path = os.path.join(state, "mcp.json")
    json.dump(cfg, open(path, "w"), indent=1)
    version = subprocess.run([py, "-c", "import importlib.metadata as m; print(m.version('mem0ai'))"], env={"PATH": "/usr/bin:/bin", "HOME": home}, capture_output=True, text=True).stdout.strip()
    return {"version": version, "python": py, "onnx": onnx, "state": state, "mcp_config": path, "system_prompt": SYSTEM_PROMPT,
            "llm_log": os.path.join(state, "llm-results.jsonl"), "env": {"MEM0_TELEMETRY": "False"}, "model": model}


def dump(home, info, user=None):
    """Every memory mem0 holds for the project (run after the history)."""
    env = {"PATH": "/usr/bin:/bin", "HOME": home, "MEM0_BENCH_DIR": info["state"], "MEM0_BENCH_ONNX": info["onnx"], "MEM0_BENCH_USER": "project", "MEM0_TELEMETRY": "False"}
    args = [info["python"], os.path.join(HERE, "mem0_mcp.py"), "dump"] + ([user] if user else [])
    p = subprocess.run(args, env=env, cwd=home, capture_output=True, text=True, timeout=300)
    try:
        return json.loads(p.stdout.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        return {"error": (p.stdout + p.stderr)[-800:]}


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    print(json.dumps(install(os.path.abspath(sys.argv[1])), indent=1))
