//! Semantic IDs: a short code per row learned from its vector, so that rows
//! whose vectors are alike share prefixes (TIGER, Rajput et al. 2023).
//!
//! A [`Codebook`] holds `levels` sets of `k` centroids (at most
//! [`MAX_LEVELS`] of at most [`MAX_K`]). A vector's code is residual
//! quantisation: level 0 is the nearest centroid of the vector (by
//! Euclidean distance), level 1 the nearest of its own centroids to what
//! is left over, and so on. One more number, the tie, tells apart the rows
//! that share the levels' numbers, so a row's code reads `12.7.201.0`.
//!
//! A semantic-ID field ([`crate::schema::SemanticId`]) is a stored KEYWORD[]
//! holding every prefix of its row's code (`12`, `12.7`, `12.7.201`,
//! `12.7.201.0`), so a prefix match is an equality on one value. The
//! writer fills it from the row's VECTOR field when a write leaves it out,
//! after the vector is embedded, and gives the tie: one more than the
//! highest tie ever given under the row's prefix in the table, so a number
//! is never given twice, also after its row is deleted ([`Ties`]). A row
//! rewritten with a vector of the same prefix keeps its code. A code a
//! write supplies is kept as given, and one of the levels alone gets its
//! tie (a vector coded ahead, outside the writer).
//!
//! Merges and flushes drop deleted rows, and with them the codes their
//! dictionaries held: each such segment keeps the highest tie of every
//! prefix whose codes it dropped in section `sid.<field>.gone`
//! ([`gone_section`]), so that the ties of a table can be counted again
//! when it opens.
//!
//! [`Db::sid_next`](crate::db::Db::sid_next) answers which codes can
//! follow a prefix among the rows a filter matches, and how many rows each
//! has: the children of the prefix in the field's dictionary (every
//! prefix is a value of it), each counted against the filter.
//!
//! A codebook is a model: `<models>/<name>/codebook.safetensors`, F32
//! tensors `level.0` .. `level.<levels - 1>` of shape `[k, dim]`, the
//! format an RQ-VAE trained in PyTorch can export
//! (`safetensors.torch.save_file`). A field records the codebook's name and
//! the SHA-256 of that file, so codes of two codebooks never mix.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::schema::{FieldType, Schema};
use crate::segment::SegmentFile;
use crate::value::Value;

/// The most levels a codebook has (a prefix of them packs into a `u32`).
pub const MAX_LEVELS: usize = 4;
/// The most centroids a level has.
pub const MAX_K: usize = 256;
/// The codebook file in a codebook's directory.
pub const FILE: &str = "codebook.safetensors";

fn bad(m: impl std::fmt::Display) -> Error {
    Error::Schema(m.to_string())
}

/// Residual k-means centroids ([`Codebook::train`], or an RQ-VAE's).
#[derive(Clone, Debug, PartialEq)]
pub struct Codebook {
    pub dim: usize,
    /// Centroids per level.
    pub k: usize,
    /// Each level's `k * dim` centroids, row by row.
    pub levels: Vec<Vec<f32>>,
    /// Squared lengths of each level's centroids.
    norms: Vec<Vec<f32>>,
}

/// How [`Codebook::train`] runs.
#[derive(Clone, Debug)]
pub struct TrainOptions {
    pub levels: usize,
    pub k: usize,
    /// Lloyd iterations per level.
    pub iters: usize,
    /// Vectors trained on, drawn at random from those given (all when
    /// fewer).
    pub sample: usize,
    pub seed: u64,
}

impl Default for TrainOptions {
    /// 3 levels of 256, 25 iterations on up to 100 vectors per centroid.
    fn default() -> Self {
        TrainOptions { levels: 3, k: MAX_K, iters: 25, sample: 100 * MAX_K, seed: 0 }
    }
}

