//! Tantivy baseline: one index directory, merged into a single segment so
//! that it compares with a fully merged Strata or SQLite index.
//!
//! | field | options |
//! |---|---|
//! | `id` | STRING, STORED, FAST (results and tie-breaks read the fast column) |
//! | `title`, `text` | STORED (LZ4 doc store, 16 KiB blocks) |
//! | `body` | TEXT with `en_stem` (simple tokenizer, lowercase, English Snowball stemmer, no stopwords), frequencies and positions |
//! | `year` | i64 INDEXED, FAST |
//! | `rating` | f64 INDEXED, FAST |
//! | `category`, `topic` | STRING, FAST |
//! | `seq` | u64 FAST: load order, for "first 10 in load order" |
//!
//! Queries:
//!
//! * **BM25** (k1 = 1.2, b = 0.75, fixed in Tantivy) over a `BooleanQuery`
//!   of `Should` term queries, top-10 by `TopDocs` (Block-WAND).
//! * **Boolean**: `Must` (AND) or `Should` (OR) term queries without
//!   frequencies; `Count` plus the 10 smallest `seq`.
//! * **Filters**: `Must` clauses of `RangeQuery` (which scans the fast column
//!   when the field is FAST) and `TermQuery`/`TermSetQuery` (postings). For
//!   filtered BM25 they are wrapped in `ConstScoreQuery(.., 0)` so they do not
//!   add to the score; the text disjunction is a further `Must` clause.
//! * **Facets**: a collector counting the category ordinals of the matches
//!   in the fast column.
//! * Tantivy has no vector search, so vector and hybrid kinds are
//!   unsupported.

use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::time::Instant;

use tantivy::collector::sort_key::{SortByStaticFastValue, SortByString};
use tantivy::collector::{Collector, Count, SegmentCollector, TopDocs};
use tantivy::columnar::StrColumn;
use tantivy::indexer::NoMergePolicy;
use tantivy::query::{BooleanQuery, ConstScoreQuery, Occur, Query, RangeQuery, TermQuery, TermSetQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED, STORED, STRING};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::{DocAddress, DocId, Index, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher, SegmentOrdinal, SegmentReader, TantivyDocument, Term};

use super::{file_size, files_under};
use crate::data::{col, Collection};
use crate::error::{Error, Result};
use crate::workload::run::{BuildReport, QueryOutput, System};
use crate::workload::{self, Kind, Pred, WorkloadQuery, FACET_K, TOP_K};

impl From<tantivy::TantivyError> for Error {
    fn from(e: tantivy::TantivyError) -> Self {
        Error::Query(format!("tantivy: {e}"))
    }
}

/// Indexing parameters.
#[derive(Clone, Debug)]
pub struct TantivyOptions {
    /// Indexing threads. One by default, like SQLite's single-threaded load.
    pub threads: usize,
    /// Total indexing memory budget, bytes.
    pub memory_budget: usize,
}

impl Default for TantivyOptions {
    fn default() -> Self {
        TantivyOptions { threads: 1, memory_budget: 512 << 20 }
    }
}

struct Fields {
    id: Field,
    title: Field,
    text: Field,
    body: Field,
    year: Field,
    rating: Field,
    category: Field,
    topic: Field,
    seq: Field,
}

impl Fields {
    fn of(schema: &Schema) -> Result<Fields> {
        let f = |name: &str| schema.get_field(name).map_err(Error::from);
        Ok(Fields {
            id: f("id")?,
            title: f("title")?,
            text: f("text")?,
            body: f("body")?,
            year: f("year")?,
            rating: f("rating")?,
            category: f("category")?,
            topic: f("topic")?,
            seq: f("seq")?,
        })
    }
}

fn schema() -> Schema {
    let mut b = Schema::builder();
    b.add_text_field("id", STRING | STORED | FAST);
    b.add_text_field("title", STORED);
    b.add_text_field("text", STORED);
    let body = TextOptions::default()
        .set_indexing_options(TextFieldIndexing::default().set_tokenizer("en_stem").set_index_option(IndexRecordOption::WithFreqsAndPositions));
    b.add_text_field("body", body);
    b.add_i64_field("year", INDEXED | FAST);
    b.add_f64_field("rating", INDEXED | FAST);
    b.add_text_field("category", STRING | FAST);
    b.add_text_field("topic", STRING | FAST);
    b.add_u64_field("seq", FAST);
    b.build()
}

