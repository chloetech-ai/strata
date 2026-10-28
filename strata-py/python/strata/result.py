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

    __slots__ = ("columns", "rows", "keys", "scores", "took_ms", "_stats")

    def __init__(self, columns: list[str], rows: list[list[Any]], keys: list[Any] | None = None, scores: list[float] | None = None, stats: Any = None, took_ms: float | None = None):
        self.columns = columns
        self.rows = rows
        self.keys = keys or []
        self.scores = scores or []
        self.took_ms = took_ms
        self._stats = stats

    @classmethod
    def _of(cls, d: dict) -> "Result":
        return cls(d["columns"], d["rows"], d.get("keys"), d.get("scores"), d.get("stats"), d.get("took_ms"))

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
