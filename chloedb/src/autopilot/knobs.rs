//! The numbers a rank profile can be tuned by ("knobs"): the defaults of
//! the parameters its ranking reads, and the numeric literals of its
//! rankings' ORDER BY expressions (a weight, a boost, a fusion's k, a
//! decay's scale). A proposal changes only these, so that it reads as the
//! profile it replaces with some numbers moved, and is applied as `CREATE
//! OR REPLACE RANK PROFILE` of the same ranking.

use std::fmt;

use serde_json::{json, Value as J};
use sqlparser::ast::{self as sql, Expr};

use crate::error::{Error, Result};
use crate::query::compound::query_mut;
use crate::storage::RankProfile;

/// One number of a profile.
#[derive(Clone, Debug, PartialEq)]
pub enum Knob {
    /// The default of parameter `:name` (`WITH (name => value)`).
    Default(String),
    /// The `index`th numeric literal of the ranking's ORDER BY expressions,
    /// counted as [`query_mut`] visits them, phase by phase.
    Literal {
        index: usize,
        /// The literal as the ranking writes it, around it, to name it.
        context: String,
        /// Written without a point: it stays a whole number.
        integer: bool,
    },
}

impl fmt::Display for Knob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Knob::Default(n) => write!(f, ":{n}"),
            Knob::Literal { context, .. } => write!(f, "{context}"),
        }
    }
}

/// A knob and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub knob: Knob,
    pub value: f64,
}

impl Setting {
    pub fn to_json(&self) -> J {
        match &self.knob {
            Knob::Default(n) => json!({"kind": "default", "name": n, "value": self.value}),
            Knob::Literal { index, context, integer } => json!({"kind": "literal", "index": index, "in": context, "integer": integer, "value": self.value}),
        }
    }
}

fn number(e: &Expr) -> Option<(f64, bool)> {
    match e {
        Expr::Value(v) => match &v.value {
            sql::Value::Number(s, _) => s.parse::<f64>().ok().filter(|x| x.is_finite()).map(|x| (x, !s.contains(['.', 'e', 'E']))),
            _ => None,
        },
        _ => None,
    }
}

/// The knobs of `p` and their values now. Literals 0 and 1, which a
/// ranking writes for structure (`coalesce(x, 0)`, `1 + w * x`) more than
/// as weights, are left out; so is a default that is not a number.
pub fn knobs(p: &RankProfile) -> Result<Vec<Setting>> {
    let mut out: Vec<Setting> = p.defaults.iter().filter_map(|(k, v)| v.as_f64().map(|x| Setting { knob: Knob::Default(k.clone()), value: x })).collect();
    let mut q = crate::query::sql::parse_query(&p.ranking)?;
    // The text around each literal names it: the smallest call or
    // operation holding it, as the ranking prints it. A parent is visited
    // before its operands, so it leaves its text by each literal operand's
    // address, which the walk does not move.
    let mut named: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut index = 0usize;
    let mut found: Vec<(usize, f64, bool, String)> = Vec::new();
    query_mut(&mut q, &mut |e| {
        let at = e as *const Expr as usize;
        if let Some((x, integer)) = number(e) {
            found.push((index, x, integer, named.remove(&at).unwrap_or_else(|| e.to_string())));
            index += 1;
            return;
        }
        let text = e.to_string();
        let mut mark = |x: &Expr| {
            if number(x).is_some() {
                named.insert(x as *const Expr as usize, text.clone());
            }
        };
        match e {
            Expr::BinaryOp { left, right, .. } => {
                mark(left);
                mark(right);
            }
            Expr::Function(f) => {
                if let sql::FunctionArguments::List(l) = &f.args {
                    for a in &l.args {
                        if let sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(x)) | sql::FunctionArg::Named { arg: sql::FunctionArgExpr::Expr(x), .. } = a {
                            mark(x);
                        }
                    }
                }
            }
            _ => {}
        }
    });
    for (i, x, integer, context) in found {
        if x == 0.0 || x == 1.0 {
            continue;
        }
        out.push(Setting { knob: Knob::Literal { index: i, context, integer }, value: x });
    }
    Ok(out)
}

/// A number as SQL writes it: a whole number without a point when it is
/// an integer knob, else the shortest text that reads back as `x`.
fn literal(x: f64, integer: bool) -> String {
    if integer {
        format!("{}", x.round().max(1.0) as i64)
    } else {
        let s = format!("{}", round_sig(x, 6));
        if s.contains(['.', 'e', 'E']) {
            s
        } else {
            format!("{s}.0")
        }
    }
}

/// `x` to `n` significant digits, so that a proposal's numbers read as a
/// person would write them.
pub fn round_sig(x: f64, n: i32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let m = 10f64.powi(n - 1 - x.abs().log10().floor() as i32);
    (x * m).round() / m
}

