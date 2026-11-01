"""Generative retrieval with semantic IDs (PR S3): a TIGER-style
encoder-decoder that writes the next item's semantic ID a number at a
time, each step constrained by Strata's next codes (`sid_next`), against
the sequence encoder (`encode()`, PR A8), item-to-item walks (`ppr` over an
edge table) and their fusion (`rrf` with `ranked(:keys)`), on Amazon
Beauty, Sports and Outdoors, and Toys and Games (5-core).

Leave-one-out as TIGER and SASRec are measured: each user's last item is
the test, the one before it validates, the rest train. Every method learns
from the transitions before each user's last two items and reads the whole
history before the test item, and none recommends an item the user already
has: the decoder's next codes are those of the items `NOT id IN (history)`
leaves, and the other queries have the same condition.

  prep   items' text (title, brand, categories, price) embedded by
         bge-small-en-v1.5 in a Strata embedded field; a codebook of 3
         levels of 256 trained on those vectors (`strata.train.codebook`),
         and a table whose semantic-ID field gives every item a code of 4
         numbers (3 levels and a tie); the events; the edge table of
         walks (1/g from the item g steps back, g up to 5).
  tiger  the encoder-decoder (user token, then the codes of the last 20
         items; 4 layers each, width 128), trained with PyTorch, stopped
         when Recall@10 of a constrained beam search on validation users
         stops rising, or at --max-minutes.
  a8     the sequence encoder, as strata/results/sequence/eval.py trains
         it (items only, learned item vectors, every item a negative).
  eval   test users: the decoder's top 10 of a beam of 20, each step's
         codes from `sid_next`; A8's query; the walk's query; and rrf of
         them. Recall@5/@10, NDCG@5/@10, and the time each takes, one
         request at a time, over the first 300 users. The queries over
         every user run on every core, and each part's answers are kept
         in <work>/eval-cache, so a run stopped part way resumes.

    python tiger.py --data beauty --dir ~/data/seq/beauty --work ~/work/beauty --models ~/data/seq/models --stage prep
    python tiger.py ... --stage tiger --max-minutes 90
    python tiger.py ... --stage a8 --max-minutes 60
    python tiger.py ... --stage eval --out .

`--models` holds bge-small-en-v1.5 (strata-server fetch-model
bge-small-en-v1.5); strata-py is built with `--features bert`.
"""
import argparse
import ast
import gzip
import json
import math
import os
import platform
import random
import re
import subprocess
import statistics
import time
from concurrent.futures import ThreadPoolExecutor

import strata
import strata.train
import torch
from torch import nn

FILES = {
    "beauty": ("reviews_Beauty_5.json.gz", "meta_Beauty.json.gz"),
    "sports": ("reviews_Sports_and_Outdoors_5.json.gz", "meta_Sports_and_Outdoors.json.gz"),
    "toys": ("reviews_Toys_and_Games_5.json.gz", "meta_Toys_and_Games.json.gz"),
}
EMBEDDER = "bge-small-en-v1.5"
LEVELS, K = 3, 256
HISTORY = 20  # items the decoder reads, as TIGER
USERS = 2000  # user tokens, hashed, as TIGER
WALK = 5  # steps back an edge of the walk reaches


def sequences(path):
    """Each user's items in time order (one day's in the file's order)."""
    by = {}
    with gzip.open(path, "rt") as f:
        for i, line in enumerate(f):
            r = json.loads(line)
            by.setdefault(r["reviewerID"], []).append((int(r["unixReviewTime"]), i, r["asin"]))
    return {u: [a for _, _, a in sorted(v)] for u, v in sorted(by.items())}


def texts(path, want):
    """The text TIGER embeds for each item: title, brand, categories, price."""
    out = {}
    pat = re.compile(r"'asin': '([^']+)'")
    with gzip.open(path, "rt") as f:
        for line in f:
            m = pat.search(line)
            if not m or m.group(1) not in want:
                continue
            d = ast.literal_eval(line)
            parts = []
            if d.get("title"):
                parts.append(f"Title: {d['title']}")
            if d.get("brand"):
                parts.append(f"Brand: {d['brand']}")
            cats = [" > ".join(c) for c in d.get("categories") or [] if c]
            if cats:
                parts.append("Categories: " + "; ".join(cats))
            if d.get("price") is not None:
                parts.append(f"Price: {d['price']}")
            out[d["asin"]] = ". ".join(parts)
    return out


