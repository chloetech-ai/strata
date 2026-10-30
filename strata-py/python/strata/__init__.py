"""Strata from Python.

`Database` runs the database in this process (a file, or a directory of
segment files); `Client` talks to a running `strata-server` over HTTP. Both
answer queries with the same `Result` and write rows the same way, so code
can move from one to the other::

    import strata

    db = strata.open("books.strata", schema={
        "table": "books", "key": "id",
        "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text"},
            {"name": "year", "type": "int"},
        ],
    })
    db.upsert([{"id": "b1", "title": "The river and the dragon", "year": 1999}])
    for row in db.search("dragon"):
        print(row["id"], row["title"])
    db.query("SELECT year, COUNT(*) FROM books GROUP BY year").rows

Rows are dicts of field name to value: str, int, float, None, a list for a
list field, a list of floats (or a numpy array) for a vector, a dict of term
weights for a sparse field. Query parameters (``:name`` in the SQL) are
passed as a dict or as keyword arguments; ``{"embed": "some text"}`` binds
the text's vector by an embedded field's model, ``{"vector_of": key}`` a
stored row's vector.
"""

from __future__ import annotations

import builtins
import json
import os
from typing import Any, Iterable, Mapping

from . import _strata, train
from ._search import Search, ordered_schema, params as _params
from .build import (
    Expr,
    Query,
    bm25,
    case,
    clamp,
    coalesce,
    col,
    count,
    exp,
    features,
    greatest,
    hops,
    least,
    lit,
    ln,
    log,
    match,
    maxsim,
    minmax,
    path,
    ppr,
    rank,
    reachable,
    rrf,
    score,
    sigmoid,
    similarity,
    sqrt,
)
from ._strata import StrataError
from .client import Client
from .result import Result

__all__ = [
    "Database",
    "Client",
    "Result",
    "StrataError",
    "open",
    "connect",
    "Query",
    "Expr",
    "col",
    "lit",
    "match",
    "bm25",
    "similarity",
    "maxsim",
    "rrf",
    "minmax",
    "rank",
    "ln",
    "log",
    "exp",
    "sqrt",
    "sigmoid",
    "clamp",
    "least",
    "greatest",
    "coalesce",
    "case",
    "score",
    "hops",
    "features",
    "count",
    "reachable",
    "path",
    "ppr",
    "train",
]


def _schema_json(schema: Any) -> str:
    """A schema given as a dict, JSON text or the path of a JSON file."""
    if isinstance(schema, Mapping):
        return json.dumps(schema)
    if isinstance(schema, (str, os.PathLike)):
        text = os.fspath(schema)
        if text.lstrip().startswith("{"):
            return text
        with builtins.open(text, encoding="utf-8") as f:
            return f.read()
    raise TypeError("a schema is a dict, JSON text or the path of a JSON file")


