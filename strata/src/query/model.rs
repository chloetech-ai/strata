//! Ranking models: a model that scores a row from values a query computes
//! for it, as a step of its ranking.
//!
//! `gbdt('name', f1, f2, ...)` is a gradient-boosted tree ensemble over
//! the features `f1, f2, ...` (scores, columns, any ranking expression of
//! one row), trained by LightGBM or XGBoost and saved as JSON:
//!
//! * LightGBM: `booster.dump_model()` written with `json.dump`
//!   (`"tree_info"`). Numerical and categorical splits, the three missing
//!   types. The score is the raw one (`predict(..., raw_score=True)`),
//!   summed in `f64` in tree order as LightGBM sums it, so the two agree
//!   bit for bit; a random forest's (`boosting="rf"`) is the sum of its
//!   trees, as there, not their mean.
//! * XGBoost: `booster.save_model("model.json")` (`"learner"`), a `gbtree`
//!   with numerical splits. The score is the margin
//!   (`predict(..., output_margin=True)`), compared and summed in `f32` as
//!   XGBoost does, from the base score in margin space.
//!
//! Either way the score is a raw margin: `sigmoid(gbdt(..))` is a binary
//! model's probability. NULL (or NaN) is a missing value, and the model's
//! splits send it where it was trained to go.
//!
//! A model lives in the models directory, as embedders do:
//! `<models>/<name>/gbdt.json`. Its hash ([`Gbdt::hash`], FNV-1a of the
//! file) goes into the plans that use it, so that a database or
//! coordinator whose model of that name is another file refuses such a
//! plan rather than rank with another model ([`Models::open`]).
//!
//! Cross-encoders (`cross_encoder()`, [`crate::query::cross_encoder`]) are
//! opened from the same directory the same way.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::Value as Json;

use crate::error::{Error, Result};
use crate::query::cross_encoder::{self, CrossEncoder};
use crate::query::ir::{Expr, Plan};

/// The file of a GBDT model in its directory.
pub const GBDT_FILE: &str = "gbdt.json";

/// A gradient-boosted tree ensemble ([module docs](self)).
pub struct Gbdt {
    pub name: String,
    /// FNV-1a of the model file, as 16 hex digits.
    pub hash: String,
    /// The features the model reads, in order.
    pub features: usize,
    /// The trees' arithmetic: LightGBM's `f64` or XGBoost's `f32`.
    xgboost: bool,
    /// What XGBoost's margin starts from.
    base: f32,
    trees: Vec<Tree>,
}

/// A tree's nodes, the root first, and the category sets of its
/// categorical splits.
struct Tree {
    nodes: Vec<Node>,
    cats: Vec<Vec<u32>>,
    /// The least and greatest leaf under each node, which bounds skip
    /// subtrees by ([`Tree::reach`]).
    range: Vec<(f64, f64)>,
}

/// A split, or a leaf: a node whose children are both itself, so that a
/// walk can take a step from it as from a split, and ends where a step
/// stays put. 24 bytes, so that a tree of 31 leaves fits in 1.5 KB.
#[derive(Clone, Copy)]
struct Node {
    /// A numerical split's threshold (XGBoost's an `f32`), a categorical
    /// split's index in [`Tree::cats`], or a leaf's value.
    value: f64,
    feature: u32,
    left: u32,
    right: u32,
    /// [`DEFAULT_LEFT`], [`MISSING_NAN`], [`MISSING_ZERO`], [`CATEGORICAL`].
    flags: u8,
}

/// Missing values go left.
const DEFAULT_LEFT: u8 = 1;
/// NaN is missing (LightGBM's `missing_type` NaN, and every XGBoost split);
/// otherwise LightGBM reads it as 0.
const MISSING_NAN: u8 = 2;
/// 0 and NaN are missing (LightGBM's `missing_type` Zero).
const MISSING_ZERO: u8 = 4;
/// LightGBM's categorical split: left when the value, as an integer, is
/// one of the categories (sorted); NaN and negatives go right.
const CATEGORICAL: u8 = 8;

/// LightGBM's `kZeroThreshold`.
const ZERO: f64 = 1e-35_f32 as f64;

fn is_zero(x: f64) -> bool {
    x > -ZERO && x <= ZERO
}

