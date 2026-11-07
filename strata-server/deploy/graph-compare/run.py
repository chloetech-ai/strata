"""Loads the arXiv graph into one engine and times the same graph queries on it.

    python run.py ENGINE --host H --port P --data DIR --out FILE [--token T]

ENGINE: strata (an EDGES field), strata-edges (an edge table), the same
queried over gRPC (strata-grpc, strata-edges-grpc), neo4j,
memgraph, falkordb or arangodb, already running and empty (compare.sh starts each in
Docker). One client, one query at a time, over each engine's own protocol:
HTTP for strata-server and ArangoDB, Bolt for Neo4j and Memgraph, RESP
for FalkorDB.

Each query kind runs WARM queries untimed, then every query timed: the
client's wall time from sending to holding the whole answer, and the
engine's own time where it reports one. Every answer is checked against
reference.json (prep.py); PageRank's against the exact top 20 (recall@20).
"""

import argparse
import json
import statistics
import sys
import time
from pathlib import Path

WARM = 20
BATCH = 10_000
KIND_BUDGET_S = 300  # a query kind that takes longer than this stops early
HOPS = [("both_1", "both", 1), ("both_2", "both", 2), ("out_3", "out", 3)]
PPR_EPS = [1e-4, 1e-5, 1e-6]


def stats(xs):
    if not xs:
        return None
    s = sorted(xs)
    q = lambda p: s[min(len(s) - 1, int(round(p * (len(s) - 1))))]
    return {"n": len(s), "mean": statistics.fmean(s), "p50": q(0.5), "p95": q(0.95), "p99": q(0.99), "max": s[-1]}


class Strata:
    """strata-server over HTTP; `edge_table`: the citations are rows of an
    edge table `links` rather than an EDGES field `cites` of `papers`."""

    def __init__(self, host, port, token, edge_table):
        import httpx

        self.http = httpx.Client(base_url=f"http://{host}:{port}", headers={"authorization": f"Bearer {token}"}, timeout=600)
        self.f = "links" if edge_table else "cites"
        self.edge_table = edge_table

    def sql(self, q, **params):
        r = self.http.post("/v1/sql", json={"sql": q, "params": params})
        j = r.json()
        if r.status_code != 200 or "error" in j:
            raise RuntimeError(f"{q}: {r.status_code} {j}")
        return j

    def import_rows(self, table, path):
        with open(path, "rb") as f:
            lines = f.readlines()
        step = 50_000
        for i in range(0, len(lines), step):
            r = self.http.post(f"/v1/import?table={table}&format=jsonl", content=b"".join(lines[i : i + step]), headers={"content-type": "application/x-ndjson"})
            if r.status_code != 200:
                raise RuntimeError(f"import {table}: {r.status_code} {r.text[:500]}")

    def load(self, data):
        t = {}
        t0 = time.perf_counter()
        if self.edge_table:
            self.import_rows("papers", data / "papers-ids.ndjson")
            t["nodes_s"] = time.perf_counter() - t0
            t1 = time.perf_counter()
            self.sql("CREATE TABLE links (id KEYWORD PRIMARY KEY, src INT NOT NULL REFERENCES papers, dst INT NOT NULL REFERENCES papers) OPTIONS(edges = 'src, dst')")
            self.import_rows("links", data / "links.ndjson")
            t["edges_s"] = time.perf_counter() - t1
        else:
            self.import_rows("papers", data / "papers.ndjson")
        t["total_s"] = time.perf_counter() - t0
        t["nodes"] = self.one("SELECT COUNT(*) FROM papers")
        if self.edge_table:
            t["edges"] = self.one("SELECT COUNT(*) FROM links")
        return t

    def one(self, q, **p):
        return self.sql(q, **p)["rows"][0][0]

    def ping(self):
        self.http.get("/v1/health").raise_for_status()

    def took(self, j):
        return j.get("took_ms")

    def hops(self, s, d, k):
        j = self.sql(f"SELECT COUNT(*) FROM papers WHERE REACHABLE({self.f}, :s, hops => {k}, dir => '{d}', max_nodes => 1000000)", s=s)
        return j["rows"][0][0], self.took(j)

    def path(self, a, b, max_hops):
        j = self.sql(f"SELECT id FROM papers WHERE PATH({self.f}, :a, :b, dir => 'both', max_hops => {max_hops}, max_nodes => 1000000) ORDER BY hops()", a=a, b=b)
        n = len(j["rows"])
        return (n - 1 if n else None), self.took(j)

    def ppr(self, s, eps):
        j = self.sql(f"SELECT id FROM papers ORDER BY ppr({self.f}, :s, eps => {eps}) DESC LIMIT 20", s=s)
        return [r[0] for r in j["rows"]], self.took(j)


