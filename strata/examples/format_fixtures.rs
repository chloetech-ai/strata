//! Writes the fixtures of formats 2 and 3 and of numbered writes that
//! `tests/format_fixtures.rs` opens with every later version of Strata
//! (`tests/fixtures/README.md`):
//!
//! ```bash
//! cargo run --release --example format_fixtures -- /tmp/fixtures
//! ```

#[path = "../tests/fixtures/writer.rs"]
mod writer;

fn main() {
    let out = std::env::args().nth(1).expect("the directory to write the fixtures in");
    writer::write_all(std::path::Path::new(&out));
}