fn bad(name: &str, what: impl std::fmt::Display) -> Error {
    Error::Query(format!("ranking model {name}: {what}"))
}

/// Models of the same file are the same.
impl PartialEq for Gbdt {
    fn eq(&self, o: &Gbdt) -> bool {
        self.name == o.name && self.hash == o.hash
    }
}

impl std::fmt::Debug for Gbdt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gbdt({} {}, {} trees)", self.name, self.hash, self.trees.len())
    }
}

impl Gbdt {
    /// A model from the text of its file ([module docs](self)).
    pub fn parse(name: &str, text: &[u8]) -> Result<Gbdt> {
        let j: Json = serde_json::from_slice(text).map_err(|e| bad(name, format!("not JSON: {e}")))?;
        let hash = format!("{:016x}", fnv(text));
        let mut m = if j.get("tree_info").is_some() {
            lightgbm(name, &j)?
        } else if j.get("learner").is_some() {
            xgboost(name, &j)?
        } else {
            return Err(bad(name, "neither a LightGBM dump (dump_model) nor an XGBoost model (save_model to .json)"));
        };
        m.hash = hash;
        if m.features == 0 {
            return Err(bad(name, "the model reads no features"));
        }
        for (t, tree) in m.trees.iter_mut().enumerate() {
            tree.check(name, t, m.features)?;
            tree.range = vec![(0.0, 0.0); tree.nodes.len()];
            // Children come after their parent.
            for (i, n) in tree.nodes.iter().enumerate().rev() {
                tree.range[i] = match n.left as usize == i {
                    true => (n.value, n.value),
                    false => {
                        let (l, r) = (tree.range[n.left as usize], tree.range[n.right as usize]);
                        (l.0.min(r.0), l.1.max(r.1))
                    }
                };
            }
        }
        Ok(m)
    }

    /// The model's score of a row whose features are `x` (NaN for NULL).
    pub fn predict(&self, x: &[f64]) -> f64 {
        if self.xgboost {
            // As XGBoost: f32 features, comparisons and sum.
            let mut s = self.base;
            for t in &self.trees {
                s += t.leaf::<true>(x) as f32;
            }
            s as f64
        } else {
            self.trees.iter().fold(0.0, |s, t| s + t.leaf::<false>(x))
        }
    }

    /// [`predict`](Self::predict) of `n` rows, feature `f` of row `i`
    /// being `columns[f][i]`, equal to it bit for bit (each row's sum is
    /// taken in the same order). Faster over many rows: it walks every
    /// row through one tree before the next, so the tree stays in cache
    /// and the processor overlaps the rows' walks.
    pub fn predict_columns(&self, columns: &[&[f64]], n: usize) -> Vec<f64> {
        assert!(columns.len() == self.features && columns.iter().all(|c| c.len() >= n), "a column per feature, of every row");
        match self.xgboost {
            true => self.walk::<true>(columns, n),
            false => self.walk::<false>(columns, n),
        }
    }

    fn walk<const XGB: bool>(&self, columns: &[&[f64]], n: usize) -> Vec<f64> {
        let (mut wide, mut narrow) = (vec![0.0f64; n], vec![self.base; n]);
        let nf = self.features;
        let rows: Vec<f64> = (0..n).flat_map(|r| columns.iter().map(move |c| c[r])).collect();
        for t in &self.trees {
            for (r, x) in rows.chunks_exact(nf).enumerate() {
                let v = t.leaf::<XGB>(x);
                if XGB {
                    narrow[r] += v as f32;
                } else {
                    wide[r] += v;
                }
            }
        }
        match XGB {
            true => narrow.into_iter().map(f64::from).collect(),
            false => wide,
        }
    }

