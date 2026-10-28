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
