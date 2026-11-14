"""ChloeDB from Python.

`Database` runs the database in this process (a file, or a directory of
segment files); `Client` talks to a running `chloedb-server` over HTTP. Both
answer queries with the same `Result` and write rows the same way, so code
can move from one to the other::

    import chloedb

    db = chloedb.open("books.chloedb", schema={
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
from typing import Any, Iterable, Iterator, Mapping

from . import ngrams, train
from ._native import _strata
from ._search import Search, ordered_schema, params as _params
from .build import (
    Expr,
    Query,
    bm25,
    case,
    clamp,
    coalesce,
    at_least,
    col,
    count,
    count_true,
    exp,
    features,
    gbdt,
    cross_encoder,
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
    ranked,
    reachable,
    rrf,
    score,
    tmm,
    zscore,
    sigmoid,
    similarity,
    sqrt,
    when,
)
from ._native import (
    BadRequestError,
    BusyError,
    ChloeDBError,
    ConflictError,
    DeadlineError,
    DuplicateKeyError,
    NotFoundError,
    OverloadedError,
    PartialWriteError,
    StrataError,
    UnauthorizedError,
    UnavailableError,
    error_code,
)
from .client import Client, Session, Written
from .aio import AsyncClient, AsyncSession
from .frame import Frame, FrameStore, GroupBy, concat, len_, _arrow_table
from .nearby import Nearby, NearbyResult, nearby
from .ngrams import next_items
from .result import ArrowResult, Result

# The package's version: pyproject.toml's (a test checks they agree).
__version__ = "0.3.3"

__all__ = [
    "__version__",
    "Database",
    "Client",
    "AsyncClient",
    "AsyncSession",
    "Nearby",
    "NearbyResult",
    "nearby",
    "Result",
    "ArrowResult",
    "Session",
    "Written",
    "ChloeDBError",
    "StrataError",
    "BadRequestError",
    "UnauthorizedError",
    "NotFoundError",
    "ConflictError",
    "DuplicateKeyError",
    "PartialWriteError",
    "BusyError",
    "OverloadedError",
    "DeadlineError",
    "UnavailableError",
    "error_code",
    "open",
    "connect",
    "from_sqlite",
    "plan_sqlite",
    "from_parquet",
    "plan_parquet",
    "Query",
    "Expr",
    "col",
    "lit",
    "match",
    "at_least",
    "bm25",
    "similarity",
    "maxsim",
    "rrf",
    "ranked",
    "minmax",
    "tmm",
    "zscore",
    "rank",
    "gbdt",
    "cross_encoder",
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
    "count_true",
    "score",
    "hops",
    "features",
    "count",
    "reachable",
    "path",
    "ppr",
    "train",
    "ngrams",
    "next_items",
    "upgrade",
    "Frame",
    "GroupBy",
    "concat",
    "when",
    "len",
]


def __getattr__(name: str) -> Any:
    # `chloedb.len()`, as Polars' `pl.len()`, without hiding the builtin
    # `len` inside this module.
    if name == "len":
        return len_
    raise AttributeError(f"module 'chloedb' has no attribute {name!r}")


def upgrade(path: str | os.PathLike) -> tuple[str, int, int] | None:
    """Copy the database at `path`, written by a version of ChloeDB from
    before field ids (which no longer opens), into this version's format in
    its place. The old one is kept as ``<path>.before-field-ids``. Returns
    None when it needs no upgrade, else (the old database's path, tables
    copied, rows copied)."""
    r = _strata.upgrade(os.fspath(path))
    return None if r is None else (os.fspath(r[0]), r[1], r[2])


# Options a `Client` takes by keyword that a database in this process has
# no use for. Passed to `Database` they would otherwise bind as query
# parameters and be ignored, so code moved from a server loses its
# deadline or its idempotency without a word: they raise instead. (The
# methods that run a query, `query`, `query_arrow`, `scroll`, `sql`,
# `search` and `run`, take `timeout_ms` as a client's do.)
_CLIENT_ONLY = {
    "timeout_ms": "Database.{method}() takes no timeout_ms (query, query_arrow, scroll, sql, search and run do). Pass params={{'timeout_ms': ...}} for a query parameter of that name.",
    "idempotency_key": "Database.{method}() takes no idempotency_key: a write in this process commits or raises, with no answer to lose (a Client's can be sent again). Pass params={{'idempotency_key': ...}} for a query parameter of that name.",
    "keep_alive_ms": "Database.{method}() takes no keep_alive_ms: a scroll in this process holds its answer itself. Pass params={{'keep_alive_ms': ...}} for a query parameter of that name.",
}


def _local_params(method: str, given: Mapping[str, Any] | None, kw: Mapping[str, Any]) -> dict | None:
    """Query parameters from a dict and keyword arguments, refusing a
    keyword that is a client's option (`_CLIENT_ONLY`)."""
    for name in kw:
        if name in _CLIENT_ONLY:
            raise TypeError(_CLIENT_ONLY[name].format(method=method))
    return _params(given, kw)


