//! Building a vector field's sections.
//!
//! Build options (keys of [`BuildOptions`], all optional):
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `vector.index` | schema's kind | `flat`, `ivf` or `hnsw` |
//! | `vector.codes` | schema's codes (`sq8`) | `sq8`, `binary`, `rabitq1`, `rabitq2` or `rabitq4` |
//! | `vector.ivf.nlist` | schema's `nlist`, else ≈ 4·√n | number of clusters |
//! | `vector.ivf.iters` | 10 | k-means iterations |
//! | `vector.ivf.sample` | 64 | k-means training points per cluster |
//! | `vector.hnsw.m` | schema's `m` (16) | links per node (2·M on layer 0) |
//! | `vector.hnsw.ef_construction` | schema's (200) | construction beam width |
//! | `vector.hnsw.build` | `sq8` with SQ8 codes | `sq8`: construction scores with the codes, as search does; `f32`: with float32 rows |
//! | `vector.threads` | 1 | threads for k-means and graph construction |
//! | `vector.seed` | fixed | seed for sampling and HNSW levels |
//!
//! A field that is not `indexed` gets the forward vectors only (flat).

use std::sync::Arc;

use crate::postings::bytes::ByteWriter;

use crate::error::{Error, Result};
use crate::schema::{Field, FieldType, VectorCodes, VectorIndexKind};
use crate::segment::build::BuildOptions;
use crate::segment::{DocId, FieldIndexBuilder, SegmentFile, SegmentWriter, Spill, SpillStream};
use crate::value::Value;
use crate::vector::hnsw::{self, HnswConfig, HnswView};
use crate::vector::ivf::{self, IvfConfig};
use crate::vector::bits::Bits;
use crate::vector::codes::Quantiser;
use crate::vector::rabitq::{self, Rabitq};
use crate::vector::sq8::Sq8;
use crate::vector::{section, PartInfo, PartitionsInfo, VectorInfo};

/// Float32 rows of `stride` floats (the first `dim` hold the vector): row
/// `i` of `data`, or with `map`, row `map[i]` (a partition's rows, numbered
/// from 0; `n` is then `map.len()`).
pub(crate) struct Rows<'a> {
    pub data: &'a [f32],
    pub dim: usize,
    pub stride: usize,
    pub n: usize,
    pub map: Option<&'a [u32]>,
}

impl Rows<'_> {
    #[inline]
    pub fn row(&self, i: usize) -> &[f32] {
        let i = self.map.map_or(i, |m| m[i] as usize);
        &self.data[i * self.stride..(i + 1) * self.stride]
    }
}

/// Row strides: float32 rows padded to 16 floats and SQ8 code rows to 64
/// bytes, so every row starts on a cache line and kernels need no tail
/// handling (binary code rows: [`VectorCodes::stride`]).
pub(crate) fn strides(dim: usize) -> (usize, usize) {
    (dim.next_multiple_of(16), dim.next_multiple_of(64))
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Flat,
    Ivf { nlist: Option<usize>, iters: usize, sample: usize },
    Hnsw { m: usize, ef_construction: usize, f32: bool },
}

fn parse<T: std::str::FromStr>(opts: &BuildOptions, key: &str) -> Result<Option<T>> {
    opts.get(key).map(|v| v.parse::<T>().map_err(|_| Error::Schema(format!("bad value {v:?} for build option {key}")))).transpose()
}

/// A vector field's float32 rows as little-endian bytes: in memory, where
/// the buffer becomes the section without a copy, or written out as they
/// are added ([`FieldIndexBuilder::spill_to`]), with a buffer for one row's
/// bytes.
enum RowBuf {
    Mem(Vec<u8>),
    Spill(SpillStream, Vec<u8>),
}

impl RowBuf {
    fn len(&self) -> usize {
        match self {
            RowBuf::Mem(v) => v.len(),
            RowBuf::Spill(s, _) => s.len(),
        }
    }

