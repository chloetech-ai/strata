"""How Claude's relevance grades (/v1/judge: UMBRELA's prompt, one row at a
time) agree with TREC-COVID's human judgments, row by row and in which of
two rankings they prefer.

For each of the first QUERIES queries (by id), the pool is the top 10 of
BM25 and the top 10 of dense retrieval (Cohere embed-english-v3,
1,024 dimensions), as the relevance lab pools rankings A and B. Each pooled row is
graded by Claude with the prompt and the passage judge.rs builds (title
and text, each cut at 4,000 characters), through `claude -p` (no tools,
no settings, no session), since the container has no API key; the
server's endpoint sends the same prompt as the only user message.

Reported:
- rows: the grades of the pooled rows that have a TREC judgment, against
  it (TREC-COVID grades 0, 1, 2; UMBRELA 0 to 3): a confusion matrix, and
  Cohen's kappa of "relevant" at each pair of thresholds;
- rankings: nDCG@10 of BM25 and of dense retrieval per query, with TREC's
  judgments and with Claude's grades, the share of queries whose winner
  (by more than 0.01) both name alike, and Kendall's tau between the two
  per-query differences.

    python umbrela.py --data ~/data/beir/trec-covid --cache grades.jsonl --out umbrela_trec-covid.json
"""

import argparse
import concurrent.futures as cf
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import time
from collections import Counter, defaultdict

import numpy as np

PROMPT = """Given a query and a passage, you must provide a score on an integer scale of 0 to 3 with the following meanings:
0 = represent that the passage has nothing to do with the query,
1 = represents that the passage seems related to the query but does not answer it,
2 = represents that the passage has some answer for the query, but the answer may be a bit unclear, or hidden amongst extraneous information and
3 = represents that the passage is dedicated to the query and contains the exact answer.

Important Instruction: Assign category 1 if the passage is somewhat related to the topic but not completely, category 2 if passage presents something very important related to the entire topic but also has some extra information and category 3 if the passage only and entirely refers to the topic. If none of the above satisfies give it category 0.

Query: {query}
Passage: {passage}

Split this problem into steps:
Consider the underlying intent of the search.
Measure how well the content matches a likely intent of the query (M).
Measure how trustworthy the passage is (T).
Consider the aspects above and the relative importance of each, and decide on a final score (O). Final score must be an integer value only.
Do not provide any code in result. Provide each score in the format of: ##final score: score without providing any reasoning."""

CHARS = 4000
# The variables that tie a process to the Claude Code session running this
# script; `claude -p` runs without them, as a session of its own.
SESSION_VARS = [
    "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_REMOTE_SESSION_ID", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDE_CODE_REMOTE_SDK_URL",
    "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_PROJECTS_SESSION", "CLAUDECODE", "CLAUDE_PID", "CLAUDE_CODE_TEE_SDK_STDOUT",
    "CLAUDE_CODE_DIAGNOSTICS_FILE", "CLAUDE_CODE_CCR_EVENT_FLUSH_POLICY", "CLAUDE_CODE_USE_CCR_V2", "CLAUDE_CODE_SYNC_SESSION_REFS",
    "CLAUDE_CODE_ENABLE_REMOTE_RECAP", "CLAUDE_CODE_WORKER_EPOCH", "CLAUDE_CODE_SESSION_ATTENDED",
]


def cut(s):
    return s[:CHARS] + "…" if len(s) > CHARS else s


def passage(doc):
    # judge.rs: one line per field, after its name, empty fields left out.
    return "\n".join(f"{k}: {cut(doc[k])}" for k in ("title", "text") if doc.get(k))


def grade_of(answer):
    # judge.rs's grade_of.
    low = answer.lower()
    i = low.rfind("final score")
    if i >= 0:
        m = re.search(r"\d", low[i + len("final score"):])
        return int(m.group()) if m and int(m.group()) <= 3 else None
    ds = re.findall(r"\d", low)
    return int(ds[0]) if len(ds) == 1 and int(ds[0]) <= 3 else None


def ask(prompt, model, cwd):
    env = {k: v for k, v in os.environ.items() if k not in SESSION_VARS}
    cmd = ["claude", "-p", "--model", model, "--tools", "", "--no-session-persistence", "--setting-sources", "", "--disable-slash-commands",
           "--strict-mcp-config", "--system-prompt", "You grade search results.", "--output-format", "json", prompt]
    for attempt in range(3):
        r = subprocess.run(cmd, capture_output=True, text=True, env=env, cwd=cwd, timeout=300)
        try:
            j = json.loads(r.stdout)
            if not j.get("is_error"):
                return j.get("result", ""), j.get("usage", {})
        except json.JSONDecodeError:
            pass
        time.sleep(5 * (attempt + 1))
    return None, {}