def machine():
    if platform.system() == "Darwin":
        q = lambda k: subprocess.run(["sysctl", "-n", k], capture_output=True, text=True).stdout.strip()  # noqa: E731
        cpu, mem = f"{q('machdep.cpu.brand_string')} ({q('hw.model')})", int(q("hw.memsize") or 0) / 1e9
    else:
        cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), platform.processor())
        mem = next((int(l.split()[1]) / 1e6 for l in open("/proc/meminfo") if l.startswith("MemTotal")), None)
    return {"cpu": cpu, "cores": os.cpu_count(), "memory_gb": round(mem, 1) if mem else None, "os": f"{platform.system()} {platform.release()}", "torch": torch.__version__}


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))]


def quoted(keys):
    return ", ".join("'" + k.replace("'", "''") + "'" for k in keys)


def metrics(ranked, target):
    """Recall and NDCG at 5 and 10 of one ranked list."""
    r = {"recall@5": 0.0, "recall@10": 0.0, "ndcg@5": 0.0, "ndcg@10": 0.0}
    if target in ranked[:10]:
        i = ranked.index(target)
        r["recall@10"], r["ndcg@10"] = 1.0, 1 / math.log2(i + 2)
        if i < 5:
            r["recall@5"], r["ndcg@5"] = 1.0, 1 / math.log2(i + 2)
    return r


def mean_metrics(rows):
    return {k: sum(r[k] for r in rows) / max(len(rows), 1) for k in ("recall@5", "recall@10", "ndcg@5", "ndcg@10")}


# ---------------------------------------------------------------- prep


def prep(a, seqs, work, models):
    items = sorted({i for s in seqs.values() for i in s})
    t0 = time.time()
    text = texts(os.path.join(a.dir, FILES[a.data][1]), set(items))
    missing = [i for i in items if not text.get(i)]
    for i in missing:
        text[i] = f"Item {i}"
    print(f"{len(items)} items, {len(missing)} without metadata ({time.time() - t0:.0f} s)", flush=True)

    # The items' vectors, by the embedded field's model.
    t0 = time.time()
    emb_path = os.path.join(work, "embed")
    edb = strata.Database.create(emb_path, {"table": "items", "key": "id", "fields": [
        {"name": "id", "type": "keyword"}, {"name": "text", "type": "text"},
        {"name": "emb", "type": "vector", "dim": 384, "index": "flat", "embed": {"from": ["text"], "model": EMBEDDER}},
    ]}, models=models)
    for j in range(0, len(items), 1000):
        edb.insert([{"id": i, "text": text[i]} for i in items[j:j + 1000]])
    vec = dict(edb.query(f"SELECT id, emb FROM items LIMIT {len(items)}").rows)
    edb.close()
    embed_s = time.time() - t0
    print(f"embedded {len(vec)} items in {embed_s:.0f} s", flush=True)

    # The codebook, then the table whose semantic-ID field it fills.
    t0 = time.time()
    book = f"{a.data}-rq"
    sha = strata.train.codebook([vec[i] for i in items], book, models=models, levels=LEVELS, k=K, seed=a.seed)
    codebook_s = time.time() - t0
    db = strata.Database.create(os.path.join(work, "db"), {"table": "items", "key": "id", "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "emb", "type": "vector", "dim": 384, "index": "flat"},
        {"name": "sid", "type": "keyword", "multi": True, "semantic_id": {"from": "emb", "codebook": book}},
        {"name": "iv", "type": "vector", "dim": a.dim, "index": "flat"},
    ]}, models=models)
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, seq INT NOT NULL, item KEYWORD NOT NULL) OPTIONS(cluster_by = 'usr')")
    db.sql("CREATE TABLE cooc (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL REFERENCES items, dst KEYWORD NOT NULL REFERENCES items, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')")
    for j in range(0, len(items), 1000):
        db.insert([{"id": i, "emb": vec[i]} for i in items[j:j + 1000]], table="items")
    rows, eid = [], 0
    for u, s in seqs.items():
        for n, i in enumerate(s):
            rows.append({"id": eid, "usr": u, "seq": n, "item": i})
            eid += 1
            if len(rows) == 50_000:
                db.insert(rows, table="events")
                rows = []
    db.insert(rows, table="events")
    # The walk's edges, from the transitions before each user's last two.
    w = {}
    for s in seqs.values():
        train = s[:-2]
        for t in range(1, len(train)):
            for g in range(1, WALK + 1):
                if t - g >= 0:
                    e = (train[t - g], train[t])
                    w[e] = w.get(e, 0.0) + 1.0 / g
    edges = [{"id": f"{x}>{y}", "src": x, "dst": y, "w": v} for (x, y), v in w.items()]
    for j in range(0, len(edges), 50_000):
        db.insert(edges[j:j + 50_000], table="cooc")
    db.optimize()

    codes = {k: max(v, key=len) for k, v in db.query(f"SELECT id, sid FROM items LIMIT {len(items)}").rows}
    db.close()
    levels = [len({c.split(".")[l] for c in codes.values()}) for l in range(LEVELS + 1)]
    ties = max(int(c.split(".")[LEVELS]) for c in codes.values())
    full3 = len({c.rsplit(".", 1)[0] for c in codes.values()})
    info = {
        "items": len(items), "without_metadata": len(missing), "embed_seconds": round(embed_s, 1), "codebook_seconds": round(codebook_s, 1),
        "codebook_sha256": sha, "codes_used_per_level": levels, "distinct_3_level_codes": full3, "max_tie": ties, "edges": len(edges),
    }
    print(json.dumps(info), flush=True)
    json.dump({"codes": codes, "info": info}, open(os.path.join(work, "codes.json"), "w"))


