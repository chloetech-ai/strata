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

/// The name of field `name`'s sections (by its id).
fn key(name: &str) -> String {
    schema().key_of(name).unwrap()
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
                let col = KeywordColumn::open(&seg, &key("k")).unwrap();
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
    let col = KeywordColumn::open(&seg, &key("k")).unwrap();
    let sizes = col.byte_sizes();
    assert_eq!(sizes.lists, 0);
    // Every value on one document: docIDs only, offsets implied.
    assert_eq!(sizes.rare, 4 * 20_000);
    assert_eq!(col.docs("id-777").to_sorted(), vec![777]);
}

/// Values with Pyramid lists and rare values holding, together, as many
/// documents as there are values: the writer took that for "one document
/// per value" and left out the rare values' offsets, so a rare value
/// matched another value's documents.
#[test]
fn rare_values_with_as_many_documents_as_values_keep_their_offsets() {
    let mut rng = StdRng::seed_from_u64(3);
    for common in 1..=3usize {
        for n_rare in 1..=4usize {
            let mut vals: Vec<Option<String>> = Vec::new();
            for c in 0..common {
                vals.extend((0..64 + 37 * c).map(|_| Some(format!("common{c}"))));
            }
            // One document each, plus `common` more: as many as values.
            let mut counts = vec![1usize; n_rare];
            for _ in 0..common {
                counts[rng.gen_range(0..n_rare)] += 1;
            }
            for (r, &k) in counts.iter().enumerate() {
                vals.extend((0..k).map(|_| Some(format!("rare{r}"))));
            }
            for i in (1..vals.len()).rev() {
                vals.swap(i, rng.gen_range(0..=i));
            }
            let docs: Vec<Document> = vals.iter().map(|v| vec![v.clone().map_or(Value::Null, Value::Str)]).collect();
            let seg = build(&schema(), &docs, &BuildOptions::default());
            let col = KeywordColumn::open(&seg, &key("k")).unwrap();
            assert!(!col.needs_values());
            assert!(col.is_view());
            check(&col, &vals, &mut rng);
        }
    }
}

/// A database written before that fix (main at 5e2687c, with the program
/// in `tests/fixtures/keyword_offsets/README.md`), whose keyword columns
/// lost those offsets: a single-valued column, a multi-valued KEYWORD
/// column and a multi-valued INT column. Opening repairs them (from the
/// codes, and from the stored values), and a merge rewrites them.
#[test]
fn columns_written_without_their_offsets_are_repaired() {
    use std::path::Path;
    use strata::db::Db;
    use strata::query::Params;
    use strata::storage::{DbOptions, MergePolicy};

    let dir = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keyword_offsets");
    let path = dir.path().join("t.strata");
    for (from, to) in [("t.strata", path.clone()), ("t.strata-wal", dir.path().join("t.strata-wal"))] {
        std::fs::copy(fixtures.join(from), to).unwrap();
    }
    // The documents of the fixture program.
    let tag = |i: usize| match i {
        0 => "one",
        1 | 2 => "two",
        _ => "common",
    };
    let tags = |i: usize| match i {
        10 | 20 => vec!["all", "x"],
        _ => vec!["all"],
    };
    let sizes = |i: usize| match i {
        30 | 40 => vec![5, 9],
        _ => vec![5],
    };
    let ids = |db: &Db, sql: &str| -> Vec<usize> {
        let r = db.query(&format!("SELECT id FROM t WHERE {sql} LIMIT 1000"), &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let mut out: Vec<usize> = r.rows.iter().map(|r| r[0].as_str().unwrap()[1..].parse().unwrap()).collect();
        out.sort();
        out
    };
    let check = |db: &Db, what: &str, other: bool| {
        for v in ["one", "two", "common", "none"] {
            assert_eq!(ids(db, &format!("tag = '{v}'")), (0..70).filter(|&i| tag(i) == v).collect::<Vec<_>>(), "{what}: tag = {v}");
        }
        assert_eq!(ids(db, "tag IN ('one', 'two')"), vec![0, 1, 2], "{what}");
        for v in ["all", "x"] {
            assert_eq!(ids(db, &format!("tags = '{v}'")), (0..70).filter(|&i| tags(i).contains(&v)).collect::<Vec<_>>(), "{what}: tags = {v}");
        }
        for v in [5, 9] {
            assert_eq!(ids(db, &format!("sizes = {v}")), (0..70).filter(|&i| sizes(i).contains(&v)).collect::<Vec<_>>(), "{what}: sizes = {v}");
        }
        let r = db.query("SELECT tag, COUNT(*) FROM t GROUP BY tag ORDER BY tag", &Params::new()).unwrap();
        let counts: Vec<(String, i64)> = r.rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_i64().unwrap())).collect();
        let mut want = vec![("common".to_string(), 67), ("one".to_string(), 1), ("two".to_string(), 2)];
        if other {
            want.insert(2, ("other".to_string(), 1));
        }
        assert_eq!(counts, want, "{what}");
    };
    // Written before field ids too: it opens to be copied.
    let old = DbOptions { open_old: true, ..DbOptions::default() };
    let db = Db::open(&path, old.clone()).unwrap();
    assert_eq!(db.snapshot().segments().len(), 1);
    check(&db, "as written", false);
    db.storage().check().unwrap();
    // A merge rebuilds the segment with offsets.
    let mut t = db.storage().begin().unwrap();
    t.insert(vec![Value::Str("k070".into()), Value::Str("other".into()), Value::List(vec![]), Value::List(vec![])]).unwrap();
    t.commit().unwrap();
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 1_000, ..MergePolicy::default() }).unwrap();
    assert_eq!(db.snapshot().segments().len(), 1);
    assert_eq!(ids(&db, "tag = 'other'"), vec![70]);
    check(&db, "merged", true);
    drop(db);
    let db = Db::open(&path, old).unwrap();
    check(&db, "reopened", true);
    drop(db);
    // And its copy holds the same.
    strata::upgrade::upgrade(&path).unwrap().unwrap();
    let db = Db::open(&path, DbOptions::default()).unwrap();
    check(&db, "upgraded", true);
    db.storage().check().unwrap();
}