    /// Bounds of the score over rows whose feature `i` lies in
    /// `inputs[i]`: `(lo, hi, null)` with the finite values in `[lo, hi]`
    /// and NULL possible when `null`. No such row's
    /// [`predict`](Self::predict) falls outside the bounds returned, and
    /// `lo > hi` when there is no such row.
    pub fn bounds(&self, inputs: &[(f64, f64, bool)]) -> (f64, f64) {
        let (mut lo, mut hi, mut size) = (0.0, 0.0, 0.0);
        for t in &self.trees {
            let mut found = (f64::INFINITY, f64::NEG_INFINITY);
            t.reach(0, inputs, self.xgboost, &mut found);
            let (a, b) = found;
            if a > b {
                return (f64::INFINITY, f64::NEG_INFINITY);
            }
            lo += a;
            hi += b;
            size += a.abs().max(b.abs());
        }
        if self.xgboost {
            lo += self.base as f64;
            hi += self.base as f64;
            size += (self.base as f64).abs();
        }
        // Room for the rounding of a sum in another order, or in f32.
        let eps = if self.xgboost { f32::EPSILON as f64 } else { f64::EPSILON };
        let pad = (self.trees.len() + 2) as f64 * eps * size;
        (lo - pad, hi + pad)
    }
}

impl Tree {
    /// The leaf value row `x` reaches.
    fn leaf<const XGB: bool>(&self, x: &[f64]) -> f64 {
        let mut i = 0;
        loop {
            let node = &self.nodes[i as usize];
            let next = self.step::<XGB>(node, x[node.feature as usize]);
            if next == i {
                return node.value;
            }
            i = next;
        }
    }

    /// The child of `node` that a row whose feature there is `v` goes to
    /// (a leaf's is itself). Numerical splits choose without a branch.
    #[inline(always)]
    fn step<const XGB: bool>(&self, node: &Node, v: f64) -> u32 {
        let f = node.flags;
        let nan = v.is_nan();
        let left = if f & CATEGORICAL != 0 {
            // The integer part (-0.5 is 0); NaN and below 0 to the right.
            !nan && v > -1.0 && v < u32::MAX as f64 && self.cats[node.value as usize].binary_search(&(v as u32)).is_ok()
        } else {
            // LightGBM reads NaN as 0 where it is not missing.
            let v0 = if nan { 0.0 } else { v };
            let missing = (nan & (f & MISSING_NAN != 0)) | (is_zero(v0) & (f & MISSING_ZERO != 0));
            let below = if XGB { (v as f32) < node.value as f32 } else { v0 <= node.value };
            (missing & (f & DEFAULT_LEFT != 0)) | (!missing & below)
        };
        if left { node.left } else { node.right }
    }

    /// Widens `found` to the least and greatest leaf the rows in `inputs`
    /// reach from node `i`, skipping a subtree whose leaves all lie within
    /// it.
    fn reach(&self, i: usize, inputs: &[(f64, f64, bool)], xgboost: bool, found: &mut (f64, f64)) {
        let (a, b) = self.range[i];
        if a >= found.0 && b <= found.1 {
            return;
        }
        let node = &self.nodes[i];
        if node.left as usize == i {
            *found = (found.0.min(node.value), found.1.max(node.value));
            return;
        }
        let (f, t) = (node.flags, node.value);
        let default_left = f & DEFAULT_LEFT != 0;
        let (lo, hi, null) = inputs[node.feature as usize];
        let some = lo <= hi;
        let (mut l, mut r) = (false, false);
        let mut side = |go_left: bool| if go_left { l = true } else { r = true };
        if f & CATEGORICAL != 0 {
            // A value is its integer part: -0.5 is category 0.
            side(false);
            if some && self.cats[t as usize].iter().any(|&c| lo < c as f64 + 1.0 && hi > c as f64 - 1.0) {
                side(true);
            }
        } else if xgboost {
            if null {
                side(default_left);
            }
            if some {
                if (lo as f32) < t as f32 {
                    side(true);
                }
                if (hi as f32) >= t as f32 {
                    side(false);
                }
            }
        } else {
            if null {
                match f & (MISSING_NAN | MISSING_ZERO) != 0 {
                    true => side(default_left),
                    false => side(0.0 <= t),
                }
            }
            if some {
                if f & MISSING_ZERO != 0 && lo <= ZERO && hi > -ZERO {
                    side(default_left);
                }
                if lo <= t {
                    side(true);
                }
                if hi > t {
                    side(false);
                }
            }
        }
        if l {
            self.reach(node.left as usize, inputs, xgboost, found);
        }
        if r {
            self.reach(node.right as usize, inputs, xgboost, found);
        }
    }

