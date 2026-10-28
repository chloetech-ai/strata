# Files from before tables

`one.strata` (with `one.strata-wal`) and `one.dir` were written by Strata at
main 6b85211, before a database could hold several tables, with the program
below. `tests/storage_tables.rs` opens copies of them with the current code,
checks their rows, writes, flushes and merges, and checks that they stay in
format 1.

Each holds 30 rows written and flushed, 5 of them deleted and 3 with `n` set
in place (flushed again), and a transaction left in the WAL: 10 inserts, a
delete and an upsert.

To write them again, check out a commit from before tables, save the program
as `strata/examples/format1_fixture.rs` and run, from `strata/`:

```bash
cargo run --release --example format1_fixture -- /tmp/format1
```

```rust
//! Writes the format-1 fixtures for strata/tests/storage_tables.rs with
//! the code on main (before tables).
use std::path::Path;
use strata::storage::{Database, DbOptions, Layout};
use strata::{Field, FieldType, Schema, Value};

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![Field::new("id", FieldType::Keyword), Field::new("n", FieldType::Int), Field::new("body", FieldType::Text { positions: false })],
        Some("id"),
    )
    .unwrap()
}

fn doc(i: u64, v: i64) -> Vec<Value> {
    vec![Value::Str(format!("k{i:06}")), Value::Int(v), Value::Str(format!("document {i} version {v} lorem ipsum"))]
}

fn write(path: &Path, layout: Layout) {
    let db = Database::create(path, schema(), DbOptions { layout, sync: true, ..DbOptions::default() }).unwrap();
    let mut t = db.begin().unwrap();
    for i in 0..30 {
        t.insert(doc(i, 0)).unwrap();
    }
    t.commit().unwrap();
    db.flush().unwrap();
    let mut t = db.begin().unwrap();
    for i in 0..5 {
        t.delete(&Value::Str(format!("k{i:06}"))).unwrap();
    }
    for i in 5..8 {
        t.set(&Value::Str(format!("k{i:06}")), "n", Value::Int(7)).unwrap();
    }
    t.commit().unwrap();
    db.flush().unwrap();
    // Left in the WAL.
    let mut t = db.begin().unwrap();
    for i in 30..40 {
        t.insert(doc(i, 1)).unwrap();
    }
    t.delete(&Value::Str("k000010".into())).unwrap();
    t.upsert(doc(11, 2)).unwrap();
    t.commit().unwrap();
}

fn main() {
    let out = Path::new(&std::env::args().nth(1).unwrap()).to_path_buf();
    std::fs::create_dir_all(&out).unwrap();
    write(&out.join("one.strata"), Layout::SingleFile);
    write(&out.join("one.dir"), Layout::Directory);
}
```
