import random
from collections import Counter

import pytest

import strata
from strata import ngrams


def sessions(seed, n, vocab):
    """Random sessions over a skewed vocabulary, so a few contexts are
    followed by hundreds of items and many items tie."""
    rng = random.Random(seed)
    items = [f"i{j:03d}" for j in range(vocab)]
    weights = [1 / (j + 1) for j in range(vocab)]
    return [rng.choices(items, weights, k=rng.randint(2, 12)) for _ in range(n)]


def load(db, sess, *, order=2, table="next"):
    ops = [op for seq in sess for i in range(1, len(seq)) for op in ngrams.ops(seq[:i], seq[i], table=table, order=order)]
    for j in range(0, len(ops), 1000):
        db.write(ops[j : j + 1000])
    return len(ops)


def counted(sess, order=3):
    """What came after each context of up to `order` items."""
    counts = {}
    for seq in sess:
        for i in range(1, len(seq)):
            for m in range(1, min(order, i) + 1):
                counts.setdefault(tuple(seq[i - m : i]), Counter())[seq[i]] += 1
    return counts


def reference(counts, history, k, *, order=2, backoff=0.4):
    """Stupid backoff scored for every item."""
    scores = {}
    for i, ctx in enumerate(tuple(history[-m:]) for m in range(min(order, len(history)), 0, -1)):
        c = counts.get(ctx)
        if not c:
            continue
        total = sum(c.values())
        for w, n in c.items():
            scores.setdefault(w, backoff**i * n / total)
    return sorted(scores.items(), key=lambda kv: (-kv[1], kv[0]))[:k]


def histories(sess, seed, n):
    rng = random.Random(seed)
    out = [[], ["nope"], ["nope", "i000"], ["i000", "nope"]]
    for _ in range(n):
        seq = rng.choice(sess)
        i = rng.randint(1, len(seq))
        out.append(seq[max(0, i - 3) : i])
    return out


def check(db, sess, hs, *, order=2, ks=(1, 3, 10, 70), backoff=0.4):
    counts = counted(sess)
    for h in hs:
        for k in ks:
            got = strata.next_items(db, h, k, order=order, backoff=backoff)
            want = reference(counts, h, k, order=order, backoff=backoff)
            assert [x for x, _ in got] == [x for x, _ in want], (h, k)
            assert [s for _, s in got] == pytest.approx([s for _, s in want]), (h, k)


def test_next_items_equal_scoring_every_item(tmp_path):
    sess = sessions(7, 3000, 400)
    path = tmp_path / "next.strata"
    with strata.open(path, ngrams.schema()) as db:
        assert db.schema["cluster_by"] == "id"
        load(db, sess)
        hs = histories(sess, 1, 150)
        check(db, sess, hs)
        # Contexts followed by more items than a page holds.
        assert db.query("SELECT COUNT(*) FROM next WHERE ctx = 'i000'").scalar() > 300
        check(db, sess, [["i001", "i000"], ["i000"]], ks=(5, 200, 1000), backoff=0.9)
        db.optimize()
        check(db, sess, hs[:40])
    with strata.open(path) as db:
        check(db, sess, hs[:40])
        assert strata.next_items(db, ["i000"], 0) == []


def test_next_items_of_longer_contexts(tmp_path):
    sess = sessions(11, 800, 60)
    with strata.Database.create(tmp_path / "db.strata", {"table": "items", "key": "id", "fields": [{"name": "id", "type": "keyword"}]}) as db:
        assert db.sql(ngrams.create_table("tri")) == 0
        load(db, sess, order=3, table="tri")
        hs = histories(sess, 2, 60)
        counts = counted(sess)
        for order in (1, 2, 3):
            for h in hs:
                for k in (1, 10):
                    got = strata.next_items(db, h, k, table="tri", order=order)
                    assert [x for x, _ in got] == [x for x, _ in reference(counts, h, k, order=order)], (h, k, order)


