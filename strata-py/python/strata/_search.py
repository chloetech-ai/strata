"""What `Database` and `Client` share: the schema's reading order,
parameters, `table` and `search`."""

from __future__ import annotations

from typing import Any, Iterable, Mapping

from ._strata import StrataError
from .build import Expr, Query, bm25, hops, rrf, score, similarity
from .result import Result


def ordered_schema(s: dict) -> dict:
    """The short schema form in reading order: table, key, fields, and each
    field's name and type first."""
    out = {"table": s["table"]}
    if s.get("key") is not None:
        out["key"] = s["key"]
    out["fields"] = [{"name": f["name"], "type": f["type"], **f} for f in s["fields"]]
    return out


def params(given: Mapping[str, Any] | None, kw: Mapping[str, Any]) -> dict | None:
    """Query parameters from a dict and keyword arguments."""
    if not given and not kw:
        return None
    return {**(given or {}), **kw}


def quote(name: str) -> str:
    """A name as an SQL identifier, whatever it is."""
    return '"' + name.replace('"', '""') + '"'


class Search:
    """`table` and `search`, shared by `Database` and `Client`, which give
    `schema`, `_embedded()` (the embedded vector fields whose model is
    open), `embed`, `query` and `run`."""

    def table(self, name: str | None = None) -> Query:
        """A query of the table, built from Python values (`strata.build`):
        `db.table().where(col("year") >= 2019).order_by(bm25("body",
        q)).limit(10)`."""
        s = self.schema
        if name is not None and name.lower() != s["table"].lower():
            raise StrataError(f"no table {name}: this serves {s['table']}")
        return Query(self, s)

    def search(
        self,
        text: str,
        *,
        limit: int = 10,
        where: Expr | str | None = None,
        columns: str | Iterable[str] = "*",
        mode: str = "auto",
        text_fields: Iterable[str] | None = None,
        vector_field: str | None = None,
        params: Mapping[str, Any] | None = None,
    ) -> Result:
        """The rows that best match `text`.

        `mode` "text" ranks by BM25 over the text fields, "vector" by the
        similarity of an embedded vector field to the text's embedding, and
        "hybrid" fuses both by reciprocal rank (RRF). "auto" is hybrid when
        the table has an embedded field whose model is open here, and text
        otherwise. The text fields are those generated from other fields if
        there are any (a combined "body"), else every indexed text field.
        `where` is a condition (`col("year") >= 2000`), or an SQL condition
        with `params` for its placeholders.
        """
        s = self.schema
        if text_fields is None:
            texts = [f for f in s["fields"] if f["type"] == "text" and f.get("indexed", True)]
            generated = [f["name"] for f in texts if f.get("from")]
            text_fields = generated or [f["name"] for f in texts]
        text_fields = list(text_fields)
        if vector_field is None:
            embedded = self._embedded()
            vector_field = embedded[0] if embedded else None
        if mode == "auto":
            mode = "hybrid" if vector_field and text_fields else "vector" if vector_field else "text"
        scores = []
        if mode in ("text", "hybrid"):
            if not text_fields:
                raise StrataError(f"{s['table']} has no indexed text field to search")
            scores += [f"bm25({quote(f)}, :_q)" for f in text_fields]
        if mode in ("vector", "hybrid"):
            if not vector_field:
                raise StrataError(f"{s['table']} has no embedded vector field with its model open")
            scores.append(f"similarity({quote(vector_field)}, :_v)")
        if mode not in ("text", "vector", "hybrid"):
            raise ValueError('mode is "auto", "text", "vector" or "hybrid"')
        if isinstance(where, str) and not where.strip():
            where = None
        if not isinstance(where, Expr):
            # SQL, as before: the names of fields quoted, so that any name
            # works; other columns (score(), an expression) as written.
            fields = {f["name"].lower() for f in s["fields"]}
            name = lambda c: quote(c) if c.lower() in fields else c
            cols = columns if isinstance(columns, str) else ", ".join(name(c.strip()) for c in columns)
            order = scores[0] if len(scores) == 1 else f"rrf({', '.join(scores)})"
            sql = f"SELECT {cols} FROM {s['table']}"
            if where is not None:
                sql += f" WHERE {where}"
            sql += f" ORDER BY {order} DESC LIMIT {int(limit)}"
            p = dict(params or {})
            if "bm25" in order:
                p["_q"] = text
            if "similarity" in order:
                p["_v"] = {"embed": text, "field": vector_field}
            return self.query(sql, p)
        # A condition built in Python: the query is a plan.
        ranks = [bm25(f, text) for f in text_fields] if mode in ("text", "hybrid") else []
        if mode in ("vector", "hybrid"):
            ranks.append(similarity(vector_field, self.embed(text, vector_field)))
        q = self.table().where(where).order_by(ranks[0] if len(ranks) == 1 else rrf(*ranks)).limit(limit)
        if columns != "*":
            names = [c.strip() for c in columns.split(",")] if isinstance(columns, str) else list(columns)
            q = q.select(*[{"score()": score(), "hops()": hops()}.get(c.lower(), c) for c in names])
        return q.run()
