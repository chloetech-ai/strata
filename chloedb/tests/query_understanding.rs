//! Query understanding: expansion by pseudo-relevance feedback
//! (`bm25(.., expand => n)`, [`chloedb::query::prf`]) and "did you mean"
//! ([`Db::correct`]).

use chloedb::db::Db;
use chloedb::query::Params;
use chloedb::schema::{Field, FieldType, Schema};
use chloedb::storage::DbOptions;
use chloedb::value::Value;

fn db(dir: &tempfile::TempDir) -> Db {
    let schema = Schema::new(
        "papers",
        vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: false }), Field::new("year", FieldType::Int)],
        Some("id"),
    )
    .unwrap();
    let db = Db::create(&dir.path().join("db.strata"), schema, DbOptions::default()).unwrap();
    let mut rows = Vec::new();
    // Vaccine papers say "immune" and "antibodies" often; one paper about
    // antibodies never says "vaccine".
    for i in 0..30 {
        rows.push(vec![Value::Str(format!("v{i}")), Value::Str(format!("mRNA vaccines raise antibodies and an immune response, trial {i}")), Value::Int(2015 + (i % 10) as i64)]);
    }
    rows.push(vec![Value::Str("ab".into()), Value::Str("antibodies of the immune system in mice".into()), Value::Int(2024)]);
    for i in 0..30 {
        rows.push(vec![Value::Str(format!("o{i}")), Value::Str(format!("graph neural networks for molecules, study {i}")), Value::Int(2020)]);
    }
    db.load(rows.into_iter()).unwrap();
    db
}

fn ids(db: &Db, sql: &str, q: &str) -> (Vec<String>, Vec<String>) {
    let r = db.query(sql, &Params::new().set("q", Value::Str(q.into()))).unwrap_or_else(|e| panic!("{sql}: {e}"));
    (r.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect(), r.stats.plan.clone())
}

#[test]
fn expand_finds_rows_with_the_words_of_the_top_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    // Plain BM25 for "vaccines" never reaches the antibody paper.
    let (plain, _) = ids(&db, "SELECT id FROM papers WHERE MATCH(body, 'vaccines OR antibodies OR immune') ORDER BY bm25(body, :q) DESC LIMIT 100", "vaccines");
    assert!(!plain.contains(&"ab".to_string()), "{plain:?}");
    let (expanded, plan) = ids(&db, "SELECT id FROM papers ORDER BY bm25(body, :q, expand => 5) DESC LIMIT 100", "vaccines");
    assert!(expanded.contains(&"ab".to_string()), "{expanded:?}");
    assert!(!expanded.iter().any(|i| i.starts_with('o')), "{expanded:?}");
    assert!(plan.iter().any(|l| l.starts_with("bm25(body) expanded from 5 rows of papers with") && l.contains("immun")), "{plan:?}");
    // The vaccine papers still rank first: the query keeps most of the weight.
    assert!(expanded[0].starts_with('v'), "{expanded:?}");
}

#[test]
fn expand_reads_its_rows_through_the_query_filter_and_takes_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    let p = Params::new().set("q", Value::Str("vaccines".into())).set("n", Value::Int(3));
    let r = db.query("SELECT id, year FROM papers WHERE year >= 2023 ORDER BY bm25(body, :q, expand => :n, expand_terms => 4, expand_weight => 0.5) DESC LIMIT 50", &p).unwrap();
    assert!(r.rows.iter().all(|row| row[1].as_i64().unwrap() >= 2023), "{:?}", r.rows);
    assert!(r.stats.plan.iter().any(|l| l.starts_with("bm25(body) expanded from 3 rows")), "{:?}", r.stats.plan);
    // expand => 0 is the query as written.
    let a = db.query("SELECT id FROM papers ORDER BY bm25(body, :q, expand => 0) DESC LIMIT 5", &p).unwrap();
    let b = db.query("SELECT id FROM papers ORDER BY bm25(body, :q) DESC LIMIT 5", &p).unwrap();
    assert_eq!(a.rows, b.rows);
}

#[test]
fn expand_refuses_what_it_cannot_expand() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    let p = Params::new().set("q", Value::Str("vaccines".into()));
    for (sql, says) in [
        ("SELECT id FROM papers ORDER BY bm25(body, :q, expand => 5, min => 2) DESC", "takes no min"),
        ("SELECT id FROM papers ORDER BY bm25(body, :q, expand => 500) DESC", "from 0 to 100"),
        ("SELECT id FROM papers ORDER BY bm25(body, :q, expand => 5, expand_weight => 2) DESC", "from 0 to 1"),
    ] {
        let e = db.query(sql, &p).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    }
}

