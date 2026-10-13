//! Shared query workloads: the queries that Strata, SQLite and Tantivy all
//! run (DESIGN.md §8).
//!
//! [`generate`] derives a [`Workload`] for one collection, deterministically
//! from a seed:
//!
//! | kind | query | result |
//! |---|---|---|
//! | `bm25` | BEIR query text, disjunctive BM25 | top-10 ids |
//! | `and2`, `and3`, `or3` | 2–3 content words of a query text | count, first 10 ids in load order |
//! | `filter` | structured predicates of a target selectivity | count, top-10 by `rating DESC, id` |
//! | `filtered_bm25` | `bm25` restricted by a filter | top-10 ids |
//! | `vector`, `filtered_vector` | query vector, exact similarity | top-10 ids |
//! | `hybrid`, `filtered_hybrid` | RRF (k = 60) of the bm25 and vector top-100 | top-10 ids |
//! | `facet` | 1–2 content words, grouped by category | top-5 (category, count) |
//!
//! Every query carries its SQL in Strata's dialect (DESIGN.md §6) plus the
//! structured fields (query index, words, filter predicates) that the
//! baselines translate into their own APIs. Files store query indices, not
//! vectors: the vector of query `i` is `Collection::queries[i].vector`, bound
//! as `:q`.
//!
//! Conventions every system follows so that results are comparable:
//!
//! * BM25 rankings hold only documents containing at least one query term,
//!   so a filtered BM25 query may return fewer than 10 rows.
//! * "Load order" is the order of `Collection::docs` (Strata's docID order,
//!   SQLite's rowid order).
//! * Ties are broken by `id` ascending (byte order) wherever the order is
//!   otherwise ambiguous: filter results by rating, facet counts, RRF scores.
//! * Filter selectivities are exact: the generator computes each filter's
//!   match count from the data ([`WorkloadQuery::matches`]) and keeps only
//!   filters within [`WorkloadConfig::tolerance`] of their target.

pub mod run;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::data::{col, Collection};
use crate::error::{corrupt, Result};
use crate::value::Document;

/// Results per ranked query.
pub const TOP_K: usize = 10;
/// Groups per facet query.
pub const FACET_K: usize = 5;
/// Reciprocal rank fusion constant: a document at rank `r` (from 1) scores
/// `1 / (RRF_K + r)`.
pub const RRF_K: f64 = 60.0;
/// Depth of each ranking fused by RRF.
pub const RRF_DEPTH: usize = 100;

/// The kinds of workload queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bm25,
    And2,
    And3,
    Or3,
    Filter,
    FilteredBm25,
    Vector,
    FilteredVector,
    Hybrid,
    FilteredHybrid,
    Facet,
}

impl Kind {
    pub const ALL: [Kind; 11] = [
        Kind::Bm25,
        Kind::And2,
        Kind::And3,
        Kind::Or3,
        Kind::Filter,
        Kind::FilteredBm25,
        Kind::Vector,
        Kind::FilteredVector,
        Kind::Hybrid,
        Kind::FilteredHybrid,
        Kind::Facet,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Bm25 => "bm25",
            Kind::And2 => "and2",
            Kind::And3 => "and3",
            Kind::Or3 => "or3",
            Kind::Filter => "filter",
            Kind::FilteredBm25 => "filtered_bm25",
            Kind::Vector => "vector",
            Kind::FilteredVector => "filtered_vector",
            Kind::Hybrid => "hybrid",
            Kind::FilteredHybrid => "filtered_hybrid",
            Kind::Facet => "facet",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }

    /// Unfiltered relevance rankings of judged queries, scored by nDCG@10.
    pub fn judged(self) -> bool {
        matches!(self, Kind::Bm25 | Kind::Vector | Kind::Hybrid)
    }

    /// Kinds that use the query vector.
    pub fn uses_vector(self) -> bool {
        matches!(self, Kind::Vector | Kind::FilteredVector | Kind::Hybrid | Kind::FilteredHybrid)
    }
}

/// One structured predicate; a query's filter is the conjunction of its
/// predicates. Ranges are inclusive at both ends, like SQL `BETWEEN`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pred {
    Year(i64, i64),
    Rating(f64, f64),
    /// `category IN (...)`, values sorted.
    Category(Vec<String>),
    /// `topic IN (...)`, values sorted.
    Topic(Vec<String>),
}