impl Codebook {
    /// A codebook of these centroids, checked.
    pub fn new(dim: usize, k: usize, levels: Vec<Vec<f32>>) -> Result<Codebook> {
        if dim == 0 || !(1..=MAX_K).contains(&k) || !(1..=MAX_LEVELS).contains(&levels.len()) {
            return Err(bad(format!("a codebook has 1 to {MAX_LEVELS} levels of 1 to {MAX_K} centroids of 1 or more dimensions, not {} of {k} of {dim}", levels.len())));
        }
        if let Some(l) = levels.iter().position(|c| c.len() != k * dim) {
            return Err(bad(format!("codebook level {l} holds {} numbers, not {k} centroids of {dim}", levels[l].len())));
        }
        if levels.iter().flatten().any(|x| !x.is_finite()) {
            return Err(bad("a codebook's centroids are finite"));
        }
        let norms = levels.iter().map(|c| c.chunks_exact(dim).map(|r| r.iter().map(|x| x * x).sum()).collect()).collect();
        Ok(Codebook { dim, k, levels, norms })
    }

    pub fn n_levels(&self) -> usize {
        self.levels.len()
    }

    /// The levels' numbers of `v`'s code.
    pub fn code(&self, v: &[f32]) -> Vec<u32> {
        let mut rest = v.to_vec();
        let mut out = Vec::with_capacity(self.levels.len());
        for (cents, norms) in self.levels.iter().zip(&self.norms) {
            let c = nearest(&rest, cents, norms, self.dim);
            for (r, x) in rest.iter_mut().zip(&cents[c * self.dim..(c + 1) * self.dim]) {
                *r -= x;
            }
            out.push(c as u32);
        }
        out
    }

    /// The safetensors bytes of the codebook ([`FILE`]).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        header.insert("__metadata__".into(), serde_json::json!({"format": "strata-codebook"}));
        let per = self.k * self.dim * 4;
        for l in 0..self.levels.len() {
            header.insert(format!("level.{l}"), serde_json::json!({"dtype": "F32", "shape": [self.k, self.dim], "data_offsets": [l * per, (l + 1) * per]}));
        }
        let mut h = serde_json::to_vec(&header).expect("a header serialises");
        // The data starts 8-byte aligned, as safetensors writers leave it.
        while (8 + h.len()) % 8 != 0 {
            h.push(b' ');
        }
        let mut out = Vec::with_capacity(8 + h.len() + per * self.levels.len());
        out.extend_from_slice(&(h.len() as u64).to_le_bytes());
        out.extend_from_slice(&h);
        for x in self.levels.iter().flatten() {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out
    }

    /// A codebook from safetensors bytes: F32 (or F64) tensors `level.0`,
    /// `level.1`, ... of one shape `[k, dim]`.
    pub fn from_bytes(b: &[u8]) -> Result<Codebook> {
        let damaged = |m: &str| bad(format!("not a codebook: {m}"));
        let hlen = b.get(..8).map(|h| u64::from_le_bytes(h.try_into().unwrap()) as usize).ok_or_else(|| damaged("too short"))?;
        let header: serde_json::Value = b.get(8..8usize.saturating_add(hlen)).and_then(|h| serde_json::from_slice(h).ok()).ok_or_else(|| damaged("no safetensors header"))?;
        let data = &b[8 + hlen..];
        let mut levels = Vec::new();
        let mut shape: Option<(usize, usize)> = None;
        while let Some(t) = header.get(format!("level.{}", levels.len())) {
            let dims: Vec<usize> = t["shape"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()).map(|x| x as usize).collect();
            let [k, dim] = dims[..] else { return Err(damaged("a level's shape is [k, dim]")) };
            if shape.is_some_and(|s| s != (k, dim)) {
                return Err(damaged("every level has the same shape"));
            }
            shape = Some((k, dim));
            let offs: Vec<usize> = t["data_offsets"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()).map(|x| x as usize).collect();
            let [a, z] = offs[..] else { return Err(damaged("a level needs data_offsets")) };
            let bytes = data.get(a..z).ok_or_else(|| damaged("a level's data is past the end"))?;
            let width = match t["dtype"].as_str() {
                Some("F32") => 4,
                Some("F64") => 8,
                _ => return Err(damaged("levels are F32 or F64 tensors")),
            };
            if bytes.len() != k * dim * width {
                return Err(damaged("a level's data does not fit its shape"));
            }
            levels.push(match width {
                4 => bytes.chunks_exact(4).map(|x| f32::from_le_bytes(x.try_into().unwrap())).collect(),
                _ => bytes.chunks_exact(8).map(|x| f64::from_le_bytes(x.try_into().unwrap()) as f32).collect(),
            });
        }
        let (k, dim) = shape.ok_or_else(|| damaged("no tensor level.0"))?;
        Codebook::new(dim, k, levels)
    }

