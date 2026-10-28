# strata-db: Strata from Python

`import strata` runs a Strata database in your process (`strata.open`,
`strata.Database`), or talks to a running `strata-server` (`strata.connect`,
`strata.Client`), with the same methods: `insert`, `upsert`, `delete`,
`import_file`, `query`, `sql` and `search`.

```sh
pip install ./strata-py          # from the repository's root; needs Rust
```

```python
import strata

db = strata.open("books.strata", schema={
    "table": "books", "key": "id",
    "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "title", "type": "text"},
        {"name": "year", "type": "int"},
    ],
})
db.insert({"id": "b1", "title": "The river and the dragon", "year": 1999})
print(db.search("dragon").dicts())
```

The guide is [docs/strata/python.md](../docs/strata/python.md). Tests:
`maturin develop --release && pytest tests` in a virtualenv (the client
tests use `../strata-server/target/release/strata-server`, or
`$STRATA_SERVER`, and are skipped without it).
