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
