# Build an app on chloedb-server

A shop in one page: search, stock that never oversells, holds that expire, and click events counted exactly once. Each part is a few lines of SQL or JSON over HTTP, with links to the reference for the rest.

## Run a server

Download the server for your machine (no Rust or build needed), and start it on a new database:

```sh
v=0.3.3 p=linux-x86_64   # or linux-aarch64, macos-arm64, macos-x86_64
curl -LO https://github.com/chloetech-ai/chloedb/releases/download/chloedb-server-v$v/chloedb-server-$p.tar.gz
tar xzf chloedb-server-$p.tar.gz && cp chloedb-server-$p/chloedb-server ./chloedb-server
./chloedb-server --db shop.chloedb --seq-no
```

The server listens on `http://127.0.0.1:7700` (HTTP JSON, and the command center in a browser) and on 7701 (gRPC). A new `--db` starts with no tables. `--seq-no` numbers every write, which the stock below needs. [Run it](server.md#run-it) has the options, the source build (`cd chloedb-server && cargo build --release`) and the image.

## Make the tables

`POST /v1/sql` takes one statement:

```sh
sql() { curl -s localhost:7700/v1/sql -H 'content-type: application/json' -d "$(jq -n --arg s "$1" '{sql: $s}')"; echo; }

sql "CREATE TABLE listings (id KEYWORD PRIMARY KEY, title TEXT, body TEXT, category KEYWORD,
     price FLOAT, stock INT DEFAULT 0,
     emb VECTOR OPTIONS(embed = 'title, body', model = 'potion-base-8M'))"
sql "CREATE TABLE holds (id KEYWORD PRIMARY KEY, listing KEYWORD NOT NULL, buyer KEYWORD NOT NULL,
     until TIMESTAMP) OPTIONS(expire_by = 'until')"
sql "CREATE TABLE buyers (id KEYWORD PRIMARY KEY, views INT DEFAULT 0)"
sql "CREATE TABLE events (id INT PRIMARY KEY, buyer KEYWORD NOT NULL, listing KEYWORD, kind KEYWORD,
     ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP) OPTIONS(shard_by = 'buyer')"
```

- `TEXT` is searched by words, `KEYWORD` is matched whole (filters, facets), and every column is indexed.
- `emb` is computed by the server from the title and body when a row is written, by a small model (potion-base-8M, 30 MB) that it downloads from Hugging Face when the table is created. Rows written before the model is ready get their vectors once it is; `GET /v1/schema` shows it `"loaded": true`.
- `expire_by` makes a hold vanish at its `until`. `shard_by` keeps a buyer's events on one shard [once there are several](#grow-to-a-cluster).

The [query language](query-language.md#tables) has every type and option.

## Write rows

`POST /v1/write` applies a list of operations in one transaction:

```sh
curl -s localhost:7700/v1/write -H 'content-type: application/json' -d '{"ops": [
  {"op": "upsert", "table": "listings", "doc": {"id": "l1", "title": "Red camping tent", "body": "Two-person tent, light and waterproof", "category": "outdoor", "price": 89.0, "stock": 3}},
  {"op": "upsert", "table": "listings", "doc": {"id": "l2", "title": "Blue desk lamp", "body": "LED lamp with a dimmer", "category": "home", "price": 24.5, "stock": 10}},
  {"op": "upsert", "table": "listings", "doc": {"id": "l3", "title": "Trail running shoes", "body": "Light shoes for wet trails", "category": "outdoor", "price": 120.0, "stock": 0}}
]}'
# {"written":3,"seq_no":5,...}
```

`insert` fails when the key exists, `upsert` replaces the row, `set` changes one column, `add` adds to a number (and creates the row with `doc` when it is missing), and `delete` removes a row by `key`. A 200 answer means the rows are durable. Files load through [`/v1/import`](server.md#loading-rows) (JSONL, CSV, Parquet, SQLite).

## Search

```sh
q() { curl -s localhost:7700/v1/query -H 'content-type: application/json' -d "$1"; echo; }

# words, ranked by BM25, in stock, cheapest first among equals
q '{"sql": "SELECT id, title, price FROM listings WHERE MATCH(title, :q) AND stock > 0 ORDER BY bm25(title, :q) DESC, price LIMIT 10",
    "params": {"q": "tent"}, "objects": true}'
# {"columns":["id","title","price"],"rows":[{"id":"l1","price":89.0,"title":"Red camping tent"}],...}

# meaning, not words: the vector of the query text by the field's model, fused with BM25
q '{"sql": "SELECT id, title FROM listings ORDER BY rrf(bm25(body, :t), similarity(emb, :v)) DESC LIMIT 10",
    "params": {"t": "shelter for two", "v": {"embed": "shelter for two"}}, "objects": true}'

# facets: how many listings per category
q '{"sql": "SELECT category, COUNT(*) FROM listings GROUP BY category", "objects": true}'
```

Rows come as lists in the order of `columns`, or as objects with `"objects": true`. Typo tolerance, synonyms, phrase and prefix search, geographic and time decay, and reranking by a model are in the [query language](query-language.md).

## Read one row by its key

```sh
curl -s localhost:7700/v1/kv/listings/l1
# {"found":true,"key":"l1","row":{"id":"l1",...,"stock":3},"seq_no":5,"table":"listings",...}
```

No SQL is parsed: a read by key costs about what a key-value store's does ([rows by key](server.md#rows-by-key)).

## Sell stock without overselling

Read the row with its `seq_no`, then write it back only if nobody wrote it since (`if_seq_no`). Of several buyers racing for the last item, exactly one write applies; the others get 409 and read again.

```python
import json, urllib.request, urllib.error

def call(method, path, body=None):
    req = urllib.request.Request("http://127.0.0.1:7700" + path, method=method,
        data=None if body is None else json.dumps(body).encode(), headers={"content-type": "application/json"})
    with urllib.request.urlopen(req) as r:
        return json.loads(r.read())

def buy(listing):
    while True:
        got = call("GET", f"/v1/kv/listings/{listing}")
        if got["row"]["stock"] <= 0:
            return False                                   # sold out
        op = {"op": "set", "table": "listings", "key": listing, "field": "stock",
              "value": got["row"]["stock"] - 1, "if_seq_no": got["seq_no"]}
        try:
            call("POST", "/v1/write", {"ops": [op]})
            return True
        except urllib.error.HTTPError as e:
            if e.code != 409:                              # 409: someone bought meanwhile
                raise
```

The Python package's client has the same calls (`chloedb.connect(url).get("l1", table="listings")`, `.write(ops)`); it needs only the standard library ([Python](python.md#a-server-from-python)). More in [conditional writes](server.md#conditional-writes).

## Hold an item for ten minutes

```sh
until=$(python3 -c 'import datetime as d; print((d.datetime.now(d.timezone.utc) + d.timedelta(minutes=10)).isoformat())')
curl -s localhost:7700/v1/write -H 'content-type: application/json' \
  -d '{"ops": [{"op": "insert", "table": "holds", "doc": {"id": "h1", "listing": "l1", "buyer": "b7", "until": "'"$until"'"}}]}'
```

From `until` on, the hold is gone from every query, count and read by key, with nothing to clean up. `SELECT COUNT(*) FROM holds WHERE listing = 'l1'` is the number of live holds.

## Count events exactly once

Clicks and views come from a log (Kafka, a queue, a file). Send them to `POST /v1/stream` as NDJSON, each with its place in the log (`source`): the server keeps each source's last offset in the same transaction as the rows, so an event sent twice, after a crash or a retry, is skipped rather than counted again.

```sh
printf '%s\n' \
  '{"source": {"name": "clicks/0", "offset": 0}, "ops": [{"op": "insert", "table": "events", "doc": {"id": 1, "buyer": "b7", "listing": "l1", "kind": "view"}}, {"op": "add", "table": "buyers", "key": "b7", "field": "views", "value": 1, "doc": {"id": "b7"}}]}' \
  '{"source": {"name": "clicks/0", "offset": 1}, "ops": [{"op": "insert", "table": "events", "doc": {"id": 2, "buyer": "b8", "listing": "l2", "kind": "view"}}, {"op": "add", "table": "buyers", "key": "b8", "field": "views", "value": 1, "doc": {"id": "b8"}}]}' |
curl -s localhost:7700/v1/stream -H 'content-type: application/x-ndjson' --data-binary @-
# {"batch":1,...,"written":4,"skipped":0,...}

curl -s localhost:7700/v1/sources/clicks/0
# {"name":"clicks/0","offset":1,...}: resume from offset 2
```

Send the same lines again and the answer says `"written":0,"skipped":4`. [Streams](server.md#streams) has Kafka consumers and the change feed.

## When a write fails

An error means nothing of the write was applied, with one exception on a cluster (the last below):

- **429**: the server refused the request for its load before doing anything (it is behind on flushing writes to disk, or its write queue is full). The [Python](python.md#a-server-from-python) and [JavaScript](javascript.md#a-server-from-javascript) clients send it again after the answer's `Retry-After`, for up to 10 seconds, and so does a coordinator for its shards; from `curl`, wait a second and send it again.
- **A lost answer** (a timeout, a dropped connection): send the request again with the same `"idempotency_key"`. A server that applied it answers its first answer instead of applying it twice. Keys are kept for an hour.
- **409 with `if_seq_no`**: the row changed; read it again.
- **`"partial": true`**, on a cluster: some shards took their part of the write and keep it, and the others did not. The answer names each shard's part and an `idempotency_key` (yours, or one the coordinator made): send the same request again with that key, and only the shards that did not take their part apply it. [When a write fails](server.md#when-a-write-fails) has the details.

## Grow to a cluster

Several servers as shards, and one more as a coordinator, answer like one database: the same tables, SQL and HTTP API. Stop the server above, then:

```sh
./chloedb-server --db s0.chloedb --seq-no --listen 127.0.0.1:7710 --grpc 127.0.0.1:7711 &
./chloedb-server --db s1.chloedb --seq-no --listen 127.0.0.1:7720 --grpc 127.0.0.1:7721 &
./chloedb-server --shards http://127.0.0.1:7711,http://127.0.0.1:7721 --listen 127.0.0.1:7700 &
```

- Rows go to shards by their key, or by the `shard_by` column, so a buyer's events sit on one shard.
- A shard added later starts as the first ones did: here on a new database with no `--schema`, since this cluster began with no tables. The coordinator gives it the cluster's tables, and a rebalance [moves slots](server.md#moving-slots) onto it while rows keep being written:

```sh
./chloedb-server --db s2.chloedb --seq-no --listen 127.0.0.1:7730 --grpc 127.0.0.1:7731 &
curl -s localhost:7700/v1/admin/shards -H 'content-type: application/json' -d '{"servers": ["http://127.0.0.1:7731"]}'
curl -s localhost:7700/v1/admin/rebalance -X POST
```

- [Replicas](server.md#replication-a-primary-and-its-replicas) of each shard take over when its primary fails.

Through a coordinator, a few things work differently:

- **Graph queries** (`REACHABLE`, `PATH`, `ppr`) run on one database only ([graphs on a cluster](graphs.md#on-a-cluster)).
- **`INSERT ... ON CONFLICT DO NOTHING`** and **`DO UPDATE`**, and `UPDATE` or `DELETE` with a `LIMIT`, are refused. `INSERT OR REPLACE` works.
- **A write across shards** can fail on one shard after others took their part: send it again with the key its error gives ([above](#when-a-write-fails)).