    /// Every split's children are later nodes, every leaf's are itself,
    /// and every feature is one of the model's: the walks above end,
    /// within bounds.
    fn check(&self, name: &str, t: usize, features: usize) -> Result<()> {
        if self.nodes.is_empty() {
            return Err(bad(name, format!("tree {t} has no nodes")));
        }
        for (i, n) in self.nodes.iter().enumerate() {
            let later = |c: u32| (c as usize) > i && (c as usize) < self.nodes.len();
            let leaf = n.left as usize == i && n.right as usize == i;
            let cats = n.flags & CATEGORICAL == 0 || (n.value as usize) < self.cats.len();
            if !(leaf || later(n.left) && later(n.right) && cats) || n.feature as usize >= features {
                return Err(bad(name, format!("tree {t}, node {i}: a split out of range")));
            }
        }
        Ok(())
    }
}

/// A LightGBM `dump_model()`.
fn lightgbm(name: &str, j: &Json) -> Result<Gbdt> {
    let int = |k: &str| j.get(k).and_then(Json::as_u64);
    if int("num_tree_per_iteration").unwrap_or(1) != 1 || int("num_class").unwrap_or(1) != 1 {
        return Err(bad(name, "a model of one output is a ranking's; this one has several (multiclass)"));
    }
    let features = int("max_feature_idx").ok_or_else(|| bad(name, "no max_feature_idx"))? as usize + 1;
    let trees = j["tree_info"].as_array().ok_or_else(|| bad(name, "tree_info is not a list"))?;
    let mut out = Vec::with_capacity(trees.len());
    for (t, info) in trees.iter().enumerate() {
        let mut tree = Tree { nodes: Vec::new(), cats: Vec::new(), range: Vec::new() };
        lightgbm_node(&info["tree_structure"], &mut tree).map_err(|e| bad(name, format!("tree {t}: {e}")))?;
        out.push(tree);
    }
    Ok(Gbdt { name: name.to_string(), hash: String::new(), features, xgboost: false, base: 0.0, trees: out })
}

/// A leaf of value `v` at index `i`.
fn leaf(i: u32, v: f64) -> Node {
    Node { value: v, feature: 0, left: i, right: i, flags: 0 }
}

/// Appends the subtree at `n` to `tree`, preorder; its index.
fn lightgbm_node(n: &Json, tree: &mut Tree) -> std::result::Result<u32, String> {
    let i = tree.nodes.len() as u32;
    if let Some(v) = n.get("leaf_value") {
        if n.get("leaf_coeff").is_some() {
            return Err("linear trees are not supported".into());
        }
        tree.nodes.push(leaf(i, v.as_f64().ok_or("a leaf_value is not a number")?));
        return Ok(i);
    }
    let feature = n["split_feature"].as_u64().ok_or("a split without split_feature")?;
    let feature = u32::try_from(feature).map_err(|_| format!("feature {feature}"))?;
    let mut flags = if n["default_left"].as_bool().unwrap_or(false) { DEFAULT_LEFT } else { 0 };
    let value = match n["decision_type"].as_str() {
        Some("<=") => {
            flags |= match n["missing_type"].as_str() {
                Some("None") | None => 0,
                Some("Zero") => MISSING_ZERO,
                Some("NaN") => MISSING_NAN,
                Some(o) => return Err(format!("missing_type {o}")),
            };
            n["threshold"].as_f64().ok_or("a numerical threshold is not a number")?
        }
        Some("==") => {
            let t = n["threshold"].as_str().ok_or("a categorical threshold is not a string")?;
            let mut cats = t.split("||").map(|c| c.parse::<u32>().map_err(|_| format!("category {c}"))).collect::<std::result::Result<Vec<u32>, String>>()?;
            cats.sort_unstable();
            tree.cats.push(cats);
            flags |= CATEGORICAL;
            (tree.cats.len() - 1) as f64
        }
        o => return Err(format!("decision_type {o:?}")),
    };
    tree.nodes.push(Node { value, feature, left: 0, right: 0, flags });
    let left = lightgbm_node(&n["left_child"], tree)?;
    let right = lightgbm_node(&n["right_child"], tree)?;
    let node = &mut tree.nodes[i as usize];
    (node.left, node.right) = (left, right);
    Ok(i)
}