#[test]
fn terms_syntax_ranks_analysed_weighted_terms() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    let r = db.query("SELECT id FROM papers ORDER BY bm25(body, 'antibodi^1', syntax => 'terms') DESC LIMIT 100", &Params::new()).unwrap();
    // "antibodi" is the analysed term: not analysed again.
    assert_eq!(r.rows.len(), 31);
    let e = db.query("SELECT id FROM papers ORDER BY bm25(body, 'x^-1', syntax => 'terms') DESC", &Params::new()).unwrap_err().to_string();
    assert!(e.contains("weight"), "{e}");
}

#[test]
fn correct_says_what_a_typed_query_meant() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    let c = db.correct("papers", "body", "mRNA vacines for nueral networks").unwrap();
    assert!(c.changed);
    assert_eq!(c.text, "mRNA vaccines for neural networks");
    let words: Vec<(&str, &str)> = c.words.iter().map(|w| (w.word.as_str(), w.to_word.as_str())).collect();
    assert_eq!(words, vec![("vacines", "vaccines"), ("nueral", "neural")]);
    // Words spelled right, and stopwords, stay.
    let ok = db.correct("papers", "body", "the immune response").unwrap();
    assert!(!ok.changed);
    assert_eq!(ok.text, "the immune response");
    assert!(db.correct("papers", "year", "x").is_err());
    assert!(db.correct("nope", "body", "x").is_err());

}

fn shop(dir: &tempfile::TempDir) -> Db {
    let schema = Schema::new(
        "products",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("body", FieldType::Text { positions: false }),
            Field::new("class", FieldType::Keyword),
            Field::new("color", FieldType::Keyword).multi(),
            Field::new("price", FieldType::Float),
        ],
        Some("id"),
    )
    .unwrap();
    let db = Db::create(&dir.path().join("shop.strata"), schema, DbOptions::default()).unwrap();
    let mut rows = Vec::new();
    let colors = ["white", "black", "oak"];
    for i in 0..60 {
        let (class, noun) = match i % 3 {
            0 => ("Coffee Tables", "table"),
            1 => ("Dining Tables", "table"),
            _ => ("Table Lamps", "lamp"),
        };
        let color = colors[(i / 3) % 3];
        // The words of a class are in some of its rows' text, and "white"
        // in one row whatever its colour.
        let body = match i {
            1 => format!("a white coffee {noun} for the dining room, {i}"),
            _ if class == "Coffee Tables" && i % 2 == 0 => format!("a sturdy coffee table with legs, model {i}"),
            _ => format!("a sturdy {noun} with legs, model {i}"),
        };
        rows.push(vec![Value::Str(format!("p{i}")), Value::Str(body), Value::Str(class.into()), Value::List(vec![Value::Str(color.into())]), Value::Float(50.0 + 10.0 * i as f64)]);
    }
    db.load(rows.into_iter()).unwrap();
    db
}

#[test]
fn understand_names_values_and_ranges() {
    use chloedb::query::understand::{Mode, Options};
    let dir = tempfile::tempdir().unwrap();
    let db = shop(&dir);
    let opts = Options { fields: Some(vec!["class".into(), "color".into(), "price".into()]), ..Options::default() };
    let u = db.understand("products", "body", "white coffee table under $300", &opts).unwrap();
    let said: Vec<(&str, String)> = u.spans.iter().map(|s| (s.words.as_str(), s.condition())).collect();
    assert_eq!(said, vec![("white", "color = 'white'".to_string()), ("coffee table", "class = 'Coffee Tables'".to_string())], "{u:?}");
    assert_eq!(u.ranges.iter().map(|r| r.condition()).collect::<Vec<_>>(), vec!["price < 300"]);
    assert_eq!(u.ranked, "white coffee table");
    assert!(u.spans.iter().all(|s| s.share.is_some()));
    // Without a list, the keyword fields but the key, and no ranges.
    let u = db.understand("products", "body", "lamp under 300", &Options::default()).unwrap();
    assert_eq!(u.read, vec!["class", "color"]);
    assert!(u.ranges.is_empty());
    assert_eq!(u.spans[0].condition(), "class = 'Table Lamps'");
    assert!(db.understand("products", "class", "x", &opts).is_err());
    assert!(db.understand("products", "body", "x", &Options { fields: Some(vec!["nope".into()]), ..opts.clone() }).is_err());
    let _ = Mode::Filter;
}