def _limits(timeout_ms: int | None, memory: int | None) -> tuple[int | None, int | None]:
    """A query's `timeout_ms` and `memory` as the native calls take them."""
    if timeout_ms is not None and timeout_ms < 0:
        raise ValueError("timeout_ms: at least 0")
    if memory is not None and memory < 0:
        raise ValueError("memory: bytes, at least 0")
    return (None if timeout_ms is None else int(timeout_ms), None if memory is None else int(memory))


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


def _schemas_json(schemas: Any) -> str | None:
    """Schemas for a SQLite import: a dict, a list of them, a plan
    (`plan_sqlite`), JSON text or the path of a JSON file."""
    if schemas is None:
        return None
    if isinstance(schemas, (Mapping, list)):
        return json.dumps(schemas)
    if isinstance(schemas, (str, os.PathLike)):
        text = os.fspath(schemas)
        if text.lstrip().startswith(("{", "[")):
            return text
        with builtins.open(text, encoding="utf-8") as f:
            return f.read()
    raise TypeError("schemas are a dict, a list, JSON text or the path of a JSON file")


def _names(tables: str | Iterable[str] | None) -> list[str] | None:
    if tables is None:
        return None
    return [tables] if isinstance(tables, str) else list(tables)


class _Pending:
    """A database `connect` was given the path of, not made yet: its
    first `create_table` makes it."""

    def __init__(self, path: str, directory: bool, models: Any, threads: int | None):
        self.path, self.directory, self.models, self.threads = path, directory, models, threads

    def __getattr__(self, name: str) -> Any:
        raise ChloeDBError(f"{self.path} has no tables yet: make one with create_table(name, data)")

    def closed(self) -> bool:
        return False

    def close(self) -> None:
        pass


