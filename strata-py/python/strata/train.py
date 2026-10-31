"""Training what a schema or a query names: semantic-ID codebooks, and
sequence encoders for `encode(model, rows)`.

A semantic-ID field (``{"name": "sid", "type": "keyword[]", "semantic_id":
{"from": "emb", "codebook": "items-rq"}}``) gives each row a code from its
vector, by a codebook of residual k-means centroids. Train the codebook
first, on a table's vectors or on vectors you have, then create the table
that names it::

    import strata

    hash = strata.train.codebook(db, "items-rq", field="emb")
    items = strata.open("items.strata", schema={
        "table": "items", "key": "id",
        "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "emb", "type": "vector(384)"},
            {"name": "sid", "type": "keyword[]",
             "semantic_id": {"from": "emb", "codebook": "items-rq"}},
        ],
    })

The codebook is saved as ``<models>/<name>/codebook.safetensors``; the
table records its SHA-256 and opens it from the same models directory.
"""

from __future__ import annotations

import json
import os
from typing import Any, Mapping

from . import _strata

__all__ = ["codebook", "sequence", "write_vectors"]


def codebook(
    source: Any,
    name: str,
    *,
    field: str | None = None,
    table: str | None = None,
    models: str | os.PathLike | None = None,
    levels: int = 3,
    k: int = 256,
    iterations: int = 25,
    sample: int | None = None,
    seed: int = 0,
) -> str:
    """Trains a semantic-ID codebook and saves it as `name` under `models`
    (default $STRATA_MODELS or ~/.local/share/strata/models); returns its
    SHA-256.

    `source` is a `Database`, whose VECTOR `field` (of `table`, the first
    table by default) it trains on, or the vectors themselves: a list of
    lists of numbers, or a 2-D numpy array. It has `levels` levels (1 to 4)
    of `k` centroids (1 to 256), each found by `iterations` rounds of
    k-means on up to `sample` vectors (100 per centroid by default) drawn
    with `seed`, so the same vectors and options give the same codebook. A
    name holds one codebook: training another under it is an error."""
    models = None if models is None else os.fspath(models)
    inner = getattr(source, "_db", None)
    if isinstance(inner, _strata.Database):
        if field is None:
            raise TypeError("training on a database needs the VECTOR field: field='emb'")
        return inner.train_codebook(field, name, table, models, levels, k, iterations, sample, seed)
    if field is not None or table is not None:
        raise TypeError("field and table name a database's vectors; pass the database")
    return _strata.train_codebook(source, name, models, levels, k, iterations, sample, seed)


def _key(source: Any, table: str) -> str:
    pk = next((t.get("key") for t in source.tables() if t.get("table") == table), None)
    if pk is None:
        raise ValueError(f"no table {table} with a primary key")
    return pk


def _count(source: Any, table: str) -> int:
    return int(source.query(f"SELECT COUNT(*) FROM {table}").rows[0][0])


def _items(source: Any, table: str, field: str) -> dict:
    """Every row's key and vector of VECTOR `field` of `table`."""
    pk = _key(source, table)
    r = source.query(f"SELECT {pk}, {field} FROM {table} LIMIT {max(_count(source, table), 1)}")
    return {k: v for k, v in r.rows if v is not None}


def _events(source: Any, ev: Mapping[str, str]) -> dict:
    """Each user's events of the events table `ev` names (`table`, `user`,
    `item`, and optionally `action`, `time` and `order`), oldest first."""
    t, user, item = ev["table"], ev["user"], ev["item"]
    action, time = ev.get("action"), ev.get("time")
    order = ev.get("order", time)
    if order is None:
        raise ValueError("events need a time or an order column: time='ts'")
    cols = [user, item] + [c for c in (action, time) if c]
    seqs: dict = {}
    r = source.query(f"SELECT {', '.join(cols)} FROM {t} ORDER BY {order} LIMIT {max(_count(source, t), 1)}")
    for row in r.rows:
        v = dict(zip(cols, row))
        seqs.setdefault(v[user], []).append((v[item], v.get(action) if action else None, _time(v.get(time)) if time else None))
    return seqs


def _time(t: Any) -> float | None:
    """Seconds, from a TIMESTAMP (ISO text) or a number of microseconds,
    as `encode()` and the rank log's export read times."""
    if t is None:
        return None
    if isinstance(t, (int, float)):
        return t / 1e6
    import datetime

    return datetime.datetime.fromisoformat(str(t).replace("Z", "+00:00")).timestamp()