    /// The codebook named `name` under `models` (or at an absolute path),
    /// and the SHA-256 of its file.
    pub fn open(name: &str, models: &Path) -> Result<(Arc<Codebook>, String)> {
        let path = path(name, models);
        let bytes = std::fs::read(&path).map_err(|e| bad(format!("codebook {name}: {}: {e}", path.display())))?;
        let cb = Codebook::from_bytes(&bytes).map_err(|e| bad(format!("codebook {name}: {e}")))?;
        Ok((Arc::new(cb), hash(&bytes)))
    }

    /// Writes the codebook as `name` under `models`; returns its SHA-256.
    /// An existing codebook of that name is replaced only by the same one:
    /// fields record the hash of the codebook they were made with.
    pub fn save(&self, name: &str, models: &Path) -> Result<String> {
        if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
            return Err(bad(format!("bad codebook name {name:?}: a directory name in the models directory")));
        }
        let path = path(name, models);
        let bytes = self.to_bytes();
        let h = hash(&bytes);
        if let Ok(old) = std::fs::read(&path) {
            if hash(&old) == h {
                return Ok(h);
            }
            return Err(bad(format!("{} holds another codebook: choose a new name (a field records the codebook it was made with)", path.display())));
        }
        let dir = path.parent().expect("a codebook's directory");
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{FILE}.tmp"));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(h)
    }

    /// Residual k-means over `vectors` (`dim` floats each): each level's
    /// centroids are k-means (Lloyd, from k-means++) of what
    /// the levels before leave over. The same vectors, options and seed
    /// give the same codebook on any number of cores.
    pub fn train(vectors: &[f32], dim: usize, opts: &TrainOptions) -> Result<Codebook> {
        use rand::seq::index::sample;
        use rand::SeedableRng;
        if dim == 0 || vectors.len() % dim != 0 || vectors.is_empty() {
            return Err(bad(format!("training a codebook needs vectors of {dim} dimensions")));
        }
        if !(1..=MAX_LEVELS).contains(&opts.levels) || !(1..=MAX_K).contains(&opts.k) {
            return Err(bad(format!("a codebook has 1 to {MAX_LEVELS} levels of 1 to {MAX_K} centroids")));
        }
        if vectors.iter().any(|x| !x.is_finite()) {
            return Err(bad("training vectors are finite"));
        }
        let n = vectors.len() / dim;
        let mut rng = rand::rngs::StdRng::seed_from_u64(opts.seed);
        let mut picked: Vec<usize> = if n > opts.sample.max(opts.k) { sample(&mut rng, n, opts.sample.max(opts.k)).into_vec() } else { (0..n).collect() };
        picked.sort_unstable();
        let mut rest: Vec<f32> = picked.iter().flat_map(|&i| vectors[i * dim..(i + 1) * dim].iter().copied()).collect();
        let mut levels = Vec::with_capacity(opts.levels);
        for _ in 0..opts.levels {
            let cents = kmeans(&rest, dim, opts.k, opts.iters, &mut rng);
            let norms: Vec<f32> = cents.chunks_exact(dim).map(|r| r.iter().map(|x| x * x).sum()).collect();
            let assigned = assign(&rest, &cents, &norms, dim);
            for (r, &c) in rest.chunks_exact_mut(dim).zip(&assigned) {
                for (x, y) in r.iter_mut().zip(&cents[c as usize * dim..(c as usize + 1) * dim]) {
                    *x -= y;
                }
            }
            levels.push(cents);
        }
        Codebook::new(dim, opts.k, levels)
    }
}

/// Where the codebook `name` lives under `models`.
pub fn path(name: &str, models: &Path) -> PathBuf {
    let dir = if Path::new(name).is_absolute() { PathBuf::from(name) } else { models.join(name) };
    dir.join(FILE)
}

