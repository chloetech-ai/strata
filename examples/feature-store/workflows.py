"""Feature workflows: each reads one or more tables (or other workflows'
output), groups by an entity key, aggregates over time windows, transforms,
and writes a table of features by key (served by key) and its history (joined
into training sets as of each event). Definitions are rows of `fs_workflows`;
each run's statistics, per feature, are rows of `fs_workflow_stats`, which the
map searches with SQL.

A definition (JSON):

    {"name": "user_activity", "title": "...",
     "inputs": [{"table": "fs_impressions", "alias": "a"},
                {"table": "fs_items", "alias": "p", "on": ["a.item", "id"]},     # LEFT JOIN p ON a.item = p.id
                {"workflow": "item_popularity", "alias": "i", "on": ["a.item", "entity"]}],
     "mode": "aggregate",            # or "map": a row per input row, no GROUP BY
     "key": "a.usr",                 # the entity: any expression over the inputs
     "time": "a.ts",                 # epoch milliseconds; windows and point in time
     "where": "",
     "features": [{"name": "clicks_5m", "fn": "sum", "arg": "a.clicked", "window": 300},
                  {"name": "ctr", "expr": "(clicks + 1.0) / (views + 10)"}],
     "every": 10}                    # seconds

A workflow's output is `fv_<name>` (id = entity, ts, the features) and
`fv_<name>_hist` (entity, ts, the features, a row each time they change). An
input naming a workflow reads its history as of the run's time, so a chain
backfilled into the past never sees the future.
"""

import json
import re
import threading
import time

FNS = ("count", "sum", "avg", "min", "max", "count_distinct")
IDENT = re.compile(r"^[a-z_][a-z0-9_]{0,40}$")
WINDOWS = (None, 60, 120, 300, 900, 3600, 86400)

TABLES = [
    "CREATE TABLE IF NOT EXISTS fs_workflows (id KEYWORD PRIMARY KEY, spec TEXT, created INT, updated INT, enabled INT DEFAULT 1)",
    "CREATE TABLE IF NOT EXISTS fs_workflow_runs (id KEYWORD PRIMARY KEY, wf KEYWORD, ts INT, ms FLOAT, query_ms FLOAT, rows INT, changed INT, backfill INT, error TEXT)",
    "CREATE TABLE IF NOT EXISTS fs_workflow_stats (id KEYWORD PRIMARY KEY, wf KEYWORD, feature KEYWORD, ts INT, rows INT, nulls INT, null_share FLOAT,"
    " min FLOAT, max FLOAT, mean FLOAT, distinct_values INT, zeros INT, run_ms FLOAT)",
]


class SpecError(ValueError):
    pass


def target(name):
    return f"fv_{name}"


def check(spec, names_known):
    """Refuses a definition that cannot run; returns it normalized."""
    s = dict(spec)
    if not IDENT.match(s.get("name", "")):
        raise SpecError("name: lower-case letters, digits and _, starting with a letter")
    ins = s.get("inputs") or []
    if not ins:
        raise SpecError("a workflow reads at least one input")
    aliases = set()
    for i, inp in enumerate(ins):
        a = inp.get("alias", "")
        if not IDENT.match(a) or a in aliases:
            raise SpecError(f"input {i + 1}: an alias of its own (a, b, ...)")
        aliases.add(a)
        if bool(inp.get("table")) == bool(inp.get("workflow")):
            raise SpecError(f"input {a}: a table or a workflow")
        if inp.get("workflow") and inp["workflow"] not in names_known:
            raise SpecError(f"input {a}: no workflow {inp['workflow']}")
        if i and not (isinstance(inp.get("on"), list) and len(inp["on"]) == 2 and all(inp["on"])):
            raise SpecError(f"input {a}: joined on [expression over the inputs before it, its own column]")
    if s.get("mode", "aggregate") not in ("aggregate", "map"):
        raise SpecError("mode: aggregate or map")
    s["mode"] = s.get("mode", "aggregate")
    if not s.get("key"):
        raise SpecError("key: the entity each row is about (a.usr, a.item, ...)")
    feats = s.get("features") or []
    if not feats:
        raise SpecError("at least one feature")
    seen = set()
    for f in feats:
        n = f.get("name", "")
        if not IDENT.match(n) or n in seen or n in ("id", "ts", "entity"):
            raise SpecError(f"feature {n!r}: a name of its own (letters, digits, _)")
        seen.add(n)
        if f.get("expr"):
            continue
        if s["mode"] == "map":
            raise SpecError(f"feature {n}: a workflow row by row takes expressions only")
        if f.get("fn") not in FNS:
            raise SpecError(f"feature {n}: fn is one of {', '.join(FNS)}")
        if f.get("window") and not s.get("time"):
            raise SpecError(f"feature {n}: a window needs the workflow's time column")
    s["every"] = max(2, int(s.get("every") or 10))
    return s