# ---------------------------------------------------------------- the decoder


def parse(code):
    return [int(x) for x in code.split(".")]


class Tiger(nn.Module):
    """An encoder over the user token and the history's codes, and a
    decoder that writes a code a number at a time (one output head a
    level)."""

    def __init__(self, width=128, heads=4, layers=4, ffn=1024, dropout=0.1, tokens="item", levels=LEVELS + 1, k=K):
        super().__init__()
        self.levels, self.k, self.tokens = levels, k, tokens
        # An input position is a code number ("code", as TIGER), or an
        # item, the sum of its numbers' embeddings ("item": a quarter of
        # the positions).
        self.slot = 1 if tokens == "code" else levels
        self.vocab = 2 + USERS + levels * k  # 0 pad, 1 start, users, codes
        self.tok = nn.Embedding(self.vocab, width, padding_idx=0)
        self.pos_in = nn.Embedding(1 + HISTORY * (levels if tokens == "code" else 1), width)
        self.pos_out = nn.Embedding(levels, width)
        self.tf = nn.Transformer(width, heads, layers, layers, ffn, dropout, batch_first=True, norm_first=True)
        self.heads = nn.ModuleList([nn.Linear(width, k) for _ in range(levels)])

    def code_token(self, level, c):
        return 2 + USERS + level * self.k + c

    def encode(self, src):
        """`src` is batch x positions x slot: each position's tokens."""
        pad = src[:, :, 0] == 0
        x = self.tok(src).sum(dim=2) + self.pos_in(torch.arange(src.shape[1]))
        # Causal, so that one pass over a sequence serves each of its
        # targets, which reads the positions before it.
        causal = self.tf.generate_square_subsequent_mask(src.shape[1])
        return self.tf.encoder(x, mask=causal, src_key_padding_mask=pad, is_causal=True), pad

    def decode(self, tgt, memory, pad):
        x = self.tok(tgt) + self.pos_out(torch.arange(tgt.shape[1]))
        mask = self.tf.generate_square_subsequent_mask(tgt.shape[1])
        return self.tf.decoder(x, memory, tgt_mask=mask, tgt_is_causal=True, memory_key_padding_mask=pad)