impl Pred {
    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Pred::Year(lo, hi) => doc[col::YEAR].as_i64().is_some_and(|y| *lo <= y && y <= *hi),
            Pred::Rating(lo, hi) => doc[col::RATING].as_f64().is_some_and(|r| *lo <= r && r <= *hi),
            Pred::Category(v) => doc[col::CATEGORY].as_str().is_some_and(|c| v.iter().any(|x| x == c)),
            Pred::Topic(v) => doc[col::TOPIC].as_str().is_some_and(|t| v.iter().any(|x| x == t)),
        }
    }

    /// The predicate in Strata's SQL dialect.
    pub fn sql(&self) -> String {
        match self {
            Pred::Year(lo, hi) => format!("year BETWEEN {lo} AND {hi}"),
            // `{:?}` prints the shortest representation that parses back to
            // the same f64, with a decimal point.
            Pred::Rating(lo, hi) => format!("rating BETWEEN {lo:?} AND {hi:?}"),
            Pred::Category(v) => in_sql("category", v),
            Pred::Topic(v) => in_sql("topic", v),
        }
    }
}

fn in_sql(field: &str, values: &[String]) -> String {
    if values.len() == 1 {
        format!("{field} = {}", quote(&values[0]))
    } else {
        format!("{field} IN ({})", values.iter().map(|v| quote(v)).collect::<Vec<_>>().join(", "))
    }
}

/// A conjunction of predicates in Strata's SQL dialect.
pub fn filter_sql(preds: &[Pred]) -> String {
    preds.iter().map(Pred::sql).collect::<Vec<_>>().join(" AND ")
}

/// Whether `doc` passes every predicate.
pub fn filter_matches(preds: &[Pred], doc: &Document) -> bool {
    preds.iter().all(|p| p.matches(doc))
}

/// An SQL string literal.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// How a filter was built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Year,
    Rating,
    Category,
    YearRating,
    YearCategory,
    RatingCategory,
    /// Topics closest to the query vector (positively correlated with it),
    /// narrowed by a rating range when one topic is too large.
    TopicNear,
    /// Topics farthest from the query vector (negatively correlated).
    TopicFar,
}

impl Shape {
    /// Shapes independent of the query (the synthetic fields are independent
    /// of content).
    pub const STRUCTURED: [Shape; 6] =
        [Shape::Year, Shape::Rating, Shape::Category, Shape::YearRating, Shape::YearCategory, Shape::RatingCategory];

    pub fn name(self) -> &'static str {
        match self {
            Shape::Year => "year",
            Shape::Rating => "rating",
            Shape::Category => "category",
            Shape::YearRating => "year_rating",
            Shape::YearCategory => "year_category",
            Shape::RatingCategory => "rating_category",
            Shape::TopicNear => "topic_near",
            Shape::TopicFar => "topic_far",
        }
    }
}

/// One query of a workload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkloadQuery {
    pub kind: Kind,
    /// Index into `Collection::queries`: the text (bm25, hybrid) and/or the
    /// vector (vector, hybrid) used, or the source of the words. `None` for
    /// `filter` queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<usize>,
    /// Words of a boolean query (`and2`, `and3`: all; `or3`: any) or of a
    /// facet query (all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<String>,
    /// Structured filter, a conjunction (sorted: year, rating, category,
    /// topic).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Pred>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<Shape>,
    /// Target selectivity of the filter (fraction of documents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<f64>,
    /// Exact number of documents passing the filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<u64>,
    /// The query in Strata's SQL dialect.
    pub sql: String,
    /// The matching count, for kinds that report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_sql: Option<String>,
}

impl WorkloadQuery {
    /// Text of the BEIR query this query uses.
    pub fn text<'a>(&self, coll: &'a Collection) -> &'a str {
        self.query.map_or("", |q| coll.queries[q].text.as_str())
    }

    /// Achieved filter selectivity.
    pub fn selectivity(&self, n_docs: usize) -> Option<f64> {
        self.matches.map(|m| m as f64 / n_docs.max(1) as f64)
    }
}

/// Generator parameters (recorded in the workload file).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkloadConfig {
    pub seed: u64,
    /// Queries used by `bm25`, `vector` and `hybrid` (all of them when the
    /// collection has fewer; else a uniform sample).
    pub max_text_queries: usize,
    /// Queries per boolean kind and for `facet`.
    pub max_boolean: usize,
    /// `filter` queries per (shape, target).
    pub filter_instances: usize,
    /// Filtered queries per target selectivity (and per filter group:
    /// structured, topic near, topic far).
    pub per_target: usize,
    /// Target filter selectivities.
    pub targets: Vec<f64>,
    /// Largest accepted relative deviation of a filter's selectivity from its
    /// target.
    pub tolerance: f64,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        WorkloadConfig {
            seed: 20261012,
            max_text_queries: 2000,
            max_boolean: 300,
            filter_instances: 5,
            per_target: 30,
            targets: vec![0.001, 0.01, 0.1, 0.5],
            tolerance: 0.25,
        }
    }
}