class StrataGrpc(Strata):
    """The same queries over strata-server's gRPC `Query` (SQL and params,
    protobuf rows); loading still goes through HTTP's /v1/import. Needs
    grpcio and the stubs generated from strata-server/proto/strata.proto."""

    def __init__(self, host, port, grpc_port, token, edge_table):
        import grpc
        import strata_pb2
        import strata_pb2_grpc

        super().__init__(host, port, token, edge_table)
        self.pb = strata_pb2
        self.stub = strata_pb2_grpc.StrataStub(grpc.insecure_channel(f"{host}:{grpc_port}"))
        self.md = (("authorization", f"Bearer {token}"),)

    def query(self, q, **params):
        req = self.pb.QueryRequest(sql=q, params={k: self.pb.Value(int=v) for k, v in params.items()})
        r = self.stub.Query(req, metadata=self.md, timeout=600)
        return [[v.int for v in row.v] for row in r.rows], r.took_ms

    def hops(self, s, d, k):
        rows, took = self.query(f"SELECT COUNT(*) FROM papers WHERE REACHABLE({self.f}, :s, hops => {k}, dir => '{d}', max_nodes => 1000000)", s=s)
        return rows[0][0], took

    def path(self, a, b, max_hops):
        rows, took = self.query(f"SELECT id FROM papers WHERE PATH({self.f}, :a, :b, dir => 'both', max_hops => {max_hops}, max_nodes => 1000000) ORDER BY hops()", a=a, b=b)
        return (len(rows) - 1 if rows else None), took

    def ppr(self, s, eps):
        rows, took = self.query(f"SELECT id FROM papers ORDER BY ppr({self.f}, :s, eps => {eps}) DESC LIMIT 20", s=s)
        return [r[0] for r in rows], took