#[test]
fn understand_boosts_and_filters_a_ranking() {
    let dir = tempfile::tempdir().unwrap();
    let db = shop(&dir);
    let p = Params::new().set("q", Value::Str("white coffee table under 300".into()));
    let run = |opts: &str| {
        let sql = format!("SELECT id, class, price FROM products ORDER BY bm25(body, :q{opts}) DESC LIMIT 20");
        db.query(&sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    // A boost: more coffee tables in the top rows; the text still ranks.
    let coffee = |r: &chloedb::query::exec::ResultSet| r.rows.iter().filter(|row| row[1].as_str() == Some("Coffee Tables")).count();
    let plain = run("");
    let r = run(", understand => 'boost', understand_fields => 'class, color, price', understand_weight => 2");
    assert!(coffee(&r) > coffee(&plain), "{:?} against {:?}", r.rows, plain.rows);
    assert!(coffee(&r) >= 15, "{:?}", r.rows);
    assert!(r.stats.plan[0].starts_with("bm25(body) understood 'white coffee table under 300' with boost of products: boost 2 for each of color = 'white', class = 'Coffee Tables', price < 300"), "{:?}", r.stats.plan);
    // A filter: only those, and none at 300 or more.
    let r = run(", understand => 'filter', understand_fields => 'class, price'");
    assert!(!r.rows.is_empty());
    assert!(r.rows.iter().all(|row| row[1].as_str() == Some("Coffee Tables") && row[2].as_f64().unwrap() < 300.0), "{:?}", r.rows);
    assert!(r.stats.plan[0].contains("WHERE (class = 'Coffee Tables' OR class IS NULL); WHERE price < 300"), "{:?}", r.stats.plan);
    // off is the call as written; understand and expand go together.
    let plain = db.query("SELECT id FROM products ORDER BY bm25(body, :q) DESC LIMIT 10", &p).unwrap();
    let off = db.query("SELECT id FROM products ORDER BY bm25(body, :q, understand => 'off') DESC LIMIT 10", &p).unwrap();
    assert_eq!(plain.rows, off.rows);
    let both = db.query("SELECT id FROM products ORDER BY bm25(body, :q, understand => 'boost', expand => 3) DESC LIMIT 10", &p).unwrap();
    assert!(both.stats.plan.iter().any(|l| l.contains("understood")) && both.stats.plan.iter().any(|l| l.contains("expanded")), "{:?}", both.stats.plan);
    for (sql, says) in [
        ("SELECT id FROM products ORDER BY bm25(body, :q, understand => 'guess') DESC", "understand is one of"),
        ("SELECT id FROM products ORDER BY bm25(body, :q, understand => 'boost', understand_fields => 'body') DESC", "neither"),
        ("SELECT id FROM products ORDER BY bm25(body, :q, understand => 'boost', understand_share => 2) DESC", "from 0 to 1"),
        ("SELECT a.id FROM products a JOIN products b ON a.id = b.id ORDER BY bm25(a.body, :q, understand => 'boost') DESC", "one table"),
    ] {
        let e = db.query(sql, &p).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    }
}

#[test]
fn derived_keywords_tag_rows_with_builtin_models_and_dictionaries() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("news.strata"), Schema::new("brands", vec![Field::new("name", FieldType::Keyword)], Some("name")).unwrap(), DbOptions::default()).unwrap();
    db.sql("INSERT INTO brands (name) VALUES ('Acme Corp'), ('Globex')", &Params::new()).unwrap();
    db.sql(
        "CREATE TABLE news (id KEYWORD PRIMARY KEY, body TEXT, \
         lang KEYWORD OPTIONS(derive = 'body', model = 'builtin:lang'), \
         phrases KEYWORD[] OPTIONS(derive = 'body', model = 'builtin:keyphrases'), \
         dup KEYWORD[] OPTIONS(derive = 'body', model = 'builtin:minhash'), \
         brands KEYWORD[] OPTIONS(derive = 'body', model = 'dict:brands.name'))",
        &Params::new(),
    )
    .unwrap();
    let story = "Globex sued Acme Corp over the patent for linear induction motors on Tuesday, and the court agreed to hear the case.";
    let p = Params::new()
        .set("a", Value::Str(story.into()))
        .set("b", Value::Str(format!("{story} More soon.")))
        .set("c", Value::Str("Der schnelle braune Fuchs springt über den faulen Hund, und das ist nicht schlimm.".into()));
    db.sql("INSERT INTO news (id, body) VALUES ('a', :a), ('b', :b), ('c', :c)", &p).unwrap();
    let r = db.query("SELECT id, lang, brands FROM news ORDER BY id", &Params::new()).unwrap();
    assert_eq!(r.rows[0][1].as_str(), Some("en"));
    assert_eq!(r.rows[2][1].as_str(), Some("de"));
    assert_eq!(r.rows[0][2], Value::List(vec![Value::Str("Globex".into()), Value::Str("Acme Corp".into())]));
    assert_eq!(r.rows[2][2], Value::Null);
    // The values are keywords: filters, facets and joins read them.
    let r = db.query("SELECT COUNT(*) FROM news WHERE brands = 'Acme Corp' AND lang = 'en'", &Params::new()).unwrap();
    assert_eq!(r.rows[0][0].as_i64(), Some(2));
    let r = db.query("SELECT phrases FROM news WHERE id = 'a'", &Params::new()).unwrap();
    assert!(matches!(&r.rows[0][0], Value::List(v) if v.contains(&Value::Str("linear induction motors".into()))), "{:?}", r.rows);
    // Near duplicates share a band.
    let r = db.query("SELECT dup, COUNT(*) FROM news GROUP BY dup ORDER BY COUNT(*) DESC LIMIT 1", &Params::new()).unwrap();
    assert_eq!(r.rows[0][1].as_i64(), Some(2), "{:?}", r.rows);
    // A brand added later tags the rows written after it.
    db.sql("INSERT INTO brands (name) VALUES ('Initech')", &Params::new()).unwrap();
    db.sql("INSERT INTO news (id, body) VALUES ('d', 'Initech hires.')", &Params::new()).unwrap();
    let r = db.query("SELECT brands FROM news WHERE id = 'd'", &Params::new()).unwrap();
    assert_eq!(r.rows[0][0], Value::List(vec![Value::Str("Initech".into())]));
    // The table needs the derive_keyword feature; a wrong model or type is refused.
    assert!(db.catalog().iter().find(|s| s.table == "news").unwrap().features().any(|f| f == "derive_keyword"));
    for (sql, says) in [
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, body TEXT, l KEYWORD[] OPTIONS(derive = 'body', model = 'builtin:lang'))", "makes a stored KEYWORD field"),
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, body TEXT, l KEYWORD OPTIONS(derive = 'body', model = 'builtin:guess'))", "builtin:lang"),
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, n INT, l KEYWORD OPTIONS(derive = 'n', model = 'builtin:lang'))", "stored TEXT or KEYWORD field"),
        // A dictionary of the table itself, which each write would read
        // again (audit 4, X-C2).
        ("CREATE TABLE t (id KEYWORD PRIMARY KEY, body TEXT, kind KEYWORD, k KEYWORD[] OPTIONS(derive = 'body', model = 'dict:T.kind'))", "itself"),
        ("ALTER TABLE news ADD COLUMN again KEYWORD[] OPTIONS(derive = 'body', model = 'dict:news.lang')", "itself"),
    ] {
        let e = db.sql(sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    }
    // A reopened database opens its built-in models itself.
    drop(db);
    let db = Db::open(&dir.path().join("news.strata"), DbOptions::default()).unwrap();
    db.sql("INSERT INTO news (id, body) VALUES ('e', 'Acme Corp is hiring.')", &Params::new()).unwrap();
    let r = db.query("SELECT lang, brands FROM news WHERE id = 'e'", &Params::new()).unwrap();
    assert_eq!((r.rows[0][0].as_str(), &r.rows[0][1]), (Some("en"), &Value::List(vec![Value::Str("Acme Corp".into())])));
}