def source(model, user, history, codes):
    """The encoder's positions: the user's token, then each history item's
    code, a number a position or an item a position."""
    pos = [[2 + (hash_user(user) % USERS)] + [0] * (model.slot - 1)]
    for i in history[-HISTORY:]:
        toks = [model.code_token(l, c) for l, c in enumerate(codes[i])]
        pos += [[t] for t in toks] if model.slot == 1 else [toks]
    return pos


def hash_user(u):
    h = 0xcbf29ce484222325
    for b in u.encode():
        h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def padded(rows):
    """Rows of positions, each a list of tokens, padded to the longest."""
    n, s = max(len(r) for r in rows), len(rows[0][0])
    return torch.tensor([r + [[0] * s] * (n - len(r)) for r in rows])


def beam_search(model, srcs, allowed, beam=20):
    """For each source, up to `beam` codes best first, each a list of
    numbers. `allowed(j, prefixes)` gives, for user j, each prefix's next
    numbers: a dict from prefix (a tuple) to a list."""
    model.eval()
    with torch.no_grad():
        memory, pad = model.encode(padded(srcs))
        beams = [[((), 0.0)] for _ in srcs]
        for level in range(model.levels):
            rows, owner = [], []
            for j, bs in enumerate(beams):
                for b, (p, s) in enumerate(bs):
                    rows.append([1] + [model.code_token(l, c) for l, c in enumerate(p)])
                    owner.append(j)
            if not rows:
                break
            idx = torch.tensor(owner)
            out = model.decode(torch.tensor(rows), memory[idx], pad[idx])[:, -1]
            logp = torch.log_softmax(model.heads[level](out), dim=-1)
            r = 0
            new = []
            for j, bs in enumerate(beams):
                nexts = allowed(j, [p for p, _ in bs]) if bs else {}
                cand = []
                for b, (p, s) in enumerate(bs):
                    for c in nexts.get(p, ()):
                        cand.append((s + float(logp[r + b, c]), p + (c,)))
                r += len(bs)
                cand.sort(key=lambda x: -x[0])
                new.append([(p, s) for s, p in cand[:beam]])
            beams = new
    return [[list(p) for p, _ in bs] for bs in beams]


class Trie:
    """The next numbers under each prefix, of every item but a user's own."""

    def __init__(self, codes):
        self.under = {}
        for c in codes.values():
            for l in range(len(c)):
                self.under.setdefault(tuple(c[:l]), {}).setdefault(c[l], 0)
                self.under[tuple(c[:l])][c[l]] += 1
        self.codes = codes

    def allowed(self, own, prefixes):
        less = {}
        for i in own:
            c = self.codes[i]
            for l in range(len(c)):
                less[(tuple(c[:l]), c[l])] = less.get((tuple(c[:l]), c[l]), 0) + 1
        return {p: [c for c, n in sorted(self.under.get(p, {}).items()) if n - less.get((p, c), 0) > 0] for p in prefixes}


