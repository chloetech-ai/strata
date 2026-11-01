"""Answers as Arrow columns (`Database.query_arrow`): typed as the
database types them, the same values `query` gives, and handed to pyarrow,
polars and pandas without a copy."""

import datetime

import pytest

import strata

pa = pytest.importorskip("pyarrow")

ALL = {
    "table": "t",
    "key": "id",
    "fields": [
        {"name": "id", "type": "keyword"},
        {"name": "body", "type": "text"},
        {"name": "n", "type": "int"},
        {"name": "x", "type": "float"},
        {"name": "ok", "type": "bool"},
        {"name": "at", "type": "timestamp"},
        {"name": "v", "type": "vector", "dim": 3},
        {"name": "tags", "type": "keyword", "multi": True},
        {"name": "nums", "type": "int", "multi": True},
        {"name": "j", "type": "json"},
    ],
}


def row(i):
    if i % 10 == 9:
        return {"id": f"r{i:04d}", "n": i % 17}
    return {
        "id": f"r{i:04d}",
        "body": f"word{i % 5} river",
        "n": i % 17,
        "x": i / 4,
        "ok": i % 2 == 0,
        "at": f"2026-10-{1 + i % 28:02d}T08:00:00Z",
        "v": [float(i), 0.5, -1.0],
        "tags": [f"t{i % 3}", f"t{i % 4}"],
        "nums": [i, i * 2],
        "j": {"a": i, "b": f"x{i % 3}"},
    }


@pytest.fixture
def tdb(tmp_path):
    with strata.Database.create(tmp_path / "t.strata", ALL) as d:
        d.insert([row(i) for i in range(5000)])
        yield d


def same_as_query(db, sql, **kw):
    """The Arrow answer to `sql` holds what `query` answers."""
    want = db.query(sql, **kw)
    got = db.query_arrow(sql, **kw)
    assert got.columns == want.columns and len(got) == len(want) == got.num_rows
    t = got.to_pyarrow()
    assert t.column_names == want.columns
    for i, name in enumerate(want.columns):
        values = t.column(i).to_pylist()
        expected = [r[i] for r in want.rows]
        if pa.types.is_timestamp(t.schema.field(i).type):
            values = [None if v is None else v.strftime("%Y-%m-%dT%H:%M:%SZ") for v in values]
        if pa.types.is_floating(t.schema.field(i).type):
            expected = [None if v is None else float(v) for v in expected]
        if pa.types.is_list(t.schema.field(i).type) and pa.types.is_floating(t.schema.field(i).type.value_type):
            values = [None if v is None else [round(x, 5) for x in v] for v in values]
            expected = [None if v is None else [round(x, 5) for x in v] for v in expected]
        assert values == expected, f"{sql}: column {name}"
    return t


def test_every_type(tdb):
    t = same_as_query(tdb, "SELECT id, body, n, x, ok, at, v, tags, nums, j FROM t")
    # Read from the segments into Arrow, not made rows first.
    steps = tdb.query_arrow("SELECT id, body, n, x, ok, at, v, tags, nums, j FROM t").stats["steps"]
    assert [s["detail"] for s in steps if s["op"] == "project"] == ["10 columns into Arrow"]
    assert same_as_query(tdb, "SELECT * FROM t").column_names == [c for c in t.column_names if c != "v"]
    types = {f.name: f.type for f in t.schema}
    assert types["id"] == pa.string() and types["body"] == pa.string() and types["j"] == pa.string()
    assert types["n"] == pa.int64() and types["x"] == pa.float64() and types["ok"] == pa.bool_()
    assert types["at"] == pa.timestamp("us", tz="UTC")
    assert types["v"] == pa.list_(pa.float32())
    assert types["tags"] == pa.list_(pa.string()) and types["nums"] == pa.list_(pa.int64())
    assert t.num_rows == 5000
    assert t.column("at")[0].as_py() == datetime.datetime(2026, 10, 1, 8, tzinfo=datetime.timezone.utc)
    assert t.column("x").null_count == 500


