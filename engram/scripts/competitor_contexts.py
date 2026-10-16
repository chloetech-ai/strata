#!/usr/bin/env python3
"""Reader contexts from other memory systems, for scripts/reader_eval.py and
scripts/bench_eval.py.

Each history (one LongMemEval question's sessions, one BEAM conversation) goes
into the system on its own; then each of its questions is asked, and what the
system returns is written as {"qid", "context", ...} lines that the two reader
scripts read like Engram's contexts:

  python3 scripts/competitor_contexts.py mem0 --data longmemeval_s.jsonl --every 5 \\
      --work /home/claude/bench/competitors/longmemeval_s --out results/v3/competitors/longmemeval_s/mem0.contexts.jsonl
  python3 scripts/competitor_contexts.py hindsight --url http://127.0.0.1:8888 --data beam_100k.jsonl ...

mem0: the open-source library with eval/competitors/mem0/mem0_mcp.py's model
and embedder (Haiku through `claude -p`; all-MiniLM-L6-v2 as ONNX) and a local
Qdrant per history. One add() per session (LongMemEval) or per BEAM message
group, with the session's date in the metadata and, through add()'s custom
instructions, as the observation date: mem0's open-source add() takes no
timestamp, and its extraction prompt would otherwise date everything on the
day of ingestion. search() returns the top 20, as AMB (vectorize-io's
agent-memory-benchmark) runs mem0; each memory is listed with its session's
date.

mem0-hybrid: the same mem0 stores with mem0's optional search extras
(mem0ai[nlp] and [extras]: spaCy lemmas and entities, fastembed BM25), which
turn on its keyword scoring and entity boosts. Each history is replayed into a
fresh store through mem0's own add(), session by session in the original
order, with the model call replaced by the memories that session produced in
the mem0 run: mem0's add() looks up existing memories by vector search alone,
so the extraction step sees the same input with or without the extras, and
only the steps after it (lemmas, BM25 vectors, entity links) differ. Needs a
virtualenv with the extras and MEM0_BENCH_BM25 (fastembed's Qdrant/bm25 files).

hindsight: a hindsight-api server. One bank per history, created and filled
as AMB's Hindsight provider does it (src/memory_bench/memory/hindsight.py at
aef3702): observations off, AMB's retain mission for BEAM, one document per
session (BEAM: per batch, split at 100,000 characters, formatted as AMB's
_format_chat) with the session's date as timestamp and AMB's context line;
recall with budget "high", max_tokens 32768 (LongMemEval) or 12288 (BEAM),
source chunks included (16384 or 8192 tokens), the question's date as
query_timestamp; results deduplicated and formatted with AMB's
_format_results. --recall-max-tokens and --chunk-tokens change the recall
budget (a second, smaller context from the same bank).

Histories run in a fixed shuffled order (seed 7), so a run cut short still
covers every question type. Finished histories are skipped on a rerun; mem0
also resumes inside a history from its last stored session. A model call that
hits the account's usage limit waits for the limit to reset and is retried.
"""

import argparse
import concurrent.futures as cf
import datetime as dt
import json
import os
import random
import re
import sys
import time
from collections import OrderedDict

HERE = os.path.dirname(os.path.abspath(__file__))
MEM0_DIR = os.path.join(HERE, "..", "eval", "competitors", "mem0")
LIMIT_RE = re.compile(r"hit your .{0,20}limit|usage limit|rate.?limit", re.I)

MEM0_OBSERVATION = (
    "The New Messages were exchanged on {date}. That is their Observation Date: resolve relative "
    "times against it. The Observation Date section above shows the day of ingestion instead."
)

# AMB, src/memory_bench/memory/hindsight.py (aef3702), _BEAM_RETAIN_MISSION.
BEAM_RETAIN_MISSION = (
    "This is a conversation between a user and an AI assistant about the user's work. Extract what "
    "the user says about themselves, their project, decisions, problems, results and plans — "
    "including negative statements ('I have never done X') and statements that contradict earlier "
    "ones, each as its own fact. Settings, versions and numbers the user states they use, chose, "
    "changed or measured are the most important facts: keep them exactly. ALSO extract what the "
    "assistant proposed, explained, designed or built in reply, at the level of the approach: which "
    "technique, component, tool or design it recommended and for what (e.g. 'Assistant suggested "
    "batching the import job and adding an index on created_at'). Do NOT record example values from "
    "the assistant's code or illustrations (sample batch sizes, TTLs, ports, thresholds) — those are "
    "not the user's settings. Do not restate the user profile given as context."
)


