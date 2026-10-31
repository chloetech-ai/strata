# An Engram store from before field ids

`m.strata` (with its WAL, `m.strata-wal`) is an Engram store written by
Engram and Strata at 6854657, the last commit before Strata named fields by
id, with the program below and `HashEmbedder { dim: 32 }`: 12 turns of
three sessions, each a prompt, an answer naming a file and a decision about
one of three entities (so the store has entity records and links), the
first eight turns flushed into a segment and the rest left in the WAL. The
records are made up; no session wrote them.

`tests/store_upgrade.rs` opens a copy with `Store::open`, which copies it
into the current format, and checks that every row and vector is as
written. Never edit or write over these files.

To write them again, check out 6854657, save the program as
`engram/examples/old_store.rs` and run, from `engram/`:

```bash
cargo run --release --example old_store -- /tmp/store/m.strata
```

```rust
//! Writes an Engram store with the Strata of this commit (before field
//! ids), for engram/tests/fixtures/store-before-field-ids.
use std::sync::Arc;

use engram::embed::HashEmbedder;
use engram::record::{Draft, Kind};
use engram::store::{Store, StoreOptions};

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("the store to write"));
    let s = Store::open(&out, Arc::new(HashEmbedder { dim: 32 }), StoreOptions { sync: false, ..Default::default() }).unwrap();
    let topics = [("refund", "Refunds are allowed within 14 days of purchase"), ("export", "The export file uses semicolons as separators"), ("deploy", "Deploys happen on Tuesdays only")];
    for turn in 0..12i64 {
        let session = ["s1", "s2", "s3"][turn as usize % 3];
        let (entity, fact) = topics[turn as usize % 3];
        let drafts = vec![
            Draft::new(Kind::Prompt, format!("turn {turn}: what do we know about {entity}?")),
            Draft::new(Kind::Answer, format!("turn {turn}: {fact}")).files(vec![format!("src/{entity}.rs")]),
            Draft::new(Kind::Decision, format!("{fact} (decided in turn {turn})")).tag("distilled").entities(vec![entity.into()]),
        ];
        s.append(session, turn, 1_790_812_800 + turn * 60, drafts).unwrap();
        if turn == 7 {
            s.flush().unwrap();
        }
    }
}
```
