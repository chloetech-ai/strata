"""nDCG@10 of the workload's BM25 queries with and without query stopwords,
run through the `strata` shell against databases kept by `strata-eval --keep`.
Usage: python3 scripts/query_stopwords.py [data dir] [db dir]
Writes results/strata/query_stopwords.json."""
import collections
import json
import math
import pathlib
import subprocess
import sys

root = pathlib.Path(__file__).resolve().parent.parent
data = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "/home/user/data/beir")
dbs = pathlib.Path(sys.argv[2] if len(sys.argv) > 2 else "/home/user/data/strata")
shell = root / "target" / "release" / "strata"


def ndcg(ranked, judged):
    dcg = sum(judged.get(d, 0) / math.log2(i + 2) for i, d in enumerate(ranked[:10]))
    ideal = sorted((g for g in judged.values() if g > 0), reverse=True)[:10]
    idcg = sum(g / math.log2(i + 2) for i, g in enumerate(ideal))
    return dcg / idcg if idcg else 0.0


out = {}
for c in ["scifact", "fiqa", "trec-covid", "quora"]:
    db = dbs / f"{c}.hnsw.strata"
    if not db.exists():
        continue
    queries = [json.loads(l) for l in open(data / c / "queries.jsonl")]
    qrels = collections.defaultdict(dict)
    for line in open(data / c / "qrels.tsv"):
        q, d, g = line.rstrip("\n").split("\t")
        if q != "query-id":
            qrels[q][d] = int(g)
    idx = [w["query"] for w in json.loads((root / "results" / "workloads" / f"{c}.json").read_text())["queries"] if w["kind"] == "bm25"]
    res = {}
    for keep in (False, True):
        opt = ", stopwords => 0" if keep else ""
        clean = lambda t: t.replace("'", "''").replace(";", " ").replace("\n", " ")
        sql = "\n".join(f"SELECT id FROM docs ORDER BY bm25(body, '{clean(queries[i]['text'])}'{opt}) DESC LIMIT 10" for i in idx)
        lines = subprocess.run([str(shell), str(db)], input=sql, capture_output=True, text=True, check=True).stdout.split("\n")
        lists, cur = [], None
        for l in lines:
            if l == "id":
                cur = []
            elif l.startswith("error"):
                lists.append([])
            elif l.startswith("(") and l.endswith("ms)"):
                lists.append(cur)
                cur = None
            elif cur is not None:
                cur.append(l)
        assert len(lists) == len(idx), (c, len(lists), len(idx))
        res["keep" if keep else "drop"] = sum(ndcg(r, qrels[queries[i]["id"]]) for r, i in zip(lists, idx)) / len(idx)
    out[c] = {"queries": len(idx), "ndcg10_drop_stopwords": res["drop"], "ndcg10_keep_stopwords": res["keep"]}
    print(c, out[c], flush=True)
(root / "results" / "strata" / "query_stopwords.json").write_text(json.dumps(out, indent=1) + "\n")