def log(msg):
    print(f"{dt.datetime.now(dt.timezone.utc):%H:%M:%S} {msg}", flush=True)


def parse_date(s):
    """'2023/05/20 (Sat) 02:21' -> datetime (UTC)."""
    if not s:
        return None
    m = re.match(r"(\d{4})/(\d{2})/(\d{2})(?:\s*\(\w+\))?\s*(\d{2}):(\d{2})", s)
    if not m:
        return None
    y, mo, d, h, mi = map(int, m.groups())
    return dt.datetime(y, mo, d, h, mi, tzinfo=dt.timezone.utc)


def sessions(unit, by="session"):
    """[(session_id, [turns])] in history order. by="batch" merges BEAM's message groups (b3g0, b3g1, ...) into their batch (b3)."""
    out = OrderedDict()
    for t in unit["turns"]:
        sid = t["session_id"]
        if by == "batch":
            sid = re.sub(r"g\d+$", "", sid)
        out.setdefault(sid, []).append(t)
    return list(out.items())


def is_beam(unit):
    return unit["dataset"].startswith("beam")


# ---------------------------------------------------------------- mem0

def mem0_unit(unit, questions, work, model):
    sys.path.insert(0, os.path.abspath(MEM0_DIR))
    import mem0_mcp as M  # noqa: E402 - needs MEM0_BENCH_DIR / MEM0_BENCH_ONNX set first
    from mem0 import Memory

    M.LlmFactory.provider_to_class["openai"] = ("mem0_mcp.ClaudeCliLLM", M.BaseLlmConfig)
    M.EmbedderFactory.provider_to_class["openai"] = "mem0_mcp.OnnxMiniLM"
    d = os.path.join(work, "mem0", unit["unit_id"])
    os.makedirs(d, exist_ok=True)
    mem = Memory.from_config({
        "llm": {"provider": "openai", "config": {"model": model}},
        "embedder": {"provider": "openai", "config": {"model": "all-MiniLM-L6-v2", "embedding_dims": 384}},
        "vector_store": {"provider": "qdrant", "config": {"collection_name": "memories", "path": os.path.join(d, "qdrant"), "embedding_model_dims": 384, "on_disk": True}},
        "history_db_path": os.path.join(d, "history.db"),
    })
    progress = os.path.join(d, "progress.json")
    done = json.load(open(progress))["sessions"] if os.path.exists(progress) else 0
    sess = sessions(unit, "session")
    t0 = time.time()
    for i, (sid, turns) in enumerate(sess):
        if i < done:
            continue
        date = parse_date(turns[0].get("timestamp"))
        day = date.strftime("%Y-%m-%d") if date else "unknown"
        msgs = [{"role": t["role"] if t["role"] in ("user", "assistant") else "user", "content": t["text"]} for t in turns if t["text"].strip()]
        if not msgs:
            continue
        retry(lambda: mem.add(msgs, user_id="u", metadata={"session_id": sid, "session_date": day}, prompt=MEM0_OBSERVATION.format(date=day)), f"mem0 add {unit['unit_id']} {sid}")
        json.dump({"sessions": i + 1}, open(progress, "w"))
    ingest_s = time.time() - t0
    rows = []
    for q in questions:
        t1 = time.time()
        res = mem.search(q["question"], top_k=20, filters={"user_id": "u"})
        items = res.get("results", res) if isinstance(res, dict) else res
        lines = []
        for r in items:
            day = (r.get("metadata") or {}).get("session_date")
            lines.append(f"- ({day}) {r.get('memory')}" if day else f"- {r.get('memory')}")
        rows.append({"qid": q["qid"], "unit_id": unit["unit_id"], "context": "\n".join(lines), "items": len(items), "retrieve_ms": round((time.time() - t1) * 1000, 1)})
    return rows, {"unit_id": unit["unit_id"], "adds": len(sess), "ingest_s": round(ingest_s, 1)}


class ReplayLLM:
    """Stands in for mem0's model in mem0-hybrid: returns the memories the current session produced in the mem0 run."""
    current = []

    def generate_response(self, messages, **kwargs):
        return json.dumps({"memory": ReplayLLM.current})