class Database(Search, FrameStore):
    """A ChloeDB database opened in this process.

    Create one with `Database.create` (or `chloedb.open` with a schema), open
    an existing one with `Database.open`, or `chloedb.connect(path)` for
    either. Only one process may have a database open: to share one, serve
    it with `chloedb-server` and use `Client`. Calls release the GIL, so
    threads can query at once. `db["table"]` is a lazy `Frame` of a table.

    Its queries are limited (see `set_limits`): those of every database of
    the process share a memory budget, a quarter of the machine's memory
    (or $CHLOEDB_QUERY_MEMORY_MB MiB), and a query that would hold more
    raises ChloeDBError (code "memory") instead of taking the program's
    memory. They have no deadline unless given `timeout_ms`, and
    `interrupt()` stops them from another thread.
    """

    # Queries run in this process (`Search.search` takes `memory`).
    _local = True

    def __init__(self, inner: Any, *, timeout_ms: int | None = None, memory: int | None = None):
        self._db = inner
        #: What the last SQLite or Parquet import into it did (`from_sqlite`,
        #: `import_sqlite`, `from_parquet`, `import_parquet`).
        self.last_import: dict | None = None
        self._limits = (timeout_ms, memory)
        if timeout_ms is not None or memory is not None:
            self.set_limits(timeout_ms=timeout_ms, memory=memory)

    @classmethod
    def create(cls, path: str | os.PathLike, schema: Any, *, directory: bool = False, models: str | os.PathLike | None = None, threads: int | None = None, timeout_ms: int | None = None, memory: int | None = None) -> "Database":
        """Creates a database at `path` with `schema` (a dict, JSON text, or
        the path of a JSON file; the short form or the long one).
        `directory` keeps it as a directory of segment files instead of one
        file. `models` is where embedded fields' models are (default
        $CHLOEDB_MODELS or ~/.local/share/chloedb/models; see `chloedb-server
        fetch-model`). `threads` is how many cores vector indexes build on
        and each query searches segments on (default: every core).
        `timeout_ms` and `memory` limit its queries (see `set_limits`)."""
        return cls(_strata.Database.create(os.fspath(path), _schema_json(schema), directory, None if models is None else os.fspath(models), threads), timeout_ms=timeout_ms, memory=memory)

    @classmethod
    def open(cls, path: str | os.PathLike, *, models: str | os.PathLike | None = None, threads: int | None = None, read_only: bool = False, recover: bool = False, timeout_ms: int | None = None, memory: int | None = None) -> "Database":
        """Opens the database at `path`; `threads`, `timeout_ms` and
        `memory` as `create` takes them.
        `read_only` opens it to read while another process writes it (a
        web server's other workers, a script): it sees each commit of the
        writer, and every write fails. Without it only one process may have
        the database open.

        A database whose newest version or write-ahead log is damaged so
        that opening it would lose acknowledged writes fails to open
        (ChloeDBError, naming what is damaged), and nothing is changed.
        `recover=True` opens it all the same, accepting the loss: copies of
        the damaged files are kept first (`<path>.damaged-<time>`,
        `<path>-wal.damaged`), `damaged` says what was lost, and a
        `RuntimeWarning` is issued."""
        db = cls(_strata.Database.open(os.fspath(path), None if models is None else os.fspath(models), threads, read_only, recover), timeout_ms=timeout_ms, memory=memory)
        if db.damaged is not None:
            import warnings

            warnings.warn(f"{os.fspath(path)}: opened past damage: {db.damaged}", RuntimeWarning, stacklevel=2)
        return db

    def set_limits(self, *, timeout_ms: int | None = None, memory: int | None = None) -> None:
        """Limits every query of this database from now on, unless a query
        gives its own (`query(..., timeout_ms=, memory=)`): a deadline of
        `timeout_ms` milliseconds (None or 0: none, the default), past
        which a query stops and raises DeadlineError; and a memory budget
        of `memory` bytes that its queries share (0: none; None, the
        default: the budget every database of the process shares, a
        quarter of the machine's memory or $CHLOEDB_QUERY_MEMORY_MB MiB),
        past which a query stops and raises ChloeDBError with code
        "memory". The budget counts what a query builds up (its groups, the
        rows it orders and returns, a join's tables), not the index it
        reads."""
        self._limits = _limits(timeout_ms, memory)
        if not isinstance(self._db, _Pending):
            self._db.set_limits(*self._limits)

    @property
    def timeout_ms(self) -> int | None:
        """The deadline every query gets unless it gives its own, in
        milliseconds (None: none)."""
        if isinstance(self._db, _Pending):
            return self._limits[0] or None
        return self._db.timeout_ms()

    @property
    def memory(self) -> int | None:
        """The memory budget its queries share, in bytes (None: none)."""
        if isinstance(self._db, _Pending):
            return self._limits[1] or None
        return self._db.memory()

    def interrupt(self) -> int:
        """Stops the queries of this database running now, from any thread
        (as `sqlite3.Connection.interrupt`): each raises DeadlineError
        within a few thousand rows. Returns how many there were; queries
        that start after it run."""
        if isinstance(self._db, _Pending):
            return 0
        return self._db.interrupt()

    @property
    def read_only(self) -> bool:
        """Whether it was opened with `read_only`."""
        return self._db.read_only()

    def refresh(self) -> bool:
        """Opened `read_only`: reads what the writer committed since the
        last query, now (each query does so by itself); whether anything
        changed. False for the writer."""
        return self._db.refresh()

    @property
    def schema(self) -> dict:
        """The first table's schema in its short form: {table, key, fields}."""
        return ordered_schema(json.loads(self._db.schema_json()))

    def _table_schema(self, name: str) -> dict:
        """The schema of the table `name`, in its short form."""
        return ordered_schema(json.loads(self._db.schema_json(name)))

    def tables(self) -> list[dict]:
        """Every table's schema in its short form, the first first (none
        before `connect`'s new file has its first table)."""
        if isinstance(self._db, _Pending):
            return []
        return [ordered_schema(json.loads(t)) for t in self._db.tables_json()]

    def add_column(self, name: str, type: str, *, table: str | None = None, **options: Any) -> None:
        """Adds a column to the first table or `table`, as `ALTER TABLE ...
        ADD COLUMN` does: `options` are the field's, in the short form of a
        schema (`default`, `not_null`, `indexed`, `embed` and `model`, ...).
        Rows written before it read it as its default, or None.

            db.add_column("price", "float", default=0.0)
            db.add_column("emb", "vector", embed=["title"], model="potion-retrieval-32M")
        """
        self._db.alter_table_json(json.dumps([{"add": {"name": name, "type": type, **options}}]), table)

    def drop_column(self, name: str, *, table: str | None = None) -> None:
        """Drops a column of the first table or `table`, with its values,
        as `ALTER TABLE ... DROP COLUMN` does. The key, and a column another
        field or the table's options name, stay."""
        self._db.alter_table_json(json.dumps([{"drop": name}]), table)

    def rename_column(self, old: str, new: str, *, table: str | None = None) -> None:
        """Renames a column of the first table or `table`, keeping its
        values; the fields and options that name it follow."""
        self._db.alter_table_json(json.dumps([{"rename": [old, new]}]), table)

    @property
    def embed_errors(self) -> dict[str, str]:
        """Embedded fields whose model did not open, with the reason: rows
        written here must bring those fields' vectors (a write that leaves
        one to the model fails), unless `set_embed_later()`."""
        return dict(self._db.embed_errors)

    def set_embed_later(self, on: bool = True) -> None:
        """Lets writes store rows without the vectors of embedded fields
        whose model is not open (see `embed_errors`); by default such a
        write fails, since `similarity()` would never find the row. The
        vectors can be filled in once the model opens (a server does it
        itself; from Rust, `Db::embed_backfill`)."""
        self._db.set_embed_later(on)

    def _embedded(self, table: str | None = None) -> list[str]:
        broken = self.embed_errors
        first = self.schema
        s = first if table is None else self._table_schema(table)
        # embed_errors names the first table's fields alone, others' as table.field.
        name = (lambda f: f) if s["table"] == first["table"] else (lambda f: f"{s['table']}.{f}")
        return [f["name"] for f in s["fields"] if f.get("embed") and name(f["name"]) not in broken]

    def __len__(self) -> int:
        return self._db.count()

    def query(self, sql: str, params: Mapping[str, Any] | None = None, *, timeout_ms: int | None = None, memory: int | None = None, min_seq_no: int | None = None, min_seq_nos: Any = None, **kw: Any) -> Result:
        """Runs a SELECT. `timeout_ms` is its deadline, as a `Client`'s
        query takes it: past it the query stops and raises DeadlineError
        (None or 0: the database's, see `set_limits`). `memory` is a budget
        of its own in bytes, past which it raises ChloeDBError with code
        "memory" (0: none; None: the database's). `min_seq_no` and
        `min_seq_nos` are taken for code written against a `Client`, and
        change nothing: a database in this process reads every write it
        committed. A client's other options (`idempotency_key`, ...) raise
        TypeError rather than bind as parameters; a parameter of that name
        goes in `params`."""
        return Result._of(self._db.query(sql, _local_params("query", params, kw), *_limits(timeout_ms, memory)))

    def query_arrow(self, sql: str, params: Mapping[str, Any] | None = None, *, timeout_ms: int | None = None, memory: int | None = None, min_seq_no: int | None = None, min_seq_nos: Any = None, **kw: Any) -> ArrowResult:
        """Runs a SELECT: its answer as Arrow columns, which pyarrow,
        polars and pandas read without copying (`ArrowResult`);
        `timeout_ms` and `memory` as `query` takes them."""
        if not hasattr(self._db, "query_arrow"):
            raise ChloeDBError("this build of chloedb has no Arrow: build it with the feature datafusion (the default)")
        return ArrowResult(self._db.query_arrow(sql, _local_params("query_arrow", params, kw), *_limits(timeout_ms, memory)))

    def scroll(self, sql: str, params: Mapping[str, Any] | None = None, *, page_size: int = 10_000, arrow: bool = False, timeout_ms: int | None = None, memory: int | None = None, min_seq_no: int | None = None, min_seq_nos: Any = None, **kw: Any) -> Iterator[Any]:
        """Runs a SELECT and yields its answer a page at a time, as
        `Client.scroll` does: `Result`s, or with `arrow=True` pyarrow
        Tables, of at most `page_size` rows. A database in this process
        answers every row at once, so its pages are slices of one answer,
        and `timeout_ms` and `memory` (as `query` takes them) bound that
        answer."""
        _local_params("scroll", params, kw)
        n = max(int(page_size), 1)
        if arrow:
            import pyarrow

            t = pyarrow.table(self.query_arrow(sql, params, timeout_ms=timeout_ms, memory=memory, **kw))
            for i in range(0, max(t.num_rows, 1), n):
                yield t.slice(i, n)
            return
        r = self.query(sql, params, timeout_ms=timeout_ms, memory=memory, **kw)
        for i in range(0, max(len(r.rows), 1), n):
            yield Result(r.columns, r.rows[i : i + n], r.keys[i : i + n], r.scores[i : i + n], r._stats if i == 0 else None, r.took_ms if i == 0 else None)

    def sql(self, sql: str, params: Mapping[str, Any] | None = None, *, timeout_ms: int | None = None, memory: int | None = None, min_seq_no: int | None = None, min_seq_nos: Any = None, **kw: Any) -> Result | int:
        """Runs any statement: a SELECT's `Result`, or the number of rows an
        INSERT, UPDATE or DELETE wrote. `timeout_ms` and `memory` limit a
        SELECT as `query` takes them; a write is not cut short. A client's
        `idempotency_key` raises TypeError (see `query`)."""
        r = self._db.sql(sql, _local_params("sql", params, kw), *_limits(timeout_ms, memory))
        return r if isinstance(r, int) else Result._of(r)

    def _query_logged(self, sql: str, params: Mapping[str, Any] | None, log: dict) -> Result:
        return Result._of(self._db.query_logged(sql, _params(params, {}), json.dumps(log)))

    def _export_rank_log(self, opts: dict) -> list[dict]:
        return json.loads(self._db.export_rank_log(json.dumps(opts)))

    def plan(self, sql: str, params: Mapping[str, Any] | None = None, **kw: Any) -> dict:
        """The plan of a SELECT, INSERT, UPDATE or DELETE, without running
        it: the plan IR as JSON, which `run` takes."""
        return json.loads(self._db.plan(sql, _local_params("plan", params, kw)))

    def run(self, plan: Mapping[str, Any] | Query, *, timeout_ms: int | None = None, memory: int | None = None, min_seq_no: int | None = None, min_seq_nos: Any = None) -> Result | int:
        """Runs a plan (as `plan`, a `Query` or a server's /v1/explain gives
        it): a read's `Result`, or the number of rows a write wrote; a read
        limited by `timeout_ms` and `memory` as `query` takes them."""
        if isinstance(plan, Query):
            plan = plan.plan()
        r = self._db.run(json.dumps(plan), *_limits(timeout_ms, memory))
        return r if isinstance(r, int) else Result._of(r)

    def insert(self, rows: Mapping[str, Any] | Iterable[Mapping[str, Any]], *, table: str | None = None) -> int:
        """Inserts a row or rows, whose keys must be new, into the first
        table or `table`; all or none are written. Returns how many. A
        vector may be a numpy array (float32 or float64), read from its
        bytes."""
        return self._db.insert(rows, table)

    def upsert(self, rows: Mapping[str, Any] | Iterable[Mapping[str, Any]], *, table: str | None = None) -> int:
        """Inserts a row or rows, replacing the rows that have their keys,
        in transactions of 10,000 rows. Returns how many."""
        return self._db.upsert(rows, table)

    def get(self, key: Any, *, table: str | None = None, min_seq_no: int | None = None, min_seq_nos: Any = None) -> dict | None:
        """The row with this primary key in the first table or `table`, read
        without SQL: a dict of the columns `SELECT *` answers, or None when
        no live row has the key (or its `expire_by` time has passed).
        `min_seq_no(s)` change nothing here (see `query`)."""
        return self._db.get_many([key], table)[0]

    def get_many(self, keys: Iterable[Any], *, table: str | None = None, min_seq_no: int | None = None, min_seq_nos: Any = None) -> list[dict | None]:
        """`get` of several keys at once, in their order."""
        return self._db.get_many(list(keys), table)

    def expire(self) -> int:
        """Deletes the rows whose `expire_by` time has passed, in every
        table with one; returns how many. Reads leave them out already."""
        return self._db.expire()

    def delete(self, keys: Any, *, table: str | None = None) -> int:
        """Deletes the rows with this key or these keys; returns how many
        there were."""
        return self._db.delete(keys, table)

    def write(self, ops: list[dict], *, idempotency_key: str | None = None) -> int:
        """Write operations as `Client.write` and /v1/write take them
        ({"op": "insert", "doc": {...}}, {"op": "add", "key": k, "field":
        "n", "value": 1}, ...), in one transaction. Returns the rows
        written. A client's `idempotency_key` raises TypeError: a
        transaction here either commits or raises, with no answer to lose."""
        if idempotency_key is not None:
            raise TypeError(_CLIENT_ONLY["idempotency_key"].format(method="write"))
        return self._db.write(ops)

    def import_file(self, path: str | os.PathLike, format: str | None = None, *, table: str | None = None) -> dict:
        """Loads a JSON Lines, JSON (an array), CSV or TSV file, in `format`
        or the one its extension names, into the first table or `table`.
        Into an empty table the rows are bulk-loaded in one commit;
        otherwise they are upserted. Returns {rows, bulk, skipped_fields}:
        the columns the schema lacks."""
        rows, bulk, skipped = self._db.import_file(os.fspath(path), format, table)
        return {"rows": rows, "bulk": bulk, "skipped_fields": skipped}

    def import_sqlite(self, path: str | os.PathLike, *, tables: str | Iterable[str] | None = None, schemas: Any = None, blobs_base64: bool = False) -> dict:
        """Imports the tables of a SQLite database file (all, or `tables`).
        Each goes into the table of its name: an existing table takes the
        columns whose names match its fields (rows upserted by key, or
        bulk-loaded when it is empty); a missing one is created from the
        SQLite table's declarations and values, or from the schema
        `schemas` gives for it (see `plan_sqlite`). BLOB columns are left
        out unless `blobs_base64`. Returns {tables: [{source, table, rows,
        created, bulk, notes}], skipped: {name: why}}; the notes say what
        did not carry over."""
        self.last_import = json.loads(self._db.import_sqlite(os.fspath(path), _names(tables), _schemas_json(schemas), blobs_base64))
        return self.last_import

    def export_sqlite(self, path: str | os.PathLike, *, tables: str | Iterable[str] | None = None, fts: bool = True, indexes: bool = True, schemas: bool = True) -> dict:
        """Writes the tables (all, or `tables`) to a new SQLite file, from
        one snapshot: rows, keys, references, NOT NULL and DEFAULT, B-tree
        indexes (`indexes`), an FTS5 index over each table's text fields
        kept in step by triggers (`fts`), and the ChloeDB schemas in table
        `_strata_tables` (`schemas`), which an import restores. Vectors are
        float32 blobs (`F32_BLOB(n)`), lists, edges and sparse weights JSON,
        timestamps UTC text. Returns {tables: [{table, rows, indexes, fts,
        notes}]}."""
        return json.loads(self._db.export_sqlite(os.fspath(path), _names(tables), fts, indexes, schemas))

    def import_parquet(self, path: str | os.PathLike, *, table: str | None = None, schema: Any = None, key: str | None = None, blobs_base64: bool = False) -> dict:
        """Imports a Parquet file, or a directory of them as Spark, DuckDB,
        polars and pandas write them (part files, Hive partitions
        `key=value/` read as columns), into one table; `path` may be an
        `s3://` URL (an object, or every object under a prefix). The rows
        go into `table` (by default, the table an export kept in the file,
        else one named for the file): an existing table takes the columns
        whose names match its fields (rows upserted by key, or bulk-loaded
        when it is empty); a missing one is created with `schema` (see
        `plan_parquet`) or from the columns' types and values, keyed by
        `key` (default: a column named id; "" for none). Binary columns
        are left out unless `blobs_base64`. Returns {table, rows, files,
        created, bulk, skipped: {column: why}, notes}."""
        self.last_import = json.loads(self._db.import_parquet(os.fspath(path), table, None if schema is None else _schema_json(schema), key, blobs_base64))
        return self.last_import

    def export_parquet(self, path: str | os.PathLike, *, tables: str | Iterable[str] | None = None, schemas: bool = True) -> dict:
        """Writes the tables (all, or `tables`) to Parquet, from one
        snapshot: to the file `path` when it ends in .parquet (one table),
        else to a new directory of `<table>.parquet` files; `path` may be an
        `s3://` URL. Each file is compressed with Zstandard and keeps the
        table's ChloeDB schema (unless not `schemas`), which `import_parquet`
        restores; any Parquet
        reader (pyarrow, polars, DuckDB, Spark) reads the columns: vectors
        as fixed-size lists of float32, timestamps as UTC microseconds,
        lists as lists, JSON, edges and sparse weights as JSON text.
        Returns {tables: [{table, file, rows, bytes, notes}]}."""
        return json.loads(self._db.export_parquet(os.fspath(path), _names(tables), schemas))

    def import_arrow(self, data: Any, *, table: str | None = None) -> dict:
        """Writes rows from anything that gives an Arrow stream: a pyarrow
        Table, RecordBatch or RecordBatchReader, a polars DataFrame, a
        DuckDB relation, a pandas DataFrame (through pyarrow), into the
        first table or `table`. Each column fills the field of its name;
        the others are left out. Into an empty table the rows are
        bulk-loaded in one commit; otherwise they are upserted by key.
        Returns {table, rows, bulk, skipped, notes}."""
        if not hasattr(data, "__arrow_c_stream__") and type(data).__module__.startswith("pandas"):
            import pyarrow

            data = pyarrow.Table.from_pandas(data, preserve_index=False)
        return json.loads(self._db.import_arrow(data, table))

    def embed(self, text: str | None = None, field: str | None = None, *, table: str | None = None, image: bytes | None = None, audio: bytes | None = None) -> list[float]:
        """The query vector of `text` by an embedded field's model (of the
        first table, or of `table`), or of `image` or `audio`, the bytes of
        an image or sound file, by a model of that."""
        return self._db.embed(text, field, table, None if image is None else bytes(image), None if audio is None else bytes(audio))

    def embed_tokens(self, text: str, field: str, *, table: str | None = None) -> list[list[float]]:
        """The vectors of `text`'s tokens as a query, by the model of
        multi-vector field `field` (a ColBERT model, `VECTOR(d)[]` embedded
        from text), as `maxsim` takes them."""
        return self._db.embed_tokens(text, field, table)

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
        """Readies a table that is done changing for queries: writes the
        rows still in memory into segments, merges its segments into as few
        as it can (one, up to 2,000,000 rows; a query visits each), drops
        deleted rows, and returns the space this frees to the file system."""
        self._db.optimize()

    def vacuum(self) -> int:
        """Returns the space merges freed to the file system (they reuse it
        otherwise); the number of bytes the file shrank by."""
        return self._db.vacuum()

    def flush(self) -> None:
        """Writes the rows still in memory into the file and empties its
        write-ahead log (`<path>-wal`): the file alone then holds every
        committed row, until the next write."""
        self._db.flush()

    def checkpoint(self, dest: str | os.PathLike, *, directory: bool = False) -> dict:
        """Writes a consistent copy of the database to `dest` (which must
        not exist) while it stays open and in use: the safe way to back it
        up. Every segment is checked against its checksum (a damaged
        database fails the checkpoint instead of being copied), the copy is
        checked, and only then given its name. Returns what it copied."""
        return json.loads(self._db.checkpoint(os.fspath(dest), directory))

    @property
    def damaged(self) -> str | None:
        """What opening found damaged and skipped or dropped, in words (with
        where it kept copies), or None. Only an open with `recover=True`
        accepts a loss of acknowledged writes; it also warns."""
        return self._db.damaged

    def close(self) -> None:
        """Writes the rows still in memory into the file (so that the file
        alone, without its `-wal`, is then a complete copy of the
        database), merges small segments, gives the file system back the
        space that merges freed when it is an eighth of the file or more,
        and closes the database. (Segments are merged in the background as
        writes add them, so a table written to for a long time stays in few
        segments.)"""
        self._db.close()

    @property
    def closed(self) -> bool:
        return self._db.closed()

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def __repr__(self) -> str:
        if isinstance(self._db, _Pending):
            return f"<chloedb.Database {self._db.path} (no tables yet)>"
        if self.closed:
            return "<chloedb.Database (closed)>"
        return f"<chloedb.Database {self.schema['table']}: {len(self)} rows>"

    # What frames need of a store (FrameStore).

    def _frame_arrow(self, sql: str, params: Mapping[str, Any], types: Mapping[str, dict]) -> ArrowResult:
        return self.query_arrow(sql, params)

    def _explain(self, sql: str, params: Mapping[str, Any]) -> list[str]:
        return [str(row[0]) for row in self.sql("EXPLAIN " + sql, params).rows]

    def _plan_parquet_file(self, path: str, table: str, key: str | None) -> dict:
        return plan_parquet(path, table=table, key=key, db=None if isinstance(self._db, _Pending) else self)

    def _create(self, schema: Mapping[str, Any]) -> None:
        if isinstance(self._db, _Pending):
            p = self._db
            self._db = _strata.Database.create(p.path, json.dumps(schema), p.directory, None if p.models is None else os.fspath(p.models), p.threads)
            self._db.set_limits(*self._limits)
        else:
            self._db.sql(_strata.create_sql(json.dumps(schema)), None)

    def _load(self, table: str, data: Any) -> dict:
        if not hasattr(data, "__arrow_c_stream__") and not type(data).__module__.startswith("pandas"):
            data = _arrow_table(data)
        return self.import_arrow(data, table=table)