/// The SHA-256 of `bytes`, in hex.
pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// The centroid of `cents` nearest to `v`: the least `|c|² - 2 v·c`.
fn nearest(v: &[f32], cents: &[f32], norms: &[f32], dim: usize) -> usize {
    let mut best = (f32::INFINITY, 0);
    for (c, (row, n)) in cents.chunks_exact(dim).zip(norms).enumerate() {
        let d = n - 2.0 * crate::vector::kernels::dot_f32(v, row);
        if d < best.0 {
            best = (d, c);
        }
    }
    best.1
}

/// The nearest centroid of every point, on the cores.
fn assign(points: &[f32], cents: &[f32], norms: &[f32], dim: usize) -> Vec<u32> {
    let n = points.len() / dim;
    let threads = std::thread::available_parallelism().map_or(1, |t| t.get()).min(n.div_ceil(256)).max(1);
    let per = n.div_ceil(threads).max(1) * dim;
    std::thread::scope(|s| {
        let parts: Vec<_> = points.chunks(per).map(|p| s.spawn(move || p.chunks_exact(dim).map(|v| nearest(v, cents, norms, dim) as u32).collect::<Vec<_>>())).collect();
        parts.into_iter().flat_map(|p| p.join().expect("assignment thread")).collect()
    })
}

/// `k` starting centroids of `points` by k-means++ (Arthur and Vassilvitskii
/// 2007): each next one a point drawn with probability its squared
/// distance to the nearest chosen so far. Points repeat when there are
/// fewer than `k`.
fn kmeans_pp(points: &[f32], dim: usize, k: usize, rng: &mut rand::rngs::StdRng) -> Vec<usize> {
    use rand::Rng;
    let n = points.len() / dim;
    if n <= k {
        return (0..k).map(|i| i % n).collect();
    }
    let row = |i: usize| &points[i * dim..(i + 1) * dim];
    let dist = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>() as f64;
    let mut chosen = vec![rng.gen_range(0..n)];
    let mut d: Vec<f64> = (0..n).map(|i| dist(row(i), row(chosen[0]))).collect();
    while chosen.len() < k {
        let total: f64 = d.iter().sum();
        let next = if total > 0.0 {
            let mut x = rng.gen_range(0.0..total);
            d.iter().position(|&w| {
                x -= w;
                x < 0.0
            })
            .unwrap_or(n - 1)
        } else {
            // Every point is a chosen one: repeat them.
            chosen[chosen.len() % chosen.len().max(1)]
        };
        chosen.push(next);
        let c = row(next);
        for (i, di) in d.iter_mut().enumerate() {
            *di = di.min(dist(row(i), c));
        }
    }
    chosen
}