/// An open index: what a cold run drops and reopens.
struct Open {
    searcher: Searcher,
    /// Kept so the searcher's segments stay registered with the reader.
    _reader: IndexReader,
    analyzer: TextAnalyzer,
    /// The `id` column of each segment.
    ids: Vec<StrColumn>,
}

impl Open {
    fn new(dir: &Path, f: &Fields) -> Result<Open> {
        let index = Index::open_in_dir(dir)?;
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        let searcher = reader.searcher();
        let analyzer = index.tokenizer_for_field(f.body)?;
        let ids = searcher
            .segment_readers()
            .iter()
            .map(|s| s.fast_fields().str("id")?.ok_or_else(|| tantivy::TantivyError::SchemaError("id is not a fast field".into())))
            .collect::<tantivy::Result<_>>()?;
        Ok(Open { searcher, _reader: reader, analyzer, ids })
    }
}

/// The Tantivy baseline (see the module docs).
pub struct Tantivy {
    dir: PathBuf,
    f: Fields,
    /// `None` only while [`System::evict_caches`] reopens the index.
    open: Option<Open>,
}

impl Tantivy {
    /// Indexes `coll` into a new index in `dir` (replacing any).
    pub fn build(dir: &Path, coll: &Collection, opts: &TantivyOptions) -> Result<(Tantivy, BuildReport)> {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        let start = Instant::now();
        let index = Index::create_in_dir(dir, schema())?;
        let f = Fields::of(&index.schema())?;
        let mut writer: IndexWriter = index.writer_with_num_threads(opts.threads, opts.memory_budget)?;
        // Merge once at the end instead of in the background.
        writer.set_merge_policy(Box::new(NoMergePolicy));
        for (i, d) in coll.docs.iter().enumerate() {
            let mut doc = TantivyDocument::default();
            let s = |c: usize| d[c].as_str().unwrap_or("");
            doc.add_text(f.id, s(col::ID));
            doc.add_text(f.title, s(col::TITLE));
            doc.add_text(f.text, s(col::TEXT));
            doc.add_text(f.body, s(col::BODY));
            doc.add_i64(f.year, d[col::YEAR].as_i64().unwrap_or(0));
            doc.add_f64(f.rating, d[col::RATING].as_f64().unwrap_or(0.0));
            doc.add_text(f.category, s(col::CATEGORY));
            doc.add_text(f.topic, s(col::TOPIC));
            doc.add_u64(f.seq, i as u64);
            writer.add_document(doc)?;
        }
        writer.commit()?;
        let index_secs = start.elapsed().as_secs_f64();
        let lap = Instant::now();
        let segments = index.searchable_segment_ids()?;
        if segments.len() > 1 {
            writer.merge(&segments).wait()?;
        }
        writer.garbage_collect_files().wait()?;
        writer.wait_merging_threads()?;
        let merge_secs = lap.elapsed().as_secs_f64();
        let seconds = start.elapsed().as_secs_f64();
        drop(index);

        let sys = Tantivy { dir: dir.to_path_buf(), open: Some(Open::new(dir, &f)?), f };
        let mut components: Vec<(String, u64)> = Vec::new();
        for p in sys.files() {
            let ext = p.extension().map_or("other".to_string(), |e| e.to_string_lossy().into_owned());
            match components.iter_mut().find(|(k, _)| *k == ext) {
                Some((_, n)) => *n += file_size(&p),
                None => components.push((ext, file_size(&p))),
            }
        }
        components.sort_by_key(|c| std::cmp::Reverse(c.1));
        let phases = vec![
            ("index".to_string(), index_secs),
            (format!("merge_{}_segments", segments.len()), merge_secs),
        ];
        let bytes = components.iter().map(|c| c.1).sum();
        Ok((sys, BuildReport { seconds, phases, bytes, components }))
    }

