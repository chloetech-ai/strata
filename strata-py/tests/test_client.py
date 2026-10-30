import json

import pytest

import strata
from conftest import BOOKS, book


@pytest.fixture
def client(tmp_path, serve):
    path = tmp_path / "served.strata"
    with strata.Database.create(path, BOOKS) as db:
        db.insert([book(i) for i in range(200)])
    return strata.connect(serve(path))


def test_client_reads_and_writes(client):
    assert len(client) == 200
    assert client.schema["table"] == "books" and client.schema["fields"][0] == {"name": "id", "type": "keyword"}
    r = client.query("SELECT id, year FROM books WHERE year >= :y ORDER BY year DESC LIMIT 3", y=2000)
    assert len(r) == 3 and r[0]["year"] >= 2000 and r.took_ms is not None
    assert client.sql("SELECT COUNT(*) FROM books").scalar() == 200
    plan = client.explain("SELECT id FROM books WHERE year >= :y LIMIT 3", y=2000)
    assert plan["tree"] == ["Project: id", "  Limit: 3", "    Search books", "      filter: year >= 2000"]
    assert plan["ir"]["root"]["node"] == "project" and plan["ir"]["version"] == 1
    assert client.run(plan["ir"]).keys == client.query("SELECT id FROM books WHERE year >= 2000 LIMIT 3").keys
    sql = "SELECT id FROM books WHERE year > (SELECT AVG(year) FROM books) ORDER BY year DESC LIMIT 5"
    plan = client.explain(sql)
    assert plan["ir"]["features"] == ["operators"]
    assert len(client.run(plan["ir"]).keys) == 5 and client.run(plan["ir"]).keys == client.query(sql).keys
    assert client.insert([book(i) for i in range(300, 2300)]) == 2000
    assert client.upsert({**book(1), "title": "Changed"}) == 1
    assert client.sql("UPDATE books SET year = 1 WHERE id = :k", k="b0002") == 1
    assert client.delete(["b0003", "b0004"]) == 2
    assert len(client) == 2198
    got = client.query("SELECT title FROM books WHERE id = 'b0001'").scalar()
    assert got == "Changed"
    with pytest.raises(strata.StrataError, match="HTTP 400") as e:
        client.query("SELECT nope FROM books")
    assert e.value.status == 400


def test_client_rank_profiles(client):
    assert client.sql("CREATE RANK PROFILE liked ON books AS SELECT * FROM books ORDER BY bm25(title, :q) * (1 + :w * rating) DESC WITH (w => 0.5)") == 0
    sql = "SELECT id, score() FROM books ORDER BY rank_profile('liked', q => :q) LIMIT 5"
    r = client.query(sql, q="dragon")
    want = client.query("SELECT id, score() FROM books ORDER BY bm25(title, :q) * (1 + 0.5 * rating) DESC LIMIT 5", q="dragon")
    assert len(r) == 5 and (r.keys, r.scores) == (want.keys, want.scores)
    plan = client.explain(sql, q="dragon")["ir"]
    assert "rank_profiles" in plan["features"] and client.run(plan).keys == r.keys
    assert client.sql("CREATE OR REPLACE RANK PROFILE liked ON books AS SELECT * FROM books ORDER BY rating DESC") == 0
    with pytest.raises(strata.StrataError, match="is not the one the plan was made with"):
        client.run(plan)
    assert client.sql("DROP RANK PROFILE liked ON books") == 0

def test_client_writes_to_any_table(client, tmp_path):
    assert client.sql("CREATE TABLE events (id INT PRIMARY KEY, usr KEYWORD NOT NULL, book KEYWORD REFERENCES books, ts INT) OPTIONS(shard_by = 'usr')") == 0
    assert [t["table"] for t in client.tables()] == ["books", "events"]
    assert client.tables()[1]["shard_by"] == "usr"
    assert client.insert([{"id": i, "usr": f"u{i % 3}", "book": book(i)["id"], "ts": i} for i in range(1500)], table="events") == 1500
    assert client.upsert({"id": 1, "usr": "u1", "book": "b0007", "ts": 9}, table="events") == 1
    assert client.delete([2, 3], table="events") == 2
    f = tmp_path / "more.csv"
    f.write_text("id,usr,ts\n5000,u9,1\n5001,u9,2\n")
    assert client.import_file(f, table="events") == {"rows": 2, "bulk": False, "skipped_fields": []}
    assert client.query("SELECT COUNT(*) FROM events").scalar() == 1500
    assert len(client.table("events").select("id").where(strata.col("usr") == "u9").run()) == 2
    with pytest.raises(strata.StrataError, match="no such table: nope"):
        client.insert({"id": 1}, table="nope")
    assert client.sql("DROP TABLE events") == 0
    assert [t["table"] for t in client.tables()] == ["books"]


