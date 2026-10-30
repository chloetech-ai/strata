"""Training what a schema names: semantic-ID codebooks.

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

import os
from typing import Any

from . import _strata

__all__ = ["codebook"]


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
