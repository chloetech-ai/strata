"""The compiled extension, `chloedb._chloedb`, which runs the database in
this process. The HTTP client (`chloedb.Client`, `chloedb.connect`) needs
only the standard library, so without the extension (a checkout's
``chloedb-py/python`` on the path, with no build) the package still imports
and the client works: `_strata` then stands in for the extension and says
what to install when anything of it is used."""

from __future__ import annotations

import re
from typing import Any

WHEELS = "https://github.com/chloetech-ai/chloedb/releases/expanded_assets/chloedb-py-v0.3.3"


class _Missing:
    """In place of the extension: any use of it raises ImportError."""

    def __init__(self, error: ImportError):
        self._error = error

    def __getattr__(self, name: str) -> Any:
        raise ImportError(
            f"chloedb's compiled extension is not installed ({self._error}), and {name} needs it: "
            "build it (pip install ./chloedb-py from the repository's root, or maturin develop in chloedb-py), "
            f"or install a release's wheel (pip install chloedb -f {WHEELS}). "
            "chloedb.Client and chloedb.connect, which talk to a chloedb-server, work without it."
        )


try:
    from . import _chloedb as _strata

    ChloeDBError = _strata.ChloeDBError
    NATIVE = True
except ImportError as e:  # the HTTP client alone
    _strata = _Missing(e)  # type: ignore[assignment]
    NATIVE = False

    class ChloeDBError(Exception):  # type: ignore[no-redef]
        """An error ChloeDB reports. A client's has the answer's HTTP
        `status`, and its JSON body as `answer`."""


# Its name when ChloeDB was called Strata: the same class.
StrataError = ChloeDBError


# ---------------------------------------------------------------- codes
#
# Every error has a stable `code`, the same in this process and from a
# server: the server's JSON `code` when it gives one, else the kind its
# message starts with ("busy: ...", as the engine words its errors), else
# the HTTP status. The codes with a subclass of ChloeDBError are raised as
# that subclass, so `except ChloeDBError` still catches every one.

# The engine's message prefixes (chloedb::Error's Display), and their codes.
_PREFIXES = (
    ("busy:", "busy"),
    ("timeout:", "timeout"),
    ("memory:", "memory"),
    ("unavailable:", "unavailable"),
    ("corrupt database:", "corrupt"),
    ("I/O error:", "io"),
    ("schema error:", "bad_request"),
    ("query error:", "bad_request"),
)

_STATUSES = {400: "bad_request", 401: "unauthorized", 403: "unauthorized", 404: "not_found", 408: "timeout", 409: "conflict", 413: "bad_request", 422: "bad_request", 429: "overloaded", 503: "unavailable", 504: "timeout"}


def error_code(message: str, status: int | None = None, answer: Any = None) -> str:
    """The code of an error: one of "bad_request", "unauthorized",
    "not_found", "conflict", "partial_write", "busy", "overloaded",
    "timeout", "memory", "unavailable", "corrupt", "io", "internal" (any
    other 5xx) and "error" (anything else)."""
    if isinstance(answer, dict):
        if isinstance(answer.get("code"), str) and answer["code"]:
            return answer["code"]
        if answer.get("partial") is True:
            return "partial_write"
    text = message.lstrip()
    for prefix, code in _PREFIXES:
        if text.startswith(prefix):
            return code
    if status is not None:
        return _STATUSES.get(int(status), "internal" if int(status) >= 500 else "error")
    return "error"


def _get_code(self: Any) -> str:
    code = self.__dict__.get("_code")
    if code is None:
        code = error_code(str(self.args[0]) if self.args else "", getattr(self, "status", None), getattr(self, "answer", None))
    return code


def _set_code(self: Any, code: str) -> None:
    self.__dict__["_code"] = code


# On the extension's class too, so every error it raises has one.
ChloeDBError.code = property(_get_code, _set_code, doc="The error's stable code (see chloedb.error_code).")  # type: ignore[attr-defined]


class BadRequestError(ChloeDBError):
    """A bad schema, query, value or request (HTTP 400, 413, 422): sending
    it again fails again."""


class UnauthorizedError(ChloeDBError):
    """No token, or one without the rights the request needs (401, 403)."""


class NotFoundError(ChloeDBError):
    """No such row, table, route or cursor (404)."""


class ConflictError(ChloeDBError):
    """Refused by the state it met (409): a conditional write's `if_seq_no`
    that no longer holds, an idempotency key still running, a read past the
    server's last write."""


class DuplicateKeyError(ConflictError, BadRequestError):
    """A write of a row whose primary key the table already holds (an
    INSERT, without OR REPLACE or ON CONFLICT). Its code is "conflict",
    and it is a ConflictError and a BadRequestError both (it was the
    latter alone before 0.3.2). `table` is the table, `field` its key
    field, `key` the key, and `row` the row's position in the write (from
    1) when the message names it; each is None when it does not."""

    table: str | None = None
    field: str | None = None
    key: Any = None
    row: int | None = None


class PartialWriteError(ChloeDBError):
    """A coordinator wrote some shards and not others (`answer["partial"]`):
    send it again with the same `idempotency_key`."""


class BusyError(ChloeDBError):
    """The database is open elsewhere, or another writer holds it."""


class OverloadedError(ChloeDBError):
    """Refused for the server's load before any work (429); the client
    sends it again for `retry_for` seconds first."""


class DeadlineError(ChloeDBError):
    """A deadline passed: a query's `timeout_ms`, a coordinator's wait for
    a shard (504), or the engine's own."""


class UnavailableError(ChloeDBError):
    """No copy can serve it now (503): a replica behind, a primary
    replaced, a hosted model down."""


ERRORS: dict[str, type] = {
    "bad_request": BadRequestError,
    "unauthorized": UnauthorizedError,
    "not_found": NotFoundError,
    "conflict": ConflictError,
    "partial_write": PartialWriteError,
    "busy": BusyError,
    "overloaded": OverloadedError,
    "timeout": DeadlineError,
    "unavailable": UnavailableError,
}


# The engine's words for a duplicate key (chloedb::query::hint::duplicate_key),
# after any "row N: " and a coordinator's "s1-0 (shard 1): ".
_DUPLICATE = re.compile(r"(?:row (\d+): )?(?:schema error: )?duplicate primary key: (\S+) already has a row with (\S+) (.+?) \((?:INSERT OR REPLACE|$)")


def _sql_value(text: str) -> Any:
    """A key as the message writes it (an SQL literal) as a Python value."""
    if len(text) >= 2 and text[0] == "'" and text[-1] == "'":
        return text[1:-1].replace("''", "'")
    for kind in (int, float):
        try:
            return kind(text)
        except ValueError:
            pass
    return text


def make_error(message: str, *, status: int | None = None, answer: Any = None, code: str | None = None) -> Any:
    """A ChloeDBError of the class its code names, with `status`, `answer`
    and `code` set. A duplicate key is a DuplicateKeyError, whatever code
    it came with."""
    dup = _DUPLICATE.search(message)
    if dup is not None and (code or error_code(message, status, answer)) in ("bad_request", "conflict", "error"):
        err = DuplicateKeyError(message)
        row, err.table, err.field, key = dup.groups()
        err.key = _sql_value(key)
        err.row = None if row is None else int(row)
        code = "conflict"
    else:
        code = code or error_code(message, status, answer)
        err = ERRORS.get(code, ChloeDBError)(message)
    err.status = status
    err.answer = answer
    err.code = code
    return err


def _error(code: str, message: str) -> Any:
    """The error the extension raises for an engine error of this code."""
    return make_error(message, code=code)
