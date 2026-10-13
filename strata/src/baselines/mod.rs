//! Baselines for the evaluation (DESIGN.md §8): SQLite (FTS5, B-tree
//! indexes, vector blobs) and Tantivy, behind the same
//! [`System`](crate::workload::run::System) interface as Strata, running the
//! same [`workload`](crate::workload). Built with the `baselines` feature.

pub mod sqlite;
pub mod tantivy_index;

pub use sqlite::Sqlite;
pub use tantivy_index::{Tantivy, TantivyOptions};

use std::path::{Path, PathBuf};

/// Every regular file under `dir`, recursively, sorted.
pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.is_file() {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// Size of a file in bytes (0 if it does not exist).
pub fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}
