//! Sequence encoders: `encode('model', (SELECT item, action, ts FROM ...
//! ORDER BY ts DESC LIMIT 200))` turns a history into a vector in the
//! space of an item table's VECTOR field, which then ranks the items as any
//! query vector does (`similarity(emb, encode(...))`, `maxsim`, `dot`).
//!
//! The model is a small causal transformer over the history, oldest event
//! first, as SASRec is, but over the items' vectors rather than over item
//! ids: each step is the item's vector (from the field the model names)
//! projected to the model's width, plus an embedding of the event's action,
//! of the time gap after it (to the next event, and for the newest to the
//! time of the request: [`gap_bucket`]), and of its position counted from
//! the oldest step. Pre-norm layers follow (attention over the steps before
//! and at each step, then a ReLU feed-forward, each added to its input);
//! the newest step's output, normalized, projected to the item dimension
//! and made a unit vector, is the history's vector.
//!
//! A model is a directory of the models directory, as embedders and ranking
//! models are: `<models>/<name>/config.json` (the item table, field and
//! dimension, the sizes, the actions) and `model.safetensors` (F32
//! tensors, named as strata-py's `strata.train.sequence` writes them; see
//! [`SequenceModel::parse`]). PyTorch trains it; this module only runs it,
//! in plain Rust over the vector kernels, since a history of 200 steps
//! through two layers of width 64 is a few million multiply-adds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::Value as Json;

use crate::error::{Error, Result};
use crate::vector::kernels::{dot_f32, dot_f32_4x4};

/// The files of a sequence model in its directory.
pub const CONFIG_FILE: &str = "config.json";
pub const WEIGHTS_FILE: &str = "model.safetensors";
/// The item vectors the model was trained with (learned with it, or read),
/// which strata-py's trainer writes beside it: the F32 tensor `vectors`
/// (one row an item) and the items' keys, a JSON array in the metadata's
/// `keys`. A model with them is versioned with them (its hash covers the
/// file), and serves only over a table whose vectors are those.
pub const ITEMS_FILE: &str = "items.safetensors";

/// The most events of a history a model may read.
pub const MAX_LEN: usize = 4096;

fn bad(name: &str, what: impl std::fmt::Display) -> Error {
    Error::Query(format!("sequence model {name}: {what}"))
}

/// The bucket of a time gap in seconds: 0 for none (or none known, or under
/// a second), else 1 + the whole part of its log2, at most `buckets - 1`.
pub fn gap_bucket(seconds: Option<f64>, buckets: usize) -> usize {
    match seconds {
        Some(s) if s >= 1.0 => (1 + s.log2().floor() as usize).min(buckets - 1),
        _ => 0,
    }
}

/// `y = W x + b` for each of a matrix's rows: PyTorch's `Linear`, whose
/// weight is `out` rows of `inp`.
struct Linear {
    w: Vec<f32>,
    b: Vec<f32>,
    inp: usize,
    out: usize,
}

impl Linear {
    /// `x` is `n` rows of `inp`; the answer `n` rows of `out`.
    fn apply(&self, x: &[f32], n: usize) -> Vec<f32> {
        let (i, o) = (self.inp, self.out);
        let mut y = vec![0f32; n * o];
        fn row(m: &[f32], r: usize, i: usize) -> &[f32] {
            &m[r * i..(r + 1) * i]
        }
        let (n4, o4) = (n / 4 * 4, o / 4 * 4);
        for r in (0..n4).step_by(4) {
            let xs = [row(x, r, i), row(x, r + 1, i), row(x, r + 2, i), row(x, r + 3, i)];
            for c in (0..o4).step_by(4) {
                let d = dot_f32_4x4(xs, [row(&self.w, c, i), row(&self.w, c + 1, i), row(&self.w, c + 2, i), row(&self.w, c + 3, i)]);
                for (a, d) in d.iter().enumerate() {
                    for (b, v) in d.iter().enumerate() {
                        y[(r + a) * o + c + b] = v + self.b[c + b];
                    }
                }
            }
            for (a, x) in xs.iter().enumerate() {
                for c in o4..o {
                    y[(r + a) * o + c] = dot_f32(x, row(&self.w, c, i)) + self.b[c];
                }
            }
        }
        for r in n4..n {
            for c in 0..o {
                y[r * o + c] = dot_f32(row(x, r, i), row(&self.w, c, i)) + self.b[c];
            }
        }
        y
    }
}

/// PyTorch's `LayerNorm` over each row.
struct Norm {
    w: Vec<f32>,
    b: Vec<f32>,
    eps: f32,
}

