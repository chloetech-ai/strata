//! Numeric columns and range postings against naive arrays.

mod forward_common;

use forward_common::build;
use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::forward::{count_range, eval_range, float_key, NumericColumn, NumericKind, RangeIndex, RangeStrategy, BLOCK};
use strata::segment::build::BuildOptions;
use strata::segment::DocSet;
use strata::{Document, Field, FieldType, Schema, Value};

fn schema() -> Schema {
    Schema::new("t", vec![Field::new("i", FieldType::Int), Field::new("f", FieldType::Float)], None).unwrap()
}

/// Column values of several shapes: negatives, nulls, duplicates, blocks
/// of one value, keys spanning more than 32 bits within a block.
fn int_values(rng: &mut StdRng, n: usize, shape: usize) -> Vec<Option<i64>> {
    (0..n)
        .map(|d| {
            if shape != 0 && rng.gen_bool(0.07) {
                return None;
            }
            Some(match shape {
                0 | 1 => rng.gen_range(-50..50),
                2 => 1990 + (d / BLOCK) as i64 % 35, // all-equal blocks
                3 => rng.gen::<i64>() >> rng.gen_range(11..40),
                4 => (d as i64) * 1000 - 7, // sorted: zone maps prune
                _ => {
                    if (d / BLOCK) % 3 == 0 {
                        i64::MIN + rng.gen_range(0..3)
                    } else {
                        i64::MAX - rng.gen_range(0..3)
                    }
                }
            })
        })
        .collect()
}

fn float_values(rng: &mut StdRng, n: usize, shape: usize) -> Vec<Option<f64>> {
    (0..n)
        .map(|_| {
            if shape != 0 && rng.gen_bool(0.05) {
                return None;
            }
            Some(match shape {
                0 | 1 => (rng.gen_range(0..500) as f64) / 100.0, // ratings
                2 => (rng.gen::<f64>() - 0.5) * 1e6,
                3 => [f64::NEG_INFINITY, -0.0, 0.0, 1.5, f64::INFINITY, -2.25, 1e300][rng.gen_range(0..7)],
                _ => -(rng.gen_range(0..100_000) as f64) / 1000.0,
            })
        })
        .collect()
}

fn docs_of(ints: &[Option<i64>], floats: &[Option<f64>]) -> Vec<Document> {
    ints.iter()
        .zip(floats)
        .map(|(i, f)| vec![i.map_or(Value::Null, Value::Int), f.map_or(Value::Null, Value::Float)])
        .collect()
}

fn naive(vals: &[Option<f64>], lo: f64, hi: f64) -> Vec<u32> {
    vals.iter().enumerate().filter(|(_, v)| v.is_some_and(|v| lo <= v && v <= hi)).map(|(d, _)| d as u32).collect()
}

/// Random bounds: at values, between them, open ends, empty ranges.
fn bounds(rng: &mut StdRng, vals: &[Option<f64>]) -> (f64, f64) {
    let present: Vec<f64> = vals.iter().flatten().copied().filter(|v| v.is_finite()).collect();
    let pick = |rng: &mut StdRng| match rng.gen_range(0..6) {
        0 if !present.is_empty() => present[rng.gen_range(0..present.len())],
        1 => f64::NEG_INFINITY,
        2 => f64::INFINITY,
        3 if !present.is_empty() => present[rng.gen_range(0..present.len())] + 0.001,
        _ => {
            let (a, b) = present.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            if a <= b {
                a + (b - a) * rng.gen::<f64>()
            } else {
                rng.gen::<f64>()
            }
        }
    };
    let (a, b) = (pick(rng), pick(rng));
    if rng.gen_bool(0.1) {
        (a.max(b), a.min(b)) // usually empty
    } else {
        (a.min(b), a.max(b))
    }
}