/// `k` centroids of `points` by Lloyd's algorithm, started by k-means++. A
/// cluster left empty takes the point farthest from its centroid. Sums run
/// in point order, so the result does not depend on the number of cores.
fn kmeans(points: &[f32], dim: usize, k: usize, iters: usize, rng: &mut rand::rngs::StdRng) -> Vec<f32> {
    let n = points.len() / dim;
    let start = kmeans_pp(points, dim, k, rng);
    let mut cents: Vec<f32> = start.iter().flat_map(|&i| points[i * dim..(i + 1) * dim].iter().copied()).collect();
    for _ in 0..iters {
        let norms: Vec<f32> = cents.chunks_exact(dim).map(|r| r.iter().map(|x| x * x).sum()).collect();
        let a = assign(points, &cents, &norms, dim);
        let mut sums = vec![0f64; k * dim];
        let mut counts = vec![0usize; k];
        for (p, &c) in points.chunks_exact(dim).zip(&a) {
            counts[c as usize] += 1;
            for (s, x) in sums[c as usize * dim..(c as usize + 1) * dim].iter_mut().zip(p) {
                *s += *x as f64;
            }
        }
        let mut moved = false;
        for c in 0..k {
            if counts[c] == 0 {
                continue;
            }
            for (x, s) in cents[c * dim..(c + 1) * dim].iter_mut().zip(&sums[c * dim..(c + 1) * dim]) {
                let y = (*s / counts[c] as f64) as f32;
                moved |= y != *x;
                *x = y;
            }
        }
        // Empty clusters take the points worst served, one each.
        let empty: Vec<usize> = (0..k).filter(|&c| counts[c] == 0).collect();
        if !empty.is_empty() && n >= k {
            let mut far: Vec<(f32, usize)> = points
                .chunks_exact(dim)
                .zip(&a)
                .enumerate()
                .map(|(i, (p, &c))| (p.iter().zip(&cents[c as usize * dim..(c as usize + 1) * dim]).map(|(x, y)| (x - y) * (x - y)).sum::<f32>(), i))
                .collect();
            far.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(&y.1)));
            for (&c, &(_, i)) in empty.iter().zip(&far) {
                cents[c * dim..(c + 1) * dim].copy_from_slice(&points[i * dim..(i + 1) * dim]);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    cents
}

// ---------------------------------------------------------------- values

/// The value of a semantic-ID field for a code of `levels` numbers and
/// `tie`: every prefix, shortest first.
pub fn value(levels: &[u32], tie: u32) -> Value {
    let mut out = Vec::with_capacity(levels.len() + 1);
    let mut s = String::new();
    for x in levels.iter().chain([&tie]) {
        if !s.is_empty() {
            s.push('.');
        }
        s.push_str(&x.to_string());
        out.push(Value::Str(s.clone()));
    }
    Value::List(out)
}

/// The prefixes [`Db::sid_next`](crate::db::Db::sid_next) takes at most
/// in one call.
pub const MAX_PREFIXES: usize = 64;

/// The numbers of a code written `12.7.201.0`.
pub fn parse(code: &str) -> Option<Vec<u32>> {
    code.split('.').map(|x| if x.is_empty() || (x.len() > 1 && x.starts_with('0')) { None } else { x.parse().ok() }).collect()
}

/// The numbers of the longest code a semantic-ID value holds, if every
/// value is a prefix of it and it has every prefix (the value a write
/// gives must be well formed).
pub fn code_of(v: &Value) -> Option<Vec<u32>> {
    let Value::List(xs) = v else { return None };
    let strs: Vec<&str> = xs.iter().map(|x| x.as_str()).collect::<Option<_>>()?;
    let longest = strs.iter().max_by_key(|s| s.len())?;
    let nums = parse(longest)?;
    let mut prefixes: Vec<String> = (1..=nums.len()).map(|n| nums[..n].iter().map(|x| x.to_string()).collect::<Vec<_>>().join(".")).collect();
    let mut given: Vec<String> = strs.iter().map(|s| s.to_string()).collect();
    prefixes.sort();
    given.sort();
    given.dedup();
    (prefixes == given).then_some(nums)
}

/// `sid_at(sid, n)`: the level-`n` prefix (from 1) of a semantic-ID value,
/// or of a code; NULL past its last level, for `n` below 1, or for NULL.
pub fn sid_at(v: &Value, n: &Value) -> Result<Value> {
    let n = match n {
        Value::Int(n) => *n,
        Value::Null => return Ok(Value::Null),
        v => return Err(Error::Query(format!("sid_at(sid, n) takes a level number, not {v:?}"))),
    };
    let code = match v {
        Value::Null => return Ok(Value::Null),
        Value::Str(s) => s.as_str(),
        Value::List(xs) => match xs.iter().filter_map(|x| x.as_str()).max_by_key(|s| s.len()) {
            Some(s) => s,
            None => return Ok(Value::Null),
        },
        v => return Err(Error::Query(format!("sid_at(sid, n) takes a semantic ID, not {v:?}"))),
    };
    if n < 1 {
        return Ok(Value::Null);
    }
    let parts: Vec<&str> = code.split('.').collect();
    Ok(match parts.get(..n as usize) {
        Some(p) => Value::Str(p.join(".")),
        None => Value::Null,
    })
}

/// The first `levels` numbers of a code, 8 bits each, as one integer.
pub fn pack(code: &[u32]) -> u32 {
    code.iter().fold(0u32, |p, &x| p << 8 | (x & 0xff))
}

// ------------------------------------------------------------------ ties

/// The section of segment fields' dropped codes.
pub fn gone_section(field: &str) -> String {
    format!("sid.{field}.gone")
}

/// The bytes of a [`gone_section`]: each prefix ([`pack`]) and the next tie
/// under it, both `u32` little-endian, by prefix.
pub fn encode_gone(next: &HashMap<u32, u32>) -> Vec<u8> {
    let mut pairs: Vec<(u32, u32)> = next.iter().map(|(&p, &t)| (p, t)).collect();
    pairs.sort_unstable();
    pairs.iter().flat_map(|&(p, t)| p.to_le_bytes().into_iter().chain(t.to_le_bytes())).collect()
}

pub fn decode_gone(b: &[u8]) -> Result<Vec<(u32, u32)>> {
    if b.len() % 8 != 0 {
        return Err(crate::error::corrupt("a semantic-ID gone section is pairs of u32"));
    }
    Ok(b.chunks_exact(8).map(|c| (u32::from_le_bytes(c[..4].try_into().unwrap()), u32::from_le_bytes(c[4..].try_into().unwrap()))).collect())
}

/// The next tie under each prefix of a table's semantic-ID fields: one
/// more than the highest given, by field.
#[derive(Clone, Debug, Default)]
pub struct Ties {
    pub next: HashMap<usize, HashMap<u32, u32>>,
}

impl Ties {
    /// The next tie of field `f` under `prefix`.
    pub fn get(&self, f: usize, prefix: u32) -> u32 {
        self.next.get(&f).and_then(|m| m.get(&prefix)).copied().unwrap_or(0)
    }

    /// Records that `code` (levels and tie) was given in field `f`.
    pub fn saw(&mut self, f: usize, code: &[u32]) {
        if let Some((&tie, levels)) = code.split_last() {
            let e = self.next.entry(f).or_default().entry(pack(levels)).or_insert(0);
            *e = (*e).max(tie.saturating_add(1));
        }
    }

    /// Records the codes `v`, a value of field `f`, holds.
    pub fn saw_value(&mut self, f: usize, v: &Value) {
        if let Some(c) = code_of(v) {
            self.saw(f, &c);
        }
    }

    /// Records the codes of `doc`, a row of the table `schema` describes.
    pub fn saw_doc(&mut self, schema: &Schema, doc: &[Value]) {
        for (f, field) in schema.fields.iter().enumerate() {
            if field.semantic_id.is_some() {
                if let Some(v) = doc.get(f) {
                    self.saw_value(f, v);
                }
            }
        }
    }

    /// Adds every tie `other` has given.
    pub fn absorb(&mut self, other: &Ties) {
        for (&f, m) in &other.next {
            let mine = self.next.entry(f).or_default();
            for (&p, &t) in m {
                let e = mine.entry(p).or_insert(0);
                *e = (*e).max(t);
            }
        }
    }

    /// Adds the ties of `seg`: of the codes its dictionaries hold (those of
    /// its deleted documents too) and of those it dropped.
    pub fn scan(&mut self, schema: &Schema, seg: &SegmentFile) -> Result<()> {
        for (f, field) in schema.fields.iter().enumerate() {
            if field.semantic_id.is_none() {
                continue;
            }
            if let Some(b) = seg.section(&gone_section(&field.name))? {
                for (p, t) in decode_gone(&b)? {
                    let e = self.next.entry(f).or_default().entry(p).or_insert(0);
                    *e = (*e).max(t);
                }
            }
            if !seg.has(&format!("kw.{}.dict", field.name)) {
                continue;
            }
            let col = crate::forward::KeywordColumn::open(seg, &field.name)?;
            let n = col.n_values() as u32;
            for c in 0..n {
                // A full code is a value no other extends: in byte order a
                // value's extensions (`v.` ...) come right after it.
                let v = col.value(c);
                let extended = c + 1 < n && {
                    let w = col.value(c + 1);
                    w.len() > v.len() && w.starts_with(v) && w.as_bytes()[v.len()] == b'.'
                };
                if let Some(code) = parse(v).filter(|code| !extended && (2..=MAX_LEVELS + 1).contains(&code.len())) {
                    self.saw(f, &code);
                }
            }
        }
        Ok(())
    }
}

/// The [`gone_section`]s of a segment that keeps documents whose codes
/// give the ties `kept`, and drops codes that gave `dropped` (those of its
/// deleted documents, and those the segments it replaces dropped): each
/// prefix whose next tie only the dropped codes know.
pub fn gone_sections(schema: &Schema, dropped: &Ties, kept: &Ties) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for (f, field) in schema.fields.iter().enumerate() {
        let Some(m) = dropped.next.get(&f) else { continue };
        let gone: HashMap<u32, u32> = m.iter().filter(|&(&p, &t)| kept.get(f, p) < t).map(|(&p, &t)| (p, t)).collect();
        if !gone.is_empty() {
            out.push((gone_section(&field.name), encode_gone(&gone)));
        }
    }
    out
}

