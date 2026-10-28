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

The package is in [`strata-py/`](https://github.com/rkonow/engram/tree/main/strata-py): a Rust extension built with [maturin](https://www.maturin.rs), for CPython 3.10 and later. It needs Rust (stable) and a C compiler.

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

Model2Vec embedding models need nothing more. For the BERT models, build with `--features bert` (`--features accelerate` on a Mac): `maturin develop --release --features bert`, or `MATURIN_PEP517_ARGS="--features bert" pip install ./strata-py`. On x86-64 the extension is compiled for the CPU it is built on (like the server); to build a wheel for other machines, set a baseline, as in `RUSTFLAGS="-C target-cpu=x86-64-v2" maturin build --release`.

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

Rows are dicts of field name to value: `str`, `int`, `float`, `None`, a list for a `multi` field, a list of floats or a numpy array for a vector, a dict of term weights for a `sparse` field. A field the schema does not have is an error.

| method | what it does |
|---|---|
| `insert(rows)` | adds a row (a dict) or rows (any iterable of dicts) whose keys must be new; all or none are written. From 1,000 rows on, they are bulk-loaded straight into segments. |
| `upsert(rows)` | adds rows, replacing the rows with their keys, in transactions of 10,000 |
| `delete(keys)` | deletes the rows with a key or keys; returns how many there were |
| `sql("UPDATE ...")` | `INSERT`, `UPDATE` and `DELETE` statements; returns the rows written |
| `import_file(path, format=None)` | loads a JSON Lines, JSON, CSV or TSV file ([how files are read](server.md#loading-rows)); returns `{rows, bulk, skipped_fields}` |

Vectors of embedded fields that a row leaves out are computed by the field's model when it is open; `db.embed_errors` lists the embedded fields whose model did not open, whose vectors rows must then bring.

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

`search(text, limit=10, where=None, columns="*", mode="auto")` writes the query for the common case. It ranks by BM25 over the text fields (the generated ones if the table has any, such as a combined `body`, else every indexed text field) fused by reciprocal rank with the similarity of the first embedded vector field whose model is open; `mode="text"` or `"vector"` uses one side only. `where` adds an SQL condition, with `params` for its placeholders:

```python
db.search("dragon", where="year >= :y", params={"y": 1990}, columns=["id", "title"])
```

`embed(text, field=None)` returns the query vector of a text, and `len(db)` the number of rows.

## A server from Python

`strata.connect(url, token=None)` returns a `Client` of a running strata-server, using only the standard library. It has `schema`, `query`, `sql`, `search`, `insert`, `upsert`, `delete`, `import_file` (an upload to [`/v1/import`](server.md#loading-rows), up to 256 MB) and `len()`, as above, plus `write(ops)` for raw [write operations](server.md#http-api), `explain`, `health`, `stats` and `ask(question)` for the [agent](server.md#the-agent).

```python
client = strata.connect("http://127.0.0.1:7700")   # token= or $STRATA_TOKEN for a server with --token
client.upsert({"id": "b3", "title": "Star letters", "year": 2024})
client.search("letters", limit=3).dicts()
```

The server embeds `{"embed": text}` parameters and the rows it is sent with its own models. Errors the server reports raise `strata.StrataError`, with the HTTP status as `.status`; a server that cannot be reached raises `urllib.error.URLError`.
