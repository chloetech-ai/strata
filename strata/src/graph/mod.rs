//! Graph indexes (DESIGN.md §5.4): edges as a field of their source row.
//!
//! An `EDGES` field holds a row's outgoing edges as a JSON array. Targets
//! are primary keys of the same table, so an edge survives merges, updates
//! and deletes of either end without rewriting anything: it is resolved to
//! a live row at query time through the primary-key indexes. Each element
//! is a target key, `[label, target]`, `[label, target, weight]` or
//! `{"to": target, "label": l, "weight": w}`:
//!
//! ```text
//! ["p2", ["cites", "p7"], {"to": "p9", "label": "extends", "weight": 0.5}]
//! ```
//!
//! Each segment stores one section per indexed EDGES field, `graph.<f>`:
//!
//! ```text
//! version | key kind (1 str, 2 int) | n_docs | n_edges | flags | in_bits
//! out:     offsets [u64; n + 1]            edges of document d: offsets[d]..offsets[d + 1]
//!          edge_src [u32; E]               the source document of each edge
//!          targets  [i64; E]  or  key offsets [u64; E + 1] + key bytes
//!          labels   [u16; E]  (flag 1), label names (offsets + bytes)
//!          weights  [f32; E]  (flag 2)
//! in:      dir [u32; 2^in_bits + 1]        radix directory over the groups' hashes
//!          hashes [u64; m]                 one per group of edges with the same target key
//!          offsets [u32; m + 1]            each group's range in the next two arrays
//!          sources [u32; E]                source document of each edge, grouped
//!          edges [u32; E]                  edge position of each, for labels and weights
//! local:   [u32; E]                        the target's docID when it is in this segment
//! ```
//!
//! The last array is a hint: a key has at most one live row in a snapshot,
//! so when the target row recorded at build time is still live it is the
//! target, and the hop needs no key lookup. Otherwise (the row was deleted
//! or updated since, or the target is in another segment) the target key is
//! looked up in the snapshot.
//!
//! Out-edges are a CSR over local docIDs. The reverse index groups edges by
//! target key, ordered by the key's hash, with the same radix directory as
//! the primary-key index, so "who points at key k" is one directory read, a
//! short scan and one stored-key comparison per group (hash collisions make
//! separate groups), after which the sources are read sequentially. The
//! traversals themselves are in [`traverse`].

pub mod traverse;

use std::collections::BTreeMap;

use posting_bench::bytes::{ByteReader, ByteWriter, OwnedBytes, Slice};
use serde_json::Value as Json;

use crate::error::{corrupt, Error, Result};
use crate::schema::{Field, FieldType, Schema};
use crate::segment::{DocId, FieldIndexBuilder, SegmentFile, SegmentWriter};
use crate::storage::{key_hash, KeyRef};
use crate::value::Value;

const VERSION: u64 = 3;
const KIND_STR: u64 = 1;
const KIND_INT: u64 = 2;
const FLAG_LABELS: u64 = 1;
const FLAG_WEIGHTS: u64 = 2;

/// Section name of an EDGES field's graph index.
pub fn section(field: &str) -> String {
    format!("graph.{field}")
}

/// One outgoing edge.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    /// The target's primary key (`Value::Str` or `Value::Int`).
    pub to: Value,
    pub label: Option<String>,
    /// Positive; 1 unless given.
    pub weight: f32,
}

impl Edge {
    pub fn to(key: impl Into<Value>) -> Edge {
        Edge { to: key.into(), label: None, weight: 1.0 }
    }
    pub fn labeled(label: &str, key: impl Into<Value>) -> Edge {
        Edge { to: key.into(), label: Some(label.to_string()), weight: 1.0 }
    }
    pub fn weight(mut self, w: f32) -> Edge {
        self.weight = w;
        self
    }
}