def mem0_hybrid_unit(unit, questions, work, model):
    import shutil

    import fastembed
    import spacy

    sys.path.insert(0, os.path.abspath(MEM0_DIR))
    import mem0_mcp as M  # noqa: E402
    from mem0 import Memory
    from qdrant_client import QdrantClient

    # the extras must be really there: mem0 silently falls back to vector search alone without them
    bm25 = os.environ.get("MEM0_BENCH_BM25", "")
    if not os.path.exists(os.path.join(bm25, "english.txt")):
        raise RuntimeError("MEM0_BENCH_BM25 must name a copy of Hugging Face's Qdrant/bm25 (english.txt missing)")
    if not spacy.util.is_package("en_core_web_sm"):
        raise RuntimeError("spaCy's en_core_web_sm is not installed")
    # mem0 builds fastembed's BM25 encoder by its Hugging Face name; point it at the local copy
    class LocalSparse(fastembed.SparseTextEmbedding):
        def __init__(self, model_name, **kw):
            super().__init__(model_name, specific_model_path=bm25, **kw)

    fastembed.SparseTextEmbedding = LocalSparse

    src = os.path.join(work, "mem0", unit["unit_id"])
    sess = sessions(unit, "session")
    if not os.path.exists(os.path.join(src, "progress.json")) or json.load(open(os.path.join(src, "progress.json")))["sessions"] < len(sess):
        raise RuntimeError(f"{unit['unit_id']}: the mem0 run has not finished this history")
    # the mem0 run's memories, by session, in the order they were stored
    snap = os.path.join(work, "mem0-hybrid", unit["unit_id"] + ".src")
    shutil.rmtree(snap, ignore_errors=True)
    shutil.copytree(os.path.join(src, "qdrant"), snap)
    cli = QdrantClient(path=snap)
    by_sid, offset = {}, None
    while True:
        pts, offset = cli.scroll("memories", limit=512, offset=offset, with_payload=True, with_vectors=False)
        for pt in pts:
            pl = pt.payload or {}
            by_sid.setdefault(pl.get("session_id"), []).append(pl)
        if offset is None:
            break
    cli.close()
    shutil.rmtree(snap, ignore_errors=True)

    M.LlmFactory.provider_to_class["openai"] = ("mem0_mcp.ClaudeCliLLM", M.BaseLlmConfig)
    M.EmbedderFactory.provider_to_class["openai"] = "mem0_mcp.OnnxMiniLM"
    d = os.path.join(work, "mem0-hybrid", unit["unit_id"])
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    mem = Memory.from_config({
        "llm": {"provider": "openai", "config": {"model": model}},
        "embedder": {"provider": "openai", "config": {"model": "all-MiniLM-L6-v2", "embedding_dims": 384}},
        "vector_store": {"provider": "qdrant", "config": {"collection_name": "memories", "path": os.path.join(d, "qdrant"), "embedding_model_dims": 384, "on_disk": True}},
        "history_db_path": os.path.join(d, "history.db"),
    })
    mem.llm = ReplayLLM()
    t0, stored = time.time(), 0
    for sid, turns in sess:
        date = parse_date(turns[0].get("timestamp"))
        day = date.strftime("%Y-%m-%d") if date else "unknown"
        msgs = [{"role": t["role"] if t["role"] in ("user", "assistant") else "user", "content": t["text"]} for t in turns if t["text"].strip()]
        if not msgs:
            continue
        got = sorted(by_sid.get(sid, []), key=lambda pl: pl.get("created_at") or "")
        ReplayLLM.current = [{k: v for k, v in {"text": pl.get("data"), "attributed_to": pl.get("attributed_to")}.items() if v} for pl in got]
        stored += len(got)
        mem.add(msgs, user_id="u", metadata={"session_id": sid, "session_date": day}, prompt=MEM0_OBSERVATION.format(date=day))
    ingest_s = time.time() - t0
    rows = []
    for q in questions:
        t1 = time.time()
        res = mem.search(q["question"], top_k=20, filters={"user_id": "u"})
        items = res.get("results", res) if isinstance(res, dict) else res
        lines = []
        for r in items:
            day = (r.get("metadata") or {}).get("session_date")
            lines.append(f"- ({day}) {r.get('memory')}" if day else f"- {r.get('memory')}")
        rows.append({"qid": q["qid"], "unit_id": unit["unit_id"], "context": "\n".join(lines), "items": len(items), "retrieve_ms": round((time.time() - t1) * 1000, 1)})
    return rows, {"unit_id": unit["unit_id"], "memories": stored, "replay_s": round(ingest_s, 1)}


