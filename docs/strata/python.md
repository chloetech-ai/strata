---
title: Python
parent: Strata
nav_order: 5
---

# Strata from Python
{: .no_toc }

The `strata` package runs a Strata database inside a Python process, the way `sqlite3` runs SQLite, or talks to a running [strata-server](server.md). Both have the same methods, so a script written against a local file works against a server.
{: .fs-5 .fw-300 }

1. TOC
{:toc}

## Install

The package is in [`strata-py/`](https://github.com/rkonow/strata/tree/main/strata-py): a Rust extension built with [maturin](https://www.maturin.rs), for CPython 3.10 and later. It needs Rust (stable) and a C compiler.

```sh
pip install ./strata-py                    # from the repository's root: builds and installs a wheel
```

or, while working on it:

```sh
cd strata-py
python3 -m venv .venv && . .venv/bin/activate
pip install maturin pytest
maturin develop --release                  # builds the extension into the virtualenv
pytest tests                               # the client tests also need strata-server built
```

Model2Vec embedding models need nothing more. For the BERT models, build with `--features bert` (`--features accelerate` on a Mac): `maturin develop --release --features bert`, or `MATURIN_PEP517_ARGS="--features bert" pip install ./strata-py`. The extension runs on any CPU of its architecture: Strata picks its SIMD kernels (AVX2, AVX-512, the aarch64 dot product instructions) at run time. `RUSTFLAGS="-C target-cpu=native"` builds one for the building machine only.

## A database in your process

```python
import strata

db = strata.open("books.strata", schema={
    "table": "books",
    "key": "id",
    "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "title", "type": "text"},
        {"name": "description", "type": "text"},
        {"name": "genres", "type": "keyword", "multi": True},
        {"name": "year", "type": "int"},
        {"name": "embedding", "type": "vector",
         "embed": {"from": ["title", "description"], "model": "potion-retrieval-32M"}},
    ],
})

db.insert([
    {"id": "b1", "title": "The river and the dragon", "description": "A fantasy of rivers.", "genres": ["fantasy"], "year": 1999},
    {"id": "b2", "title": "Winter garden", "description": "A quiet mystery.", "genres": ["mystery"], "year": 2005},
])

for row in db.search("dragon", limit=5):
    print(row["id"], row["title"])

db.query("SELECT year, COUNT(*) FROM books WHERE genres = :g GROUP BY year", g="fantasy").rows
db.close()
```

`strata.open(path, schema)` opens the database at `path`, or creates it with `schema` if it does not exist. The schema is a dict, JSON text or the path of a JSON file, in the [form the server takes](server.md#schemas). `Database.create(path, schema)` and `Database.open(path)` do one or the other. `models=` says where embedding models are (default `$STRATA_MODELS`, else `~/.local/share/strata/models`; `strata-server fetch-model NAME` downloads one there), and `directory=True` creates the database as a directory of segment files.

A database file can be open in one process at a time. Within that process, threads can query at once: every call releases the GIL while it runs. To share a database between processes, serve it with `strata-server` and use the client below. A `with` block, or `close()`, merges the small segments that writes left and closes the file.

## Writing

Rows are dicts of field name to value: `str`, `int`, `float`, `None`, `bool` for a `bool` field, a `datetime` or `date` (or ISO 8601 text) for a `timestamp` field, a list for a `multi` field, a list of floats or a numpy array for a vector, a dict of term weights for a `sparse` field. A naive `datetime` is taken as UTC. A field the schema does not have is an error. Results give a `bool` field's values as `bool` and a `timestamp` field's as ISO 8601 text in UTC (`"2026-10-01T12:30:00Z"`), in-process and from the client alike; `datetime.fromisoformat` reads them.

| method | what it does |
|---|---|
| `insert(rows)` | adds a row (a dict) or rows (any iterable of dicts) whose keys must be new; all or none are written. From 1,000 rows on, they are bulk-loaded straight into segments. |
| `upsert(rows)` | adds rows, replacing the rows with their keys, in transactions of 10,000 |
| `delete(keys)` | deletes the rows with a key or keys; returns how many there were |
| `sql("UPDATE ...")` | `INSERT`, `UPDATE` and `DELETE` statements; returns the rows written. It also runs `CREATE TABLE` and `DROP TABLE` ([tables](query-language.md#tables)), which return 0, and writes any table |
| `import_file(path, format=None)` | loads a JSON Lines, JSON, CSV or TSV file ([how files are read](server.md#loading-rows)); returns `{rows, bulk, skipped_fields}` |

`insert`, `upsert`, `delete` and `import_file` write the database's first table, the one `schema` describes, or the one `table=` names (`db.insert(rows, table="events")`); `sql` names the table it writes, and `tables()` lists every table's schema. Through a coordinator, `delete` on a table placed by a column takes that column's value as `routing=` ([tables on shards](server.md#tables-on-shards)). A key a row leaves out gets its field's `default`, and a field with `not_null` refuses a row without a value ([schema options](server.md#schemas)); a `timestamp` defaulting to `"now"` takes one time for all the rows of a call.

Vectors of embedded fields that a row leaves out are computed by the field's model when it is open; `db.embed_errors` lists the embedded fields whose model did not open, whose vectors rows must then bring, and the [semantic-ID fields](#semantic-id-codebooks) whose codebook did not open.

## Querying

`query(sql, params)` runs a `SELECT` in [Strata's SQL](query-language.md). Parameters bind `:name` placeholders, from a dict, keyword arguments, or both:

```python
db.query("SELECT id, title FROM books WHERE year >= :y ORDER BY bm25(title, :q) DESC LIMIT 10", {"y": 2000}, q="dragon")
db.query("SELECT id FROM books ORDER BY similarity(embedding, :v) DESC LIMIT 10", v={"embed": "a quiet mystery"})
db.query("SELECT id FROM books ORDER BY similarity(embedding, :v) DESC LIMIT 10", v={"vector_of": "b1"})
```

A parameter is a number, a string, `None`, a list of numbers or a numpy array (a vector), a list, a dict of term weights, `{"embed": text}` (the text's vector by an embedded field's model; add `"field"` to pick one) or `{"vector_of": key}` (a stored row's vector).

The answer is a `Result`:

| | |
|---|---|
| `for row in result`, `result[i]` | rows as dicts of column to value |
| `result.rows`, `result.columns` | rows as lists, in column order |
| `result.keys`, `result.scores` | each row's primary key and the score it was ranked by |
| `result.column("title")`, `result.scalar()` | one column; the first value (a count, a sum) |
| `result.stats` | what the engine did |
| `result.to_pandas()` | a DataFrame (with pandas installed) |

`search(text, limit=10, where=None, columns="*", mode="auto")` writes the query for the common case. It ranks by BM25 over the text fields (the generated ones if the table has any, such as a combined `body`, else every indexed text field) fused by reciprocal rank with the similarity of the first embedded vector field whose model is open; `mode="text"` or `"vector"` uses one side only. `where` adds a condition, [built in Python](#queries-built-in-python) or in SQL with `params` for its placeholders:

```python
db.search("dragon", where=col("year") >= 1990, columns=["id", "title"])
db.search("dragon", where="year >= :y", params={"y": 1990})
```

`embed(text, field=None)` returns the query vector of a text, and `len(db)` the number of rows.

`plan(sql, params)` returns a statement's [plan](query-language.md#explain) as a dict without running it, and `run(plan)` runs a plan: a read's `Result`, or the number of rows a write wrote.

```python
p = db.plan("SELECT id FROM books WHERE year >= :y LIMIT 3", y=2000)
db.run(p)
```

## Queries built in Python

`table()` starts a query of the table built from Python values instead of SQL text, so nothing is quoted or escaped: fields are named as the schema names them (`col("order")`, `col("doc no")`), and values keep their types (a `str` is text whatever quotes it holds, a `date` or `datetime` a timestamp, UTC unless it says otherwise). Each method returns a new query; iterating one, or `run()`, runs it and gives its `Result`:

```python
from strata import col, match, bm25, similarity, rrf, count, score

q = (db.table()
     .select("id", "title", score())
     .where((col("year") >= 2019) & match("body", "graph neural") & ~col("tags").isin(["draft"]))
     .order_by(bm25("body", "graph neural"))
     .limit(10))
for row in q:
    print(row["id"], row["score()"])

db.table().select("genre", count().alias("n"), col("rating").avg().alias("avg")).group_by("genre").having(count() > 10).order_by(col("genre"))
db.table().order_by(rrf(bm25("title", text), similarity("emb", db.embed(text)))).limit(10)
```

| | |
|---|---|
| `col(name)`, `lit(value)` | a field; a value where an expression is wanted |
| `==`, `!=`, `<`, `<=`, `>`, `>=` | comparisons with a value or another expression |
| `&`, `\|`, `~` | and, or, not (Python's `and`, `or`, `not` and chained comparisons such as `1 < col("x") < 5` raise `TypeError`) |
| `.isin(values)`, `.between(lo, hi)`, `.is_null()`, `.is_not_null()` | as SQL's `IN`, `BETWEEN` (a bound of `None` is open), `IS NULL` |
| `match(field, query)` | the full-text condition, in [MATCH's syntax](query-language.md#full-text-match) |
| `bm25(field, text)`, `similarity(field, vector)`, `rrf(*scores, k=60, depth=100)` | scores; `similarity` takes a vector (`embed(text)` gives a text's), or `{term: weight}` for a SPARSE field |
| `+`, `-`, `*`, `/`, `%`, `**`, `abs()`, `round()`, `ln`, `log`, `exp`, `sqrt`, `sigmoid`, `clamp`, `least`, `greatest`, `coalesce`, `minmax`, `rank`, `case((cond, value), ..., otherwise=None)` | a [ranking expression](query-language.md#ranking-expressions) of scores, fields and numbers: `bm25("body", q) * (1 + 0.2 * coalesce(col("rating"), 0))`. `+` adds raw values; `0.7 * minmax(a) + 0.3 * minmax(b)` is weighted fusion |
| `reachable(field, seeds, hops=1, ...)`, `path(field, seeds, to, ...)`, `ppr(field, seeds, ...)` | [graph](graphs.md) conditions and PageRank as a score, with the SQL functions' options as keyword arguments |
| `score()`, `hops()` | the score a row was ranked by; its hops from a traversal's seeds |
| `count()`, `col(f).count(distinct=True)`, `.sum()`, `.avg()`, `.min()`, `.max()` | aggregates, for `select`, `having` and `order_by` over groups |
| `.alias(name)`, `.asc()`, `.desc(nulls_first=False)` | a column's name; an `order_by` direction |

`select(...)` names the columns (without it, the stored fields but vectors, as `SELECT *`), `where(...)` keeps rows, `order_by(...)` ranks by a score best first or orders by fields ascending, `group_by(...)` and `having(...)` group, and `limit(n)` and `offset(n)` page. A query is a [plan](query-language.md#explain), the same JSON that `plan(sql)` gives: `q.plan()` shows it, and a `Client` runs it on a server the same way.

## Semantic-ID codebooks

A [semantic-ID field](server.md#semantic-ids) gives each row a code from its vector by a codebook, which `strata.train.codebook` trains and saves in the models directory before a table names it:

```python
h = strata.train.codebook(db, "items-rq", field="emb")       # a table's vectors
h = strata.train.codebook(vectors, "items-rq")               # a list of lists, or a 2-D numpy array

items = strata.open("items.strata", schema={
    "table": "items", "key": "id",
    "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "emb", "type": "vector", "dim": 384},
        {"name": "sid", "type": "keyword", "multi": True,
         "semantic_id": {"from": "emb", "codebook": "items-rq"}},
    ],
})
items.upsert(rows)                                            # each row gets its sid
items.query("SELECT id FROM items WHERE sid = '12.7'")
```

It returns the SHA-256 of the file, which the table records. The options are `levels` (1 to 4, default 3), `k` centroids a level (up to 256), `iterations` of k-means a level (25), `sample`, the vectors it trains on (100 a centroid by default, drawn at random), `seed` (the same vectors and options give the same codebook), `table` (a database's first by default) and `models` (the default models directory, as for `Database.create`). Pass the same `models=` to `Database.create` and `Database.open`: a codebook that does not open is listed in `db.embed_errors`, and writes that need a code then fail.

## A server from Python

`strata.connect(url, token=None)` returns a `Client` of a running strata-server, using only the standard library. It has `schema`, `tables`, `query`, `sql`, `search`, `insert`, `upsert`, `delete`, `import_file` (an upload to [`/v1/import`](server.md#loading-rows), up to the server's `--max-body-mb`, 256 MiB unless set) and `len()`, as above, plus `write(ops)` for raw [write operations](server.md#http-api), `explain`, `run(plan)` (a plan from `explain(...)["ir"]` or a `table()` query, run by [`/v1/plan`](server.md#http-api)), `table()` ([queries built in Python](#queries-built-in-python)), `embed(text, field=None)` (a text's vector by the server's model, from [`/v1/embed`](server.md#http-api)), `health`, `stats` and `ask(question)` for the [agent](server.md#the-agent).

```python
client = strata.connect("http://127.0.0.1:7700")   # token= or $STRATA_TOKEN for a server with --token
client.upsert({"id": "b3", "title": "Star letters", "year": 2024})
client.search("letters", limit=3).dicts()
```

The server embeds `{"embed": text}` parameters and the rows it is sent with its own models. Errors the server reports raise `strata.StrataError`, with the HTTP status as `.status`; a server that cannot be reached raises `urllib.error.URLError`.
