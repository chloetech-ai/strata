#!/usr/bin/env python3
"""A feature store demo on ChloeDB: a shop that ranks its products for each
shopper by a model, fed by features ChloeDB keeps.

    python3 featurestore.py serve --local DIR     # starts chloedb-server too
    python3 featurestore.py serve --server URL    # an app of a running server

The parts, each visible in its command center page (Apps -> Feature store):

- shoppers (simulated) ask the shop for a ranking, see 8 products and click
  some; every impression is a row of `fs_impressions`, its click the label;
- streaming features: the same transaction as the impressions adds to the
  shopper's, the product's and the shopper x category's counters
  (`fs_user_online`, `fs_item_online`, `fs_usercat_online`), so a click is
  in the features the moment its write returns;
- a batch feature: every 10 s a SQL aggregate over the last 2 minutes of
  impressions (`fs_item_trend`, a product's recent click rate);
- snapshots: every 2 s the rows that changed are copied with their time into
  `*_snap`, the history a training set is joined to;
- transformations: smoothed rates, shares, a log and a clipped price ratio,
  one Python definition used for training and for serving alike;
- the training set: an ASOF JOIN of the impressions to the snapshots, each
  row with the features as they were just before it (and, to compare, the
  join to the latest rows, which leaks the label);
- models: logistic regression, stored as rows of `fs_models`, one deployed;
- serving: four reads by key (`/v1/kv/get`) for the shopper and all products,
  the transformations, the model, the top 8.

Only the standard library. It talks to chloedb-server over HTTP.
"""

import argparse
import http.client
import json
import math
import os
import random
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import workflows as wfm  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
VERSION = "0.1.0"

# ---------------------------------------------------------------- the server


class Chloe:
    """chloedb-server over HTTP, one connection a thread."""

    def __init__(self, url, key=None):
        u = urllib.parse.urlparse(url)
        self.host, self.port = u.hostname, u.port or 80
        self.key = key
        self.local = threading.local()

    def call(self, method, path, body=None, timeout=120):
        for attempt in range(4):
            c = getattr(self.local, "c", None)
            if c is None:
                c = self.local.c = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
            headers = {"content-type": "application/json"}
            if self.key:
                headers["authorization"] = "Bearer " + self.key
            try:
                c.request(method, path, None if body is None else json.dumps(body), headers)
                r = c.getresponse()
                data = r.read()
            except (OSError, http.client.HTTPException):
                self.local.c = None
                if attempt == 3:
                    raise
                time.sleep(0.2 * (attempt + 1))
                continue
            if r.status == 429 and attempt < 3:
                time.sleep(0.3 * (attempt + 1))
                continue
            if r.status >= 300:
                raise ChloeError(r.status, data.decode(errors="replace")[:500])
            return json.loads(data) if data else {}

    def sql(self, q):
        return self.call("POST", "/v1/sql", {"sql": q})

    def query(self, q, params=None, timeout_ms=120000):
        b = {"sql": q, "objects": True, "timeout_ms": timeout_ms}
        if params:
            b["params"] = params
        return self.call("POST", "/v1/query", b)

    def rows(self, q, params=None):
        return self.query(q, params)["rows"]

    def write(self, ops):
        return self.call("POST", "/v1/write", {"ops": ops})

    def get(self, table, keys, columns=None):
        b = {"table": table, "keys": keys}
        if columns:
            b["columns"] = columns
        return self.call("POST", "/v1/kv/get", b)["rows"]


class ChloeError(Exception):
    def __init__(self, status, text):
        super().__init__(f"{status}: {text}")
        self.status = status


TABLES = [
    "CREATE TABLE IF NOT EXISTS fs_items (id KEYWORD PRIMARY KEY, title TEXT, category KEYWORD, price FLOAT)",
    "CREATE TABLE IF NOT EXISTS fs_users (id KEYWORD PRIMARY KEY, name KEYWORD)",
    # The event log, and the labels: one row a product shown, clicked or not.
    "CREATE TABLE IF NOT EXISTS fs_impressions (id KEYWORD PRIMARY KEY, ts INT, bucket INT, req KEYWORD, usr KEYWORD, item KEYWORD,"
    " category KEYWORD, uc KEYWORD, price FLOAT, pos INT, model KEYWORD, clicked INT DEFAULT 0)",
    # Streaming features, added to in the impressions' transaction.
    "CREATE TABLE IF NOT EXISTS fs_user_online (id KEYWORD PRIMARY KEY, views INT DEFAULT 0, clicks INT DEFAULT 0, click_price FLOAT DEFAULT 0, ts INT DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS fs_item_online (id KEYWORD PRIMARY KEY, views INT DEFAULT 0, clicks INT DEFAULT 0, ts INT DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS fs_usercat_online (id KEYWORD PRIMARY KEY, views INT DEFAULT 0, clicks INT DEFAULT 0, ts INT DEFAULT 0)",
    # The batch feature, written every 10 s.
    "CREATE TABLE IF NOT EXISTS fs_item_trend (id KEYWORD PRIMARY KEY, views_2m INT, clicks_2m INT, ts INT)",
    # Snapshots: each feature row as it was at a time.
    "CREATE TABLE IF NOT EXISTS fs_user_snap (id KEYWORD PRIMARY KEY, usr KEYWORD, ts INT, views INT, clicks INT, click_price FLOAT)",
    "CREATE TABLE IF NOT EXISTS fs_item_snap (id KEYWORD PRIMARY KEY, item KEYWORD, ts INT, views INT, clicks INT, views_2m INT, clicks_2m INT)",
    "CREATE TABLE IF NOT EXISTS fs_usercat_snap (id KEYWORD PRIMARY KEY, uc KEYWORD, ts INT, views INT, clicks INT)",
    # Models, as rows.
    "CREATE TABLE IF NOT EXISTS fs_models (id KEYWORD PRIMARY KEY, created INT, name KEYWORD, join_kind KEYWORD, features TEXT, weights TEXT,"
    " train_rows INT, auc_train FLOAT, auc_holdout FLOAT, auc_true FLOAT, build_ms FLOAT, train_ms FLOAT, sql TEXT, deployed INT DEFAULT 0)",
]

# ------------------------------------------------------------------- the shop

CATS = {
    "outdoor": (["Tent", "Lantern", "Backpack", "Sleeping bag", "Camp stove", "Hiking poles"], 25, 220),
    "kitchen": (["Chef's knife", "Skillet", "Kettle", "Cutting board", "Dutch oven", "Grinder"], 12, 160),
    "books": (["Novel", "Cookbook", "Atlas", "Field guide", "Biography", "Poetry"], 8, 40),
    "audio": (["Headphones", "Speaker", "Turntable", "Earbuds", "Microphone", "Radio"], 20, 300),
    "garden": (["Pruner", "Hose", "Planter", "Seed kit", "Trowel", "Watering can"], 6, 90),
    "games": (["Board game", "Puzzle", "Card deck", "Chess set", "Dice set", "Party game"], 10, 70),
    "sport": (["Yoga mat", "Dumbbells", "Jump rope", "Water bottle", "Running belt", "Foam roller"], 8, 120),
    "home": (["Desk lamp", "Throw blanket", "Candle", "Clock", "Vase", "Rug"], 10, 180),
}
ADJ = ["Classic", "Light", "Pro", "Compact", "Everyday", "Deluxe", "Trail", "Studio", "Little", "Grand", "Quiet", "Bright"]
FIRST = ["Ada", "Ben", "Cleo", "Dev", "Eli", "Fay", "Gus", "Hana", "Ivo", "Juno", "Kai", "Lea", "Max", "Nia", "Oto", "Pia", "Quin", "Rae", "Sol", "Tao"]
K = 8  # products shown a request
SHOPPER_THREADS = 3


