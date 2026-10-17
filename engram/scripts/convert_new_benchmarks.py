#!/usr/bin/env python3
"""Converts BEAM and MemoryAgentBench's FactConsolidation to the common JSONL
format of convert_memory_benchmarks.py, so engram-bench and the eval scripts
read them like LongMemEval.

  python3 scripts/convert_new_benchmarks.py --beam BEAM/chats --mab MemoryAgentBench/data --out /home/user/data/memory

BEAM (github.com/mohammadtavakoli78/BEAM, chats/<size>/<n>/chat.json and
probing_questions/probing_questions.json) -> beam_100k.jsonl, beam_500k.jsonl,
beam_1m.jsonl. One unit per conversation. Turns are the messages; a session
is one exchange group of a batch; a turn's timestamp is the latest time
anchor seen so far (a minute apart within a session, to keep their order).
turn_id is BEAM's message id, so a question's source_chat_ids are its
evidence. Questions keep BEAM's rubric; answer is the ideal answer (empty for
instruction and preference following, which are graded by rubric only).

MemoryAgentBench (huggingface.co/datasets/ai-hyz/MemoryAgentBench,
Conflict_Resolution split) -> mab_factconsolidation.jsonl. One unit per
context (single-hop and multi-hop, 6k to 262k tokens). Each fact of the
numbered knowledge pool is one user turn "<serial>. <fact>", a minute after
the previous one, a hundred facts per session. A later fact overrides an
earlier one that contradicts it; the questions want the latest. Questions
keep all accepted answers (scored by substring match, as the benchmark does).
"""

import argparse
import datetime as dt
import glob
import json
import os
import re

MARK = re.compile(r"\s*->->\s*\d+\s*,\s*\d+\s*$")
BASE = dt.datetime(2024, 1, 1)


def stamp(t):
    return t.strftime("%Y/%m/%d (%a) %H:%M")


def anchor(s):
    try:
        return dt.datetime.strptime(s.strip(), "%B-%d-%Y").replace(hour=9)
    except (ValueError, AttributeError):
        return None


def flat(x):
    if isinstance(x, dict):
        return [i for v in x.values() for i in flat(v)]
    if isinstance(x, list):
        return [i for v in x for i in flat(v)]
    return [x]


BEAM_ANSWER = ("answer", "ideal_answer", "ideal_response", "ideal_summary", "expected_compliance")


def beam_unit(path, size):
    conv = os.path.basename(path)
    chat = json.load(open(os.path.join(path, "chat.json")))
    pq = json.load(open(os.path.join(path, "probing_questions", "probing_questions.json")))
    turns, now = [], None
    for b in chat:
        for g, group in enumerate(b["turns"]):
            sid = f"b{b['batch_number']}g{g}"
            for k, m in enumerate(group):
                now = anchor(m.get("time_anchor") or "") or now or BASE
                turns.append({
                    "turn_id": str(m["id"]), "session_id": sid, "role": m["role"],
                    "text": MARK.sub("", m["content"]), "timestamp": stamp(now + dt.timedelta(minutes=k)),
                })
    ids = {t["turn_id"] for t in turns}
    last = max(anchor(m.get("time_anchor") or "") or BASE for b in chat for g in b["turns"] for m in g)
    questions = []
    for ability, qs in pq.items():
        for i, q in enumerate(qs):
            ev = [str(x) for x in flat(q.get("source_chat_ids") or []) if str(x) in ids]
            answer = next((q[k] for k in BEAM_ANSWER if q.get(k)), "")
            questions.append({
                "qid": f"beam{size}-{conv}-{ability}-{i}", "question": q["question"], "answer": answer,
                "question_type": ability, "question_date": stamp(last + dt.timedelta(days=1)),
                "evidence_turn_ids": sorted(set(ev), key=ev.index),
                "evidence_session_ids": sorted({t["session_id"] for t in turns if t["turn_id"] in set(ev)}),
                "abstention": ability == "abstention", "rubric": q.get("rubric") or [], "difficulty": q.get("difficulty"),
            })
    return {"dataset": f"beam_{size.lower()}", "unit_id": f"beam{size}-{conv}", "turns": turns, "questions": questions}


def convert_beam(chats, out):
    for size in ("100K", "500K", "1M"):
        dirs = sorted(glob.glob(os.path.join(chats, size, "*", "chat.json")), key=lambda p: int(p.split(os.sep)[-2]))
        if not dirs:
            continue
        path = os.path.join(out, f"beam_{size.lower()}.jsonl")
        n = q = ev = 0
        with open(path, "w") as f:
            for c in dirs:
                u = beam_unit(os.path.dirname(c), size)
                f.write(json.dumps(u) + "\n")
                n += 1
                q += len(u["questions"])
                ev += sum(1 for x in u["questions"] if x["evidence_turn_ids"])
        print(f"{path}: {n} conversations, {q} questions ({ev} with evidence ids)")


FACT = re.compile(r"^(\d+)\.\s+(.*)$")


def convert_mab(data, out):
    import pandas as pd  # only this part needs pandas + pyarrow

    df = pd.read_parquet(os.path.join(data, "Conflict_Resolution-00000-of-00001.parquet"))
    path = os.path.join(out, "mab_factconsolidation.jsonl")
    with open(path, "w") as f:
        for _, row in df.iterrows():
            src = row["metadata"]["source"]
            facts = [m.groups() for m in map(FACT.match, row["context"].splitlines()) if m]
            turns = [{
                "turn_id": f"{src}:{n}", "session_id": f"{src}:s{int(n) // 100}", "role": "user", "text": f"{n}. {text}",
                "timestamp": stamp(BASE + dt.timedelta(minutes=int(n))),
            } for n, text in facts]
            end = BASE + dt.timedelta(minutes=len(facts) + 60)
            hop = "multi-hop" if "_mh_" in src else "single-hop"
            questions = [{
                "qid": qid, "question": q, "answer": list(a)[0], "answers": [str(x) for x in a], "question_type": hop,
                "question_date": stamp(end), "evidence_turn_ids": [], "evidence_session_ids": [], "abstention": False,
            } for qid, q, a in zip(row["metadata"]["qa_pair_ids"], row["questions"], row["answers"])]
            f.write(json.dumps({"dataset": "mab_factconsolidation", "unit_id": src, "turns": turns, "questions": questions}) + "\n")
            print(f"  {src}: {len(turns)} facts, {len(questions)} questions, {len(row['context'])} chars")
    print(f"{path}: {len(df)} contexts")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--beam", help="BEAM's chats/ folder")
    ap.add_argument("--mab", help="MemoryAgentBench's data/ folder (parquet)")
    ap.add_argument("--out", default="/home/user/data/memory")
    args = ap.parse_args()
    if args.beam:
        convert_beam(args.beam, args.out)
    if args.mab:
        convert_mab(args.mab, args.out)


if __name__ == "__main__":
    main()
