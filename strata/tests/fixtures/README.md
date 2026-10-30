# Files older versions of Strata wrote

Each directory here holds databases that a version of Strata wrote, kept
byte for byte so that every later version is tested on them. Never edit or
write over a file in a fixture set: a set is a record of what Strata wrote
once, and a database someone wrote with that version looks the same.

| set | format | written by | tested in |
|---|---|---|---|
| `format1/` | 1, one table | main 6b85211, before a database could hold several tables ([its README](format1/README.md)) | `tests/storage_tables.rs` |
| `format2/` | 2, two tables | 835ca9a | `tests/format_fixtures.rs` |
| `format3/` | 3, with `analyzer:simple:1`, `bool`, `default`, `not_null` and `timestamp` | 835ca9a | `tests/format_fixtures.rs` |
| `seqno/` | 3, with `seq_no`: numbered writes and rows, and a numbered WAL (`STRWAL02`) | c0d46c2 | `tests/format_fixtures.rs` |
| `profiles/` | 3, with `rank_profiles`: rank profiles in the manifest, and profile records in a WAL that names the feature (`STRWAL02`) | b43bfb2 | `tests/format_fixtures.rs` |

`format2/`, `format3/`, `seqno/` and `profiles/` each hold `db.strata` (with its WAL,
`db.strata-wal`) and `db.dir`: the same writes in the single-file and the
directory layout. [`writer.rs`](writer.rs) makes the writes:

- format 2: tables `t` and `u` (whose `parent` names rows of `t`), rows
  written and flushed, then deletes in both and `n` set in place on three
  rows of `t` (flushed again), and a transaction on both tables left in the
  WAL;
- format 3: a `tasks` table with a `simple` TEXT field, an `english` one,
  `BOOL`, `TIMESTAMP`, `NOT NULL` and `DEFAULT` fields, written and flushed,
  then deletes and `priority` set in place (flushed again), and a
  transaction left in the WAL;
- numbered writes: format 2's table `t` with the same rows, its first write
  made before numbering started (`Database::start_seq_no`) and the rest
  under term 2 (`Database::begin_term`), numbered 1 to 5, the last left in
  the WAL; each row keeps the number of its last write (in a segment's
  section `seq` and beside values set in place), 0 for rows from before;
- rank profiles: format 2's writes, with two profiles of `t` (`by_n`, and
  `phased` with rerank phases and defaults) flushed into the manifest with
  the deletes, and logged in the WAL before its transaction a profile of
  `u` (`notes`), `by_n` replaced and `phased` dropped.

`tests/format_fixtures.rs` opens copies of every set in both layouts with
the current code, checks their rows, filters and searches (`simple` against
`english`, `BOOL` and `TIMESTAMP` filters, defaults and `NOT NULL` on new
rows), writes, flushes and merges, checks that each stays in its format and
opens it again. A numbered set must also go on counting from its last
number and give each row the number of its last write, and the profiles
set must rank as its profiles say and go back to format 2 once they are
dropped.

It also writes the sets again with `writer.rs` and checks that the bytes
are the same, but for the database id (in the single file's meta pages,
`CURRENT` and the WAL header). The writes are deterministic, so a failure
there means Strata now writes something else for the same writes. If that
is deliberate, keep the old set and add the new one: move `format3/` to
`format3-<commit that wrote it>/` (the tests open every `format3*` set),
write a new `format3/` with the current code, from `strata/`:

```bash
cargo run --release --example format_fixtures -- /tmp/fixtures
cp -r /tmp/fixtures/format3 tests/fixtures/format3
```

and add a row to the table above. Do not change the writes in `writer.rs`:
the tests expect every set of a format to hold the same rows. A new format,
or a feature the sets do not cover, gets a new function there, a set with a
name of its own and its own test.