def deps(spec):
    return [i["workflow"] for i in spec["inputs"] if i.get("workflow")]


def topo(specs):
    """The workflows in an order that runs inputs first; refuses a cycle."""
    out, state = [], {}

    def visit(n, path):
        if state.get(n) == 2:
            return
        if state.get(n) == 1:
            raise SpecError("a cycle: " + " -> ".join(path + [n]))
        state[n] = 1
        for d in deps(specs[n]):
            if d in specs:
                visit(d, path + [n])
        state[n] = 2
        out.append(n)

    for n in sorted(specs):
        visit(n, [])
    return out


def build_sql(spec, specs):
    """The SQL of a run, with :now and one :since_<s> a window."""
    ctes, params = [], {"now"}
    src = []
    for inp in spec["inputs"]:
        if inp.get("workflow"):
            up = specs[inp["workflow"]]
            cols = ", ".join(["entity", "ts"] + [f["name"] for f in up["features"]])
            cte = f"{inp['alias']}_src"
            ctes.append(f"{cte} AS (SELECT DISTINCT ON (entity) {cols} FROM {target(up['name'])}_hist WHERE ts <= :now ORDER BY entity, ts DESC)")
            src.append((cte, inp))
        else:
            src.append((inp["table"], inp))
    frm = f"{src[0][0]} {src[0][1]['alias']}"
    for name, inp in src[1:]:
        frm += f"\n  LEFT JOIN {name} {inp['alias']} ON {inp['on'][0]} = {inp['alias']}.{inp['on'][1]}"
    conds = []
    t = spec.get("time")
    if t:
        conds.append(f"{t} <= :now")
    if spec.get("where"):
        conds.append(f"({spec['where']})")
    aggs = [f for f in spec["features"] if not f.get("expr")]
    exprs = [f for f in spec["features"] if f.get("expr")]
    if spec["mode"] == "aggregate":
        if t and aggs and all(f.get("window") for f in aggs):
            w = max(f["window"] for f in aggs)
            conds.append(f"{t} > :since_{w}")
            params.add(f"since_{w}")
        items = [f"{spec['key']} AS entity"]
        for f in aggs:
            arg = f.get("arg") or ("1" if f["fn"] == "count" else None)
            if not arg:
                raise SpecError(f"feature {f['name']}: {f['fn']} of what?")
            cond = []
            if f.get("window"):
                cond.append(f"{t} > :since_{f['window']}")
                params.add(f"since_{f['window']}")
            if f.get("where"):
                cond.append(f"({f['where']})")
            if cond:
                arg = f"CASE WHEN {' AND '.join(cond)} THEN {arg} ELSE NULL END"
            fn = f["fn"]
            items.append(f"COUNT(DISTINCT {arg}) AS {f['name']}" if fn == "count_distinct" else f"{fn.upper()}({arg}) AS {f['name']}")
        inner = "SELECT " + ",\n    ".join(items) + f"\n  FROM {frm}"
        if conds:
            inner += "\n  WHERE " + " AND ".join(conds)
        inner += f"\n  GROUP BY {spec['key']}"
        ctes.append(f"w AS ({inner})")
        outer = ["entity"] + [f["name"] for f in aggs] + [f"{f['expr']} AS {f['name']}" for f in exprs]
        sql = "WITH " + ",\n".join(ctes) + "\nSELECT " + ", ".join(outer) + " FROM w"
    else:
        items = [f"{spec['key']} AS entity"] + [f"{f['expr']} AS {f['name']}" for f in exprs]
        sql = ("WITH " + ",\n".join(ctes) + "\n" if ctes else "") + "SELECT " + ",\n  ".join(items) + f"\nFROM {frm}"
        if conds:
            sql += "\nWHERE " + " AND ".join(conds)
    return sql, sorted(params)


def bind(names, now):
    p = {"now": now}
    for n in names:
        if n.startswith("since_"):
            p[n] = now - int(n[6:]) * 1000
    return p