    fn push(&mut self, v: &[f32]) {
        match self {
            RowBuf::Mem(b) => b.extend(v.iter().flat_map(|x| x.to_le_bytes())),
            RowBuf::Spill(s, b) => {
                b.clear();
                b.extend(v.iter().flat_map(|x| x.to_le_bytes()));
                s.extend_from_slice(b);
            }
        }
    }

    fn zeros(&mut self, n: usize) {
        match self {
            RowBuf::Mem(b) => b.resize(b.len() + n, 0),
            RowBuf::Spill(s, _) => s.zeros(n),
        }
    }
}

pub(crate) struct VectorBuilder {
    field: String,
    dim: usize,
    stride: usize,
    kind: Kind,
    codes: VectorCodes,
    /// Segments with more vectors than this build IVF instead of HNSW
    /// (`vector.hnsw.max_docs`): an HNSW graph's build time grows faster
    /// than the segment, and is paid again at every merge.
    hnsw_max_docs: Option<usize>,
    threads: usize,
    seed: u64,
    /// Segments whose graphs this one's starts from, and the docID each
    /// one's first document has here ([`hnsw::StartGraph`]); the first that
    /// fits keeps its links, the others are joined to it.
    start: Vec<(SegmentFile, u32)>,
    /// Float32 rows as little-endian bytes.
    rows: RowBuf,
    present: Vec<u64>,
    n: usize,
    missing: usize,
    /// A multi-vector field's row offsets (`n` counts vectors), and whether
    /// it keeps its float32 vectors.
    multi: Option<(Vec<u32>, bool)>,
    /// A partitioned field's rows' partition keys so far.
    partition: Option<PartitionKeys>,
}

/// Each row's partition ([`crate::schema::VectorPartition`]): the range of
/// a numeric key, or the number of a KEYWORD key's value, [`NO_PART`] for
/// NULL.
struct PartitionKeys {
    bounds: Vec<f64>,
    keys: Vec<u32>,
    values: Vec<String>,
    numbers: std::collections::HashMap<String, u32>,
}

const NO_PART: u32 = u32::MAX;

impl PartitionKeys {
    fn push(&mut self, field: &str, doc: DocId, key: &Value) -> Result<()> {
        if doc as usize != self.keys.len() {
            return Err(Error::Schema(format!("vector field {field}: partition key of document {doc} out of order")));
        }
        let k = match (key, self.bounds.is_empty()) {
            (Value::Null, _) => NO_PART,
            (Value::Str(s), true) => match self.numbers.get(s) {
                Some(&k) => k,
                None => {
                    let k = self.values.len() as u32;
                    self.values.push(s.clone());
                    self.numbers.insert(s.clone(), k);
                    k
                }
            },
            (Value::Int(x), false) => self.bounds.partition_point(|&b| b <= *x as f64) as u32,
            (Value::Float(x), false) => self.bounds.partition_point(|&b| b <= *x) as u32,
            _ => return Err(Error::Schema(format!("vector field {field}: bad partition key {key:?} for document {doc}"))),
        };
        self.keys.push(k);
        Ok(())
    }

    /// The partitions to build, each with its rows (those with a vector)
    /// in docID order, and whether every row with a key is in one: a
    /// KEYWORD key's most frequent values only, up to
    /// [`crate::schema::MAX_VALUE_PARTITIONS`].
    fn groups(&self, present: &dyn Fn(u32) -> bool) -> (Vec<(u32, Vec<u32>)>, bool) {
        let count = if self.bounds.is_empty() { self.values.len() } else { self.bounds.len() + 1 };
        let mut groups: Vec<(u32, Vec<u32>)> = (0..count as u32).map(|k| (k, Vec::new())).collect();
        for (d, &k) in self.keys.iter().enumerate() {
            if k != NO_PART && present(d as u32) {
                groups[k as usize].1.push(d as u32);
            }
        }
        groups.retain(|g| !g.1.is_empty());
        let mut complete = true;
        if groups.len() > crate::schema::MAX_VALUE_PARTITIONS {
            groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
            groups.truncate(crate::schema::MAX_VALUE_PARTITIONS);
            groups.sort_by_key(|g| g.0);
            complete = false;
        }
        (groups, complete)
    }
}