class Bolt:
    """Neo4j or Memgraph over Bolt, with the neo4j driver."""

    def __init__(self, host, port, memgraph):
        import neo4j

        self.drv = neo4j.GraphDatabase.driver(f"bolt://{host}:{port}", auth=None if not memgraph else ("", ""))
        self.mg = memgraph
        self.s = self.drv.session()

    def run(self, q, **p):
        r = self.s.run(q, **p)
        rows = [rec.values() for rec in r]
        summ = r.consume()
        if self.mg:
            md = summ.metadata
            took = 1000 * sum(md.get(k, 0) for k in ("parsing_time", "planning_time", "plan_execution_time")) if "plan_execution_time" in md else None
        else:
            a, c = summ.result_available_after, summ.result_consumed_after
            took = a + c if a is not None and c is not None else None
        return rows, took

    def ping(self):
        self.run("RETURN 1")

    def load(self, data):
        t = {}
        ids = list(range(json.load(open(data / "queries.json"))["nodes"]))
        edges = [tuple(map(int, l.split(","))) for l in open(data / "edges.csv").read().splitlines()[1:]]
        t0 = time.perf_counter()
        if self.mg:
            self.run("CREATE INDEX ON :Paper(id)")
        else:
            self.run("CREATE CONSTRAINT paper_id IF NOT EXISTS FOR (p:Paper) REQUIRE p.id IS UNIQUE")
            self.run("CALL db.awaitIndexes(300)")
        for i in range(0, len(ids), BATCH):
            self.run("UNWIND $ids AS i CREATE (:Paper {id: i})", ids=ids[i : i + BATCH])
        t["nodes_s"] = time.perf_counter() - t0
        t1 = time.perf_counter()
        for i in range(0, len(edges), BATCH):
            self.run("UNWIND $rows AS r MATCH (a:Paper {id: r[0]}) MATCH (b:Paper {id: r[1]}) CREATE (a)-[:CITES]->(b)", rows=[list(e) for e in edges[i : i + BATCH]])
        t["edges_s"] = time.perf_counter() - t1
        t["total_s"] = time.perf_counter() - t0
        t["nodes"] = self.run("MATCH (p:Paper) RETURN count(p)")[0][0][0]
        t["edges"] = self.run("MATCH ()-[r:CITES]->() RETURN count(r)")[0][0][0]
        if not self.mg:
            t2 = time.perf_counter()
            self.run("CALL gds.graph.project('g', 'Paper', {CITES: {orientation: 'UNDIRECTED'}})")
            t["gds_project_s"] = time.perf_counter() - t2
        return t

    def hops(self, s, d, k):
        arrow = "->" if d == "out" else "-"
        if self.mg:
            q = f"MATCH (s:Paper {{id: $s}})-[:CITES *BFS ..{k}]{arrow}(n) WHERE n <> s RETURN count(DISTINCT n)"
        else:
            q = f"MATCH (s:Paper {{id: $s}})-[:CITES*1..{k}]{arrow}(n) WHERE n <> s RETURN count(DISTINCT n)"
        rows, took = self.run(q, s=s)
        return rows[0][0], took

    def path(self, a, b, max_hops):
        if a == b:
            return 0, 0.0
        if self.mg:
            # Both ends bound first: the planner then searches from both
            # (STShortestPath); with b matched inside the pattern it runs a
            # one-sided BFS and filters at the end (~250 ms here).
            q = f"MATCH (a:Paper {{id: $a}}), (b:Paper {{id: $b}}) WITH a, b MATCH p = (a)-[:CITES *BFS ..{max_hops}]-(b) RETURN size(relationships(p))"
        else:
            q = f"MATCH (a:Paper {{id: $a}}), (b:Paper {{id: $b}}) MATCH p = shortestPath((a)-[:CITES*..{max_hops}]-(b)) RETURN length(p)"
        rows, took = self.run(q, a=a, b=b)
        return (rows[0][0] if rows else None), took

    def ppr(self, s, eps):
        if self.mg:
            raise NotImplementedError
        # GDS PageRank from one source node is personalized PageRank. Its
        # stopping rule is a tolerance and an iteration cap, not eps: GDS's
        # defaults stand for the coarse setting, 100 iterations to 1e-9 for
        # the fine one, which already finds the exact top 20. Progress logs
        # off: on, they write about 4,000 lines a query to the container's log.
        if eps < 1e-5:
            raise NotImplementedError
        q = (
            "MATCH (s:Paper {id: $s}) CALL gds.pageRank.stream('g', {sourceNodes: [s], dampingFactor: 0.85, maxIterations: $it, tolerance: $tol, logProgress: false}) "
            "YIELD nodeId, score WITH nodeId, score ORDER BY score DESC LIMIT 20 RETURN gds.util.asNode(nodeId).id"
        )
        rows, took = self.run(q, s=s, it=20 if eps >= 1e-4 else 100, tol=1e-7 if eps >= 1e-4 else 1e-9)
        return [r[0] for r in rows], took


class Falkor:
    """FalkorDB over RESP (GRAPH.QUERY), with redis-py."""

    def __init__(self, host, port):
        import redis

        self.r = redis.Redis(host=host, port=port, socket_timeout=900)

    def ping(self):
        self.r.ping()

    def run(self, q, **p):
        pre = "CYPHER " + " ".join(f"{k}={json.dumps(v)}" for k, v in p.items()) + " " if p else ""
        res = self.r.execute_command("GRAPH.QUERY", "g", pre + q, "--compact")
        rows = res[1] if len(res) == 3 else []
        took = None
        for s in res[-1]:
            s = s.decode() if isinstance(s, bytes) else s
            if s.startswith("Query internal execution time"):
                took = float(s.split(":")[1].split()[0])
        # compact rows: [[type, value], ...]
        return [[c[1] for c in row] for row in rows], took

    def load(self, data):
        t = {}
        ids = list(range(json.load(open(data / "queries.json"))["nodes"]))
        edges = [list(map(int, l.split(","))) for l in open(data / "edges.csv").read().splitlines()[1:]]
        t0 = time.perf_counter()
        self.run("CREATE INDEX FOR (p:Paper) ON (p.id)")
        for i in range(0, len(ids), BATCH):
            self.run("UNWIND $ids AS i CREATE (:Paper {id: i})", ids=ids[i : i + BATCH])
        t["nodes_s"] = time.perf_counter() - t0
        t1 = time.perf_counter()
        for i in range(0, len(edges), BATCH):
            self.run("UNWIND $rows AS r MATCH (a:Paper {id: r[0]}) MATCH (b:Paper {id: r[1]}) CREATE (a)-[:CITES]->(b)", rows=edges[i : i + BATCH])
        t["edges_s"] = time.perf_counter() - t1
        t["total_s"] = time.perf_counter() - t0
        t["nodes"] = self.run("MATCH (p:Paper) RETURN count(p)")[0][0][0]
        t["edges"] = self.run("MATCH ()-[r:CITES]->() RETURN count(r)")[0][0][0]
        return t

    def hops(self, s, d, k):
        arrow = "->" if d == "out" else "-"
        rows, took = self.run(f"MATCH (s:Paper {{id: $s}})-[:CITES*1..{k}]{arrow}(n) WHERE n <> s RETURN count(DISTINCT n)", s=s)
        return rows[0][0], took

    def path(self, a, b, max_hops):
        if a == b:
            return 0, 0.0
        # shortestPath() is directed only in FalkorDB; algo.SPpaths takes
        # relDirection 'both'.
        rows, took = self.run(
            "MATCH (a:Paper {id: $a}), (b:Paper {id: $b}) CALL algo.SPpaths({sourceNode: a, targetNode: b, relTypes: ['CITES'], "
            f"relDirection: 'both', maxLen: {max_hops}, pathCount: 1}}) YIELD path RETURN length(path)",
            a=a,
            b=b,
        )
        return (rows[0][0] if rows else None), took

    def ppr(self, s, eps):
        raise NotImplementedError


