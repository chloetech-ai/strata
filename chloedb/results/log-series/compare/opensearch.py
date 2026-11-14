#!/usr/bin/env python3
"""OpenSearch 3.3's side: DATA/NAME.shifted.jsonl (shift.py) bulk-loaded
as ../../log-bytes/opensearch.py loads (5,000 lines a request, 4
threads, refresh off), into one shard, no replicas, ts a date_nanos (so
microseconds are kept), host, service and severity keywords, latency a
long, body text; refreshed and force-merged to one segment. Then the
harness's queries as date_histogram, terms and percentiles aggregations,
with the request cache off.

Container (opensearch-min is not on Docker Hub; this is the full image
with its security plugin off):
  docker run -d --name ls-os -p 9200:9200 -v ls-os:/usr/share/opensearch/data \
    -e discovery.type=single-node -e OPENSEARCH_JAVA_OPTS="-Xms4g -Xmx4g" \
    -e DISABLE_SECURITY_PLUGIN=true -e DISABLE_INSTALL_DEMO_CONFIG=true opensearchproject/opensearch:3.3.0
opensearch.py DATA NAME...   (appends to ../mac-others.jsonl)
"""
import http.client
import json
import os
import queue
import sys
import threading
import time

import common as c

HOST, PORT = "127.0.0.1", 9200
BATCH, THREADS = 5000, 4


def call(method, path, body=None, conn=None):
    cn = conn or http.client.HTTPConnection(HOST, PORT, timeout=7200)
    data = body if isinstance(body, (bytes, type(None))) else json.dumps(body).encode()
    cn.request(method, path, body=data, headers={"content-type": "application/x-ndjson" if path.endswith("_bulk") else "application/json"})
    r = cn.getresponse()
    out = json.loads(r.read() or b"null")
    if r.status >= 300:
        raise SystemExit(f"{method} {path}: {r.status} {str(out)[:800]}")
    return out


MAPPINGS = {"dynamic": "strict", "properties": {
    "ts": {"type": "date_nanos"}, "host": {"type": "keyword"}, "service": {"type": "keyword"},
    "severity": {"type": "keyword"}, "pid": {"type": "long"}, "latency": {"type": "long"}, "body": {"type": "text"}}}


def load(index, path):
    q = queue.Queue(maxsize=THREADS * 2)
    errors = []

    def worker():
        conn = http.client.HTTPConnection(HOST, PORT, timeout=7200)
        while (body := q.get()) is not None:
            r = call("POST", f"/{index}/_bulk", body, conn)
            if r.get("errors"):
                errors.append(next(i for i in r["items"] if "error" in i["index"]))
        conn.close()

    threads = [threading.Thread(target=worker) for _ in range(THREADS)]
    for t in threads:
        t.start()
    action = b'{"index":{}}\n'
    lines, buf = 0, []
    with open(path, "rb") as f:
        for line in f:
            # Drop JSON nulls (pid in BGL), which a strict mapping takes but
            # which are no field at all.
            if b":null" in line:
                r = json.loads(line)
                line = json.dumps({k: v for k, v in r.items() if v is not None}).encode() + b"\n"
            buf.append(action)
            buf.append(line)
            lines += 1
            if lines % BATCH == 0:
                q.put(b"".join(buf))
                buf = []
    if buf:
        q.put(b"".join(buf))
    for _ in threads:
        q.put(None)
    for t in threads:
        t.join()
    if errors:
        raise SystemExit(f"bulk errors, first: {errors[0]}")
    return lines


def search(index, body):
    return call("POST", f"/{index}/_search?request_cache=false", body)


