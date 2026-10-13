#!/usr/bin/env python3
"""Engram live inspector: chat with Claude Code and watch Engram store,
distill and recall, with every record, link and vector readable from the
Strata file and an SQL console over it.

Nothing here is simulated. Each chat turn is a real `claude -p` run in a
demo project with Engram's hooks installed, so the records are the ones the
hooks write, the distilled notes are the ones the distill model writes,
and the injected context is what the UserPromptSubmit hook returned. The
inspector reads the store through the project's Engram daemon (`engram
rpc`), the same process the hooks talk to.

    python3 demo/server.py                 # http://127.0.0.1:8765
    python3 demo/server.py --model <model> --distill-model <small model> --data ~/engram-demo

Standard library only.
"""

import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import threading
import time
import uuid
import webbrowser
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
TOOLS = "Bash,Edit,Write,Read,Glob,Grep,MultiEdit,TodoWrite,mcp__engram"
# Daemon ops the browser may call (all read-only).
RPC_OPS = {"inspect_query", "inspect_layout", "inspect_embed", "search", "timeline", "stats", "briefing"}
SEED_README = """# Demo project

A scratch project for the Engram live inspector. Claude Code works here;
Engram records every turn into its Strata store.
"""


class Demo:
    def __init__(self, args):
        self.args = args
        self.data = os.path.abspath(os.path.expanduser(args.data))
        self.project = os.path.join(self.data, "project")
        self.home = os.path.join(self.data, "engram")
        self.engram = os.path.abspath(args.engram)
        self.sessions_path = os.path.join(self.data, "sessions.json")
        self.sessions = json.load(open(self.sessions_path)) if os.path.exists(self.sessions_path) else []
        self.current = self.sessions[-1]["id"] if self.sessions else None
        self.turn_lock = threading.Lock()
        self.proc = None
        self.env = dict(os.environ, ENGRAM_HOME=self.home)
        self.env.pop("ENGRAM_DISABLE", None)
        self.env.pop("CLAUDECODE", None)
        if args.embedder:
            self.env["ENGRAM_EMBEDDER"] = args.embedder
        self.env["ENGRAM_MODELS"] = self.models_dir()
        if args.distill_model:
            self.env["ENGRAM_DISTILL_MODEL"] = args.distill_model
        elif args.model:
            self.env["ENGRAM_DISTILL_MODEL"] = args.model

    def models_dir(self):
        if self.args.models:
            return os.path.abspath(os.path.expanduser(self.args.models))
        if os.environ.get("ENGRAM_MODELS"):
            return os.environ["ENGRAM_MODELS"]
        shared = os.path.expanduser("~/.local/share/engram/models")
        return shared if os.path.isdir(os.path.join(shared, "potion-retrieval-32M")) else os.path.join(self.data, "models")

    def log(self, msg):
        print(f"[demo] {msg}", file=sys.stderr, flush=True)

    # ------------------------------------------------------------ setup

    def setup(self):
        if not os.path.exists(self.engram):
            sys.exit(f"{self.engram} not found: run `cargo build --release` in {ROOT}")
        if not shutil.which("claude"):
            sys.exit("the `claude` CLI is not on PATH")
        os.makedirs(self.data, exist_ok=True)
        spec = self.env.get("ENGRAM_EMBEDDER", "static")
        if spec.startswith("static") and ":" not in spec and not os.path.isdir(os.path.join(self.env["ENGRAM_MODELS"], "potion-retrieval-32M")):
            self.log(f"downloading the embedding model into {self.env['ENGRAM_MODELS']}")
            subprocess.run([self.engram, "fetch-model"], env=self.env, check=True)
        if not os.path.isdir(os.path.join(self.project, ".git")):
            os.makedirs(self.project, exist_ok=True)
            with open(os.path.join(self.project, "README.md"), "w") as f:
                f.write(SEED_README)
            for c in (["init", "-q"], ["add", "-A"], ["-c", "user.name=Engram Demo", "-c", "user.email=demo@example.com", "commit", "-qm", "Demo project"]):
                subprocess.run(["git"] + c, cwd=self.project, check=True)
        out = subprocess.run([self.engram, "install", "--project", self.project], env=self.env, capture_output=True, text=True, check=True).stdout
        self.store = next(line.split("store:", 1)[1].strip() for line in out.splitlines() if "store:" in line)
        self.events_log = os.path.join(self.store, "events.log")
        # A daemon left from an earlier run has that run's environment
        # (distill model, embedder); start a fresh one with ours.
        subprocess.run([self.engram, "stop", "--project", self.project], env=self.env, capture_output=True, timeout=30)
        time.sleep(0.5)
        stats = self.rpc({"op": "stats"})
        if "error" in stats:
            sys.exit(f"engram daemon: {stats['error']}")
        self.log(f"project {self.project}")
        self.log(f"store   {self.store} ({stats.get('records')} records, embedder {stats.get('embedder')})")

    # ------------------------------------------------------------ daemon

    def rpc(self, req, timeout=60):
        p = subprocess.run([self.engram, "rpc", json.dumps(req), "--timeout", str(timeout), "--project", self.project],
                           env=self.env, capture_output=True, text=True, timeout=timeout + 10)
        try:
            return json.loads(p.stdout)
        except json.JSONDecodeError:
            return {"error": (p.stderr or p.stdout or "no response").strip()[-500:]}

    def live(self, after_seq, log_offset):
        """Records written since `after_seq` and daemon events since byte
        `log_offset` of events.log."""
        r = self.rpc({"op": "inspect_query", "sql": "SELECT id, session, kind, tag, turn, seq, ts, importance, files, text, links FROM memory "
                      f"WHERE seq > {int(after_seq)} ORDER BY seq ASC LIMIT 500"})
        events = []
        size = os.path.getsize(self.events_log) if os.path.exists(self.events_log) else 0
        if log_offset > size:
            log_offset = 0
        if size > log_offset:
            with open(self.events_log, "rb") as f:
                f.seek(log_offset)
                chunk = f.read()
            end = chunk.rfind(b"\n") + 1
            for line in chunk[:end].splitlines():
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
            log_offset += end
        return {"records": r, "events": events, "log_offset": log_offset, "busy": self.turn_lock.locked()}

    # ------------------------------------------------------------ chat

    def remember_session(self, sid, first_prompt):
        if not any(s["id"] == sid for s in self.sessions):
            self.sessions.append({"id": sid, "started": time.time(), "title": first_prompt[:80]})
            with open(self.sessions_path, "w") as f:
                json.dump(self.sessions, f, indent=1)
        self.current = sid

    def chat(self, message, session, emit):
        """Runs one turn and calls `emit` with each event worth showing."""
        # Only the demo project's settings: user-level plugins and hooks would
        # add their own context (and failures) to every turn.
        cmd = ["claude", "-p", message, "--output-format", "stream-json", "--verbose", "--include-partial-messages",
               "--setting-sources", "project,local", "--allowedTools", TOOLS, "--disallowedTools", "WebFetch,WebSearch"]
        if self.args.model:
            cmd += ["--model", self.args.model]
        if self.args.budget:
            cmd += ["--max-budget-usd", str(self.args.budget)]
        sid = session or str(uuid.uuid4())
        cmd += ["--resume", sid] if session else ["--session-id", sid]
        lock = threading.Lock()

        def send(ev):
            with lock:
                emit(ev)

        # The stream carries SessionStart hook output but not
        # UserPromptSubmit's; the transcript records both, so recall is read
        # from there while the turn runs.
        tail = TranscriptTail(sid, send)
        t0 = time.time()
        send({"t": "init", "session": sid})
        self.proc = subprocess.Popen(cmd, cwd=self.project, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
        tail.start()
        try:
            for line in self.proc.stdout:
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                for out in simplify(ev):
                    if out["t"] == "init" and out.get("session"):
                        sid = out["session"]
                        tail.session = sid
                        self.remember_session(sid, message)
                    send(out)
            self.proc.wait()
            if self.proc.returncode:
                send({"t": "error", "text": self.proc.stderr.read()[-2000:] or f"claude exited with {self.proc.returncode}"})
        finally:
            self.proc = None
            tail.stop()
        send({"t": "done", "session": sid, "wall_s": time.time() - t0})


class TranscriptTail(threading.Thread):
    """Emits the context the UserPromptSubmit hook injected in this turn,
    read from the session's transcript (from its size when the turn began)."""

    def __init__(self, session, emit):
        super().__init__(daemon=True)
        self.session, self.emit = session, emit
        self.done = threading.Event()
        path = self.path()
        self.offset = os.path.getsize(path) if path else 0

    def path(self):
        paths = glob.glob(os.path.expanduser(f"~/.claude/projects/*/{self.session}.jsonl"))
        return paths[0] if paths else None

    def run(self):
        while True:
            last = self.done.is_set()
            self.scan()
            if last:
                return
            self.done.wait(0.3)

    def stop(self):
        self.done.set()
        self.join(timeout=3)

    def scan(self):
        path = self.path()
        if not path or os.path.getsize(path) <= self.offset:
            return
        with open(path, "rb") as f:
            f.seek(self.offset)
            chunk = f.read()
        end = chunk.rfind(b"\n") + 1
        self.offset += end
        for line in chunk[:end].splitlines():
            try:
                a = json.loads(line).get("attachment") or {}
            except (json.JSONDecodeError, AttributeError):
                continue
            if a.get("type") == "hook_additional_context" and a.get("hookEvent") == "UserPromptSubmit":
                c = a.get("content")
                text = "\n".join(c) if isinstance(c, list) else str(c or "")
                if text.strip():
                    self.emit({"t": "hook", "event": a.get("hookEvent"), "name": a.get("hookName"), "context": text, "exit_code": 0, "stderr": ""})


def simplify(ev):
    """The parts of a stream-json event the page shows."""
    t, st = ev.get("type"), ev.get("subtype")
    if t == "system" and st == "init":
        yield {"t": "init", "session": ev.get("session_id"), "model": ev.get("model"), "tools": [x for x in ev.get("tools", []) if x.startswith("mcp__engram")]}
    elif t == "system" and st == "hook_response":
        ctx = None
        try:
            o = json.loads(ev.get("output") or ev.get("stdout") or "null") or {}
            ctx = (o.get("hookSpecificOutput") or {}).get("additionalContext")
        except (json.JSONDecodeError, AttributeError):
            pass
        yield {"t": "hook", "event": ev.get("hook_event"), "name": ev.get("hook_name"), "context": ctx, "exit_code": ev.get("exit_code"), "stderr": (ev.get("stderr") or "")[-500:]}
    elif t == "stream_event":
        e = ev.get("event") or {}
        mid = ev.get("api_message_id")
        if e.get("type") == "content_block_delta" and (e.get("delta") or {}).get("type") == "text_delta":
            yield {"t": "delta", "mid": mid, "text": e["delta"]["text"]}
        elif e.get("type") == "content_block_start" and (e.get("content_block") or {}).get("type") == "tool_use":
            yield {"t": "tool_start", "mid": mid, "id": e["content_block"].get("id"), "name": e["content_block"].get("name")}
    elif t == "assistant":
        m = ev.get("message") or {}
        for c in m.get("content") or []:
            if c.get("type") == "text":
                yield {"t": "text", "mid": m.get("id"), "text": c.get("text", "")}
            elif c.get("type") == "tool_use":
                yield {"t": "tool_use", "mid": m.get("id"), "id": c.get("id"), "name": c.get("name"), "input": c.get("input")}
    elif t == "user":
        for c in (ev.get("message") or {}).get("content") or []:
            if isinstance(c, dict) and c.get("type") == "tool_result":
                body = c.get("content")
                if isinstance(body, list):
                    body = "\n".join(x.get("text", "") for x in body if isinstance(x, dict))
                body = body or ""
                yield {"t": "tool_result", "id": c.get("tool_use_id"), "is_error": bool(c.get("is_error")), "text": body[:4000], "chars": len(body)}
    elif t == "result":
        u = ev.get("usage") or {}
        yield {"t": "result", "is_error": ev.get("is_error"), "cost_usd": ev.get("total_cost_usd"), "turns": ev.get("num_turns"), "ms": ev.get("duration_ms"),
               "input_tokens": u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0), "output_tokens": u.get("output_tokens", 0)}


