---
title: Server
parent: Strata
nav_order: 4
---

# strata-server
{: .no_toc }

`strata-server` serves a Strata database over HTTP JSON and gRPC, with a browser UI for building queries and an agent that answers questions by writing and running SQL. Run several of them as shards and one more as a coordinator, and the coordinator answers like one database.
{: .fs-5 .fw-300 }

1. TOC
{:toc}

## Run it

```sh
cd strata-server && cargo build --release
target/release/strata-server --demo 20000 --db products.strata   # a demo catalogue
# open http://127.0.0.1:7700
```

`--demo N` creates a product catalogue of *N* rows (titles, descriptions, brands, categories, tags, prices, ratings, sales and a 64-dimension vector) to try things on. Its vectors are hashed bags of words, not a learned model, so "similar" means "shares words".

| flag | default | effect |
|---|---|---|
| `--db PATH` | | the database to serve; created if missing, with `--schema` or `--demo` |
| `--schema FILE` | | the schema of a new database, as JSON (below) |
| `--directory` | off | create the new database as a directory of segment files instead of one file |
| `--shards URL,URL,…` | | run as a coordinator over these shard servers' gRPC addresses |
| `--listen ADDR` | `127.0.0.1:7700` | HTTP: the API, `/metrics` and the UI |
| `--grpc ADDR` | `127.0.0.1:7701` | gRPC (`off` to disable) |
| `--token T` | `$STRATA_TOKEN` | require `Authorization: Bearer T` on the API |
| `--timeout-ms N` | 5000 | default query deadline |
| `--max-concurrent N` | 64 | queries running at once; more wait for a slot until their deadline |
| `--model ID` | `claude-haiku-5-5` | the agent's default model |
| `--hash-embedder` | off | answer `{"embed": "text"}` parameters with hashed bag-of-words vectors |
| `--synonyms FILE` | | the synonym list for `synonyms => 1` searches ([format](query-language.md#typos-and-synonyms)): read at start if it exists, written by `PUT /v1/synonyms` |

A schema file is Strata's schema as JSON. `GET /v1/schema` returns it in the same form:

```json
{
  "table": "docs",
  "primary_key": 0,
  "fields": [
    {"name": "id", "ty": "Keyword", "stored": true, "indexed": true},
    {"name": "body", "ty": {"Text": {"positions": true}}, "stored": true, "indexed": true},
    {"name": "tags", "ty": "Keyword", "multi": true, "stored": true, "indexed": true},
    {"name": "published", "ty": "Int", "stored": true, "indexed": true},
    {"name": "emb", "ty": {"Vector": {"dim": 768, "index": {"Hnsw": {"m": 16, "ef_construction": 200}}}}, "codes": "binary", "stored": false, "indexed": true},
    {"name": "splade", "ty": "Sparse", "stored": true, "indexed": true}
  ]
}
```

The [query language](query-language.md#columns-and-types) page describes the types.

## HTTP API

Every request and answer is JSON. Parameters bind `:name` placeholders: a number, a string, an array for a vector, an object of term weights for a `SPARSE` field, `{"vector_of": "key"}` for a stored row's vector (more like this), or `{"embed": "text"}` when the server has an embedder.

| route | body | answer |
|---|---|---|
| `POST /v1/query` | `{sql, params?, timeout_ms?}` | `{columns, rows, keys, scores, stats, took_ms}` |
| `POST /v1/sql` | `{sql, params?, timeout_ms?}` | rows for a `SELECT`; `{written}` for `INSERT`, `UPDATE`, `DELETE` |
| `POST /v1/write` | `{ops: [...]}` | `{written}` |
| `POST /v1/explain` | `{sql, params?}` | the bound query and its plan lines |
| `POST /v1/term_stats` | `{sql, params?}` | BM25 statistics and a snapshot lease, for a coordinator |
| `POST /v1/snapshot` | | a snapshot lease: `{snapshot, txn, ttl_s}` |
| `POST /v1/agent` | `{question, model?, effort?, api_key?, max_steps?}` | an answer and the queries it ran |
| `GET /v1/synonyms`, `PUT /v1/synonyms` | `{text}` for `PUT` | the synonym list: `{text, rules, file}` |
| `GET /v1/health`, `/v1/stats`, `/v1/schema`, `/v1/config` | | JSON |
| `GET /metrics` | | Prometheus text |
| `GET /` | | the UI |

```sh
curl -s localhost:7700/v1/query -H 'content-type: application/json' -d '{
  "sql": "SELECT id, title, price, score() FROM products WHERE category = :c ORDER BY 0.5 * bm25(body, :t) + 0.5 * similarity(emb, :v) DESC LIMIT 5",
  "params": {"c": "kitchen", "t": "quiet espresso", "v": {"vector_of": "p0011051"}}
}'
```

`/v1/write` takes a batch of operations, applied in one transaction:

```json
{"ops": [
  {"op": "insert", "doc": {"id": "p1", "title": "red mug", "price": 12.5, "tags": ["kitchen", "gift"]}},
  {"op": "upsert", "doc": {"id": "p2", "title": "blue mug"}},
  {"op": "upsert_if_newer", "doc": {"id": "p3", "title": "green mug", "version": 7}, "version": "version"},
  {"op": "set", "key": "p1", "field": "price", "value": 11.0},
  {"op": "add", "key": "p1", "field": "sold", "value": 1},
  {"op": "delete", "key": "p9"}
]}
```

A compound query (`WITH`, a subquery, a join, `SELECT DISTINCT`) runs as several single-table queries on one snapshot; on a coordinator, each step goes to the shards. Its plan lists the steps. A coordinator has no synonym list of its own: start each shard with `--synonyms FILE`.

`set` and `add` change a numeric field in place, without rewriting the row; `upsert_if_newer` writes the row only if its version field is higher than the stored row's, so a feed replayed out of order keeps the newest.

With `--token`, every route except `/` and `/v1/health` needs `Authorization: Bearer <token>`; the UI asks for the token.

## gRPC

`proto/strata.proto` defines the service: `Query`, `TermStats`, `Write`, `Schema` and `Health`, mirroring the HTTP routes. Values are typed (`int`, `float`, `str`, `vector`, `list`); statistics, plans and fusion parts travel as JSON bytes, since they are Strata's own serde types. The server compiles the proto at build time with a pure-Rust compiler, so building needs no `protoc`.

## Sharding: one coordinator, many shards

```sh
strata-server --demo 20000 --demo-shard 0/2 --db a.strata --listen 127.0.0.1:7710 --grpc 127.0.0.1:7711
strata-server --demo 20000 --demo-shard 1/2 --db b.strata --listen 127.0.0.1:7720 --grpc 127.0.0.1:7721
strata-server --shards http://127.0.0.1:7711,http://127.0.0.1:7721 --listen 127.0.0.1:7700 --grpc off
```

A coordinator holds no data. For a query, it

1. asks every shard for the BM25 statistics of the query's terms and a snapshot lease (skipped when nothing in the query needs collection statistics: vectors, sparse fields, columns),
2. sends every shard the query with the summed statistics and its lease, in shard mode, so that each shard scores exactly as the whole collection would and returns what a merge needs (for fusions and products, each input's raw scores over its candidates),
3. merges the answers with `strata::cluster::merge`: top *k*, fusion, products and facet counts.

A sharded collection therefore ranks like one database: the tests run every query kind against three shards and one whole database and compare keys and scores. Writes are routed by primary key (a hash of the key picks the shard); `UPDATE` and `DELETE` go to every shard. Shards can also be built offline: `Db::write_segment` writes a segment file anywhere, and `attach_segments` adds built segments to a running database in one commit.

## Deadlines, admission and metrics

Every query runs under a deadline (`timeout_ms`, or the server's default) and gets its share of the cores: a query arriving when *r* are running searches its segments on about *cores / r* threads. At most `--max-concurrent` run at once; the rest wait for a slot, and one still waiting at its deadline is refused with 503 and counted in `strata_rejected_total`. A query still running at its deadline stops at the next segment and returns an error.

`GET /metrics` serves, in Prometheus text format:

| metric | labels | what |
|---|---|---|
| `strata_queries_total` | `kind`, `outcome` | queries by kind (`bm25`, `vector`, `sparse`, `hybrid`, `boosted`, `decay`, `graph`, `column`, `facet`, `count`, `filter`) and outcome |
| `strata_query_ms` | `kind` | latency histogram |
| `strata_writes_total`, `strata_written_docs_total` | | write requests and rows written |
| `strata_rejected_total` | | queries refused at admission |
| `strata_agent_runs_total` | | agent runs |
| `strata_docs`, `strata_segments` | | the database's size |

## The UI

`GET /` serves a single page with five tabs.

![The query builder: search text, similar to a description, weighted fusion, a filter, the SQL it writes, and the results](../assets/img/strata-server-builder.png)

- **Build** writes SQL from a form: search text (ranked, or only matching rows, with typo tolerance and synonyms), "similar to" a stored row or a description, rank fusion or a weighted sum with a slider, conditions, a boost by closeness (`decay`), a facet, the columns and the row count. Conditions nest in groups of *all of* (`AND`), *any of* (`OR`) and *none of* (`NOT`), and a condition can be "is among the *field* of the rows where …", an `IN (SELECT ...)` subquery. **Join related rows** pairs each row with the rows it links to, the rows linking to it (for each `EDGES` field) or the rows sharing a value, with conditions and columns for the related rows; **Summarize** groups by any fields of either side and computes counts, distinct counts, sums, averages, minimums and maximums, with `HAVING` and an order. The SQL updates as you type; "Edit as SQL" takes it to the editor. Clicking a keyword field in the sidebar shows its most common values, and clicking a value adds a condition. Each result row has a "similar" link that searches for rows like it, and a "links" link that opens its neighbourhood in the Graph tab. A search that finds nothing offers to search again allowing typos, or with synonyms.
- **Graph** (for a table with an `EDGES` or `VECTOR` field) starts from keys, the top rows of a search or the rows matching a condition, follows links (out, in or both ways, optionally one label) or similarity (each row's *k* nearest), and finds the rows within some hops, the shortest path to other rows, or the rows ranked by personalized PageRank, with conditions on the rows found. The answer is a table and a picture: rings by hop distance, a path left to right, or PageRank as node size. Clicking a node shows its row, expands its links or starts a new walk from it.
- **SQL** is an editor with parameters, results and the plan the executor recorded, segment by segment, with example queries one click away.
- **Ask an agent** takes a question in plain language.
- **Query language** is a guide to the SQL with examples written against the table's own fields and words from its rows: boolean, phrase, prefix, typo and synonym searches, conditions, aggregates and `HAVING`, subqueries and `WITH`, joins, graph queries, hybrid ranking and boosts, and what is not supported. "Try it" runs an example in the SQL tab. It also edits the server's synonym list.

![The SQL editor with a plan](../assets/img/strata-server-sql.png)

## The agent

`POST /v1/agent`, and the UI's third tab, give a Claude model one tool, `run_sql`, which runs a read-only `SELECT` on the table. The model sees the schema and a description of Strata's SQL (it runs facet queries to learn a field's values), then searches, filters, ranks, counts and facets until it can answer, for at most `max_steps` model calls (8 by default, 20 at most). The answer lists every query it ran, with its row count and time, so you can check its work or open a query in the editor. Writes are refused.

The model is a setting: `--model` sets the server's default (`claude-haiku-5-5`), and a request or the UI can name any other model id and an effort level. The API key comes from `ANTHROPIC_API_KEY` on the server, or from the request (`api_key`, never stored by the server; the UI keeps it in the browser only if you tick "remember").