/// An XGBoost model saved as JSON.
fn xgboost(name: &str, j: &Json) -> Result<Gbdt> {
    let learner = &j["learner"];
    let booster = &learner["gradient_booster"];
    if booster["name"].as_str() != Some("gbtree") {
        return Err(bad(name, format!("XGBoost booster {}: only gbtree is supported", booster["name"])));
    }
    let param = &learner["learner_model_param"];
    let num = |v: &Json| -> Option<f64> {
        match v {
            Json::Number(n) => n.as_f64(),
            // "5E-1", or from XGBoost 3 one per target: "[5E-1]".
            Json::String(s) => s.trim_matches(|c| c == '[' || c == ']').parse().ok(),
            _ => None,
        }
    };
    if num(&param["num_class"]).unwrap_or(0.0) > 1.0 || num(&param["num_target"]).unwrap_or(1.0) > 1.0 {
        return Err(bad(name, "a model of one output is a ranking's; this one has several"));
    }
    let features = num(&param["num_feature"]).ok_or_else(|| bad(name, "no num_feature"))? as usize;
    let base_score = num(&param["base_score"]).unwrap_or(0.5) as f32;
    // The base score is a prediction, in margin space as the objective
    // links them, in f32 as XGBoost computes it (its logf rounds
    // correctly, as a log in f64 rounded to f32 does).
    let ln = |x: f32| (x as f64).ln() as f32;
    let base = match learner["objective"]["name"].as_str().unwrap_or("") {
        "binary:logistic" | "reg:logistic" | "binary:logitraw" => -ln(1.0 / base_score - 1.0),
        "count:poisson" | "reg:gamma" | "reg:tweedie" => ln(base_score),
        o if o.starts_with("rank:") || o.starts_with("reg:") => base_score,
        o => return Err(bad(name, format!("XGBoost objective {o} is not supported"))),
    };
    let trees = booster["model"]["trees"].as_array().ok_or_else(|| bad(name, "no trees"))?;
    let mut out = Vec::with_capacity(trees.len());
    for (t, tree) in trees.iter().enumerate() {
        out.push(xgboost_tree(tree).map_err(|e| bad(name, format!("tree {t}: {e}")))?);
    }
    Ok(Gbdt { name: name.to_string(), hash: String::new(), features, xgboost: true, base, trees: out })
}

fn xgboost_tree(t: &Json) -> std::result::Result<Tree, String> {
    let ints = |k: &str| -> std::result::Result<Vec<i64>, String> {
        t[k].as_array()
            .ok_or(format!("no {k}"))?
            .iter()
            .map(|v| v.as_i64().or_else(|| v.as_bool().map(i64::from)).ok_or(format!("{k} holds {v}")))
            .collect()
    };
    let (left, right, feature, default_left) = (ints("left_children")?, ints("right_children")?, ints("split_indices")?, ints("default_left")?);
    let cond: Vec<f64> = t["split_conditions"].as_array().ok_or("no split_conditions")?.iter().map(|v| v.as_f64().ok_or("a split condition is not a number")).collect::<std::result::Result<_, _>>()?;
    let n = left.len();
    if [right.len(), feature.len(), default_left.len(), cond.len()].iter().any(|&l| l != n) {
        return Err("arrays of different lengths".into());
    }
    if ints("split_type").map(|s| s.iter().any(|&x| x != 0)).unwrap_or(false) {
        return Err("categorical splits are not supported".into());
    }
    // Children come after their parent in XGBoost's numbering, as the
    // walk needs ([`Tree::check`]); values are f32s.
    let index = |x: i64| u32::try_from(x).map_err(|_| format!("index {x}"));
    let nodes = (0..n)
        .map(|i| match left[i] {
            -1 => Ok(leaf(i as u32, cond[i] as f32 as f64)),
            l => Ok(Node {
                value: cond[i] as f32 as f64,
                feature: index(feature[i])?,
                left: index(l)?,
                right: index(right[i])?,
                flags: MISSING_NAN | if default_left[i] != 0 { DEFAULT_LEFT } else { 0 },
            }),
        })
        .collect::<std::result::Result<_, String>>()?;
    Ok(Tree { nodes, cats: Vec::new(), range: Vec::new() })
}

