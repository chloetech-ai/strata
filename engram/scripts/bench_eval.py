#!/usr/bin/env python3
"""Answer quality on BEAM and MemoryAgentBench's FactConsolidation, scored the
way each benchmark scores it (data from scripts/convert_new_benchmarks.py).

  python3 scripts/bench_eval.py --bench mab --data mab_factconsolidation.jsonl \
      --config engram=results/v2/new/mab.contexts.jsonl@base10 --config none=none --config full=full \
      --reader haiku --judge haiku --out results/v2/new/mab_reader

Contexts as in reader_eval.py: name=contexts.jsonl[@field] (engram-bench deep
writes `context`, the query engine's picks, and `base10`, the plain hybrid
search's top 10), none, or full (the whole history, when it fits in
--max-full-chars; --full-every samples it, being the costly one).

  mab   The reader gets MemoryAgentBench's own FactConsolidation question
        (utils/templates.py, rag_agent) under "[Knowledge Pool]". Scored by
        substring exact match against any accepted answer after the
        benchmark's normalisation (utils/eval_other_utils.py). No judge.
  beam  The reader gets BEAM's RAG answering prompt (src/prompts.py,
        answer_generation_for_rag). The judge scores each rubric item 0, 0.5
        or 1 with BEAM's unified judge prompt; a question scores the mean of
        its items. BEAM asks one call per item; here one call grades all of
        an answer's items, each under its own copy of the prompt. Event
        ordering's Kendall tau is not computed (judge score only).

Answers are cached in <out>/answers.jsonl and grades in <out>/grades.jsonl.
Both prompts are copied from the benchmarks' repositories (MIT licence).
"""

import argparse
import concurrent.futures as cf
import json
import os
import re
import string
import sys
import threading
import time
from collections import defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from reader_eval import call, sessions_text  # noqa: E402

MAB_SYSTEM = "You are a helpful assistant that can read the context and memorize it for future retrieval."
MAB_QUERY = (
    "Pretend you are a knowledge management system. Each fact in the knowledge pool is provided with a serial number at the beginning, "
    "and the newer fact has larger serial number. \n You need to solve the conflicts of facts in the knowledge pool by finding the newest fact "
    "with larger serial number. You need to answer a question based on this rule. You should give a very concise answer without saying other "
    "words for the question **only** from the knowledge pool you have memorized rather than the real facts in real world. \n\nFor example:\n\n"
    " [Knowledge Pool] \n\n Question: Based on the provided Knowledge Pool, what is the name of the current president of Russia? \n"
    "Answer: Donald Trump \n\n Now Answer the Question: Based on the provided Knowledge Pool, {question} \nAnswer:"
)

BEAM_SYSTEM = "You are a helpful assistant."
BEAM_ANSWER = """
You are an assistant that MUST answer questions using ONLY the information provided in the context below.

STRICT INSTRUCTIONS:
1. Answer ONLY based on the provided context
2. Do NOT use your internal knowledge

CONTEXT:
<context>

QUESTION:
<question>

ANSWER REQUIREMENTS:
- Be direct and concise
- Only output the answer to the question without any explanation

RESPONSE:
"""

BEAM_JUDGE = """
You are an expert evaluator tasked with judging whether the LLM's response demonstrates compliance with the specified RUBRIC CRITERION.

## EVALUATION INPUTS
- RUBRIC CRITERION (what to check): <rubric_item>
- RESPONSE TO EVALUATE: <llm_response>

## EVALUATION RUBRIC:
The rubric defines a specific requirement, constraint, or expected behavior that the LLM response should demonstrate.

**IMPORTANT**: Pay careful attention to whether the rubric specifies:
- **Positive requirements** (things the response SHOULD include/do)
- **Negative constraints** (things the response SHOULD NOT include/do, often indicated by "no", "not", "avoid", "absent")

## RESPONSIVENESS REQUIREMENT
A compliant response must be **on-topic** and attempt to answer it.
- If the response does not address the QUESTION, score **0.0** and stop.
- For negative constraints, both must hold: (a) the response is responsive to the QUESTION, and (b) the prohibited element is absent.

## SEMANTIC TOLERANCE RULES:
Judge by meaning, not exact wording.
- Accept **paraphrases** and **synonyms** that preserve intent.
- **Case/punctuation/whitespace** differences must be ignored.
- **Numbers/currencies/dates** may appear in equivalent forms (e.g., “$68,000”, “68k”, “68,000 USD”, or “sixty-eight thousand dollars”). Treat them as equal when numerically equivalent.
- If the rubric expects a number or duration, prefer **normalized comparison** (extract and compare values) over string matching.

## STYLE NEUTRALITY (prevents style contamination):
Ignore tone, politeness, length, and flourish unless the rubric explicitly requires a format/structure (e.g., “itemized list”, “no citations”, “one sentence”).
- Do **not** penalize hedging, voice, or verbosity if content satisfies the rubric.
- Only evaluate format when the rubric **explicitly** mandates it.

## SCORING SCALE:
- **1.0 (Complete Compliance)**: Fully complies with the rubric criterion.
  - Positive: required element present, accurate, properly executed (allowing semantic equivalents).
  - Negative: prohibited element **absent** AND response is **responsive**.

- **0.5 (Partial Compliance)**: Partially complies.
  - Positive: element present but minor inaccuracies/incomplete execution.
  - Negative: generally responsive and mostly avoids the prohibited element but with minor/edge violations.

- **0.0 (No Compliance)**: Fails to comply.
  - Positive: required element missing or incorrect.
  - Negative: prohibited element present **or** response is non-responsive/evasive even if the element is absent.

## EVALUATION INSTRUCTIONS:
1. **Understand the Requirement**: Determine if the rubric is asking for something to be present (positive) or absent (negative/constraint).

2. **Parse Compound Statements**: If the rubric contains multiple elements connected by "and" or commas, evaluate whether:
   - **All elements** must be present for full compliance (1.0)
   - **Some elements** present indicates partial compliance (0.5)
   - **No elements** present indicates no compliance (0.0)

3. **Check Compliance**:
   - For positive requirements: Look for the presence and quality of the required element
   - For negative constraints: Look for the absence of the prohibited element

4. **Assign Score**: Based on compliance with the specific rubric criterion according to the scoring scale above.

5. **Provide Reasoning**: Explain whether the rubric criterion was satisfied and justify the score.

## OUTPUT FORMAT:
Return your evaluation in JSON format with two fields:

{
   "score": [your score: 1.0, 0.5, or 0.0],
   "reason": "[detailed explanation of whether the rubric criterion was satisfied and why this justified the assigned score]"
}

NOTE: ONLY output the json object, without any explanation before or after that
"""