impl VectorBuilder {
    pub fn new(field: &Field, opts: &BuildOptions) -> Result<Self> {
        let FieldType::Vector { dim, index } = &field.ty else {
            return Err(Error::Schema(format!("field {} is not a vector", field.name)));
        };
        let schema_kind = if field.indexed { index.clone() } else { VectorIndexKind::Flat };
        let name = match opts.get("vector.index") {
            Some(k) => k.to_string(),
            None => match schema_kind {
                VectorIndexKind::Flat => "flat".into(),
                VectorIndexKind::Ivf { .. } => "ivf".into(),
                VectorIndexKind::Hnsw { .. } => "hnsw".into(),
            },
        };
        let (schema_nlist, schema_m, schema_ef) = match schema_kind {
            VectorIndexKind::Ivf { nlist } => (Some(nlist).filter(|&n| n > 0), 16, 200),
            VectorIndexKind::Hnsw { m, ef_construction } => (None, m, ef_construction),
            VectorIndexKind::Flat => (None, 16, 200),
        };
        let kind = match name.as_str() {
            "flat" => Kind::Flat,
            "ivf" => Kind::Ivf {
                nlist: parse(opts, "vector.ivf.nlist")?.or(schema_nlist),
                iters: parse(opts, "vector.ivf.iters")?.unwrap_or(10),
                sample: parse(opts, "vector.ivf.sample")?.unwrap_or(64),
            },
            "hnsw" => Kind::Hnsw {
                m: parse(opts, "vector.hnsw.m")?.unwrap_or(schema_m),
                ef_construction: parse(opts, "vector.hnsw.ef_construction")?.unwrap_or(schema_ef),
                f32: match opts.get("vector.hnsw.build") {
                    None | Some("sq8") => false,
                    Some("f32") => true,
                    Some(b) => return Err(Error::Schema(format!("bad value {b:?} for build option vector.hnsw.build (sq8 or f32)"))),
                },
            },
            other => return Err(Error::Schema(format!("unknown vector index {other:?}"))),
        };
        Ok(VectorBuilder {
            field: field.key(),
            dim: *dim,
            stride: strides(*dim).0,
            kind,
            codes: match opts.get("vector.codes") {
                None => field.codes,
                Some(c) => VectorCodes::parse(c).ok_or_else(|| Error::Schema(format!("unknown vector codes {c:?}")))?,
            },
            hnsw_max_docs: parse(opts, "vector.hnsw.max_docs")?,
            threads: parse(opts, "vector.threads")?.unwrap_or(1),
            seed: parse(opts, "vector.seed")?.unwrap_or(0x5EE_D0F5_747A),
            start: Vec::new(),
            rows: RowBuf::Mem(Vec::new()),
            present: Vec::new(),
            n: 0,
            missing: 0,
            multi: field.is_multi_vector().then(|| (vec![0], field.keep_f32)),
            partition: field.partition.as_ref().map(|p| PartitionKeys { bounds: p.bounds.clone(), keys: Vec::new(), values: Vec::new(), numbers: Default::default() }),
        })
    }
}

impl VectorBuilder {
    /// Starts the field's HNSW graph from the graphs of `segs`, whose
    /// documents are this build's from each one's docID `base` on, all of
    /// them with the same vectors: the first that fits keeps its links and
    /// the others are joined to it.
    pub fn start_from(mut self, segs: Vec<(SegmentFile, u32)>) -> Self {
        self.start = segs;
        self
    }
}

/// `seg`'s HNSW graph of `field` and its node count, if it has one of
/// `dim` dimensions.
fn start_graph(seg: &SegmentFile, field: &str, dim: usize) -> Option<(HnswView, usize)> {
    let info: VectorInfo = serde_json::from_slice(&seg.section(&section(field, "meta")).ok()??).ok()?;
    if info.index != "hnsw" || info.dim != dim || info.n != seg.n_docs() {
        return None;
    }
    let g = HnswView::open(info.m?, info.entry?, seg.section(&section(field, "hnsw")).ok()??, seg.section(&section(field, "hnsw_upper")).ok()??, info.n).ok()?;
    Some((g, info.n))
}