def test_client_search_and_import(client, tmp_path):
    assert client.search("dragon", mode="text", limit=4).keys
    assert len(client.search("dragon", limit=4)) == 4  # hybrid: the server embeds the text
    f = tmp_path / "more.csv"
    f.write_text("id,title,year\nz1,Zebra,2020\nz2,Zulu,2021\n")
    assert client.import_file(f) == {"rows": 2, "bulk": False, "skipped_fields": []}
    j = tmp_path / "more.jsonl"
    j.write_text(json.dumps({"id": "z3", "title": "Zed", "extra": 1}) + "\n")
    assert client.import_file(j)["skipped_fields"] == ["extra"]
    assert client.query("SELECT id FROM books WHERE MATCH(title, 'zebra')").keys == ["z1"]


def test_client_builder_and_embed(client, tmp_path):
    """A built query runs on the server as its SQL does; /v1/embed gives the
    vector the server's model gives a text, the local model's too."""
    from strata import bm25, col, count, similarity

    t = client.table()
    q = t.select("id", "year").where((col("year") >= 1950) & ~col("genres").isin(["classic"])).order_by(bm25("description", "dragon ship")).limit(5)
    want = client.query("SELECT id, year FROM books WHERE year >= 1950 AND NOT (genres IN ('classic')) ORDER BY bm25(description, 'dragon ship') DESC LIMIT 5")
    assert client.run(q).rows == want.rows and [r["id"] for r in q] == want.keys
    g = t.select("genres", count().alias("n")).group_by("genres").having(count() > 50).order_by(col("genres"))
    assert client.run(g).rows == client.query("SELECT genres, COUNT(*) AS n FROM books GROUP BY genres HAVING COUNT(*) > 50 ORDER BY genres").rows
    v = client.embed("dragon ship")
    assert len(v) == 32 and client.embed("dragon ship", "emb") == v
    with strata.Database.create(tmp_path / "local.strata", BOOKS) as local:
        assert local.embed("dragon ship", "emb") == pytest.approx(v)
    near = client.run(t.select("id").order_by(similarity("emb", v)).limit(3))
    assert near.rows == client.query("SELECT id FROM books ORDER BY similarity(emb, :v) DESC LIMIT 3", v={"embed": "dragon ship"}).rows
    with pytest.raises(strata.StrataError, match="HTTP 400"):
        client.embed("x", "title")
    a = client.search("dragon", limit=5, where=col("year") >= 1950)
    b = client.search("dragon", limit=5, where="year >= 1950")
    assert a.keys == b.keys and len(a) == 5


def test_client_token(tmp_path, serve):
    path = tmp_path / "t.strata"
    strata.Database.create(path, BOOKS).close()
    url = serve(path, "--token", "s3cret")
    with pytest.raises(strata.StrataError) as e:
        strata.connect(url, token="wrong").query("SELECT id FROM books")
    assert e.value.status == 401
    assert len(strata.connect(url, token="s3cret")) == 0


def test_client_bool_and_timestamp(tmp_path, serve):
    from datetime import datetime, timezone

    from test_database import TASKS

    path = tmp_path / "tasks.strata"
    strata.Database.create(path, TASKS).close()
    client = strata.connect(serve(path))
    assert client.insert([
        {"id": "a", "done": True, "at": datetime(2026, 10, 1, 9, 0, tzinfo=timezone.utc), "n": 1},
        {"id": "b", "done": False, "at": "2026-10-02", "n": 2},
    ]) == 2
    r = client.query("SELECT id, done, at FROM tasks WHERE at >= :t ORDER BY n", t=datetime(2026, 10, 1))
    assert [list(row.values()) for row in r] == [["a", True, "2026-10-01T09:00:00Z"], ["b", False, "2026-10-02T00:00:00Z"]]
    assert client.query("SELECT id FROM tasks WHERE done = :d", d=False).keys == ["b"]
    with pytest.raises(strata.StrataError, match="HTTP 400"):
        client.insert({"id": "x", "done": 2})


