import json
import os
import shutil
import threading

import pytest

import strata
from strata import bm25, col, cross_encoder, gbdt, score
from conftest import BOOKS, book, tone_wav


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


def test_a_query_by_sound_needs_a_model_of_sound(db):
    """`embed(audio=...)` and `{"embed": {"audio": ...}}` go to a model of
    sound; the books' model reads text only, and a file given as audio has
    to be sound."""
    import base64

    wav = tone_wav()
    with pytest.raises(ValueError, match="no embedded field has a model of sound open"):
        db.embed(audio=wav)
    with pytest.raises(ValueError, match="one of them"):
        db.embed(image=b"x", audio=wav)
    with pytest.raises(strata.StrataError, match="does not embed sound"):
        db.embed(audio=wav, field="emb")
    with pytest.raises(strata.StrataError, match="not a sound file"):
        db.embed(audio=b"not a sound", field="emb")
    q = "SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 1"
    with pytest.raises(strata.StrataError, match="not a sound file"):
        db.query(q, v={"embed": {"audio": base64.b64encode(b"not a sound").decode()}})
    with pytest.raises(strata.StrataError, match="is a sound file"):
        db.query(q, v={"embed": {"image": base64.b64encode(wav).decode()}})


def test_vector_of_any_field_name(tmp_path):
    # The SQL it ran did not quote the names, so a space broke it.
    schema = {"table": "t", "primary_key": "doc id", "fields": [{"name": "doc id", "type": "keyword"}, {"name": "my vec", "type": "vector", "dim": 2}]}
    with strata.open(tmp_path / "v.strata", schema) as db:
        db.insert([{"doc id": f"n{i}", "my vec": [1.0, float(i)]} for i in range(4)])
        near = db.query('SELECT "doc id" FROM t ORDER BY similarity("my vec", :v) DESC LIMIT 1', v={"vector_of": "n3", "field": "my vec"}).keys
        assert near == ["n3"]



def test_multi_vector_field(tmp_path):
    # A row holds several vectors; maxsim sums each query vector's best match.
    schema = {"table": "pages", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "patches", "type": "vector", "dim": 3, "multi": True, "index": "flat", "keep_f32": True}]}
    with strata.open(tmp_path / "m.strata", schema) as db:
        db.insert([{"id": "a", "patches": [[1, 0, 0], [0, 1, 0]]}, {"id": "b", "patches": [[0, 0, 1]]}, {"id": "c", "patches": [0.5, 0.5, 0]}, {"id": "d"}])
        r = db.query("SELECT id, patches FROM pages ORDER BY maxsim(patches, :qs) DESC LIMIT 5", qs=[[1, 0, 0], [0, 1, 0]])
        assert r.keys == ["a", "c", "b"] and r.scores == [2.0, 1.0, 0.0]
        assert r.rows[0][1] == [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]
        assert db.query("SELECT patches FROM pages WHERE id = 'd'").scalar() is None
        with pytest.raises(strata.StrataError, match="maxsim"):
            db.query("SELECT id FROM pages ORDER BY similarity(patches, :q) DESC LIMIT 1", q=[1, 0, 0])

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