fn check_column(col: &NumericColumn, idx: Option<&RangeIndex>, vals: &[Option<f64>], rng: &mut StdRng) {
    let n = vals.len() as u32;
    for (d, v) in vals.iter().enumerate() {
        let got = col.get_f64(d as u32);
        assert!(got == *v || (got.is_some_and(f64::is_nan) && v.is_some_and(f64::is_nan)), "doc {d}: {got:?} vs {v:?}");
    }
    // Keys order like the values, NaN above +inf.
    let by_key = |a: &&f64, b: &&f64| float_key(**a).cmp(&float_key(**b));
    assert_eq!(col.n_present(), vals.iter().flatten().count());
    let same = |a: Option<f64>, b: Option<&f64>| a.map(f64::to_bits) == b.map(|b| if *b == 0.0 { 0.0f64.to_bits() } else { b.to_bits() });
    assert!(same(col.min(), vals.iter().flatten().min_by(by_key)));
    assert!(same(col.max(), vals.iter().flatten().max_by(by_key)));
    for _ in 0..150 {
        let (lo, hi) = bounds(rng, vals);
        let want = naive(vals, lo, hi);
        let want_set = DocSet::from_sorted(want.clone(), n);
        assert_eq!(col.scan(lo, hi), want_set, "scan [{lo}, {hi}]");
        assert_eq!(col.scan_count(lo, hi), want.len());
        for s in [RangeStrategy::Scan, RangeStrategy::Postings, RangeStrategy::Auto] {
            assert_eq!(eval_range(col, idx, lo, hi, s), want_set, "{s:?} [{lo}, {hi}]");
            assert_eq!(count_range(col, idx, lo, hi, s), want.len(), "{s:?} count [{lo}, {hi}]");
        }
        if let Some(idx) = idx {
            assert_eq!(idx.eval(lo, hi), want_set);
            assert_eq!(idx.count(lo, hi), want.len());
        }
        let f = col.filter(lo, hi);
        for _ in 0..if n > 0 { 50 } else { 0 } {
            let d = rng.gen_range(0..n);
            let want = want.binary_search(&d).is_ok();
            assert_eq!(col.probe(d, lo, hi), want);
            assert_eq!(strata::segment::DocFilter::contains(&f, d), want);
        }
        // Estimates are exact on bucket-aligned ranges and bounded overall.
        let e = col.estimate(lo, hi);
        assert!(e >= 0.0 && e <= col.n_present() as f64 + 1e-6, "estimate {e}");
    }
    // Everything but NaN (a bucket shared with NaN counts it too).
    let all = col.estimate(f64::NEG_INFINITY, f64::INFINITY);
    let nan = vals.iter().flatten().filter(|v| v.is_nan()).count() as f64;
    let not_nan = col.n_present() as f64 - nan;
    assert!(all >= not_nan - 1e-6 && all <= not_nan + nan + 1e-6, "estimate of everything {all} vs {not_nan}");
}

#[test]
fn columns_and_ranges_match_naive() {
    let mut rng = StdRng::seed_from_u64(1);
    for shape in 0..6 {
        for n in [0usize, 1, 127, 128, 129, 1000, 9000] {
            let ints = int_values(&mut rng, n, shape);
            let floats = float_values(&mut rng, n, shape.min(4));
            let docs = docs_of(&ints, &floats);
            for buckets in [64usize, 7, 0] {
                let opts = BuildOptions::default().set("forward.range_buckets", buckets);
                let seg = build(&schema(), &docs, &opts);
                let ci = NumericColumn::open(&seg, "i").unwrap();
                let cf = NumericColumn::open(&seg, "f").unwrap();
                assert!(ci.is_view() && cf.is_view());
                assert_eq!(ci.kind(), NumericKind::Int);
                let ii = RangeIndex::open_if_present(&seg, "i").unwrap();
                let fi = RangeIndex::open_if_present(&seg, "f").unwrap();
                assert_eq!(ii.is_some(), buckets > 0);
                for (d, v) in ints.iter().enumerate() {
                    assert_eq!(ci.get_i64(d as u32), *v);
                    assert_eq!(ci.get(d as u32), v.map_or(Value::Null, Value::Int));
                }
                let as_f64: Vec<Option<f64>> = ints.iter().map(|v| v.map(|v| v as f64)).collect();
                // Integer bounds beyond 2^53 are inexact as doubles; keep
                // those columns to the access checks above.
                if shape != 5 {
                    check_column(&ci, ii.as_ref(), &as_f64, &mut rng);
                }
                check_column(&cf, fi.as_ref(), &floats, &mut rng);
                if let Some(idx) = &fi {
                    assert!((0..idx.len()).all(|b| idx.bucket(b).is_view()));
                }
            }
        }
    }
}

