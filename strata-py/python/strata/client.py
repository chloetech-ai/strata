"""A client of a running strata-server, over its HTTP JSON API, with the
same methods as `Database` where the server offers them. It needs only the
standard library."""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Iterable, Mapping

from ._search import Search, ordered_schema, params as _params
from ._strata import StrataError
from .build import Query
from .result import Result

# Rows a write request carries.
WRITE_BATCH = 1000

FORMATS = {".jsonl": "jsonl", ".ndjson": "jsonl", ".json": "json", ".csv": "csv", ".tsv": "tsv", ".tab": "tsv"}
# The content type an import is sent with: a server refuses text/plain.
CONTENT_TYPES = {"jsonl": "application/x-ndjson", "json": "application/json", "csv": "text/csv", "tsv": "text/tab-separated-values"}


def _default(o: Any) -> Any:
    # numpy arrays and scalars
    if hasattr(o, "tolist"):
        return o.tolist()
    # datetimes and dates, as a TIMESTAMP's ISO 8601 text (naive is UTC)
    if hasattr(o, "isoformat") and hasattr(o, "year"):
        return o.isoformat()
    raise TypeError(f"cannot send a {type(o).__name__}")


def _rows(rows: Any) -> list:
    return [rows] if isinstance(rows, Mapping) else list(rows)