def sequence(
    source: Any,
    name: str,
    *,
    table: str = "items",
    field: str = "emb",
    vectors: Mapping[Any, Any] | None = None,
    sequences: Mapping[Any, Any] | None = None,
    events: Mapping[str, str] | None = None,
    records: Any = None,
    models: str | os.PathLike | None = None,
    width: int = 64,
    layers: int = 2,
    heads: int = 1,
    ffn: int | None = None,
    max_len: int = 200,
    gap_buckets: int = 32,
    actions: list | None = None,
    learn_items: bool = False,
    negatives: str = "in_batch",
    temperature: float = 0.05,
    epochs: int = 50,
    batch: int = 128,
    lr: float = 1e-3,
    dropout: float = 0.2,
    holdout: bool = False,
    patience: int = 5,
    max_seconds: float | None = None,
    seed: int = 0,
    log: Any = None,
) -> dict:
    """Trains a sequence encoder with PyTorch and saves it as `name` under
    `models` (default $STRATA_MODELS or ~/.local/share/strata/models), for
    `encode('name', (SELECT item, action, ts FROM ... ORDER BY ts DESC))`
    to turn a history into a vector of VECTOR `field` of `table`.

    The item vectors are `field`'s, read from `source` (a `Database` or a
    `Client`), or `vectors` (key to vector). The histories are one of:

    * `sequences`: user to events (item key, action, time in seconds or
      None), oldest first. Each event's item is a target of the events
      before it, so every prefix is a history as of its time.
    * `events`: an events table of `source` read into `sequences`:
      ``{"table": "events", "user": "usr", "item": "item", "action":
      "action", "time": "ts"}`` (and ``"order"`` for a column that orders
      events with equal times). Its times are a TIMESTAMP or an INT of
      microseconds, as `encode()` reads them.
    * `records`: the rank log's export (`export_rank_log` with `history`),
      point-in-time histories with the row served; a row with a positive
      label (or none) is the target of its history.

    Every candidate is scored as `similarity` scores it, the dot product of
    the model's unit vector with the item's vector, over `temperature`, and
    the loss is cross-entropy against the other items of the batch
    (`negatives="in_batch"`) or every item (`"all"`, for catalogs that fit).
    With `learn_items` the item vectors are trained too (from the field's,
    or from `vectors`) and returned normalized: the model then ranks like
    SASRec, but a new item needs a vector before it can be found. Either
    way the model is saved with the item vectors it was trained with
    (`items.safetensors`), and `encode()` refuses a history whose items
    have other vectors in the field: write learned ones in with
    `write_vectors`. `holdout` keeps each sequence's last
    event out of training and stops when NDCG@10 on those stops rising for
    `patience` epochs, keeping the best epoch. `max_seconds` stops training
    after the first epoch that ends past that many seconds.

    Returns ``{"path": ..., "history": [per-epoch loss and metrics],
    "vectors": learned vectors or None}``."""
    from . import _sequence as S

    if negatives not in ("in_batch", "all"):
        raise ValueError("negatives is 'in_batch' or 'all'")
    if sum(x is not None for x in (sequences, events, records)) != 1:
        raise TypeError("give one of sequences, events or records")
    if vectors is None:
        vectors = _items(source, table, field)
    if not vectors:
        raise ValueError(f"no vectors in {table}.{field}")
    if events is not None:
        sequences = _events(source, events)
    if records is not None:
        records = list(records)
    seen = sorted({S._action(s[1]) for seq in (sequences or {}).values() for s in seq} | {S._action(a) for r in records or () for a in r.get("history_actions") or ()})
    acts = [""] + [a for a in (actions if actions is not None else seen) if a != ""]
    if width % heads:
        raise ValueError("width is a multiple of heads")
    cfg = {
        "kind": "sequence",
        "table": table,
        "field": field,
        "dim": None,
        "width": width,
        "layers": layers,
        "heads": heads,
        "ffn": ffn or 4 * width,
        "max_len": max_len,
        "gap_buckets": gap_buckets,
        "actions": acts,
        "eps": S.EPS,
    }
    model, items, learned, history = S.train(
        sequences or {}, records, vectors, cfg, learn_items=learn_items, negatives=negatives, temperature=temperature,
        epochs=epochs, batch=batch, lr=lr, dropout=dropout, holdout=holdout, patience=patience, seed=seed, log=log, max_seconds=max_seconds,
    )
    path = os.path.join(_models_dir(models), name)
    S.save(model, cfg, path, items=items)
    return {"path": path, "history": history, "vectors": learned}


def _models_dir(models: str | os.PathLike | None) -> str:
    return os.fspath(models) if models is not None else os.environ.get("STRATA_MODELS") or os.path.expanduser("~/.local/share/strata/models")


def write_vectors(source: Any, name: str, *, models: str | os.PathLike | None = None) -> int:
    """Writes the item vectors sequence model `name` was trained with (its
    `items.safetensors`, under `models`) into the field of the table its
    config names, in `source` (a `Database` or a `Client`): each item's row
    keeps its other columns, and only that field changes. An item without
    a row is skipped. Returns how many rows were written."""
    from . import _sequence as S

    path = os.path.join(_models_dir(models), name)
    cfg = json.load(open(os.path.join(path, S.CONFIG)))
    items = S.read_items(path)
    if not items:
        raise ValueError(f"sequence model {name} was saved without its item vectors ({S.ITEMS}): train it again with strata.train.sequence")
    table, field = cfg["table"], cfg["field"]
    pk = _key(source, table)
    written = 0
    for k, v in items.items():
        written += int(source.sql(f"UPDATE {table} SET {field} = :v WHERE {pk} = :k", {"k": k, "v": v}) or 0)
    return written
