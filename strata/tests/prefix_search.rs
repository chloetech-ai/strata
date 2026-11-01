//! KEYWORD prefixes: `LIKE 'p%'` and `PREFIX(f, 'p')` select the rows with
//! a value starting with `p`, as one run of the field's sorted dictionary,
//! and agree with a scan of the documents through the WAL, flushes,
//! reopening and merges; the autocomplete shape (a prefix, `ORDER BY` a
//! count, `LIMIT k`) returns the top k. With `fuzzy`, a walk of the
//! dictionary selects the values with a prefix within that many edits,
//! first letter exact, as a scan of every value finds them.

use strata::db::{Db, SqlResult};
use strata::query::ir::Plan;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::{DbOptions, MergePolicy};
use strata::value::{Document, Value};

fn schema() -> Schema {
    Schema::new(
        "queries",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("lang", FieldType::Keyword),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("n", FieldType::Int),
            Field::new("shown", FieldType::Text { positions: false }).stored_only(),
        ],
        Some("id"),
    )
    .unwrap()
}

const WORDS: [&str; 12] = ["how", "to", "install", "instant", "in", "a_b", "50%", "été", "étage", "zzz", "h", "ho"];

/// Query `i`: two or three words (the first from 'a'-heavy or 'h'-heavy
/// halves, so that one-letter prefixes cover over a thousand values), a
/// count, a language for most, and up to two tags.
fn doc(i: usize) -> Document {
    let mut x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66;
    let mut next = |m: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % m as u64) as usize
    };
    let lead = if i % 2 == 0 { "a" } else { "h" };
    let words: Vec<&str> = (0..2 + next(2)).map(|_| WORDS[next(WORDS.len())]).collect();
    let id = format!("{lead}{} {} #{i}", words.join(" "), next(10));
    let lang = match i % 5 {
        0 => Value::Null,
        1 => Value::Str("en".into()),
        2 => Value::Str("en-gb".into()),
        3 => Value::Str("es".into()),
        _ => Value::Str("fr".into()),
    };
    let tags: Vec<Value> = (0..next(3)).map(|_| Value::Str(WORDS[next(WORDS.len())].to_string())).collect();
    vec![Value::Str(id.clone()), lang, Value::List(tags), Value::Int(((i * 7919) % 100_003) as i64), Value::Str(id)]
}

