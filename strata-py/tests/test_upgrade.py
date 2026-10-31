"""strata.upgrade copies a database written before field ids, which no
longer opens, into this version's format in its place."""

import pathlib
import shutil

import pytest

import strata

# Two tables (t and u) written by Strata at 835ca9a: strata/tests/fixtures/README.md.
FIXTURE = pathlib.Path(__file__).resolve().parents[2] / "strata" / "tests" / "fixtures" / "format2"


def test_upgrade_copies_a_database_from_before_field_ids(tmp_path):
    for name in ["db.strata", "db.strata-wal"]:
        shutil.copy(FIXTURE / name, tmp_path / name)
    path = tmp_path / "db.strata"
    with pytest.raises(strata.StrataError, match="before field ids"):
        strata.Database.open(path)

    old, tables, rows = strata.upgrade(path)
    assert pathlib.Path(old) == tmp_path / "db.strata.before-field-ids"
    # t holds keys 5 to 39 but 10, and u ids 0 to 8 and 10.
    assert (tables, rows) == (2, 44)
    db = strata.Database.open(path)
    assert db.query("SELECT COUNT(*) FROM t").rows == [[34]]
    assert db.query("SELECT COUNT(*) FROM u WHERE MATCH(note, 'run')").rows == [[10]]
    db.close()
    assert strata.upgrade(path) is None