def test_writes_to_any_table(db, tmp_path):
    assert db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, book KEYWORD REFERENCES books, ts INT) OPTIONS(shard_by = 'usr')") == 0
    assert [t["table"] for t in db.tables()] == ["books", "events"]
    assert db.tables()[1]["shard_by"] == "usr" and list(db.tables()[1])[:3] == ["table", "key", "shard_by"]
    assert db.schema["table"] == "books"
    assert db.insert([{"id": i, "usr": f"u{i % 3}", "book": book(i)["id"], "ts": i} for i in range(1500)], table="events") == 1500
    assert db.upsert({"id": 1, "usr": "u1", "book": "b0007", "ts": 9}, table="EVENTS") == 1
    assert db.delete([2, 3, 99999], table="events") == 2
    f = tmp_path / "more.csv"
    f.write_text("id,usr,ts\n5000,u9,1\n5001,u9,2\n")
    assert db.import_file(f, table="events") == {"rows": 2, "bulk": False, "skipped_fields": []}
    assert db.query("SELECT COUNT(*) FROM events").scalar() == 1500
    assert len(db) == 200
    r = db.table("events").select("book").where(strata.col("usr") == "u9").order_by(strata.col("ts")).run()
    assert r.column("book") == [None, None]
    assert db.query("SELECT book FROM events WHERE id = 1").scalar() == "b0007"
    with pytest.raises(strata.StrataError, match="no such table: nope"):
        db.insert({"id": 1}, table="nope")
    with pytest.raises(strata.StrataError, match="no table nope"):
        db.table("nope")
    with pytest.raises(strata.StrataError, match="shard_by"):
        db.sql("UPDATE events SET usr = 'u2' WHERE id = 1")


def test_an_edge_table_walked_from_another_table(db):
    db.sql("CREATE TABLE cooc (id KEYWORD PRIMARY KEY, src KEYWORD NOT NULL REFERENCES books, dst KEYWORD NOT NULL REFERENCES books, w FLOAT DEFAULT 0) OPTIONS(edges = 'src, dst, w')")
    t = db.tables()[-1]
    assert list(t)[:3] == ["table", "key", "edges"] and t["edges"] == "src, dst, w"
    db.insert([{"id": f"{a}>{b}", "src": a, "dst": b, "w": w} for a, b, w in [("b0001", "b0002", 1.0), ("b0002", "b0003", 2.0), ("b0003", "b0004", 0.0)]], table="cooc")
    hops = db.query("SELECT id, hops() FROM books WHERE REACHABLE(cooc, 'b0001', hops => 3) LIMIT 10")
    # The edge of weight 0 is not followed.
    assert sorted((r["id"], r["hops()"]) for r in hops) == [("b0002", 1), ("b0003", 2)]
    top = db.query("SELECT id FROM books ORDER BY ppr(cooc, 'b0002') DESC LIMIT 3").column("id")
    assert top[0] == "b0002" and set(top[1:]) == {"b0001", "b0003"}
    with pytest.raises(strata.StrataError, match="not an edge table"):
        db.query("SELECT id FROM books ORDER BY ppr(books, 'b0001') DESC LIMIT 3")


def test_a_table_ordered_by_a_column_and_kept_for_a_period(db):
    assert db.sql("CREATE TABLE views (id INT PRIMARY KEY, usr KEYWORD, ts TIMESTAMP) OPTIONS(cluster_by = 'usr', retain = '30 days')") == 0
    t = db.tables()[1]
    assert list(t)[:5] == ["table", "key", "cluster_by", "retain", "retain_by"]
    assert (t["cluster_by"], t["retain"], t["retain_by"]) == ("usr", "30 days", "ts")
    with pytest.raises(strata.StrataError, match="cluster_by"):
        db.sql("CREATE TABLE bad (id INT PRIMARY KEY, body TEXT) OPTIONS(cluster_by = 'body')")