/// The value of an EDGES field holding `edges`.
pub fn edges_value(edges: &[Edge]) -> Value {
    let items: Vec<Json> = edges
        .iter()
        .map(|e| {
            let to = match &e.to {
                Value::Int(x) => Json::from(*x),
                Value::Str(s) => Json::from(s.as_str()),
                _ => Json::Null,
            };
            match (&e.label, e.weight == 1.0) {
                (None, true) => to,
                (Some(l), true) => Json::Array(vec![Json::from(l.as_str()), to]),
                (l, false) => Json::Array(vec![l.as_deref().map_or(Json::Null, Json::from), to, Json::from(e.weight as f64)]),
            }
        })
        .collect();
    Value::Str(Json::Array(items).to_string())
}

/// Parses an EDGES value. `int_keys`: the table's primary key is INT.
/// An empty string or `Null` has no edges.
pub fn parse_edges(v: &Value, int_keys: bool) -> Result<Vec<Edge>> {
    let s = match v {
        Value::Null => return Ok(Vec::new()),
        Value::Str(s) if s.trim().is_empty() => return Ok(Vec::new()),
        Value::Str(s) => s,
        other => return Err(bad(format!("expected a JSON array, got {other:?}"))),
    };
    let json: Json = serde_json::from_str(s).map_err(|e| bad(format!("not JSON: {e}")))?;
    let Json::Array(items) = json else { return Err(bad("expected a JSON array")) };
    let key = |j: &Json| -> Result<Value> {
        match (j, int_keys) {
            (Json::String(s), false) => Ok(Value::Str(s.clone())),
            (Json::Number(n), false) => Ok(Value::Str(n.to_string())),
            (Json::Number(n), true) => n.as_i64().map(Value::Int).ok_or_else(|| bad(format!("target {n} is not an integer key"))),
            (Json::String(s), true) => s.parse::<i64>().map(Value::Int).map_err(|_| bad(format!("target {s:?} is not an integer key"))),
            (j, _) => Err(bad(format!("bad edge target {j}"))),
        }
    };
    let label = |j: &Json| -> Result<Option<String>> {
        match j {
            Json::Null => Ok(None),
            Json::String(s) if s.is_empty() => Ok(None),
            Json::String(s) => Ok(Some(s.clone())),
            j => Err(bad(format!("bad edge label {j}"))),
        }
    };
    let weight = |j: Option<&Json>| -> Result<f32> {
        match j {
            None | Some(Json::Null) => Ok(1.0),
            Some(Json::Number(n)) => {
                let w = n.as_f64().unwrap_or(f64::NAN) as f32;
                if w.is_finite() && w > 0.0 { Ok(w) } else { Err(bad(format!("edge weight {n} must be positive"))) }
            }
            Some(j) => Err(bad(format!("bad edge weight {j}"))),
        }
    };
    let mut out = Vec::with_capacity(items.len());
    for it in &items {
        out.push(match it {
            Json::String(_) | Json::Number(_) => Edge { to: key(it)?, label: None, weight: 1.0 },
            Json::Array(a) if a.len() == 2 || a.len() == 3 => Edge { to: key(&a[1])?, label: label(&a[0])?, weight: weight(a.get(2))? },
            Json::Object(o) => Edge {
                to: key(o.get("to").ok_or_else(|| bad("edge object needs \"to\""))?)?,
                label: o.get("label").map(label).transpose()?.flatten(),
                weight: weight(o.get("weight"))?,
            },
            j => return Err(bad(format!("bad edge {j}"))),
        });
    }
    Ok(out)
}

fn bad(m: impl Into<String>) -> Error {
    Error::Schema(format!("edges: {}", m.into()))
}

/// Whether the table's primary key is INT (EDGES targets are its keys).
pub fn int_keys(schema: &Schema) -> bool {
    schema.primary_key.is_some_and(|pk| schema.fields[pk].ty == FieldType::Int)
}

/// Builds `graph.<f>` for an indexed EDGES field of a segment whose rows
/// have primary keys `keys` (in docID order), for same-segment targets.
pub fn field_builder(field: &Field, schema: &Schema, keys: Vec<Value>) -> Result<Box<dyn FieldIndexBuilder>> {
    if schema.primary_key.is_none() {
        return Err(Error::Schema(format!("EDGES field {} needs a primary key", field.name)));
    }
    Ok(Box::new(GraphBuilder { name: field.name.clone(), int_keys: int_keys(schema), offsets: vec![0], keys, ..Default::default() }))
}