class Database(Search):
    """A Strata database opened in this process.

    Create one with `Database.create` (or `strata.open` with a schema), open
    an existing one with `Database.open`. Only one process may have a
    database open: to share one, serve it with `strata-server` and use
    `Client`. Calls release the GIL, so threads can query at once.
    """

    def __init__(self, inner: Any):
        self._db = inner

    @classmethod
    def create(cls, path: str | os.PathLike, schema: Any, *, directory: bool = False, models: str | os.PathLike | None = None) -> "Database":
        """Creates a database at `path` with `schema` (a dict, JSON text, or
        the path of a JSON file; the short form or the long one).
        `directory` keeps it as a directory of segment files instead of one
        file. `models` is where embedded fields' models are (default
        $STRATA_MODELS or ~/.local/share/strata/models; see `strata-server
        fetch-model`)."""
        return cls(_strata.Database.create(os.fspath(path), _schema_json(schema), directory, None if models is None else os.fspath(models)))

    @classmethod
    def open(cls, path: str | os.PathLike, *, models: str | os.PathLike | None = None) -> "Database":
        """Opens the database at `path`."""
        return cls(_strata.Database.open(os.fspath(path), None if models is None else os.fspath(models)))

    @property
    def schema(self) -> dict:
        """The first table's schema in its short form: {table, key, fields}."""
        return ordered_schema(json.loads(self._db.schema_json()))

    def tables(self) -> list[dict]:
        """Every table's schema in its short form, the first first."""
        return [ordered_schema(json.loads(t)) for t in self._db.tables_json()]

    @property
    def embed_errors(self) -> dict[str, str]:
        """Embedded fields whose model did not open, with the reason: rows
        written here must bring those fields' vectors."""
        return dict(self._db.embed_errors)

    def _embedded(self) -> list[str]:
        broken = self.embed_errors
        return [f["name"] for f in self.schema["fields"] if f.get("embed") and f["name"] not in broken]

    def __len__(self) -> int:
        return self._db.count()

    def query(self, sql: str, params: Mapping[str, Any] | None = None, **kw: Any) -> Result:
        """Runs a SELECT."""
        return Result._of(self._db.query(sql, _params(params, kw)))

    def sql(self, sql: str, params: Mapping[str, Any] | None = None, **kw: Any) -> Result | int:
        """Runs any statement: a SELECT's `Result`, or the number of rows an
        INSERT, UPDATE or DELETE wrote."""
        r = self._db.sql(sql, _params(params, kw))
        return r if isinstance(r, int) else Result._of(r)

    def plan(self, sql: str, params: Mapping[str, Any] | None = None, **kw: Any) -> dict:
        """The plan of a SELECT, INSERT, UPDATE or DELETE, without running
        it: the plan IR as JSON, which `run` takes."""
        return json.loads(self._db.plan(sql, _params(params, kw)))

    def run(self, plan: Mapping[str, Any] | Query) -> Result | int:
        """Runs a plan (as `plan`, a `Query` or a server's /v1/explain gives
        it): a read's `Result`, or the number of rows a write wrote."""
        if isinstance(plan, Query):
            plan = plan.plan()
        r = self._db.run(json.dumps(plan))
        return r if isinstance(r, int) else Result._of(r)

    def insert(self, rows: Mapping[str, Any] | Iterable[Mapping[str, Any]], *, table: str | None = None) -> int:
        """Inserts a row or rows, whose keys must be new, into the first
        table or `table`; all or none are written. Returns how many."""
        return self._db.insert(rows, table)

    def upsert(self, rows: Mapping[str, Any] | Iterable[Mapping[str, Any]], *, table: str | None = None) -> int:
        """Inserts a row or rows, replacing the rows that have their keys,
        in transactions of 10,000 rows. Returns how many."""
        return self._db.upsert(rows, table)

    def delete(self, keys: Any, *, table: str | None = None) -> int:
        """Deletes the rows with this key or these keys; returns how many
        there were."""
        return self._db.delete(keys, table)

    def import_file(self, path: str | os.PathLike, format: str | None = None, *, table: str | None = None) -> dict:
        """Loads a JSON Lines, JSON (an array), CSV or TSV file, in `format`
        or the one its extension names, into the first table or `table`.
        Into an empty table the rows are bulk-loaded in one commit;
        otherwise they are upserted. Returns {rows, bulk, skipped_fields}:
        the columns the schema lacks."""
        rows, bulk, skipped = self._db.import_file(os.fspath(path), format, table)
        return {"rows": rows, "bulk": bulk, "skipped_fields": skipped}

    def embed(self, text: str | None = None, field: str | None = None, *, table: str | None = None, image: bytes | None = None) -> list[float]:
        """The query vector of `text` by an embedded field's model (of the
        first table, or of `table`), or of `image`, the bytes of an image
        file, by a model of images."""
        return self._db.embed(text, field, table, None if image is None else bytes(image))

    def sid_next(self, prefixes: Iterable[str], *, field: str, where: str | None = None, params: Mapping[str, Any] | None = None, table: str | None = None, **kw: Any) -> dict[str, dict[str, int]]:
        """The codes that can follow each prefix of semantic-ID field
        `field` (``""`` for the first number) among the rows SQL condition
        `where` matches, or all rows: {prefix: {code: rows}}, the codes of
        each prefix in the order of their last numbers. At most 64
        prefixes."""
        prefixes = builtins.list(prefixes)
        got = self._db.sid_next(field, prefixes, where, _params(params, kw), table)
        return {p: dict(cs) for p, cs in zip(prefixes, got)}

    def optimize(self) -> None:
        """Merges small segments (many small writes leave many)."""
        self._db.optimize()

    def close(self) -> None:
        """Merges small segments and closes the database."""
        self._db.close()

    @property
    def closed(self) -> bool:
        return self._db.closed()

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def __repr__(self) -> str:
        if self.closed:
            return "<strata.Database (closed)>"
        return f"<strata.Database {self.schema['table']}: {len(self)} rows>"


def open(path: str | os.PathLike, schema: Any = None, *, directory: bool = False, models: str | os.PathLike | None = None) -> Database:
    """Opens the database at `path`, creating it with `schema` if it does
    not exist."""
    if os.path.exists(path):
        return Database.open(path, models=models)
    if schema is None:
        raise StrataError(f"{os.fspath(path)} does not exist: pass a schema to create it")
    return Database.create(path, schema, directory=directory, models=models)


def connect(url: str = "http://127.0.0.1:7700", *, token: str | None = None, timeout: float = 30.0) -> Client:
    """A `Client` of the strata-server at `url`."""
    return Client(url, token=token, timeout=timeout)