/// A shop whose products' names say their class, embedded by feature
/// hashing of their words, which needs no model files.
fn catalog(dir: &tempfile::TempDir) -> Db {
    let seed = Schema::new("seed", vec![Field::new("id", FieldType::Keyword)], Some("id")).unwrap();
    let db = Db::create(&dir.path().join("db.strata"), seed, DbOptions::default()).unwrap();
    let none = Params::new();
    db.sql("CREATE TABLE products (id KEYWORD PRIMARY KEY, name TEXT, label KEYWORD, emb VECTOR(256) OPTIONS (embed = 'name', model = 'hash:256', index = 'flat'))", &none).unwrap();
    assert!(db.open_embedders(dir.path()).is_empty());
    let kinds: [(&str, [&str; 4]); 3] = [
        ("Sofas", ["leather sofa", "velvet couch", "sectional sofa with chaise", "loveseat couch"]),
        ("Rugs", ["wool area rug", "jute runner rug", "shag rug", "outdoor area rug"]),
        ("Lamps", ["brass floor lamp", "table lamp with shade", "desk lamp", "arc floor lamp"]),
    ];
    let mut values = Vec::new();
    for (label, names) in kinds {
        for (i, n) in names.iter().enumerate() {
            for j in 0..6 {
                let extra = ["grey", "blue", "modern", "rustic", "small", "large"][j];
                values.push(format!("('{label}{i}{j}', '{extra} {n}', '{label}')"));
            }
        }
    }
    // Two products whose class is not known yet.
    values.push("('u1', 'blue velvet sofa', NULL)".into());
    values.push("('u2', 'round wool rug', NULL)".into());
    db.sql(&format!("INSERT INTO products (id, name, label) VALUES {}", values.join(", ")), &none).unwrap();
    db
}