def test_ops_keys_and_decay(tmp_path):
    assert ngrams.ops(["a"], "b") == [{"op": "add", "table": "next", "field": "n", "value": 1, "doc": {"id": "a\x1eb", "ctx": "a", "nxt": "b"}}]
    assert [o["doc"]["id"] for o in ngrams.ops(["x", "a", "b"], "c", order=3, weight=0.5)] == ["b\x1ec", "a\x1fb\x1ec", "x\x1fa\x1fb\x1ec"]
    assert ngrams.ops([], "a") == []
    with pytest.raises(ValueError, match="cannot hold"):
        ngrams.ops(["a\x1fb"], "c")
    with pytest.raises(TypeError, match="items are str"):
        ngrams.ops([1], "c")
    with strata.open(tmp_path / "n.strata", ngrams.schema()) as db:
        for _ in range(3):
            db.write(ngrams.ops(["a", "b"], "c"))
        db.write(ngrams.ops(["z", "b"], "d"))
        assert strata.next_items(db, ["a", "b"]) == [("c", 1.0), ("d", 0.4 * 1 / 4)]
        # An unseen pair backs off to what came after b.
        assert strata.next_items(db, ["q", "b"]) == [("c", 0.4 * 3 / 4), ("d", 0.4 * 1 / 4)]
        assert db.sql("UPDATE next SET n = n * 0.5") == 4
        assert strata.next_items(db, ["b"]) == [("c", 0.75), ("d", 0.25)]
        # A weight of 0 is not followed.
        assert db.sql("UPDATE next SET n = 0 WHERE nxt = 'c'") == 2
        assert strata.next_items(db, ["a", "b"]) == [("d", 0.4)]
        assert strata.next_items(db, ["a", "b"], backoff=1) == [("d", 1.0)]


def test_database_write_ops(tmp_path):
    schema = {"table": "t", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "n", "type": "int"}, {"name": "w", "type": "float"}]}
    with strata.Database.create(tmp_path / "w.strata", schema) as db:
        assert db.write([{"op": "insert", "doc": {"id": "a", "n": 1}}, {"op": "upsert", "doc": {"id": "b", "n": 2}}]) == 2
        assert db.write([{"op": "add", "key": "a", "field": "n", "value": 5}, {"op": "add", "key": "nope", "field": "n", "value": 5}]) == 1
        assert db.write([{"op": "add", "field": "w", "value": 0.5, "doc": {"id": "c"}}, {"op": "add", "field": "w", "value": 0.25, "doc": {"id": "c"}}]) == 2
        assert db.write([{"op": "set", "key": "b", "field": "n", "value": 9}, {"op": "delete", "key": "a"}, {"op": "delete", "key": "a"}]) == 2
        assert sorted(db.query("SELECT id, n, w FROM t").rows) == [["b", 9, None], ["c", None, 0.75]]
        for bad, msg in [
            ({"doc": {"id": "x"}}, 'needs "op"'),
            ({"op": "insert"}, 'insert needs "doc"'),
            ({"op": "add", "key": "b", "field": "n", "value": "1"}, "numeric value"),
            ({"op": "add", "key": "b", "field": "n", "value": 1, "doc": {"id": "c"}}, "differ"),
            ({"op": "set", "key": "b", "field": "nope", "value": 1}, "no field nope"),
            ({"op": "merge", "key": "b"}, "unknown op"),
            ({"op": "insert", "doc": {"id": "b"}}, "duplicate"),
        ]:
            with pytest.raises(strata.StrataError, match=msg):
                db.write([{"op": "insert", "doc": {"id": "y"}}, bad])
        # All or none: the first operation of each refused batch is not written.
        assert db.query("SELECT COUNT(*) FROM t WHERE id = 'y'").scalar() == 0
        assert db.sql(ngrams.create_table()) == 0
        assert db.write(ngrams.ops(["a"], "b")) == 1
        assert db.query("SELECT ctx, nxt, n FROM next").rows == [["a", "b", 1.0]]


def test_next_items_over_a_server(tmp_path, serve):
    sess = sessions(3, 300, 40)
    path = tmp_path / "served.strata"
    with strata.open(path, ngrams.schema()) as db:
        load(db, sess)
    client = strata.connect(serve(path))
    hs = histories(sess, 4, 30)
    check(client, sess, hs, ks=(5,))
    assert client.write(ngrams.ops(["i000", "i001"], "new")) == 2
    assert ("new", pytest.approx(1 / client.query("SELECT SUM(n) FROM next WHERE ctx = :c", c="i000\x1fi001").scalar())) in strata.next_items(client, ["i000", "i001"], 100)