def stats_of(rows, feats):
    out = {}
    for f in feats:
        vals = [r.get(f) for r in rows]
        nums = [v for v in vals if isinstance(v, (int, float)) and not isinstance(v, bool)]
        nulls = sum(1 for v in vals if v is None)
        out[f] = {"rows": len(vals), "nulls": nulls, "null_share": nulls / len(vals) if vals else 0.0,
                  "min": min(nums) if nums else None, "max": max(nums) if nums else None,
                  "mean": sum(nums) / len(nums) if nums else None, "distinct_values": len(set(vals)), "zeros": sum(1 for v in nums if v == 0)}
    return out


class Runner:
    """Loads the definitions, runs each when due (inputs first), and keeps
    the output tables, their history and the statistics."""

    def __init__(self, db):
        self.db = db
        self.lock = threading.RLock()
        self.job = threading.RLock()  # one run at a time
        self.specs = {}
        self.last = {}        # name -> last run time (ms)
        self.prev = {}        # name -> {entity: tuple(values)}, to write history only on change
        self.ready = set()    # output tables made
        self.runs = {}        # name -> list of recent runs
        # Readers of workflows' tables other than workflows (the deployed
        # model): each a function of a workflow's name to the names of
        # what reads it, which a delete refuses while there are any.
        self.readers = []

    def setup(self):
        for t in TABLES:
            self.db.sql(t)
        self.load()

    def load(self):
        with self.lock:
            self.specs = {r["id"]: json.loads(r["spec"]) for r in self.db.rows("SELECT id, spec FROM fs_workflows LIMIT 10000")}
            for r in self.db.rows("SELECT wf, MAX(ts) AS t FROM fs_workflow_runs GROUP BY wf"):
                self.last.setdefault(r["wf"], r["t"] or 0)

    def save(self, spec, backfill_min=0):
        with self.lock:
            s = check(spec, set(self.specs) | {spec.get("name")})
            specs = dict(self.specs)
            old = specs.get(s["name"])
            specs[s["name"]] = s
            topo(specs)
            build_sql(s, specs)  # refuses what cannot be built
            now = int(time.time() * 1000)
            self.db.write([{"op": "upsert", "table": "fs_workflows", "doc": {"id": s["name"], "spec": json.dumps(s), "created": (old or {}).get("created", now), "updated": now, "enabled": 1}}])
            if old and [f["name"] for f in old["features"]] != [f["name"] for f in s["features"]]:
                self.prev.pop(s["name"], None)
            self.specs = specs
            self.ready.discard(s["name"])
        if backfill_min:
            self.backfill(s["name"], backfill_min)
        else:
            self.run(s["name"], int(time.time() * 1000))
        return s

    def delete(self, name):
        with self.lock:
            users = [n for n, s in self.specs.items() if name in deps(s)]
            users += [u for f in self.readers for u in f(name)]
            if users:
                raise SpecError(f"{', '.join(users)} read {name}: delete them first")
            self.db.write([{"op": "delete", "table": "fs_workflows", "key": name}])
            for t in (target(name), target(name) + "_hist"):
                try:
                    self.db.sql(f"DROP TABLE IF EXISTS {t}")
                except Exception:
                    pass
            ids = [r["id"] for r in self.db.rows("SELECT id FROM fs_workflow_stats WHERE wf = :w", {"w": name})]
            if ids:
                self.db.write([{"op": "delete", "table": "fs_workflow_stats", "key": i} for i in ids])
            self.specs.pop(name, None)
            self.ready.discard(name)
            self.prev.pop(name, None)

    def preview(self, spec, limit=20):
        s = check(spec, set(self.specs) | {spec.get("name")})
        specs = dict(self.specs)
        specs[s["name"]] = s
        topo(specs)
        sql, names = build_sql(s, specs)
        now = int(time.time() * 1000)
        t0 = time.perf_counter()
        res = self.db.query(sql, bind(names, now))
        ms = (time.perf_counter() - t0) * 1000
        rows = res["rows"]
        feats = [f["name"] for f in s["features"]]
        return {"sql": sql, "params": bind(names, now), "rows": rows[:limit], "count": len(rows), "ms": ms, "columns": res["columns"], "stats": stats_of(rows, feats)}

    def _ensure(self, s, rows):
        name = s["name"]
        if name in self.ready:
            return
        feats = [f["name"] for f in s["features"]]

        def ty(f):
            for r in rows:
                v = r.get(f)
                if isinstance(v, str):
                    return "KEYWORD"
                if v is not None:
                    return "FLOAT"
            return "FLOAT"

        t = target(name)
        cols = ", ".join(f"{f} {ty(f)}" for f in feats)
        self.db.sql(f"CREATE TABLE IF NOT EXISTS {t} (id KEYWORD PRIMARY KEY, ts INT, {cols})")
        self.db.sql(f"CREATE TABLE IF NOT EXISTS {t}_hist (id KEYWORD PRIMARY KEY, entity KEYWORD, ts INT, {cols})")
        for f in feats:
            for tb in (t, t + "_hist"):
                self.db.sql(f"ALTER TABLE {tb} ADD COLUMN IF NOT EXISTS {f} {ty(f)}")
        self.ready.add(name)

    def run(self, name, now, backfill=False):
        with self.job:
            return self._run(name, now, backfill)

    def _run(self, name, now, backfill=False):
        s = self.specs[name]
        t0 = time.perf_counter()
        err = None
        rows, query_ms, changed = [], 0.0, 0
        try:
            sql, names = build_sql(s, self.specs)
            rows = self.db.rows(sql, bind(names, now))
            query_ms = (time.perf_counter() - t0) * 1000
            self._ensure(s, rows)
            feats = [f["name"] for f in s["features"]]
            prev = self.prev.setdefault(name, {})
            cur, hist = [], []
            for r in rows:
                e = r.get("entity")
                if e is None:
                    continue
                e = str(e)
                vals = tuple(r.get(f) for f in feats)
                doc = {f: r.get(f) for f in feats}
                if not backfill:
                    cur.append({"op": "upsert", "table": target(name), "doc": {"id": e, "ts": now, **doc}})
                if prev.get(e) != vals:
                    prev[e] = vals
                    hist.append({"op": "upsert", "table": target(name) + "_hist", "doc": {"id": f"{e}@{now}", "entity": e, "ts": now, **doc}})
            changed = len(hist)
            for i in range(0, len(cur) + len(hist), 2000):
                batch = (cur + hist)[i:i + 2000]
                if batch:
                    self.db.write(batch)
            st = stats_of(rows, feats)
        except Exception as e:  # noqa: BLE001
            err = str(e)[:400]
            st = {}
        ms = (time.perf_counter() - t0) * 1000
        ops = [{"op": "upsert", "table": "fs_workflow_runs", "doc": {"id": f"{name}@{now}", "wf": name, "ts": now, "ms": ms, "query_ms": query_ms,
                                                                       "rows": len(rows), "changed": changed, "backfill": int(backfill), "error": err}}]
        if not backfill or err:
            ops += [{"op": "upsert", "table": "fs_workflow_stats", "doc": {"id": f"{name}.{f}", "wf": name, "feature": f, "ts": now, "run_ms": ms, **v}} for f, v in st.items()]
        self.db.write(ops)
        self.last[name] = now
        run = {"t": now, "ms": ms, "query_ms": query_ms, "rows": len(rows), "changed": changed, "error": err, "backfill": backfill}
        self.runs.setdefault(name, []).append(run)
        del self.runs[name][:-120]
        return run

    def tick(self, now=None):
        now = now or int(time.time() * 1000)
        with self.lock:
            order = topo(self.specs)
        for n in order:
            s = self.specs.get(n)
            if s and now - self.last.get(n, 0) >= s["every"] * 1000:
                self.run(n, now)

    def backfill(self, name, minutes, step_s=None):
        """Runs `name` (and what it reads, first) at past times, writing only
        history, then once now. Each past run sees the inputs as of its time."""
        with self.lock:
            order = [n for n in topo(self.specs) if n == name or n in self.upstream(name)]
        with self.job:
            self._backfill(order, minutes, step_s)

    def _backfill(self, order, minutes, step_s):
        now = int(time.time() * 1000)
        for n in order:
            self.prev.pop(n, None)
            step = (step_s or max(10, self.specs[n]["every"])) * 1000
            t = now - minutes * 60_000
            while t < now:
                self.run(n, t, backfill=True)
                t += step
            self.run(n, now)

    def upstream(self, name, acc=None):
        acc = acc if acc is not None else set()
        for d in deps(self.specs[name]):
            if d not in acc:
                acc.add(d)
                self.upstream(d, acc)
        return acc

    def downstream(self, name):
        out, todo = set(), [name]
        while todo:
            x = todo.pop()
            for n, s in self.specs.items():
                if x in deps(s) and n not in out:
                    out.add(n)
                    todo.append(n)
        return out

    def search(self, cond, now):
        """Features whose latest statistics match a SQL condition; `age_s` is
        the seconds since their workflow last ran."""
        if re.search(r"\b(select|from|union|join|into)\b|;|--|/\*", cond, re.I):
            raise SpecError("a condition on the statistics' columns, not a query")
        c = re.sub(r"\bage_s\b", f"(({int(now)} - ts) / 1000)", cond)
        rows = self.db.rows(f"SELECT wf, feature, rows, null_share, min, max, mean, distinct_values, zeros, run_ms, ts FROM fs_workflow_stats WHERE {c} LIMIT 1000")
        hit = sorted({r["wf"] for r in rows})
        down, up = set(), set()
        for w in hit:
            if w in self.specs:
                down |= self.downstream(w)
                up |= self.upstream(w)
        return {"features": rows, "workflows": hit, "downstream": sorted(down - set(hit)), "upstream": sorted(up - set(hit))}