/// `p` with the knobs set to these values (others as they are).
pub fn with(p: &RankProfile, settings: &[Setting]) -> Result<RankProfile> {
    let mut out = p.clone();
    let mut lits: Vec<(usize, f64, bool)> = Vec::new();
    for s in settings {
        match &s.knob {
            Knob::Default(n) => {
                let v = out.defaults.get_mut(n).ok_or_else(|| Error::Query(format!("rank profile {} has no parameter :{n}", p.name)))?;
                *v = serde_json::from_str(&literal(s.value, v.is_i64() || v.is_u64())).unwrap_or(json!(s.value));
            }
            Knob::Literal { index, integer, .. } => lits.push((*index, s.value, *integer)),
        }
    }
    if !lits.is_empty() {
        let mut q = crate::query::sql::parse_query(&p.ranking)?;
        let mut i = 0usize;
        query_mut(&mut q, &mut |e| {
            if number(e).is_some() {
                if let Some(&(_, x, integer)) = lits.iter().find(|(at, _, _)| *at == i) {
                    *e = Expr::value(sql::Value::Number(literal(x, integer), false));
                }
                i += 1;
            }
        });
        out.ranking = q.to_string();
    }
    Ok(out)
}

/// The ranking with every number [`knobs`] counts written as 0, and those
/// numbers in order: two rankings of one shape differ in these alone.
fn shape(ranking: &str) -> Result<(String, Vec<f64>)> {
    let mut q = crate::query::sql::parse_query(ranking)?;
    let mut found = Vec::new();
    query_mut(&mut q, &mut |e| {
        if let Some((x, _)) = number(e) {
            found.push(x);
            *e = Expr::value(sql::Value::Number("0".into(), false));
        }
    });
    Ok((q.to_string(), found))
}

/// The knobs `cand` moved from `p`, each with its new value, when `cand`
/// is `p` with some of its knobs' numbers changed and nothing else: the
/// same ranking but for those numbers (a 0 or a 1, which are structure
/// and no knob, as they were), the same parameters, the same defaults
/// that are not numbers. Anything else is refused: a proposal changes a
/// profile's numbers, never what it ranks by (audit 4, N-C1), so a row
/// of the proposals table that someone wrote by hand cannot carry a
/// ranking of its own.
pub fn moved(p: &RankProfile, cand: &RankProfile) -> Result<Vec<(Setting, f64)>> {
    let not = |why: String| Error::Query(format!("the candidate is not rank profile {} with some of its numbers moved: {why}", p.name));
    let (a, xs) = shape(&p.ranking)?;
    let (b, ys) = shape(&cand.ranking).map_err(|e| not(e.to_string()))?;
    if a != b || xs.len() != ys.len() {
        return Err(not("its ranking is another".into()));
    }
    if p.defaults.len() != cand.defaults.len() || p.defaults.keys().any(|k| !cand.defaults.contains_key(k)) {
        return Err(not("its parameters are others".into()));
    }
    let mut out = Vec::new();
    for s in knobs(p)? {
        let to = match &s.knob {
            Knob::Default(n) => cand.defaults.get(n).and_then(|v| v.as_f64()).ok_or_else(|| not(format!("its default of :{n} is not a number")))?,
            Knob::Literal { index, .. } => ys[*index],
        };
        if !to.is_finite() {
            return Err(not(format!("{} is not a finite number", s.knob)));
        }
        if to != s.value {
            out.push((s, to));
        }
    }
    // What is not a knob stays as it is.
    for (i, (x, y)) in xs.iter().zip(&ys).enumerate() {
        if (*x == 0.0 || *x == 1.0) && x != y {
            return Err(not(format!("its number {i} of the ranking ({x}, which is structure, not a knob) is {y}")));
        }
    }
    for (k, v) in &p.defaults {
        if v.as_f64().is_none() && cand.defaults.get(k) != Some(v) {
            return Err(not(format!("its default of :{k} is another")));
        }
    }
    Ok(out)
}

/// The statement that makes `p` the profile of `table`.
pub fn statement(table: &str, p: &RankProfile) -> String {
    let mut s = format!("CREATE OR REPLACE RANK PROFILE {} ON {table} AS {}", p.name, p.ranking);
    if !p.defaults.is_empty() {
        let ds: Vec<String> = p.defaults.iter().map(|(k, v)| format!("{k} => {}", default_sql(v))).collect();
        s.push_str(&format!(" WITH ({})", ds.join(", ")));
    }
    s
}