/// The queries of one collection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workload {
    pub collection: String,
    pub n_docs: usize,
    pub config: WorkloadConfig,
    pub queries: Vec<WorkloadQuery>,
}

impl Workload {
    /// Indices into `queries` of one kind.
    pub fn of_kind(&self, kind: Kind) -> Vec<usize> {
        (0..self.queries.len()).filter(|&i| self.queries[i].kind == kind).collect()
    }

    /// File name for a collection, with the document limit if any.
    pub fn file_name(collection: &str, limit: Option<usize>) -> String {
        match limit {
            Some(l) => format!("{collection}_{l}.json"),
            None => format!("{collection}.json"),
        }
    }

    /// Writes JSON with one query per line.
    pub fn save(&self, path: &Path) -> Result<()> {
        let head = serde_json::json!({
            "collection": self.collection,
            "n_docs": self.n_docs,
            "config": self.config,
        });
        let head = serde_json::to_string(&head).map_err(corrupt)?;
        let mut out = String::with_capacity(self.queries.len() * 256);
        out.push_str(&head[..head.len() - 1]);
        out.push_str(",\"queries\":[\n");
        for (i, q) in self.queries.iter().enumerate() {
            out.push_str(&serde_json::to_string(q).map_err(corrupt)?);
            out.push_str(if i + 1 < self.queries.len() { ",\n" } else { "\n" });
        }
        out.push_str("]}\n");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        File::create(path)?.write_all(out.as_bytes())?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Workload> {
        serde_json::from_reader(BufReader::new(File::open(path)?)).map_err(corrupt)
    }
}

/// English stopwords (NLTK's list without the apostrophe forms, which the
/// tokenizer splits), sorted; the list Strata's ranked queries drop too.
pub use crate::text::analyzer::QUERY_STOPWORDS as STOPWORDS;

pub fn is_stopword(word: &str) -> bool {
    STOPWORDS.binary_search(&word).is_ok()
}

/// The simple tokenizer: lowercased maximal runs of alphanumeric characters.
pub fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase)
}

/// The terms the baselines search for a BM25 query: the tokens of `text`
/// that are not stopwords, deduplicated, in order of first occurrence.
/// (Strata analyses the text itself; its stopword list may differ.)
pub fn query_terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokens(text).filter(|t| !is_stopword(t) && seen.insert(t.clone())).collect()
}

/// A candidate word for boolean and facet queries: ASCII letters only (so
/// that diacritic folding cannot make systems disagree), at least 3 long,
/// not a stopword.
fn is_content_word(w: &str) -> bool {
    w.len() >= 3 && w.bytes().all(|b| b.is_ascii_lowercase()) && !is_stopword(w)
}

/// Content words of each query text that occur in at least one document
/// body, deduplicated, in query order.
fn content_words(coll: &Collection, queries: &[usize]) -> Vec<Vec<String>> {
    let per_query: Vec<Vec<String>> = queries
        .iter()
        .map(|&q| {
            let mut seen = HashSet::new();
            tokens(&coll.queries[q].text).filter(|t| is_content_word(t) && seen.insert(t.clone())).collect()
        })
        .collect();
    let candidates: HashSet<&str> = per_query.iter().flatten().map(String::as_str).collect();
    // Document frequencies of the candidates only, so the scan allocates
    // nothing per token.
    let mut df: HashMap<&str, u32> = HashMap::new();
    let mut in_doc: Vec<&str> = Vec::new();
    let mut buf = String::new();
    for doc in &coll.docs {
        in_doc.clear();
        for t in doc[col::BODY].as_str().unwrap_or("").split(|c: char| !c.is_alphanumeric()) {
            if t.len() < 3 || !t.bytes().all(|b| b.is_ascii_alphabetic()) {
                continue;
            }
            buf.clear();
            buf.push_str(t);
            buf.make_ascii_lowercase();
            if let Some(&w) = candidates.get(buf.as_str()) {
                in_doc.push(w);
            }
        }
        in_doc.sort_unstable();
        in_doc.dedup();
        for &w in &in_doc {
            *df.entry(w).or_default() += 1;
        }
    }
    per_query.iter().map(|ws| ws.iter().filter(|w| df.get(w.as_str()).is_some_and(|&d| d >= 1)).cloned().collect()).collect()
}

/// Reciprocal rank fusion of rankings (best first): `Σ 1/(RRF_K + rank)`,
/// ties by id; the top `k`.
pub fn rrf(rankings: &[&[String]], k: usize) -> Vec<String> {
    let mut score: HashMap<&str, f64> = HashMap::new();
    for ranking in rankings {
        for (r, id) in ranking.iter().enumerate() {
            *score.entry(id.as_str()).or_default() += 1.0 / (RRF_K + (r + 1) as f64);
        }
    }
    let mut fused: Vec<(&str, f64)> = score.into_iter().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    fused.into_iter().take(k).map(|(id, _)| id.to_string()).collect()
}