#[test]
fn classifiers_learn_a_tables_labels_and_classify_rows_and_queries() {
    let dir = tempfile::tempdir().unwrap();
    let db = catalog(&dir);
    let none = Params::new();
    for method in ["centroid", "logistic"] {
        let name = format!("kind_{method}");
        let r = db.sql(&format!("CREATE CLASSIFIER {name} ON products(emb) LABELS label OPTIONS (method = '{method}', holdout = 0.25)"), &none).unwrap();
        let chloedb::db::SqlResult::Rows(r) = r else { panic!("a report") };
        let col = |c: &str| r.rows[0][r.columns.iter().position(|x| x == c).unwrap()].clone();
        assert_eq!(col("labels"), Value::Int(3), "{method}");
        assert_eq!(col("rows"), Value::Int(72), "the two rows without a label are not read");
        assert!(col("holdout_accuracy").as_f64().unwrap() >= 0.9, "{method}: {:?}", r.rows);

        // A query's text, embedded by the field's model.
        let c = db.classify(&name, "a comfy couch", 3).unwrap();
        assert_eq!(c.label.as_deref(), Some("Sofas"), "{method}: {c:?}");
        assert_eq!((c.top.len(), c.labels_from.as_str(), c.version), (3, "label", 1));

        // Each row's class, a column of the rows a query returns.
        let r = db.query(&format!("SELECT id, {name}(emb) AS kind FROM products WHERE label IS NULL ORDER BY id"), &none).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Str("u1".into()), Value::Str("Sofas".into())], vec![Value::Str("u2".into()), Value::Str("Rugs".into())]], "{method}");
        assert!(r.stats.plan.iter().any(|l| l.starts_with("classify: 2 values of rows")), "{:?}", r.stats.plan);

        // A parameter's class, computed before planning: it filters.
        let v = db.embed_query_in("products", "emb", "lamp for reading").unwrap();
        let p = Params::new().set("v", v);
        let r = db.query(&format!("SELECT COUNT(*) FROM products WHERE label = {name}(:v)"), &p).unwrap();
        assert_eq!(r.rows[0][0], Value::Int(24), "{method}");
        let r = db.query(&format!("SELECT {name}(:v, 'score'), {name}(:v, 'labels')"), &p);
        let r = r.unwrap_or_else(|e| panic!("{method}: {e}"));
        assert!(matches!(r.rows[0][0], Value::Float(s) if s > 0.34), "{:?}", r.rows);
        assert!(matches!(&r.rows[0][1], Value::List(ls) if ls.first() == Some(&Value::Str("Lamps".into()))), "{:?}", r.rows);
    }

    // Rows of another table tagged as they are written, by a classifier's
    // best label (KEYWORD) or its labels (KEYWORD[]).
    db.sql("CREATE TABLE listings (id KEYWORD PRIMARY KEY, title TEXT, kind KEYWORD OPTIONS (derive = 'title', model = 'classifier:kind_centroid'), kinds KEYWORD[] OPTIONS (derive = 'title', model = 'classifier:kind_logistic'), later KEYWORD OPTIONS (derive = 'title', model = 'classifier:not_yet'))", &none).unwrap();
    db.sql("INSERT INTO listings (id, title) VALUES ('a', 'old leather couch'), ('b', 'shag rug, barely used'), ('c', '')", &none).unwrap();
    let r = db.query("SELECT id, kind, kinds, later FROM listings ORDER BY id", &none).unwrap();
    assert_eq!(r.rows[0][1], Value::Str("Sofas".into()), "{:?}", r.rows);
    assert_eq!(r.rows[1][1], Value::Str("Rugs".into()), "{:?}", r.rows);
    assert!(matches!(&r.rows[0][2], Value::List(ls) if ls.first() == Some(&Value::Str("Sofas".into()))), "{:?}", r.rows);
    assert_eq!((&r.rows[2][1], &r.rows[0][3]), (&Value::Null, &Value::Null), "no text, or no classifier yet: no label");
    let r = db.query("SELECT kind, COUNT(*) FROM listings WHERE kind IS NOT NULL GROUP BY kind ORDER BY kind", &none).unwrap();
    assert_eq!(r.rows.len(), 2, "a column to group by: {:?}", r.rows);
    let e = db.sql("CREATE TABLE bad (id KEYWORD PRIMARY KEY, t TEXT, k KEYWORD OPTIONS (derive = 't', model = 'classifier:'))", &none).unwrap_err().to_string();
    assert!(e.contains("classifier:<name>"), "{e}");

    // A version a training; refusals; a drop.
    let e = db.sql("CREATE CLASSIFIER kind_centroid ON products(emb) LABELS label", &none).unwrap_err().to_string();
    assert!(e.contains("CREATE OR REPLACE CLASSIFIER"), "{e}");
    db.sql("CREATE OR REPLACE CLASSIFIER kind_centroid ON products(emb) LABELS label OPTIONS (method = 'centroid') WHERE id <> 'Sofas00'", &none).unwrap();
    assert_eq!(db.classify("kind_centroid", "couch", 1).unwrap().version, 2);
    for (bad, says) in [
        ("CREATE CLASSIFIER k ON products(name) LABELS label", "not a VECTOR field"),
        ("CREATE CLASSIFIER k ON products(emb) LABELS nothing", "no column nothing"),
        ("CREATE CLASSIFIER k ON products(emb) LABELS label WHERE label = 'Rugs'", "tells two or more apart"),
        ("CREATE CLASSIFIER lower ON products(emb) LABELS label", "built-in function"),
    ] {
        let e = db.sql(bad, &none).unwrap_err().to_string();
        assert!(e.contains(says), "{bad}: {e}");
    }
    let e = db.query("SELECT id FROM products WHERE kind_logistic(emb) = 'Rugs'", &none).unwrap_err().to_string();
    assert!(e.contains("keep it in a column"), "{e}");
    db.sql("DROP CLASSIFIER kind_logistic", &none).unwrap();
    assert!(db.classify("kind_logistic", "rug", 1).is_err());
    // Kept across a reopen, as functions are.
    drop(db);
    let db = Db::open(&dir.path().join("db.strata"), DbOptions::default()).unwrap();
    assert!(db.open_embedders(dir.path()).is_empty());
    assert_eq!(db.classify("kind_centroid", "a wool rug", 1).unwrap().label.as_deref(), Some("Rugs"));
}