class Arango:
    """ArangoDB over its HTTP API: papers documents and a cites edge collection."""

    def __init__(self, host, port):
        import httpx

        self.http = httpx.Client(base_url=f"http://{host}:{port}/_db/_system", timeout=600)

    def ping(self):
        self.http.get("/_api/version").raise_for_status()

    def aql(self, q, **p):
        r = self.http.post("/_api/cursor", json={"query": q, "bindVars": p, "batchSize": 1000})
        j = r.json()
        if r.status_code >= 300 or j.get("error"):
            raise RuntimeError(f"{q}: {r.status_code} {j}")
        rows = j["result"]
        while j.get("hasMore"):
            j = self.http.post(f"/_api/cursor/{j['id']}").json()
            rows += j["result"]
        return rows, 1000 * j.get("extra", {}).get("stats", {}).get("executionTime", float("nan"))

    def import_lines(self, coll, lines):
        for i in range(0, len(lines), 50_000):
            r = self.http.post(f"/_api/import?collection={coll}&type=documents", content="\n".join(lines[i : i + 50_000]))
            j = r.json()
            if r.status_code >= 300 or j.get("errors"):
                raise RuntimeError(f"import {coll}: {r.status_code} {str(j)[:500]}")

    def load(self, data):
        t = {}
        n = json.load(open(data / "queries.json"))["nodes"]
        edges = [l.split(",") for l in open(data / "edges.csv").read().splitlines()[1:]]
        t0 = time.perf_counter()
        self.http.post("/_api/collection", json={"name": "papers"}).raise_for_status()
        self.import_lines("papers", [f'{{"_key":"{i}"}}' for i in range(n)])
        t["nodes_s"] = time.perf_counter() - t0
        t1 = time.perf_counter()
        self.http.post("/_api/collection", json={"name": "cites", "type": 3}).raise_for_status()
        self.import_lines("cites", [f'{{"_from":"papers/{s}","_to":"papers/{d}"}}' for s, d in edges])
        t["edges_s"] = time.perf_counter() - t1
        t["total_s"] = time.perf_counter() - t0
        t["nodes"] = self.aql("RETURN LENGTH(papers)")[0][0]
        t["edges"] = self.aql("RETURN LENGTH(cites)")[0][0]
        return t

    def hops(self, s, d, k):
        way = "OUTBOUND" if d == "out" else "ANY"
        rows, took = self.aql(
            f"FOR v IN 1..{k} {way} @s cites OPTIONS {{order: 'bfs', uniqueVertices: 'global'}} COLLECT WITH COUNT INTO c RETURN c", s=f"papers/{s}"
        )
        return rows[0], took

    def path(self, a, b, max_hops):
        # SHORTEST_PATH has no depth limit: a longer path counts as none.
        rows, took = self.aql("FOR v IN ANY SHORTEST_PATH @a TO @b cites RETURN 1", a=f"papers/{a}", b=f"papers/{b}")
        return (len(rows) - 1 if rows and len(rows) - 1 <= max_hops else None), took

    def ppr(self, s, eps):
        raise NotImplementedError