impl Norm {
    fn apply(&self, x: &[f32]) -> Vec<f32> {
        let d = self.w.len();
        let mut y = vec![0f32; x.len()];
        for (x, y) in x.chunks_exact(d).zip(y.chunks_exact_mut(d)) {
            let mean = x.iter().sum::<f32>() / d as f32;
            let var = x.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / d as f32;
            let s = 1.0 / (var + self.eps).sqrt();
            for (((y, x), w), b) in y.iter_mut().zip(x).zip(&self.w).zip(&self.b) {
                *y = (x - mean) * s * w + b;
            }
        }
        y
    }
}

struct Layer {
    ln1: Norm,
    qkv: Linear,
    proj: Linear,
    ln2: Norm,
    up: Linear,
    down: Linear,
}

/// An event of a history, as the model reads it: the item's vector, the
/// action, the time in seconds since the Unix epoch.
#[derive(Clone, Copy, Debug)]
pub struct Event<'a> {
    pub item: &'a [f32],
    pub action: Option<&'a str>,
    pub time: Option<f64>,
}

/// A sequence encoder ([module docs](self)).
pub struct SequenceModel {
    pub name: String,
    /// FNV-1a of the config, the weights and the item vectors (if it has
    /// them), as 16 hex digits.
    pub hash: String,
    /// The item table and VECTOR field whose vectors the model reads and
    /// answers in, and their dimension.
    pub table: String,
    pub field: String,
    pub dim: usize,
    /// The most events it reads: the newest ones.
    pub max_len: usize,
    width: usize,
    heads: usize,
    buckets: usize,
    /// Action names, by index; 0 is any other (and none).
    actions: HashMap<String, usize>,
    /// The item vectors it was trained with ([`ITEMS_FILE`]): each item's
    /// row in `item_rows`, by key.
    items: HashMap<ItemKey, usize>,
    item_rows: Vec<f32>,
    inp: Linear,
    action: Vec<f32>,
    gap: Vec<f32>,
    position: Vec<f32>,
    layers: Vec<Layer>,
    ln: Norm,
    out: Linear,
}

impl std::fmt::Debug for SequenceModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SequenceModel({} {}, {}.{} of {}, {} layers of {})", self.name, self.hash, self.table, self.field, self.dim, self.layers.len(), self.width)
    }
}

impl PartialEq for SequenceModel {
    fn eq(&self, o: &SequenceModel) -> bool {
        self.name == o.name && self.hash == o.hash
    }
}

/// The F32 tensors of a safetensors file, by name: their shapes and values.
fn tensors(name: &str, file: &str, raw: &[u8]) -> Result<HashMap<String, (Vec<usize>, Vec<f32>)>> {
    let bad = |m: &str| bad(name, format!("{file}: {m}"));
    let n = raw.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize).ok_or_else(|| bad("not a safetensors file"))?;
    let header: serde_json::Map<String, Json> = raw.get(8..8 + n).and_then(|h| serde_json::from_slice(h).ok()).ok_or_else(|| bad("a header that is not JSON"))?;
    let data = &raw[8 + n..];
    let mut out = HashMap::new();
    for (k, t) in header {
        if k == "__metadata__" {
            continue;
        }
        if t["dtype"] != "F32" {
            return Err(bad(&format!("{k} is {} (need F32)", t["dtype"])));
        }
        let shape: Vec<usize> = t["shape"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()).map(|x| x as usize).collect();
        let offs: Vec<usize> = t["data_offsets"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()).map(|x| x as usize).collect();
        let [a, b] = offs[..] else { return Err(bad(&format!("{k} has no data offsets"))) };
        let bytes = data.get(a..b).filter(|s| s.len() == shape.iter().product::<usize>() * 4).ok_or_else(|| bad(&format!("{k}: {} bytes for shape {shape:?}", b.saturating_sub(a))))?;
        out.insert(k, (shape, bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()));
    }
    Ok(out)
}

/// An item's key as [`ITEMS_FILE`] lists it: text or a whole number.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ItemKey {
    Str(String),
    Int(i64),
}