def normalize(text):
    text = text.lower()
    text = "".join(c for c in text if c not in string.punctuation)
    text = re.sub(r"\b(a|an|the)\b", " ", text)
    return " ".join(text.split())


def sub_em(prediction, answers):
    p = normalize(prediction)
    return float(any(normalize(a) in p for a in answers))


def facts_text(turns):
    return "\n".join(t["text"] for t in turns)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bench", required=True, choices=["mab", "beam"])
    ap.add_argument("--data", required=True)
    ap.add_argument("--config", action="append", required=True, help="name=context.jsonl[@field] | name=none | name=full")
    ap.add_argument("--out", required=True)
    ap.add_argument("--reader", default="haiku")
    ap.add_argument("--judge", default="haiku")
    ap.add_argument("--every", type=int, default=1)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--workers", type=int, default=8)
    ap.add_argument("--full-every", type=int, default=1, help="the full config reads only every n-th question (it is the costly one)")
    ap.add_argument("--max-full-chars", type=int, default=600_000, help="skip full where the history is longer (the reader's window)")
    args = ap.parse_args()

    qs = {}
    for line in open(args.data):
        u = json.loads(line)
        for q in u["questions"]:
            qs[q["qid"]] = (q, u)
    qids = list(qs)[:: args.every]
    if args.limit:
        qids = qids[: args.limit]

    contexts = {}
    for spec in args.config:
        name, src = spec.split("=", 1)
        src, _, field = src.partition("@")
        if src == "none":
            contexts[name] = {q: None for q in qids}
        elif src == "full":
            text = {}
            for q in qids[:: args.full_every]:
                u = qs[q][1]
                if u["unit_id"] not in text:
                    text[u["unit_id"]] = facts_text(u["turns"]) if args.bench == "mab" else sessions_text(u["turns"])
            contexts[name] = {q: text[qs[q][1]["unit_id"]] for q in qids[:: args.full_every] if len(text[qs[q][1]["unit_id"]]) <= args.max_full_chars}
        else:
            c = {}
            for line in open(src):
                r = json.loads(line)
                c[r["qid"]] = r[field or "context"]
            contexts[name] = c

    os.makedirs(args.out, exist_ok=True)
    ans_path, grade_path = os.path.join(args.out, "answers.jsonl"), os.path.join(args.out, "grades.jsonl")
    done = {}
    if os.path.exists(ans_path):
        for line in open(ans_path):
            r = json.loads(line)
            done[(r["config"], r["qid"])] = r
    grades = {}
    if os.path.exists(grade_path):
        for line in open(grade_path):
            g = json.loads(line)
            grades[(g["config"], g["qid"])] = g
    lock = threading.Lock()

    def read(name, qid):
        q, _ = qs[qid]
        ctx = contexts[name].get(qid)
        if args.bench == "mab":
            query = MAB_QUERY.format(question=q["question"])
            prompt = query if ctx is None else f"[Knowledge Pool]\n{ctx.strip() or '(nothing)'}\n\n{query}"
            system = MAB_SYSTEM
        else:
            prompt = BEAM_ANSWER.replace("<context>", (ctx or "").strip() or "(nothing)").replace("<question>", q["question"])
            system = BEAM_SYSTEM
        answer, cost, tokens = call(args.reader, system, prompt)
        r = {"config": name, "qid": qid, "qtype": q["question_type"], "context_chars": len(ctx) if ctx else 0, "reader_tokens": tokens, "answer": answer, "cost_reader": cost}
        with lock, open(ans_path, "a") as f:
            f.write(json.dumps(r) + "\n")
        return r

    def grade(r):
        q, _ = qs[r["qid"]]
        if args.bench == "mab":
            g = {"config": r["config"], "qid": r["qid"], "score": sub_em(r["answer"], q.get("answers") or [q["answer"]]), "cost_judge": 0.0}
        else:
            items = q.get("rubric") or []
            if not items:
                return None
            parts = [f"### Item {k}\n" + BEAM_JUDGE.replace("<rubric_item>", it).replace("<llm_response>", r["answer"]) for k, it in enumerate(items, 1)]
            prompt = (
                "Grade each item below independently, following its own instructions.\n\n" + "\n\n".join(parts)
                + f"\n\nReply with exactly {len(items)} lines, one per item, in the form '<item number>: <score>' where score is 1.0, 0.5 or 0.0."
            )
            out, cost, _ = call(args.judge, "You grade answers. Reply only with the requested lines.", prompt)
            got = {}
            for line in out.splitlines():
                m = re.match(r"\s*(?:item\s*)?(\d+)\s*[:.)-]\s*(1(?:\.0)?|0?\.5|0(?:\.0)?)\b", line.strip().lower())
                if m:
                    got[int(m.group(1))] = float(m.group(2))
            if sorted(got) != list(range(1, len(items) + 1)):
                raise RuntimeError(f"judge returned {len(got)} of {len(items)} scores")
            g = {"config": r["config"], "qid": r["qid"], "score": sum(got.values()) / len(items), "items": [got[k] for k in sorted(got)], "cost_judge": cost}
        with lock, open(grade_path, "a") as f:
            f.write(json.dumps(g) + "\n")
        return g

    def run(fn, jobs, what):
        out, t0 = [], time.time()
        with cf.ThreadPoolExecutor(args.workers) as ex:
            futs = [ex.submit(fn, *j) for j in jobs]
            for i, fut in enumerate(cf.as_completed(futs), 1):
                try:
                    v = fut.result()
                    if v is not None:
                        out.append(v)
                except Exception as e:  # noqa: BLE001 - reported; a rerun retries it
                    print(f"{what} failed: {e}", file=sys.stderr)
                if i % 50 == 0:
                    print(f"  {what} {i}/{len(jobs)} in {time.time() - t0:.0f} s", file=sys.stderr)
        return out

    jobs = [(n, q) for n in contexts for q in qids if q in contexts[n] and (n, q) not in done]
    print(f"{len(jobs)} answers to get ({len(done)} cached)", file=sys.stderr)
    for r in run(read, jobs, "read"):
        done[(r["config"], r["qid"])] = r
    wanted = set(qids)
    pending = [(r,) for k, r in done.items() if k[0] in contexts and k[1] in wanted and k not in grades]
    print(f"{len(pending)} answers to grade", file=sys.stderr)
    for g in run(grade, pending, "grade"):
        grades[(g["config"], g["qid"])] = g

    summary = {}
    for name in contexts:
        rows = [(done[(name, q)], grades[(name, q)]) for q in qids if (name, q) in done and (name, q) in grades]
        by = defaultdict(list)
        unit = defaultdict(list)
        for r, g in rows:
            by[r["qtype"]].append(g["score"])
            unit[qs[r["qid"]][1]["unit_id"]].append(g["score"])
        types = {k: {"n": len(v), "score": sum(v) / len(v)} for k, v in sorted(by.items())}
        summary[name] = {
            "n": len(rows),
            "score": sum(g["score"] for _, g in rows) / max(1, len(rows)),
            # BEAM reports the mean over abilities
            "mean_over_types": sum(t["score"] for t in types.values()) / max(1, len(types)),
            "by_type": types,
            "by_unit": {k: {"n": len(v), "score": sum(v) / len(v)} for k, v in sorted(unit.items())},
            "context_chars": sum(r["context_chars"] for r, _ in rows) / max(1, len(rows)),
            "reader_tokens": sum(r["reader_tokens"] for r, _ in rows) / max(1, len(rows)),
            "cost_usd": sum(r["cost_reader"] + g.get("cost_judge", 0.0) for r, g in rows),
        }
        s = summary[name]
        print(f"{name:<24} n={s['n']:<5} score {s['score']:.3f}  by type {s['mean_over_types']:.3f}  context {s['context_chars']:.0f} chars  ${s['cost_usd']:.2f}", file=sys.stderr)
    with open(os.path.join(args.out, "summary.json"), "w") as f:
        json.dump({"bench": args.bench, "data": os.path.basename(args.data), "reader": args.reader, "judge": args.judge, "questions": len(qids), "configs": summary}, f, indent=1)


if __name__ == "__main__":
    main()