/// `seg`'s graphs of `field`'s partitions, each with its rows (docIDs of
/// `seg`), if the segment has an HNSW graph of `dim` dimensions. A merge
/// starts a partition's graph from these when an input's rows of that
/// partition are exactly one of its graphs' rows ([`part_starts`]).
fn start_parts(seg: &SegmentFile, field: &str, dim: usize) -> Vec<(Vec<u32>, HnswView)> {
    let Some(info) = seg.section(&section(field, "meta")).ok().flatten().and_then(|b| serde_json::from_slice::<VectorInfo>(&b).ok()) else {
        return Vec::new();
    };
    let (Some(p), Some(m)) = (&info.partitions, info.m) else {
        return Vec::new();
    };
    if info.index != "hnsw" || info.dim != dim || info.n != seg.n_docs() {
        return Vec::new();
    }
    p.parts
        .iter()
        .enumerate()
        .filter_map(|(i, pi)| {
            let part = |x: &str| section(field, &format!("part{i}.{x}"));
            let docs: Vec<u32> = seg.section(&part("docs")).ok()??.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes"))).collect();
            if docs.len() != pi.n || !docs.windows(2).all(|w| w[0] < w[1]) || docs.last().is_some_and(|&d| d as usize >= info.n) {
                return None;
            }
            let g = HnswView::open(m, pi.entry, seg.section(&part("hnsw")).ok()??, seg.section(&part("hnsw_upper")).ok()??, pi.n).ok()?;
            Some((docs, g))
        })
        .collect()
}

/// The graphs a partition of rows `docs` (docIDs of this build, in order)
/// starts from: of each start segment (its first docID here, its document
/// count and its partitions' graphs from [`start_parts`]), the graph whose
/// rows are exactly this partition's rows in that segment's range, numbered
/// as the partition numbers them. A segment whose rows of the partition
/// are not exactly one of its graphs' (a key written in place since, or
/// other bounds) is left out; the same rows under another key are not,
/// since the graph is over the same vectors. Largest first, as the field's
/// graph starts from the largest input's.
fn part_starts<'a>(docs: &[u32], from: &'a [(u32, usize, Vec<(Vec<u32>, HnswView)>)]) -> Vec<hnsw::StartGraph<'a>> {
    let mut out = Vec::new();
    for (base, n_docs, parts) in from {
        let lo = docs.partition_point(|&d| d < *base);
        let hi = docs.partition_point(|&d| (d as u64) < *base as u64 + *n_docs as u64);
        let mine = &docs[lo..hi];
        if mine.is_empty() {
            continue;
        }
        if let Some((_, g)) = parts.iter().find(|(pd, _)| pd.len() == mine.len() && pd.iter().zip(mine).all(|(&a, &b)| a + base == b)) {
            out.push(hnsw::StartGraph { view: g, base: lo as u32, n: mine.len() });
        }
    }
    out.sort_by_key(|s| (std::cmp::Reverse(s.n), s.base));
    out
}