def tiger(a, seqs, work):
    codes = {k: parse(v) for k, v in json.load(open(os.path.join(work, "codes.json")))["codes"].items()}
    inv = {tuple(c): i for i, c in codes.items()}
    if max(c[-1] for c in codes.values()) >= K:
        raise SystemExit(f"a tie of {K} or more: the tie level needs a wider head")
    torch.manual_seed(a.seed)
    rng = random.Random(a.seed)
    model = Tiger(width=a.tiger_width, heads=a.tiger_heads, layers=a.tiger_layers, ffn=a.tiger_ffn, dropout=0.1, tokens=a.tiger_tokens)
    # Windows of up to HISTORY + 1 items of each user's training items;
    # each item after a window's first is a target of the items before it.
    per = model.levels if model.slot == 1 else 1
    windows, targets = [], 0
    for u, s in seqs.items():
        train = s[:-2]
        for b in range(0, max(len(train) - 1, 0), HISTORY):
            w = train[b:b + HISTORY + 1]
            if len(w) >= 2:
                windows.append((source(model, u, w[:-1], codes), [codes[i] for i in w[1:]]))
                targets += len(w) - 1
    users = list(seqs)
    val_users = rng.sample(users, min(a.val_users, len(users)))
    trie = Trie(codes)
    print(f"{len(windows)} windows, {targets} training targets, {sum(p.numel() for p in model.parameters())} parameters", flush=True)
    opt = torch.optim.AdamW(model.parameters(), lr=a.tiger_lr, weight_decay=0.01)
    warm = 1000
    sched = torch.optim.lr_scheduler.LambdaLR(opt, lambda step: min((step + 1) / warm, math.sqrt(warm / (step + 1))))
    lossf = nn.CrossEntropyLoss()
    best, best_state, stale, history = -1.0, None, 0, []
    start = time.monotonic()
    for epoch in range(1, a.tiger_epochs + 1):
        model.train()
        rng.shuffle(windows)
        total, n = 0.0, 0
        for j in range(0, len(windows), a.batch):
            chunk = windows[j:j + a.batch]
            src = padded([w[0] for w in chunk])
            memory, pad = model.encode(src)
            # Target t of a window reads its user token and items 0..t-1.
            owner, upto, tgt = [], [], []
            for k, (_, ts) in enumerate(chunk):
                for t, c in enumerate(ts):
                    owner.append(k)
                    upto.append(1 + (t + 1) * per)
                    tgt.append(c)
            idx = torch.tensor(owner)
            mpad = pad[idx] | (torch.arange(src.shape[1])[None, :] >= torch.tensor(upto)[:, None])
            tgt_codes = torch.tensor(tgt)
            tgt_in = torch.cat([torch.ones(len(tgt), 1, dtype=torch.long), torch.tensor([[model.code_token(l, c) for l, c in enumerate(x[:-1])] for x in tgt])], dim=1)
            out = model.decode(tgt_in, memory[idx], mpad)
            loss = sum(lossf(model.heads[l](out[:, l]), tgt_codes[:, l]) for l in range(model.levels))
            opt.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
            opt.step()
            sched.step()
            total += float(loss.detach()) * len(tgt)
            n += len(tgt)
        # Validation: the item before the last, from the items before it.
        rows = []
        for j in range(0, len(val_users), 256):
            us = val_users[j:j + 256]
            srcs = [source(model, u, seqs[u][:-2], codes) for u in us]
            got = beam_search(model, srcs, lambda k, ps, us=us: trie.allowed(seqs[us[k]][:-2], ps), beam=a.beam)
            for u, g in zip(us, got):
                rows.append(metrics([inv[tuple(c)] for c in g], seqs[u][-2]))
        row = {"epoch": epoch, "loss": total / n, **mean_metrics(rows), "seconds": round(time.monotonic() - start, 1)}
        history.append(row)
        print(json.dumps(row), flush=True)
        if row["recall@10"] > best:
            best, stale = row["recall@10"], 0
            best_state = {k: v.clone() for k, v in model.state_dict().items()}
        else:
            stale += 1
            if stale >= a.patience:
                break
        if a.max_minutes is not None and time.monotonic() - start > a.max_minutes * 60:
            break
    torch.save({"state": best_state, "config": {"width": a.tiger_width, "heads": a.tiger_heads, "layers": a.tiger_layers, "ffn": a.tiger_ffn, "tokens": a.tiger_tokens}, "history": history}, os.path.join(work, "tiger.pt"))


# ---------------------------------------------------------------- A8


