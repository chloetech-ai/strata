//! Collection statistics that travel between shards and a coordinator.
//!
//! BM25 scores a document with the collection's document count, average
//! length and the document frequency of each query term. A shard that holds
//! part of a collection knows only its own part, so a coordinator first asks
//! every shard for the statistics of a query's terms ([`TermStats`]), sums
//! them, and sends the sum back with the query
//! ([`crate::query::exec::ExecOptions::stats`]). Each shard then scores
//! exactly as the unsplit collection would, and the coordinator merges
//! scores that are comparable ([`crate::cluster`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::text::CollectionStats;

/// Statistics of one TEXT field.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldStats {
    /// Documents with at least one term in the field (deleted ones
    /// included, as in a segment's own statistics).
    pub n_docs: u64,
    /// Sum of document lengths, in analysed terms.
    pub total_len: u64,
    /// Document frequency of each analysed term asked about.
    #[serde(default)]
    pub df: BTreeMap<String, u64>,
}

impl FieldStats {
    pub fn add(&mut self, other: &FieldStats) {
        self.n_docs += other.n_docs;
        self.total_len += other.total_len;
        for (t, n) in &other.df {
            *self.df.entry(t.clone()).or_insert(0) += n;
        }
    }

    pub fn from_collection(c: &CollectionStats) -> FieldStats {
        FieldStats { n_docs: c.n_docs, total_len: c.total_len, df: c.df.iter().map(|(t, n)| (t.clone(), *n)).collect() }
    }
}

/// Statistics of the TEXT fields a query scores with BM25, by field name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TermStats {
    pub fields: BTreeMap<String, FieldStats>,
}

impl TermStats {
    /// Adds another shard's statistics (the two must not share documents).
    pub fn add(&mut self, other: &TermStats) {
        for (f, s) in &other.fields {
            self.fields.entry(f.clone()).or_default().add(s);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The sum of several shards' statistics.
    pub fn sum<'a>(parts: impl IntoIterator<Item = &'a TermStats>) -> TermStats {
        let mut out = TermStats::default();
        for p in parts {
            out.add(p);
        }
        out
    }

    /// Records one field's statistics, keeping terms already present (the
    /// same field may be scored by several inputs of a query).
    pub(crate) fn merge_field(&mut self, field: &str, s: FieldStats) {
        let e = self.fields.entry(field.to_string()).or_default();
        e.n_docs = s.n_docs;
        e.total_len = s.total_len;
        e.df.extend(s.df);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_shards() {
        let a = TermStats { fields: [("body".into(), FieldStats { n_docs: 10, total_len: 100, df: [("cat".into(), 2)].into() })].into() };
        let b = TermStats {
            fields: [("body".into(), FieldStats { n_docs: 5, total_len: 40, df: [("cat".into(), 1), ("dog".into(), 3)].into() })].into(),
        };
        let s = TermStats::sum([&a, &b]);
        let f = &s.fields["body"];
        assert_eq!((f.n_docs, f.total_len), (15, 140));
        assert_eq!(f.df["cat"], 3);
        assert_eq!(f.df["dog"], 3);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<TermStats>(&json).unwrap(), s);
    }
}