#[derive(Default)]
struct GraphBuilder {
    name: String,
    int_keys: bool,
    offsets: Vec<u64>,
    edge_src: Vec<u32>,
    ints: Vec<i64>,
    key_off: Vec<u64>,
    key_bytes: Vec<u8>,
    labels: Vec<u16>,
    label_codes: BTreeMap<String, u16>,
    weights: Vec<f32>,
    hashes: Vec<u64>,
    /// The segment's primary keys, by docID.
    keys: Vec<Value>,
}

impl FieldIndexBuilder for GraphBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        debug_assert_eq!(doc as usize + 1, self.offsets.len());
        for e in parse_edges(value, self.int_keys)? {
            self.edge_src.push(doc);
            let k = match &e.to {
                Value::Int(x) => {
                    self.ints.push(*x);
                    KeyRef::Int(*x)
                }
                Value::Str(s) => {
                    if self.key_off.is_empty() {
                        self.key_off.push(0);
                    }
                    self.key_bytes.extend_from_slice(s.as_bytes());
                    self.key_off.push(self.key_bytes.len() as u64);
                    KeyRef::Str(s.as_bytes())
                }
                _ => unreachable!("parse_edges returns keys"),
            };
            self.hashes.push(key_hash(k));
            let code = match &e.label {
                None => 0,
                Some(l) => {
                    let next = self.label_codes.len() as u16 + 1;
                    if next == u16::MAX {
                        return Err(Error::Schema(format!("EDGES field {} has too many labels", self.name)));
                    }
                    *self.label_codes.entry(l.clone()).or_insert(next)
                }
            };
            self.labels.push(code);
            self.weights.push(e.weight);
        }
        self.offsets.push(self.edge_src.len() as u64);
        Ok(())
    }

    fn finish(self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
        let b = *self;
        let n_edges = b.edge_src.len();
        let has_labels = b.labels.iter().any(|&l| l != 0);
        let has_weights = b.weights.iter().any(|&w| w != 1.0);
        // Reverse index: edges grouped by target key, groups ordered by the
        // key's hash (keys with colliding hashes form separate groups).
        let key_of = |e: u32| -> KeyRef<'_> {
            let e = e as usize;
            if b.int_keys {
                KeyRef::Int(b.ints[e])
            } else {
                KeyRef::Str(&b.key_bytes[b.key_off[e] as usize..b.key_off[e + 1] as usize])
            }
        };
        let cmp_key = |x: KeyRef, y: KeyRef| match (x, y) {
            (KeyRef::Int(a), KeyRef::Int(b)) => a.cmp(&b),
            (KeyRef::Str(a), KeyRef::Str(b)) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        };
        let mut by_hash: Vec<(u64, u32)> = b.hashes.iter().enumerate().map(|(e, &h)| (h, e as u32)).collect();
        by_hash.sort_unstable_by(|x, y| x.0.cmp(&y.0).then_with(|| cmp_key(key_of(x.1), key_of(y.1))).then(x.1.cmp(&y.1)));
        let mut hashes: Vec<u64> = Vec::new();
        let mut in_off: Vec<u32> = Vec::new();
        for (i, &(h, e)) in by_hash.iter().enumerate() {
            let new_group = i == 0 || by_hash[i - 1].0 != h || cmp_key(key_of(by_hash[i - 1].1), key_of(e)) != std::cmp::Ordering::Equal;
            if new_group {
                hashes.push(h);
                in_off.push(i as u32);
            }
        }
        in_off.push(n_edges as u32);
        let in_edges: Vec<u32> = by_hash.iter().map(|&(_, e)| e).collect();
        let in_src: Vec<u32> = in_edges.iter().map(|&e| b.edge_src[e as usize]).collect();
        // Same-segment targets: the last row holding the key (a memtable
        // segment keeps deleted versions; the newest is the one that can be live).
        let mut by_key: std::collections::HashMap<u64, Vec<u32>> = std::collections::HashMap::new();
        for (d, k) in b.keys.iter().enumerate() {
            if let Some(k) = KeyRef::of(k) {
                by_key.entry(key_hash(k)).or_default().push(d as u32);
            }
        }
        let local: Vec<u32> = (0..n_edges as u32)
            .map(|e| {
                let k = key_of(e);
                by_key
                    .get(&b.hashes[e as usize])
                    .and_then(|ds| ds.iter().rev().find(|&&d| KeyRef::of(&b.keys[d as usize]) == Some(k)))
                    .copied()
                    .unwrap_or(u32::MAX)
            })
            .collect();
        let bits = dir_bits(hashes.len());
        let mut dir = vec![0u32; (1usize << bits) + 1];
        let mut i = 0;
        for (slot_b, slot) in dir.iter_mut().enumerate() {
            while i < hashes.len() && bucket(hashes[i], bits) < slot_b {
                i += 1;
            }
            *slot = i as u32;
        }
        let mut names: Vec<(u16, &String)> = b.label_codes.iter().map(|(l, &c)| (c, l)).collect();
        names.sort();
        let mut name_off = vec![0u64];
        let mut name_bytes = Vec::new();
        for (_, l) in names {
            name_bytes.extend_from_slice(l.as_bytes());
            name_off.push(name_bytes.len() as u64);
        }
        let mut w = ByteWriter::new();
        w.u64(VERSION);
        w.u64(if b.int_keys { KIND_INT } else { KIND_STR });
        w.u64((b.offsets.len() - 1) as u64);
        w.u64(n_edges as u64);
        w.u64(if has_labels { FLAG_LABELS } else { 0 } | if has_weights { FLAG_WEIGHTS } else { 0 });
        w.u64(bits as u64);
        w.array(&b.offsets);
        w.array(&b.edge_src);
        if b.int_keys {
            w.array(&b.ints);
        } else {
            let key_off = if b.key_off.is_empty() { vec![0u64] } else { b.key_off };
            w.array(&key_off);
            w.array(&b.key_bytes);
        }
        w.array(if has_labels { &b.labels[..] } else { &[] });
        w.array(&name_off);
        w.array(&name_bytes);
        w.array(if has_weights { &b.weights[..] } else { &[] });
        w.array(&dir);
        w.array(&hashes);
        w.array(&in_off);
        w.array(&in_src);
        w.array(&in_edges);
        w.array(&local);
        out.put(section(&b.name), w.finish());
        Ok(())
    }
}

