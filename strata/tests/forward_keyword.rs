//! Keyword dictionary, codes, value lists, IN and facets against naive
//! arrays.

mod forward_common;

use std::collections::BTreeMap;

use forward_common::{build, random_docset};
use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::forward::{FacetStrategy, KeywordColumn};
use strata::segment::build::BuildOptions;
use strata::segment::{DocFilter, DocSet};
use strata::{Document, Field, FieldType, Schema, Value};

fn schema() -> Schema {
    Schema::new("t", vec![Field::new("k", FieldType::Keyword)], None).unwrap()
}

/// Values of several shapes: Zipf-like over few values, many rare values,
/// unique values, one value, with or without nulls.
fn values(rng: &mut StdRng, n: usize, shape: usize) -> Vec<Option<String>> {
    (0..n)
        .map(|d| {
            if shape % 2 == 1 && rng.gen_bool(0.1) {
                return None;
            }
            Some(match shape / 2 {
                0 => format!("c{:02}", (64.0 * rng.gen::<f64>().powi(3)) as u32),
                1 => format!("v{}", rng.gen_range(0..n / 10 + 1)),
                2 => format!("id-{d}"),
                _ => "only".to_string(),
            })
        })
        .collect()
}

fn naive_counts(vals: &[Option<String>], docs: &DocSet) -> Vec<(String, u64)> {
    let mut m: BTreeMap<&str, u64> = BTreeMap::new();
    for d in docs.iter() {
        if let Some(v) = &vals[d as usize] {
            *m.entry(v).or_default() += 1;
        }
    }
    let mut out: Vec<(String, u64)> = m.into_iter().map(|(k, c)| (k.to_string(), c)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[test]
fn keyword_columns_match_naive() {
    let mut rng = StdRng::seed_from_u64(1);
    for shape in 0..8 {
        for n in [0usize, 1, 200, 5000, 70_000] {
            let vals = values(&mut rng, n, shape);
            let docs: Vec<Document> = vals.iter().map(|v| vec![v.clone().map_or(Value::Null, Value::Str)]).collect();
            for rare in [64usize, 1, 1_000_000] {
                let seg = build(&schema(), &docs, &BuildOptions::default().set("forward.kw_rare", rare));
                let col = KeywordColumn::open(&seg, "k").unwrap();
                assert!(col.is_view());
                check(&col, &vals, &mut rng);
            }
        }
    }
}

fn check(col: &KeywordColumn, vals: &[Option<String>], rng: &mut StdRng) {
    let n = vals.len() as u32;
    let mut distinct: Vec<&str> = vals.iter().flatten().map(|s| s.as_str()).collect();
    distinct.sort();
    distinct.dedup();
    // Dictionary: sorted, codes are ranks.
    assert_eq!(col.n_values(), distinct.len());
    for (c, v) in distinct.iter().enumerate() {
        assert_eq!(col.value(c as u32), *v);
        assert_eq!(col.code(v), Some(c as u32));
    }
    assert_eq!(col.code("not a value"), None);
    assert_eq!(col.code(""), None);
    for (d, v) in vals.iter().enumerate() {
        assert_eq!(col.code_of_doc(d as u32), v.as_ref().map(|v| col.code(v).unwrap()));
        assert_eq!(col.value_of_doc(d as u32), v.as_deref());
    }
    // Lists, counts and probes for a sample of values (all when few).
    let sample: Vec<&str> = if distinct.len() <= 100 { distinct.clone() } else { (0..100).map(|_| distinct[rng.gen_range(0..distinct.len())]).collect() };
    for v in sample.iter().copied().chain(["missing"]) {
        let want: Vec<u32> = (0..n).filter(|&d| vals[d as usize].as_deref() == Some(v)).collect();
        assert_eq!(col.docs(v), DocSet::from_sorted(want.clone(), n), "docs({v})");
        assert_eq!(col.count(v), want.len());
        let f = col.filter(v);
        assert_eq!(f.cardinality(), want.len());
        for _ in 0..200 {
            let d = if !want.is_empty() && rng.gen_bool(0.5) { want[rng.gen_range(0..want.len())] } else { rng.gen_range(0..n.max(1)) };
            if d < n {
                assert_eq!(f.contains(d), want.binary_search(&d).is_ok());
            }
        }
    }
    // IN lists.
    for _ in 0..20 {
        let k = rng.gen_range(0..6);
        let mut ins: Vec<&str> = (0..k).filter(|_| !distinct.is_empty()).map(|_| distinct[rng.gen_range(0..distinct.len())]).collect();
        ins.push("missing");
        let want: Vec<u32> = (0..n).filter(|&d| vals[d as usize].as_deref().is_some_and(|v| ins.contains(&v))).collect();
        assert_eq!(col.docs_in(&ins), DocSet::from_sorted(want.clone(), n), "IN {ins:?}");
        let f = col.filter_in(&ins);
        assert_eq!(f.cardinality(), want.len());
        for d in 0..n.min(3000) {
            assert_eq!(f.contains(d), want.binary_search(&d).is_ok());
        }
    }
    // Facets over sparse and dense results, by every strategy.
    for density in [0.0, 0.001, 0.02, 0.3, 0.9, 1.0] {
        let docs = random_docset(rng, n, density);
        let want = naive_counts(vals, &docs);
        for s in [FacetStrategy::Codes, FacetStrategy::Postings, FacetStrategy::Auto] {
            assert_eq!(col.facet_counts(&docs, s), want, "{s:?} density {density}");
        }
    }
}

#[test]
fn rare_values_use_plain_arrays() {
    let mut rng = StdRng::seed_from_u64(2);
    let vals = values(&mut rng, 20_000, 4); // unique ids
    let docs: Vec<Document> = vals.iter().map(|v| vec![Value::Str(v.clone().unwrap())]).collect();
    let seg = build(&schema(), &docs, &BuildOptions::default());
    let col = KeywordColumn::open(&seg, "k").unwrap();
    let sizes = col.byte_sizes();
    assert_eq!(sizes.lists, 0);
    // Every value on one document: docIDs only, offsets implied.
    assert_eq!(sizes.rare, 4 * 20_000);
    assert_eq!(col.docs("id-777").to_sorted(), vec![777]);
}