def bm25_top(docs, queries, k=10, k1=0.9, b=0.4):
    tok = lambda s: re.findall(r"[a-z0-9]+", s.lower())
    df = Counter()
    lens = []
    postings = defaultdict(list)
    for d, doc in enumerate(docs):
        tf = Counter(tok(doc.get("title", "") + " " + doc.get("text", "")))
        lens.append(sum(tf.values()))
        for t, c in tf.items():
            postings[t].append((d, c))
    n = len(docs)
    avg = sum(lens) / n
    out = {}
    for qid, q in queries.items():
        s = defaultdict(float)
        for t in set(tok(q)):
            p = postings.get(t)
            if not p:
                continue
            idf = math.log(1 + (n - len(p) + 0.5) / (len(p) + 0.5))
            for d, c in p:
                s[d] += idf * c * (k1 + 1) / (c + k1 * (1 - b + b * lens[d] / avg))
        out[qid] = [d for d, _ in sorted(s.items(), key=lambda x: -x[1])[:k]]
    return out


def ndcg(ranked, grades, k=10):
    dcg = sum((2 ** grades.get(d, 0) - 1) / math.log2(i + 2) for i, d in enumerate(ranked[:k]))
    ideal = sorted(grades.values(), reverse=True)[:k]
    idcg = sum((2 ** g - 1) / math.log2(i + 2) for i, g in enumerate(ideal))
    return dcg / idcg if idcg > 0 else 0.0


def kappa(pairs):
    n = len(pairs)
    po = sum(a == b for a, b in pairs) / n
    pa = sum(a for a, _ in pairs) / n
    pb = sum(b for _, b in pairs) / n
    pe = pa * pb + (1 - pa) * (1 - pb)
    return (po - pe) / (1 - pe) if pe < 1 else None