#[test]
fn chunks_of_splits_a_tables_text_and_inserts_its_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    let long = (0..80).map(|i| format!("Sentence {i} of a long paper about vaccines.")).collect::<Vec<_>>().join(" ");
    db.sql("INSERT INTO papers (id, body, year) VALUES ('long', :b, 2030)", &Params::new().set("b", Value::Str(long.clone()))).unwrap();
    let r = db.query("SELECT id, n, chunk_id, chunk, tokens, start_char, end_char FROM chunks_of(papers, body, max_tokens => 64) WHERE year = 2030", &Params::new()).unwrap();
    assert!(r.rows.len() > 5, "{:?}", r.rows);
    assert_eq!(r.columns, ["id", "n", "chunk_id", "chunk", "tokens", "start_char", "end_char"]);
    for (i, row) in r.rows.iter().enumerate() {
        assert_eq!(row[0], Value::Str("long".into()));
        assert_eq!(row[1], Value::Int(i as i64));
        assert_eq!(row[2], Value::Str(format!("long#{i}")));
        let chunk = row[3].as_str().unwrap();
        assert!(row[4].as_i64().unwrap() <= 64 && chunk.ends_with('.'), "cut after a sentence: {chunk:?}");
        let (s, e) = (row[5].as_i64().unwrap() as usize, row[6].as_i64().unwrap() as usize);
        assert_eq!(&long[s..e], chunk);
    }
    assert!(r.stats.plan.iter().any(|l| l.starts_with("chunks_of: 1 rows of papers.body in")), "{:?}", r.stats.plan);
    // Its columns by name, `*`, overlap, LIMIT and parameters.
    let p = Params::new().set("m", Value::Int(32)).set("y", Value::Int(2030));
    let r = db.query("SELECT chunk AS text, n FROM chunks_of(papers, body, max_tokens => :m, overlap => 8) WHERE year = :y LIMIT 3", &p).unwrap();
    assert_eq!((r.columns.clone(), r.rows.len()), (vec!["text".to_string(), "n".to_string()], 3));
    assert!(r.rows[1][0].as_str().unwrap().starts_with("of a long paper") || r.rows[1][0].as_str().unwrap().contains("vaccines. Sentence"), "{:?}", r.rows);
    assert_eq!(db.query("SELECT * FROM chunks_of(papers, body) LIMIT 4", &Params::new()).unwrap().columns.len(), 7);
    // INSERT ... SELECT writes them to a table of chunks.
    db.sql("CREATE TABLE chunks (id KEYWORD PRIMARY KEY, doc KEYWORD, n INT, body TEXT, tokens INT)", &Params::new()).unwrap();
    let written = db.sql("INSERT INTO chunks (id, doc, n, body, tokens) SELECT chunk_id, id, n, chunk, tokens FROM chunks_of(papers, body, max_tokens => 64)", &Params::new()).unwrap();
    let all = db.query("SELECT count(*) FROM chunks_of(papers, body, max_tokens => 64)", &Params::new());
    assert!(all.is_err(), "count(*) is not a column of chunks_of");
    let n = db.query("SELECT id FROM chunks_of(papers, body, max_tokens => 64)", &Params::new()).unwrap().rows.len();
    assert!(matches!(written, chloedb::db::SqlResult::Written(w) if w as usize == n && n > 61), "{n}");
    let r = db.query("SELECT doc, n FROM chunks WHERE MATCH(body, 'sentence') ORDER BY bm25(body, 'sentence 42') DESC LIMIT 1", &Params::new()).unwrap();
    assert_eq!(r.rows[0][0], Value::Str("long".into()));
    // What it refuses.
    for (sql, says) in [
        ("SELECT id FROM chunks_of(papers, year)", "not a stored TEXT field"),
        ("SELECT id FROM chunks_of(papers, body, max_tokens => 4)", "max_tokens is from 16"),
        ("SELECT id FROM chunks_of(papers, body, max_tokens => 32, overlap => 40)", "overlap"),
        ("SELECT id FROM chunks_of(papers, body) ORDER BY n", "INSERT INTO chunks SELECT"),
        ("SELECT words FROM chunks_of(papers, body)", "gives the columns"),
        ("SELECT id FROM chunks_of(nope, body)", "no table nope"),
        ("SELECT id FROM chunks_of(papers, body, size => 3)", "no option size"),
    ] {
        let e = db.query(sql, &Params::new()).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    }
}