class World:
    """The shoppers' and products' hidden truth, which the model never sees:
    each shopper likes two categories and minds prices more or less, each
    product is better or worse, and some products are hot for a while."""

    def __init__(self, n_users=300, n_items=240, seed=11):
        r = random.Random(seed)
        self.cats = list(CATS)
        self.items = []
        for i in range(n_items):
            cat = self.cats[i % len(self.cats)]
            nouns, lo, hi = CATS[cat]
            price = round(math.exp(r.uniform(math.log(lo), math.log(hi))), 2)
            self.items.append({"id": f"p{i:03d}", "title": f"{r.choice(ADJ)} {r.choice(nouns).lower()}",
                               "category": cat, "price": price, "quality": r.gauss(0, 0.7)})
        self.by_id = {it["id"]: it for it in self.items}
        self.mean_logp = {c: sum(math.log(it["price"]) for it in self.items if it["category"] == c) / sum(1 for it in self.items if it["category"] == c) for c in self.cats}
        self.users = []
        for u in range(n_users):
            likes = r.sample(self.cats, 2)
            pref = {c: (1.6 if c == likes[0] else 1.0 if c == likes[1] else -0.4 + r.gauss(0, 0.2)) for c in self.cats}
            self.users.append({"id": f"u{u:03d}", "name": f"{r.choice(FIRST)} {chr(65 + u % 26)}.", "pref": pref, "likes": likes,
                               "price_sens": r.uniform(0.2, 1.6), "activity": r.paretovariate(1.5)})
        self.user_by_id = {u["id"]: u for u in self.users}
        self.weights = [u["activity"] for u in self.users]
        self.r = r
        self.hot = {}
        self.hot_until = 0

    def hot_items(self, now):
        # Every 3 minutes, six products turn hot (or cold) for the next 3.
        epoch = now // 180_000
        if epoch != self.hot_until:
            r = random.Random(epoch)
            self.hot = {it["id"]: r.choice([1.4, 1.2, -1.2]) for it in r.sample(self.items, 6)}
            self.hot_until = epoch
        return self.hot

    def p_click(self, user, item, pos, now):
        z = -3.0 + user["pref"][item["category"]] + item["quality"] - user["price_sens"] * (math.log(item["price"]) - self.mean_logp[item["category"]])
        z += self.hot_items(now).get(item["id"], 0.0)
        z -= 0.22 * pos
        return 1 / (1 + math.exp(-z))


# ------------------------------------------------------------ transformations

# One definition of each feature the model reads, used to train and to serve.
# `raw` is a dict of the joined columns (None where a row is missing).


def _n(v):
    return 0.0 if v is None else float(v)


TRANSFORMS = [
    ("u_ctr", "streaming", "(user.clicks + 1) / (user.views + 10)", "the shopper's click rate, smoothed toward 10%",
     lambda r: (_n(r["u_clicks"]) + 1) / (_n(r["u_views"]) + 10)),
    ("i_ctr", "streaming", "(item.clicks + 1) / (item.views + 10)", "the product's click rate, all time",
     lambda r: (_n(r["i_clicks"]) + 1) / (_n(r["i_views"]) + 10)),
    ("i_trend", "batch", "(trend.clicks_2m + 1) / (trend.views_2m + 10)", "the product's click rate over the last 2 minutes, every 10 s",
     lambda r: (_n(r["clicks_2m"]) + 1) / (_n(r["views_2m"]) + 10)),
    ("uc_ctr", "streaming", "(usercat.clicks + 1) / (usercat.views + 5)", "the shopper's click rate in the product's category",
     lambda r: (_n(r["uc_clicks"]) + 1) / (_n(r["uc_views"]) + 5)),
    ("uc_share", "streaming", "usercat.clicks / (user.clicks + 1)", "the share of the shopper's clicks in this category",
     lambda r: _n(r["uc_clicks"]) / (_n(r["u_clicks"]) + 1)),
    ("log_views", "streaming", "ln(1 + item.views)", "how much the product was shown",
     lambda r: math.log1p(_n(r["i_views"]))),
    ("price_ratio", "streaming", "clip(ln(price / (user.click_price / user.clicks)), -2, 2); 0 with no clicks",
     "the price against what the shopper clicked before; cleaned: missing to 0, clipped",
     lambda r: 0.0 if not r["u_clicks"] else max(-2.0, min(2.0, math.log(_n(r["price"]) / max(0.01, _n(r["u_click_price"]) / _n(r["u_clicks"])))))),
]
FEATURES = [t[0] for t in TRANSFORMS]


def transform(raw):
    return [f(raw) for (_, _, _, _, f) in TRANSFORMS]


# --------------------------------------------------------------------- models


def sigmoid(z):
    return 1 / (1 + math.exp(-max(-30.0, min(30.0, z))))


def train_lr(X, y, epochs=6, lr=0.08, l2=1e-4, seed=1):
    """Logistic regression by SGD on standardized features (pure Python)."""
    n, d = len(X), len(X[0]) if X else 0
    mean = [sum(x[j] for x in X) / max(1, n) for j in range(d)]
    std = [max(1e-6, math.sqrt(sum((x[j] - mean[j]) ** 2 for x in X) / max(1, n))) for j in range(d)]
    Z = [[(x[j] - mean[j]) / std[j] for j in range(d)] for x in X]
    w, b = [0.0] * d, math.log((sum(y) + 1) / (n - sum(y) + 1))
    order = list(range(n))
    r = random.Random(seed)
    for e in range(epochs):
        r.shuffle(order)
        step = lr / (1 + e)
        for i in order:
            z = Z[i]
            g = sigmoid(b + sum(w[j] * z[j] for j in range(d))) - y[i]
            b -= step * g
            for j in range(d):
                w[j] -= step * (g * z[j] + l2 * w[j])
    return {"mean": mean, "std": std, "w": w, "b": b}


def score(m, x):
    return sigmoid(m["b"] + sum(m["w"][j] * (x[j] - m["mean"][j]) / m["std"][j] for j in range(len(x))))


def contributions(m, x):
    return [m["w"][j] * (x[j] - m["mean"][j]) / m["std"][j] for j in range(len(x))]


def auc(scores, labels):
    pairs = sorted(zip(scores, labels))
    pos = sum(labels)
    neg = len(labels) - pos
    if not pos or not neg:
        return None
    rank, s, i = 0, 0.0, 0
    while i < len(pairs):
        j = i
        while j < len(pairs) and pairs[j][0] == pairs[i][0]:
            j += 1
        avg = (i + j + 1) / 2
        s += avg * sum(l for _, l in pairs[i:j])
        i = j
    return (s - pos * (pos + 1) / 2) / (pos * neg)


# ---------------------------------------------------------------- the queries

RAW_COLS = ["u_views", "u_clicks", "u_click_price", "i_views", "i_clicks", "views_2m", "clicks_2m", "uc_views", "uc_clicks", "price"]

ASOF_SQL = """SELECT s.id AS id, s.ts AS ts, s.clicked AS label, s.price AS price,
  u.views AS u_views, u.clicks AS u_clicks, u.click_price AS u_click_price, u.ts AS u_ts,
  i.views AS i_views, i.clicks AS i_clicks, i.views_2m AS views_2m, i.clicks_2m AS clicks_2m, i.ts AS i_ts,
  c.views AS uc_views, c.clicks AS uc_clicks, c.ts AS c_ts
FROM fs_impressions s
ASOF JOIN fs_user_snap u MATCH_CONDITION (s.ts > u.ts) ON s.usr = u.usr
ASOF JOIN fs_item_snap i MATCH_CONDITION (s.ts > i.ts) ON s.item = i.item
ASOF JOIN fs_usercat_snap c MATCH_CONDITION (s.ts > c.ts) ON s.uc = c.uc
WHERE s.ts >= :since AND s.ts < :until"""

LEAKY_SQL = """SELECT s.id AS id, s.ts AS ts, s.clicked AS label, s.price AS price,
  u.views AS u_views, u.clicks AS u_clicks, u.click_price AS u_click_price, u.ts AS u_ts,
  i.views AS i_views, i.clicks AS i_clicks, t.views_2m AS views_2m, t.clicks_2m AS clicks_2m, i.ts AS i_ts,
  c.views AS uc_views, c.clicks AS uc_clicks, c.ts AS c_ts
FROM fs_impressions s
LEFT JOIN fs_user_online u ON s.usr = u.id
LEFT JOIN fs_item_online i ON s.item = i.id
LEFT JOIN fs_item_trend t ON s.item = t.id
LEFT JOIN fs_usercat_online c ON s.uc = c.id
WHERE s.ts >= :since AND s.ts < :until"""

TREND_SQL = """SELECT item, COUNT(*) AS views_2m, SUM(clicked) AS clicks_2m
FROM fs_impressions WHERE ts > :since AND ts <= :now GROUP BY item"""

# ---------------------------------------------------------------------- state


class Ring:
    def __init__(self, n=600):
        self.n, self.v = n, []
        self.lock = threading.Lock()

    def add(self, x):
        with self.lock:
            self.v.append(x)
            if len(self.v) > self.n:
                del self.v[: len(self.v) - self.n]

    def all(self):
        with self.lock:
            return list(self.v)


def pct(v, p):
    if not v:
        return None
    s = sorted(v)
    return s[min(len(s) - 1, int(p * len(s)))]


# Impressions kept (the shoppers write about a hundred a second), and those
# a model is trained on by default: training reads its whole window, so a
# demo left running for a day must not train on the day (DAP-12).
KEEP_MIN = 180
TRAIN_MIN = 30