class Handler(BaseHTTPRequestHandler):
    demo: Demo = None
    protocol_version = "HTTP/1.0"

    def log_message(self, fmt, *a):
        if self.demo.args.verbose:
            super().log_message(fmt, *a)

    def send_json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"{}")

    def do_GET(self):
        u = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        d = self.demo
        if u.path in ("/", "/index.html"):
            body = open(os.path.join(HERE, "index.html"), "rb").read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        elif u.path == "/api/info":
            self.send_json({"project": d.project, "store": d.store, "model": d.args.model, "distill_model": d.env.get("ENGRAM_DISTILL_MODEL"),
                            "sessions": d.sessions, "current": d.current, "busy": d.turn_lock.locked(), "stats": d.rpc({"op": "stats"})})
        elif u.path == "/api/live":
            self.send_json(d.live(int(q.get("seq", 0)), int(q.get("log", 0))))
        else:
            self.send_json({"error": "not found"}, 404)

    def do_POST(self):
        u = urlparse(self.path)
        d = self.demo
        if u.path == "/api/rpc":
            req = self.body()
            if req.get("op") not in RPC_OPS:
                return self.send_json({"error": f"op {req.get('op')!r} is not allowed here"}, 400)
            return self.send_json(d.rpc(req))
        if u.path == "/api/session":
            req = self.body()
            d.current = req.get("session") or None
            return self.send_json({"current": d.current})
        if u.path == "/api/stop":
            if d.proc:
                d.proc.terminate()
            return self.send_json({"ok": True})
        if u.path == "/api/chat":
            req = self.body()
            msg = (req.get("message") or "").strip()
            if not msg:
                return self.send_json({"error": "empty message"}, 400)
            if not d.turn_lock.acquire(blocking=False):
                return self.send_json({"error": "a turn is already running"}, 409)
            try:
                self.send_response(200)
                self.send_header("Content-Type", "application/x-ndjson")
                self.send_header("Cache-Control", "no-store")
                self.end_headers()

                def emit(ev):
                    try:
                        self.wfile.write((json.dumps(ev) + "\n").encode())
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        pass

                # "session": null starts a new Claude session; no key continues the current one.
                session = req["session"] if "session" in req else d.current
                d.chat(msg, session, emit)
            finally:
                d.turn_lock.release()
            return
        self.send_json({"error": "not found"}, 404)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--data", default="~/engram-demo", help="demo project and its Engram store (default ~/engram-demo)")
    ap.add_argument("--model", default=os.environ.get("ENGRAM_DEMO_MODEL"), help="model for the chat (default: the claude CLI's)")
    ap.add_argument("--distill-model", default=os.environ.get("ENGRAM_DISTILL_MODEL"), help="model Engram distills turns with (default: --model)")
    ap.add_argument("--embedder", default=None, help="Engram embedder spec (default static; hash:256 needs no model files)")
    ap.add_argument("--models", default=None, help="embedding model directory (default $ENGRAM_MODELS or ~/.local/share/engram/models)")
    ap.add_argument("--engram", default=os.path.join(ROOT, "target", "release", "engram"))
    ap.add_argument("--budget", type=float, default=2.0, help="max USD per chat turn")
    ap.add_argument("--no-open", action="store_true", help="do not open a browser")
    ap.add_argument("--verbose", action="store_true")
    args = ap.parse_args()
    demo = Demo(args)
    demo.setup()
    Handler.demo = demo
    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    url = f"http://{args.host}:{args.port}/"
    demo.log(f"open {url}")
    if not args.no_open:
        threading.Timer(0.8, lambda: webbrowser.open(url)).start()
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
