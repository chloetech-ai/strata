# A database whose keyword columns lost their offsets

`t.strata` (with `t.strata-wal`) was written by Strata at main 5e2687c with
the program below: 70 documents, flushed into one segment.
`tests/forward_keyword.rs` opens a copy with the current code.

Up to that commit, a keyword column whose rare values (fewer than 64
documents each) held, together, as many documents as the column had values
was written as if every value had exactly one document: the rare values'
offsets were left out, and each value was read as one document of that
array. Every column here has that shape: `tag` (single-valued: `common` on
67 documents, `one` on 1, `two` on 2), `tags` (multi-valued KEYWORD: `all`
on every document, `x` on 2) and `sizes` (multi-valued INT: 5 on every
document, 9 on 2). With the code of that commit, `tag = 'common'` matches
1 document and `tag = 'two'` matches 1.

To write it again, check out 5e2687c, save the program as
`strata/examples/keyword_offsets_fixture.rs` and run, from `strata/`:

```bash
cargo run --release --example keyword_offsets_fixture -- /tmp/keyword_offsets
```

```rust
//! Writes the keyword-offsets fixture for strata/tests/forward_keyword.rs
//! with the code on main before the fix (5e2687c).
use std::path::Path;
use strata::db::Db;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value};

fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("tag", FieldType::Keyword),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("sizes", FieldType::Int).multi(),
        ],
        Some("id"),
    )
    .unwrap()
}

/// Each column: one value on every other document (a Pyramid list) and
/// rare values with as many documents, together, as the column has values.
fn doc(i: usize) -> Vec<Value> {
    let tag = match i {
        0 => "one",
        1 | 2 => "two",
        _ => "common",
    };
    let tags = match i {
        10 => vec!["x", "all"],
        20 => vec!["all", "x"],
        _ => vec!["all"],
    };
    let sizes = match i {
        30 => vec![9, 5],
        40 => vec![5, 9],
        _ => vec![5],
    };
    vec![
        Value::Str(format!("k{i:03}")),
        Value::Str(tag.into()),
        Value::List(tags.into_iter().map(|t| Value::Str(t.into())).collect()),
        Value::List(sizes.into_iter().map(Value::Int).collect()),
    ]
}

fn main() {
    let out = Path::new(&std::env::args().nth(1).unwrap()).to_path_buf();
    std::fs::create_dir_all(&out).unwrap();
    let db = Db::create(&out.join("t.strata"), schema(), DbOptions { sync: true, ..DbOptions::default() }).unwrap();
    let mut t = db.storage().begin().unwrap();
    for i in 0..70 {
        t.insert(doc(i)).unwrap();
    }
    t.commit().unwrap();
    db.storage().flush().unwrap();
}
```
