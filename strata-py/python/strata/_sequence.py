"""The sequence encoder `encode(model, rows)` runs, and its training.

A model turns a history (item, action, time; newest last here) into a
vector in the space of an item table's VECTOR field. Each step is the
item's vector, projected to the model's width, plus an embedding of the
action, of the time gap after the event (to the next event, or for the
newest to the time of the request: a bucket of its log2 seconds), and of
the step's position counted from the oldest. Pre-norm causal transformer
layers follow; the newest step's output, normalized and projected to the
item dimension, then normalized, is the vector. A row's score is its dot
product with an item's vector as stored, which is what `similarity` in
Strata computes.

Strata reads the files this module writes (`<models>/<name>/config.json`,
`model.safetensors` and `items.safetensors`) and runs the same arithmetic
in Rust; the names and shapes of the tensors here are the format. The
items file holds the item vectors the model was trained with, the tensor
`vectors` (a row an item) with the items' keys as a JSON array in its
metadata's `keys`: Strata serves the model only over a field that holds
those vectors.
"""

from __future__ import annotations

import json
import math
import os
import random
import tempfile
import time
from typing import Any, Iterable, Mapping, Sequence

CONFIG = "config.json"
WEIGHTS = "model.safetensors"
ITEMS = "items.safetensors"
EPS = 1e-5


def gap_bucket(seconds: float | None, buckets: int) -> int:
    """0 for no gap (or none known, or under a second), else 1 + the whole
    part of its log2, at most `buckets - 1`."""
    if seconds is None or not seconds >= 1.0:
        return 0
    return min(buckets - 1, 1 + int(math.floor(math.log2(seconds))))


def _torch():
    try:
        import torch
    except ImportError as e:  # pragma: no cover
        raise ImportError("strata.train.sequence needs PyTorch: pip install torch") from e
    return torch