def open(path: str | os.PathLike, schema: Any = None, *, directory: bool = False, models: str | os.PathLike | None = None, threads: int | None = None, read_only: bool = False, recover: bool = False, timeout_ms: int | None = None, memory: int | None = None) -> Database:
    """Opens the database at `path`, creating it with `schema` if it does
    not exist. `threads`: how many cores vector indexes build on and each
    query searches segments on (default: every core). `read_only`: opens
    it to read while another process writes it; `recover`: opens a
    damaged database accepting the loss of acknowledged writes (see
    `Database.open`). `timeout_ms` and `memory` limit its queries (see
    `Database.set_limits`)."""
    if os.path.exists(path) or read_only:
        return Database.open(path, models=models, threads=threads, read_only=read_only, recover=recover, timeout_ms=timeout_ms, memory=memory)
    if schema is None:
        raise ChloeDBError(f"{os.fspath(path)} does not exist: pass a schema to create it")
    return Database.create(path, schema, directory=directory, models=models, threads=threads, timeout_ms=timeout_ms, memory=memory)


def from_sqlite(sqlite_path: str | os.PathLike, path: str | os.PathLike, *, tables: str | Iterable[str] | None = None, schemas: Any = None, blobs_base64: bool = False, directory: bool = False, models: str | os.PathLike | None = None, threads: int | None = None) -> Database:
    """Creates a database at `path` from the tables of a SQLite database
    file (all, or `tables`), with schemas made from the SQLite tables or
    given in `schemas` (see `plan_sqlite`). What the import did, as
    `Database.import_sqlite` returns it, is the database's `last_import`."""
    inner, report = _strata.Database.from_sqlite(os.fspath(sqlite_path), os.fspath(path), _names(tables), _schemas_json(schemas), blobs_base64, directory, None if models is None else os.fspath(models), threads)
    db = Database(inner)
    db.last_import = json.loads(report)
    return db