def a8(a, seqs, work, models):
    items = sorted({i for s in seqs.values() for i in s})
    g = torch.Generator().manual_seed(a.seed)
    start = torch.nn.functional.normalize(torch.randn(len(items), a.dim, generator=g), dim=-1)
    vectors = {k: start[i].tolist() for i, k in enumerate(items)}
    t0 = time.time()
    r = strata.train.sequence(
        None, f"{a.data}-seq", table="items", field="iv", vectors=vectors, sequences={u: [(i, None, None) for i in s[:-1]] for u, s in seqs.items()},
        models=models, width=64, layers=2, heads=1, max_len=50, learn_items=True, negatives="all", temperature=0.05, epochs=200, batch=128,
        lr=1e-3, dropout=0.2, holdout=True, patience=10, max_seconds=None if a.max_minutes is None else a.max_minutes * 60, seed=a.seed,
        log=lambda row: print(json.dumps(row), flush=True),
    )
    train_s = time.time() - t0
    db = strata.Database.open(os.path.join(work, "db"), models=models)
    emb = dict(db.query(f"SELECT id, emb FROM items LIMIT {len(items)}").rows)
    learned = r["vectors"]
    for j in range(0, len(items), 1000):
        db.upsert([{"id": i, "emb": emb[i], "iv": learned[i]} for i in items[j:j + 1000]], table="items")
    db.close()
    best = max(r["history"], key=lambda h: h["ndcg@10"])
    json.dump({"seconds": round(train_s, 1), "epochs": len(r["history"]), "best_epoch": best["epoch"], "valid": {k: best[k] for k in ("recall@10", "ndcg@10")}, "history": r["history"]}, open(os.path.join(work, "a8.json"), "w"))


# ---------------------------------------------------------------- eval