/// The semantic-ID fields of `schema`: (field, its vector field).
pub fn fields(schema: &Schema) -> Vec<(usize, usize)> {
    schema
        .fields
        .iter()
        .enumerate()
        .filter_map(|(i, f)| {
            let s = f.semantic_id.as_ref()?;
            Some((i, schema.field_index(&s.from)?))
        })
        .collect()
}

/// Fills in the hash of each semantic-ID field of `schema` that records
/// none from its codebook under `models`, and checks the codebooks of
/// those that do record one: a table is created with the codebook its
/// codes will come from.
pub fn resolve(schema: &mut Schema, models: &Path) -> Result<()> {
    let dims: Vec<Option<usize>> = schema
        .fields
        .iter()
        .map(|f| {
            let src = schema.field(&f.semantic_id.as_ref()?.from)?;
            match src.ty {
                FieldType::Vector { dim, .. } => Some(dim),
                _ => None,
            }
        })
        .collect();
    for (f, dim) in schema.fields.iter_mut().zip(dims) {
        let Some(s) = &mut f.semantic_id else { continue };
        let opened = Codebook::open(&s.codebook, models);
        let (cb, h) = match opened {
            Ok(x) => x,
            // A codebook recorded by hash may come later (copied with the
            // database); writes need it.
            Err(_) if !s.hash.is_empty() => continue,
            Err(e) => return Err(bad(format!("field {}: {e} (a semantic-ID field is created with its codebook; train one with strata.train.codebook)", f.name))),
        };
        if !s.hash.is_empty() && s.hash != h {
            return Err(bad(format!("field {}: codebook {} is not the one the field records (its hash is {h}, the field's {})", f.name, s.codebook, s.hash)));
        }
        if Some(cb.dim) != dim {
            return Err(bad(format!("field {}: codebook {} codes {}-dimension vectors; {} holds {}", f.name, s.codebook, cb.dim, s.from, dim.unwrap_or(0))));
        }
        s.hash = h;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random(n: usize, dim: usize, seed: u64) -> Vec<f32> {
        use rand::{Rng, SeedableRng};
        let mut r = rand::rngs::StdRng::seed_from_u64(seed);
        (0..n * dim).map(|_| r.gen_range(-1.0..1.0)).collect()
    }

    #[test]
    fn codes_are_residual_nearest_centroids() {
        // Two levels in one dimension: 0 or 10, then -1 or +1.
        let cb = Codebook::new(1, 2, vec![vec![0.0, 10.0], vec![-1.0, 1.0]]).unwrap();
        assert_eq!(cb.code(&[9.2]), [1, 0]);
        assert_eq!(cb.code(&[10.8]), [1, 1]);
        assert_eq!(cb.code(&[-0.7]), [0, 0]);
        assert!(Codebook::new(1, 300, vec![vec![0.0; 300]]).is_err());
        assert!(Codebook::new(2, 2, vec![vec![0.0; 3]]).is_err());
    }

    #[test]
    fn files_round_trip_and_training_is_seeded() {
        let v = random(2000, 8, 1);
        let opts = TrainOptions { levels: 3, k: 16, iters: 10, sample: 1000, seed: 7 };
        let a = Codebook::train(&v, 8, &opts).unwrap();
        assert_eq!(a, Codebook::train(&v, 8, &opts).unwrap());
        assert_ne!(a, Codebook::train(&v, 8, &TrainOptions { seed: 8, ..opts.clone() }).unwrap());
        assert_eq!((a.n_levels(), a.k, a.dim), (3, 16, 8));
        let bytes = a.to_bytes();
        assert_eq!(Codebook::from_bytes(&bytes).unwrap(), a);
        assert_eq!(&bytes[8..9], b"{");
        // Each level makes the leftover smaller.
        let err = |levels: usize| -> f32 {
            v.chunks_exact(8)
                .map(|x| {
                    let mut r = x.to_vec();
                    for (l, c) in a.code(x).iter().take(levels).enumerate() {
                        for (y, z) in r.iter_mut().zip(&a.levels[l][*c as usize * 8..(*c as usize + 1) * 8]) {
                            *y -= z;
                        }
                    }
                    r.iter().map(|y| y * y).sum::<f32>()
                })
                .sum()
        };
        assert!(err(1) < err(0) && err(2) < err(1) && err(3) < err(2));
        // Fewer vectors than centroids still train.
        assert!(Codebook::train(&v[..5 * 8], 8, &opts).is_ok());
        assert!(Codebook::from_bytes(&bytes[..bytes.len() - 4]).is_err());
    }

    #[test]
    fn saving_keeps_a_name_for_one_codebook() {
        let dir = tempfile::tempdir().unwrap();
        let cb = Codebook::new(2, 2, vec![vec![0.0, 1.0, 1.0, 0.0]]).unwrap();
        let h = cb.save("toy", dir.path()).unwrap();
        assert_eq!(cb.save("toy", dir.path()).unwrap(), h);
        let (back, h2) = Codebook::open("toy", dir.path()).unwrap();
        assert_eq!((*back == cb, h2 == h), (true, true));
        let other = Codebook::new(2, 2, vec![vec![0.0, 1.0, 1.0, 0.5]]).unwrap();
        assert!(other.save("toy", dir.path()).unwrap_err().to_string().contains("another codebook"));
        assert!(cb.save("../x", dir.path()).is_err());
    }

    #[test]
    fn values_hold_every_prefix() {
        let v = value(&[12, 7, 201], 0);
        assert_eq!(v, Value::List(["12", "12.7", "12.7.201", "12.7.201.0"].iter().map(|s| Value::Str(s.to_string())).collect()));
        assert_eq!(code_of(&v), Some(vec![12, 7, 201, 0]));
        let s = |xs: &[&str]| Value::List(xs.iter().map(|x| Value::Str(x.to_string())).collect());
        assert_eq!(code_of(&s(&["12.7", "12"])), Some(vec![12, 7]));
        assert_eq!(code_of(&s(&["12", "12.7.1"])), None);
        assert_eq!(code_of(&s(&["12", "13.7"])), None);
        assert_eq!(code_of(&s(&["012"])), None);
        assert_eq!(sid_at(&v, &Value::Int(2)).unwrap(), Value::Str("12.7".into()));
        assert_eq!(sid_at(&v, &Value::Int(4)).unwrap(), Value::Str("12.7.201.0".into()));
        assert_eq!(sid_at(&v, &Value::Int(5)).unwrap(), Value::Null);
        assert_eq!(sid_at(&v, &Value::Int(0)).unwrap(), Value::Null);
        assert_eq!(sid_at(&Value::Str("3.4.5".into()), &Value::Int(1)).unwrap(), Value::Str("3".into()));
        assert!(sid_at(&v, &Value::Str("2".into())).is_err());
        assert_eq!(pack(&[12, 7, 201]), 12 << 16 | 7 << 8 | 201);
        let mut next = HashMap::new();
        next.insert(pack(&[1, 2, 3]), 4);
        next.insert(pack(&[0, 0, 9]), 1);
        assert_eq!(decode_gone(&encode_gone(&next)).unwrap(), vec![(9, 1), (pack(&[1, 2, 3]), 4)]);
    }
}