def retry(fn, what, tries=6):
    """Retry a call; a usage-limit failure waits (10 minutes at a time, up to 12 hours) without using up a try."""
    waited, n = 0, 0
    while True:
        try:
            return fn()
        except Exception as e:  # noqa: BLE001 - reported below
            msg = str(e)
            if LIMIT_RE.search(msg) and waited < 12 * 3600:
                log(f"usage limit during {what}; waiting 10 min ({msg[:120]})")
                time.sleep(600)
                waited += 600
                continue
            n += 1
            if n >= tries:
                raise
            log(f"retry {n} {what}: {msg[:200]}")
            time.sleep(5 * 2**n)


# ---------------------------------------------------------------- hindsight

def amb_format_chat(turns):
    """AMB BEAMDataset._format_chat: '[anchor | Turn id] Role: content', blank line between turns."""
    lines = []
    for t in turns:
        d = parse_date(t.get("timestamp"))
        anchor = f"{d:%B}-{d.day}-{d.year}" if d else ""
        prefix = f"[{anchor} | Turn {t['turn_id']}] " if anchor else f"[Turn {t['turn_id']}] "
        lines.append(f"{prefix}{t['role'].capitalize()}: {t['text']}")
    return "\n\n".join(lines)


def hindsight_documents(unit):
    """AMB's documents for this history: id, content, timestamp (ISO), context."""
    docs = []
    if is_beam(unit):
        conv = unit["unit_id"]
        batches = sessions(unit, "batch")
        for s_idx, (sid, turns) in enumerate(batches):
            start, doc_idx = 0, 0
            while start < len(turns):
                end = start + 1
                while end < len(turns) and len(amb_format_chat(turns[start:end + 1])) <= 100_000:
                    end += 1
                chunk = turns[start:end]
                d = parse_date(chunk[0].get("timestamp"))
                docs.append({"id": f"{conv}_s{s_idx}_{doc_idx}", "content": amb_format_chat(chunk),
                             "timestamp": d.strftime("%Y-%m-%dT00:00:00+00:00") if d else None,
                             "context": f"Conversation {conv} (session {s_idx + 1}/{len(batches)})"})
                doc_idx += 1
                start = end
    else:
        qid = unit["unit_id"]
        for sid, turns in sessions(unit, "session"):
            d = parse_date(turns[0].get("timestamp"))
            doc_id = f"{qid}_{sid}"
            docs.append({"id": doc_id, "content": json.dumps([{"role": t["role"], "content": t["text"]} for t in turns]),
                         "timestamp": d.isoformat() if d else None,
                         "context": f"Session {doc_id} - you are the assistant in this conversation - happened on {d:%Y-%m-%d %H:%M:%S} UTC." if d else f"Session {doc_id}"})
    return docs


def amb_format(results, chunks):
    """AMB _deduplicate_results + _format_results: each fact with its type and dates, a source chunk inlined on first use."""
    seen, out, seen_chunks = set(), [], set()
    for r in results:
        if r.get("id") in seen:
            continue
        seen.add(r.get("id"))
        lines = [f"**[{r['type']}]** {r.get('text') or ''}" if r.get("type") else (r.get("text") or "")]
        meta = []
        s, e = r.get("occurred_start"), r.get("occurred_end")
        if s and e and s != e:
            meta.append(f"occurred: {s} – {e}")
        elif s:
            meta.append(f"occurred: {s}")
        if r.get("mentioned_at"):
            meta.append(f"mentioned: {r['mentioned_at']}")
        cid = r.get("chunk_id")
        if cid:
            meta.append(f"chunk: {cid}")
        if meta:
            lines.append("_" + " · ".join(meta) + "_")
        if chunks and cid and cid in chunks and cid not in seen_chunks:
            lines.append(f"> {(chunks[cid] or {}).get('text', '')}")
            seen_chunks.add(cid)
        out.append("\n".join(lines))
    return out