def evaluate(a, seqs, work, models):
    c = json.load(open(os.path.join(work, "codes.json")))
    codes = {k: parse(v) for k, v in c["codes"].items()}
    by_code = {tuple(v): k for k, v in codes.items()}
    # --no-decoder: the queries alone, without the trained decoder (as on the
    # laptop, where the decoder is not run).
    saved = None if a.no_decoder else torch.load(os.path.join(work, "tiger.pt"))
    if saved is not None:
        model = Tiger(**saved["config"], dropout=0.1)
        model.load_state_dict(saved["state"])
        model.eval()
    torch.set_num_threads(a.threads)
    db = strata.Database.open(os.path.join(work, "db"), models=models)
    trie = Trie(codes)
    rng = random.Random(a.seed)
    users = list(seqs) if a.users is None else rng.sample(list(seqs), a.users)
    timed_users = users[:300]

    # Each part's answers are kept in the work directory, so that a run
    # stopped part way (by a time limit) resumes where it stopped.
    trained = [os.path.getmtime(os.path.join(work, f)) for f in (("a8.json",) if a.no_decoder else ("tiger.pt", "a8.json"))]
    key = {"users": a.users, "seed": a.seed, "beam": a.beam, "trained": trained}

    def cached(name, key, compute):
        path = os.path.join(work, "eval-cache", f"{name}.json")
        if os.path.exists(path):
            kept = json.load(open(path))
            if kept["key"] == key:
                print(f"{name}: kept from an earlier run", flush=True)
                return kept["value"]
        value = compute()
        os.makedirs(os.path.dirname(path), exist_ok=True)
        json.dump({"key": key, "value": value}, open(path + ".tmp", "w"))
        os.replace(path + ".tmp", path)
        return value

    # The decoder, each step's numbers from Strata's next codes among the
    # items the user does not have.
    def from_strata(hist, prefixes):
        where = f"NOT id IN ({quoted(sorted(set(hist)))})"
        got = db.sid_next([".".join(map(str, p)) for p in prefixes], field="sid", where=where, table="items")
        return {p: [int(code.rsplit(".", 1)[-1]) for code in got[".".join(map(str, p))]] for p in prefixes}

    def decode():
        checked, same_next = 0, 0
        generated, gen_ms, model_ms, next_ms = {}, [], [], []
        for j in range(0, len(users), a.decode_batch):
            us = users[j:j + a.decode_batch]
            srcs = [source(model, u, seqs[u][:-1], codes) for u in us]

            def allowed(k, ps, us=us):
                t = time.perf_counter()
                r = from_strata(seqs[us[k]][:-1], ps)
                next_ms.append((time.perf_counter() - t) * 1e3)
                return r

            got = beam_search(model, srcs, allowed, beam=a.beam)
            for u, g in zip(us, got):
                generated[u] = [by_code[tuple(x)] for x in g]
            # Strata's next codes against a trie of the same codes, at each
            # level of the first users' best beams.
            if checked < 300:
                for u, g in zip(us, got):
                    for x in g[:5]:
                        for l in range(len(x)):
                            p = tuple(x[:l])
                            checked += 1
                            same_next += from_strata(seqs[u][:-1], [p]) == trie.allowed(seqs[u][:-1], [p])
            print(f"decoded {min(j + a.decode_batch, len(users))} of {len(users)}", flush=True)
        # One request at a time: the decoder for one user, and its next codes.
        for u in timed_users:
            hist = seqs[u][:-1]
            t = time.perf_counter()
            spent = []

            def one(k, ps):
                s = time.perf_counter()
                r = from_strata(hist, ps)
                spent.append(time.perf_counter() - s)
                return r

            beam_search(model, [source(model, u, hist, codes)], one, beam=a.beam)
            total = (time.perf_counter() - t) * 1e3
            gen_ms.append(total)
            model_ms.append(total - sum(spent) * 1e3)
        return {"generated": generated, "checked": checked, "same_next": same_next, "ms": gen_ms, "model_ms": model_ms, "next_ms": next_ms}

    dec = None if a.no_decoder else cached("generated", key, decode)
    generated = {} if dec is None else dec["generated"]

    name = f"{a.data}-seq"
    hist_sql = "(SELECT item FROM events WHERE usr = :u AND seq < :n ORDER BY seq DESC LIMIT 50)"
    not_own = "NOT id IN (SELECT item FROM events WHERE usr = :u AND seq < :n)"
    seq_score = f"similarity(iv, encode('{name}', {hist_sql}))"
    walk_score = f"ppr(cooc, (SELECT item FROM events WHERE usr = :u AND seq < :n ORDER BY seq DESC LIMIT {WALK}), dir => 'out', eps => 1e-6)"
    queries = {
        "sequence encoder (A8)": f"SELECT id FROM items WHERE {not_own} ORDER BY {seq_score} DESC LIMIT 10",
        "item-to-item walk": f"SELECT id FROM items WHERE {not_own} ORDER BY {walk_score} DESC LIMIT 10",
        "generated (TIGER) + A8, rrf": f"SELECT id FROM items WHERE {not_own} ORDER BY rrf({seq_score}, ranked(:gen)) DESC LIMIT 10",
        "A8 + walk, rrf": f"SELECT id FROM items WHERE {not_own} ORDER BY rrf({seq_score}, {walk_score}) DESC LIMIT 10",
        "generated + A8 + walk, rrf": f"SELECT id FROM items WHERE {not_own} ORDER BY rrf({seq_score}, {walk_score}, ranked(:gen)) DESC LIMIT 10",
    }
    if dec is None:
        queries = {k: v for k, v in queries.items() if ":gen" not in v}
    params = lambda u: {"u": u, "n": len(seqs[u]) - 1, **({} if dec is None else {"gen": generated[u]})}  # noqa: E731

    def answers(sql):
        """Every user's top 10 by `sql`, on every core (a query runs
        without the GIL), and the time of each of the first users' queries
        run one at a time."""
        def one(u):
            return [r[0] for r in db.query(sql, params(u)).rows]
        for u in timed_users[:50]:
            one(u)
        ms = []
        for u in timed_users:
            t = time.perf_counter()
            one(u)
            ms.append((time.perf_counter() - t) * 1e3)
        with ThreadPoolExecutor(a.threads) as ex:
            got = dict(zip(users, ex.map(one, users)))
        return {"ranked": got, "ms": ms}

    results = {} if dec is None else {"generated (TIGER)": {"metrics": mean_metrics([metrics(generated[u][:10], seqs[u][-1]) for u in users]),
                                    "ms": {"p50": pct(dec["ms"], 50), "p99": pct(dec["ms"], 99), "model_p50": pct(dec["model_ms"], 50), "sid_next_call_p50": pct(dec["next_ms"], 50)}}}
    for i, (label, sql) in enumerate(queries.items()):
        r = cached(f"query-{i}", {**key, "sql": sql}, lambda sql=sql: answers(sql))
        results[label] = {"metrics": mean_metrics([metrics(r["ranked"][u], seqs[u][-1]) for u in users]), "ms": {"p50": pct(r["ms"], 50), "p99": pct(r["ms"], 99)}, "sql": sql}
        print(label, json.dumps(results[label]["metrics"]), flush=True)
    db.close()
    invalid = None if dec is None else sum(len(generated[u]) < 10 for u in users)
    out = {
        "data": {"name": a.data, "users": len(seqs), "items": len(codes), "events": sum(len(s) for s in seqs.values()), "evaluated_users": len(users)},
        "codes": c["info"],
        "machine": machine(),
        "tiger": None if saved is None else {"config": saved["config"], "epochs": len(saved["history"]), "best_epoch": max(saved["history"], key=lambda h: h["recall@10"])["epoch"], "seconds": saved["history"][-1]["seconds"], "history": saved["history"], "beam": a.beam},
        "a8": json.load(open(os.path.join(work, "a8.json"))),
        "users_with_fewer_than_10_generated": invalid,
        "sid_next_equal_to_trie": None if dec is None else f"{dec['same_next']}/{dec['checked']}",
        "timed_users": len(timed_users),
        "results": results,
    }
    print(json.dumps({k: v for k, v in out.items() if k in ("results", "sid_next_equal_to_trie", "users_with_fewer_than_10_generated")}, indent=1), flush=True)
    os.makedirs(a.out, exist_ok=True)
    json.dump(out, open(os.path.join(a.out, f"{a.data}{a.tag}.json"), "w"), indent=1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", choices=sorted(FILES), required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("--models", required=True)
    ap.add_argument("--out", default=".")
    ap.add_argument("--stage", choices=["prep", "tiger", "a8", "eval"], required=True)
    ap.add_argument("--max-minutes", type=float, default=None)
    ap.add_argument("--dim", type=int, default=64, help="A8's item vectors")
    ap.add_argument("--tiger-width", type=int, default=128)
    ap.add_argument("--tiger-heads", type=int, default=4)
    ap.add_argument("--tiger-layers", type=int, default=4)
    ap.add_argument("--tiger-ffn", type=int, default=1024)
    ap.add_argument("--tiger-lr", type=float, default=1e-3)
    ap.add_argument("--tiger-tokens", choices=["item", "code"], default="item", help="an input position an item (the sum of its numbers' embeddings) or a number, as TIGER")
    ap.add_argument("--tiger-epochs", type=int, default=200)
    ap.add_argument("--batch", type=int, default=48, help="the decoder's windows a step (about 6 targets each on these sets)")
    ap.add_argument("--patience", type=int, default=5)
    ap.add_argument("--beam", type=int, default=20)
    ap.add_argument("--val-users", type=int, default=2000)
    ap.add_argument("--decode-batch", type=int, default=64)
    ap.add_argument("--threads", type=int, default=os.cpu_count())
    ap.add_argument("--users", type=int, default=None, help="evaluate this many users (all by default)")
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--no-decoder", action="store_true", help="eval: the queries without the decoder (no tiger stage needed)")
    ap.add_argument("--tag", default="", help="eval: appended to the output's name (<data><tag>.json)")
    a = ap.parse_args()
    torch.set_num_threads(a.threads)
    os.makedirs(a.work, exist_ok=True)
    seqs = sequences(os.path.join(a.dir, FILES[a.data][0]))
    print(f"{a.data}: {len(seqs)} users, {sum(len(s) for s in seqs.values())} events", flush=True)
    models = os.path.expanduser(a.models)
    {"prep": lambda: prep(a, seqs, a.work, models), "tiger": lambda: tiger(a, seqs, a.work), "a8": lambda: a8(a, seqs, a.work, models), "eval": lambda: evaluate(a, seqs, a.work, models)}[a.stage]()


if __name__ == "__main__":
    main()
