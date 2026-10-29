import json
import threading

import pytest

import strata
from conftest import BOOKS, book


def test_create_open_and_schema(tmp_path):
    path = tmp_path / "s.strata"
    schema_file = tmp_path / "books.json"
    schema_file.write_text(json.dumps(BOOKS))
    with strata.open(path, schema=schema_file) as db:
        assert db.schema["key"] == "id"
        assert [f["name"] for f in db.schema["fields"]][:2] == ["id", "title"]
        assert list(db.schema["fields"][0])[:2] == ["name", "type"]
        db.insert({"id": "a", "title": "one"})
    with strata.open(path) as db:
        assert len(db) == 1
        assert db.schema == strata.Database.create(tmp_path / "t.strata", json.dumps(BOOKS)).schema
    with pytest.raises(strata.StrataError, match="does not exist"):
        strata.open(tmp_path / "missing.strata")
    with pytest.raises(strata.StrataError, match="dim"):
        strata.Database.create(tmp_path / "bad.strata", {"table": "t", "fields": [{"name": "v", "type": "vector"}]})


def test_writes(db):
    assert len(db) == 200
    # Small inserts go through the log, large ones are bulk-loaded; both
    # write all rows or none.
    with pytest.raises(strata.StrataError, match="b0001"):
        db.insert([book(1000), book(1)])
    with pytest.raises(strata.StrataError):
        db.insert([book(i) for i in range(1000, 2500)] + [book(3)])
    assert len(db) == 200
    assert db.insert([book(i) for i in range(1000, 2500)]) == 1500
    assert db.upsert([{**book(5), "title": "Changed"}, book(9000)]) == 2
    assert db.query("SELECT title FROM books WHERE id = 'b0005'").scalar() == "Changed"
    assert db.delete(["b0005", "nope"]) == 1
    assert db.delete("b9000") == 1
    assert len(db) == 1699
    with pytest.raises(strata.StrataError, match="colour"):
        db.insert({"id": "x", "colour": "red"})
    with pytest.raises(strata.StrataError, match="year"):
        db.insert({"id": "x", "year": "soon"})
    with pytest.raises(ValueError, match="cannot store"):
        db.insert({"id": "x", "title": object()})


def test_queries_and_results(db):
    r = db.query("SELECT id, year FROM books WHERE year >= :y AND genres = :g ORDER BY year DESC LIMIT 3", {"y": 2000}, g="fantasy")
    assert r.columns == ["id", "year"] and len(r) == 3
    assert r[0]["year"] >= r[1]["year"] >= 2000
    assert [row["id"] for row in r] == r.column("id") == r.keys
    assert isinstance(r.stats, dict)
    assert db.sql("SELECT COUNT(*) FROM books").scalar() == 200
    assert db.sql("EXPLAIN SELECT id FROM books LIMIT 2").column("plan") == ["Project: id", "  Limit: 2", "    Search books"]
    # A plan, as a dict, runs as its statement does.
    plan = db.plan("SELECT id FROM books WHERE year >= :y ORDER BY year DESC LIMIT 3", y=2000)
    assert plan["version"] == 1 and plan["root"]["node"] == "project"
    assert db.run(plan).keys == db.query("SELECT id FROM books WHERE year >= 2000 ORDER BY year DESC LIMIT 3").keys
    assert db.run(db.plan("UPDATE books SET rating = 1 WHERE id = 'b0002'")) == 1
    with pytest.raises(strata.StrataError, match="newer"):
        db.run({**plan, "version": 2})
    # A compound query's plan has operators over single-table queries.
    sql = "SELECT id FROM books WHERE year > (SELECT AVG(year) FROM books) ORDER BY year DESC LIMIT 5"
    plan = db.plan(sql)
    assert plan["features"] == ["operators"]
    assert len(db.run(plan).keys) == 5 and db.run(plan).keys == db.query(sql).keys
    assert db.sql("UPDATE books SET rating = rating + 1 WHERE id = :k", k="b0001") == 1
    assert db.query("SELECT genres FROM books WHERE id = 'b0001'").scalar() == ["fantasy", "new"]
    # A vector, the text's embedding, and a stored row's vector.
    v = db.embed("dragon river")
    assert len(v) == 32
    a = db.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 5", v=v).keys
    b = db.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 5", v={"embed": "dragon river"}).keys
    assert a == b
    near = db.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 1", v={"vector_of": "b0007"}).keys
    assert near == ["b0007"]
    with pytest.raises(strata.StrataError):
        db.query("SELECT nope FROM books")