# ------------------------------------------------------------------ the shop's

SEEDS = [
    {"name": "user_activity", "title": "What each shopper did",
     "inputs": [{"table": "fs_impressions", "alias": "a"}], "mode": "aggregate", "key": "a.usr", "time": "a.ts", "where": "",
     "features": [{"name": "views", "fn": "count"}, {"name": "clicks", "fn": "sum", "arg": "a.clicked"},
                  {"name": "clicks_5m", "fn": "sum", "arg": "a.clicked", "window": 300},
                  {"name": "avg_click_price", "fn": "avg", "arg": "a.price", "where": "a.clicked = 1"},
                  {"name": "ctr", "expr": "(clicks + 1.0) / (views + 10)"}], "every": 10},
    {"name": "item_popularity", "title": "How each product does, joined to the catalog",
     "inputs": [{"table": "fs_impressions", "alias": "a"}, {"table": "fs_items", "alias": "p", "on": ["a.item", "id"]}],
     "mode": "aggregate", "key": "a.item", "time": "a.ts", "where": "",
     "features": [{"name": "views", "fn": "count"}, {"name": "clicks", "fn": "sum", "arg": "a.clicked"},
                  {"name": "views_2m", "fn": "count", "window": 120}, {"name": "clicks_2m", "fn": "sum", "arg": "a.clicked", "window": 120},
                  {"name": "price", "fn": "max", "arg": "p.price"},
                  {"name": "ctr", "expr": "(clicks + 1.0) / (views + 10)"}, {"name": "trend_ctr", "expr": "(clicks_2m + 1.0) / (views_2m + 10)"}], "every": 10},
    {"name": "user_category", "title": "Each shopper in each category",
     "inputs": [{"table": "fs_impressions", "alias": "a"}], "mode": "aggregate", "key": "a.uc", "time": "a.ts", "where": "",
     "features": [{"name": "usr", "fn": "max", "arg": "a.usr"}, {"name": "views", "fn": "count"}, {"name": "clicks", "fn": "sum", "arg": "a.clicked"},
                  {"name": "affinity", "expr": "(clicks + 1.0) / (views + 5)"}], "every": 10},
    {"name": "item_signals", "title": "Products: hot or cold, from item_popularity",
     "inputs": [{"workflow": "item_popularity", "alias": "i"}], "mode": "map", "key": "i.entity", "time": "", "where": "",
     "features": [{"name": "hot", "expr": "clamp((i.trend_ctr - i.ctr) * 10, -2, 2)"}, {"name": "log_views", "expr": "ln(1 + i.views)"}], "every": 10},
    {"name": "user_category_share", "title": "A shopper's share of clicks per category, from two workflows",
     "inputs": [{"workflow": "user_category", "alias": "c"}, {"workflow": "user_activity", "alias": "u", "on": ["c.usr", "entity"]}],
     "mode": "map", "key": "c.entity", "time": "", "where": "",
     "features": [{"name": "share", "expr": "c.clicks / (u.clicks + 1.0)"}, {"name": "affinity", "expr": "c.affinity"},
                  {"name": "lift", "expr": "clamp(c.affinity / u.ctr, 0, 5)"}], "every": 10},
]
