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

`--demo N` creates a product catalogue of *N* rows (titles, descriptions, brands, categories, tags, prices, ratings, sales and a 64-dimension vector) to try things on. Its `emb` field is declared as the `hash:64` model's embedding of the title and description ([below](#embedded-vector-fields)): hashed bags of words, not a learned model, so "similar" means "shares words".

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
| `--models DIR` | `$STRATA_MODELS`, else `~/.local/share/strata/models` | where the models of [embedded vector fields](#embedded-vector-fields) are |
| `--copy-from PATH` | | fill the new database (`--db` with `--schema`) with this database's rows, re-embedding the vector fields whose embedding changed |

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

## Embedded vector fields

A `VECTOR` field can say what its vectors are: a model's embedding of other fields.

```json
{"name": "emb_title", "ty": {"Vector": {"dim": 512, "index": "Flat"}}, "stored": false, "indexed": true,
 "embed": {"from": ["title"], "model": "potion-retrieval-32M"}},
{"name": "emb_both", "ty": {"Vector": {"dim": 512, "index": "Flat"}}, "stored": false, "indexed": true,
 "embed": {"from": ["title", "description"], "model": "potion-retrieval-32M"}}
```

The sources are stored (or generated) `TEXT` or `KEYWORD` fields, and the text embedded is their non-empty values joined by newlines, as for a generated field. The declaration is kept in the database, returned by `/v1/schema` and shown by the UI ("emb_title ← title") and the agent. When the server starts it opens each field's model from `--models` and logs what each field is. With the model open:

- writes that leave the vector out get it computed: `/v1/write` documents (embedded before they queue for the writer, on all cores) and SQL `INSERT`; a vector a write supplies is kept as given;
- an `UPDATE` that changes a source recomputes the vector; merges do not (they keep the stored vectors);
- a parameter `{"embed": "some text", "field": "emb_title"}` is the text's vector by that field's model, so queries and rows are always embedded alike (`field` defaults to the first embedded field with a model); the UI's "Similar to: a description" uses the chosen vector field's model.

Several fields can embed different sources side by side, and a query picks one: `ORDER BY similarity(emb_title, :q)`, or both fused, `rrf(similarity(emb_title, :q), similarity(emb_both, :p))`. Each 512-dimension field costs about 0.5 KB a row as 8-bit codes, plus its HNSW graph.

A field may also name a model the server cannot run, such as `cohere/embed-english-v3.0` for vectors computed elsewhere: that documents what the vectors are, writes must supply them, and text cannot be searched against them (only rows, with `vector_of`). The BEIR collections of the evaluation declare their Cohere vectors this way.

| model | what it is | |
|---|---|---|
| `potion-retrieval-32M` | Model2Vec (a static token table, averaged), 512 dimensions; the model engram uses | tens of microseconds a row on one core |
| `potion-base-8M` | Model2Vec, 256 dimensions | smallest |
| `bge-small-en-v1.5` | BERT encoder, 384 dimensions | build with `--features bert`; milliseconds a row |
| `snowflake-arctic-embed-m-v1.5` | BERT encoder, 768 dimensions | build with `--features bert`; slower, better |
| `hash:N` | hashed bag of words, *N* dimensions | no files; tests and the demo |

On a Mac, build with `--features accelerate` instead of `bert`: the BERT models' matrix products then run in Apple's Accelerate framework.

`strata-server fetch-model NAME` downloads one of these (or any Hugging Face repository `owner/name` with `config.json`, `tokenizer.json` and `model.safetensors`) into the models directory.

**Changing your mind.** Vectors are stored, so changing a field's sources or model means re-embedding. Create a database with the new schema and fill it from the old one:

```sh
strata-server --db products-v2.strata --schema v2.json --copy-from products.strata
```

Rows are copied field by field by name; a vector field is copied when the old one has the same dimensions, sources and model, and embedded anew otherwise; fields the new schema drops are left out. `Db::load_from` does the same from Rust.

**Which fields to embed.** Embed the text that carries what people search for, usually title and description; keep brand, price, category and dates out of the vector and filter on them instead. Measured on four BEIR collections with titles (nDCG@10; Apple M2, 2026-10-27; [all tables, recall and times](https://github.com/rkonow/engram/tree/main/strata/results/embed-fields)):

| collection | model | title | text | title + text | title + text with BM25 |
|---|---|---|---|---|---|
| SciFact | Model2Vec potion-retrieval-32M | 0.487 | 0.613 | **0.633** | 0.697 |
| SciFact | BGE-small v1.5 | 0.598 | 0.697 | **0.713** | 0.724 |
| SciFact | Arctic Embed M v1.5 | 0.576 | 0.704 | **0.716** | 0.729 |
| NFCorpus | Model2Vec potion-retrieval-32M | 0.283 | 0.295 | **0.308** | 0.340 |
| NFCorpus | BGE-small v1.5 | 0.318 | **0.349** | 0.344 | 0.365 |
| NFCorpus | Arctic Embed M v1.5 | 0.321 | 0.361 | **0.363** | 0.370 |
| SciDocs | Model2Vec potion-retrieval-32M | 0.128 | 0.120 | **0.137** | 0.159 |
| TREC-COVID | Model2Vec potion-retrieval-32M | **0.504** | 0.482 | 0.446 | 0.654 |

Title + text is the best single vector, or within 0.005 of it, everywhere but TREC-COVID with Model2Vec, where short COVID questions match its on-topic titles better than a static model's average over long abstracts. Fusing the vector with BM25 (`rrf(bm25(body, :t), similarity(emb, :q))`) beats the best single vector in every run. So start with one vector of title + description searched together with BM25, and when titles are informative and descriptions long, above all with a static model, add a title-only field and compare the two on your own queries. On the M2's CPU, Model2Vec loaded and embedded 12,000 to 22,000 texts a second and the BERT models 8 to 15 (build with `--features accelerate` on a Mac).

`strata-embed-eval` (in `strata/`) runs this comparison: it loads a BEIR collection into fields embedding its titles, its texts and both, by one model, and reports nDCG@10 and recall@100 for each, with BM25, hybrid and the collection's Cohere vectors as references.

```sh
cd strata && STRATA_DATA=~/data/beir cargo run --release --bin strata-embed-eval -- --dataset scifact --model potion-retrieval-32M
```

## HTTP API

Every request and answer is JSON. Parameters bind `:name` placeholders: a number, a string, an array for a vector, an object of term weights for a `SPARSE` field, `{"vector_of": "key", "field": "emb"}` for a stored row's vector (more like this), or `{"embed": "text", "field": "emb"}` for the text's vector by an [embedded field](#embedded-vector-fields)'s model.

| route | body | answer |
|---|---|---|
| `POST /v1/query` | `{sql, params?, timeout_ms?}` | `{columns, rows, keys, scores, stats, took_ms}` |
| `POST /v1/sql` | `{sql, params?, timeout_ms?}` | rows for a `SELECT`; `{written}` for `INSERT`, `UPDATE`, `DELETE` |
| `POST /v1/write` | `{ops: [...]}` | `{written}` |
| `POST /v1/explain` | `{sql, params?}` | the bound query and its plan lines |
| `POST /v1/term_stats` | `{sql, params?}` | BM25 statistics and a snapshot lease, for a coordinator |
| `POST /v1/snapshot` | | a snapshot lease: `{snapshot, txn, ttl_s}` |
| `POST /v1/agent` | `{question, model?, effort?, api_key?, max_steps?}` | an answer and the queries it ran |
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

`GET /` serves a single page with three tabs.

![The query builder: search text, similar to a description, weighted fusion, a filter, the SQL it writes, and the results](../assets/img/strata-server-builder.png)

- **Build** writes SQL from a form: search text (any word, or every word), "similar to" a stored row or a description, rank fusion or a weighted sum with a slider, filters, a boost by closeness (`decay`), a facet, the columns and the row count. The SQL updates as you type; "Edit as SQL" takes it to the editor. Clicking a keyword field in the sidebar shows its most common values, and clicking a value filters on it. Each result row has a "similar" link that searches for rows like it.
- **SQL** is an editor with parameters, results and the plan the executor recorded, segment by segment.
- **Ask an agent** takes a question in plain language.

![The SQL editor with a plan](../assets/img/strata-server-sql.png)

## The agent

`POST /v1/agent`, and the UI's third tab, give a Claude model one tool, `run_sql`, which runs a read-only `SELECT` on the table. The model sees the schema and a description of Strata's SQL (it runs facet queries to learn a field's values), then searches, filters, ranks, counts and facets until it can answer, for at most `max_steps` model calls (8 by default, 20 at most). The answer lists every query it ran, with its row count and time, so you can check its work or open a query in the editor. Writes are refused.

The model is a setting: `--model` sets the server's default (`claude-haiku-5-5`), and a request or the UI can name any other model id and an effort level. The API key comes from `ANTHROPIC_API_KEY` on the server, or from the request (`api_key`, never stored by the server; the UI keeps it in the browser only if you tick "remember").