/// 64-bit FNV-1a of bytes.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// The ranking models of a models directory, opened on first use and
/// again when their files change.
#[derive(Default)]
pub struct Models {
    open: Mutex<HashMap<PathBuf, (SystemTime, u64, Arc<Gbdt>)>>,
    cross: Mutex<HashMap<PathBuf, (Vec<(SystemTime, u64)>, Arc<CrossEncoder>)>>,
    /// One lock per cross-encoder directory, held while it is opened, so
    /// that queries arriving together open it once (a large model takes
    /// seconds and up to a gigabyte) and the others wait for that copy.
    opening: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Sequence models (`encode()`), from the same directories.
    sequences: crate::query::sequence::SequenceModels,
}

/// Refuses a model name that is not a directory's own name.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)) {
        return Err(Error::Query(format!("a ranking model's name is letters, digits, _, - and ., not {name:?}")));
    }
    Ok(())
}

/// Refuses a cross-encoder's name that is not a directory's name, or two
/// of them (`cohere/rerank-v3.5`, a provider's model).
fn check_path(name: &str) -> Result<()> {
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() > 2 || parts.iter().any(|p| check_name(p).is_err()) {
        return Err(Error::Query(format!("a ranking model's name is letters, digits, _, - and . (a cross-encoder's may be two such names joined by /, as a provider's model is), not {name:?}")));
    }
    Ok(())
}

impl Models {
    /// The GBDT model `name` of the directory `dir`: its file
    /// `<dir>/<name>/gbdt.json`, read again if it changed since.
    pub fn gbdt(&self, dir: &Path, name: &str) -> Result<Arc<Gbdt>> {
        check_name(name)?;
        let path = dir.join(name).join(GBDT_FILE);
        let meta = std::fs::metadata(&path).map_err(|_| Error::Query(format!("no ranking model named {name}: no file {}", path.display())))?;
        let stamp = (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len());
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, len, m)) = open.get(&path) {
            if (*t, *len) == stamp {
                return Ok(m.clone());
            }
        }
        let m = Arc::new(Gbdt::parse(name, &std::fs::read(&path)?)?);
        open.insert(path, (stamp.0, stamp.1, m.clone()));
        Ok(m)
    }

    /// The cross-encoder `name` of the directory `dir` (its files under
    /// `<dir>/<name>`, opened again if one changed since, or a provider's
    /// model), or the built-in one ([`cross_encoder::OVERLAP`]).
    pub fn cross_encoder(&self, dir: &Path, name: &str) -> Result<Arc<CrossEncoder>> {
        if name == cross_encoder::OVERLAP {
            return Ok(Arc::new(CrossEncoder::open(name, dir)?));
        }
        check_path(name)?;
        let path = dir.join(name);
        let stamp: Vec<(SystemTime, u64)> = cross_encoder::files(name, &path)
            .iter()
            .map(|f| std::fs::metadata(f).map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len())).unwrap_or((SystemTime::UNIX_EPOCH, 0)))
            .collect();
        let cached = || -> Option<Arc<CrossEncoder>> {
            let cross = self.cross.lock().unwrap_or_else(|e| e.into_inner());
            cross.get(&path).filter(|(t, _)| *t == stamp).map(|(_, m)| m.clone())
        };
        if let Some(m) = cached() {
            return Ok(m);
        }
        // Opened outside the cache's lock, since hashing a large model
        // takes seconds, but under this directory's: one query opens it,
        // and those that came meanwhile find its copy.
        let gate = self.opening.lock().unwrap_or_else(|e| e.into_inner()).entry(path.clone()).or_default().clone();
        let _opening = gate.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = cached() {
            return Ok(m);
        }
        let m = Arc::new(CrossEncoder::open(name, &path)?);
        self.cross.lock().unwrap_or_else(|e| e.into_inner()).insert(path, (stamp, m.clone()));
        Ok(m)
    }
}