class Client(Search):
    """A strata-server at `url` ("http://host:7700"). `token` is the
    server's --token, if it has one. Errors the server reports raise
    `StrataError`; one it cannot be reached raises `urllib.error.URLError`.
    """

    def __init__(self, url: str = "http://127.0.0.1:7700", *, token: str | None = None, timeout: float = 30.0):
        self.url = url.rstrip("/")
        self.token = token if token is not None else os.environ.get("STRATA_TOKEN")
        self.timeout = timeout
        self._schema: dict | None = None

    def _call(self, method: str, path: str, body: Any = None, *, data: Any = None, headers: Mapping[str, str] | None = None) -> Any:
        h = dict(headers or {})
        if body is not None:
            data = json.dumps(body, default=_default).encode()
            h["content-type"] = "application/json"
        if self.token:
            h["authorization"] = f"Bearer {self.token}"
        req = urllib.request.Request(self.url + path, data=data, method=method, headers=h)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                return json.loads(r.read() or b"null")
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                answer = json.loads(raw)
                msg = answer.get("error") or raw.decode(errors="replace")
            except (ValueError, AttributeError):
                answer, msg = None, raw.decode(errors="replace") or str(e)
            err = StrataError(f"{msg} (HTTP {e.code})")
            err.status = e.code
            err.answer = answer
            raise err from None

    def _schema_answer(self) -> dict:
        if self._schema is None:
            self._schema = self._call("GET", "/v1/schema")
        return self._schema

    @property
    def schema(self) -> dict:
        """The first table's schema in its short form: {table, key, fields}."""
        return ordered_schema(self._schema_answer()["short"])

    def tables(self) -> list[dict]:
        """Every table's schema in its short form, the first first."""
        return [ordered_schema(t["short"]) for t in self._call("GET", "/v1/tables")["tables"]]

    def _embedded(self) -> list[str]:
        return [e["field"] for e in self._schema_answer().get("embeddings", []) if e.get("loaded")]

    def health(self) -> dict:
        """{status, docs, segments, role, ...}, also when the status is "down"
        or "failing" (which the server answers with 503)."""
        try:
            return self._call("GET", "/v1/health")
        except StrataError as e:
            if e.status == 503 and isinstance(e.answer, dict) and "status" in e.answer:
                return e.answer
            raise

    def stats(self) -> dict:
        return self._call("GET", "/v1/stats")

    def __len__(self) -> int:
        return int(self.health()["docs"])

    def query(self, sql: str, params: Mapping[str, Any] | None = None, *, timeout_ms: int | None = None, **kw: Any) -> Result:
        """Runs a SELECT."""
        body: dict[str, Any] = {"sql": sql, "params": _params(params, kw)}
        if timeout_ms:
            body["timeout_ms"] = timeout_ms
        return Result._of(self._call("POST", "/v1/query", body))

    def sql(self, sql: str, params: Mapping[str, Any] | None = None, **kw: Any) -> Result | int:
        """Runs any statement: a SELECT's `Result`, or the number of rows an
        INSERT, UPDATE or DELETE wrote."""
        r = self._call("POST", "/v1/sql", {"sql": sql, "params": _params(params, kw)})
        return Result._of(r) if "rows" in r else int(r["written"])

    def explain(self, sql: str, params: Mapping[str, Any] | None = None, **kw: Any) -> dict:
        """Runs the query and returns its plan: the plan IR as JSON ("ir")
        and as lines ("tree"), the executor's plan lines ("plan") and the
        bound query ("logical")."""
        return self._call("POST", "/v1/explain", {"sql": sql, "params": _params(params, kw)})

    def run(self, plan: Mapping[str, Any] | Query) -> Result | int:
        """Runs a plan (the "ir" of `explain`, or a `Query`): a read's
        `Result`, or the number of rows a write wrote."""
        if isinstance(plan, Query):
            plan = plan.plan()
        r = self._call("POST", "/v1/plan", {"plan": plan})
        return Result._of(r) if "rows" in r else int(r["written"])

    def embed(self, text: str, field: str | None = None, *, table: str | None = None) -> list[float]:
        """The query vector of `text` by an embedded field's model (of the
        first table, or of `table`), on the server."""
        body = {k: v for k, v in {"text": text, "field": field, "table": table}.items() if v is not None}
        return self._call("POST", "/v1/embed", body)["vector"]

    def write(self, ops: list[dict]) -> int:
        """Write operations as /v1/write takes them ({"op": "insert",
        "doc": {...}}, {"op": "delete", "key": k}, ...), in one request."""
        return int(self._call("POST", "/v1/write", {"ops": ops})["written"])

    def _ops(self, op: str, rows: list, table: str | None) -> int:
        named = {} if table is None else {"table": table}
        n = 0
        for i in range(0, len(rows), WRITE_BATCH):
            n += self.write([{"op": op, **named, "doc": r} for r in rows[i : i + WRITE_BATCH]])
        return n

    def insert(self, rows: Mapping[str, Any] | Iterable[Mapping[str, Any]], *, table: str | None = None) -> int:
        """Inserts a row or rows, whose keys must be new, into the first
        table or `table`, 1,000 rows a request. Returns how many."""
        return self._ops("insert", _rows(rows), table)

    def upsert(self, rows: Mapping[str, Any] | Iterable[Mapping[str, Any]], *, table: str | None = None) -> int:
        """Inserts a row or rows, replacing the rows that have their keys,
        1,000 rows a request. Returns how many."""
        return self._ops("upsert", _rows(rows), table)

    def delete(self, keys: Any, *, table: str | None = None, routing: Any = None) -> int:
        """Deletes the rows with this key or these keys. Through a
        coordinator, `routing` gives the `shard_by` value of a table placed
        by a column."""
        keys = [keys] if isinstance(keys, (str, int)) else list(keys)
        named = {k: v for k, v in {"table": table, "routing": routing}.items() if v is not None}
        n = 0
        for i in range(0, len(keys), WRITE_BATCH):
            n += self.write([{"op": "delete", **named, "key": k} for k in keys[i : i + WRITE_BATCH]])
        return n

    def stream(self, events: Iterable[Mapping[str, Any]]) -> dict:
        """Writes `events` through /v1/stream as they come, in batches the
        server makes: each an operation, or one event's operations as
        {"ops": [...], "source": {"name": ..., "offset": n, "ts": ...}},
        which a batch never splits. A source's offsets increase along the
        stream; one the server has reached already (a replay's) is skipped,
        so resuming from `source(name) + 1`, or from before it, applies
        each event once. Returns the last answer, {"done": True, "lines",
        "written", "skipped", "batches"}; a failure raises StrataError,
        whose `answer["lines"]` counts the events written before it."""

        def lines() -> Iterable[bytes]:
            chunk: list[str] = []
            for e in events:
                chunk.append(json.dumps(e, default=_default))
                if len(chunk) >= 256:
                    yield ("\n".join(chunk) + "\n").encode()
                    chunk = []
            if chunk:
                yield ("\n".join(chunk) + "\n").encode()

        h = {"content-type": "application/x-ndjson"}
        if self.token:
            h["authorization"] = f"Bearer {self.token}"
        req = urllib.request.Request(self.url + "/v1/stream", data=lines(), method="POST", headers=h)
        last: dict = {}
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                for raw in r:
                    if raw.strip():
                        last = json.loads(raw)
        except urllib.error.HTTPError as e:
            raw = e.read()
            err = StrataError(f"{raw.decode(errors='replace')} (HTTP {e.code})")
            err.status, err.answer = e.code, None
            raise err from None
        if "error" in last or not last.get("done"):
            err = StrataError(f"{last.get('error', 'the stream ended early')} (HTTP {last.get('status', 500)})")
            err.status, err.answer = last.get("status", 500), last
            raise err
        return last

    def source(self, name: str) -> int | None:
        """The offset of the last event written from stream source `name`,
        or None for a source not written from: resume from the next."""
        return self._call("GET", "/v1/sources/" + urllib.parse.quote(name, safe="/"))["offset"]

    def sources(self) -> list[dict]:
        """Every stream source written from: [{name, offset, updated}]."""
        return self._call("GET", "/v1/sources")["sources"]

    def import_file(self, path: str | os.PathLike, format: str | None = None, *, table: str | None = None) -> dict:
        """Uploads a JSON Lines, JSON (an array), CSV or TSV file (up to the
        server's 256 MB), in `format` or the one its extension names, into
        the first table or `table`; its rows are upserted. Returns {rows,
        bulk, skipped_fields}."""
        path = os.fspath(path)
        format = format or FORMATS.get(os.path.splitext(path)[1].lower())
        if not format:
            raise ValueError(f"name the format of {path}: jsonl, json, csv or tsv")
        with open(path, "rb") as f:
            size = os.fstat(f.fileno()).st_size
            ct = CONTENT_TYPES.get(format, "application/octet-stream")
            query = {"format": format} if table is None else {"format": format, "table": table}
            r = self._call("POST", "/v1/import?" + urllib.parse.urlencode(query), data=f, headers={"content-length": str(size), "content-type": ct})
        return {"rows": r["written"], "bulk": False, "skipped_fields": r.get("skipped_fields", [])}

    def ask(self, question: str, *, model: str | None = None, effort: str | None = None, api_key: str | None = None, max_steps: int | None = None) -> dict:
        """Asks the server's agent a question about the data: {answer,
        steps (the queries it ran), ...}. Needs an Anthropic API key, the
        server's or `api_key`."""
        body = {k: v for k, v in {"question": question, "model": model, "effort": effort, "api_key": api_key, "max_steps": max_steps}.items() if v is not None}
        return self._call("POST", "/v1/agent", body)

    def close(self) -> None:
        pass

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def __repr__(self) -> str:
        return f"<strata.Client {self.url}>"