def run(data, name, out):
    index = "logs-" + name
    call("DELETE", f"/{index}?ignore_unavailable=true")
    call("PUT", f"/{index}", {"settings": {"number_of_shards": 1, "number_of_replicas": 0, "refresh_interval": "-1"}, "mappings": MAPPINGS})
    t0 = time.time()
    lines = load(index, os.path.join(data, name + ".shifted.jsonl"))
    call("POST", f"/{index}/_refresh")
    load_s = time.time() - t0
    t0 = time.time()
    call("POST", f"/{index}/_forcemerge?max_num_segments=1")
    call("POST", f"/{index}/_refresh")
    call("POST", f"/{index}/_flush")
    merge_s = time.time() - t0
    # The merged-away segments' files go some time after the merge: read
    # the store size until it has not changed for 10 s.
    sizes = []
    while len(sizes) < 10 or len(set(sizes[-10:])) > 1:
        time.sleep(1)
        st = call("GET", f"/{index}/_stats/store,docs,segments")["indices"][index]["primaries"]
        sizes.append(st["store"]["size_in_bytes"])
    assert st["docs"]["count"] == lines, st["docs"]
    version = call("GET", "/")["version"]["number"]
    r = {"mode": "load", "store": "opensearch", "input": name + ".jsonl", "lines": lines, "seconds": round(load_s, 1),
         "lines_per_second": round(lines / load_s), "force_merge_seconds": round(merge_s, 1),
         "segments": st["segments"]["count"], "bytes_on_disk": st["store"]["size_in_bytes"], "version": version, "heap": "4g"}
    print(json.dumps(r), flush=True)
    out.write(json.dumps(r) + "\n")

    new = c.newest(name)
    rng = lambda w: {"range": {"ts": {"gte": c.iso(new - w)}}}
    filt = lambda w, *more: {"bool": {"filter": [rng(w), *more]}}
    ns = lambda b: int(b["key"]) * 1000  # date_nanos keys are epoch millis
    total = lambda res: res["hits"]["total"]["value"]
    hist = lambda iv: {"date_histogram": {"field": "ts", "fixed_interval": iv, "min_doc_count": 1}}
    qs = [
        ("count_last_hour", {"size": 0, "track_total_hits": True, "query": filt(c.HOUR)}, lambda r: total(r), lambda r: 1),
        ("count_last_day", {"size": 0, "track_total_hits": True, "query": filt(c.DAY)}, lambda r: total(r), lambda r: 1),
        ("count_all", {"size": 0, "track_total_hits": True, "query": {"match_all": {}}}, lambda r: total(r), lambda r: 1),
        ("hourly_7d", {"size": 0, "query": filt(7 * c.DAY), "aggs": {"h": hist("1h")}},
         lambda r: c.digest([[ns(b), b["doc_count"]] for b in r["aggregations"]["h"]["buckets"]]),
         lambda r: len(r["aggregations"]["h"]["buckets"])),
        ("minutely_by_severity_24h", {"size": 0, "query": filt(c.DAY), "aggs": {"m": {**hist("1m"), "aggs": {"s": {"terms": {"field": "severity", "size": 100}}}}}},
         lambda r: c.digest([[ns(b), s["key"], s["doc_count"]] for b in r["aggregations"]["m"]["buckets"] for s in b["s"]["buckets"]]),
         lambda r: sum(len(b["s"]["buckets"]) for b in r["aggregations"]["m"]["buckets"])),
        ("errors_top_hosts_24h", {"size": 0, "query": filt(c.DAY, {"term": {"severity": "ERROR"}}), "aggs": {"h": {"terms": {"field": "host", "size": 10}}}},
         lambda r: [b["doc_count"] for b in r["aggregations"]["h"]["buckets"]], lambda r: len(r["aggregations"]["h"]["buckets"])),
        ("match_in_hour_newest", {"size": 100, "track_total_hits": False, "_source": ["ts", "host", "body"], "sort": [{"ts": "desc"}],
                                  "query": filt(c.HOUR, {"match": {"body": "error"}})},
         lambda r: [c.parse_ts(r["hits"]["hits"][0]["_source"]["ts"]), c.parse_ts(r["hits"]["hits"][-1]["_source"]["ts"])] if r["hits"]["hits"] else [],
         lambda r: len(r["hits"]["hits"])),
    ]
    if name == "syn20m":
        qs += [
            ("p99_hourly_by_service_24h", {"size": 0, "query": filt(c.DAY), "aggs": {"h": {**hist("1h"), "aggs": {"s": {"terms": {"field": "service", "size": 100},
                "aggs": {"p": {"percentiles": {"field": "latency", "percents": [99]}}}}}}}},
             lambda r: c.digest([[ns(b), s["key"], s["p"]["values"]["99.0"]] for b in r["aggregations"]["h"]["buckets"] for s in b["s"]["buckets"]]),
             lambda r: sum(len(b["s"]["buckets"]) for b in r["aggregations"]["h"]["buckets"])),
            ("p50_p95_p99_24h", {"size": 0, "query": filt(c.DAY), "aggs": {"p": {"percentiles": {"field": "latency", "percents": [50, 95, 99]}}}},
             lambda r: [r["aggregations"]["p"]["values"][k] for k in ("50.0", "95.0", "99.0")], lambda r: 1),
        ]
    for q, body, val, nrows in qs:
        med, p95, cold, res = c.timed(lambda: search(index, body))
        extra = {"took_ms_last": res.get("took")}
        if q.startswith("p"):
            extra["percentile"] = "percentiles aggregation, default t-digest (compression 100; approximate)"
        if q == "p99_hourly_by_service_24h":
            extra["groups"] = sorted([ns(b), s["key"], s["p"]["values"]["99.0"]] for b in res["aggregations"]["h"]["buckets"] for s in b["s"]["buckets"])
        if q == "match_in_hour_newest":
            extra["matches_in_window"] = call("POST", f"/{index}/_count", {"query": filt(c.HOUR, {"match": {"body": "error"}})})["count"]
        c.emit(out, "opensearch", name, lines, q, json.dumps(body), med, p95, cold, nrows(res), val(res), **extra)
    call("DELETE", f"/{index}")


if __name__ == "__main__":
    with open(os.path.join(c.RESULTS, "mac-others.jsonl"), "a") as out:
        for n in sys.argv[2:]:
            run(sys.argv[1], n, out)