/// About four distinct hashes per directory slot, at most 2^24 slots.
fn dir_bits(n: usize) -> u32 {
    if n < 8 {
        0
    } else {
        (n.ilog2() - 2).min(24)
    }
}

#[inline]
fn bucket(h: u64, bits: u32) -> usize {
    h.checked_shr(64 - bits).unwrap_or(0) as usize
}

/// Zero-copy reader of one segment's `graph.<f>` section.
#[derive(Clone, Debug)]
pub struct GraphReader {
    int_keys: bool,
    n_docs: usize,
    offsets: Slice<u64>,
    edge_src: Slice<u32>,
    ints: Slice<i64>,
    key_off: Slice<u64>,
    key_bytes: Slice<u8>,
    labels: Slice<u16>,
    names: Vec<String>,
    weights: Slice<f32>,
    bits: u32,
    dir: Slice<u32>,
    hashes: Slice<u64>,
    in_off: Slice<u32>,
    in_src: Slice<u32>,
    in_edges: Slice<u32>,
    local: Slice<u32>,
}

impl GraphReader {
    /// The reader of field `field`, or `None` if the segment has no such section.
    pub fn open(seg: &SegmentFile, field: &str) -> Result<Option<GraphReader>> {
        seg.section(&section(field)).map(GraphReader::from_section).transpose()
    }