def test_queries(tdb):
    for sql in [
        "SELECT id, n, x FROM t WHERE n > 3 ORDER BY x DESC LIMIT 100",
        "SELECT id, score() AS s FROM t ORDER BY bm25(body, 'word1') DESC LIMIT 20",
        "SELECT n, COUNT(*) AS c, AVG(x) AS m FROM t GROUP BY n",
        "SELECT a.id, b.id AS other, a.n FROM t a JOIN t b ON a.n = b.n AND a.x < b.x - 1200",
        "WITH big AS (SELECT id, n, x FROM t WHERE x > 600) SELECT n, COUNT(*) AS c, MAX(x) AS top FROM big GROUP BY n ORDER BY n",
        "SELECT id, tags, nums FROM t WHERE ok = TRUE LIMIT 50 OFFSET 10",
        "SELECT id FROM t WHERE n > 100",
    ]:
        same_as_query(tdb, sql)
    r = tdb.query_arrow("SELECT n, COUNT(*) AS c FROM t GROUP BY n")
    assert isinstance(r.stats, dict) and "plan" in r.stats
    assert repr(r) == "<strata.ArrowResult 17 rows: n, c>"


def test_no_rows_and_mixed_columns(tdb):
    r = tdb.query_arrow("SELECT id, n FROM t WHERE n > 100")
    assert len(r) == 0 and r.to_pyarrow().column_names == ["id", "n"]
    # A column of integers and floats is Float64; of numbers and text, an
    # error that says which.
    t = tdb.query_arrow("SELECT CASE WHEN n > 3 THEN n ELSE 0.5 END AS m FROM t LIMIT 20").to_pyarrow()
    assert t.schema.field("m").type == pa.float64()
    with pytest.raises(strata.StrataError, match="column m: a column of values of several types"):
        tdb.query_arrow("SELECT CASE WHEN n > 3 THEN n ELSE 'small' END AS m FROM t LIMIT 20")


def test_multi_vector_field(tmp_path):
    schema = {"table": "m", "key": "id", "fields": [{"name": "id", "type": "keyword"}, {"name": "tok", "type": "vector", "dim": 2, "multi": True}]}
    with strata.Database.create(tmp_path / "m.strata", schema) as db:
        db.insert([{"id": f"a{i}", "tok": [[1.0, 0.0], [0.0, float(i)]][: 1 + i % 2]} for i in range(4)] + [{"id": "none"}])
        t = same_as_query(db, "SELECT id, tok FROM m")
        assert t.schema.field("tok").type == pa.list_(pa.list_(pa.float32()))


def test_columns_are_shared_not_copied(tdb):
    r = tdb.query_arrow("SELECT id, n, x FROM t")
    a, b = pa.table(r), r.to_pyarrow()
    buffers = lambda t, c: [x.address for x in t.column(c).chunks[0].buffers() if x is not None]
    for c in ["id", "n", "x"]:
        assert buffers(a, c) == buffers(b, c)
    pd = pytest.importorskip("pandas")
    df = r.to_pandas()
    assert list(df.columns) == ["id", "n", "x"] and isinstance(df["n"].dtype, pd.ArrowDtype)
    assert df["n"].array._pa_array.chunks[0].buffers()[1].address == buffers(a, "n")[-1]
    assert df["n"].tolist() == a.column("n").to_pylist()
    df = r.to_pandas(arrow_dtypes=False)
    assert str(df["n"].dtype) == "int64" and len(df) == 5000


def test_polars(tdb):
    pl = pytest.importorskip("polars")
    r = tdb.query_arrow("SELECT id, n, x, ok, at, tags FROM t")
    df = r.to_polars()
    assert df.columns == ["id", "n", "x", "ok", "at", "tags"] and df.height == 5000
    assert df.schema["n"] == pl.Int64 and df.schema["at"] == pl.Datetime("us", "UTC") and df.schema["tags"] == pl.List(pl.String)
    assert df["n"].to_list() == tdb.query("SELECT n FROM t").column("n")
    # Polars reads the integers where the database wrote them.
    n = r.to_pyarrow().column("n").chunks[0].buffers()[1].address
    assert df["n"].to_arrow().buffers()[1].address == n
    assert pl.DataFrame(r).equals(df)
