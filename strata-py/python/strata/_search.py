"""What `Database` and `Client` share: the schema's reading order,
parameters, and `search`."""

from __future__ import annotations

from typing import Any, Iterable, Mapping

from ._strata import StrataError
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


class Search:
    """`search`, shared by `Database` and `Client`, which give `schema`,
    `_embedded()` (the embedded vector fields whose model is open) and
    `query`."""

    def search(
        self,
        text: str,
        *,
        limit: int = 10,
        where: str | None = None,
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
        `where` is an SQL condition, with `params` for its placeholders.
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
            scores += [f"bm25({f}, :_q)" for f in text_fields]
        if mode in ("vector", "hybrid"):
            if not vector_field:
                raise StrataError(f"{s['table']} has no embedded vector field with its model open")
            scores.append(f"similarity({vector_field}, :_v)")
        if mode not in ("text", "vector", "hybrid"):
            raise ValueError('mode is "auto", "text", "vector" or "hybrid"')
        order = scores[0] if len(scores) == 1 else f"rrf({', '.join(scores)})"
        cols = columns if isinstance(columns, str) else ", ".join(columns)
        sql = f"SELECT {cols} FROM {s['table']}"
        if where:
            sql += f" WHERE {where}"
        sql += f" ORDER BY {order} DESC LIMIT {int(limit)}"
        p = dict(params or {})
        if "bm25" in order:
            p["_q"] = text
        if "similarity" in order:
            p["_v"] = {"embed": text, "field": vector_field}
        return self.query(sql, p)