#[test]
fn float_encodings() {
    let mut rng = StdRng::seed_from_u64(2);
    let n = 3000;
    let cases: [(Vec<Option<f64>>, NumericKind); 4] = [
        ((0..n).map(|_| Some(rng.gen_range(0..500) as f64 / 100.0)).collect(), NumericKind::Decimal(2)),
        ((0..n).map(|i| Some((i % 9) as f64)).collect(), NumericKind::Decimal(0)),
        ((0..n).map(|_| Some(rng.gen::<f64>())).collect(), NumericKind::FloatBits),
        ((0..n).map(|i| Some(if i == 5 { f64::NAN } else { 1.25 })).collect(), NumericKind::FloatBits),
    ];
    for (vals, kind) in cases {
        let docs: Vec<Document> = vals.iter().map(|v| vec![Value::Int(0), Value::Float(v.unwrap())]).collect();
        let seg = build(&schema(), &docs, &BuildOptions::default());
        let col = NumericColumn::open(&seg, "f").unwrap();
        assert_eq!(col.kind(), kind);
        check_column(&col, RangeIndex::open_if_present(&seg, "f").unwrap().as_ref(), &vals, &mut rng);
    }
    // Ratings pack in 9 bits per document (plus zone maps).
    let docs: Vec<Document> = (0..12_800).map(|_| vec![Value::Int(0), Value::Float(rng.gen_range(0..500) as f64 / 100.0)]).collect();
    let seg = build(&schema(), &docs, &BuildOptions::default());
    let sizes = NumericColumn::open(&seg, "f").unwrap().byte_sizes();
    assert_eq!(sizes[2], 12_800 * 9 / 8);
    // An integer stored in a FLOAT field reads back as a float.
    let docs = vec![vec![Value::Int(1), Value::Int(3)], vec![Value::Int(2), Value::Float(0.5)]];
    let seg = build(&schema(), &docs, &BuildOptions::default());
    let col = NumericColumn::open(&seg, "f").unwrap();
    assert_eq!(col.get(0), Value::Float(3.0));
    assert_eq!(col.get_i64(1), Some(0));
}

#[test]
fn zone_maps_prune_sorted_columns() {
    let n = 100_000;
    let docs: Vec<Document> = (0..n).map(|d| vec![Value::Int(d as i64), Value::Float(0.0)]).collect();
    let seg = build(&schema(), &docs, &BuildOptions::default());
    let col = NumericColumn::open(&seg, "i").unwrap();
    // A range inside one block decodes only that block; one aligned to
    // blocks decodes none.
    assert_eq!(col.blocks_to_decode(1000.0, 1010.0), 1);
    assert_eq!(col.blocks_to_decode(1280.0, 2559.0), 0);
    assert_eq!(col.scan_count(1280.0, 2559.0), 1280);
    let idx = RangeIndex::open(&seg, "i").unwrap();
    let p = idx.plan(0.0, 50_000.0).unwrap();
    assert_eq!(p.covered.len() + p.boundary.len(), 33);
    assert!(p.boundary.len() <= 1);
    let est = col.estimate(0.0, 50_000.0);
    assert!((est - 50_001.0).abs() < 1.0, "estimate {est}");
}