def kendall(x, y):
    c = d = 0
    for i in range(len(x)):
        for j in range(i + 1, len(x)):
            s = (x[i] - x[j]) * (y[i] - y[j])
            c += s > 0
            d += s < 0
    return (c - d) / (c + d) if c + d else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--queries", type=int, default=25)
    ap.add_argument("--model", default="claude-haiku-5-5")
    ap.add_argument("--workers", type=int, default=6)
    ap.add_argument("--out", required=True)
    ap.add_argument("--cache", help="grades already asked for, reused (JSON lines)")
    a = ap.parse_args()

    docs = [json.loads(l) for l in open(os.path.join(a.data, "docs.jsonl"))]
    index = {d["id"]: i for i, d in enumerate(docs)}
    qs = [json.loads(l) for l in open(os.path.join(a.data, "queries.jsonl"))]
    qvec_order = [q["id"] for q in qs]
    qs = sorted(qs, key=lambda q: int(q["id"]))[: a.queries]
    queries = {q["id"]: q["text"] for q in qs}
    qrels = defaultdict(dict)
    for line in open(os.path.join(a.data, "qrels.tsv")):
        qid, did, g = line.split("\t")
        if qid in queries and did in index and int(g) >= 0:
            qrels[qid][did] = int(g)
    meta = json.load(open(os.path.join(a.data, "meta.json")))
    dim = meta["dim"]
    dv = np.fromfile(os.path.join(a.data, "doc_vecs.f32"), dtype=np.float32).reshape(-1, dim)
    qv = np.fromfile(os.path.join(a.data, "query_vecs.f32"), dtype=np.float32).reshape(-1, dim)
    dense = {}
    for qid in queries:
        s = dv @ qv[qvec_order.index(qid)]
        top = np.argpartition(-s, 10)[:10]
        dense[qid] = [int(d) for d in top[np.argsort(-s[top])]]
    print("dense done", file=sys.stderr, flush=True)
    bm25 = bm25_top(docs, queries)
    print("bm25 done", file=sys.stderr, flush=True)
    runs = {"bm25": {q: [docs[d]["id"] for d in r] for q, r in bm25.items()}, "dense": {q: [docs[d]["id"] for d in r] for q, r in dense.items()}}

    cache = {}
    if a.cache and os.path.exists(a.cache):
        for line in open(a.cache):
            r = json.loads(line)
            cache[(r["query"], r["doc"])] = r
    jobs = sorted({(q, d) for run in runs.values() for q, r in run.items() for d in r} - set(cache))
    print(f"{len(jobs)} rows to grade ({len(cache)} cached)", file=sys.stderr, flush=True)
    # An empty directory: no CLAUDE.md for `claude -p` to find.
    cwd = tempfile.mkdtemp()
    t0 = time.time()
    with cf.ThreadPoolExecutor(a.workers) as ex, open(a.cache or os.devnull, "a") as log:
        futs = {ex.submit(ask, PROMPT.format(query=queries[q], passage=passage(docs[index[d]])), a.model, cwd): (q, d) for q, d in jobs}
        for i, f in enumerate(cf.as_completed(futs)):
            q, d = futs[f]
            answer, usage = f.result()
            r = {"query": q, "doc": d, "answer": answer, "grade": grade_of(answer) if answer is not None else None, "usage": usage}
            cache[(q, d)] = r
            log.write(json.dumps(r) + "\n")
            log.flush()
            if (i + 1) % 25 == 0:
                print(f"{i + 1}/{len(jobs)} graded, {time.time() - t0:.0f} s", file=sys.stderr, flush=True)

    llm = defaultdict(dict)
    failed = 0
    for (q, d), r in cache.items():
        if q in queries and r["grade"] is not None:
            llm[q][d] = r["grade"]
        elif q in queries:
            failed += 1
    # Rows: the pooled rows TREC judged.
    rows = [(qrels[q][d], g) for q in queries for d, g in llm[q].items() if d in qrels[q]]
    confusion = {f"trec {t}": {f"claude {c}": sum(1 for x, y in rows if x == t and y == c) for c in range(4)} for t in range(3)}
    kappas = {f"trec >= {t}, claude >= {c}": kappa([(x >= t, y >= c) for x, y in rows]) for t in (1, 2) for c in (1, 2, 3)}
    # Rankings: nDCG@10 per query by each grader; unjudged rows count 0.
    per = []
    for q in queries:
        h = {s: ndcg(runs[s][q], qrels[q]) for s in runs}
        c = {s: ndcg(runs[s][q], llm[q]) for s in runs}
        per.append({"query": q, "trec": h, "claude": c})
    dh = [p["trec"]["dense"] - p["trec"]["bm25"] for p in per]
    dc = [p["claude"]["dense"] - p["claude"]["bm25"] for p in per]
    sign = lambda x: 0 if abs(x) <= 0.01 else (1 if x > 0 else -1)
    decided = [(sign(x), sign(y)) for x, y in zip(dh, dc) if sign(x) != 0]
    pooled = sum(len(r) for r in llm.values())
    judged_share = len(rows) / pooled if pooled else None
    out = {
        "collection": "trec-covid", "queries": len(queries), "model": a.model, "prompt": "umbrela (strata-server/src/judge.rs)",
        "pool": "bm25 top 10 + dense top 10", "graded": pooled, "failed": failed, "judged_by_trec": len(rows), "judged_share": judged_share,
        "confusion": confusion, "kappa": kappas,
        "mean_ndcg@10": {g: {s: sum(p[g][s] for p in per) / len(per) for s in runs} for g in ("trec", "claude")},
        "winner_agreement": {"queries_trec_decides": len(decided), "same_winner": sum(x == y for x, y in decided)},
        "kendall_tau_of_differences": kendall(dh, dc),
        "per_query": per,
        # `claude -p` sends Claude Code's own context with each prompt (the
        # cache tokens): the server's requests are the prompt alone.
        "usage": {
            "calls": len(cache),
            "input_tokens": sum(sum(r["usage"].get(k, 0) or 0 for k in ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens")) for r in cache.values()),
            "output_tokens": sum(r["usage"].get("output_tokens", 0) or 0 for r in cache.values()),
            "thinking_tokens": sum((r["usage"].get("output_tokens_details") or {}).get("thinking_tokens", 0) or 0 for r in cache.values()),
            "prompt_chars_mean": sum(len(PROMPT.format(query=queries[q], passage=passage(docs[index[d]]))) for q, d in cache) / max(1, len(cache)),
        },
    }
    json.dump(out, open(a.out, "w"), indent=1)
    print(json.dumps({k: v for k, v in out.items() if k != "per_query"}, indent=1))


if __name__ == "__main__":
    main()
