"""Next items from counted transitions: how often each item came right
after the last one or two items, kept in a table and read with stupid
backoff (Brants et al., 2007)::

    from strata import ngrams

    db.sql(ngrams.create_table("next"))
    db.write(ngrams.ops(["a", "b"], "c", table="next"))  # after each event
    strata.next_items(db, ["a", "b"], table="next")      # [("c", 1.0)]

Each row counts one item after one context: its key is the context's
items, then the item, so a context's rows sit together with
``cluster_by = 'id'``. `ops` gives an ``add`` per order, which creates the
row on its first event; `next_items` reads the longest context first and
backs off to shorter ones at `backoff` of their weight. The same works
over `Client` and `Database`, and from any language with the SQL that
`next_items` runs (docs: Next items).
"""

from __future__ import annotations

from typing import Any, Iterable

from .build import col

#: Between a context's items, and between a context and the next item.
SEP = "\x1f"
NEXT = "\x1e"


def context(items: Iterable[str]) -> str:
    """The key part of a context: its items, oldest first."""
    items = list(items)
    for x in items:
        if not isinstance(x, str):
            raise TypeError(f"items are str, not {type(x).__name__}")
        if SEP in x or NEXT in x:
            raise ValueError(f"an item cannot hold \\x1f or \\x1e: {x!r}")
    return SEP.join(items)


def create_table(table: str = "next") -> str:
    """The table's `CREATE TABLE` statement: a key, the context, the next
    item and a weight, which `UPDATE ... SET n = n * 0.9` can decay."""
    return (
        f'CREATE TABLE "{table}" (id KEYWORD PRIMARY KEY, ctx KEYWORD NOT NULL, nxt KEYWORD NOT NULL, n FLOAT DEFAULT 0) '
        "OPTIONS(cluster_by = 'id')"
    )


def schema(table: str = "next") -> dict:
    """The same table as a schema, for `strata.open(path, schema)` or
    `strata-server --schema`."""
    return {
        "table": table,
        "key": "id",
        "cluster_by": "id",
        "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "ctx", "type": "keyword", "not_null": True},
            {"name": "nxt", "type": "keyword", "not_null": True},
            {"name": "n", "type": "float", "default": 0},
        ],
    }


def ops(history: Iterable[str], item: str, *, table: str = "next", order: int = 2, weight: float = 1) -> list[dict]:
    """The write operations for `item` after `history` (oldest first):
    `weight` added to the row of `item` after the last 1, ..., `order`
    items, each created on its first event. Pass them to `write`."""
    history = list(history)
    nxt = context([item])
    out = []
    for m in range(1, min(order, len(history)) + 1):
        c = context(history[-m:])
        out.append({"op": "add", "table": table, "field": "n", "value": weight, "doc": {"id": c + NEXT + nxt, "ctx": c, "nxt": nxt}})
    return out


def next_items(db: Any, history: Iterable[str], k: int = 10, *, table: str = "next", order: int = 2, backoff: float = 0.4) -> list[tuple[str, float]]:
    """The `k` items most likely to come after `history` (oldest first), as
    (item, score), best first, ties by item.

    An item seen after the last `order` items scores its share of that
    context's weight; one seen only after a shorter context scores its
    share there times `backoff` per order backed off. A context never
    seen backs off the same way. The answer equals scoring every item, but
    reads only the best rows of each context, and of the shorter contexts
    the rows that can still make the top `k`. `db` is a `Database` or a
    `Client`."""
    history = list(history)
    if k <= 0:
        return []
    levels = [context(history[-m:]) for m in range(min(order, len(history)), 0, -1)]
    t = db.table(table)
    scores: dict[str, float] = {}
    # Per context read so far: the items read, and whether that was all.
    read: list[tuple[str, set[str], bool]] = []
    page = max(4 * k, 64)

    def kth() -> float:
        return sorted(scores.values(), reverse=True)[k - 1] if len(scores) >= k else -1.0

    def seen_after(c: str, got: set[str], whole: bool, items: list[str]) -> set[str]:
        if whole:
            return {x for x in items if x in got}
        keys = [c + NEXT + x for x in items if x not in got]
        rows = t.select("nxt").where(col("id").isin(keys), col("n") > 0).limit(len(keys)).run() if keys else []
        return {x for x in items if x in got} | {r["nxt"] for r in rows}

    for i, c in enumerate(levels):
        here = t.where(col("ctx") == c, col("n") > 0)
        total = here.select(col("n").sum()).run().scalar() or 0
        if total <= 0:
            read.append((c, set(), True))
            continue
        factor = backoff**i
        got: set[str] = set()
        new = 0
        offset = 0
        whole = False
        while True:
            # Ties by key, which within a context is by item.
            rows = here.select("nxt", "n").order_by(col("n").desc(), col("id")).limit(page).offset(offset).run().rows
            offset += len(rows)
            if len(rows) < page:
                whole = True
            bar = kth()
            fresh = []
            stop = False
            for nxt, n in rows:
                s = factor * n / total
                if s < bar:
                    stop = True
                    break
                got.add(nxt)
                if nxt not in scores:
                    fresh.append((nxt, s))
            # Items a longer context has: their score is that context's.
            names = [x for x, _ in fresh]
            taken: set[str] = set()
            for hc, hgot, hwhole in read:
                taken |= seen_after(hc, hgot, hwhole, names)
            for x, s in fresh:
                if x not in taken and new < k:
                    scores[x] = s
                    new += 1
            if stop or whole or new >= k:
                whole = whole and not stop
                break
        read.append((c, got, whole))
    return sorted(scores.items(), key=lambda kv: (-kv[1], kv[0]))[:k]