def plan_sqlite(sqlite_path: str | os.PathLike, *, tables: str | Iterable[str] | None = None, schemas: Any = None, blobs_base64: bool = False) -> dict:
    """What `from_sqlite` would make of a SQLite file, without writing:
    {tables: [...], skipped: {name: why}}, each table the schema of the
    ChloeDB table it becomes (with its `source`, `columns` and `notes`). Edit
    the plan and pass it to `from_sqlite` as `schemas`."""
    return json.loads(_strata.Database.plan_sqlite(os.fspath(sqlite_path), _names(tables), _schemas_json(schemas), blobs_base64))


def from_parquet(parquet_path: str | os.PathLike, path: str | os.PathLike, *, table: str | None = None, schema: Any = None, key: str | None = None, blobs_base64: bool = False, directory: bool = False, models: str | os.PathLike | None = None) -> Database:
    """Creates a database at `path` from a Parquet file or directory (or an
    `s3://` URL), its one table made from the columns, kept by an export,
    or given as `schema` (see `plan_parquet`). What the import did, as
    `Database.import_parquet` returns it, is the database's
    `last_import`."""
    inner, report = _strata.Database.from_parquet(os.fspath(parquet_path), os.fspath(path), table, None if schema is None else _schema_json(schema), key, blobs_base64, directory, None if models is None else os.fspath(models))
    db = Database(inner)
    db.last_import = json.loads(report)
    return db