/// The item vectors of an [`ITEMS_FILE`], `dim` components each.
fn item_vectors(name: &str, raw: &[u8], dim: usize) -> Result<(HashMap<ItemKey, usize>, Vec<f32>)> {
    let fail = |m: String| bad(name, format!("{ITEMS_FILE}: {m}"));
    let n = raw.get(..8).map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize).ok_or_else(|| fail("not a safetensors file".into()))?;
    let header: Json = raw.get(8..8 + n).and_then(|h| serde_json::from_slice(h).ok()).ok_or_else(|| fail("a header that is not JSON".into()))?;
    let keys: Vec<Json> = header["__metadata__"]["keys"].as_str().and_then(|k| serde_json::from_str(k).ok()).ok_or_else(|| fail("no keys in its metadata (a JSON array)".into()))?;
    let mut ts = tensors(name, ITEMS_FILE, raw)?;
    let (shape, rows) = ts.remove("vectors").ok_or_else(|| fail("no tensor named vectors".into()))?;
    if shape != [keys.len(), dim] {
        return Err(fail(format!("vectors has shape {shape:?}, not {} keys by {dim}", keys.len())));
    }
    let mut index = HashMap::with_capacity(keys.len());
    for (i, k) in keys.into_iter().enumerate() {
        let key = match k {
            Json::String(s) => ItemKey::Str(s),
            Json::Number(n) if n.is_i64() => ItemKey::Int(n.as_i64().unwrap()),
            k => return Err(fail(format!("a key is text or a whole number, not {k}"))),
        };
        if index.insert(key, i).is_some() {
            return Err(fail("a key is listed twice".into()));
        }
    }
    Ok((index, rows))
}

impl SequenceModel {
    /// The vector the model was trained with for item `key`, if it keeps
    /// its items' ([`ITEMS_FILE`]) and has that one.
    pub fn item_vector(&self, key: &crate::value::Value) -> Option<&[f32]> {
        let k = match key {
            crate::value::Value::Str(s) => ItemKey::Str(s.clone()),
            crate::value::Value::Int(n) => ItemKey::Int(*n),
            _ => return None,
        };
        self.items.get(&k).map(|&i| &self.item_rows[i * self.dim..(i + 1) * self.dim])
    }

    /// Whether the model keeps the item vectors it was trained with.
    pub fn has_items(&self) -> bool {
        !self.items.is_empty()
    }