fn ids(db: &Db, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

fn want(docs: &[Document], f: impl Fn(&Document) -> bool) -> Vec<String> {
    let mut out: Vec<String> = docs.iter().filter(|d| f(d)).map(|d| d[0].as_str().unwrap().to_string()).collect();
    out.sort();
    out
}

fn starts(v: &Value, p: &str) -> bool {
    v.as_str().is_some_and(|s| s.starts_with(p))
}

fn any_tag(d: &Document, p: &str) -> bool {
    d[2].items().iter().any(|v| starts(v, p))
}

/// Whether `v` has a prefix within `k` edits of `p` with `p`'s first
/// letter, by the edit distance of every prefix of `v`.
fn near(v: &Value, p: &str, k: u8) -> bool {
    let (Some(v), q) = (v.as_str(), p.chars().collect::<Vec<char>>()) else { return false };
    let v: Vec<char> = v.chars().collect();
    match q.first() {
        None => true,
        Some(c) => v.first() == Some(c) && (1..=v.len()).any(|j| strata::text::fuzzy::within(&q, &v[..j].iter().collect::<String>(), k).is_some()),
    }
}

/// The fewest edits from `p` to a prefix of `v` with `p`'s first letter,
/// if one is within `k`.
fn fewest(v: &Value, p: &str, k: u8) -> Option<u8> {
    let (v, q) = (v.as_str()?.chars().collect::<Vec<char>>(), p.chars().collect::<Vec<char>>());
    if v.first() != q.first() {
        return None;
    }
    (1..=v.len()).filter_map(|j| strata::text::fuzzy::within(&q, &v[..j].iter().collect::<String>(), k)).min()
}

/// A literal for SQL: quotes doubled.
fn q(s: &str) -> String {
    s.replace('\'', "''")
}

/// `p` escaped for LIKE with `!` as the escape character.
fn esc(p: &str) -> String {
    p.chars().flat_map(|c| if matches!(c, '%' | '_' | '!') { vec!['!', c] } else { vec![c] }).collect()
}

fn prefixes(docs: &[Document]) -> Vec<String> {
    let mut ps: Vec<String> = vec!["a".into(), "h".into(), "ah".into(), "hinst".into(), "how".into(), "x".into(), "é".into(), "zzzz".into()];
    for d in docs.iter().step_by(97) {
        let id = d[0].as_str().unwrap();
        for k in [1, 2, 4, 7, 12, id.len()] {
            if let Some(p) = id.get(..k.min(id.len())) {
                ps.push(p.to_string());
            }
        }
    }
    ps.extend(["a a_b".to_string(), "h50%".to_string(), "a été".to_string()]);
    ps.sort();
    ps.dedup();
    ps
}

fn check(db: &Db, docs: &[Document]) {
    let all = "SELECT id FROM queries";
    for p in prefixes(docs) {
        let w = want(docs, |d| starts(&d[0], &p));
        assert_eq!(ids(db, &format!("{all} WHERE id LIKE '{}%' ESCAPE '!'", q(&esc(&p)))), w, "LIKE {p:?}");
        assert_eq!(ids(db, &format!("{all} WHERE PREFIX(id, '{}')", q(&p))), w, "PREFIX {p:?}");
        assert_eq!(ids(db, &format!("{all} WHERE id NOT LIKE '{}%' ESCAPE '!'", q(&esc(&p)))), want(docs, |d| !starts(&d[0], &p)), "NOT LIKE {p:?}");
        // Multi-valued: some value starts with it; NOT: none does.
        assert_eq!(ids(db, &format!("{all} WHERE PREFIX(tags, '{}')", q(&p))), want(docs, |d| any_tag(d, &p)), "tags {p:?}");
        assert_eq!(ids(db, &format!("{all} WHERE NOT PREFIX(tags, '{}')", q(&p))), want(docs, |d| !any_tag(d, &p)), "NOT tags {p:?}");
        // Probed against a selective conjunct, and counted.
        assert_eq!(
            ids(db, &format!("{all} WHERE n < 300 AND PREFIX(id, '{}')", q(&p))),
            want(docs, |d| d[3].as_i64().unwrap() < 300 && starts(&d[0], &p)),
            "probe {p:?}"
        );
        let count = db.query(&format!("SELECT COUNT(*) FROM queries WHERE PREFIX(id, '{}')", q(&p)), &Params::new()).unwrap();
        assert_eq!(count.rows[0][0], Value::Int(w.len() as i64), "count {p:?}");
        // Autocomplete: the ten most asked with the prefix.
        let r = db.query(&format!("SELECT id, n FROM queries WHERE PREFIX(id, '{}') ORDER BY n DESC, id LIMIT 10", q(&p)), &Params::new()).unwrap();
        let mut top: Vec<(i64, String)> = docs.iter().filter(|d| starts(&d[0], &p)).map(|d| (-d[3].as_i64().unwrap(), d[0].as_str().unwrap().to_string())).collect();
        top.sort();
        let top: Vec<String> = top.into_iter().take(10).map(|(_, id)| id).collect();
        assert_eq!(r.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect::<Vec<_>>(), top, "top {p:?}");
    }
    // With typos, by a scan of every value; a sample of the prefixes, and
    // some typed with a swap, a missing or an extra letter.
    let mut typos: Vec<String> = prefixes(docs).into_iter().step_by(3).collect();
    typos.extend(["hwo to", "hiw ot", "ainstal", "hinstnat", "a tto", "aété", "hho too", "h", "ab", "qwerty"].map(String::from));
    for p in &typos {
        let auto = match p.chars().count() {
            0..=2 => 0,
            3..=5 => 1,
            _ => 2,
        };
        for (fz, k) in [("1", 1), ("2", 2), ("'auto'", auto)] {
            let w = want(docs, |d| near(&d[0], p, k));
            assert_eq!(ids(db, &format!("{all} WHERE PREFIX(id, '{}', fuzzy => {fz})", q(p))), w, "fuzzy {fz} {p:?}");
            assert_eq!(ids(db, &format!("{all} WHERE PREFIX(tags, '{}', fuzzy => {fz})", q(p))), want(docs, |d| d[2].items().iter().any(|v| near(v, p, k))), "tags fuzzy {fz} {p:?}");
            assert_eq!(
                ids(db, &format!("{all} WHERE NOT PREFIX(tags, '{}', fuzzy => {fz})", q(p))),
                want(docs, |d| !d[2].items().iter().any(|v| near(v, p, k))),
                "NOT tags fuzzy {fz} {p:?}"
            );
            assert_eq!(
                ids(db, &format!("{all} WHERE n < 300 AND PREFIX(id, '{}', fuzzy => {fz})", q(p))),
                want(docs, |d| d[3].as_i64().unwrap() < 300 && near(&d[0], p, k)),
                "probe fuzzy {fz} {p:?}"
            );
            let count = db.query(&format!("SELECT COUNT(*) FROM queries WHERE PREFIX(id, '{}', fuzzy => {fz})", q(p)), &Params::new()).unwrap();
            assert_eq!(count.rows[0][0], Value::Int(w.len() as i64), "count fuzzy {fz} {p:?}");
            let r = db.query(&format!("SELECT id, n FROM queries WHERE PREFIX(id, '{}', fuzzy => {fz}) ORDER BY n DESC, id LIMIT 10", q(p)), &Params::new()).unwrap();
            let mut top: Vec<(i64, String)> = docs.iter().filter(|d| near(&d[0], p, k)).map(|d| (-d[3].as_i64().unwrap(), d[0].as_str().unwrap().to_string())).collect();
            top.sort();
            let top: Vec<String> = top.into_iter().take(10).map(|(_, id)| id).collect();
            assert_eq!(r.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect::<Vec<_>>(), top, "top fuzzy {fz} {p:?}");
        }
        // Completions ranked by their edits, exact ones first, each tier
        // by its count (IR-N1 of the review of 31 October).
        let sql = format!(
            "SELECT id, n FROM queries WHERE PREFIX(id, '{p}', fuzzy => 2) ORDER BY CASE WHEN PREFIX(id, '{p}') THEN 2 WHEN PREFIX(id, '{p}', fuzzy => 1) THEN 1 ELSE 0 END * 1000000 + n DESC LIMIT 10",
            p = q(p)
        );
        let r = db.query(&sql, &Params::new()).unwrap();
        let mut top: Vec<(u8, i64, String)> = docs.iter().filter_map(|d| fewest(&d[0], p, 2).map(|e| (e, -d[3].as_i64().unwrap(), d[0].as_str().unwrap().to_string()))).collect();
        top.sort();
        let top: Vec<String> = top.into_iter().take(10).map(|(_, _, id)| id).collect();
        assert_eq!(r.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect::<Vec<_>>(), top, "exact first {p:?}");
    }
    // A nullable field: NOT holds only where there is a value.
    assert_eq!(ids(db, &format!("{all} WHERE lang LIKE 'en%'")), want(docs, |d| starts(&d[1], "en")));
    assert_eq!(ids(db, &format!("{all} WHERE NOT (lang LIKE 'en%')")), want(docs, |d| d[1] != Value::Null && !starts(&d[1], "en")));
    assert_eq!(ids(db, &format!("{all} WHERE lang NOT LIKE 'e%' OR lang IS NULL")), want(docs, |d| !starts(&d[1], "e")));
    // No wildcard is equality; '%' alone is any value.
    assert_eq!(ids(db, &format!("{all} WHERE lang LIKE 'en'")), want(docs, |d| d[1].as_str() == Some("en")));
    assert_eq!(ids(db, &format!("{all} WHERE lang LIKE '%%'")), want(docs, |d| d[1] != Value::Null));
    assert_eq!(ids(db, &format!("{all} WHERE PREFIX(lang, '')")), want(docs, |d| d[1] != Value::Null));
    // A parameter.
    let params = Params::new().set("p", Value::Str("en%".into()));
    let got = db.query(&format!("{all} WHERE lang LIKE :p"), &params).unwrap().rows.len();
    assert_eq!(got, want(docs, |d| starts(&d[1], "en")).len());
}

#[test]
fn prefixes_through_the_lifecycle() {
    let docs: Vec<Document> = (0..6000).map(doc).collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.strata");
    let opts = || DbOptions { segment_max_docs: 2500, ..DbOptions::default() };
    {
        let db = Db::create(&path, schema(), opts()).unwrap();
        db.load(docs[..4000].iter().cloned()).unwrap();
        let mut t = db.storage().begin().unwrap();
        for d in &docs[4000..5200] {
            t.insert(d.clone()).unwrap();
        }
        t.commit().unwrap();
        db.storage().flush().unwrap();
        let mut t = db.storage().begin().unwrap();
        for d in &docs[5200..] {
            t.insert(d.clone()).unwrap();
        }
        t.commit().unwrap();
        check(&db, &docs);
    }
    let db = Db::open(&path, opts()).unwrap();
    check(&db, &docs);
    db.storage().flush().unwrap();
    db.storage().merge(&MergePolicy { merge_factor: 2, floor_docs: 100_000, ..MergePolicy::default() }).unwrap();
    check(&db, &docs);
}

#[test]
fn like_patterns_the_index_cannot_answer_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("r.strata"), schema(), DbOptions::default()).unwrap();
    db.load((0..50).map(doc)).unwrap();
    let none = Params::new();
    for (sql, says) in [
        ("SELECT id FROM queries WHERE id LIKE '%how%'", "not indexed"),
        ("SELECT id FROM queries WHERE id LIKE 'a_b%'", "not indexed"),
        ("SELECT id FROM queries WHERE id ILIKE 'how%'", "lowercased"),
        ("SELECT id FROM queries WHERE n LIKE '1%'", "KEYWORD"),
        ("SELECT id FROM queries WHERE PREFIX(id, 'a', synonyms => 1)", "unknown option"),
        ("SELECT id FROM queries WHERE PREFIX(id, 'a', fuzzy => 3)", "fuzzy => 0, 1, 2 or 'auto'"),
        ("SELECT id FROM queries WHERE PREFIX(id, 3)", "text"),
        ("SELECT id FROM queries WHERE id LIKE 'a!' ESCAPE '!'", "escape"),
    ] {
        let e = db.query(sql, &none).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    }
}

