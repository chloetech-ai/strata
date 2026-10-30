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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde_json::Value as Json;

use crate::error::{Error, Result};
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

/// A tree's nodes, the root first.
struct Tree(Vec<Node>);

enum Node {
    Leaf(f64),
    Split { feature: usize, test: Test, default_left: bool, left: u32, right: u32 },
}

/// Which way a split sends a row.
enum Test {
    /// LightGBM's numerical split: left when `x <= threshold`.
    Le(f64, Missing),
    /// LightGBM's categorical split: left when `x`, as an integer, is one
    /// of the categories (sorted).
    In(Vec<u32>),
    /// XGBoost's split: left when `x as f32 < threshold`, missing values
    /// to the default side.
    Lt(f32),
}

/// What LightGBM counts as missing at a numerical split.
#[derive(Clone, Copy, PartialEq)]
enum Missing {
    /// Nothing: NaN is 0.
    None,
    /// 0 (within LightGBM's `kZeroThreshold`) and NaN, which is 0.
    Zero,
    NaN,
}

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
        for (t, tree) in m.trees.iter().enumerate() {
            tree.check(name, t, m.features)?;
        }
        Ok(m)
    }

    /// The model's score of a row whose features are `x` (NaN for NULL).
    pub fn predict(&self, x: &[f64]) -> f64 {
        if self.xgboost {
            // As XGBoost: f32 features, comparisons and sum.
            let mut s = self.base;
            for t in &self.trees {
                s += t.leaf(x) as f32;
            }
            s as f64
        } else {
            self.trees.iter().fold(0.0, |s, t| s + t.leaf(x))
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
            let (a, b) = t.bounds(0, inputs, self.xgboost);
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
    fn leaf(&self, x: &[f64]) -> f64 {
        let mut i = 0;
        loop {
            match &self.0[i] {
                Node::Leaf(v) => return *v,
                Node::Split { feature, test, default_left, left, right } => {
                    let v = x[*feature];
                    let go_left = match test {
                        Test::Le(t, missing) => {
                            let v = if v.is_nan() && *missing != Missing::NaN { 0.0 } else { v };
                            if (*missing == Missing::Zero && is_zero(v)) || (*missing == Missing::NaN && v.is_nan()) {
                                *default_left
                            } else {
                                v <= *t
                            }
                        }
                        // The integer part (-0.5 is 0); NaN and below 0 to the right.
                        Test::In(cats) => !v.is_nan() && v > -1.0 && v < u32::MAX as f64 && cats.binary_search(&(v as u32)).is_ok(),
                        Test::Lt(t) => match v.is_nan() {
                            true => *default_left,
                            false => (v as f32) < *t,
                        },
                    };
                    i = if go_left { *left } else { *right } as usize;
                }
            }
        }
    }

    /// The least and greatest leaf the rows in `inputs` reach from node
    /// `i`.
    fn bounds(&self, i: usize, inputs: &[(f64, f64, bool)], xgboost: bool) -> (f64, f64) {
        match &self.0[i] {
            Node::Leaf(v) => {
                let v = if xgboost { *v as f32 as f64 } else { *v };
                (v, v)
            }
            Node::Split { feature, test, default_left, left, right } => {
                let (lo, hi, null) = inputs[*feature];
                let some = lo <= hi;
                let (mut l, mut r) = (false, false);
                let mut side = |go_left: bool| if go_left { l = true } else { r = true };
                match test {
                    Test::Le(t, missing) => {
                        if null {
                            match missing {
                                Missing::NaN | Missing::Zero => side(*default_left),
                                Missing::None => side(0.0 <= *t),
                            }
                        }
                        if some {
                            if *missing == Missing::Zero && lo <= ZERO && hi > -ZERO {
                                side(*default_left);
                            }
                            if lo <= *t {
                                side(true);
                            }
                            if hi > *t {
                                side(false);
                            }
                        }
                    }
                    Test::In(cats) => {
                        // A value is its integer part: -0.5 is category 0.
                        side(false);
                        if some && cats.iter().any(|&c| lo < c as f64 + 1.0 && hi > c as f64 - 1.0) {
                            side(true);
                        }
                    }
                    Test::Lt(t) => {
                        if null {
                            side(*default_left);
                        }
                        if some {
                            if (lo as f32) < *t {
                                side(true);
                            }
                            if (hi as f32) >= *t {
                                side(false);
                            }
                        }
                    }
                }
                let reach = |n: u32| self.bounds(n as usize, inputs, xgboost);
                match (l, r) {
                    (true, true) => {
                        let (a, b) = (reach(*left), reach(*right));
                        (a.0.min(b.0), a.1.max(b.1))
                    }
                    (true, false) => reach(*left),
                    (false, true) => reach(*right),
                    // No row reaches the node: nothing to bound.
                    (false, false) => (f64::INFINITY, f64::NEG_INFINITY),
                }
            }
        }
    }

    /// Every child index is a later node, and every feature one of the
    /// model's: the walks above end, within bounds.
    fn check(&self, name: &str, t: usize, features: usize) -> Result<()> {
        if self.0.is_empty() {
            return Err(bad(name, format!("tree {t} has no nodes")));
        }
        for (i, n) in self.0.iter().enumerate() {
            if let Node::Split { feature, left, right, .. } = n {
                let ok = |c: u32| (c as usize) > i && (c as usize) < self.0.len();
                if !ok(*left) || !ok(*right) || *feature >= features {
                    return Err(bad(name, format!("tree {t}, node {i}: a split out of range")));
                }
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
        let mut nodes = Vec::new();
        lightgbm_node(name, &info["tree_structure"], &mut nodes).map_err(|e| bad(name, format!("tree {t}: {e}")))?;
        out.push(Tree(nodes));
    }
    Ok(Gbdt { name: name.to_string(), hash: String::new(), features, xgboost: false, base: 0.0, trees: out })
}

/// Appends the subtree at `n` to `nodes`, preorder; its index.
fn lightgbm_node(name: &str, n: &Json, nodes: &mut Vec<Node>) -> std::result::Result<u32, String> {
    let i = nodes.len() as u32;
    if let Some(v) = n.get("leaf_value") {
        if n.get("leaf_coeff").is_some() {
            return Err("linear trees are not supported".into());
        }
        nodes.push(Node::Leaf(v.as_f64().ok_or("a leaf_value is not a number")?));
        return Ok(i);
    }
    let feature = n["split_feature"].as_u64().ok_or("a split without split_feature")? as usize;
    let default_left = n["default_left"].as_bool().unwrap_or(false);
    let test = match n["decision_type"].as_str() {
        Some("<=") => {
            let t = n["threshold"].as_f64().ok_or("a numerical threshold is not a number")?;
            let missing = match n["missing_type"].as_str() {
                Some("None") | None => Missing::None,
                Some("Zero") => Missing::Zero,
                Some("NaN") => Missing::NaN,
                Some(o) => return Err(format!("missing_type {o}")),
            };
            Test::Le(t, missing)
        }
        Some("==") => {
            let t = n["threshold"].as_str().ok_or("a categorical threshold is not a string")?;
            let mut cats = t.split("||").map(|c| c.parse::<u32>().map_err(|_| format!("category {c}"))).collect::<std::result::Result<Vec<u32>, String>>()?;
            cats.sort_unstable();
            Test::In(cats)
        }
        o => return Err(format!("decision_type {o:?}")),
    };
    nodes.push(Node::Split { feature, test, default_left, left: 0, right: 0 });
    let left = lightgbm_node(name, &n["left_child"], nodes)?;
    let right = lightgbm_node(name, &n["right_child"], nodes)?;
    if let Node::Split { left: l, right: r, .. } = &mut nodes[i as usize] {
        (*l, *r) = (left, right);
    }
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
    // walk needs ([`Tree::check`]).
    let nodes = (0..n)
        .map(|i| match left[i] {
            -1 => Node::Leaf(cond[i] as f32 as f64),
            l => Node::Split { feature: feature[i] as usize, test: Test::Lt(cond[i] as f32), default_left: default_left[i] != 0, left: l as u32, right: right[i] as u32 },
        })
        .collect();
    Ok(Tree(nodes))
}

/// 64-bit FNV-1a of bytes.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// The ranking models of a models directory, opened on first use and
/// again when their file changes.
#[derive(Default)]
pub struct Models {
    open: Mutex<HashMap<PathBuf, (SystemTime, u64, Arc<Gbdt>)>>,
}

impl Models {
    /// The GBDT model `name` of the directory `dir`: its file
    /// `<dir>/<name>/gbdt.json`, read again if it changed since.
    pub fn gbdt(&self, dir: &Path, name: &str) -> Result<Arc<Gbdt>> {
        if name.is_empty() || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c)) {
            return Err(Error::Query(format!("a ranking model's name is letters, digits, _, - and ., not {name:?}")));
        }
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
}

impl Models {
    /// Opens the models that the plan's `gbdt()` calls name, from `dir`,
    /// for its Searches to rank with. A call whose plan names a hash runs
    /// only with the model of that hash, and a plan with another one is
    /// refused ([`stale`]); a call without one (a plan just bound from
    /// SQL) gets the hash of the model opened, which the plan then names
    /// to the shards that run it.
    pub fn open(&self, dir: &Path, plan: &mut Plan) -> Result<()> {
        plan.root.visit_exprs_mut(&mut |e| {
            if let Expr::Gbdt { model, hash, open, .. } = e {
                let m = self.gbdt(dir, model)?;
                if !hash.is_empty() && *hash != m.hash {
                    return Err(stale(model, &m.hash, hash));
                }
                hash.clone_from(&m.hash);
                *open = Opened(Some(m));
            }
            Ok(())
        })
    }
}

/// The model a `gbdt()` call of a plan runs with, once a database or
/// coordinator opened it ([`Models::open`]). A plan's JSON and protobuf
/// leave it out (they name the model and its hash), and it equals any
/// other.
#[derive(Clone, Debug, Default)]
pub struct Opened(pub Option<Arc<Gbdt>>);

impl PartialEq for Opened {
    fn eq(&self, _: &Opened) -> bool {
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