def plan_parquet(parquet_path: str | os.PathLike, *, table: str | None = None, schema: Any = None, key: str | None = None, blobs_base64: bool = False, db: "Database | None" = None) -> dict:
    """What `from_parquet` (or, with `db`, `db.import_parquet`) would make
    of a Parquet file or directory, without writing: the table's schema
    with whether it `exists`, the `files` and `rows`, the `columns` read
    and the fields they fill, the Hive `partitions`, what is `skipped` and
    `notes`. Edit the plan and pass it back as `schema`."""
    return json.loads(_strata.Database.plan_parquet(os.fspath(parquet_path), table, None if schema is None else _schema_json(schema), key, blobs_base64, None if db is None else db._db))


def connect(url: str | os.PathLike = "http://127.0.0.1:7700", *, token: str | None = None, timeout: float = 30.0, retry_for: float = 10.0, models: str | os.PathLike | None = None, threads: int | None = None, directory: bool = False) -> Client | Database:
    """A store, for frames and queries alike: a `Client` of the
    chloedb-server (or coordinator) at an `http://` or `https://` URL, or
    the `Database` at a path, in this process. A path that does not exist
    yet makes a database at its first `create_table`. The code after it is
    the same either way: `connect(...)["orders"]` is a frame of a table."""
    target = os.fspath(url)
    if target.startswith(("http://", "https://")):
        return Client(target, token=token, timeout=timeout, retry_for=retry_for)
    if "://" in target:
        raise ChloeDBError(f"connect takes a path or an http(s):// URL, not {target!r}")
    if target.startswith("file:"):
        target = target[len("file:"):]
    if os.path.exists(target):
        return Database.open(target, models=models, threads=threads)
    return Database(_Pending(target, directory, models, threads))