    /// Opens an existing index.
    pub fn open(dir: &Path) -> Result<Tantivy> {
        let f = Fields::of(&Index::open_in_dir(dir)?.schema())?;
        Ok(Tantivy { dir: dir.to_path_buf(), open: Some(Open::new(dir, &f)?), f })
    }

    /// Tantivy library version.
    pub fn version() -> String {
        format!("Tantivy {}", tantivy::version_string())
    }

    /// Number of segments of the open index.
    pub fn segments(&self) -> usize {
        self.state().searcher.segment_readers().len()
    }

    fn state(&self) -> &Open {
        self.open.as_ref().expect("index is open")
    }

    /// `body` terms of `words` after analysis.
    fn terms(&self, words: &[String]) -> Vec<Term> {
        let mut analyzer = self.state().analyzer.clone();
        let mut out = Vec::new();
        for w in words {
            let mut ts = analyzer.token_stream(w);
            while ts.advance() {
                out.push(Term::from_field_text(self.f.body, &ts.token().text));
            }
        }
        out
    }

    /// The terms of `words` combined with `occur`, with frequencies if
    /// `scored`.
    fn text_query(&self, words: &[String], occur: Occur, scored: bool) -> BooleanQuery {
        let option = if scored { IndexRecordOption::WithFreqs } else { IndexRecordOption::Basic };
        let clauses = self.terms(words).into_iter().map(|t| (occur, Box::new(TermQuery::new(t, option)) as Box<dyn Query>)).collect();
        BooleanQuery::new(clauses)
    }

    fn pred_query(&self, p: &Pred) -> Box<dyn Query> {
        let keywords = |field: Field, values: &[String]| -> Box<dyn Query> {
            let terms: Vec<Term> = values.iter().map(|v| Term::from_field_text(field, v)).collect();
            if terms.len() == 1 {
                Box::new(TermQuery::new(terms[0].clone(), IndexRecordOption::Basic))
            } else {
                Box::new(TermSetQuery::new(terms))
            }
        };
        match p {
            Pred::Year(lo, hi) => Box::new(RangeQuery::new(
                Bound::Included(Term::from_field_i64(self.f.year, *lo)),
                Bound::Included(Term::from_field_i64(self.f.year, *hi)),
            )),
            Pred::Rating(lo, hi) => Box::new(RangeQuery::new(
                Bound::Included(Term::from_field_f64(self.f.rating, *lo)),
                Bound::Included(Term::from_field_f64(self.f.rating, *hi)),
            )),
            Pred::Category(v) => keywords(self.f.category, v),
            Pred::Topic(v) => keywords(self.f.topic, v),
        }
    }

    /// `text` restricted by `filter` without changing its scores.
    fn filtered(&self, text: Box<dyn Query>, filter: &[Pred]) -> Box<dyn Query> {
        if filter.is_empty() {
            return text;
        }
        let mut clauses = vec![(Occur::Must, text)];
        for p in filter {
            clauses.push((Occur::Must, Box::new(ConstScoreQuery::new(self.pred_query(p), 0.0)) as Box<dyn Query>));
        }
        Box::new(BooleanQuery::new(clauses))
    }

    fn ids(&self, docs: impl Iterator<Item = DocAddress>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut s = String::new();
        for a in docs {
            let col = &self.state().ids[a.segment_ord as usize];
            let ord = col.ords().first(a.doc_id).ok_or_else(|| Error::Query("tantivy: document without id".into()))?;
            col.ord_to_str(ord, &mut s)?;
            out.push(s.clone());
        }
        Ok(out)
    }
}

impl System for Tantivy {
    fn name(&self) -> &str {
        "tantivy"
    }

    fn supports(&self, kind: Kind) -> bool {
        !kind.uses_vector()
    }

