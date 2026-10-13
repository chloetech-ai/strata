//! Helpers shared by the storage tests.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use strata::error::corrupt;
use strata::segment::{DocId, SegmentFile};
use strata::storage::{Database, DbOptions, DocumentSource, PkIndex, Snapshot};
use strata::{Document, Field, FieldType, Result, Schema, Value};

pub fn schema() -> Schema {
    Schema::new(
        "t",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("n", FieldType::Int),
            Field::new("body", FieldType::Text { positions: false }),
        ],
        Some("id"),
    )
    .unwrap()
}

pub fn key(i: u64) -> String {
    format!("k{i:06}")
}

/// Document for key `i`, version `v`.
pub fn doc(i: u64, v: i64) -> Document {
    vec![Value::Str(key(i)), Value::Int(v), Value::Str(format!("document {i} version {v} lorem ipsum"))]
}

/// A doc store stand-in: the latest document written for each key.
#[derive(Default)]
pub struct MapSource {
    pub docs: Mutex<HashMap<String, Document>>,
    /// Runs once, inside the next `documents` call (to write while a merge
    /// is between its snapshot and its commit).
    pub hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl MapSource {
    pub fn put(&self, d: &Document) {
        self.docs.lock().unwrap().insert(d[0].as_str().unwrap().to_string(), d.clone());
    }
}

impl DocumentSource for MapSource {
    fn documents(&self, _schema: &Schema, seg: &SegmentFile, docs: &[DocId]) -> Result<Vec<Document>> {
        if let Some(h) = self.hook.lock().unwrap().take() {
            h();
        }
        let pk = PkIndex::open(seg)?.ok_or_else(|| corrupt("no pk"))?;
        let m = self.docs.lock().unwrap();
        docs.iter()
            .map(|&d| {
                let k = pk.key(d);
                m.get(k.as_str().unwrap()).cloned().ok_or_else(|| corrupt(format!("source lacks {k:?}")))
            })
            .collect()
    }
}

pub fn opts(source: &Arc<MapSource>) -> DbOptions {
    DbOptions { doc_source: Some(source.clone()), sync: false, ..DbOptions::default() }
}

/// Live keys of a snapshot with how often each is live (must be once).
pub fn live_keys(s: &Snapshot) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for (i, seg) in s.segments().iter().enumerate() {
        let pk = s.pk(i).expect("pk index");
        for d in 0..seg.file.n_docs() as DocId {
            if seg.live.as_ref().is_none_or(|l| l.contains(d)) {
                *out.entry(pk.key(d).as_str().unwrap().to_string()).or_insert(0) += 1;
            }
        }
    }
    out
}

/// Checks a snapshot against the expected live keys: each live once,
/// counts agree, and lookups find exactly the live keys.
pub fn assert_keys(s: &Snapshot, want: &std::collections::BTreeSet<String>) {
    let got = live_keys(s);
    let dups: Vec<_> = got.iter().filter(|(_, &c)| c != 1).collect();
    assert!(dups.is_empty(), "keys live more than once: {dups:?}");
    let got: std::collections::BTreeSet<String> = got.into_keys().collect();
    assert_eq!(&got, want);
    assert_eq!(s.n_live_docs(), want.len() as u64);
    let segs_live: u64 = s.segments().iter().map(|v| v.n_live as u64).sum();
    assert_eq!(segs_live, want.len() as u64);
    for k in want.iter().take(50) {
        let (seg, d) = s.lookup(&Value::Str(k.clone())).unwrap_or_else(|| panic!("lookup {k}"));
        assert_eq!(s.key(seg, d).unwrap(), Value::Str(k.clone()));
    }
}

/// Opens or creates `path` with `opts`.
pub fn open_or_create(path: &std::path::Path, opts: DbOptions) -> Database {
    if path.exists() {
        Database::open(path, opts).unwrap()
    } else {
        Database::create(path, schema(), opts).unwrap()
    }
}