impl FieldIndexBuilder for VectorBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        if let Some((offsets, _)) = &self.multi {
            if doc as usize + 1 != offsets.len() {
                return Err(Error::Schema(format!("vector field {}: document {doc} out of order", self.field)));
            }
            for v in value.items() {
                match v {
                    Value::Vector(v) if v.len() == self.dim => {
                        if self.n.is_multiple_of(64) {
                            self.present.push(0);
                        }
                        self.rows.push(v);
                        self.rows.zeros((self.stride - self.dim) * 4);
                        self.present[self.n / 64] |= 1 << (self.n % 64);
                        self.n += 1;
                    }
                    _ => return Err(Error::Schema(format!("vector field {}: bad value for document {doc}", self.field))),
                }
            }
            let n = u32::try_from(self.n).map_err(|_| Error::Schema(format!("vector field {}: more than 2^32 vectors in a segment", self.field)))?;
            self.multi.as_mut().expect("multi").0.push(n);
            return Ok(());
        }
        if doc as usize != self.n {
            return Err(Error::Schema(format!("vector field {}: document {doc} out of order", self.field)));
        }
        if self.n.is_multiple_of(64) {
            self.present.push(0);
        }
        match value {
            Value::Vector(v) if v.len() == self.dim => {
                self.rows.push(v);
                self.present[self.n / 64] |= 1 << (self.n % 64);
            }
            Value::Null => {
                self.rows.zeros(self.dim * 4);
                self.missing += 1;
            }
            _ => return Err(Error::Schema(format!("vector field {}: bad value for document {doc}", self.field))),
        }
        self.rows.zeros((self.stride - self.dim) * 4);
        self.n += 1;
        Ok(())
    }

    fn partition_key(&mut self, doc: DocId, key: &Value) -> Result<()> {
        match &mut self.partition {
            Some(p) => p.push(&self.field, doc, key),
            None => Ok(()),
        }
    }

    fn spill_to(&mut self, spill: &Arc<Spill>) -> Result<()> {
        if self.rows.len() == 0 {
            self.rows = RowBuf::Spill(spill.stream()?, Vec::new());
        }
        Ok(())
    }

    fn finish(self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
        let VectorBuilder { field, dim, stride, kind, codes: code_kind, hnsw_max_docs, threads, seed, start, mut rows, present, n, missing, multi, partition } = *self;
        let code_stride = code_kind.stride(dim);
        let is_present = |d: u32| present[d as usize / 64] >> (d % 64) & 1 == 1;
        let ids: Vec<u32> = (0..n as u32).filter(|&d| is_present(d)).collect();
        let kind = match kind {
            Kind::Hnsw { .. } if hnsw_max_docs.is_some_and(|m| ids.len() > m) => Kind::Ivf { nlist: None, iters: 10, sample: 64 },
            // A multi-vector segment of NULL rows has no vectors to index.
            _ if multi.is_some() && n == 0 => Kind::Flat,
            k => k,
        };
        // A merge's graph is a graph of rows, not of a multi-vector field's vectors.
        let start = if multi.is_some() { Vec::new() } else { start };
        // Spilled rows are read back through a mapping of their file.
        let map = match &mut rows {
            RowBuf::Mem(_) => None,
            RowBuf::Spill(s, _) => s.map()?,
        };
        let bytes: &[u8] = match (&rows, &map) {
            (RowBuf::Mem(v), _) => v,
            (_, Some(m)) => m,
            (_, None) => &[],
        };
        // SAFETY: f32 has no invalid bit patterns; `align_to` only yields the
        // aligned middle part, and we require it to be everything.
        let (head, data, tail) = unsafe { bytes.align_to::<f32>() };
        assert!(head.is_empty() && tail.is_empty(), "allocator returned a misaligned vector buffer");
        let view = Rows { data, dim, stride, n, map: None };

        let present_rows = ids.iter().map(|&d| &view.row(d as usize)[..dim]);
        let quant = match code_kind {
            VectorCodes::Sq8 => Quantiser::Sq8(Sq8::fit(dim, present_rows)),
            VectorCodes::Binary => Quantiser::Bits(Bits::fit(dim, present_rows)),
            c => Quantiser::Rabitq(Rabitq::fit(dim, c.rabitq_bits().expect("rabitq"), present_rows)),
        };
        let mut codes = vec![0u8; n * code_stride];
        encode_rows(&quant, &view, &is_present, &mut codes, code_stride, threads);

        let mut info = VectorInfo {
            dim,
            n,
            missing,
            index: "flat".into(),
            stride_f32: stride,
            stride_u8: code_stride,
            codes: code_kind,
            nlist: None,
            train_points: None,
            iters: None,
            m: None,
            ef_construction: None,
            entry: None,
            max_level: None,
            rows: multi.as_ref().map(|(o, _)| o.len() - 1),
            f32: multi.as_ref().is_none_or(|&(_, keep)| keep),
            partitions: None,
        };
        let mut index_sections: Vec<(String, Vec<u8>)> = Vec::new();
        match kind {
            Kind::Flat => {}
            Kind::Ivf { nlist, iters, sample } => {
                let cfg = IvfConfig { nlist: nlist.unwrap_or_else(|| ivf::default_nlist(ids.len())), iters, sample_per_list: sample, threads, seed };
                let built = ivf::build(&view, &ids, &codes, code_stride, &cfg);
                info.index = "ivf".into();
                info.nlist = Some(built.nlist);
                info.train_points = Some(built.train_points);
                info.iters = Some(iters);
                index_sections.push((section(&field, "ivf_centroids"), built.centroid_bytes()));
                index_sections.push((section(&field, "ivf_dir"), built.dir_bytes()));
                index_sections.push((section(&field, "ivf"), built.lists));
            }
            Kind::Hnsw { m, ef_construction, f32 } => {
                let graphs: Vec<((HnswView, usize), u32)> = start.iter().filter_map(|(seg, base)| Some((start_graph(seg, &field, dim)?, *base))).collect();
                let starts: Vec<hnsw::StartGraph> = graphs.iter().map(|((g, n), base)| hnsw::StartGraph { view: g, base: *base, n: *n }).collect();
                let on = match &quant {
                    Quantiser::Sq8(sq8) if !f32 => Some(hnsw::BuildCodes { sq8, codes: &codes, stride: code_stride, map: None }),
                    _ => None,
                };
                let g = hnsw::build(&view, &is_present, &HnswConfig { m, ef_construction, threads, seed }, &starts, on.as_ref());
                info.index = "hnsw".into();
                info.m = Some(g.m);
                info.ef_construction = Some(ef_construction);
                info.entry = Some(g.entry);
                info.max_level = Some(g.max_level);
                index_sections.push((section(&field, "hnsw_upper"), g.upper_bytes()));
                index_sections.push((section(&field, "hnsw"), g.layer0_bytes()));
                // A graph per partition, over its rows numbered from 0 and
                // scoring the field's codes through the partition's docIDs.
                if let Some(keys) = partition.as_ref().filter(|_| multi.is_none()) {
                    if keys.keys.len() != n {
                        return Err(Error::Schema(format!("vector field {field}: partition keys of {} documents, not {n}", keys.keys.len())));
                    }
                    let (groups, complete) = keys.groups(&is_present);
                    // A merge starts each partition's graph from its inputs'
                    // graphs of the same rows, as it starts the field's.
                    let from: Vec<(u32, usize, Vec<(Vec<u32>, HnswView)>)> = start.iter().map(|(seg, base)| (*base, seg.n_docs(), start_parts(seg, &field, dim))).filter(|(_, _, p)| !p.is_empty()).collect();
                    let mut parts = Vec::with_capacity(groups.len());
                    for (i, (k, docs)) in groups.iter().enumerate() {
                        let rows = Rows { data, dim, stride, n: docs.len(), map: Some(docs) };
                        let on = match &quant {
                            Quantiser::Sq8(sq8) if !f32 => Some(hnsw::BuildCodes { sq8, codes: &codes, stride: code_stride, map: Some(docs) }),
                            _ => None,
                        };
                        let starts = part_starts(docs, &from);
                        let g = hnsw::build(&rows, &|_| true, &HnswConfig { m, ef_construction, threads, seed: seed ^ (*k as u64 + 1) }, &starts, on.as_ref());
                        parts.push(PartInfo {
                            bucket: (!keys.bounds.is_empty()).then_some(*k as usize),
                            value: keys.bounds.is_empty().then(|| keys.values[*k as usize].clone()),
                            n: docs.len(),
                            entry: g.entry,
                            max_level: g.max_level,
                        });
                        let part = |x: &str| section(&field, &format!("part{i}.{x}"));
                        index_sections.push((part("docs"), docs.iter().flat_map(|d| d.to_le_bytes()).collect()));
                        index_sections.push((part("hnsw_upper"), g.upper_bytes()));
                        index_sections.push((part("hnsw"), g.layer0_bytes()));
                    }
                    info.partitions = Some(PartitionsInfo { bounds: keys.bounds.clone(), complete, parts });
                }
            }
        }

        let mut p = ByteWriter::new();
        match &quant {
            Quantiser::Sq8(sq) => {
                p.array(&sq.min);
                p.array(&sq.scale);
            }
            Quantiser::Bits(b) => {
                p.array(&b.mean);
                p.array(&b.spread);
            }
            Quantiser::Rabitq(r) => {
                p.array(&r.centre);
                p.u64(rabitq::SEED);
            }
        }
        let (codes_part, params_part) = crate::vector::parts(code_kind);
        out.put(section(&field, "meta"), serde_json::to_vec(&info).expect("serialisable"));
        out.put(section(&field, params_part), p.finish());
        if missing > 0 {
            out.put(section(&field, "present"), present.iter().flat_map(|w| w.to_le_bytes()).collect());
        }
        for (name, bytes) in index_sections {
            out.put(name, bytes);
        }
        out.put(section(&field, codes_part), codes);
        if let Some((offsets, _)) = &multi {
            let mut owner = Vec::with_capacity(n * 4);
            for (r, w) in offsets.windows(2).enumerate() {
                for _ in w[0]..w[1] {
                    owner.extend((r as u32).to_le_bytes());
                }
            }
            out.put(section(&field, "rows"), offsets.iter().flat_map(|o| o.to_le_bytes()).collect());
            out.put(section(&field, "owner"), owner);
            // Rows are scored exactly with SQ8 codes when binary or RaBitQ
            // codes are searched and the float32 vectors are not kept.
            if !info.f32 && code_kind != VectorCodes::Sq8 {
                let sq = Sq8::fit(dim, (0..n).map(|d| &view.row(d)[..dim]));
                let s = strides(dim).1;
                let mut c = vec![0u8; n * s];
                for d in 0..n {
                    sq.encode(&view.row(d)[..dim], &mut c[d * s..(d + 1) * s]);
                }
                let mut p = ByteWriter::new();
                p.array(&sq.min);
                p.array(&sq.scale);
                out.put(section(&field, "rescore"), c);
                out.put(section(&field, "rescorep"), p.finish());
            }
        }
        if info.f32 {
            match rows {
                RowBuf::Mem(v) => out.put(section(&field, "f32"), v),
                RowBuf::Spill(s, _) => {
                    drop(map);
                    let (at, crc) = s.finish()?;
                    out.put_spilled(section(&field, "f32"), at, crc);
                }
            }
        }
        Ok(())
    }
}

/// The codes of the rows with a vector, into `codes` (`code_stride` bytes
/// a row), on up to `threads` threads: RaBitQ's rotation and grid make
/// encoding a large part of a flat or IVF build.
fn encode_rows(quant: &Quantiser, view: &Rows, is_present: &(dyn Fn(u32) -> bool + Sync), codes: &mut [u8], code_stride: usize, threads: usize) {
    let n = view.n;
    let encode = |lo: usize, out: &mut [u8]| {
        for (i, row) in out.chunks_exact_mut(code_stride).enumerate() {
            let d = lo + i;
            if is_present(d as u32) {
                quant.encode(&view.row(d)[..view.dim], row);
            }
        }
    };
    let threads = threads.clamp(1, n.div_ceil(4096).max(1));
    if threads == 1 {
        return encode(0, codes);
    }
    let per = n.div_ceil(threads);
    std::thread::scope(|s| {
        for (t, out) in codes.chunks_mut(per * code_stride).enumerate() {
            let encode = &encode;
            s.spawn(move || encode(t * per, out));
        }
    });
}
