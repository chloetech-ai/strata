#!/usr/bin/env python3
"""Installs claude-mem (github.com/thedotmack/claude-mem) into one isolated HOME.

Used by scripts/memory_bench.py for the 'claude-mem' lane, and runnable by
hand:

  python3 eval/competitors/claude_mem/setup.py HOME [--version 13.39.0] [--port N]

What it does, in order (see README.md next to this file for why):
1. downloads the npm package claude-mem@VERSION once into the shared
   directory (default /home/claude/bench/shared/claude-mem);
2. runs its own installer, `npx claude-mem install --ide claude-code
   --provider claude --no-auto-start`, with HOME set to the lane's home (the
   plugin, its hook launcher ~/.local/bin/claude-mem and ~/.claude-mem end up
   there);
3. makes the plugin's Stop hook synchronous: Claude Code cancels async Stop
   hooks when a headless `claude -p` exits, so without this the session
   summary (claude-mem's main memory artifact) is never written;
4. settings (~/.claude-mem/settings.json): every model claude-mem calls is
   `haiku`, a worker port of its own, telemetry off, and CLAUDE_CODE_PATH
   pointing at a small wrapper that runs the real `claude` and appends each
   result message (with its cost) to ~/.claude-mem-bench/observer-results.jsonl;
5. provisions Chroma (claude-mem's vector store) for offline use: the
   chroma-mcp uv environment is built into a shared uv cache, and Chroma's
   ONNX embedding model is copied into ~/.cache/chroma. The lane then runs
   with UV_CACHE_DIR=<shared cache> and UV_OFFLINE=1 (returned in "env").

Prints (and `install` returns) a JSON object with the version, port and the
extra environment the lane needs.
"""

import argparse
import glob
import hashlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time

VERSION = "13.39.0"
SHARED = "/home/claude/bench/shared"
CHROMA_ONNX_URL = "https://chroma-onnx-models.s3.amazonaws.com/all-MiniLM-L6-v2/onnx.tar.gz"
CHROMA_ONNX_SHA256 = "913d7300ceae3b2dbc2c50d1de4baacab4be7b9380491c27fab7418616a16ec3"
# Variables that let installers reach the network through this container's
# proxy. They are used for installing only, never in a benchmark run.
NET_VARS = ("HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy", "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE")

WRAPPER = """#!/bin/bash
# claude-mem's observer runs Claude Code through the Agent SDK. This wrapper
# runs the real CLI unchanged and keeps a copy of each result message (they
# carry the cost) so the benchmark can count what claude-mem spent.
exec "{real}" "$@" > >(tee >(grep --line-buffered '"type":"result"' >> "{log}"))
"""


def _run(cmd, env, timeout=900, cwd=None):
    p = subprocess.run(cmd, env=env, cwd=cwd, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd[:4])}… failed ({p.returncode}): {(p.stdout + p.stderr)[-1500:]}")
    return p.stdout


def net_env(base):
    env = dict(base)
    env.update({k: os.environ[k] for k in NET_VARS if k in os.environ})
    return env


def fetch(version=VERSION, shared=SHARED):
    """The unpacked npm package (downloaded once)."""
    d = os.path.join(shared, "claude-mem", version)
    pkg = os.path.join(d, "package")
    if not os.path.exists(os.path.join(pkg, "dist", "npx-cli", "index.js")):
        os.makedirs(d, exist_ok=True)
        _run(["npm", "pack", f"claude-mem@{version}", "--pack-destination", d], net_env(os.environ), cwd=d)
        _run(["tar", "xzf", os.path.join(d, f"claude-mem-{version}.tgz"), "-C", d], os.environ)
    return pkg


def chroma_pins(pkg):
    """The uvx arguments the worker uses to start chroma-mcp (read from the bundle)."""
    src = open(os.path.join(pkg, "plugin", "scripts", "worker-service.cjs"), encoding="utf-8", errors="replace").read()
    withs = re.search(r'\[("onnxruntime[^\]]*)\]', src)
    mcp = re.search(r'="chroma-mcp",.{0,300}?="(\d+\.\d+\.\d+)",', src)
    if not withs or not mcp:
        raise RuntimeError("could not find the chroma-mcp pins in worker-service.cjs")
    args = ["--python", "3.13"]
    for pin in json.loads("[" + withs.group(1) + "]"):
        args += ["--with", pin]
    return args + ["--from", f"chroma-mcp=={mcp.group(1)}", "chroma-mcp"]


def chroma_model(shared=SHARED):
    """Chroma's default ONNX embedding model (all-MiniLM-L6-v2), downloaded once."""
    d = os.path.join(shared, "chroma-onnx", "all-MiniLM-L6-v2")
    tgz = os.path.join(d, "onnx.tar.gz")
    if not os.path.exists(os.path.join(d, "onnx", "model.onnx")):
        os.makedirs(d, exist_ok=True)
        _run(["curl", "-sSfL", "-o", tgz, CHROMA_ONNX_URL], net_env(os.environ))
        if hashlib.sha256(open(tgz, "rb").read()).hexdigest() != CHROMA_ONNX_SHA256:
            raise RuntimeError("chroma ONNX model checksum mismatch")
        _run(["tar", "xzf", tgz, "-C", d], os.environ)
    return d