fn default_sql(v: &J) -> String {
    match v {
        J::String(s) => format!("'{}'", s.replace('\'', "''")),
        J::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        J::Number(n) => n.to_string(),
        _ => "NULL".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn profile(ranking: &str, defaults: &[(&str, J)]) -> RankProfile {
        RankProfile { name: "p".into(), ranking: ranking.into(), defaults: defaults.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<BTreeMap<_, _>>() }
    }

    #[test]
    fn finds_defaults_and_weights_not_structure() {
        let p = profile("SELECT * FROM t ORDER BY 0.3 * bm25(title, :q) + bm25(body, :q) * (1 + :w * coalesce(rating, 0)) DESC", &[("w", json!(0.2))]);
        let ks = knobs(&p).unwrap();
        assert_eq!(ks.len(), 2, "{ks:?}");
        assert_eq!(ks[0].knob, Knob::Default("w".into()));
        assert!(matches!(&ks[1].knob, Knob::Literal { index: 0, integer: false, context } if context.contains("0.3")), "{ks:?}");
        assert_eq!(ks[1].value, 0.3);
    }

    #[test]
    fn sets_them_and_keeps_the_rest() {
        let p = profile(
            "WITH a AS (SELECT * FROM t ORDER BY bm25(body, :q) DESC LIMIT 100) SELECT * FROM a ORDER BY bm25(body, :q) + 0.5 * coalesce(rating, 0) + 2 * recency DESC",
            &[("k", json!(60))],
        );
        let ks = knobs(&p).unwrap();
        assert_eq!(ks.len(), 3, "{ks:?}");
        let moved: Vec<Setting> = ks.iter().map(|s| Setting { knob: s.knob.clone(), value: s.value * 1.5 }).collect();
        let q = with(&p, &moved).unwrap();
        assert!(q.ranking.contains("0.75 * coalesce(rating, 0)"), "{}", q.ranking);
        assert!(q.ranking.contains("3 * recency"), "{}", q.ranking);
        assert!(q.ranking.contains("LIMIT 100"), "{}", q.ranking);
        assert_eq!(q.defaults["k"], json!(90));
        let again: Vec<f64> = knobs(&q).unwrap().iter().map(|s| s.value).collect();
        assert_eq!(again, vec![90.0, 0.75, 3.0]);
        assert_eq!(statement("t", &q), format!("CREATE OR REPLACE RANK PROFILE p ON t AS {} WITH (k => 90)", q.ranking));
    }

    #[test]
    fn a_candidate_moves_numbers_and_nothing_else() {
        let p = profile("SELECT * FROM t ORDER BY 0.3 * bm25(title, :q) + bm25(body, :q) * (1 + :w * coalesce(rating, 0)) DESC LIMIT 100", &[("w", json!(0.2)), ("lang", json!("en"))]);
        let ks = knobs(&p).unwrap();
        let set: Vec<Setting> = ks.iter().map(|s| Setting { knob: s.knob.clone(), value: s.value * 2.0 }).collect();
        let q = with(&p, &set).unwrap();
        let m = moved(&p, &q).unwrap();
        assert_eq!(m.iter().map(|(_, to)| *to).collect::<Vec<_>>(), vec![0.4, 0.6]);
        assert!(moved(&p, &p).unwrap().is_empty());
        // A forged ranking, another parameter, a structural number, a text
        // default: refused.
        let forged = [
            profile("SELECT * FROM t ORDER BY CASE WHEN vendor = 'me' THEN 1e9 ELSE bm25(body, :q) END DESC", &[("w", json!(0.2)), ("lang", json!("en"))]),
            profile("SELECT * FROM t ORDER BY 0.3 * bm25(title, :q) + bm25(body, :q) * (1 + :w * coalesce(rating, 0)) DESC LIMIT 100", &[("w", json!(0.2)), ("lang", json!("en")), ("x", json!(1))]),
            profile("SELECT * FROM t ORDER BY 0.3 * bm25(title, :q) + bm25(body, :q) * (1 + :w * coalesce(rating, 50)) DESC LIMIT 100", &[("w", json!(0.2)), ("lang", json!("en"))]),
            profile("SELECT * FROM t ORDER BY 0.3 * bm25(title, :q) + bm25(body, :q) * (1 + :w * coalesce(rating, 0)) DESC LIMIT 100000", &[("w", json!(0.2)), ("lang", json!("en"))]),
            profile("SELECT * FROM t ORDER BY 0.3 * bm25(title, :q) + bm25(body, :q) * (1 + :w * coalesce(rating, 0)) DESC LIMIT 100", &[("w", json!(0.2)), ("lang", json!("fr"))]),
            profile("SELECT * FROM t WHERE vendor = 'me' ORDER BY 0.3 * bm25(title, :q) + bm25(body, :q) * (1 + :w * coalesce(rating, 0)) DESC LIMIT 100", &[("w", json!(0.2)), ("lang", json!("en"))]),
        ];
        for f in forged {
            assert!(moved(&p, &f).is_err(), "{}", f.ranking);
        }
    }

    #[test]
    fn rounds_to_what_a_person_writes() {
        assert_eq!(round_sig(0.123456789, 3), 0.123);
        assert_eq!(literal(0.30000000000000004, false), "0.3");
        assert_eq!(literal(2.0, false), "2.0");
        assert_eq!(literal(59.6, true), "60");
    }
}