def test_rank_profiles(db):
    liked = "CREATE RANK PROFILE liked ON books AS SELECT * FROM books ORDER BY bm25(title, :q) * (1 + :w * rating) DESC WITH (w => 0.5)"
    assert db.sql(liked) == 0
    sql = "SELECT id, score() FROM books WHERE year >= 1950 ORDER BY rank_profile('liked', q => :q) LIMIT 5"
    written = "SELECT id, score() FROM books WHERE year >= 1950 ORDER BY bm25(title, :q) * (1 + {w} * rating) DESC LIMIT 5"
    r, want = db.query(sql, q="dragon"), db.query(written.format(w=0.5), q="dragon")
    assert len(r) == 5 and (r.keys, r.scores) == (want.keys, want.scores)
    r = db.query(sql.replace("q => :q", "q => :q, w => 0"), q="dragon")
    assert r.keys == db.query(written.format(w=0), q="dragon").keys
    # A plan names the profile, and a database whose profile has changed
    # since refuses it.
    plan = db.plan(sql, q="dragon")
    assert "rank_profiles" in plan["features"]
    assert db.run(plan).keys == db.query(sql, q="dragon").keys
    with pytest.raises(strata.StrataError, match="already"):
        db.sql(liked)
    assert db.sql(liked.replace("CREATE", "CREATE OR REPLACE").replace("w => 0.5", "w => 2")) == 0
    with pytest.raises(strata.StrataError, match="is not the one the plan was made with"):
        db.run(plan)
    assert db.query(sql, q="dragon").keys == db.query(written.format(w=2), q="dragon").keys
    assert db.sql("DROP RANK PROFILE liked") == 0
    with pytest.raises(strata.StrataError, match="books has no rank profile named liked"):
        db.query(sql, q="dragon")
    assert db.sql("DROP RANK PROFILE IF EXISTS liked ON books") == 0


MODELS = os.path.join(os.path.dirname(__file__), "..", "..", "strata", "tests", "models")


def model_dir(tmp_path, source="lgb_rank"):
    """A models directory with strata/tests/models' `source` as model m."""
    models = tmp_path / "models"
    (models / "m").mkdir(parents=True, exist_ok=True)
    shutil.copy(os.path.join(MODELS, source, "gbdt.json"), models / "m" / "gbdt.json")
    return models


def test_gbdt(tmp_path):
    models = model_dir(tmp_path)
    with strata.Database.create(tmp_path / "books.strata", BOOKS, models=str(models)) as db:
        db.insert([book(i) for i in range(200)])
        by = "gbdt('m', bm25(description, :q), rating, year % 6)"
        sql = f"SELECT id, score(), features() FROM books ORDER BY {by} DESC LIMIT 5"
        r = db.query(sql, q="dragon ship")
        assert len(r) == 5 and r.columns[-4:] == ["bm25(description, :q)", "rating", "year", by]
        assert [row[by] for row in r] == r.scores and r.scores == sorted(r.scores, reverse=True)
        # The builder ranks as the SQL does.
        q = db.table().select("id", score()).order_by(gbdt("m", bm25("description", "dragon ship"), col("rating"), col("year") % 6)).limit(5)
        got = db.run(q)
        assert (got.keys, got.scores) == (r.keys, r.scores)
        # A plan names the model's file; another file of it refuses the plan.
        plan = db.plan(sql, q="dragon ship")
        assert "rank_models" in plan["features"] and '"hash":"' in json.dumps(plan, separators=(",", ":"))
        assert db.run(plan).keys == r.keys
        model_dir(tmp_path, "xgb_rank")
        with pytest.raises(strata.StrataError, match="ranking model m here is not the one the plan was made with"):
            db.run(plan)
        assert db.query(sql, q="dragon ship").keys != r.keys or db.query(sql, q="dragon ship").scores != r.scores
        with pytest.raises(strata.StrataError, match="reads 3 features, not 2"):
            db.query("SELECT id FROM books ORDER BY gbdt('m', rating, year) DESC LIMIT 5")
        with pytest.raises(strata.StrataError, match="no ranking model named nope"):
            db.query("SELECT id FROM books ORDER BY gbdt('nope', rating, year, year) DESC LIMIT 5")


