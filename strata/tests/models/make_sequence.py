"""Writes the sequence model seq_small/ and sequence_reference.json: the
vectors PyTorch gives histories through strata-py's encoder, which
tests/sequence.rs checks Strata's Rust encoder against.

    pip install torch numpy        # CPU torch 2.x
    PYTHONPATH=../../../strata-py/python python3 make_sequence.py

Every weight is perturbed from PyTorch's initialization, so that each
tensor (layer norms, biases, embeddings) changes the answer.
"""
import json
import os
import random

import torch

from strata import _sequence as S

HERE = os.path.dirname(os.path.abspath(__file__))
torch.manual_seed(20261030)
rng = random.Random(20261030)

cfg = {
    "kind": "sequence", "table": "items", "field": "emb", "dim": 8, "width": 16, "layers": 2, "heads": 2,
    "ffn": 32, "max_len": 12, "gap_buckets": 8, "actions": ["", "view", "buy"], "eps": S.EPS,
}
model = S._module(torch, cfg["dim"], cfg)
with torch.no_grad():
    for p in model.parameters():
        p.add_(0.3 * torch.randn_like(p))
model.eval()
S.save(model, cfg, os.path.join(HERE, "seq_small"))

items = {f"i{i}": [round(rng.gauss(0, 1), 4) for _ in range(cfg["dim"])] for i in range(30)}
index = {k: i for i, k in enumerate(items)}
E = torch.tensor(list(items.values()))
actions = {a: i for i, a in enumerate(cfg["actions"])}
cases = []
for n in [1, 2, 5, 11, 12, 13, 20]:
    t = 1_700_000_000.0
    h = []
    for _ in range(n):
        t += rng.choice([0.0, 0.5, 1.0, 3.0, 90.0, 4000.0, 1e6, 1e9])
        h.append((rng.choice(list(items)), rng.choice(["view", "buy", "other", None]), None if rng.random() < 0.1 else t))
    for now in [None, t + 30.0]:
        x = S.history(h, now, index, actions, cfg)
        with torch.no_grad():
            out = model(E[torch.tensor([x[0]])], torch.tensor([x[1]]), torch.tensor([x[2]]))[0, -1]
        cases.append({"history": h, "now": now, "vector": out.tolist()})
json.dump({"items": items, "cases": cases}, open(os.path.join(HERE, "sequence_reference.json"), "w"), indent=1)