// Structured dimensions, in the order predicates are listed in a filter.
const YEAR: usize = 0;
const RATING: usize = 1;
const CATEGORY: usize = 2;
const TOPIC: usize = 3;

/// Random restarts per filter before a (shape, target) is given up.
const ATTEMPTS: usize = 16;

/// The structured fields as dense codes over their sorted distinct values,
/// so that the count of any filter is exact and cheap.
struct Columns {
    n: usize,
    years: Vec<i64>,
    ratings: Vec<f64>,
    categories: Vec<String>,
    topics: Vec<String>,
    /// Per dimension, each document's code.
    codes: [Vec<u32>; 4],
    /// Per dimension, documents per code.
    hist: [Vec<u64>; 4],
}

/// A predicate on one dimension: a set of codes, contiguous for the ordered
/// dimensions (year, rating).
struct DimPred {
    dim: usize,
    codes: Vec<u32>,
}

impl Columns {
    fn new(coll: &Collection) -> Self {
        let docs = &coll.docs;
        let mut years: Vec<i64> = docs.iter().map(|d| d[col::YEAR].as_i64().unwrap_or(0)).collect();
        years.sort_unstable();
        years.dedup();
        let mut ratings: Vec<f64> = docs.iter().map(|d| d[col::RATING].as_f64().unwrap_or(0.0)).collect();
        ratings.sort_by(f64::total_cmp);
        ratings.dedup();
        let keywords = |c: usize| {
            let mut v: Vec<String> = docs.iter().map(|d| d[c].as_str().unwrap_or("").to_string()).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let categories = keywords(col::CATEGORY);
        let topics = keywords(col::TOPIC);
        let code_of = |v: &[String], c: usize| -> Vec<u32> {
            docs.iter().map(|d| v.binary_search_by(|x| x.as_str().cmp(d[c].as_str().unwrap_or(""))).unwrap() as u32).collect()
        };
        let codes = [
            docs.iter().map(|d| years.binary_search(&d[col::YEAR].as_i64().unwrap_or(0)).unwrap() as u32).collect(),
            docs.iter()
                .map(|d| ratings.binary_search_by(|x| x.total_cmp(&d[col::RATING].as_f64().unwrap_or(0.0))).unwrap() as u32)
                .collect(),
            code_of(&categories, col::CATEGORY),
            code_of(&topics, col::TOPIC),
        ];
        let sizes = [years.len(), ratings.len(), categories.len(), topics.len()];
        let hist = std::array::from_fn(|dim| {
            let mut h = vec![0u64; sizes[dim]];
            for &c in &codes[dim] {
                h[c as usize] += 1;
            }
            h
        });
        Columns { n: docs.len(), years, ratings, categories, topics, codes, hist }
    }

    fn ordered(dim: usize) -> bool {
        dim == YEAR || dim == RATING
    }

    /// Documents passing `p`, per code of dimension `dim`.
    fn joint(&self, p: &DimPred, dim: usize) -> Vec<u64> {
        let mut mask = vec![false; self.hist[p.dim].len()];
        for &c in &p.codes {
            mask[c as usize] = true;
        }
        let mut g = vec![0u64; self.hist[dim].len()];
        for (&a, &b) in self.codes[p.dim].iter().zip(&self.codes[dim]) {
            if mask[a as usize] {
                g[b as usize] += 1;
            }
        }
        g
    }

    /// A predicate on `dim` selecting about `target` documents given the
    /// per-code counts `g`: a window at a random start for ordered
    /// dimensions, a subset in random order otherwise.
    fn single(&self, dim: usize, g: &[u64], target: f64, rng: &mut StdRng) -> (DimPred, u64) {
        if Columns::ordered(dim) {
            let (a, b, sum) = window(g, target, rng);
            (DimPred { dim, codes: (a as u32..=b as u32).collect() }, sum)
        } else {
            let mut order: Vec<u32> = (0..g.len() as u32).collect();
            order.shuffle(rng);
            let (codes, sum) = subset(g, target, &order, false);
            (DimPred { dim, codes }, sum)
        }
    }

    /// `first` AND a predicate on `second`, tuned on their joint counts:
    /// `first` takes about the square root of the selectivity and `second`
    /// brings the exact count to the target.
    fn conjunction(&self, first: usize, second: usize, target: f64, rng: &mut StdRng) -> (Vec<DimPred>, u64) {
        let s1 = (target / self.n as f64).sqrt() * self.n as f64;
        let (p1, _) = self.single(first, &self.hist[first], s1, rng);
        let g = self.joint(&p1, second);
        let (p2, count) = self.single(second, &g, target, rng);
        (vec![p1, p2], count)
    }

    /// A filter of a query-independent shape with selectivity `sel`, or
    /// `None` if the shape cannot get within tolerance of it.
    fn structured(&self, shape: Shape, sel: f64, tolerance: f64, rng: &mut StdRng) -> Option<(Vec<Pred>, u64)> {
        let target = sel * self.n as f64;
        for _ in 0..ATTEMPTS {
            let (preds, count) = match shape {
                Shape::Year | Shape::Rating | Shape::Category => {
                    let dim = match shape {
                        Shape::Year => YEAR,
                        Shape::Rating => RATING,
                        _ => CATEGORY,
                    };
                    let (p, count) = self.single(dim, &self.hist[dim], target, rng);
                    (vec![p], count)
                }
                Shape::YearRating => self.conjunction(YEAR, RATING, target, rng),
                Shape::YearCategory => self.conjunction(YEAR, CATEGORY, target, rng),
                Shape::RatingCategory => self.conjunction(CATEGORY, RATING, target, rng),
                Shape::TopicNear | Shape::TopicFar => return None,
            };
            if within(count, target, tolerance) {
                return Some((self.preds(preds), count));
            }
        }
        None
    }

    /// A topic filter with selectivity `sel` whose first topic is
    /// `topics[0]`: topics added in the given order while they bring the
    /// count closer to the target, or, when `topics[0]` alone is too large,
    /// `topics[0]` narrowed by a rating range.
    fn topical(&self, topics: &[u32], sel: f64, tolerance: f64, rng: &mut StdRng) -> Option<(Vec<Pred>, u64)> {
        let target = sel * self.n as f64;
        let first = || DimPred { dim: TOPIC, codes: vec![topics[0]] };
        if self.hist[TOPIC][topics[0] as usize] as f64 <= target * (1.0 + tolerance) {
            let (codes, count) = subset(&self.hist[TOPIC], target, topics, true);
            return within(count, target, tolerance).then(|| (self.preds(vec![DimPred { dim: TOPIC, codes }]), count));
        }
        let g = self.joint(&first(), RATING);
        for _ in 0..ATTEMPTS {
            let (p, count) = self.single(RATING, &g, target, rng);
            if within(count, target, tolerance) {
                return Some((self.preds(vec![p, first()]), count));
            }
        }
        None
    }

    /// Converts dimension predicates to [`Pred`]s, in dimension order.
    fn preds(&self, mut ps: Vec<DimPred>) -> Vec<Pred> {
        ps.sort_by_key(|p| p.dim);
        ps.into_iter()
            .map(|p| {
                let (lo, hi) = (p.codes[0] as usize, *p.codes.last().unwrap() as usize);
                let names = |v: &[String]| {
                    let mut s: Vec<String> = p.codes.iter().map(|&c| v[c as usize].clone()).collect();
                    s.sort_unstable();
                    s
                };
                match p.dim {
                    YEAR => Pred::Year(self.years[lo], self.years[hi]),
                    RATING => Pred::Rating(self.ratings[lo], self.ratings[hi]),
                    CATEGORY => Pred::Category(names(&self.categories)),
                    _ => Pred::Topic(names(&self.topics)),
                }
            })
            .collect()
    }

    /// Unit-length mean document vector of each topic.
    fn topic_centroids(&self, coll: &Collection) -> Vec<Vec<f32>> {
        let mut sums = vec![vec![0f64; coll.dim]; self.topics.len()];
        for (doc, &t) in coll.docs.iter().zip(&self.codes[TOPIC]) {
            if let Some(v) = doc[col::EMB].as_vector() {
                for (s, &x) in sums[t as usize].iter_mut().zip(v) {
                    *s += x as f64;
                }
            }
        }
        sums.into_iter()
            .map(|s| {
                let norm = s.iter().map(|x| x * x).sum::<f64>().sqrt().max(1e-12);
                s.iter().map(|x| (x / norm) as f32).collect()
            })
            .collect()
    }
}

/// A positive count within `tolerance` (relative) of `target`.
fn within(count: u64, target: f64, tolerance: f64) -> bool {
    count > 0 && (count as f64 - target).abs() <= tolerance * target
}

fn closer(a: u64, b: u64, target: f64) -> bool {
    (a as f64 - target).abs() < (b as f64 - target).abs()
}

/// The window `[a, b]` of `g` whose sum is closest to `target`, starting at
/// a random position and growing forwards (backwards once it hits the end).
fn window(g: &[u64], target: f64, rng: &mut StdRng) -> (usize, usize, u64) {
    let m = g.len();
    let s = rng.gen_range(0..m);
    let mut best = (s, s, g[s]);
    let mut sum = 0u64;
    for e in s..m {
        sum += g[e];
        if closer(sum, best.2, target) {
            best = (s, e, sum);
        }
        if sum as f64 >= target {
            return best;
        }
    }
    for a in (0..s).rev() {
        sum += g[a];
        if closer(sum, best.2, target) {
            best = (a, m - 1, sum);
        }
        if sum as f64 >= target {
            break;
        }
    }
    best
}

/// Codes taken greedily in `order` while each brings the sum closer to
/// `target` (the first always, if `force_first`); sorted codes and sum.
fn subset(g: &[u64], target: f64, order: &[u32], force_first: bool) -> (Vec<u32>, u64) {
    let mut codes = Vec::new();
    let mut sum = 0u64;
    for (i, &c) in order.iter().enumerate() {
        let n = g[c as usize];
        if n > 0 && ((force_first && i == 0) || closer(sum + n, sum, target)) {
            codes.push(c);
            sum += n;
        }
    }
    codes.sort_unstable();
    (codes, sum)
}

/// An independent random stream per purpose, so that changing one part of
/// the configuration leaves the other parts of the workload unchanged.
fn stream(seed: u64, purpose: u64) -> StdRng {
    StdRng::seed_from_u64(seed.wrapping_add(purpose.wrapping_mul(0x9E37_79B9_7F4A_7C15)))
}

/// Builds a query and its SQL.
fn make(
    coll: &Collection,
    kind: Kind,
    query: Option<usize>,
    words: Vec<String>,
    filter: Option<(Vec<Pred>, u64, Shape, f64)>,
) -> WorkloadQuery {
    let text = query.map_or(String::new(), |q| quote(&coll.queries[q].text));
    let (filter, matches, shape, target) = match filter {
        Some((f, m, s, t)) => (f, Some(m), Some(s), Some(t)),
        None => (Vec::new(), None, None, None),
    };
    let fsql = filter_sql(&filter);
    let wh = if filter.is_empty() { String::new() } else { format!(" WHERE {fsql}") };
    let op = if kind == Kind::Or3 { " OR " } else { " AND " };
    let m = format!("MATCH(body, {})", quote(&words.join(op)));
    let (sql, count_sql) = match kind {
        Kind::Bm25 | Kind::FilteredBm25 => (format!("SELECT id FROM docs{wh} ORDER BY bm25(body, {text}) DESC LIMIT {TOP_K}"), None),
        Kind::And2 | Kind::And3 | Kind::Or3 => {
            (format!("SELECT id FROM docs WHERE {m} LIMIT {TOP_K}"), Some(format!("SELECT COUNT(*) FROM docs WHERE {m}")))
        }
        Kind::Filter => (
            format!("SELECT id FROM docs{wh} ORDER BY rating DESC, id LIMIT {TOP_K}"),
            Some(format!("SELECT COUNT(*) FROM docs{wh}")),
        ),
        Kind::Vector | Kind::FilteredVector => {
            (format!("SELECT id FROM docs{wh} ORDER BY similarity(emb, :q) DESC LIMIT {TOP_K}"), None)
        }
        Kind::Hybrid | Kind::FilteredHybrid => (
            format!(
                "SELECT id FROM docs{wh} ORDER BY rrf(bm25(body, {text}), similarity(emb, :q), depth => {RRF_DEPTH}) DESC LIMIT {TOP_K}"
            ),
            None,
        ),
        Kind::Facet => (
            format!("SELECT category, COUNT(*) FROM docs WHERE {m} GROUP BY category ORDER BY 2 DESC, 1 LIMIT {FACET_K}"),
            None,
        ),
    };
    WorkloadQuery { kind, query, words, filter, shape, target, matches, sql, count_sql }
}

/// Picks `k` of `words` at random, keeping their order in the query.
fn pick(words: &[String], k: usize, rng: &mut StdRng) -> Vec<String> {
    let mut idx = rand::seq::index::sample(rng, words.len(), k).into_vec();
    idx.sort_unstable();
    idx.into_iter().map(|i| words[i].clone()).collect()
}

/// Generates the workload of a collection (see the module docs).
pub fn generate(coll: &Collection, cfg: &WorkloadConfig) -> Workload {
    let cols = Columns::new(coll);
    let nq = coll.queries.len();
    let sample: Vec<usize> = if nq <= cfg.max_text_queries {
        (0..nq).collect()
    } else {
        let mut v = rand::seq::index::sample(&mut stream(cfg.seed, 0), nq, cfg.max_text_queries).into_vec();
        v.sort_unstable();
        v
    };
    let with_terms: Vec<usize> = sample.iter().copied().filter(|&q| !query_terms(&coll.queries[q].text).is_empty()).collect();
    let words = content_words(coll, &sample);
    let mut out = Vec::new();

    for &q in &with_terms {
        out.push(make(coll, Kind::Bm25, Some(q), Vec::new(), None));
    }

    for (p, (kind, k)) in [(Kind::And2, 2), (Kind::And3, 3), (Kind::Or3, 3)].into_iter().enumerate() {
        let mut rng = stream(cfg.seed, 1 + p as u64);
        let mut order: Vec<usize> = (0..sample.len()).collect();
        order.shuffle(&mut rng);
        let mut made = 0;
        for i in order {
            if made == cfg.max_boolean {
                break;
            }
            if words[i].len() >= k {
                let w = pick(&words[i], k, &mut rng);
                out.push(make(coll, kind, Some(sample[i]), w, None));
                made += 1;
            }
        }
    }

    let mut rng = stream(cfg.seed, 4);
    for &t in &cfg.targets {
        for shape in Shape::STRUCTURED {
            for _ in 0..cfg.filter_instances {
                if let Some((f, m)) = cols.structured(shape, t, cfg.tolerance, &mut rng) {
                    out.push(make(coll, Kind::Filter, None, Vec::new(), Some((f, m, shape, t))));
                }
            }
        }
    }

    // A structured filter of a random shape; shapes that cannot reach a
    // target (e.g. a year range at 0.1%) fail and another shape is drawn.
    let random_structured = |t: f64, rng: &mut StdRng| -> Option<(Vec<Pred>, u64, Shape, f64)> {
        for _ in 0..4 * Shape::STRUCTURED.len() {
            let shape = *Shape::STRUCTURED.choose(rng).unwrap();
            if let Some((f, m)) = cols.structured(shape, t, cfg.tolerance, rng) {
                return Some((f, m, shape, t));
            }
        }
        None
    };
    let centroids = cols.topic_centroids(coll);
    let topical = |q: usize, shape: Shape, t: f64, rng: &mut StdRng| -> Option<(Vec<Pred>, u64, Shape, f64)> {
        let v = &coll.queries[q].vector;
        let sims: Vec<f32> = centroids.iter().map(|c| c.iter().zip(v.iter()).map(|(a, b)| a * b).sum()).collect();
        let mut order: Vec<u32> = (0..sims.len() as u32).collect();
        order.sort_by(|&a, &b| sims[b as usize].total_cmp(&sims[a as usize]).then(a.cmp(&b)));
        if shape == Shape::TopicFar {
            order.reverse();
        }
        cols.topical(&order, t, cfg.tolerance, rng).map(|(f, m)| (f, m, shape, t))
    };
    // Each filtered kind cycles through a shuffled order of its queries;
    // a query whose filter cannot be built is skipped.
    let filtered = |kind: Kind, pool: &[usize], groups: &[Option<Shape>], purpose: u64, out: &mut Vec<WorkloadQuery>| {
        let mut rng = stream(cfg.seed, purpose);
        let mut order = pool.to_vec();
        order.shuffle(&mut rng);
        if order.is_empty() {
            return;
        }
        let mut next = 0;
        for &t in &cfg.targets {
            for &group in groups {
                let mut made = 0;
                for _ in 0..4 * cfg.per_target {
                    if made == cfg.per_target {
                        break;
                    }
                    let q = order[next % order.len()];
                    next += 1;
                    let f = match group {
                        None => random_structured(t, &mut rng),
                        Some(shape) => topical(q, shape, t, &mut rng),
                    };
                    if let Some(f) = f {
                        out.push(make(coll, kind, Some(q), Vec::new(), Some(f)));
                        made += 1;
                    }
                }
            }
        }
    };
    let all_groups = [None, Some(Shape::TopicNear), Some(Shape::TopicFar)];
    filtered(Kind::FilteredBm25, &with_terms, &[None], 5, &mut out);

    for &q in &sample {
        out.push(make(coll, Kind::Vector, Some(q), Vec::new(), None));
    }
    filtered(Kind::FilteredVector, &sample, &all_groups, 6, &mut out);

    for &q in &with_terms {
        out.push(make(coll, Kind::Hybrid, Some(q), Vec::new(), None));
    }
    filtered(Kind::FilteredHybrid, &with_terms, &all_groups, 7, &mut out);

    let mut rng = stream(cfg.seed, 8);
    let mut order: Vec<usize> = (0..sample.len()).collect();
    order.shuffle(&mut rng);
    let mut made = 0;
    for i in order {
        if made == cfg.max_boolean {
            break;
        }
        if !words[i].is_empty() {
            let k = if words[i].len() >= 2 && rng.gen_bool(0.5) { 2 } else { 1 };
            let w = pick(&words[i], k, &mut rng);
            out.push(make(coll, Kind::Facet, Some(sample[i]), w, None));
            made += 1;
        }
    }

    Workload { collection: coll.name.clone(), n_docs: coll.docs.len(), config: cfg.clone(), queries: out }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Query;
    use crate::value::Value;
    use std::sync::Arc;

    #[test]
    fn stopwords_sorted_and_terms() {
        assert!(STOPWORDS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(query_terms("What's the origin of COVID-19? origin"), vec!["origin", "covid", "19"]);
        assert!(is_content_word("vaccine") && !is_content_word("the") && !is_content_word("ab") && !is_content_word("x2y"));
    }

    #[test]
    fn rrf_fuses_and_breaks_ties_by_id() {
        let a: Vec<String> = ["x", "y", "z"].map(String::from).to_vec();
        let b: Vec<String> = ["y", "w"].map(String::from).to_vec();
        assert_eq!(rrf(&[&a, &b], 3), vec!["y", "x", "w"]);
    }

    #[test]
    fn window_and_subset_reach_targets() {
        let mut rng = StdRng::seed_from_u64(1);
        let g = vec![10u64; 100];
        for _ in 0..50 {
            let (a, b, sum) = window(&g, 95.0, &mut rng);
            assert_eq!(sum, g[a..=b].iter().sum::<u64>());
            assert!(sum == 90 || sum == 100, "{sum}");
        }
        let g = vec![50u64, 1, 2, 30, 4];
        let (codes, sum) = subset(&g, 7.0, &[0, 1, 2, 3, 4], false);
        assert_eq!((codes, sum), (vec![1, 2, 4], 7));
        let (codes, sum) = subset(&g, 7.0, &[0, 1, 2, 3, 4], true);
        assert_eq!((codes, sum), (vec![0], 50));
    }

    /// A small synthetic collection in the BEIR table layout.
    fn synthetic(n: usize) -> Collection {
        let mut rng = StdRng::seed_from_u64(7);
        let vocab = ["alpha", "beta", "gamma", "delta", "omega", "sigma", "kappa", "theta"];
        let dim = 4;
        let unit = |rng: &mut StdRng| -> Arc<[f32]> {
            let v: Vec<f32> = (0..dim).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / norm).collect::<Vec<f32>>().into()
        };
        let docs = (0..n)
            .map(|i| {
                let body: Vec<&str> = (0..6).map(|_| *vocab.choose(&mut rng).unwrap()).collect();
                let rating = (rng.gen_range(0.0..5.0f64) * 100.0).round() / 100.0;
                vec![
                    Value::Str(format!("d{i}")),
                    Value::Str(String::new()),
                    Value::Str(body.join(" ")),
                    Value::Str(body.join(" ")),
                    Value::Int(rng.gen_range(1990..2025)),
                    Value::Str(format!("c{:02}", (rng.gen_range(0.0..1.0f64).powi(3) * 64.0) as u32)),
                    Value::Float(rating),
                    Value::Str(format!("t{:02}", rng.gen_range(0..32))),
                    Value::Vector(unit(&mut rng)),
                ]
            })
            .collect();
        let queries = (0..40)
            .map(|i| Query { id: format!("q{i}"), text: format!("the {} and {} of {}", vocab[i % 8], vocab[(i + 3) % 8], vocab[(i + 5) % 8]), vector: unit(&mut rng) })
            .collect();
        Collection { name: "synthetic".into(), dim, docs, queries, qrels: HashMap::new() }
    }

    #[test]
    fn synthetic_workload_is_exact_and_deterministic() {
        let coll = synthetic(20_000);
        let cfg = WorkloadConfig { per_target: 5, ..WorkloadConfig::default() };
        let w = generate(&coll, &cfg);
        assert_eq!(w, generate(&coll, &cfg));
        for kind in Kind::ALL {
            assert!(!w.of_kind(kind).is_empty(), "no {kind:?} queries");
        }
        for q in &w.queries {
            if let (Some(t), Some(m)) = (q.target, q.matches) {
                let exact = coll.docs.iter().filter(|d| filter_matches(&q.filter, d)).count() as u64;
                assert_eq!(m, exact, "{}", q.sql);
                let sel = m as f64 / coll.docs.len() as f64;
                assert!((sel - t).abs() <= cfg.tolerance * t + 1e-12, "{sel} vs {t}: {}", q.sql);
            }
        }
        let path = std::env::temp_dir().join(format!("strata-workload-{}.json", std::process::id()));
        w.save(&path).unwrap();
        assert_eq!(Workload::load(&path).unwrap(), w);
        std::fs::remove_file(path).unwrap();
    }
}