    /// A model from its files ([module docs](self)), the item vectors'
    /// ([`ITEMS_FILE`]) if it has them. The config is
    /// `{"table", "field", "dim", "width", "layers", "heads", "ffn",
    /// "max_len", "gap_buckets", "actions", "eps"}`; the weights are
    /// `inp` (dim to width), `action`, `gap`, `position` (embeddings),
    /// `layers.<i>.{ln1, qkv, proj, ln2, up, down}` and `ln`, `out` (width
    /// to dim), each `.weight` and `.bias` (`.weight` alone for an
    /// embedding).
    pub fn parse(name: &str, config: &[u8], weights: &[u8], items: Option<&[u8]>) -> Result<SequenceModel> {
        let c: Json = serde_json::from_slice(config).map_err(|e| bad(name, format!("{CONFIG_FILE} is not JSON: {e}")))?;
        let num = |k: &str| c.get(k).and_then(|v| v.as_u64()).filter(|&v| v > 0).map(|v| v as usize).ok_or_else(|| bad(name, format!("{CONFIG_FILE} has no {k} (a positive whole number)")));
        let text = |k: &str| c.get(k).and_then(|v| v.as_str()).map(String::from).ok_or_else(|| bad(name, format!("{CONFIG_FILE} has no {k}")));
        if c.get("kind").and_then(|k| k.as_str()) != Some("sequence") {
            return Err(bad(name, format!("{CONFIG_FILE} is not a sequence model's (kind \"sequence\")")));
        }
        let (dim, width, heads, ffn, max_len, buckets, n_layers) = (num("dim")?, num("width")?, num("heads")?, num("ffn")?, num("max_len")?, num("gap_buckets")?, num("layers")?);
        if width % heads != 0 || max_len > MAX_LEN || n_layers > 64 {
            return Err(bad(name, format!("width {width} is not a multiple of heads {heads}, or max_len is over {MAX_LEN}, or layers over 64")));
        }
        let eps = c.get("eps").and_then(|v| v.as_f64()).unwrap_or(1e-5) as f32;
        let actions: Vec<String> = c.get("actions").and_then(|a| a.as_array()).map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect()).unwrap_or_else(|| vec![String::new()]);
        if actions.is_empty() {
            return Err(bad(name, "actions lists at least the action of index 0, \"\" (any other)"));
        }
        let mut ts = tensors(name, WEIGHTS_FILE, weights)?;
        let mut take = |k: &str, shape: &[usize]| {
            let (s, v) = ts.remove(k).ok_or_else(|| bad(name, format!("{WEIGHTS_FILE} has no tensor {k}")))?;
            if s != shape {
                return Err(bad(name, format!("{k} has shape {s:?}, the config says {shape:?}")));
            }
            Ok(v)
        };
        let mut linear = |k: &str, inp: usize, out: usize| -> Result<Linear> { Ok(Linear { w: take(&format!("{k}.weight"), &[out, inp])?, b: take(&format!("{k}.bias"), &[out])?, inp, out }) };
        let inp = linear("inp", dim, width)?;
        let out = linear("out", width, dim)?;
        let mut layers = Vec::new();
        for l in 0..n_layers {
            let p = format!("layers.{l}");
            let (qkv, proj, up, down) = (linear(&format!("{p}.qkv"), width, 3 * width)?, linear(&format!("{p}.proj"), width, width)?, linear(&format!("{p}.up"), width, ffn)?, linear(&format!("{p}.down"), ffn, width)?);
            layers.push((qkv, proj, up, down));
        }
        let mut norm = |k: &str| -> Result<Norm> { Ok(Norm { w: take(&format!("{k}.weight"), &[width])?, b: take(&format!("{k}.bias"), &[width])?, eps }) };
        let mut built = Vec::new();
        for (l, (qkv, proj, up, down)) in layers.into_iter().enumerate() {
            built.push(Layer { ln1: norm(&format!("layers.{l}.ln1"))?, qkv, proj, ln2: norm(&format!("layers.{l}.ln2"))?, up, down });
        }
        let ln = norm("ln")?;
        let action = take("action.weight", &[actions.len(), width])?;
        let gap = take("gap.weight", &[buckets, width])?;
        let position = take("position.weight", &[max_len, width])?;
        if let Some(k) = ts.keys().next() {
            return Err(bad(name, format!("{WEIGHTS_FILE} has a tensor the config does not name: {k}")));
        }
        let mut h = fnv(config);
        h = fnv_more(h, weights);
        let (items, item_rows) = match items {
            Some(raw) => {
                h = fnv_more(h, raw);
                item_vectors(name, raw, dim)?
            }
            None => (HashMap::new(), Vec::new()),
        };
        Ok(SequenceModel {
            name: name.to_string(),
            hash: format!("{h:016x}"),
            table: text("table")?,
            field: text("field")?,
            dim,
            max_len,
            width,
            heads,
            buckets,
            actions: actions.into_iter().enumerate().skip(1).map(|(i, a)| (a, i)).collect(),
            items,
            item_rows,
            inp,
            action,
            gap,
            position,
            layers: built,
            ln,
            out,
        })
    }

    /// The vector of `history` (oldest event first; the model reads its
    /// newest [`SequenceModel::max_len`]) at time `now` (seconds since the
    /// Unix epoch): a unit vector of [`SequenceModel::dim`] components, or
    /// `None` for an empty history.
    pub fn encode(&self, history: &[Event], now: Option<f64>) -> Option<Vec<f32>> {
        let h = &history[history.len().saturating_sub(self.max_len)..];
        let n = h.len();
        if n == 0 {
            return None;
        }
        let w = self.width;
        let items: Vec<f32> = h.iter().flat_map(|e| e.item.iter().copied()).collect();
        let mut x = self.inp.apply(&items, n);
        for (i, e) in h.iter().enumerate() {
            let next = if i + 1 < n { h[i + 1].time } else { now };
            let gap = gap_bucket(next.zip(e.time).map(|(t, s)| t - s), self.buckets);
            let action = e.action.and_then(|a| self.actions.get(a)).copied().unwrap_or(0);
            let row = &mut x[i * w..(i + 1) * w];
            for (j, v) in row.iter_mut().enumerate() {
                *v += self.action[action * w + j] + self.gap[gap * w + j] + self.position[i * w + j];
            }
        }
        let last = self.layers.len() - 1;
        for (l, layer) in self.layers.iter().enumerate() {
            // The last layer's output is needed at the newest step only.
            x = self.layer(layer, x, n, if l == last { n - 1 } else { 0 });
        }
        let mut y = self.out.apply(&self.ln.apply(&x[x.len() - w..]), 1);
        let norm = y.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
        y.iter_mut().for_each(|v| *v /= norm);
        Some(y)
    }

    /// A layer over `n` steps, answering for steps `from..n`.
    fn layer(&self, l: &Layer, x: Vec<f32>, n: usize, from: usize) -> Vec<f32> {
        let (w, heads) = (self.width, self.heads);
        let dh = w / heads;
        let scale = 1.0 / (dh as f32).sqrt();
        let qkv = l.qkv.apply(&l.ln1.apply(&x), n);
        let m = n - from;
        let mut att = vec![0f32; m * w];
        let mut p = vec![0f32; n];
        for (r, i) in (from..n).enumerate() {
            for hd in 0..heads {
                let q = &qkv[i * 3 * w + hd * dh..][..dh];
                let mut top = f32::NEG_INFINITY;
                for (j, p) in p.iter_mut().enumerate().take(i + 1) {
                    *p = dot_f32(q, &qkv[j * 3 * w + w + hd * dh..][..dh]) * scale;
                    top = top.max(*p);
                }
                let mut sum = 0.0;
                for p in &mut p[..=i] {
                    *p = (*p - top).exp();
                    sum += *p;
                }
                let out = &mut att[r * w + hd * dh..][..dh];
                for (j, p) in p[..=i].iter().enumerate() {
                    let v = &qkv[j * 3 * w + 2 * w + hd * dh..][..dh];
                    let p = p / sum;
                    for (o, v) in out.iter_mut().zip(v) {
                        *o += p * v;
                    }
                }
            }
        }
        let mut y = l.proj.apply(&att, m);
        for (y, x) in y.iter_mut().zip(&x[from * w..]) {
            *y += x;
        }
        let f = l.down.apply(&l.up.apply(&l.ln2.apply(&y), m).into_iter().map(|v| v.max(0.0)).collect::<Vec<_>>(), m);
        for (y, f) in y.iter_mut().zip(f) {
            *y += f;
        }
        y
    }
}

