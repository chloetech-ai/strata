"""A query's answer."""

from __future__ import annotations

import json
from typing import Any, Iterator


class Result:
    """A query's answer: `columns`, `rows` (lists of values in column
    order), each row's primary key in `keys` and the score it was ranked by
    in `scores`, and what the engine did in `stats`.

    Iterating gives each row as a dict of column to value; `result[i]` is
    row i as a dict, and `len(result)` the number of rows.
    """

    __slots__ = ("columns", "rows", "keys", "scores", "took_ms", "request", "teams", "_stats")

    def __init__(self, columns: list[str], rows: list[list[Any]], keys: list[Any] | None = None, scores: list[float] | None = None, stats: Any = None, took_ms: float | None = None, request: str | None = None, teams: list[str | None] | None = None):
        self.columns = columns
        self.rows = rows
        self.keys = keys or []
        self.scores = scores or []
        self.took_ms = took_ms
        # The request's id in the rank log (`query_logged`); else None.
        self.request = request
        # An interleaved query's: per row, the ranking that placed it.
        self.teams = teams
        self._stats = stats

    @classmethod
    def _of(cls, d: dict) -> "Result":
        return cls(d["columns"], d["rows"], d.get("keys"), d.get("scores"), d.get("stats"), d.get("took_ms"), d.get("request"), d.get("teams"))

    @property
    def stats(self) -> dict:
        """What the engine did: segments, candidates, time per stage."""
        if isinstance(self._stats, str):
            self._stats = json.loads(self._stats) if self._stats else {}
        return self._stats or {}

    def __len__(self) -> int:
        return len(self.rows)

    def __iter__(self) -> Iterator[dict]:
        cols = self.columns
        return (dict(zip(cols, r)) for r in self.rows)

    def __getitem__(self, i: int) -> dict:
        return dict(zip(self.columns, self.rows[i]))

    def dicts(self) -> list[dict]:
        """Every row as a dict of column to value."""
        return list(self)

    def column(self, name: str) -> list[Any]:
        """One column's values."""
        i = self.columns.index(name)
        return [r[i] for r in self.rows]

    def scalar(self) -> Any:
        """The first column of the first row (None if there is none): a
        count, a sum."""
        return self.rows[0][0] if self.rows and self.rows[0] else None

    def to_pandas(self):
        """The rows as a pandas DataFrame (pandas must be installed)."""
        import pandas

        return pandas.DataFrame(self.rows, columns=self.columns)

    def __repr__(self) -> str:
        return f"<strata.Result {len(self.rows)} row{'' if len(self.rows) == 1 else 's'}: {', '.join(self.columns)}>"


class ArrowResult:
    """A query's answer as Arrow columns (`Database.query_arrow`): one
    record batch, held by the database's process, which pyarrow, polars,
    pandas and DuckDB read without copying it (the Arrow PyCapsule
    interface, ``__arrow_c_stream__``).

    `to_pyarrow()`, `to_polars()` and `to_pandas()` make a table or a data
    frame of it; any library that takes Arrow streams takes the result
    itself (``pyarrow.table(r)``, ``polars.DataFrame(r)``,
    ``duckdb.sql("SELECT * FROM r")``).

    INT is Int64, FLOAT Float64, text and keywords Utf8, BOOL Boolean,
    TIMESTAMP microseconds in UTC, a vector a list of Float32, a list
    field a list of Int64 or Utf8, and a column of NULLs Null.
    """

    __slots__ = ("_a", "_stats")

    def __init__(self, a: Any):
        self._a = a
        self._stats = None

    @property
    def columns(self) -> list[str]:
        return self._a.columns

    @property
    def num_rows(self) -> int:
        return self._a.num_rows

    def __len__(self) -> int:
        return self._a.num_rows

    @property
    def stats(self) -> dict:
        """What the engine did: segments, candidates, time per stage."""
        if self._stats is None:
            self._stats = json.loads(self._a.stats) if self._a.stats else {}
        return self._stats

    def __arrow_c_stream__(self, requested_schema: Any = None) -> Any:
        return self._a.__arrow_c_stream__(requested_schema)

    def __arrow_c_schema__(self) -> Any:
        return self._a.__arrow_c_schema__()

    def to_pyarrow(self):
        """A pyarrow Table of the columns, sharing their memory (pyarrow
        must be installed)."""
        import pyarrow

        return pyarrow.table(self)

    def to_polars(self):
        """A polars DataFrame of the columns, sharing their memory (polars
        must be installed; pyarrow need not be)."""
        import polars

        return polars.DataFrame(self)

    def to_pandas(self, *, arrow_dtypes: bool = True):
        """A pandas DataFrame of the columns (pandas and pyarrow must be
        installed): backed by the Arrow columns themselves (dtypes
        ``pandas.ArrowDtype``), or, with ``arrow_dtypes=False``, copied
        into numpy arrays (pyarrow's ``Table.to_pandas``)."""
        import pandas

        t = self.to_pyarrow()
        return t.to_pandas(types_mapper=pandas.ArrowDtype) if arrow_dtypes else t.to_pandas()

    def __repr__(self) -> str:
        n = self.num_rows
        return f"<strata.ArrowResult {n} row{'' if n == 1 else 's'}: {', '.join(self.columns)}>"
