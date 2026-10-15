#!/usr/bin/env python3
"""Exports Claude Code session transcripts as readable markdown.

The 'transcripts' lane of scripts/memory_bench.py gives the agent the raw
history of earlier sessions (no summarising, no index) and lets it search
them with its own tools. Each session becomes one file with the user's
prompts, the assistant's text, every tool call (name and the important part
of its input) and a short excerpt of each tool result.

  python3 eval/competitors/transcripts/export_transcripts.py OUT_DIR TRANSCRIPT.jsonl [...]

Standard library only.
"""

import json
import os
import sys
from datetime import datetime, timezone

RESULT_CHARS = 600
INPUT_CHARS = 1200


def _clip(text, n):
    text = text if isinstance(text, str) else json.dumps(text, ensure_ascii=False)
    return text if len(text) <= n else text[:n] + f" […{len(text) - n} more chars]"


def _tool_input(name, inp):
    """The part of a tool call's input worth reading later."""
    if not isinstance(inp, dict):
        return _clip(inp, INPUT_CHARS)
    if name == "Bash":
        return "$ " + _clip(inp.get("command", ""), INPUT_CHARS)
    if name in ("Read", "NotebookRead"):
        return inp.get("file_path", "")
    if name == "Write":
        return f"{inp.get('file_path', '')}\n" + _clip(inp.get("content", ""), INPUT_CHARS)
    if name == "Edit":
        return f"{inp.get('file_path', '')}\n--- old\n{_clip(inp.get('old_string', ''), INPUT_CHARS // 2)}\n+++ new\n{_clip(inp.get('new_string', ''), INPUT_CHARS // 2)}"
    if name == "MultiEdit":
        parts = [inp.get("file_path", "")]
        for e in inp.get("edits", [])[:6]:
            parts.append(f"--- old\n{_clip(e.get('old_string', ''), 300)}\n+++ new\n{_clip(e.get('new_string', ''), 300)}")
        return "\n".join(parts)
    if name in ("Grep", "Glob"):
        return " ".join(f"{k}={v}" for k, v in inp.items())
    if name == "TodoWrite":
        return "\n".join(f"- [{t.get('status', '')}] {t.get('content', '')}" for t in inp.get("todos", []))
    return _clip(inp, INPUT_CHARS)


def _result_text(content):
    if isinstance(content, list):
        content = "\n".join(c.get("text", "") for c in content if isinstance(c, dict) and c.get("type") == "text")
    return content if isinstance(content, str) else json.dumps(content, ensure_ascii=False)


def render(path, title):
    """Markdown for one transcript file."""
    lines, started = [], None
    for raw in open(path, encoding="utf-8"):
        try:
            ev = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if ev.get("type") not in ("user", "assistant") or ev.get("isMeta") or ev.get("isCompactSummary") or ev.get("isSidechain"):
            continue
        started = started or ev.get("timestamp")
        content = (ev.get("message") or {}).get("content")
        if ev["type"] == "user":
            if isinstance(content, str):
                if content.startswith("<") and ("command-name>" in content[:200] or "local-command" in content[:200]):
                    continue
                lines += ["", "## User", "", content.strip()]
            elif isinstance(content, list):
                for c in content:
                    if c.get("type") == "text" and not c.get("text", "").startswith("<system-reminder>"):
                        lines += ["", "## User", "", c["text"].strip()]
                    elif c.get("type") == "tool_result":
                        text = _result_text(c.get("content")).strip()
                        if text:
                            tag = "error" if c.get("is_error") else "result"
                            lines += [f"> {tag}: " + _clip(text, RESULT_CHARS).replace("\n", "\n> ")]
        else:
            for c in content if isinstance(content, list) else []:
                if c.get("type") == "text" and c.get("text", "").strip():
                    lines += ["", "**Assistant:** " + c["text"].strip()]
                elif c.get("type") == "tool_use":
                    lines += ["", f"Tool call `{c.get('name')}`:", "```", _tool_input(c.get("name"), c.get("input")), "```"]
    when = ""
    if started:
        try:
            when = datetime.fromisoformat(started.replace("Z", "+00:00")).astimezone(timezone.utc).strftime(" (%Y-%m-%d %H:%M UTC)")
        except ValueError:
            pass
    return f"# {title}{when}\n" + "\n".join(lines) + "\n"


def export(transcripts, out_dir, titles=None):
    """Writes one markdown file per transcript into out_dir; returns their paths."""
    os.makedirs(out_dir, exist_ok=True)
    out = []
    for k, path in enumerate(transcripts, 1):
        title = (titles or [])[k - 1] if titles and k <= len(titles) else f"Work session {k}"
        dst = os.path.join(out_dir, f"session-{k:02d}.md")
        with open(dst, "w", encoding="utf-8") as f:
            f.write(render(path, title))
        out.append(dst)
    return out


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    for p in export(sys.argv[2:], sys.argv[1]):
        print(p)