def free_port(seed):
    """A free local port, stable for a given seed when possible."""
    base = 20000 + int(hashlib.sha256(seed.encode()).hexdigest(), 16) % 20000
    for port in list(range(base, base + 200, 2)):
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", port))
            except OSError:
                continue
        with socket.socket() as s:
            try:
                s.bind(("127.0.0.1", port + 1))
                return port
            except OSError:
                continue
    raise RuntimeError("no free port")


def install(home, version=VERSION, model="haiku", port=None, shared=SHARED, path_dirs=(), real_claude=None, anthropic_base_url=None):
    """Installs claude-mem into `home`; returns {"version", "port", "env", "plugin_dir", ...}."""
    pkg = fetch(version, shared)
    os.makedirs(home, exist_ok=True)
    path = os.pathsep.join([os.path.join(home, ".local", "bin"), *path_dirs, "/usr/local/bin", "/usr/bin", "/bin"])
    base = {"PATH": path, "HOME": home, "CLAUDE_MEM_ONLINE_OPTIN": "false", "CI": "1", "DO_NOT_TRACK": "1"}
    if anthropic_base_url or os.environ.get("ANTHROPIC_BASE_URL"):
        base["ANTHROPIC_BASE_URL"] = anthropic_base_url or os.environ["ANTHROPIC_BASE_URL"]
    log = _run(["node", os.path.join(pkg, "dist", "npx-cli", "index.js"), "install", "--ide", "claude-code", "--provider", "claude", "--no-auto-start"], net_env(base))
    launcher = os.path.join(home, ".local", "bin", "claude-mem")
    if not os.path.exists(launcher):
        raise RuntimeError("claude-mem launcher missing after install:\n" + log[-1500:])

    # 3. Synchronous Stop hook (see the docstring).
    plugin_dirs = sorted(glob.glob(os.path.join(home, ".claude", "plugins", "cache", "thedotmack", "claude-mem", "*")))
    for d in plugin_dirs:
        hp = os.path.join(d, "hooks", "hooks.json")
        hooks = json.load(open(hp))
        for group in hooks["hooks"].get("Stop", []):
            for h in group.get("hooks", []):
                h.pop("async", None)
        json.dump(hooks, open(hp, "w"), indent=2)

    # 4. Settings, cost wrapper and telemetry.
    real_claude = real_claude or os.path.realpath(shutil.which("claude", path=os.pathsep.join(path_dirs) or None) or shutil.which("claude"))
    bench_dir = os.path.join(home, ".claude-mem-bench")
    os.makedirs(bench_dir, exist_ok=True)
    wrapper = os.path.join(bench_dir, "claude")
    with open(wrapper, "w") as f:
        f.write(WRAPPER.format(real=real_claude, log=os.path.join(bench_dir, "observer-results.jsonl")))
    os.chmod(wrapper, 0o755)
    port = port or free_port(home)
    sp = os.path.join(home, ".claude-mem", "settings.json")
    settings = json.load(open(sp)) if os.path.exists(sp) else {}
    settings.update({
        "CLAUDE_MEM_MODEL": model, "CLAUDE_MEM_TIER_SIMPLE_MODEL": model, "CLAUDE_MEM_TIER_FAST_MODEL": model,
        "CLAUDE_MEM_TIER_SMART_MODEL": model, "CLAUDE_MEM_TIER_SUMMARY_MODEL": model,
        "CLAUDE_MEM_WORKER_PORT": str(port), "CLAUDE_MEM_CHROMA_PORT": str(port + 1), "CLAUDE_CODE_PATH": wrapper,
    })
    json.dump(settings, open(sp, "w"), indent=2)
    tp = os.path.join(home, ".claude-mem", "telemetry.json")
    tel = json.load(open(tp)) if os.path.exists(tp) else {}
    tel.update(enabled=False, decidedAt=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
    json.dump(tel, open(tp, "w"))

    # 5. Chroma, offline.
    uv_cache = os.path.join(shared, "uv-cache")
    _run(["uvx", *chroma_pins(pkg), "--help"], net_env(dict(base, UV_CACHE_DIR=uv_cache, UV_NATIVE_TLS="1")), timeout=900)
    _run(["uvx", *chroma_pins(pkg), "--help"], dict(base, UV_CACHE_DIR=uv_cache, UV_OFFLINE="1"), timeout=300)
    dst = os.path.join(home, ".cache", "chroma", "onnx_models", "all-MiniLM-L6-v2")
    if not os.path.exists(os.path.join(dst, "onnx", "model.onnx")):
        shutil.copytree(chroma_model(shared), dst, dirs_exist_ok=True)

    return {
        "version": version, "port": port, "model": model, "plugin_dirs": plugin_dirs, "wrapper": wrapper, "real_claude": real_claude,
        "env": {"UV_CACHE_DIR": uv_cache, "UV_OFFLINE": "1"},
        "observer_log": os.path.join(bench_dir, "observer-results.jsonl"),
    }


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("home")
    ap.add_argument("--version", default=VERSION)
    ap.add_argument("--model", default="haiku")
    ap.add_argument("--port", type=int, default=None)
    ap.add_argument("--shared", default=SHARED)
    a = ap.parse_args()
    bins = [os.path.dirname(shutil.which(t)) for t in ("claude", "node", "bun", "uvx") if shutil.which(t)]
    print(json.dumps(install(os.path.abspath(a.home), a.version, a.model, a.port, a.shared, path_dirs=list(dict.fromkeys(bins))), indent=1))
    sys.exit(0)