def hindsight_unit(unit, questions, work, url, recall_max, chunk_tokens, label, ingest_only=False):
    import httpx

    cli = httpx.Client(base_url=url, timeout=600)
    bank = re.sub(r"[^A-Za-z0-9_-]", "-", unit["unit_id"])
    marker = os.path.join(work, "hindsight", f"{bank}.ingested")
    os.makedirs(os.path.dirname(marker), exist_ok=True)
    t0 = time.time()
    docs = hindsight_documents(unit)
    if not os.path.exists(marker):
        cfg = {"enable_observations": False}
        if is_beam(unit):
            cfg["retain_mission"] = BEAM_RETAIN_MISSION
        # a bank left half-loaded by an interrupted run is resumed, not rebuilt: Hindsight's retain of
        # byte-identical content is a no-op (no changed chunk, no extraction), so only missing documents cost calls
        existing = {b["bank_id"] for b in cli.get("/v1/default/banks").raise_for_status().json().get("banks", [])}
        if bank in existing:
            log(f"{unit['unit_id']}: resuming a partly loaded bank")
        cli.put(f"/v1/default/banks/{bank}", json=cfg).raise_for_status()
        items = [{k: v for k, v in {"content": doc["content"].replace("\x00", ""), "document_id": doc["id"], "metadata": {"doc_id": doc["id"]},
                                     "timestamp": doc["timestamp"], "context": doc["context"]}.items() if v is not None} for doc in docs]
        per = 1 if is_beam(unit) else 4  # small requests: a failed operation costs only its own documents
        batches = [items[i:i + per] for i in range(0, len(items), per)]
        for round_ in range(80):
            ops = {}
            for b, batch in enumerate(batches):
                r = cli.post(f"/v1/default/banks/{bank}/memories", json={"items": batch, "async": True})
                r.raise_for_status()
                body = r.json()
                for op in body.get("operation_ids") or [body.get("operation_id")]:
                    if op:
                        ops[op] = b
            failed = wait_ops(cli, bank, list(ops), unit["unit_id"])
            if not failed:
                break
            # resubmit the batches whose operations failed (same document ids: a retain replaces its document)
            limited = any(LIMIT_RE.search(err) for err in failed.values())
            if not limited and round_ >= 3:
                raise RuntimeError(f"{unit['unit_id']}: {len(failed)} retain operations failed: {list(failed.values())[0][:300]}")
            batches = [batches[b] for b in sorted({ops[op] for op in failed})]
            log(f"{unit['unit_id']}: {len(failed)} operations failed ({'usage limit' if limited else 'error'}); resubmitting {len(batches)} batches")
            time.sleep(600 if limited else 60)
        else:
            raise RuntimeError(f"{unit['unit_id']}: retain kept failing")
        open(marker, "w").write(json.dumps({"documents": len(docs), "ingest_s": round(time.time() - t0, 1)}))
    info = json.load(open(marker))
    if ingest_only:
        return [], {"unit_id": unit["unit_id"], "documents": info["documents"], "ingest_s": info["ingest_s"]}
    beam = is_beam(unit)
    max_tokens = recall_max or (12288 if beam else 32768)
    chunk_max = chunk_tokens if chunk_tokens is not None else (8192 if beam else 16384)
    rows = []
    for q in questions:
        body = {"query": q["question"][:1900], "budget": "high", "max_tokens": max_tokens,
                "include": {"entities": None, "chunks": {"max_tokens": chunk_max} if chunk_max > 0 else None}}
        qd = parse_date(q.get("question_date"))
        if qd:
            body["query_timestamp"] = qd.isoformat()
        t1 = time.time()
        r = retry(lambda: cli.post(f"/v1/default/banks/{bank}/memories/recall", json=body).raise_for_status().json(), f"recall {q['qid']}")
        blocks = amb_format(r.get("results") or [], r.get("chunks") or {})
        rows.append({"qid": q["qid"], "unit_id": unit["unit_id"], "context": "\n\n".join(blocks), "items": len(blocks), "retrieve_ms": round((time.time() - t1) * 1000, 1), "config": label})
    return rows, {"unit_id": unit["unit_id"], "documents": info["documents"], "ingest_s": info["ingest_s"]}