def _module(torch, dim: int, cfg: Mapping[str, Any]):
    nn, F = torch.nn, torch.nn.functional
    width, heads, ffn = cfg["width"], cfg["heads"], cfg["ffn"]

    class Layer(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.ln1 = nn.LayerNorm(width, eps=EPS)
            self.qkv = nn.Linear(width, 3 * width)
            self.proj = nn.Linear(width, width)
            self.ln2 = nn.LayerNorm(width, eps=EPS)
            self.up = nn.Linear(width, ffn)
            self.down = nn.Linear(ffn, width)

        def forward(self, x, drop: float):
            b, n, _ = x.shape
            q, k, v = self.qkv(self.ln1(x)).split(width, dim=-1)
            q, k, v = (t.view(b, n, heads, width // heads).transpose(1, 2) for t in (q, k, v))
            a = F.scaled_dot_product_attention(q, k, v, is_causal=True, dropout_p=drop if self.training else 0.0)
            x = x + F.dropout(self.proj(a.transpose(1, 2).reshape(b, n, width)), drop, self.training)
            return x + F.dropout(self.down(F.relu(self.up(self.ln2(x)))), drop, self.training)

    class Encoder(nn.Module):
        def __init__(self) -> None:
            super().__init__()
            self.inp = nn.Linear(dim, width)
            self.action = nn.Embedding(len(cfg["actions"]), width)
            self.gap = nn.Embedding(cfg["gap_buckets"], width)
            self.position = nn.Embedding(cfg["max_len"], width)
            self.layers = nn.ModuleList(Layer() for _ in range(cfg["layers"]))
            self.ln = nn.LayerNorm(width, eps=EPS)
            self.out = nn.Linear(width, dim)
            self.drop = 0.0

        def forward(self, items, actions, gaps):
            """Items [b, n, dim] (oldest first, padding after the end),
            actions and gap buckets [b, n]: a unit vector per step."""
            n = items.shape[1]
            x = self.inp(items) + self.action(actions) + self.gap(gaps) + self.position(torch.arange(n, device=items.device))
            x = F.dropout(x, self.drop, self.training)
            for layer in self.layers:
                x = layer(x, self.drop)
            return F.normalize(self.out(self.ln(x)), dim=-1)

    return Encoder()


def _write_safetensors(path: str, tensors: Mapping[str, Any], metadata: Mapping[str, str] | None = None) -> None:
    """The safetensors format: an 8-byte little-endian header length, the
    JSON header (names, dtype, shape, byte offsets; text `__metadata__`),
    then the data."""
    header, blobs, at = {}, [], 0
    if metadata:
        header["__metadata__"] = dict(metadata)
    for name, t in tensors.items():
        b = t.detach().to("cpu").float().contiguous().numpy().astype("<f4").tobytes()
        header[name] = {"dtype": "F32", "shape": list(t.shape), "data_offsets": [at, at + len(b)]}
        blobs.append(b)
        at += len(b)
    h = json.dumps(header, separators=(",", ":")).encode()
    h += b" " * (-len(h) % 8)
    with open(path, "wb") as f:
        f.write(len(h).to_bytes(8, "little"))
        f.write(h)
        for b in blobs:
            f.write(b)


def _item_keys(keys: Sequence[Any]) -> str:
    """The keys as the items file lists them: text or whole numbers."""
    for k in keys:
        if isinstance(k, bool) or not isinstance(k, (str, int)):
            raise ValueError(f"an item's key is text or a whole number, as a table's primary key, not {k!r}")
    return json.dumps(list(keys), separators=(",", ":"))


def save(model, cfg: Mapping[str, Any], directory: str, items: tuple[Sequence[Any], Any] | None = None) -> None:
    """Writes `config.json`, `model.safetensors` and, given `items` (the
    keys, and a tensor of their vectors in that order), `items.safetensors`
    into `directory`, each through a temporary file so that a reader never
    sees half of one. Without `items` an items file left there is removed:
    it would be another model's."""
    os.makedirs(directory, exist_ok=True)
    files = []
    if items is not None:
        keys, matrix = items
        meta = {"keys": _item_keys(keys)}
        if tuple(matrix.shape) != (len(keys), cfg["dim"]):
            raise ValueError(f"the item vectors are {tuple(matrix.shape)}, not {len(keys)} keys by {cfg['dim']}")
        files.append((ITEMS, lambda p: _write_safetensors(p, {"vectors": matrix}, meta)))
    elif os.path.exists(os.path.join(directory, ITEMS)):
        os.unlink(os.path.join(directory, ITEMS))
    files.append((WEIGHTS, lambda p: _write_safetensors(p, model.state_dict())))
    files.append((CONFIG, lambda p: open(p, "w").write(json.dumps(cfg, indent=2) + "\n")))
    for file, write in files:
        fd, tmp = tempfile.mkstemp(dir=directory, prefix="." + file)
        os.close(fd)
        try:
            write(tmp)
            mask = os.umask(0)
            os.umask(mask)
            os.chmod(tmp, 0o666 & ~mask)
            os.replace(tmp, os.path.join(directory, file))
        except BaseException:
            os.unlink(tmp)
            raise


def _read_safetensors(path: str):
    """The header and the data of a safetensors file."""
    raw = open(path, "rb").read()
    n = int.from_bytes(raw[:8], "little")
    return json.loads(raw[8 : 8 + n]), raw[8 + n :]


def read_items(directory: str) -> dict:
    """The item vectors a model in `directory` was trained with (key to a
    list of floats), or {} for a model saved without them."""
    path = os.path.join(directory, ITEMS)
    if not os.path.exists(path):
        return {}
    import numpy as np

    header, data = _read_safetensors(path)
    keys = json.loads(header["__metadata__"]["keys"])
    t = header["vectors"]
    a, b = t["data_offsets"]
    rows = np.frombuffer(data[a:b], dtype="<f4").reshape(t["shape"])
    return {k: rows[i].tolist() for i, k in enumerate(keys)}


def load(directory: str, item_dim: int | None = None):
    """The model saved in `directory` and its config, for evaluation."""
    torch = _torch()
    cfg = json.load(open(os.path.join(directory, CONFIG)))
    m = _module(torch, cfg["dim"], cfg)
    header, data = _read_safetensors(os.path.join(directory, WEIGHTS))
    import numpy as np

    state = {}
    for name, t in header.items():
        if name == "__metadata__":
            continue
        a, b = t["data_offsets"]
        state[name] = torch.from_numpy(np.frombuffer(data[a:b], dtype="<f4").reshape(t["shape"]).copy())
    m.load_state_dict(state)
    m.eval()
    return m, cfg


class Steps:
    """Histories as the model reads them: per example, item indices into
    the item matrix, action indices, gap buckets and per step its target
    item (-1 for none)."""

    def __init__(self) -> None:
        self.items: list[list[int]] = []
        self.actions: list[list[int]] = []
        self.gaps: list[list[int]] = []
        self.targets: list[list[int]] = []

    def __len__(self) -> int:
        return len(self.items)

    def add(self, items, actions, gaps, targets) -> None:
        self.items.append(list(items))
        self.actions.append(list(actions))
        self.gaps.append(list(gaps))
        self.targets.append(list(targets))


def history(steps: Sequence[tuple], now: float | None, index: Mapping[Any, int], actions: Mapping[str, int], cfg: Mapping[str, Any]):
    """The model's inputs for one history (item, action, time in seconds;
    oldest first) at time `now`: its newest `max_len` steps whose item has
    a vector."""
    steps = [s for s in steps if s[0] in index][-cfg["max_len"] :]
    times = [s[2] for s in steps[1:]] + [now]
    gaps = [None if t is None or s[2] is None else t - s[2] for s, t in zip(steps, times)]
    return (
        [index[s[0]] for s in steps],
        [actions.get(_action(s[1]), 0) for s in steps],
        [gap_bucket(g, cfg["gap_buckets"]) for g in gaps],
    )


def _action(a: Any) -> str:
    return "" if a is None else str(a)


def windows(seq: Sequence[tuple], index: Mapping[Any, int], actions: Mapping[str, int], cfg: Mapping[str, Any], out: Steps) -> None:
    """Training examples of one user's events (oldest first): windows of
    `max_len` steps from the newest back, each step's target the next
    event's item. A step's gap is the time to that next event, which is
    the request's time when the model predicts it."""
    seq = [s for s in seq if s[0] in index]
    m = cfg["max_len"]
    end = len(seq)
    while end >= 2:
        start = max(0, end - 1 - m)
        w = seq[start:end]
        items, acts, gaps = history(w[:-1], w[-1][2], index, actions, cfg)
        out.add(items, acts, gaps, [index[s[0]] for s in w[1:]])
        end = start + 1 if start > 0 else 0


def _batches(torch, steps: Steps, order: list[int], size: int, vectors):
    for at in range(0, len(order), size):
        ids = order[at : at + size]
        n = max(len(steps.items[i]) for i in ids)
        pad = lambda xs, v: [x + [v] * (n - len(x)) for x in xs]  # noqa: E731
        items = torch.tensor(pad([steps.items[i] for i in ids], 0))
        yield (
            items,
            torch.tensor(pad([steps.actions[i] for i in ids], 0)),
            torch.tensor(pad([steps.gaps[i] for i in ids], 0)),
            torch.tensor(pad([steps.targets[i] for i in ids], -1)),
        )


def item_matrix(torch, base, learned):
    """The item vectors as Strata will store them: the given ones, or the
    learned ones normalized."""
    return torch.nn.functional.normalize(learned, dim=-1) if learned is not None else base


def train(
    sequences: Mapping[Any, Sequence[tuple]],
    records: Iterable[Mapping[str, Any]] | None,
    vectors: Mapping[Any, Sequence[float]],
    cfg: dict,
    *,
    learn_items: bool,
    negatives: str,
    temperature: float,
    epochs: int,
    batch: int,
    lr: float,
    dropout: float,
    holdout: bool,
    patience: int,
    seed: int,
    log,
    max_seconds: float | None = None,
):
    torch = _torch()
    torch.manual_seed(seed)
    rng = random.Random(seed)
    keys = list(vectors)
    index = {k: i for i, k in enumerate(keys)}
    base = torch.tensor([list(map(float, vectors[k])) for k in keys], dtype=torch.float32)
    dim = base.shape[1]
    cfg["dim"] = dim
    actions = {a: i for i, a in enumerate(cfg["actions"])}
    learned = torch.nn.Parameter(base.clone()) if learn_items else None

    train_steps, valid = Steps(), []
    for user, seq in sequences.items():
        seq = [s for s in seq if s[0] in index]
        if holdout and len(seq) >= 3:
            valid.append((seq[:-1], seq[-1]))
            seq = seq[:-1]
        windows(seq, index, actions, cfg, train_steps)
    for r in records or ():
        label = r.get("label")
        if r.get("row_key") not in index or not (label is None or label > 0):
            continue
        acts = r.get("history_actions") or [None] * len(r.get("history_items", []))
        times = [None if t is None else t / 1e6 for t in r.get("history_times", [])]
        h = list(zip(r.get("history_items", []), acts, times))
        items, a, g = history(h, None if r.get("ts") is None else _seconds(r["ts"]), index, actions, cfg)
        if items:
            train_steps.add(items, a, g, [-1] * (len(items) - 1) + [index[r["row_key"]]])
    if not len(train_steps):
        raise ValueError("no training examples: each needs at least two events whose items have vectors")

    model = _module(torch, dim, cfg)
    model.drop = dropout
    params = list(model.parameters()) + ([learned] if learned is not None else [])
    opt = torch.optim.Adam(params, lr=lr)
    best, best_state, stale, history_ = -1.0, None, 0, []
    start = time.monotonic()
    for epoch in range(epochs):
        model.train()
        order = list(range(len(train_steps)))
        rng.shuffle(order)
        total, count = 0.0, 0
        for items, acts, gaps, targets in _batches(torch, train_steps, order, batch, None):
            E = item_matrix(torch, base, learned)
            out = model(E[items], acts, gaps)
            mask = targets >= 0
            h, t = out[mask], targets[mask]
            if negatives == "all":
                logits = h @ E.T / temperature
            else:
                # The batch's target items, each once, are the candidates.
                cand, t = torch.unique(t, return_inverse=True)
                logits = h @ E[cand].T / temperature
            loss = torch.nn.functional.cross_entropy(logits, t)
            opt.zero_grad()
            loss.backward()
            opt.step()
            total += loss.item() * len(t)
            count += len(t)
        row = {"epoch": epoch + 1, "loss": total / max(count, 1)}
        if valid:
            row.update(evaluate(model, valid, index, actions, cfg, item_matrix(torch, base, learned)))
        history_.append(row)
        if log:
            log(row)
        if valid:
            if row["ndcg@10"] > best:
                best, stale = row["ndcg@10"], 0
                best_state = ({k: v.clone() for k, v in model.state_dict().items()}, None if learned is None else learned.detach().clone())
            else:
                stale += 1
                if stale >= patience:
                    break
        if max_seconds is not None and time.monotonic() - start > max_seconds:
            break
    if best_state is not None:
        model.load_state_dict(best_state[0])
        if learned is not None:
            learned = best_state[1]
    model.eval()
    E = item_matrix(torch, base, learned).detach()
    out_vectors = {k: E[i].tolist() for i, k in enumerate(keys)} if learn_items else None
    return model, (keys, E), out_vectors, history_


def _seconds(t: Any) -> float:
    return t / 1e6 if isinstance(t, (int, float)) else float(t)


def encode_batch(model, histories, index, actions, cfg, E, nows):
    """The vector of each history ((item, action, seconds) oldest first) at
    its time in `nows`."""
    torch = _torch()
    steps = Steps()
    for h, now in zip(histories, nows):
        items, a, g = history(h, now, index, actions, cfg)
        steps.add(items, a, g, [0] * len(items))
    outs = []
    with torch.no_grad():
        for i in range(len(steps)):
            items = torch.tensor([steps.items[i]])
            o = model(E[items], torch.tensor([steps.actions[i]]), torch.tensor([steps.gaps[i]]))
            outs.append(o[0, -1])
    return torch.stack(outs)


def evaluate(model, cases, index, actions, cfg, E, k: int = 10, chunk: int = 256):
    """Recall@k and NDCG@k of the next item, ranked against every item but
    those of the history (full ranking, leave-one-out)."""
    torch = _torch()
    model.eval()
    hits, ndcg = 0.0, 0.0
    with torch.no_grad():
        for at in range(0, len(cases), chunk):
            part = cases[at : at + chunk]
            steps = Steps()
            for h, target in part:
                items, a, g = history(h, target[2], index, actions, cfg)
                steps.add(items, a, g, [0] * len(items))
            last = torch.tensor([len(x) - 1 for x in steps.items])
            for items, acts, gaps, _ in _batches(torch, steps, list(range(len(steps))), len(steps), None):
                out = model(E[items], acts, gaps)[torch.arange(len(part)), last]
            scores = out @ E.T
            for row, (h, target) in enumerate(part):
                seen = [index[s[0]] for s in h if s[0] in index]
                scores[row, seen] = -math.inf
                t = index[target[0]]
                rank = int((scores[row] > scores[row, t]).sum())
                if scores[row, t] > -math.inf and rank < k:
                    hits += 1
                    ndcg += 1 / math.log2(rank + 2)
    n = max(len(cases), 1)
    return {f"recall@{k}": hits / n, f"ndcg@{k}": ndcg / n}
