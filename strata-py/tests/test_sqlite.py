import sqlite3
import struct

import pytest

import strata


def shop(path):
    c = sqlite3.connect(path)
    c.executescript(
        """
        CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL, born DATE);
        CREATE TABLE books (
            isbn TEXT PRIMARY KEY,
            title TEXT,
            author INTEGER REFERENCES authors(id),
            price NUMERIC,
            in_print BOOLEAN DEFAULT 1,
            tags JSON,
            emb F32_BLOB(2),
            cover BLOB
        );
        CREATE VIRTUAL TABLE books_fts USING fts5(title, content='books', tokenize='porter');
        CREATE VIEW cheap AS SELECT * FROM books WHERE price < 10;
        INSERT INTO authors VALUES (1, 'Ursula Le Guin', '1929-10-21'), (2, 'Italo Calvino', '1923-10-15');
        """
    )
    rows = [
        ("isbn-1", "A Wizard of Earthsea", 1, 8.5, 1, '["fantasy", "classic"]', struct.pack("<2f", 1.0, 0.0), b"\x89PNG"),
        ("isbn-2", "Invisible Cities", 2, 12, 0, '["fable"]', struct.pack("<2f", 0.0, 1.0), None),
    ]
    c.executemany("INSERT INTO books VALUES (?, ?, ?, ?, ?, ?, ?, ?)", rows)
    c.execute("INSERT INTO books_fts (books_fts) VALUES ('rebuild')")
    c.commit()
    c.close()


def test_from_sqlite_and_back(tmp_path):
    src = tmp_path / "shop.sqlite"
    shop(src)
    plan = strata.plan_sqlite(src)
    assert [t["table"] for t in plan["tables"]] == ["authors", "books"]
    assert set(plan["skipped"]) == {"books_fts", "cheap"}
    books = plan["tables"][1]
    assert {f["name"]: f["type"] for f in books["fields"]} == {
        "isbn": "keyword", "title": "text", "author": "int", "price": "float", "in_print": "bool", "tags": "keyword", "emb": "vector",
    }
    assert any("cover" in n for n in books["notes"])

    with strata.from_sqlite(src, tmp_path / "shop.strata") as db:
        assert [(t["table"], t["rows"]) for t in db.last_import["tables"]] == [("authors", 2), ("books", 2)]
        assert db.query("SELECT isbn FROM books WHERE MATCH(title, 'wizards')").scalar() == "isbn-1"
        assert db.query("SELECT isbn FROM books WHERE in_print = false").scalar() == "isbn-2"
        assert db.query("SELECT isbn FROM books WHERE tags = 'classic'").scalar() == "isbn-1"
        assert db.query("SELECT name FROM authors WHERE born < '1925-01-01'").scalar() == "Italo Calvino"
        out = tmp_path / "copy.sqlite"
        done = db.export_sqlite(out)
        assert [(t["table"], t["rows"], t["fts"]) for t in done["tables"]] == [("authors", 2, None), ("books", 2, "books_fts")]
        with pytest.raises(strata.StrataError, match="exists"):
            db.export_sqlite(out)

    c = sqlite3.connect(out)
    assert c.execute("SELECT born FROM authors WHERE id = 1").fetchone() == ("1929-10-21 00:00:00",)
    assert c.execute("SELECT isbn FROM books_fts JOIN books ON books.rowid = books_fts.rowid WHERE books_fts MATCH 'cities'").fetchall() == [("isbn-2",)]
    assert struct.unpack("<2f", c.execute("SELECT emb FROM books WHERE isbn = 'isbn-2'").fetchone()[0]) == (0.0, 1.0)
    assert c.execute("SELECT b.title FROM books b JOIN authors a ON a.id = b.author WHERE a.name LIKE 'Italo%'").fetchone() == ("Invisible Cities",)
    c.close()


def test_import_sqlite_into_a_database(tmp_path):
    src = tmp_path / "shop.sqlite"
    shop(src)
    schema = {"table": "books", "key": "isbn", "fields": [{"name": "isbn", "type": "keyword"}, {"name": "title", "type": "text"}]}
    with strata.open(tmp_path / "b.strata", schema=schema) as db:
        db.insert({"isbn": "isbn-9", "title": "Already here"})
        r = db.import_sqlite(src, tables="books", blobs_base64=True)
        (books,) = r["tables"]
        assert (books["table"], books["rows"], books["created"], books["bulk"]) == ("books", 2, False, False)
        assert any('"price" left out' in n for n in books["notes"])
        assert len(db) == 3
        # A given schema for a new table, from an edited plan.
        plan = strata.plan_sqlite(src, tables=["authors"])
        plan["tables"][0]["fields"][1]["type"] = "text"
        db.import_sqlite(src, tables=["authors"], schemas=plan)
        assert db.query("SELECT id FROM authors WHERE MATCH(name, 'calvino')").scalar() == 2