def test_client_gbdt(tmp_path, serve):
    from test_database import model_dir
    models = model_dir(tmp_path)
    path = tmp_path / "models.strata"
    with strata.Database.create(path, BOOKS) as db:
        db.insert([book(i) for i in range(200)])
    client = strata.connect(serve(path, "--models", str(models)))
    sql = "SELECT id, score() FROM books ORDER BY gbdt('m', bm25(description, :q), rating, year % 6) DESC LIMIT 5"
    r = client.query(sql, q="dragon ship")
    assert len(r) == 5
    q = client.table().select("id", strata.score()).order_by(strata.gbdt("m", strata.bm25("description", "dragon ship"), strata.col("rating"), strata.col("year") % 6)).limit(5)
    got = client.run(q)
    assert (got.keys, got.scores) == (r.keys, r.scores)
    plan = client.explain(sql, q="dragon ship")
    assert "rank_models" in plan["ir"]["features"] and any(line.startswith("ranking model: m (hash ") for line in plan["tree"])
    assert client.run(plan["ir"]).keys == r.keys
    model_dir(tmp_path, "xgb_rank")
    with pytest.raises(strata.StrataError, match="is not the one the plan was made with"):
        client.run(plan["ir"])


def test_client_cross_encoder(tmp_path, serve):
    path = tmp_path / "ce.strata"
    with strata.Database.create(path, BOOKS) as db:
        db.insert([book(i) for i in range(200)])
    client = strata.connect(serve(path, "--max-rerank", "20"))
    top = "WITH top AS (SELECT * FROM books ORDER BY bm25(description, :q) DESC LIMIT 20)"
    sql = f"{top} SELECT id, score() FROM top ORDER BY cross_encoder('builtin:overlap', title, description, :q) DESC LIMIT 5"
    r = client.query(sql, q="dragon ship winter")
    assert len(r) == 5 and r.scores[0] > 0
    q = client.table().select("id", strata.score()).order_by(strata.bm25("description", "dragon ship winter"))
    got = client.run(q.rerank(strata.cross_encoder("builtin:overlap", ["title", "description"], "dragon ship winter"), 20).limit(5))
    assert (got.keys, got.scores) == (r.keys, r.scores)
    with pytest.raises(strata.StrataError, match="at most 20 rows"):
        client.query(sql.replace("LIMIT 20", "LIMIT 21"), q="dragon ship winter")


def test_client_streams_events_once(client):
    client.sql("CREATE TABLE views (id INT PRIMARY KEY, book KEYWORD, ts INT)")
    client.sql("CREATE TABLE counts (book KEYWORD PRIMARY KEY, n INT DEFAULT 0)")
    client.insert([{"book": book(i)["id"]} for i in range(10)], table="counts")

    def events(lo, hi):
        for i in range(lo, hi):
            b = book(i % 10)["id"]
            yield {
                "source": {"name": "views/0", "offset": i, "ts": 1_700_000_000_000 + i},
                "ops": [
                    {"op": "insert", "table": "views", "doc": {"id": i, "book": b, "ts": i}},
                    {"op": "add", "table": "counts", "key": b, "field": "n", "value": 1},
                ],
            }

    assert client.source("views/0") is None
    done = client.stream(events(0, 3000))
    assert (done["done"], done["lines"], done["written"], done["skipped"]) == (True, 3000, 6000, 0)
    assert client.source("views/0") == 2999
    # Resumed from before the offset kept: what was written is skipped.
    done = client.stream(events(2500, 4000))
    assert (done["written"], done["skipped"]) == (2000, 1000)
    assert client.query("SELECT COUNT(*) FROM views").scalar() == 4000
    assert client.query("SELECT SUM(n) FROM counts").scalar() == 4000
    assert [s["name"] for s in client.sources()] == ["views/0"]
    # A replay alone writes nothing; the same offset twice in one stream
    # is refused, after the lines before it.
    assert client.stream(events(10, 20))["skipped"] == 20
    twice = [*events(4000, 4001), *events(4000, 4001)]
    with pytest.raises(strata.StrataError, match="offset 4000 comes after 4000") as e:
        client.stream(twice)
    assert e.value.answer["lines"] == 1
    assert client.source("views/0") == 4000