    pub fn from_section(bytes: OwnedBytes) -> Result<GraphReader> {
        let mut r = ByteReader::new(bytes);
        let mut u = || r.u64().map_err(corrupt);
        let (version, kind, n_docs, n_edges, flags, bits) = (u()?, u()?, u()? as usize, u()? as usize, u()?, u()? as u32);
        if version != VERSION || !(kind == KIND_STR || kind == KIND_INT) || bits > 24 {
            return Err(corrupt("bad graph section header"));
        }
        let int_keys = kind == KIND_INT;
        let offsets: Slice<u64> = r.array().map_err(corrupt)?;
        let edge_src: Slice<u32> = r.array().map_err(corrupt)?;
        let (mut ints, mut key_off, mut key_bytes) = (Slice::default(), Slice::default(), Slice::default());
        if int_keys {
            ints = r.array().map_err(corrupt)?;
        } else {
            key_off = r.array().map_err(corrupt)?;
            key_bytes = r.array().map_err(corrupt)?;
        }
        let labels: Slice<u16> = r.array().map_err(corrupt)?;
        let name_off: Slice<u64> = r.array().map_err(corrupt)?;
        let name_bytes: Slice<u8> = r.array().map_err(corrupt)?;
        let weights: Slice<f32> = r.array().map_err(corrupt)?;
        let dir: Slice<u32> = r.array().map_err(corrupt)?;
        let hashes: Slice<u64> = r.array().map_err(corrupt)?;
        let in_off: Slice<u32> = r.array().map_err(corrupt)?;
        let in_src: Slice<u32> = r.array().map_err(corrupt)?;
        let in_edges: Slice<u32> = r.array().map_err(corrupt)?;
        let local: Slice<u32> = r.array().map_err(corrupt)?;
        let keys_ok = if int_keys { ints.len() == n_edges } else { key_off.len() == n_edges + 1 && key_off[n_edges] as usize == key_bytes.len() };
        if offsets.len() != n_docs + 1
            || offsets[n_docs] as usize != n_edges
            || edge_src.len() != n_edges
            || !keys_ok
            || (flags & FLAG_LABELS != 0 && labels.len() != n_edges)
            || (flags & FLAG_WEIGHTS != 0 && weights.len() != n_edges)
            || dir.len() != (1 << bits) + 1
            || in_off.len() != hashes.len() + 1
            || in_edges.len() != n_edges
            || in_src.len() != n_edges
            || local.len() != n_edges
            || name_off.is_empty()
        {
            return Err(corrupt("graph section sizes disagree"));
        }
        let names = (1..name_off.len())
            .map(|i| String::from_utf8_lossy(&name_bytes[name_off[i - 1] as usize..name_off[i] as usize]).into_owned())
            .collect();
        Ok(GraphReader { int_keys, n_docs, offsets, edge_src, ints, key_off, key_bytes, labels, names, weights, bits, dir, hashes, in_off, in_src, in_edges, local })
    }

    pub fn n_docs(&self) -> usize {
        self.n_docs
    }

    pub fn n_edges(&self) -> usize {
        self.edge_src.len()
    }

    /// Edge positions of document `d`'s outgoing edges.
    #[inline]
    pub fn out_edges(&self, d: DocId) -> std::ops::Range<usize> {
        self.offsets[d as usize] as usize..self.offsets[d as usize + 1] as usize
    }

    #[inline]
    pub fn source(&self, e: usize) -> DocId {
        self.edge_src[e]
    }