impl Models {
    /// Opens the models that the plan's `gbdt()` calls name, from `dir`,
    /// for its Searches to rank with, and its cross-encoders unless
    /// `cross` is false (a shard, whose coordinator scores with them). A
    /// call whose plan names a hash runs only with the model of that hash,
    /// and a plan with another one is refused ([`stale`]); a call without
    /// one (a plan just bound from SQL) gets the hash of the model opened,
    /// which the plan then names to the shards that run it.
    pub fn open(&self, dir: &Path, plan: &mut Plan, cross: bool) -> Result<()> {
        plan.root.visit_exprs_mut(&mut |e| {
            match e {
                Expr::Gbdt { model, hash, open, .. } => {
                    let m = self.gbdt(dir, model)?;
                    if !hash.is_empty() && *hash != m.hash {
                        return Err(stale(model, &m.hash, hash));
                    }
                    hash.clone_from(&m.hash);
                    *open = Opened(Some(m));
                }
                Expr::CrossEncoder { model, hash, open, .. } if cross => {
                    let m = self.cross_encoder(dir, model)?;
                    if !hash.is_empty() && *hash != m.hash {
                        return Err(stale(model, &m.hash, hash));
                    }
                    hash.clone_from(&m.hash);
                    *open = Opened(Some(m));
                }
                // Sequence models the same way.
                Expr::Encode { model, hash, open, .. } => {
                    let m = self.sequences.get(dir, model)?;
                    if !hash.is_empty() && *hash != m.hash {
                        return Err(stale(model, &m.hash, hash));
                    }
                    hash.clone_from(&m.hash);
                    *open = Opened(Some(m));
                }
                _ => {}
            }
            Ok(())
        })
    }
}

/// The model a `gbdt()` or `cross_encoder()` call of a plan runs with,
/// once a database or coordinator opened it ([`Models::open`]). A plan's
/// JSON and protobuf leave it out (they name the model and its hash), and
/// it equals any other.
pub struct Opened<T = Gbdt>(pub Option<Arc<T>>);

impl<T> Clone for Opened<T> {
    fn clone(&self) -> Self {
        Opened(self.0.clone())
    }
}

impl<T> Default for Opened<T> {
    fn default() -> Self {
        Opened(None)
    }
}

impl<T> std::fmt::Debug for Opened<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Opened" } else { "NotOpened" })
    }
}

impl<T> PartialEq for Opened<T> {
    fn eq(&self, _: &Opened<T>) -> bool {
        true
    }
}