def timed(name, fn, args, check):
    """Runs fn over args: WARM untimed, then all timed; check(i, answer) is
    True when answer i is right."""
    t_warm = time.perf_counter()
    for a in args[:WARM]:
        if time.perf_counter() - t_warm > 60:
            break
        try:
            fn(*a)
        except NotImplementedError:
            raise
        except Exception:
            pass
    wall, server, wrong, t_start = [], [], [], time.perf_counter()
    for i, a in enumerate(args):
        t0 = time.perf_counter()
        try:
            ans, took = fn(*a)
        except NotImplementedError:
            raise
        except Exception as x:  # counted wrong, timed to the failure
            ans, took = f"error: {str(x)[:200]}", None
        wall.append(1000 * (time.perf_counter() - t0))
        if took is not None:
            server.append(took)
        ok = check(i, ans)
        if ok is not True:
            wrong.append({"i": i, "args": a, "got": ans, **({} if ok is False else ok)})
        if time.perf_counter() - t_start > KIND_BUDGET_S:
            print(f"{name}: stopped after {i + 1} queries ({KIND_BUDGET_S} s budget)", file=sys.stderr)
            break
    row = {"client_ms": stats(wall), "server_ms": stats(server), "wrong": len(wrong), "wrong_examples": wrong[:5], "done": len(wall), "asked": len(args)}
    print(f"{name}: p50 {row['client_ms']['p50']:.3f} ms, p95 {row['client_ms']['p95']:.3f} ms, wrong {len(wrong)}/{len(wall)}", file=sys.stderr)
    return row


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("engine", choices=["strata", "strata-edges", "strata-grpc", "strata-edges-grpc", "neo4j", "memgraph", "falkordb", "arangodb"])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--grpc-port", type=int, default=7701, help="strata-grpc engines: the gRPC port (queries); --port is HTTP (loading)")
    ap.add_argument("--token", default="")
    ap.add_argument("--data", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--skip", default="", help="comma-separated query kinds to leave out")
    a = ap.parse_args()
    data = Path(a.data)
    q = json.load(open(data / "queries.json"))
    ref = json.load(open(data / "reference.json"))
    skip = set(filter(None, a.skip.split(",")))

    e = {
        "strata": lambda: Strata(a.host, a.port, a.token, False),
        "strata-edges": lambda: Strata(a.host, a.port, a.token, True),
        "strata-grpc": lambda: StrataGrpc(a.host, a.port, a.grpc_port, a.token, False),
        "strata-edges-grpc": lambda: StrataGrpc(a.host, a.port, a.grpc_port, a.token, True),
        "neo4j": lambda: Bolt(a.host, a.port, False),
        "memgraph": lambda: Bolt(a.host, a.port, True),
        "falkordb": lambda: Falkor(a.host, a.port),
        "arangodb": lambda: Arango(a.host, a.port),
    }[a.engine]()
    t0 = time.perf_counter()
    while True:  # the engine may still be starting
        try:
            e.ping()
            break
        except Exception:
            if time.perf_counter() - t0 > 300:
                raise
            time.sleep(1)

    out = {"engine": a.engine, "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    print(f"{a.engine}: loading", file=sys.stderr)
    out["load"] = e.load(data)
    print(f"{a.engine}: load {out['load']}", file=sys.stderr)
    assert out["load"]["nodes"] == q["nodes"], out["load"]
    assert out["load"].get("edges", q["edges"]) == q["edges"], out["load"]
    res = {}
    for name, d, k in HOPS:
        if f"hops_{name}" in skip:
            continue
        want = ref["hops"][name]
        res[f"hops_{name}"] = timed(f"hops_{name}", lambda s, d=d, k=k: e.hops(s, d, k), [(s,) for s in q["seeds"]], lambda i, x, w=want: x == w[i] or {"want": w[i]})
    if "path" not in skip:
        res["path"] = timed("path", lambda x, y: e.path(x, y, q["max_path"]), [tuple(p) for p in q["pairs"]], lambda i, x: x == ref["path_len"][i] or {"want": ref["path_len"][i]})
    for eps in PPR_EPS:
        name = f"ppr_{eps:g}"
        if name in skip:
            continue
        recalls = []

        def check(i, got):
            top = set(ref["ppr"][i]["ids"][:20])
            recalls.append(len(top & set(got)) / 20)
            return True

        try:
            row = timed(name, lambda s, eps=eps: e.ppr(s, eps), [(s,) for s in q["seeds"]], check)
        except NotImplementedError:
            res[name] = {"unavailable": True}
            continue
        row["recall_at_20"] = statistics.fmean(recalls[-row["done"] :])
        res[name] = row
    out["queries"] = res
    out["finished"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    Path(a.out).write_text(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
