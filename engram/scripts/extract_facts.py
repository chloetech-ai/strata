#!/usr/bin/env python3
"""Structured keys written by a model: one call per conversation session
extracts self-contained facts, each tied to the turn it comes from.

  python3 scripts/extract_facts.py --data /home/user/data/memory/longmemeval_s.jsonl \
      --every 1 --model <model> --out /home/user/data/memory/facts_entities.jsonl

The output has one line per session: {"session_id", "hash", "facts": [{"turn",
"fact", "entities"}], "cost_usd", "input_chars"}; `engram-structure --facts`
reads it. Entities are the people, places, things and topics a fact is about;
Engram links facts that share one. Sessions already in the output are
skipped, so a run can resume.
"""

import argparse
import concurrent.futures as cf
import hashlib
import json
import os
import subprocess
import sys
import threading
import time

SYSTEM = "You extract facts from conversations for a long-term memory. Output JSON lines only."

PROMPT = """Below is one conversation between {who}, held on {date}. Turns are numbered.

{turns}

List the facts from this conversation that are worth remembering later:
- about {subject}: personal details, preferences, plans, decisions, events and activities (with their dates, counts, names, places, prices);
- specific information {other} gave that might be asked about again (names, recommendations, figures, steps).

Write each fact as a self-contained sentence that names its subject (say "{subject_name}", not "I"), and resolve relative dates against the conversation date when you can. Skip greetings and generic advice.

For each fact, also list the entities it is about, so that facts about the same thing can be found together later:
- the specific named things: people, pets, places, organizations, products, titles, events (short names, e.g. "Luna", "Honda Civic", "Lisbon");
- one or two general topics as plain lowercase nouns (e.g. "car", "houseplants", "running", "budget", "job").
Do not list {skip} as entities.

Output one JSON object per line, nothing else:
{{"turn": <number of the turn it comes from>, "fact": "<sentence>", "entities": ["<entity>", ...]}}"""

MAX_TURN_CHARS = 2500


def session_text(turns):
    out = []
    for i, t in enumerate(turns):
        text = t["text"]
        if len(text) > MAX_TURN_CHARS:
            text = text[:MAX_TURN_CHARS] + " …"
        out.append(f"[{i}] {t['role']}: {text}")
    return "\n\n".join(out)


def prompt_for(turns, text):
    roles = list(dict.fromkeys(t["role"] for t in turns))
    date = turns[0].get("timestamp") or "an unknown date"
    if set(roles) <= {"user", "assistant"}:
        return PROMPT.format(who="a user and an AI assistant", date=date, turns=text, subject="the user", other="the assistant", subject_name="The user", skip="the user or the assistant")
    who = " and ".join(roles[:2]) if len(roles) <= 2 else ", ".join(roles)
    return PROMPT.format(who=who, date=date, turns=text, subject="each speaker", other="a speaker", subject_name=roles[0], skip="the speakers themselves")


def call(prompt, model, retries=4):
    cmd = ["claude", "-p", "--model", model, "--system-prompt", SYSTEM, "--tools", "", "--no-session-persistence", "--output-format", "json"]
    last = None
    for attempt in range(retries):
        try:
            p = subprocess.run(cmd, input=prompt, capture_output=True, text=True, timeout=600, cwd=os.environ.get("READER_CWD", "/tmp"))
            d = json.loads(p.stdout)
            if d.get("is_error") or d.get("result") is None:
                raise RuntimeError(str(d.get("result"))[:200])
            return d["result"], d.get("total_cost_usd", 0.0)
        except Exception as e:  # noqa: BLE001 - retried, then reported
            last = e
            time.sleep(2 * 2**attempt)
    raise RuntimeError(f"extraction failed: {last}")


def parse(text, n_turns):
    facts = []
    for line in text.splitlines():
        line = line.strip().strip(",")
        if not line.startswith("{"):
            continue
        try:
            o = json.loads(line)
            t, f = int(o["turn"]), str(o["fact"]).strip()
        except Exception:  # noqa: BLE001 - a malformed line is dropped
            continue
        ents = o.get("entities") if isinstance(o.get("entities"), list) else []
        ents = [str(e).strip()[:60] for e in ents if str(e).strip()][:6]
        if f and 0 <= t < n_turns:
            facts.append({"turn": t, "fact": f, "entities": ents})
    return facts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--every", type=int, default=5)
    ap.add_argument("--out", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--workers", type=int, default=12)
    args = ap.parse_args()

    sessions = {}
    with open(args.data) as f:
        for i, line in enumerate(f):
            if i % args.every:
                continue
            u = json.loads(line)
            by = {}
            for t in u["turns"]:
                by.setdefault(t["session_id"], []).append(t)
            for sid, ts in by.items():
                h = hashlib.sha1(json.dumps([[t["role"], t["text"]] for t in ts]).encode()).hexdigest()[:16]
                sessions[(sid, h)] = ts
    done = set()
    if os.path.exists(args.out):
        with open(args.out) as f:
            for line in f:
                r = json.loads(line)
                done.add((r["session_id"], r["hash"]))
    todo = [k for k in sessions if k not in done]
    print(f"{len(sessions)} sessions, {len(todo)} to extract", file=sys.stderr)
    lock = threading.Lock()

    def work(key):
        sid, h = key
        ts = sessions[key]
        text = session_text(ts)
        out, cost = call(prompt_for(ts, text), args.model)
        r = {"session_id": sid, "hash": h, "facts": parse(out, len(ts)), "cost_usd": cost, "input_chars": len(text)}
        with lock:
            with open(args.out, "a") as f:
                f.write(json.dumps(r) + "\n")
        return r

    t0 = time.time()
    cost = 0.0
    n_facts = 0
    with cf.ThreadPoolExecutor(args.workers) as ex:
        futs = {ex.submit(work, k): k for k in todo}
        for i, fut in enumerate(cf.as_completed(futs), 1):
            try:
                r = fut.result()
                cost += r["cost_usd"]
                n_facts += len(r["facts"])
            except Exception as e:  # noqa: BLE001
                print(f"failed {futs[fut]}: {e}", file=sys.stderr)
            if i % 100 == 0:
                print(f"  {i}/{len(todo)} in {time.time() - t0:.0f} s, ${cost:.2f}, {n_facts / i:.1f} facts per session", file=sys.stderr)
    print(f"done: ${cost:.2f}, {n_facts} facts", file=sys.stderr)


if __name__ == "__main__":
    main()