fn fnv(b: &[u8]) -> u64 {
    fnv_more(0xcbf29ce484222325, b)
}

fn fnv_more(mut h: u64, b: &[u8]) -> u64 {
    for &c in b {
        h ^= c as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The sequence models of models directories, each read again when its
/// files change.
#[derive(Default)]
pub struct SequenceModels {
    open: Mutex<HashMap<PathBuf, (Stamp, Arc<SequenceModel>)>>,
}

type Stamp = [(SystemTime, u64); 3];

impl SequenceModels {
    /// The model `name` of the directory `dir`.
    pub fn get(&self, dir: &Path, name: &str) -> Result<Arc<SequenceModel>> {
        if name.is_empty() || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)) {
            return Err(Error::Query(format!("a sequence model's name is letters, digits, _, - and ., not {name:?}")));
        }
        let at = dir.join(name);
        let (config, weights) = (at.join(CONFIG_FILE), at.join(WEIGHTS_FILE));
        let stamp = |p: &Path| {
            std::fs::metadata(p).map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len())).map_err(|_| Error::Query(format!("no sequence model named {name}: no file {}", p.display())))
        };
        let items = at.join(ITEMS_FILE);
        let s = [stamp(&config)?, stamp(&weights)?, std::fs::metadata(&items).map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len())).unwrap_or((SystemTime::UNIX_EPOCH, 0))];
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, m)) = open.get(&at) {
            if *t == s {
                return Ok(m.clone());
            }
        }
        let raw_items = match std::fs::read(&items) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let m = Arc::new(SequenceModel::parse(name, &std::fs::read(&config)?, &std::fs::read(&weights)?, raw_items.as_deref())?);
        open.insert(at, (s, m.clone()));
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_bucket_by_log2_seconds() {
        let b = |s: f64| gap_bucket(Some(s), 32);
        assert_eq!((gap_bucket(None, 32), b(-5.0), b(0.5), b(1.0), b(1.9), b(2.0), b(3600.0), b(1e30)), (0, 0, 0, 1, 1, 2, 12, 31));
    }

    #[test]
    fn linear_takes_every_row_and_column() {
        // 5 rows of 3 into 6: the 4x4 blocks and both remainders.
        let l = Linear { w: (0..18).map(|v| v as f32).collect(), b: (0..6).map(|v| v as f32 * 0.5).collect(), inp: 3, out: 6 };
        let x: Vec<f32> = (0..15).map(|v| (v as f32) - 7.0).collect();
        let y = l.apply(&x, 5);
        for r in 0..5 {
            for c in 0..6 {
                let want: f32 = (0..3).map(|k| x[r * 3 + k] * l.w[c * 3 + k]).sum::<f32>() + l.b[c];
                assert_eq!(y[r * 6 + c], want, "{r} {c}");
            }
        }
    }
}