    /// The target key of edge `e`.
    #[inline]
    pub fn target(&self, e: usize) -> KeyRef<'_> {
        if self.int_keys {
            KeyRef::Int(self.ints[e])
        } else {
            KeyRef::Str(&self.key_bytes[self.key_off[e] as usize..self.key_off[e + 1] as usize])
        }
    }

    /// The docID in this segment that held edge `e`'s target when the
    /// segment was built, if any (see the module notes).
    #[inline]
    pub fn local_target(&self, e: usize) -> Option<DocId> {
        Some(self.local[e]).filter(|&d| d != u32::MAX)
    }

    /// Label code of edge `e` (0: no label).
    #[inline]
    pub fn label(&self, e: usize) -> u16 {
        if self.labels.is_empty() { 0 } else { self.labels[e] }
    }

    /// The code of label `name` in this segment, if any edge has it.
    pub fn label_code(&self, name: &str) -> Option<u16> {
        self.names.iter().position(|n| n == name).map(|i| i as u16 + 1)
    }

    pub fn label_name(&self, code: u16) -> Option<&str> {
        code.checked_sub(1).and_then(|i| self.names.get(i as usize)).map(|s| s.as_str())
    }

    #[inline]
    pub fn weight(&self, e: usize) -> f32 {
        if self.weights.is_empty() { 1.0 } else { self.weights[e] }
    }

    /// The edges whose target is `key` (with hash `h = key_hash(key)`), as
    /// a range of reverse-index positions (see [`in_source`](Self::in_source)
    /// and [`in_edge`](Self::in_edge)); empty if none.
    pub fn in_group(&self, key: KeyRef, h: u64) -> std::ops::Range<usize> {
        let b = bucket(h, self.bits);
        let (lo, hi) = (self.dir[b] as usize, self.dir[b + 1] as usize);
        let mut i = lo;
        while i < hi && self.hashes[i] < h {
            i += 1;
        }
        while i < hi && self.hashes[i] == h {
            let range = self.in_off[i] as usize..self.in_off[i + 1] as usize;
            // Integer hashes are a bijection: equal hashes are equal keys.
            if self.int_keys || self.target(self.in_edges[range.start] as usize) == key {
                return range;
            }
            i += 1;
        }
        0..0
    }

    /// The source document of reverse-index position `i`.
    #[inline]
    pub fn in_source(&self, i: usize) -> DocId {
        self.in_src[i]
    }

    /// The edge position of reverse-index position `i`.
    #[inline]
    pub fn in_edge(&self, i: usize) -> usize {
        self.in_edges[i] as usize
    }

    /// Whether any edge has a label or a weight other than 1.
    pub fn has_labels_or_weights(&self) -> bool {
        !self.labels.is_empty() || !self.weights.is_empty()
    }

    /// Edges whose target is `key` (with hash `h`), as edge positions.
    pub fn edges_to<'a>(&'a self, key: KeyRef<'a>, h: u64) -> impl Iterator<Item = usize> + 'a {
        self.in_group(key, h).map(move |i| self.in_edges[i] as usize)
    }

    /// Document `d`'s edges as an EDGES value (merges and projections of
    /// unstored fields).
    pub fn edges_value(&self, d: DocId) -> Value {
        let edges: Vec<Edge> = self
            .out_edges(d)
            .map(|e| Edge {
                to: match self.target(e) {
                    KeyRef::Int(x) => Value::Int(x),
                    KeyRef::Str(s) => Value::Str(String::from_utf8_lossy(s).into_owned()),
                },
                label: self.label_name(self.label(e)).map(String::from),
                weight: self.weight(e),
            })
            .collect();
        edges_value(&edges)
    }

    /// Bytes of the section's parts, for size breakdowns.
    pub fn sizes(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("offsets", self.offsets.byte_len()),
            ("sources", self.edge_src.byte_len()),
            ("targets", self.ints.byte_len() + self.key_off.byte_len() + self.key_bytes.byte_len()),
            ("labels", self.labels.byte_len()),
            ("weights", self.weights.byte_len()),
            ("reverse", self.dir.byte_len() + self.hashes.byte_len() + self.in_off.byte_len() + self.in_src.byte_len() + self.in_edges.byte_len()),
            ("local targets", self.local.byte_len()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Field;

    fn schema(int: bool) -> Schema {
        let id = Field::new("id", if int { FieldType::Int } else { FieldType::Keyword });
        Schema::new("t", vec![id, Field::new("links", FieldType::Edges)], Some("id")).unwrap()
    }

    fn build(schema: &Schema, values: &[Value]) -> GraphReader {
        let int = schema.fields[0].ty == FieldType::Int;
        let keys = (0..values.len()).map(|i| if int { Value::Int(i as i64) } else { Value::Str(["a", "b", "c"].get(i).map_or(format!("k{i}"), |s| s.to_string())) }).collect();
        let mut b = field_builder(&schema.fields[1], schema, keys).unwrap();
        for (d, v) in values.iter().enumerate() {
            b.add(d as DocId, v).unwrap();
        }
        let mut w = SegmentWriter::new(values.len());
        b.finish(&mut w).unwrap();
        GraphReader::open(&SegmentFile::from_vec(w.finish()).unwrap(), "links").unwrap().unwrap()
    }

    #[test]
    fn parses_every_edge_form() {
        let v = Value::Str(r#"["a", ["cites", "b"], ["cites", "c", 0.5], {"to": "d", "label": "x"}, 7]"#.into());
        let e = parse_edges(&v, false).unwrap();
        assert_eq!(e.len(), 5);
        assert_eq!(e[0], Edge::to("a"));
        assert_eq!(e[1], Edge::labeled("cites", "b"));
        assert_eq!(e[2], Edge::labeled("cites", "c").weight(0.5));
        assert_eq!(e[3], Edge::labeled("x", "d"));
        assert_eq!(e[4].to, Value::Str("7".into()));
        assert_eq!(parse_edges(&edges_value(&e), false).unwrap(), e);
        assert!(parse_edges(&Value::Str("".into()), false).unwrap().is_empty());
        assert!(parse_edges(&Value::Null, true).unwrap().is_empty());
        for bad in ["{", r#"{"to": "a"}"#, r#"[["a"]]"#, r#"[["l", "a", -1]]"#, r#"[true]"#] {
            assert!(parse_edges(&Value::Str(bad.into()), false).is_err(), "{bad}");
        }
        assert!(parse_edges(&Value::Str(r#"["x"]"#.into()), true).is_err());
        assert_eq!(parse_edges(&Value::Str("[3, \"4\"]".into()), true).unwrap()[1].to, Value::Int(4));
    }

    #[test]
    fn out_and_reverse_edges_with_labels_and_weights() {
        let s = schema(false);
        let vals = [
            edges_value(&[Edge::to("b"), Edge::labeled("cites", "c").weight(2.0)]),
            Value::Null,
            edges_value(&[Edge::labeled("cites", "b"), Edge::to("missing")]),
        ];
        let g = build(&s, &vals);
        assert_eq!((g.n_docs(), g.n_edges()), (3, 4));
        assert_eq!(g.out_edges(0), 0..2);
        assert_eq!(g.out_edges(1), 2..2);
        assert_eq!(g.target(1), KeyRef::Str(b"c"));
        assert_eq!(g.label_name(g.label(1)), Some("cites"));
        assert_eq!(g.weight(1), 2.0);
        assert_eq!(g.label(0), 0);
        let to_b: Vec<DocId> = g.edges_to(KeyRef::Str(b"b"), key_hash(KeyRef::Str(b"b"))).map(|e| g.source(e)).collect();
        assert_eq!(to_b, vec![0, 2]);
        assert_eq!(g.edges_to(KeyRef::Str(b"zz"), key_hash(KeyRef::Str(b"zz"))).count(), 0);
        // Rows a, b, c: targets b and c are in the segment, "missing" is not.
        assert_eq!((g.local_target(0), g.local_target(1), g.local_target(3)), (Some(1), Some(2), None));
        assert_eq!(parse_edges(&g.edges_value(0), false).unwrap(), parse_edges(&vals[0], false).unwrap());
    }

    #[test]
    fn many_int_keys_found_through_the_directory() {
        let s = schema(true);
        let n = 3000i64;
        let vals: Vec<Value> = (0..n).map(|i| edges_value(&[Edge::to(Value::Int((i * 7) % n)), Edge::to(Value::Int((i + 1) % n))])).collect();
        let g = build(&s, &vals);
        for t in [0i64, 1, 999, 2999] {
            let k = KeyRef::Int(t);
            let mut srcs: Vec<DocId> = g.edges_to(k, key_hash(k)).map(|e| g.source(e)).collect();
            srcs.sort();
            let mut want: Vec<DocId> = (0..n).filter(|i| (i * 7) % n == t).map(|i| i as DocId).collect();
            want.push(((t - 1).rem_euclid(n)) as DocId);
            want.sort();
            assert_eq!(srcs, want, "target {t}");
        }
    }
}