/// The error of a plan made with another file of a model than the one
/// where it runs.
pub fn stale(name: &str, here: &str, plan: &str) -> Error {
    Error::Query(format!("ranking model {name} here is not the one the plan was made with (hash {here} here, {plan} in the plan): it was replaced since, or the copies differ"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lgb(trees: &str) -> Gbdt {
        let text = format!(r#"{{"num_class": 1, "num_tree_per_iteration": 1, "max_feature_idx": 1, "tree_info": [{trees}]}}"#);
        Gbdt::parse("m", text.as_bytes()).unwrap()
    }

    fn split(feature: u32, threshold: &str, decision: &str, missing: &str, default_left: bool, left: &str, right: &str) -> String {
        format!(r#"{{"split_feature": {feature}, "threshold": {threshold}, "decision_type": "{decision}", "missing_type": "{missing}", "default_left": {default_left}, "left_child": {left}, "right_child": {right}}}"#)
    }

    fn leaf(v: f64) -> String {
        format!(r#"{{"leaf_value": {v}}}"#)
    }

    fn tree(s: String) -> String {
        format!(r#"{{"tree_structure": {s}}}"#)
    }

    #[test]
    fn lightgbm_splits_and_missing_values() {
        let nan = f64::NAN;
        for (missing, default_left, want) in [
            // NaN is 0 unless it is missing; 0 is missing under Zero.
            ("None", false, [1.0, 1.0, 2.0, 1.0]),
            ("Zero", false, [2.0, 2.0, 2.0, 1.0]),
            ("Zero", true, [1.0, 1.0, 2.0, 1.0]),
            ("NaN", false, [2.0, 1.0, 2.0, 1.0]),
            ("NaN", true, [1.0, 1.0, 2.0, 1.0]),
        ] {
            let m = lgb(&tree(split(0, "0.5", "<=", missing, default_left, &leaf(1.0), &leaf(2.0))));
            let got: Vec<f64> = [nan, 0.0, 0.75, 0.5].iter().map(|&x| m.predict(&[x, 0.0])).collect();
            assert_eq!(got, want, "{missing} {default_left}");
        }
        // Categories: the integer part, NaN and negatives to the right.
        let m = lgb(&tree(split(1, r#""1||3""#, "==", "None", false, &leaf(1.0), &leaf(2.0))));
        let got: Vec<f64> = [1.0, 1.9, 3.0, 2.0, -0.5, -1.0, nan, 0.0].iter().map(|&x| m.predict(&[0.0, x])).collect();
        assert_eq!(got, [1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0, 2.0]);
        // The trees' sum.
        let two = format!("{}, {}", tree(leaf(0.25)), tree(split(0, "0", "<=", "None", false, &leaf(1.0), &leaf(3.0))));
        assert_eq!(lgb(&two).predict(&[1.0, 0.0]), 3.25);
    }

    #[test]
    fn bounds_hold_every_row() {
        let t = |a: &str, c: &str| tree(split(0, "0.5", "<=", "NaN", true, &split(1, "-2", "<=", a, false, &leaf(1.0), &leaf(-4.0)), &split(1, r#""0||2""#, "==", "None", false, &leaf(c.parse().unwrap()), &leaf(0.5))));
        let m = lgb(&format!("{}, {}", t("None", "7"), t("Zero", "-3")));
        let mut seed = 7u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as f64 / (1u64 << 31) as f64
        };
        let points = [f64::NAN, 0.0, -0.5, 0.5, 2.0, -2.0, 1.0, 3.0, -7.0];
        for _ in 0..2000 {
            let mut boxes = Vec::new();
            for _ in 0..2 {
                let (a, b) = (points[(next() * 9.0) as usize], points[(next() * 9.0) as usize]);
                let (lo, hi) = if a.is_nan() || b.is_nan() { (1.0, 0.0) } else { (a.min(b), a.max(b)) };
                boxes.push((lo, hi, a.is_nan() || b.is_nan() || next() < 0.3));
            }
            let (lo, hi) = m.bounds(&boxes);
            // Skipping subtrees changes nothing.
            let mut every = lgb(&format!("{}, {}", t("None", "7"), t("Zero", "-3")));
            every.trees.iter_mut().for_each(|t| t.range.fill((f64::NEG_INFINITY, f64::INFINITY)));
            assert_eq!(every.bounds(&boxes), (lo, hi), "{boxes:?}");
            let values = |&(l, h, null): &(f64, f64, bool)| {
                let mut v: Vec<f64> = points.iter().copied().filter(|x| *x >= l && *x <= h).chain([l, h].into_iter().filter(|_| l <= h)).collect();
                if null {
                    v.push(f64::NAN);
                }
                v
            };
            for a in values(&boxes[0]) {
                for b in values(&boxes[1]) {
                    let p = m.predict(&[a, b]);
                    assert!(p >= lo && p <= hi, "{p} outside [{lo}, {hi}] for {a} {b} in {boxes:?}");
                }
            }
        }
    }

    #[test]
    fn refuses_what_it_cannot_run() {
        let parse = |t: &str| Gbdt::parse("m", t.as_bytes()).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(parse("{}").contains("neither a LightGBM dump"));
        assert!(parse(r#"{"num_class": 3, "num_tree_per_iteration": 3, "max_feature_idx": 0, "tree_info": []}"#).contains("several"));
        let out_of_range = format!(r#"{{"max_feature_idx": 0, "tree_info": [{}]}}"#, tree(split(4, "1", "<=", "None", false, &leaf(1.0), &leaf(2.0))));
        assert!(parse(&out_of_range).contains("out of range"));
        let linear = r#"{"max_feature_idx": 0, "tree_info": [{"tree_structure": {"leaf_value": 1, "leaf_coeff": [1]}}]}"#;
        assert!(parse(linear).contains("linear trees"));
        let models = Models::default();
        for name in ["../x", ".hidden", "a/b", ""] {
            assert!(models.gbdt(Path::new("/nonexistent"), name).unwrap_err().to_string().contains("letters, digits"), "{name}");
        }
        assert!(models.gbdt(Path::new("/nonexistent"), "m").unwrap_err().to_string().contains("no ranking model named m"));
    }
}