class App:
    def __init__(self, db, rate=12.0, backfill_min=12, keep_min=KEEP_MIN):
        self.db = db
        self.keep_min = keep_min
        self.last_prune = 0
        self.world = World()
        self.rate = rate
        self.running = True
        self.backfill_min = backfill_min
        self.lock = threading.Lock()
        self.model = None  # the deployed model, as a row (dict) with "m"
        self.changed = {"user": set(), "item": set(), "uc": set()}
        self.counts = {"impressions": 0, "clicks": 0, "writes": 0, "snap_rows": 0, "trend_runs": 0, "requests": 0}
        self.serve_ms = Ring(2000)      # (t, total, fetch, transform, score, rows read by key)
        self.write_ms = Ring(2000)      # (t, ms, ops)
        self.fresh_ms = Ring(600)       # (t, ms): a click written to visible in the features
        self.jobs = Ring(200)           # {t, job, ms, rows}
        self.rates = Ring(600)          # (t, impressions/s, clicks/s, requests/s)
        self.ctr = Ring(2000)           # (t, model, shown, clicked)
        self.phase = "starting"
        self.error = None
        self.seq = 0
        self.last_trend = 0
        self.last_snap = 0
        self.stop = threading.Event()
        self.wf = wfm.Runner(db)
        self.wf.readers.append(self.models_reading)
        self.wf_status = "starting"

    def models_reading(self, wf):
        """The deployed model, when it reads workflow `wf`'s table: deleting
        the workflow would drop the table every ranking reads."""
        m = self.model
        if not m or not m.get("spec"):
            return []
        spec = json.loads(m["spec"])
        used = {f.split(".", 1)[0] for f in spec.get("features", [])}
        if any(j["workflow"] == wf and wf in used for j in spec.get("joins", [])):
            return [f"the deployed model {m.get('name') or m['id']}"]
        return []

    # -- setup

    def setup(self):
        for t in TABLES:
            self.db.sql(t)
        have = self.db.rows("SELECT COUNT(*) AS n FROM fs_items")[0]["n"]
        if not have:
            ops = [{"op": "upsert", "table": "fs_items", "doc": {k: it[k] for k in ("id", "title", "category", "price")}} for it in self.world.items]
            ops += [{"op": "upsert", "table": "fs_users", "doc": {"id": u["id"], "name": u["name"]}} for u in self.world.users]
            self.db.write(ops)
        self.db.sql("ALTER TABLE fs_models ADD COLUMN IF NOT EXISTS spec TEXT")
        self.wf.setup()
        m = self.db.rows("SELECT id FROM fs_models WHERE deployed = 1 ORDER BY created DESC LIMIT 1")
        if m:
            self.model = self.load_model(m[0]["id"])
        c = self.db.rows("SELECT COUNT(*) AS n, SUM(clicked) AS c FROM fs_impressions")[0]
        n = c["n"]
        self.counts["impressions"], self.counts["clicks"] = n, c["c"] or 0
        if n == 0 and self.backfill_min:
            self.backfill(self.backfill_min)
        self.phase = "live"

    def backfill(self, minutes):
        """History to train on: `minutes` of shoppers on a random ranking, at
        the simulated times they would have happened, with the same jobs."""
        self.phase = "backfill"
        now = int(time.time() * 1000)
        t = now - minutes * 60_000
        step = 1000 / self.rate
        pending = []
        while t < now:
            pending += self.session_ops(t, None)
            if len(pending) > 1500:
                self.write(pending, t)
                pending = []
            if t - self.last_snap >= 2000:
                if pending:
                    self.write(pending, t)
                    pending = []
                self.snapshot(t)
            if t - self.last_trend >= 10_000:
                if pending:
                    self.write(pending, t)
                    pending = []
                self.trend(t)
            t += int(random.expovariate(1 / step)) + 1
        if pending:
            self.write(pending, now)
        self.snapshot(now)

    def seed_workflows(self, minutes=10):
        """The shop's workflows, on a new database: made and backfilled."""
        try:
            if not self.wf.specs:
                self.wf_status = "making the shop's workflows"
                for spec in wfm.SEEDS:
                    self.wf.save(spec)
                self.wf_status = f"backfilling {minutes} minutes of history"
                for spec in wfm.SEEDS:
                    self.wf.backfill(spec["name"], minutes, step_s=30)
            self.wf_status = "live"
        except Exception as e:  # noqa: BLE001
            self.wf_status = f"failed: {e}"

    # -- sessions

    def rank(self, user_id, model=None, explain=False):
        """The shop's answer for a shopper: features by key, transformed,
        scored, the top K. Timed by part."""
        model = model if model is not None else self.model
        t0 = time.perf_counter()
        items = self.world.items
        if model is None:
            ids = random.sample([it["id"] for it in items], K)
            return {"items": [{"id": i, "score": None} for i in ids], "model": "random", "ms": {"fetch": 0, "transform": 0, "score": 0, "total": (time.perf_counter() - t0) * 1000}, "rows": 0}
        if model.get("spec"):
            return self.rank_workflows(user_id, model, explain, t0)
        keys = [it["id"] for it in items]
        ucs = [f"{user_id}|{c}" for c in self.world.cats]
        t1 = time.perf_counter()
        u = self.db.get("fs_user_online", [user_id], ["views", "clicks", "click_price"])[0]
        io = self.db.get("fs_item_online", keys, ["views", "clicks"])
        tr = self.db.get("fs_item_trend", keys, ["views_2m", "clicks_2m"])
        uc = dict(zip(self.world.cats, self.db.get("fs_usercat_online", ucs, ["views", "clicks"])))
        t2 = time.perf_counter()
        rows = []
        for it, a, b in zip(items, io, tr):
            c = uc[it["category"]] or {}
            u_ = u or {}
            a = a or {}
            b = b or {}
            raw = {"u_views": u_.get("views"), "u_clicks": u_.get("clicks"), "u_click_price": u_.get("click_price"),
                   "i_views": a.get("views"), "i_clicks": a.get("clicks"), "views_2m": b.get("views_2m"), "clicks_2m": b.get("clicks_2m"),
                   "uc_views": c.get("views"), "uc_clicks": c.get("clicks"), "price": it["price"]}
            rows.append((it, raw, transform(raw)))
        t3 = time.perf_counter()
        m = model["m"]
        scored = sorted(((score(m, x), it, raw, x) for it, raw, x in rows), key=lambda r: -r[0])[:K]
        t4 = time.perf_counter()
        out = []
        for s, it, raw, x in scored:
            e = {"id": it["id"], "score": s}
            if explain:
                e.update({"title": it["title"], "category": it["category"], "price": it["price"], "raw": raw,
                          "features": dict(zip(FEATURES, x)), "contrib": dict(zip(FEATURES, contributions(m, x)))})
            out.append(e)
        ms = {"fetch": (t2 - t1) * 1000, "transform": (t3 - t2) * 1000, "score": (t4 - t3) * 1000, "total": (t4 - t0) * 1000}
        # Rows read by key: the shopper's, two of each product's, one of each category's.
        return {"items": out, "model": model["id"], "ms": ms, "candidates": len(items), "rows": 1 + len(io) + len(tr) + len(ucs)}

    def context(self, user_id, it):
        """What the shop knows of a request for each product: the columns of
        an impression, before it is written."""
        return {"usr": user_id, "item": it["id"], "category": it["category"], "uc": f"{user_id}|{it['category']}", "price": it["price"], "pos": 0}

    def rank_workflows(self, user_id, model, explain, t0):
        spec = json.loads(model["spec"])
        feats = json.loads(model["features"])
        items = self.world.items
        ctxs = [self.context(user_id, it) for it in items]
        t1 = time.perf_counter()
        got = {}
        n_rows = 0
        for j in spec["joins"]:
            cols = [f.split(".", 1)[1] for f in feats if f.split(".", 1)[0] == j["workflow"]]
            if not cols:
                continue
            if j["on"] not in ctxs[0]:
                raise ValueError(f"the shop does not know {j['on']} before an impression")
            keys = sorted({str(c[j["on"]]) for c in ctxs})
            got[j["workflow"]] = (j["on"], dict(zip(keys, self.db.get(wfm.target(j["workflow"]), keys, cols))))
            n_rows += len(keys)
        t2 = time.perf_counter()
        X = []
        for c in ctxs:
            x = []
            for f in feats:
                w, col = f.split(".", 1)
                if w == "shop":
                    v = c.get(col)
                else:
                    on, rows = got[w]
                    v = (rows.get(str(c[on])) or {}).get(col)
                x.append(float(v) if isinstance(v, (int, float)) else 0.0)
            X.append(x)
        t3 = time.perf_counter()
        m = json.loads(model["weights"]) if "m" not in model else model["m"]
        scored = sorted(((score(m, x), it, x) for it, x in zip(items, X)), key=lambda r: -r[0])[:K]
        t4 = time.perf_counter()
        out = []
        for sc, it, x in scored:
            e = {"id": it["id"], "score": sc}
            if explain:
                e.update({"title": it["title"], "category": it["category"], "price": it["price"],
                          "features": dict(zip(feats, x)), "contrib": dict(zip(feats, contributions(m, x)))})
            out.append(e)
        ms = {"fetch": (t2 - t1) * 1000, "transform": (t3 - t2) * 1000, "score": (t4 - t3) * 1000, "total": (t4 - t0) * 1000}
        return {"items": out, "model": model["id"], "ms": ms, "candidates": len(items), "reads": len(got), "rows": n_rows}

    def session_ops(self, now, ranked, user=None):
        w = self.world
        user = user or w.r.choices(w.users, w.weights)[0]
        if ranked is None:
            ids = w.r.sample(range(len(w.items)), K)
            shown, model = [w.items[i] for i in ids], "random"
        else:
            shown, model = [w.by_id[e["id"]] for e in ranked["items"]], ranked["model"]
        with self.lock:
            self.seq += 1
            req = f"r{now}-{self.seq}"
        clicked = [w.r.random() < w.p_click(user, it, pos, now) for pos, it in enumerate(shown)]
        return self.impression_ops(now, req, user["id"], shown, model, clicked)

    def impression_ops(self, now, req, uid, shown, model, clicked):
        with self.lock:
            return self._impression_ops(now, req, uid, shown, model, clicked)

    def _impression_ops(self, now, req, uid, shown, model, clicked):
        ops = []
        nv, nc, cp = len(shown), sum(clicked), sum(it["price"] for it, c in zip(shown, clicked) if c)
        for pos, (it, c) in enumerate(zip(shown, clicked)):
            uc = f"{uid}|{it['category']}"
            ops.append({"op": "insert", "table": "fs_impressions", "doc": {"id": f"{req}:{pos}", "ts": now, "bucket": now // 10_000, "req": req, "usr": uid,
                        "item": it["id"], "category": it["category"], "uc": uc, "price": it["price"], "pos": pos, "model": model, "clicked": int(c)}})
            ops += self.add_ops("fs_item_online", it["id"], {"views": 1, "clicks": int(c)}, now)
            ops += self.add_ops("fs_usercat_online", uc, {"views": 1, "clicks": int(c)}, now)
            self.changed["item"].add(it["id"])
            self.changed["uc"].add(uc)
        ops += self.add_ops("fs_user_online", uid, {"views": nv, "clicks": nc, "click_price": cp}, now)
        self.changed["user"].add(uid)
        self.counts["impressions"] += nv
        self.counts["clicks"] += nc
        self.ctr.add((now, model, nv, nc))
        return ops

    @staticmethod
    def add_ops(table, key, adds, now):
        ops = [{"op": "add", "table": table, "key": key, "field": f, "value": v, "doc": {"id": key}} for f, v in adds.items() if v]
        ops.append({"op": "set", "table": table, "key": key, "field": "ts", "value": now})
        if len(ops) == 1:  # nothing to add, but the row must exist for the set
            ops.insert(0, {"op": "add", "table": table, "key": key, "field": "views", "value": 0, "doc": {"id": key}})
        return ops

    def write(self, ops, now=None):
        t0 = time.perf_counter()
        self.db.write(ops)
        ms = (time.perf_counter() - t0) * 1000
        self.counts["writes"] += 1
        self.write_ms.add((now or int(time.time() * 1000), ms, len(ops)))
        return ms

    # -- jobs

    def snapshot(self, now):
        with self.lock:
            ch = {k: sorted(v) for k, v in self.changed.items()}
            self.changed = {"user": set(), "item": set(), "uc": set()}
        t0 = time.perf_counter()
        ops = []
        for chunk in chunks(ch["user"], 500):
            for k, r in zip(chunk, self.db.get("fs_user_online", chunk, ["views", "clicks", "click_price"])):
                if r:
                    ops.append({"op": "upsert", "table": "fs_user_snap", "doc": {"id": f"{k}@{now}", "usr": k, "ts": now, **r}})
        for chunk in chunks(ch["item"], 500):
            a = self.db.get("fs_item_online", chunk, ["views", "clicks"])
            b = self.db.get("fs_item_trend", chunk, ["views_2m", "clicks_2m"])
            for k, r, t in zip(chunk, a, b):
                if r:
                    ops.append({"op": "upsert", "table": "fs_item_snap", "doc": {"id": f"{k}@{now}", "item": k, "ts": now, **r, **(t or {"views_2m": 0, "clicks_2m": 0})}})
        for chunk in chunks(ch["uc"], 500):
            for k, r in zip(chunk, self.db.get("fs_usercat_online", chunk, ["views", "clicks"])):
                if r:
                    ops.append({"op": "upsert", "table": "fs_usercat_snap", "doc": {"id": f"{k}@{now}", "uc": k, "ts": now, **r}})
        for c in chunks(ops, 2000):
            self.db.write(c)
        self.counts["snap_rows"] += len(ops)
        self.last_snap = now
        self.jobs.add({"t": now, "job": "snapshot", "ms": (time.perf_counter() - t0) * 1000, "rows": len(ops)})

    def trend(self, now):
        t0 = time.perf_counter()
        rows = self.db.rows(TREND_SQL, {"since": now - 120_000, "now": now})
        t1 = time.perf_counter()
        got = {r["item"]: r for r in rows}
        ops = []
        for it in self.world.items:
            r = got.get(it["id"], {"views_2m": 0, "clicks_2m": 0})
            ops.append({"op": "upsert", "table": "fs_item_trend", "doc": {"id": it["id"], "views_2m": r["views_2m"], "clicks_2m": r["clicks_2m"], "ts": now}})
            self.changed["item"].add(it["id"])
        self.db.write(ops)
        self.counts["trend_runs"] += 1
        self.last_trend = now
        self.jobs.add({"t": now, "job": "trend", "ms": (time.perf_counter() - t0) * 1000, "query_ms": (t1 - t0) * 1000, "rows": len(ops)})

    # -- the live loop

    def shoppers(self, count_rates=True):
        last_rate = time.time()
        n_imp, n_clk = self.counts["impressions"], self.counts["clicks"]
        req0 = self.counts["requests"]
        while not self.stop.is_set():
            if not self.running:
                time.sleep(0.2)
                continue
            try:
                t = time.perf_counter()
                user = self.world.r.choices(self.world.users, self.world.weights)[0]
                ranked = self.rank(user["id"])
                self.serve_ms.add((int(time.time() * 1000), ranked["ms"]["total"], ranked["ms"]["fetch"], ranked["ms"]["transform"], ranked["ms"]["score"], ranked["rows"]))
                self.counts["requests"] += 1
                now = int(time.time() * 1000)
                ops = self.session_ops(now, ranked if ranked["model"] != "random" else None, user)
                self.write(ops, now)
                # Freshness: how long until the shopper's counters show it.
                if random.random() < 0.2:
                    v = (self.db.get("fs_user_online", [user["id"]], ["ts"])[0] or {}).get("ts")
                    if v == now:
                        self.fresh_ms.add((now, time.time() * 1000 - now))
                spent = time.perf_counter() - t
                time.sleep(max(0.0, random.expovariate(self.rate / SHOPPER_THREADS) - spent))
            except Exception as e:  # keep going; show the last error
                self.error = f"shoppers: {e}"
                time.sleep(1)
            if not count_rates:
                continue
            if time.time() - last_rate >= 1:
                dt = time.time() - last_rate
                # Every shopper thread's requests of the window, as the
                # impressions are (DAP-15: those made by the other threads
                # while this one slept were never counted).
                req = self.counts["requests"]
                self.rates.add((int(time.time() * 1000), (self.counts["impressions"] - n_imp) / dt, (self.counts["clicks"] - n_clk) / dt, (req - req0) / dt))
                n_imp, n_clk, req0, last_rate = self.counts["impressions"], self.counts["clicks"], req, time.time()

    def jobs_loop(self):
        while not self.stop.is_set():
            now = int(time.time() * 1000)
            try:
                if now - self.last_trend >= 10_000:
                    self.trend(now)
                if now - self.last_snap >= 2000:
                    self.snapshot(now)
                if self.wf_status == "live":
                    self.wf.tick(now)
                if self.keep_min and now - self.last_prune >= 60_000:
                    self.prune(now)
            except Exception as e:
                self.error = f"jobs: {e}"
            time.sleep(0.25)

    def prune(self, now):
        """Lets the impressions older than `keep_min` go."""
        self.last_prune = now
        self.db.sql(f"DELETE FROM fs_impressions WHERE ts < {now - self.keep_min * 60_000}")

    # -- training

    def training_set(self, leaky, since, until):
        sql = LEAKY_SQL if leaky else ASOF_SQL
        t0 = time.perf_counter()
        rows = self.db.rows(sql, {"since": since, "until": until})
        return rows, (time.perf_counter() - t0) * 1000, sql

    def train(self, leaky=False, minutes=None):
        """Train on the impressions up to a minute ago; hold out the last
        minute. Score each model on the held-out rows twice: with the
        features of its own join, and with the features serving would have
        had then (the ASOF JOIN's), which is the truth."""
        now = int(time.time() * 1000)
        cut = now - 60_000
        since = now - int(minutes or TRAIN_MIN) * 60_000
        rows, build_ms, sql = self.training_set(leaky, since, cut)
        if len(rows) < 200:
            raise ValueError(f"only {len(rows)} impressions to train on")
        X = [transform(r) for r in rows]
        y = [int(r["label"]) for r in rows]
        t0 = time.perf_counter()
        m = train_lr(X, y)
        train_ms = (time.perf_counter() - t0) * 1000
        hold_own, _, _ = self.training_set(leaky, cut, now)
        hold_true, _, _ = self.training_set(False, cut, now)
        a_train = auc([score(m, x) for x in X[:20000]], y[:20000])
        a_hold = auc([score(m, transform(r)) for r in hold_own], [int(r["label"]) for r in hold_own])
        a_true = auc([score(m, transform(r)) for r in hold_true], [int(r["label"]) for r in hold_true])
        n = 1 + self.db.rows("SELECT COUNT(*) AS n FROM fs_models")[0]["n"]
        mid = f"m{int(time.time() * 1000)}"
        row = {"id": mid, "created": now, "name": f"{'leaky' if leaky else 'asof'}-v{n}", "join_kind": "latest (leaks)" if leaky else "asof",
               "features": json.dumps(FEATURES), "weights": json.dumps(m), "train_rows": len(rows), "auc_train": a_train,
               "auc_holdout": a_hold, "auc_true": a_true, "build_ms": build_ms, "train_ms": train_ms, "sql": sql, "deployed": 0}
        self.db.write([{"op": "upsert", "table": "fs_models", "doc": row}])
        return self.model_view(row)

    WF_SPINE_COLS = ("usr", "item", "category", "uc", "price", "pos")

    def wf_training_sql(self, spec, leaky):
        """The training set of a model over workflows: each impression (the
        spine) and the chosen features of each workflow it joins, as of its
        time (or, leaky, as they are now)."""
        if spec.get("spine", "fs_impressions") != "fs_impressions":
            raise ValueError("the shop trains on fs_impressions")
        feats = spec["features"]
        cols = ["s.id AS id", "s.ts AS ts", "s.clicked AS label"]
        joins = []
        for i, j in enumerate(spec["joins"]):
            w = j["workflow"]
            if w not in self.wf.specs:
                raise ValueError(f"no workflow {w}")
            if j["on"] not in self.WF_SPINE_COLS:
                raise ValueError(f"join on one of {', '.join(self.WF_SPINE_COLS)}")
            names = {f["name"] for f in self.wf.specs[w]["features"]}
            use = [f.split(".", 1)[1] for f in feats if f.split(".", 1)[0] == w]
            for c in use:
                if c not in names:
                    raise ValueError(f"{w} has no feature {c}")
                cols.append(f"j{i}.{c} AS {w}__{c}")
            if leaky:
                joins.append(f"LEFT JOIN {wfm.target(w)} j{i} ON s.{j['on']} = j{i}.id")
            else:
                joins.append(f"ASOF JOIN {wfm.target(w)}_hist j{i} MATCH_CONDITION (s.ts > j{i}.ts) ON s.{j['on']} = j{i}.entity")
        for f in feats:
            w, c = f.split(".", 1)
            if w == "shop":
                if c not in ("price", "pos"):
                    raise ValueError("the impression's own features: price, pos")
                cols.append(f"s.{c} AS shop__{c}")
        return "SELECT " + ",\n  ".join(cols) + "\nFROM fs_impressions s\n" + "\n".join(joins) + "\nWHERE s.ts >= :since AND s.ts < :until"

    def train_workflows(self, spec, leaky=False):
        feats = spec.get("features") or []
        if not feats:
            raise ValueError("pick at least one feature")
        sql = self.wf_training_sql(spec, leaky)
        true_sql = self.wf_training_sql(spec, False)
        now = int(time.time() * 1000)
        cut = now - 60_000
        # Only impressions every joined workflow has history for, of the
        # last TRAIN_MIN minutes.
        since = now - TRAIN_MIN * 60_000
        for j in spec["joins"]:
            r = self.db.rows(f"SELECT MIN(ts) AS t FROM {wfm.target(j['workflow'])}_hist")[0]
            since = max(since, r["t"] or now)
        vec = lambda r: [float(r.get(f.replace(".", "__"))) if isinstance(r.get(f.replace(".", "__")), (int, float)) else 0.0 for f in feats]
        t0 = time.perf_counter()
        rows = self.db.rows(sql, {"since": since, "until": cut})
        build_ms = (time.perf_counter() - t0) * 1000
        if len(rows) < 200:
            raise ValueError(f"only {len(rows)} impressions to train on")
        X, y = [vec(r) for r in rows], [int(r["label"]) for r in rows]
        t0 = time.perf_counter()
        m = train_lr(X, y)
        train_ms = (time.perf_counter() - t0) * 1000
        hold_own = self.db.rows(sql, {"since": cut, "until": now})
        hold_true = self.db.rows(true_sql, {"since": cut, "until": now})
        n = 1 + self.db.rows("SELECT COUNT(*) AS n FROM fs_models")[0]["n"]
        row = {"id": f"m{now}", "created": now, "name": f"{'wf-leaky' if leaky else 'wf'}-v{n}", "join_kind": "latest (leaks)" if leaky else "asof",
               "features": json.dumps(feats), "weights": json.dumps(m), "train_rows": len(rows),
               "auc_train": auc([score(m, x) for x in X[:20000]], y[:20000]),
               "auc_holdout": auc([score(m, vec(r)) for r in hold_own], [int(r["label"]) for r in hold_own]),
               "auc_true": auc([score(m, vec(r)) for r in hold_true], [int(r["label"]) for r in hold_true]),
               "build_ms": build_ms, "train_ms": train_ms, "sql": sql, "deployed": 0,
               "spec": json.dumps({"joins": spec["joins"], "features": feats})}
        self.db.write([{"op": "upsert", "table": "fs_models", "doc": row}])
        return self.model_view(row)

    # -- the workflows' map

    def graph(self):
        now = int(time.time() * 1000)
        specs = dict(self.wf.specs)
        nodes, edges = [], []
        tables = set()
        for n, s in specs.items():
            for i, inp in enumerate(s["inputs"]):
                if inp.get("table"):
                    tables.add(inp["table"])
                    edges.append({"from": "t:" + inp["table"], "to": "w:" + n, "primary": i == 0, "on": inp.get("on")})
                else:
                    edges.append({"from": "w:" + inp["workflow"], "to": "w:" + n, "primary": i == 0, "on": inp.get("on")})
        counts = {}
        for t in tables:
            try:
                counts[t] = self.db.rows(f"SELECT COUNT(*) AS n FROM {t}")[0]["n"]
            except ChloeError:
                counts[t] = None
        for t in sorted(tables):
            nodes.append({"id": "t:" + t, "kind": "table", "name": t, "rows": counts[t]})
        stats = {}
        for r in self.db.rows("SELECT wf, feature, rows, null_share, min, max, mean, distinct_values, zeros, ts, run_ms FROM fs_workflow_stats LIMIT 10000"):
            stats.setdefault(r["wf"], {})[r["feature"]] = r
        for n, s in specs.items():
            runs = self.wf.runs.get(n, [])
            live = [r for r in runs if not r["backfill"]]
            last = live[-1] if live else (runs[-1] if runs else None)
            nodes.append({"id": "w:" + n, "kind": "workflow", "name": n, "title": s.get("title", ""), "target": wfm.target(n), "every": s["every"],
                          "mode": s["mode"], "features": [f["name"] for f in s["features"]], "last": last, "age_ms": now - self.wf.last.get(n, 0) if self.wf.last.get(n) else None,
                          "runs": [{"t": r["t"], "ms": r["ms"], "rows": r["rows"], "changed": r["changed"]} for r in live[-30:]], "stats": stats.get(n, {}),
                          "errors": sum(1 for r in runs if r["error"])})
        for r in self.db.rows("SELECT id, name, features, deployed, auc_true, spec FROM fs_models ORDER BY created DESC LIMIT 50"):
            if not r.get("spec"):
                continue
            used = sorted({f.split(".", 1)[0] for f in json.loads(r["features"])} - {"shop"})
            nodes.append({"id": "m:" + r["id"], "kind": "model", "name": r["name"], "deployed": bool(r["deployed"]), "auc": r["auc_true"]})
            for w in used:
                edges.append({"from": "w:" + w, "to": "m:" + r["id"], "primary": True})
        return {"nodes": nodes, "edges": edges, "status": self.wf_status, "now": now}

    def load_model(self, mid):
        r = self.db.get("fs_models", [mid])[0]
        if not r:
            raise KeyError(mid)
        r["m"] = json.loads(r["weights"])
        return r

    def deploy(self, mid):
        m = self.load_model(mid) if mid != "random" else None
        if m and m.get("spec"):
            gone = [j["workflow"] for j in json.loads(m["spec"]).get("joins", []) if j["workflow"] not in self.wf.specs]
            if gone:
                raise ValueError(f"{m.get('name') or mid} reads {', '.join(gone)}, which no longer exist{'s' if len(gone) == 1 else ''}: train a model on the workflows there are")
        ops = [{"op": "set", "table": "fs_models", "key": r["id"], "field": "deployed", "value": 1 if r["id"] == mid else 0}
               for r in self.db.rows("SELECT id FROM fs_models")]
        if ops:
            self.db.write(ops)
        self.model = m
        return {"deployed": mid}

    @staticmethod
    def model_view(r):
        r = dict(r)
        w = json.loads(r.pop("weights"))
        r["weights"] = dict(zip(json.loads(r["features"]), w["w"]))
        r.pop("m", None)
        return r

    # -- views

    def state(self):
        now = int(time.time() * 1000)
        recent = [x for x in self.serve_ms.all() if x[0] > now - 60_000]
        writes = [x for x in self.write_ms.all() if x[0] > now - 60_000]
        fresh = [x[1] for x in self.fresh_ms.all() if x[0] > now - 60_000]
        rates = self.rates.all()
        last = rates[-1] if rates else (now, 0, 0, 0)
        jobs = self.jobs.all()
        last_job = {}
        for j in jobs:
            last_job[j["job"]] = j
        return {
            "now": now, "phase": self.phase, "running": self.running, "rate": self.rate, "error": self.error,
            "counts": self.counts, "model": self.model and self.model_view(self.model),
            "serve": {"p50": pct([x[1] for x in recent], 0.5), "p99": pct([x[1] for x in recent], 0.99),
                      "fetch_p50": pct([x[2] for x in recent], 0.5), "n": len(recent),
                      # Rows read by key by the last request (0: the random order reads none).
                      "rows": recent[-1][5] if recent else None},
            "write": {"p50": pct([x[1] for x in writes], 0.5), "p99": pct([x[1] for x in writes], 0.99)},
            "fresh": {"p50": pct(fresh, 0.5), "p99": pct(fresh, 0.99)},
            "rates": {"impressions": last[1], "clicks": last[2], "requests": last[3]},
            "jobs": last_job,
            "series": self.series(now),
        }

    def series(self, now, window=300_000, step=5000):
        """5-second points over the last 5 minutes: serving latency, write
        latency, freshness, and the click rate of each model."""
        t0 = now - window
        bins = {}
        for t, total, fetch, *_ in self.serve_ms.all():
            if t >= t0:
                bins.setdefault(t // step, {}).setdefault("serve", []).append(total)
        for t, ms, _ in self.write_ms.all():
            if t >= t0:
                bins.setdefault(t // step, {}).setdefault("write", []).append(ms)
        for t, ms in self.fresh_ms.all():
            if t >= t0:
                bins.setdefault(t // step, {}).setdefault("fresh", []).append(ms)
        out = []
        for b in sorted(bins):
            v = bins[b]
            out.append({"t": b * step, "serve_p50": pct(v.get("serve"), 0.5), "serve_p99": pct(v.get("serve"), 0.99),
                        "write_p50": pct(v.get("write"), 0.5), "fresh_p50": pct(v.get("fresh"), 0.5)})
        return out

    def ctr_series(self, minutes=20):
        """The click rate a request of each model got, by 10-second bucket, from
        the impressions table."""
        since = int(time.time() * 1000) - minutes * 60_000
        rows = self.db.rows("SELECT bucket, model, COUNT(*) AS shown, SUM(clicked) AS clicked FROM fs_impressions WHERE ts >= :since GROUP BY bucket, model", {"since": since})
        names = {r["id"]: r["name"] for r in self.db.rows("SELECT id, name FROM fs_models")}
        base = self.db.rows("SELECT COUNT(*) AS shown, SUM(clicked) AS clicked FROM fs_impressions WHERE model = 'random'")[0]
        return {"random_ctr": (base["clicked"] or 0) / base["shown"] if base["shown"] else None, "rows": [{"t": int(r["bucket"]) * 10_000, "model": r["model"], "name": names.get(r["model"], r["model"]), "shown": r["shown"], "clicked": r["clicked"]} for r in rows]}

    def features(self):
        """Each feature's definition and, over the current rows, its spread."""
        t0 = time.perf_counter()
        users = self.db.rows("SELECT id, views, clicks, click_price FROM fs_user_online")
        items = self.db.rows("SELECT o.id AS id, o.views AS views, o.clicks AS clicks, t.views_2m AS views_2m, t.clicks_2m AS clicks_2m, it.price AS price, it.category AS category FROM fs_item_online o LEFT JOIN fs_item_trend t ON o.id = t.id LEFT JOIN fs_items it ON o.id = it.id")
        ucs = self.db.rows("SELECT id, views, clicks FROM fs_usercat_online")
        read_ms = (time.perf_counter() - t0) * 1000
        u_by = {u["id"]: u for u in users}
        # A sample of (user, item) pairs, as serving would see them.
        r = random.Random(5)
        uc_by = {c["id"]: c for c in ucs}
        sample = []
        for _ in range(3000):
            u = r.choice(users) if users else {}
            it = r.choice(items) if items else {}
            c = uc_by.get(f"{u.get('id')}|{it.get('category')}", {})
            sample.append({"u_views": u.get("views"), "u_clicks": u.get("clicks"), "u_click_price": u.get("click_price"),
                           "i_views": it.get("views"), "i_clicks": it.get("clicks"), "views_2m": it.get("views_2m"), "clicks_2m": it.get("clicks_2m"),
                           "uc_views": c.get("views"), "uc_clicks": c.get("clicks"), "price": it.get("price")})
        out = []
        clipped = sum(1 for s in sample if s["u_clicks"] and s["price"] and abs(math.log(s["price"] / max(0.01, s["u_click_price"] / s["u_clicks"]))) > 2)
        missing = sum(1 for s in sample if not s["u_clicks"])
        for j, (name, kind, formula, what, f) in enumerate(TRANSFORMS):
            vals = [f(s) for s in sample]
            srt = sorted(vals)
            lo, hi = srt[int(0.01 * len(srt))], srt[int(0.99 * len(srt)) - 1]
            nb = 24
            hist = [0] * nb
            for v in vals:
                hist[max(0, min(nb - 1, int((v - lo) / (hi - lo) * nb))) if hi > lo else 0] += 1
            w = self.model and self.model["m"]["w"][j]
            out.append({"name": name, "kind": kind, "formula": formula, "what": what, "lo": lo, "hi": hi, "mean": sum(vals) / len(vals),
                        "hist": hist, "weight": w,
                        "cleaning": {"missing": missing, "clipped": clipped, "n": len(sample)} if name == "price_ratio" else None})
        jobs = {j["job"]: j for j in self.jobs.all()}
        sources = [
            {"table": "fs_user_online", "kind": "streaming", "key": "shopper", "rows": len(users), "columns": ["views", "clicks", "click_price"], "how": "added to in the impressions' own transaction (`add` ops)"},
            {"table": "fs_item_online", "kind": "streaming", "key": "product", "rows": len(items), "columns": ["views", "clicks"], "how": "added to in the impressions' own transaction"},
            {"table": "fs_usercat_online", "kind": "streaming", "key": "shopper|category", "rows": len(ucs), "columns": ["views", "clicks"], "how": "added to in the impressions' own transaction"},
            {"table": "fs_item_trend", "kind": "batch", "key": "product", "rows": len(items), "columns": ["views_2m", "clicks_2m"], "how": "every 10 s: " + " ".join(TREND_SQL.split()), "last": jobs.get("trend")},
        ]
        return {"features": out, "sources": sources, "read_ms": read_ms, "sample": len(sample)}

    def browse(self, table, where, order, limit=50):
        allowed = {"fs_user_online", "fs_item_online", "fs_usercat_online", "fs_item_trend", "fs_user_snap", "fs_item_snap", "fs_usercat_snap", "fs_impressions", "fs_items", "fs_models"}
        if table not in allowed:
            raise ValueError(f"not a table of this app: {table}")
        q = f"SELECT * FROM {table}"
        if where.strip():
            q += f" WHERE {where}"
        if order.strip():
            q += f" ORDER BY {order}"
        q += f" LIMIT {int(limit)}"
        t0 = time.perf_counter()
        res = self.db.query(q)
        n = self.db.rows(f"SELECT COUNT(*) AS n FROM {table}" + (f" WHERE {where}" if where.strip() else ""))[0]["n"]
        return {"sql": q, "columns": res["columns"], "rows": res["rows"], "count": n, "ms": (time.perf_counter() - t0) * 1000}

    def timeline(self, uid, minutes=6):
        """One shopper: impressions with their labels, the snapshots of their
        features, and which snapshot the ASOF JOIN gave each impression."""
        now = int(time.time() * 1000)
        since = now - minutes * 60_000
        snaps = self.db.rows("SELECT ts, views, clicks FROM fs_user_snap WHERE usr = :u AND ts >= :s ORDER BY ts LIMIT 2000", {"u": uid, "s": since - 30_000})
        sql = ("SELECT s.id AS id, s.ts AS ts, s.clicked AS label, s.item AS item, u.ts AS u_ts, u.clicks AS u_clicks, u.views AS u_views, o.clicks AS latest_clicks"
               " FROM fs_impressions s ASOF JOIN fs_user_snap u MATCH_CONDITION (s.ts > u.ts) ON s.usr = u.usr"
               " LEFT JOIN fs_user_online o ON s.usr = o.id WHERE s.usr = :u AND s.ts >= :s ORDER BY s.ts LIMIT 400")
        t0 = time.perf_counter()
        imps = self.db.rows(sql, {"u": uid, "s": since})
        ms = (time.perf_counter() - t0) * 1000
        return {"user": uid, "since": since, "now": now, "snaps": snaps, "impressions": imps, "sql": sql, "ms": ms}

    def bench_serve(self, clients=4, seconds=4):
        res = []
        stop = time.time() + seconds
        model = self.model
        if model is None:
            raise ValueError("deploy a model first")

        def worker():
            db = Chloe(f"http://{self.db.host}:{self.db.port}", self.db.key)
            me = App.__new__(App)
            me.db, me.world = db, self.world
            while time.time() < stop:
                u = random.choice(self.world.users)["id"]
                r = App.rank(me, u, model)
                res.append(r["ms"]["total"])

        ts = [threading.Thread(target=worker) for _ in range(clients)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        return {"clients": clients, "seconds": seconds, "requests": len(res), "per_s": len(res) / seconds, "p50": pct(res, 0.5), "p99": pct(res, 0.99),
                "rows_per_request": 1 + 2 * len(self.world.items) + len(self.world.cats)}

    def bench_train(self):
        now = int(time.time() * 1000)
        out = {}
        for name, leaky in (("asof", False), ("latest", True)):
            ts = []
            for _ in range(3):
                rows, ms, _ = self.training_set(leaky, 0, now)
                ts.append(ms)
            out[name] = {"rows": len(rows), "ms": sorted(ts)[1]}
        return out

    def user(self, uid):
        u = self.world.user_by_id[uid]
        return {"id": uid, "name": u["name"], "likes": u["likes"], "price_sens": u["price_sens"]}

    def click(self, uid, ranked_ids, clicked_id, model):
        """A shopper in the shop page clicks a product: the impressions of the
        8 it saw, the click among them, written as one transaction; then the
        features read back."""
        now = int(time.time() * 1000)
        self.seq += 1
        req = f"r{now}-{self.seq}"
        shown = [self.world.by_id[i] for i in ranked_ids]
        before = self.db.get("fs_user_online", [uid])[0]
        cat = self.world.by_id[clicked_id]["category"]
        before_uc = self.db.get("fs_usercat_online", [f"{uid}|{cat}"])[0]
        ops = self.impression_ops(now, req, uid, shown, model, [i == clicked_id for i in ranked_ids])
        t0 = time.perf_counter()
        self.db.write(ops)
        t1 = time.perf_counter()
        after = self.db.get("fs_user_online", [uid])[0]
        after_uc = self.db.get("fs_usercat_online", [f"{uid}|{cat}"])[0]
        t2 = time.perf_counter()
        return {"write_ms": (t1 - t0) * 1000, "read_ms": (t2 - t1) * 1000, "ops": len(ops), "before": before, "after": after,
                "before_uc": before_uc, "after_uc": after_uc, "category": cat}


def chunks(v, n):
    return [v[i:i + n] for i in range(0, len(v), n)]


# ----------------------------------------------------------------- HTTP app

ICON = '<path d="M4 6h16M4 12h10M4 18h6"/><circle cx="18" cy="15" r="3"/><path d="M20.2 17.2 22 19"/>'


def manifest():
    return {"name": "features", "title": "Feature store", "description": "A shop ranked by a model on live features: streaming and batch transforms, point-in-time training sets, models as rows, serving by key",
            "icon_svg": ICON, "view": "ui/view.js", "version": VERSION}


def handler(app, token):
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):
            pass

        def send(self, code, body, ctype="application/json"):
            data = body if isinstance(body, bytes) else json.dumps(body, default=str).encode()
            self.send_response(code)
            self.send_header("content-type", ctype)
            self.send_header("content-length", str(len(data)))
            self.send_header("cache-control", "no-cache")
            self.end_headers()
            self.wfile.write(data)

        def authed(self):
            if not token:
                return True
            got = self.headers.get("authorization", "")
            return secrets.compare_digest(got.encode(), ("Bearer " + token).encode())

        def body(self):
            n = int(self.headers.get("content-length") or 0)
            return json.loads(self.rfile.read(n)) if n else {}

        def do_GET(self):
            self.route("GET")

        def do_POST(self):
            self.route("POST")

        def route(self, method):
            u = urllib.parse.urlparse(self.path)
            q = {k: v[0] for k, v in urllib.parse.parse_qs(u.query).items()}
            p = u.path
            try:
                if p == "/app.json":
                    return self.send(200, manifest())
                if p == "/ui/view.js":
                    d = os.environ.get("CHLOEDB_FEATURES_UI_DIR") or os.path.join(HERE, "ui")
                    with open(os.path.join(d, "view.js"), "rb") as f:
                        return self.send(200, f.read(), "text/javascript; charset=utf-8")
                if not self.authed():
                    return self.send(401, {"error": "this app answers its server only"})
                b = self.body() if method == "POST" else {}
                if p == "/api/state":
                    return self.send(200, app.state())
                if p == "/api/ctr":
                    return self.send(200, app.ctr_series(int(q.get("minutes", 20))))
                if p == "/api/features":
                    return self.send(200, app.features())
                if p == "/api/browse":
                    return self.send(200, app.browse(b.get("table", ""), b.get("where", ""), b.get("order", ""), b.get("limit", 50)))
                if p == "/api/users":
                    return self.send(200, {"users": [app.user(u["id"]) for u in app.world.users], "categories": app.world.cats})
                if p == "/api/shop":
                    uid = q.get("user") or random.choice(app.world.users)["id"]
                    r = app.rank(uid, explain=True)
                    if r["model"] == "random":
                        r["items"] = [{**e, **{k: app.world.by_id[e["id"]][k] for k in ("title", "category", "price")}} for e in r["items"]]
                    r["user"] = app.user(uid)
                    r["model_name"] = app.model["name"] if app.model and r["model"] != "random" else "random"
                    r["online"] = app.db.get("fs_user_online", [uid])[0]
                    r["usercat"] = dict(zip(app.world.cats, app.db.get("fs_usercat_online", [f"{uid}|{c}" for c in app.world.cats], ["views", "clicks"])))
                    r["features"] = json.loads(app.model["features"]) if app.model else FEATURES
                    return self.send(200, r)
                if p == "/api/click":
                    return self.send(200, app.click(b["user"], b["shown"], b["item"], b.get("model", "random")))
                if p == "/api/timeline":
                    return self.send(200, app.timeline(q.get("user", "u000")))
                if p == "/api/models":
                    rows = app.db.rows("SELECT * FROM fs_models ORDER BY created DESC LIMIT 50")
                    return self.send(200, {"models": [App.model_view(r) for r in rows], "transforms": [{"name": t[0], "kind": t[1], "formula": t[2], "what": t[3]} for t in TRANSFORMS],
                                           "asof_sql": ASOF_SQL, "leaky_sql": LEAKY_SQL})
                if p == "/api/train":
                    return self.send(200, app.train(bool(b.get("leaky")), b.get("minutes")))
                if p == "/api/deploy":
                    return self.send(200, app.deploy(b["id"]))
                if p == "/api/sim":
                    if "running" in b:
                        app.running = bool(b["running"])
                    if "rate" in b:
                        app.rate = max(0.5, min(100.0, float(b["rate"])))
                    return self.send(200, {"running": app.running, "rate": app.rate})
                if p == "/api/bench/serve":
                    return self.send(200, app.bench_serve(int(b.get("clients", 4)), float(b.get("seconds", 4))))
                if p == "/api/bench/train":
                    return self.send(200, app.bench_train())
                if p == "/api/wf/graph":
                    return self.send(200, app.graph())
                if p == "/api/wf/spec":
                    n = q.get("name", "")
                    if n not in app.wf.specs:
                        raise KeyError(f"no workflow {n}")
                    sql, names = wfm.build_sql(app.wf.specs[n], app.wf.specs)
                    return self.send(200, {"spec": app.wf.specs[n], "sql": sql, "target": wfm.target(n), "runs": app.wf.runs.get(n, [])[-60:],
                                           "upstream": sorted(app.wf.upstream(n)), "downstream": sorted(app.wf.downstream(n))})
                if p == "/api/wf/preview":
                    return self.send(200, app.wf.preview(b["spec"]))
                if p == "/api/wf/save":
                    s = app.wf.save(b["spec"], int(b.get("backfill") or 0))
                    return self.send(200, {"saved": s["name"], "run": app.wf.runs.get(s["name"], [None])[-1]})
                if p == "/api/wf/delete":
                    app.wf.delete(b["name"])
                    return self.send(200, {"deleted": b["name"]})
                if p == "/api/wf/run":
                    return self.send(200, app.wf.run(b["name"], int(time.time() * 1000)))
                if p == "/api/wf/backfill":
                    app.wf.backfill(b["name"], int(b.get("minutes", 10)))
                    return self.send(200, {"backfilled": b["name"]})
                if p == "/api/wf/search":
                    return self.send(200, app.wf.search(b.get("where", ""), int(time.time() * 1000)))
                if p == "/api/wf/lookup":
                    n = b["name"]
                    t0 = time.perf_counter()
                    rows = app.db.get(wfm.target(n), [str(k) for k in b["keys"]][:100])
                    return self.send(200, {"rows": rows, "ms": (time.perf_counter() - t0) * 1000, "table": wfm.target(n)})
                if p == "/api/wf/tables":
                    ts = app.db.call("GET", "/v1/tables")["tables"]
                    out = [{"table": t["schema"]["table"], "columns": [{"name": f["name"], "type": f["type"]} for f in t["short"]["fields"]]} for t in ts]
                    return self.send(200, {"tables": out, "workflows": {n: {"target": wfm.target(n), "features": [f["name"] for f in s["features"]]} for n, s in app.wf.specs.items()},
                                           "windows": [w for w in wfm.WINDOWS if w], "fns": list(wfm.FNS)})
                if p == "/api/wf/train":
                    return self.send(200, app.train_workflows(b, bool(b.get("leaky"))))
                if p == "/api/wf/training_sql":
                    return self.send(200, {"sql": app.wf_training_sql(b, bool(b.get("leaky")))})
                if p == "/api/measured":
                    return self.send(200, measured())
                return self.send(404, {"error": f"no {p}"})
            except ChloeError as e:
                return self.send(502, {"error": str(e)})
            except (KeyError, ValueError, wfm.SpecError) as e:
                return self.send(400, {"error": str(e)})
            except Exception as e:  # noqa: BLE001
                return self.send(500, {"error": f"{type(e).__name__}: {e}"})

    return H


def measured():
    """The benchmark against Feast, as committed (chloedb/results/feature-store)."""
    d = os.path.join(HERE, "..", "..", "chloedb", "results", "feature-store")
    out = {"source": "chloedb/results/feature-store (Intel Xeon @ 2.10GHz, 4 cores, cloud container, 2026-11-13)"}
    for name in ("offline", "serve", "online-redis"):
        try:
            with open(os.path.join(d, name + ".json")) as f:
                out[name] = json.load(f)
        except OSError:
            out[name] = None
    return out


# --------------------------------------------------------------------- start


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def find_server(given):
    if given:
        return given
    if os.environ.get("CHLOEDB_SERVER_BIN"):
        return os.environ["CHLOEDB_SERVER_BIN"]
    root = os.path.join(HERE, "..", "..", "chloedb-server", "target")
    for prof in ("release", "ci", "debug"):
        p = os.path.join(root, prof, "chloedb-server")
        if os.path.exists(p):
            return p
    p = shutil.which("chloedb-server")
    if p:
        return p
    sys.exit("no chloedb-server: build it (cd chloedb-server && cargo build --release), or pass --server-bin")


def wait_up(url, secs=30):
    end = time.time() + secs
    while time.time() < end:
        try:
            Chloe(url).call("GET", "/v1/health", timeout=2)
            return
        except Exception:
            time.sleep(0.2)
    sys.exit(f"chloedb-server at {url} did not answer")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("serve", help="run the demo")
    s.add_argument("--local", help="a folder: start chloedb-server on a database in it, with this app")
    s.add_argument("--server", help="a running chloedb-server (start it with --app features=http://127.0.0.1:PORT)")
    s.add_argument("--key", help="the server's admin key (or CHLOEDB_KEY)")
    s.add_argument("--listen", default="127.0.0.1:7810", help="this app's address (default 127.0.0.1:7810)")
    s.add_argument("--port", type=int, default=7700, help="with --local, the server's port (default 7700)")
    s.add_argument("--server-bin", help="with --local, the chloedb-server program")
    s.add_argument("--rate", type=float, default=12, help="shopper sessions a second (default 12)")
    s.add_argument("--keep", type=int, default=KEEP_MIN, help=f"minutes of impressions kept (default {KEEP_MIN}; 0 keeps them all); a model trains on the last {TRAIN_MIN}")
    s.add_argument("--backfill", type=int, default=12, help="minutes of history to make on a new database (default 12)")
    a = ap.parse_args(argv)

    host, port = a.listen.rsplit(":", 1)
    app_url = f"http://{host}:{port}"
    child = None
    token = os.environ.get("CHLOEDB_APP_TOKEN_FEATURES") or os.environ.get("CHLOEDB_APP_TOKEN")
    if a.local:
        os.makedirs(a.local, exist_ok=True)
        token = secrets.token_urlsafe(24)
        # The demo alone: the server starts none of the apps it would start
        # beside it otherwise (chloectx, with its model download), as the
        # crawler's and the sheets' --local do.
        env = dict(os.environ, CHLOEDB_APP_TOKEN_FEATURES=token, CHLOEDB_START_APPS="off")
        env.pop("CHLOEDB_APPS", None)
        url = f"http://127.0.0.1:{a.port}"
        cmd = [find_server(a.server_bin), "--db", os.path.join(a.local, "features.chloedb"), "--listen", f"127.0.0.1:{a.port}",
               "--grpc", f"127.0.0.1:{free_port()}", "--max-result-window", "0", "--app", f"features={app_url}"]
        log = open(os.path.join(a.local, "chloedb-server.log"), "ab")
        child = subprocess.Popen(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)
        wait_up(url)
    elif a.server:
        url = a.server.rstrip("/")
    else:
        ap.error("serve needs --local DIR or --server URL")
    db = Chloe(url, a.key or os.environ.get("CHLOEDB_KEY"))
    app = App(db, rate=a.rate, backfill_min=a.backfill, keep_min=a.keep)
    srv = ThreadingHTTPServer((host, int(port)), handler(app, token))
    srv.daemon_threads = True
    threading.Thread(target=srv.serve_forever, daemon=True).start()

    def bye(*_):
        app.stop.set()
        if child:
            child.terminate()
        os._exit(0)

    signal.signal(signal.SIGTERM, bye)
    signal.signal(signal.SIGINT, bye)
    print(f"feature store demo: {app_url} (app), {url}/#/app/features (command center)", flush=True)
    t0 = time.time()
    app.setup()
    print(f"ready in {time.time() - t0:.1f}s: {app.counts['impressions']} impressions", flush=True)
    threading.Thread(target=app.seed_workflows, daemon=True).start()
    threading.Thread(target=app.jobs_loop, daemon=True).start()
    for i in range(SHOPPER_THREADS):
        threading.Thread(target=app.shoppers, args=(i == 0,), daemon=True).start()
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    main()