def test_vector_of_any_field_name(tmp_path):
    # The SQL it ran did not quote the names, so a space broke it.
    schema = {"table": "t", "primary_key": "doc id", "fields": [{"name": "doc id", "type": "keyword"}, {"name": "my vec", "type": "vector", "dim": 2}]}
    with strata.open(tmp_path / "v.strata", schema) as db:
        db.insert([{"doc id": f"n{i}", "my vec": [1.0, float(i)]} for i in range(4)])
        near = db.query('SELECT "doc id" FROM t ORDER BY similarity("my vec", :v) DESC LIMIT 1', v={"vector_of": "n3", "field": "my vec"}).keys
        assert near == ["n3"]


def test_numpy_values(db):
    np = pytest.importorskip("numpy")
    v = np.array(db.embed("winter star"), dtype=np.float32)
    assert db.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 1", v=v).keys
    db.upsert({"id": "n", "title": "numpy", "year": np.int64(2001), "emb": v})
    assert db.query("SELECT year FROM books WHERE id = 'n'").scalar() == 2001


def test_search(db):
    text = db.search("dragon", mode="text", limit=5)
    assert len(text) == 5 and all("dragon" in (r["title"] + r["description"]) for r in text)
    assert db.search("dragon", mode="vector", limit=5).keys
    hybrid = db.search("dragon", limit=5, where="year >= :y", params={"y": 1950}, columns=["id", "year"])
    assert hybrid.columns == ["id", "year"] and all(r["year"] >= 1950 for r in hybrid)
    with pytest.raises(ValueError):
        db.search("x", mode="fuzzy")


def test_import_file(tmp_path):
    rows = tmp_path / "rows.jsonl"
    rows.write_text("\n".join(json.dumps({**book(i), "publisher": "Acme"}) for i in range(50)))
    csv = tmp_path / "more.csv"
    csv.write_text('id,title,genres,year\nb0001,"Re, titled",a|b,1999\nc1,New,,\n')
    with strata.Database.create(tmp_path / "i.strata", BOOKS) as db:
        assert db.import_file(rows) == {"rows": 50, "bulk": True, "skipped_fields": ["publisher"]}
        assert db.import_file(csv) == {"rows": 2, "bulk": False, "skipped_fields": []}
        assert len(db) == 51
        assert db.query("SELECT title, genres FROM books WHERE id = 'b0001'").rows == [["Re, titled", ["a", "b"]]]
        with pytest.raises(strata.StrataError, match="line 1"):
            bad = tmp_path / "bad.jsonl"
            bad.write_text('{"id": 1, "year": "x"}\n')
            db.import_file(bad)


def test_models_that_do_not_open(tmp_path):
    schema = {"table": "t", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "body", "type": "text"}, {"name": "v", "type": "vector", "embed": {"from": "body", "model": "potion-base-8M"}}]}
    with strata.Database.create(tmp_path / "m.strata", schema, models=tmp_path) as db:
        assert "v" in db.embed_errors
        db.insert({"id": "a", "body": "no vector"})
        db.insert({"id": "b", "body": "a vector", "v": [0.1] * 256})
        assert len(db) == 2
        assert db.search("vector").keys  # text only: the model is not open
        with pytest.raises(strata.StrataError, match="model"):
            db.embed("x")


def test_threads_and_close(tmp_path, db):
    errors = []

    def work():
        try:
            for _ in range(20):
                assert len(db.search("winter", limit=3)) == 3
        except Exception as e:  # noqa: BLE001
            errors.append(e)

    threads = [threading.Thread(target=work) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors
    db.close()
    assert db.closed and "closed" in repr(db)
    with pytest.raises(strata.StrataError, match="closed"):
        db.query("SELECT id FROM books")


TASKS = {
    "table": "tasks",
    "key": "id",
    "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "done", "type": "bool"},
        {"name": "at", "type": "timestamp"},
        {"name": "n", "type": "int"},
    ],
}


