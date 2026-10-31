import datetime
import json

import pytest

import strata


def grid(n):
    """Vectors near the corners of a square (level 1) and then a smaller
    square around each (level 2)."""
    out = []
    for i in range(n):
        a, b = i % 4, (i // 4) % 4
        out.append([10.0 * (a % 2) + (1 if b % 2 else -1) + 0.01 * (i % 7), 10.0 * (a // 2) + (1 if b // 2 else -1)])
    return out


def items_schema(codebook):
    return {
        "table": "items",
        "key": "id",
        "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "emb", "type": "vector", "dim": 2, "index": "flat"},
            {"name": "sid", "type": "keyword", "multi": True, "semantic_id": {"from": "emb", "codebook": codebook}},
        ],
    }


def test_a_codebook_from_vectors_gives_a_table_its_codes(tmp_path):
    models = tmp_path / "models"
    h = strata.train.codebook(grid(400), "grid", models=models, levels=2, k=4)
    assert len(h) == 64 and (models / "grid" / "codebook.safetensors").exists()
    # The same vectors and options give the same file; another under the
    # same name is refused.
    with pytest.raises(strata.StrataError, match="grid"):
        strata.train.codebook(grid(400)[::-1][:50], "grid", models=models, levels=2, k=3)
    db = strata.Database.create(tmp_path / "items.strata", items_schema("grid"), models=models)
    assert db.schema["fields"][2]["semantic_id"]["hash"] == h
    db.upsert([{"id": f"i{i}", "emb": v} for i, v in enumerate(grid(40))])
    sids = {r["id"]: r["sid"] for r in db.query("SELECT id, sid FROM items")}
    # Every prefix of a two-level code and its tie, shortest first.
    for sid in sids.values():
        assert len(sid) == 3 and sid[1].startswith(sid[0] + ".") and sid[2].startswith(sid[1] + ".")
    # Rows 0, 16 and 32 sit at the same point of the grid: one code, three
    # ties; rows 0 and 4 share a first level only.
    assert sids["i0"][1] == sids["i16"][1] == sids["i32"][1]
    assert sorted(sids[i][2].rsplit(".", 1)[1] for i in ("i0", "i16", "i32")) == ["0", "1", "2"]
    assert sids["i0"][0] == sids["i4"][0] and sids["i0"][1] != sids["i4"][1]
    level1 = sids["i0"][0]
    got = db.query("SELECT id FROM items WHERE sid = :p LIMIT 100", p=level1)
    assert {r["id"] for r in got} == {i for i, s in sids.items() if s[0] == level1}
    assert db.query("SELECT sid_at(sid, 1) AS l1 FROM items WHERE id = 'i0'").scalar() == level1
    db.close()
    # Reopened with its models directory, new rows get codes too.
    db = strata.Database.open(tmp_path / "items.strata", models=models)
    assert db.embed_errors == {}
    db.insert({"id": "new", "emb": grid(1)[0]})
    assert db.query("SELECT sid FROM items WHERE id = 'new'").scalar()[1] == sids["i0"][1]


def test_a_codebook_from_a_tables_vectors(tmp_path):
    models = tmp_path / "models"
    src = strata.Database.create(tmp_path / "src.strata", {"table": "docs", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "emb", "type": "vector", "dim": 2}]})
    src.upsert([{"id": f"d{i}", "emb": v} for i, v in enumerate(grid(200))])
    a = strata.train.codebook(src, "learned", field="emb", models=models, levels=2, k=4, seed=7)
    b = strata.train.codebook(grid(200), "again", models=models, levels=2, k=4, seed=7)
    assert len(a) == 64 and len(b) == 64
    with pytest.raises(TypeError, match="field"):
        strata.train.codebook(src, "x", models=models)
    with pytest.raises(strata.StrataError, match="VECTOR"):
        strata.train.codebook(src, "x", field="id", models=models)
    # A table cannot name a codebook that is not there.
    with pytest.raises(strata.StrataError, match="train"):
        strata.Database.create(tmp_path / "none.strata", items_schema("missing"), models=models)


def brute_next(rows, prefix, keep=lambda r: True):
    """The children of `prefix` in a trie of the rows' codes, with counts."""
    depth = 0 if prefix == "" else prefix.count(".") + 1
    out = {}
    for r in rows:
        code = r["sid"][-1].split(".")
        if keep(r) and len(code) > depth and ".".join(code[:depth]) == prefix:
            child = ".".join(code[: depth + 1])
            out[child] = out.get(child, 0) + 1
    return dict(sorted(out.items(), key=lambda kv: int(kv[0].rsplit(".", 1)[-1])))


def test_next_codes_in_process_and_over_http(tmp_path, serve):
    models = tmp_path / "models"
    strata.train.codebook(grid(400), "grid", models=models, levels=2, k=4)
    schema = items_schema("grid")
    schema["fields"].append({"name": "tag", "type": "keyword"})
    path = tmp_path / "items.strata"
    db = strata.Database.create(path, schema, models=models)
    db.upsert([{"id": f"i{i}", "emb": v, "tag": ["red", "blue"][i % 2]} for i, v in enumerate(grid(120))])
    rows = list(db.query("SELECT id, sid, tag FROM items LIMIT 1000"))
    prefixes = ["", "0", "1", rows[0]["sid"][1], "9"]
    want = {p: brute_next(rows, p) for p in prefixes}
    assert db.sid_next(prefixes, field="sid") == want
    red = {p: brute_next(rows, p, lambda r: r["tag"] == "red") for p in prefixes}
    assert db.sid_next(prefixes, field="sid", where="tag = :t", t="red") == red
    with pytest.raises(strata.StrataError, match="not a prefix"):
        db.sid_next(["1..2"], field="sid")
    db.close()
    url = serve(path, "--models", str(models))
    c = strata.connect(url)
    assert c.sid_next(prefixes, field="sid") == want
    assert c.sid_next(prefixes, field="sid", where="tag = :t", params={"t": "red"}) == red


def test_a_sequence_model_trained_from_a_table_ranks_in_strata_as_in_pytorch(tmp_path):
    torch = pytest.importorskip("torch")
    import random

    from strata import _sequence as S

    rng = random.Random(3)
    models = tmp_path / "models"
    schema = {"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "tag", "type": "keyword"}, {"name": "emb", "type": "vector", "dim": 8, "index": "flat"}]}
    db = strata.Database.create(tmp_path / "db", schema, models=models)
    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD NOT NULL, action KEYWORD, ts TIMESTAMP NOT NULL)")
    db.upsert([{"id": f"i{i}", "tag": f"t{i % 3}", "emb": [rng.gauss(0, 0.3) for _ in range(8)]} for i in range(30)])
    # Each user walks the items two at a time from a start of their own.
    events, e = [], 0
    for u in range(60):
        start = rng.randrange(30)
        for j in range(rng.randrange(4, 12)):
            events.append({"id": e, "usr": f"u{u}", "item": f"i{(start + 2 * j) % 30}", "action": rng.choice(["view", "buy"]), "ts": 1_700_000_000 + j * 60})
            e += 1
    iso = lambda t: datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")  # noqa: E731
    db.insert([{**x, "ts": iso(x["ts"])} for x in events], table="events")
    out = strata.train.sequence(db, "walk", events={"table": "events", "user": "usr", "item": "item", "action": "action", "time": "ts"},
                                models=models, width=16, max_len=10, learn_items=True, epochs=80, batch=16, lr=1e-2, seed=1)
    assert (models / "walk" / "config.json").exists() and (models / "walk" / "model.safetensors").exists()
    config = json.loads((models / "walk" / "config.json").read_text())
    assert (config["table"], config["field"], config["dim"], config["actions"]) == ("items", "emb", 8, ["", "buy", "view"])
    # The model keeps the vectors it learned, and is not served over others.
    saved = S.read_items(str(models / "walk"))
    assert list(saved) == list(out["vectors"]) and all(max(abs(a - b) for a, b in zip(saved[k], v)) < 1e-6 for k, v in out["vectors"].items())
    history = "(SELECT item, action, ts FROM events WHERE usr = :u ORDER BY ts DESC LIMIT 200)"
    sql = f"SELECT id, score() FROM items ORDER BY similarity(emb, encode('walk', {history}, now => :t)) DESC LIMIT 5"
    with pytest.raises(strata.StrataError, match="not those sequence model walk was trained with"):
        db.query(sql, {"u": "u0", "t": iso(1_800_000_000)})
    assert strata.train.write_vectors(db, "walk", models=models) == 30
    # Only the field changed.
    assert db.query("SELECT id, tag FROM items WHERE id = 'i4'").rows == [["i4", "t1"]]

    model, cfg = S.load(str(models / "walk"))
    keys = list(out["vectors"])
    index = {k: i for i, k in enumerate(keys)}
    E = torch.tensor([out["vectors"][k] for k in keys])
    learned = 0
    for u in range(20):
        mine = [x for x in events if x["usr"] == f"u{u}"]
        now = mine[-1]["ts"] + 60
        r = db.query(sql, {"u": f"u{u}", "t": iso(now)})
        h = [(x["item"], x["action"], float(x["ts"])) for x in mine]
        v = S.encode_batch(model, [h], index, {a: i for i, a in enumerate(cfg["actions"])}, cfg, E, [float(now)])[0]
        want = sorted(keys, key=lambda k: -float(E[index[k]] @ v))[:5]
        assert [row[0] for row in r.rows] == want
        assert all(abs(s - float(E[index[k]] @ v)) < 1e-4 for k, s in r.rows)
        # The next item of the walk.
        learned += r.rows[0][0] == f"i{(int(mine[-1]['item'][1:]) + 2) % 30}"
    assert learned >= 15, learned
