//! Workload generation on SciFact (skipped if the collection is not
//! prepared): deterministic, exact, on target, and identical to the
//! committed workload file.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use strata::data::{self, col};
use strata::workload::{filter_matches, filter_sql, generate, tokens, Kind, Shape, Workload, WorkloadConfig};

#[test]
fn scifact_workload_is_deterministic_and_on_target() {
    if !data::data_dir().join("scifact/meta.json").exists() {
        eprintln!("SciFact not prepared; skipping");
        return;
    }
    let coll = data::load("scifact", None).unwrap();
    let cfg = WorkloadConfig::default();
    let w = generate(&coll, &cfg);
    assert_eq!(w, generate(&coll, &cfg), "generation is not deterministic");
    assert_eq!(w.n_docs, coll.docs.len());
    assert_eq!(w.of_kind(Kind::Bm25).len(), coll.queries.len());
    for kind in Kind::ALL {
        assert!(!w.of_kind(kind).is_empty(), "no {} queries", kind.name());
    }

    let vocabulary: HashSet<String> = coll.docs.iter().flat_map(|d| tokens(d[col::BODY].as_str().unwrap()).collect::<Vec<_>>()).collect();
    let n = coll.docs.len() as f64;
    let mut covered: BTreeMap<(Kind, String), usize> = BTreeMap::new();
    for q in &w.queries {
        for word in &q.words {
            assert!(vocabulary.contains(word), "{word} occurs in no document");
        }
        let Some(target) = q.target else { continue };
        let exact = coll.docs.iter().filter(|d| filter_matches(&q.filter, d)).count() as u64;
        assert_eq!(q.matches, Some(exact), "{}", q.sql);
        let sel = exact as f64 / n;
        assert!((sel - target).abs() <= cfg.tolerance * target + 1e-12, "selectivity {sel} for target {target}: {}", q.sql);
        assert!(q.sql.contains(&filter_sql(&q.filter)), "{}", q.sql);
        *covered.entry((q.kind, format!("{target}"))).or_default() += 1;
        if let Some(Shape::TopicNear | Shape::TopicFar) = q.shape {
            assert!(q.filter.iter().any(|p| matches!(p, strata::workload::Pred::Topic(_))));
        }
    }
    for kind in [Kind::Filter, Kind::FilteredBm25, Kind::FilteredVector, Kind::FilteredHybrid] {
        for t in &cfg.targets {
            assert!(covered.get(&(kind, format!("{t}"))).is_some_and(|&c| c > 0), "{} has no filter at {t}", kind.name());
        }
    }

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("results/workloads/scifact.json");
    if path.exists() {
        assert!(Workload::load(&path).unwrap() == w, "{} is stale; regenerate it", path.display());
    }
}