#[test]
fn a_tables_synonym_list_widens_its_searches() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    let none = Params::new();
    let count = |db: &Db, opts: &chloedb::query::exec::ExecOptions, q: &str| -> i64 {
        db.query_with(&format!("SELECT COUNT(*) FROM papers WHERE MATCH(body, '{q}', synonyms => 1)"), &none, opts).unwrap_or_else(|e| panic!("{q}: {e}")).rows[0][0].as_i64().unwrap()
    };
    let e = db.query("SELECT COUNT(*) FROM papers WHERE MATCH(body, 'rodents', synonyms => 1)", &none).unwrap_err().to_string();
    assert!(e.contains("no synonym list"), "{e}");
    db.sql("CREATE SYNONYMS ON papers AS 'rodents => mice\nshots, vaccines'", &none).unwrap();
    let opts = chloedb::query::exec::ExecOptions::default();
    assert_eq!(count(&db, &opts, "rodents"), 1);
    assert_eq!(count(&db, &opts, "shots"), 30);
    assert_eq!(db.query("SELECT COUNT(*) FROM papers WHERE MATCH(body, 'rodents')", &none).unwrap().rows[0][0], Value::Int(0), "only with synonyms => 1");
    // Ranked, as a stored function a query could read, and with the
    // server's list before it.
    let r = db.query("SELECT id FROM papers ORDER BY bm25(body, 'rodents immune', synonyms => 1) DESC LIMIT 1", &none).unwrap();
    assert_eq!(r.rows[0][0], Value::Str("ab".into()));
    assert!(db.functions().unwrap().current("synonyms_papers").is_some());
    let base = std::sync::Arc::new(chloedb::text::Synonyms::parse("molecules, chemistry").unwrap());
    let with_base = chloedb::query::exec::ExecOptions { synonyms: Some(base), ..Default::default() };
    assert_eq!((count(&db, &with_base, "chemistry"), count(&db, &with_base, "rodents")), (30, 1));
    // Its next version, what it refuses, and its drop.
    let e = db.sql("CREATE SYNONYMS ON papers AS 'rodents => mice'", &none).unwrap_err().to_string();
    assert!(e.contains("exists") || e.contains("OR REPLACE"), "{e}");
    db.sql("CREATE OR REPLACE SYNONYMS ON papers AS 'rodents => graph'", &none).unwrap();
    assert_eq!(count(&db, &opts, "rodents"), 30);
    for (sql, says) in [("CREATE SYNONYMS ON nope AS 'a, b'", "nope"), ("CREATE OR REPLACE SYNONYMS ON papers AS '=> x'", "synonyms of papers"), ("CREATE OR REPLACE SYNONYMS ON papers AS '# none'", "no rules")] {
        let e = db.sql(sql, &none).unwrap_err().to_string();
        assert!(e.contains(says), "{sql}: {e}");
    }
    db.sql("DROP SYNONYMS ON papers", &none).unwrap();
    assert!(db.query("SELECT COUNT(*) FROM papers WHERE MATCH(body, 'rodents', synonyms => 1)", &none).is_err());
    db.sql("DROP SYNONYMS IF EXISTS ON papers", &none).unwrap();
    // A reopened database keeps a list.
    db.sql("CREATE OR REPLACE SYNONYMS ON papers AS 'rodents => mice'", &none).unwrap();
    drop(db);
    let db = Db::open(&dir.path().join("db.strata"), DbOptions::default()).unwrap();
    assert_eq!(count(&db, &opts, "rodents"), 1);
}

