"""SQL writes through strata-server with a BERT-embedded field.

Usage: bench_sqlw.py SERVER_BIN MODELS_DIR OUT_JSON

Starts the server on a fresh database whose `emb` field is
bge-small-en-v1.5's embedding of title and body, then measures:
  inserts:  K clients send `INSERT` (one row each, embedded) back to back;
  updates:  one client sends in-place `UPDATE ... SET stock = stock + 1`
            by key back to back, alone and beside two inserting clients.
"""
import http.client
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import threading
import time

BIN, MODELS, OUT = sys.argv[1:4]
PORT = 7790
WORDS = ("search engine index segment merge vector query ranking token document field table writer "
         "lock model embedding latency memory shard replica snapshot commit flush text keyword filter "
         "score graph page rank recall precision batch thread core cache disk file schema row column").split()


def text(rng, n):
    return " ".join(rng.choice(WORDS) for _ in range(n))


def conn():
    return http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)


def sql(c, stmt, params):
    t = time.perf_counter()
    c.request("POST", "/v1/sql", body=json.dumps({"sql": stmt, "params": params, "timeout_ms": 60000}), headers={"content-type": "application/json"})
    r = c.getresponse()
    body = r.read()
    if r.status != 200:
        raise RuntimeError(f"{r.status}: {body[:300]!r}")
    return (time.perf_counter() - t) * 1e3


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p * len(xs)))]


def summary(lat, secs):
    return {"n": len(lat), "secs": secs, "per_sec": len(lat) / secs, "p50_ms": pct(lat, 0.5), "p99_ms": pct(lat, 0.99), "max_ms": max(lat)}


INSERT = "INSERT INTO docs (id, title, body, stock) VALUES (:k, :t, :b, 0)"
UPDATE = "UPDATE docs SET stock = stock + 1 WHERE id = :k"


def inserts(clients, per_client, prefix):
    lat, lock = [], threading.Lock()

    def run(i):
        rng, c, mine = random.Random(i), conn(), []
        for j in range(per_client):
            mine.append(sql(c, INSERT, {"k": f"{prefix}-{i}-{j}", "t": text(rng, 8), "b": text(rng, 60)}))
        with lock:
            lat.extend(mine)

    t0 = time.perf_counter()
    ts = [threading.Thread(target=run, args=(i,)) for i in range(clients)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    return summary(lat, time.perf_counter() - t0)


def updates(secs, keys, inserting):
    stop, lat, ins = threading.Event(), [], []

    def insert(i):
        rng, c, j = random.Random(100 + i), conn(), 0
        while not stop.is_set():
            ins.append(sql(c, INSERT, {"k": f"u{inserting}-{i}-{j}", "t": text(rng, 8), "b": text(rng, 60)}))
            j += 1

    ts = [threading.Thread(target=insert, args=(i,)) for i in range(inserting)]
    [t.start() for t in ts]
    time.sleep(1.0 if inserting else 0)
    c, rng = conn(), random.Random(7)
    t0 = time.perf_counter()
    while time.perf_counter() - t0 < secs:
        lat.append(sql(c, UPDATE, {"k": rng.choice(keys)}))
    took = time.perf_counter() - t0
    stop.set()
    [t.join() for t in ts]
    return {"inserting_clients": inserting, "updates": summary(lat, took), "inserts_meanwhile": len(ins)}


def main():
    work = tempfile.mkdtemp(prefix="sqlw-")
    schema = os.path.join(work, "schema.json")
    with open(schema, "w") as f:
        json.dump({"table": "docs", "key": "id", "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text"},
            {"name": "body", "type": "text"},
            {"name": "stock", "type": "int"},
            {"name": "emb", "type": "vector", "embed": {"from": ["title", "body"], "model": "bge-small-en-v1.5"}},
        ]}, f)
    log = open(os.path.join(work, "server.log"), "w")
    srv = subprocess.Popen([BIN, "--db", os.path.join(work, "b.strata"), "--schema", schema, "--models", MODELS,
                            "--listen", f"127.0.0.1:{PORT}", "--grpc", "off"], stdout=log, stderr=subprocess.STDOUT)
    try:
        for _ in range(600):
            try:
                c = conn()
                c.request("GET", "/v1/health")
                if c.getresponse().status == 200:
                    break
            except OSError:
                pass
            time.sleep(0.1)
        else:
            raise RuntimeError("the server did not start")
        res = {"server": BIN}
        # Seed rows for the updates, and warm the model.
        res["warmup"] = inserts(1, 100, "seed")
        keys = [f"seed-0-{j}" for j in range(100)]
        res["inserts_1"] = inserts(1, 200, "a")
        res["inserts_4"] = inserts(4, 100, "b")
        res["updates_alone"] = updates(10, keys, 0)
        res["updates_beside_2_inserting"] = updates(20, keys, 2)
        print(json.dumps(res, indent=1))
        with open(OUT, "w") as f:
            json.dump(res, f, indent=1)
    finally:
        srv.terminate()
        srv.wait()
        shutil.copy(os.path.join(work, "server.log"), OUT + ".server.log")
        shutil.rmtree(work)


main()