    fn run(&mut self, q: &WorkloadQuery, coll: &Collection) -> Result<QueryOutput> {
        let searcher = &self.state().searcher;
        let mut out = QueryOutput::default();
        match q.kind {
            Kind::Bm25 | Kind::FilteredBm25 => {
                let terms = workload::query_terms(q.text(coll));
                if !terms.is_empty() {
                    let query = self.filtered(Box::new(self.text_query(&terms, Occur::Should, true)), &q.filter);
                    let top = searcher.search(&query, &TopDocs::with_limit(TOP_K).order_by_score())?;
                    out.ids = self.ids(top.into_iter().map(|(_, a)| a))?;
                }
            }
            Kind::And2 | Kind::And3 | Kind::Or3 => {
                let occur = if q.kind == Kind::Or3 { Occur::Should } else { Occur::Must };
                let query = self.text_query(&q.words, occur, false);
                let first = TopDocs::with_limit(TOP_K).order_by_u64_field("seq", Order::Asc);
                let (count, top) = searcher.search(&query, &(Count, first))?;
                out.count = Some(count as u64);
                out.ids = self.ids(top.into_iter().map(|(_, a)| a))?;
            }
            Kind::Filter => {
                let clauses = q.filter.iter().map(|p| (Occur::Must, self.pred_query(p))).collect();
                let by_rating = TopDocs::with_limit(TOP_K).order_by((
                    (SortByStaticFastValue::<f64>::for_field("rating"), Order::Desc),
                    (SortByString::for_field("id"), Order::Asc),
                ));
                let (count, top) = searcher.search(&BooleanQuery::new(clauses), &(Count, by_rating))?;
                out.count = Some(count as u64);
                out.ids = top.into_iter().map(|((_, id), _)| id.unwrap_or_default()).collect();
            }
            Kind::Facet => {
                let query = self.text_query(&q.words, Occur::Must, false);
                let mut facets = searcher.search(&query, &FacetCounts { field: "category" })?;
                facets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                facets.truncate(FACET_K);
                out.facets = Some(facets);
            }
            kind => return Err(Error::Query(format!("tantivy: {} queries are not supported", kind.name()))),
        }
        Ok(out)
    }

    fn evict_caches(&mut self) -> Result<()> {
        // Dropping the searcher unmaps the files, so eviction can drop them.
        self.open = None;
        crate::iostat::evict_dir(&self.dir)?;
        self.open = Some(Open::new(&self.dir, &self.f)?);
        Ok(())
    }

    fn files(&self) -> Vec<PathBuf> {
        files_under(&self.dir)
    }
}

/// Counts the matches per value of a keyword fast field.
struct FacetCounts {
    field: &'static str,
}

struct FacetSegment {
    column: StrColumn,
    counts: Vec<u64>,
}

impl Collector for FacetCounts {
    type Fruit = Vec<(String, u64)>;
    type Child = FacetSegment;

    fn for_segment(&self, _ord: SegmentOrdinal, segment: &SegmentReader) -> tantivy::Result<FacetSegment> {
        let column = segment
            .fast_fields()
            .str(self.field)?
            .ok_or_else(|| tantivy::TantivyError::SchemaError(format!("{} is not a fast field", self.field)))?;
        Ok(FacetSegment { counts: vec![0; column.num_terms()], column })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Vec<(String, u64)>>) -> tantivy::Result<Vec<(String, u64)>> {
        let mut all: Vec<(String, u64)> = fruits.into_iter().flatten().collect();
        all.sort();
        let mut merged: Vec<(String, u64)> = Vec::new();
        for (v, n) in all {
            match merged.last_mut() {
                Some((last, total)) if *last == v => *total += n,
                _ => merged.push((v, n)),
            }
        }
        Ok(merged)
    }
}

impl SegmentCollector for FacetSegment {
    type Fruit = Vec<(String, u64)>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if let Some(ord) = self.column.ords().first(doc) {
            self.counts[ord as usize] += 1;
        }
    }

    fn harvest(self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        let mut s = String::new();
        for (ord, &n) in self.counts.iter().enumerate() {
            if n > 0 && self.column.ord_to_str(ord as u64, &mut s).unwrap_or(false) {
                out.push((s.clone(), n));
            }
        }
        out
    }
}