def wait_ops(cli, bank, ops, what, max_wait=12 * 3600):
    """Wait for retain operations (the server retries a failed task itself first); returns {operation: error} for those that failed."""
    import httpx

    t0 = time.time()
    pending, failed = set(ops), {}
    while pending and time.time() - t0 < max_wait:
        for op in list(pending):
            try:
                r = cli.get(f"/v1/default/banks/{bank}/operations/{op}")
            except httpx.TransportError:  # the server restarting: it recovers its tasks, so keep waiting
                break
            if r.status_code == 404:
                pending.discard(op)
                continue
            st = (r.json() or {}).get("status")
            if st in ("completed", "failed", "cancelled"):
                if st != "completed":
                    failed[op] = json.dumps(r.json())
                    log(f"{what}: operation {op} {st}: {failed[op][:300]}")
                pending.discard(op)
        if pending:
            time.sleep(10)
    if pending:
        raise RuntimeError(f"{what}: {len(pending)} retain operations still pending")
    return failed


# ---------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("system", choices=["mem0", "mem0-hybrid", "hindsight"])
    ap.add_argument("--data", required=True)
    ap.add_argument("--every", type=int, default=1, help="every n-th question, in data order")
    ap.add_argument("--work", required=True, help="stores and progress")
    ap.add_argument("--out", required=True, help="contexts .jsonl (appended; finished histories are skipped)")
    ap.add_argument("--stats", help="per-history ingest stats .jsonl (default: <out>.ingest.jsonl)")
    ap.add_argument("--workers", type=int, default=6)
    ap.add_argument("--model", default="haiku", help="mem0's model")
    ap.add_argument("--url", default="http://127.0.0.1:8888", help="hindsight-api")
    ap.add_argument("--recall-max-tokens", type=int, default=0)
    ap.add_argument("--chunk-tokens", type=int, default=None)
    ap.add_argument("--label", default="")
    ap.add_argument("--limit-units", type=int, default=0)
    ap.add_argument("--ingest-only", action="store_true", help="hindsight: load the banks, recall nothing")
    args = ap.parse_args()

    units = [json.loads(line) for line in open(args.data)]
    allq = [(q, u) for u in units for q in u["questions"]]
    wanted = {q["qid"] for q, _ in allq[:: args.every]}
    jobs = [(u, [q for q in u["questions"] if q["qid"] in wanted]) for u in units]
    jobs = [j for j in jobs if j[1]]
    random.Random(7).shuffle(jobs)
    if args.limit_units:
        jobs = jobs[: args.limit_units]
    done = set()
    if os.path.exists(args.out):
        done = {json.loads(line)["unit_id"] for line in open(args.out)}
    todo = [j for j in jobs if j[0]["unit_id"] not in done]
    stats_path = args.stats or args.out.replace(".jsonl", "") + ".ingest.jsonl"
    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)
    log(f"{args.system}: {len(jobs)} histories, {sum(len(q) for _, q in jobs)} questions; {len(todo)} to do")

    if args.system in ("mem0", "mem0-hybrid"):
        os.environ.setdefault("MEM0_BENCH_DIR", os.path.join(args.work, args.system))
        os.environ.setdefault("MEM0_BENCH_ONNX", "/home/claude/bench/shared/chroma-onnx/all-MiniLM-L6-v2")
        os.environ["MEM0_TELEMETRY"] = "False"
        pool = cf.ProcessPoolExecutor(args.workers)
        fn = mem0_unit if args.system == "mem0" else mem0_hybrid_unit
        futs = {pool.submit(fn, u, qs, args.work, args.model): u["unit_id"] for u, qs in todo}
    else:
        pool = cf.ThreadPoolExecutor(args.workers)
        futs = {pool.submit(hindsight_unit, u, qs, args.work, args.url, args.recall_max_tokens, args.chunk_tokens, args.label, args.ingest_only): u["unit_id"] for u, qs in todo}
    n, failed = 0, []
    for f in cf.as_completed(futs):
        uid = futs[f]
        try:
            rows, st = f.result()
        except Exception as e:  # noqa: BLE001 - one history failing should not stop the rest
            failed.append(uid)
            log(f"FAILED {uid}: {str(e)[:300]}")
            continue
        with open(args.out if rows else os.devnull, "a") as fo:
            for r in rows:
                fo.write(json.dumps(r) + "\n")
        with open(stats_path, "a") as fs:
            fs.write(json.dumps(st) + "\n")
        n += 1
        log(f"done {uid} ({n}/{len(todo)}): {len(rows)} questions, {st}")
    log(f"finished: {n} histories, {len(failed)} failed {failed}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