#[test]
fn synonyms_are_mined_from_a_query_log_and_from_vectors() {
    use chloedb::query::mine::{Options, Request, Source};
    let dir = tempfile::tempdir().unwrap();
    let db = catalog(&dir);
    let none = Params::new();
    db.sql("CREATE TABLE searches (id KEYWORD PRIMARY KEY, context KEYWORD, ts INT, text TEXT)", &none).unwrap();
    let mut values = Vec::new();
    for u in 0..5 {
        values.push(format!("('s{u}a', 'u{u}', {}, 'grey couch')", 1000 * u));
        values.push(format!("('s{u}b', 'u{u}', {}, 'grey sofa')", 1000 * u + 20));
    }
    values.push("('x', 'u9', 0, 'rug')".into());
    db.sql(&format!("INSERT INTO searches (id, context, ts, text) VALUES {}", values.join(", ")), &none).unwrap();
    let m = db.mine_synonyms(&Request { table: "products".into(), field: "name".into(), source: Source::Log { log: "searches".into() }, options: Options::default() }).unwrap();
    assert_eq!((m.rows, m.proposals.len()), (11, 1), "{m:?}");
    assert_eq!((m.proposals[0].rule.as_str(), m.proposals[0].score), ("couch => sofa", 5.0));
    assert!(m.rules.starts_with("# mined from the searches of searches") && m.rules.ends_with("couch => sofa\n"), "{}", m.rules);
    // The rules as the table's list.
    db.sql(&format!("CREATE SYNONYMS ON products AS '{}'", m.rules.replace('\'', "''")), &none).unwrap();
    let r = db.query("SELECT COUNT(*) FROM products WHERE MATCH(name, 'couch', synonyms => 1)", &none).unwrap();
    assert_eq!(r.rows[0][0], Value::Int(6 * 4 + 1), "every couch and sofa");
    // From vectors: the words of the names, each embedded.
    let req = Request { table: "products".into(), field: "name".into(), source: Source::Vectors { vector: "emb".into() }, options: Options { min_cos: -1.0, per_word: 1, ..Options::default() } };
    let m = db.mine_synonyms(&req).unwrap();
    assert_eq!(m.rows, 74);
    // A cosine below the floor is taken as the floor: below it nearly every pair passes.
    assert!(m.rules.contains("cosine 0.3 or more"), "{}", m.rules);
    assert!(m.words > 15 && m.proposals.len() <= m.words / 2 && m.proposals.iter().all(|p| p.score >= 0.3), "{m:?}");
    assert!(m.proposals.iter().all(|p| p.rule.contains(", ")));
    let e = db.mine_synonyms(&Request { source: Source::Vectors { vector: "nope".into() }, ..req.clone() }).unwrap_err().to_string();
    assert!(e.contains("no model"), "{e}");
    let e = db.mine_synonyms(&Request { field: "label".into(), ..req }).unwrap_err().to_string();
    assert!(e.contains("not a TEXT field"), "{e}");
}

#[test]
fn chunks_of_reads_only_the_rows_its_limit_needs() {
    let dir = tempfile::tempdir().unwrap();
    let db = db(&dir);
    // Rows without text give no chunk, so a LIMIT they leave unfilled is
    // read again with more rows.
    for i in 0..40 {
        db.sql("INSERT INTO papers (id, year) VALUES (:id, 2040)", &Params::new().set("id", Value::Str(format!("e{i}")))).unwrap();
    }
    for i in 0..40 {
        db.sql("INSERT INTO papers (id, body, year) VALUES (:id, 'A short text about chunks.', 2040)", &Params::new().set("id", Value::Str(format!("t{i}")))).unwrap();
    }
    let read = |r: &chloedb::query::exec::ResultSet| -> usize {
        let line = r.stats.plan.iter().find(|l| l.starts_with("chunks_of: ")).unwrap_or_else(|| panic!("{:?}", r.stats.plan));
        line["chunks_of: ".len()..].split(' ').next().unwrap().parse().unwrap()
    };
    // Every row has text: the LIMIT's rows alone.
    let r = db.query("SELECT id FROM chunks_of(papers, body) LIMIT 3", &Params::new()).unwrap();
    assert_eq!((r.rows.len(), read(&r)), (3, 3), "{:?}", r.stats.plan);
    // Half of them have none: still 3 chunks, from fewer rows than the table's 80.
    let r = db.query("SELECT id FROM chunks_of(papers, body) WHERE year = 2040 LIMIT 3", &Params::new()).unwrap();
    assert_eq!(r.rows.len(), 3, "{:?}", r.rows);
    assert!(read(&r) < 80 && read(&r) >= 3, "{:?}", r.stats.plan);
    // More than the rows hold: every row is read, once the reads come back short.
    let r = db.query("SELECT id FROM chunks_of(papers, body) WHERE year = 2040 LIMIT 1000", &Params::new()).unwrap();
    assert_eq!((r.rows.len(), read(&r)), (40, 80), "{:?}", r.stats.plan);
}

#[test]
fn expand_reads_a_field_or_table_whose_name_is_a_keyword() {
    // Its first step's query is written as text: a name such as `from`,
    // written bare there, did not read back, and the query panicked.
    for (table, field) in [("t", "from"), ("order", "body"), ("t", "select")] {
        let dir = tempfile::tempdir().unwrap();
        let schema = Schema::new(table, vec![Field::new("id", FieldType::Keyword), Field::new(field, FieldType::Text { positions: false })], Some("id")).unwrap();
        let db = Db::create(&dir.path().join("db.strata"), schema, DbOptions::default()).unwrap();
        db.load((0..10).map(|i| vec![Value::Str(format!("r{i}")), Value::Str(format!("mRNA vaccines raise antibodies, trial {i}"))]).collect::<Vec<_>>()).unwrap();
        let sql = format!("SELECT id FROM \"{table}\" ORDER BY bm25(\"{field}\", 'vaccines', expand => 3) DESC LIMIT 3");
        let r = db.query(&sql, &Params::new()).unwrap_or_else(|e| panic!("{table}.{field}: {e}"));
        assert_eq!(r.rows.len(), 3, "{table}.{field}");
        assert!(r.stats.plan.iter().any(|l| l.contains("expanded from")), "{table}.{field}: {:?}", r.stats.plan);
    }
}