def test_cross_encoder(tmp_path):
    with strata.Database.create(tmp_path / "books.strata", BOOKS) as db:
        db.insert([book(i) for i in range(200)])
        ce = "cross_encoder('builtin:overlap', title, description, :q)"
        top = "WITH top AS (SELECT * FROM books ORDER BY bm25(description, :q) DESC LIMIT 30)"
        sql = f"{top} SELECT id, score(), features() FROM top ORDER BY {ce} DESC LIMIT 5"
        r = db.query(sql, q="dragon ship winter")
        assert len(r) == 5 and r.scores == sorted(r.scores, reverse=True) and r.scores[0] > 0
        # The model is no feature: features() lists the first ranking's.
        assert r.columns == ["id", "score()", "bm25(description, :q)"]
        # The builder ranks as the SQL does, in a global phase.
        q = db.table().select("id", score()).order_by(bm25("description", "dragon ship winter"))
        q = q.rerank(cross_encoder("builtin:overlap", ["title", "description"], "dragon ship winter"), 30).limit(5)
        assert q.plan()["root"]["input"]["input"]["phases"][0]["global"] is True
        got = db.run(q)
        assert (got.keys, got.scores) == (r.keys, r.scores)
        plan = db.plan(sql, q="dragon ship winter")
        assert "rank_models" in plan["features"] and '"cross_encoder":{"model":"builtin:overlap","hash":"builtin"' in json.dumps(plan, separators=(",", ":"))
        assert db.run(plan).keys == r.keys
        with pytest.raises(strata.StrataError, match="a cross-encoder reranks the top rows of a cheaper ranking"):
            db.query(f"SELECT id FROM books ORDER BY {ce} DESC LIMIT 5", q="dragon")
        with pytest.raises(strata.StrataError, match="no cross-encoder named nope"):
            db.query(f"{top} SELECT id FROM top ORDER BY cross_encoder('nope', title, :q) DESC LIMIT 5", q="dragon")


def test_the_rank_log(db):
    import datetime as dt

    db.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, item KEYWORD, action KEYWORD, ts TIMESTAMP)")
    t0 = dt.datetime(2026, 10, 1, tzinfo=dt.timezone.utc)
    sql = "SELECT id, title FROM books ORDER BY bm25(title, :q) * (1 + 0.1 * rating) DESC LIMIT 5"
    served = db.query("SELECT id, score(), features() FROM books ORDER BY bm25(title, :q) * (1 + 0.1 * rating) DESC LIMIT 8", q="dragon")
    r = db.query_logged(sql, {"q": "dragon"}, context="u1", variant="a", candidates=8, ts=t0)
    assert r.columns == ["id", "title"] and r.keys == served.keys[:5] and len(r.request) == 32
    db.insert([
        {"id": 1, "usr": "u1", "item": "b0002", "action": "view", "ts": t0 - dt.timedelta(minutes=5)},
        {"id": 2, "usr": "u1", "item": r.keys[0], "action": "buy", "ts": t0 + dt.timedelta(minutes=1)},
    ], table="events")
    # The ratings change after: the export keeps what was served.
    db.sql("UPDATE books SET rating = 5.0 WHERE year > 0")
    db.sql("UPDATE rank_log SET label = 2 WHERE request = :r AND row_key = :k", r=r.request, k=r.keys[1])
    records = db.export_rank_log(
        events={"table": "events", "user": "usr", "item": "item", "time": "ts", "action": "action"},
        history=5, window="10 minutes", grades={"buy": 3},
    )
    assert [x["row_key"] for x in records] == served.keys
    assert [x["shown"] for x in records] == [True] * 5 + [False] * 3
    want = [dict(zip(served.columns[2:], row[2:])) for row in served.rows]
    assert [x["features"] for x in records] == want
    assert [x["score"] for x in records] == served.scores
    assert [x["label"] for x in records] == [3, 2] + [0] * 6
    assert records[0]["history_items"] == ["b0002"] and records[0]["history_actions"] == ["view"]
    assert records[0]["context"] == "u1" and records[0]["variant"] == "a"
    assert len(db.export_rank_log(since=t0 + dt.timedelta(seconds=1))) == 0
    with pytest.raises(strata.StrataError, match="ORDER BY"):
        db.query_logged("SELECT id FROM books WHERE year > 2000")