#[test]
fn a_prefix_plan_names_its_feature_and_runs_from_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("j.strata"), schema(), DbOptions::default()).unwrap();
    let docs: Vec<Document> = (0..300).map(doc).collect();
    db.load(docs.iter().cloned()).unwrap();
    let none = Params::new();
    let sql = "SELECT id FROM queries WHERE id LIKE 'a!_%' ESCAPE '!' OR PREFIX(id, 'hho') ORDER BY n DESC, id LIMIT 5";
    let plan = db.explain(sql, &none).unwrap();
    assert!(plan.features.iter().any(|f| f == "prefix"), "{:?}", plan.features);
    let text = plan.explain(&[&db.schema()]).join("\n");
    assert!(text.contains("PREFIX(id, 'a_')"), "{text}");
    let again = Plan::from_json(&serde_json::from_str(&plan.to_json_text().unwrap()).unwrap()).unwrap();
    let want = db.query(sql, &none).unwrap().rows;
    match db.run(&again).unwrap() {
        SqlResult::Rows(r) => assert_eq!(r.rows, want),
        SqlResult::Written(_) => panic!("a query"),
    }
    // A reader without the feature refuses the plan by name.
    let e = Plan::readable_by(1, &["prefix"], &["operators"]).unwrap_err().to_string();
    assert!(e.contains("prefix"), "{e}");
    // With typos, a second feature, which a reader of exact prefixes only
    // refuses rather than read the prefix as exact.
    let sql = "SELECT id FROM queries WHERE PREFIX(id, 'hwo', fuzzy => 'auto') ORDER BY n DESC, id LIMIT 5";
    let plan = db.explain(sql, &none).unwrap();
    assert!(plan.features.iter().any(|f| f == "prefix_edits"), "{:?}", plan.features);
    let text = plan.explain(&[&db.schema()]).join("\n");
    assert!(text.contains("PREFIX(id, 'hwo', fuzzy => 1)"), "{text}");
    let again = Plan::from_json(&serde_json::from_str(&plan.to_json_text().unwrap()).unwrap()).unwrap();
    let want = db.query(sql, &none).unwrap().rows;
    assert!(!want.is_empty());
    match db.run(&again).unwrap() {
        SqlResult::Rows(r) => assert_eq!(r.rows, want),
        SqlResult::Written(_) => panic!("a query"),
    }
    let e = Plan::readable_by(1, &["prefix", "prefix_edits"], &["prefix"]).unwrap_err().to_string();
    assert!(e.contains("prefix_edits"), "{e}");
    // Over rows of a subquery, PREFIX tests each value.
    let rows = db.query("SELECT id FROM (SELECT id, lang FROM queries) AS t WHERE PREFIX(id, 'ha') AND PREFIX(lang, 'en')", &none).unwrap().rows;
    assert_eq!(rows.len(), want_len(&docs, |d| starts(&d[0], "ha") && starts(&d[1], "en")));
}

fn want_len(docs: &[Document], f: impl Fn(&Document) -> bool) -> usize {
    docs.iter().filter(|d| f(d)).count()
}