def test_bool_and_timestamp(tmp_path):
    from datetime import date, datetime, timedelta, timezone

    with strata.Database.create(tmp_path / "tasks.strata", TASKS) as db:
        assert [f["type"] for f in db.schema["fields"]] == ["keyword", "bool", "timestamp", "int"]
        db.insert([
            {"id": "a", "done": True, "at": datetime(2026, 10, 1, 9, 0, tzinfo=timezone.utc), "n": 1},
            {"id": "b", "done": False, "at": date(2026, 10, 2), "n": 2},
            {"id": "c", "done": None, "at": datetime(2026, 10, 1, 3, 30, tzinfo=timezone(timedelta(hours=2))), "n": 3},
            {"id": "d", "done": "true", "at": "2026-10-03T12:00:00.25Z", "n": 4},
        ])
        # Booleans come back as bool, times as ISO 8601 text in UTC (as the
        # HTTP API gives them).
        r = db.query("SELECT id, done, at FROM tasks ORDER BY n")
        assert [list(row.values()) for row in r] == [
            ["a", True, "2026-10-01T09:00:00Z"],
            ["b", False, "2026-10-02T00:00:00Z"],
            ["c", None, "2026-10-01T01:30:00Z"],
            ["d", True, "2026-10-03T12:00:00.250Z"],
        ]
        assert db.query("SELECT id FROM tasks WHERE done = :d ORDER BY n", d=True).keys == ["a", "d"]
        since = datetime(2026, 10, 1, 12, 0)
        assert db.query("SELECT id FROM tasks WHERE at >= :t ORDER BY n", t=since).keys == ["b", "d"]
        assert db.query("SELECT id FROM tasks WHERE NOT done AND at < '2026-10-05'").keys == ["b"]
        assert db.query("SELECT MAX(at) FROM tasks").scalar() == "2026-10-03T12:00:00.250Z"
        assert db.sql("UPDATE tasks SET done = :d, at = :t WHERE id = 'c'", d=False, t=date(2026, 11, 1)) == 1
        assert db.query("SELECT done, at FROM tasks WHERE id = 'c'")[0] == {"done": False, "at": "2026-11-01T00:00:00Z"}
        with pytest.raises(strata.StrataError, match="done"):
            db.insert({"id": "x", "done": 2})
        with pytest.raises(strata.StrataError, match="at"):
            db.insert({"id": "x", "at": "next week"})


def test_not_null_and_default(tmp_path):
    schema = {
        "table": "events",
        "key": "id",
        "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "kind", "type": "keyword", "not_null": True, "default": "click"},
            {"name": "n", "type": "int", "default": 1},
            {"name": "at", "type": "timestamp", "default": "now"},
        ],
    }
    with strata.Database.create(tmp_path / "events.strata", schema) as db:
        assert db.schema["fields"][1] == {"name": "kind", "type": "keyword", "not_null": True, "default": "click"}
        db.insert([{"id": "a"}, {"id": "b", "kind": "view", "n": None}])
        r = db.query("SELECT id, kind, n FROM events WHERE at IS NOT NULL ORDER BY n")
        assert sorted(list(row.values()) for row in r) == [["a", "click", 1], ["b", "view", None]]
        with pytest.raises(strata.StrataError, match="kind is NOT NULL"):
            db.insert({"id": "x", "kind": None})
        assert db.sql("INSERT INTO events (id, n) VALUES ('c', DEFAULT)") == 1
        assert db.query("SELECT kind, n FROM events WHERE id = 'c'")[0] == {"kind": "click", "n": 1}
        with pytest.raises(strata.StrataError, match="kind is NOT NULL"):
            db.sql("UPDATE events SET kind = NULL WHERE id = 'a'")


def test_simple_analyzer(tmp_path):
    schema = {
        "table": "notes",
        "key": "id",
        "fields": [
            {"name": "id", "type": "keyword"},
            {"name": "title", "type": "text", "analyzer": "simple"},
            {"name": "body", "type": "text"},
        ],
    }
    with strata.Database.create(tmp_path / "notes.strata", schema) as db:
        assert db.schema["fields"][1] == {"name": "title", "type": "text", "analyzer": "simple"}
        db.insert([{"id": "a", "title": "The Running Cats", "body": "The Running Cats"}, {"id": "b", "title": "they run", "body": "they run"}])

        def ids(field, q):
            return sorted(row["id"] for row in db.query(f"SELECT id FROM notes WHERE MATCH({field}, '{q}')"))

        assert ids("title", "run") == ["b"]
        assert ids("body", "run") == ["a", "b"]
        assert ids("title", "the") == ["a"]
    with pytest.raises(strata.StrataError, match="analyzer"):
        strata.Database.create(tmp_path / "bad.strata", {"table": "t", "fields": [{"name": "t", "type": "text", "analyzer": "klingon"}]})


def test_joins_across_tables(db):
    assert db.sql("CREATE TABLE genres (name KEYWORD PRIMARY KEY, shelf INT)") == 0
    assert db.sql("INSERT INTO genres (name, shelf) VALUES ('fantasy', 1), ('mystery', 2), ('classic', 3)") == 3
    # Each 1910 book with the shelf of each of its genres that has one.
    r = db.query("SELECT b.id, g.shelf FROM books b JOIN genres g ON b.genres = g.name WHERE b.year = 1910 ORDER BY b.id, g.shelf")
    want = sorted((book(i)["id"], {"fantasy": 1, "mystery": 2, "classic": 3}[g]) for i in range(200) if book(i)["year"] == 1910 for g in book(i)["genres"] if g != "new")
    assert [tuple(row.values()) for row in r] == want
    r = db.query("SELECT COUNT(*) AS n FROM books WHERE genres IN (SELECT name FROM genres WHERE shelf >= :s)", {"s": 3})
    assert r[0]["n"] == sum(1 for i in range(200) if "classic" in book(i)["genres"])
