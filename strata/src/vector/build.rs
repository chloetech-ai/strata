//! Building a vector field's sections.
//!
//! Build options (keys of [`BuildOptions`], all optional):
//!
//! | key | default | meaning |
//! |---|---|---|
//! | `vector.index` | schema's kind | `flat`, `ivf` or `hnsw` |
//! | `vector.ivf.nlist` | schema's `nlist`, else ≈ 4·√n | number of clusters |
//! | `vector.ivf.iters` | 10 | k-means iterations |
//! | `vector.ivf.sample` | 64 | k-means training points per cluster |
//! | `vector.hnsw.m` | schema's `m` (16) | links per node (2·M on layer 0) |
//! | `vector.hnsw.ef_construction` | schema's (200) | construction beam width |
//! | `vector.threads` | 1 | threads for k-means and graph construction |
//! | `vector.seed` | fixed | seed for sampling and HNSW levels |
//!
//! A field that is not `indexed` gets the forward vectors only (flat).

use posting_bench::bytes::ByteWriter;

use crate::error::{Error, Result};
use crate::schema::{Field, FieldType, VectorIndexKind};
use crate::segment::build::BuildOptions;
use crate::segment::{DocId, FieldIndexBuilder, SegmentWriter};
use crate::value::Value;
use crate::vector::hnsw::{self, HnswConfig};
use crate::vector::ivf::{self, IvfConfig};
use crate::vector::sq8::Sq8;
use crate::vector::{section, VectorInfo};

/// Float32 rows of `stride` floats (the first `dim` hold the vector).
pub(crate) struct Rows<'a> {
    pub data: &'a [f32],
    pub dim: usize,
    pub stride: usize,
    pub n: usize,
}

impl Rows<'_> {
    #[inline]
    pub fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.stride..(i + 1) * self.stride]
    }
}

/// Row strides: float32 rows padded to 16 floats and code rows to 64 bytes,
/// so every row starts on a cache line and kernels need no tail handling.
pub(crate) fn strides(dim: usize) -> (usize, usize) {
    (dim.next_multiple_of(16), dim.next_multiple_of(64))
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Flat,
    Ivf { nlist: Option<usize>, iters: usize, sample: usize },
    Hnsw { m: usize, ef_construction: usize },
}

fn parse<T: std::str::FromStr>(opts: &BuildOptions, key: &str) -> Result<Option<T>> {
    opts.get(key).map(|v| v.parse::<T>().map_err(|_| Error::Schema(format!("bad value {v:?} for build option {key}")))).transpose()
}

pub(crate) struct VectorBuilder {
    field: String,
    dim: usize,
    stride: usize,
    kind: Kind,
    threads: usize,
    seed: u64,
    /// Float32 rows as little-endian bytes, so the buffer becomes the
    /// section without a copy.
    rows: Vec<u8>,
    present: Vec<u64>,
    n: usize,
    missing: usize,
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
            },
            other => return Err(Error::Schema(format!("unknown vector index {other:?}"))),
        };
        Ok(VectorBuilder {
            field: field.name.clone(),
            dim: *dim,
            stride: strides(*dim).0,
            kind,
            threads: parse(opts, "vector.threads")?.unwrap_or(1),
            seed: parse(opts, "vector.seed")?.unwrap_or(0x5EE_D0F5_747A),
            rows: Vec::new(),
            present: Vec::new(),
            n: 0,
            missing: 0,
        })
    }
}

impl FieldIndexBuilder for VectorBuilder {
    fn add(&mut self, doc: DocId, value: &Value) -> Result<()> {
        if doc as usize != self.n {
            return Err(Error::Schema(format!("vector field {}: document {doc} out of order", self.field)));
        }
        if self.n.is_multiple_of(64) {
            self.present.push(0);
        }
        match value {
            Value::Vector(v) if v.len() == self.dim => {
                self.rows.extend(v.iter().flat_map(|x| x.to_le_bytes()));
                self.present[self.n / 64] |= 1 << (self.n % 64);
            }
            Value::Null => {
                self.rows.resize(self.rows.len() + self.dim * 4, 0);
                self.missing += 1;
            }
            _ => return Err(Error::Schema(format!("vector field {}: bad value for document {doc}", self.field))),
        }
        self.rows.resize(self.rows.len() + (self.stride - self.dim) * 4, 0);
        self.n += 1;
        Ok(())
    }

    fn finish(self: Box<Self>, out: &mut SegmentWriter) -> Result<()> {
        let VectorBuilder { field, dim, stride, kind, threads, seed, rows, present, n, missing } = *self;
        let code_stride = strides(dim).1;
        let is_present = |d: u32| present[d as usize / 64] >> (d % 64) & 1 == 1;
        let ids: Vec<u32> = (0..n as u32).filter(|&d| is_present(d)).collect();
        // SAFETY: f32 has no invalid bit patterns; `align_to` only yields the
        // aligned middle part, and we require it to be everything.
        let (head, data, tail) = unsafe { rows.align_to::<f32>() };
        assert!(head.is_empty() && tail.is_empty(), "allocator returned a misaligned vector buffer");
        let view = Rows { data, dim, stride, n };

        let sq = Sq8::fit(dim, ids.iter().map(|&d| &view.row(d as usize)[..dim]));
        let mut codes = vec![0u8; n * code_stride];
        for &d in &ids {
            let d = d as usize;
            sq.encode(&view.row(d)[..dim], &mut codes[d * code_stride..(d + 1) * code_stride]);
        }

        let mut info = VectorInfo {
            dim,
            n,
            missing,
            index: "flat".into(),
            stride_f32: stride,
            stride_u8: code_stride,
            nlist: None,
            train_points: None,
            iters: None,
            m: None,
            ef_construction: None,
            entry: None,
            max_level: None,
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
            Kind::Hnsw { m, ef_construction } => {
                let g = hnsw::build(&view, &is_present, &HnswConfig { m, ef_construction, threads, seed });
                info.index = "hnsw".into();
                info.m = Some(g.m);
                info.ef_construction = Some(ef_construction);
                info.entry = Some(g.entry);
                info.max_level = Some(g.max_level);
                index_sections.push((section(&field, "hnsw_upper"), g.upper_bytes()));
                index_sections.push((section(&field, "hnsw"), g.layer0_bytes()));
            }
        }

        let mut p = ByteWriter::new();
        p.array(&sq.min);
        p.array(&sq.scale);
        out.put(section(&field, "meta"), serde_json::to_vec(&info).expect("serialisable"));
        out.put(section(&field, "sq8p"), p.finish());
        if missing > 0 {
            out.put(section(&field, "present"), present.iter().flat_map(|w| w.to_le_bytes()).collect());
        }
        for (name, bytes) in index_sections {
            out.put(name, bytes);
        }
        out.put(section(&field, "sq8"), codes);
        out.put(section(&field, "f32"), rows);
        Ok(())
    }
}
