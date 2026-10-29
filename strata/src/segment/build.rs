//! Turning documents into a segment.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::schema::{FieldType, Schema};
use crate::segment::{DocId, FieldIndexBuilder, SegmentFile, SegmentWriter};
use crate::text::TextFieldBuilder;
use crate::value::Document;

/// Build-time knobs, mostly for experiments that compare encodings
/// (e.g. `text.postings = loom | bp128`). Components document their keys.
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    pub params: BTreeMap<String, String>,
}

impl BuildOptions {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(|s| s.as_str())
    }
    pub fn get_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).unwrap_or(default)
    }
    pub fn set(mut self, key: &str, value: impl ToString) -> Self {
        self.params.insert(key.to_string(), value.to_string());
        self
    }
}

/// Seconds spent in each part of segment builds since the last
/// [`take_build_times`], summed over all builds and threads.
static BUILD_TIMES: Mutex<BTreeMap<String, f64>> = Mutex::new(BTreeMap::new());

fn add_build_time(part: &str, since: Instant) {
    let mut t = BUILD_TIMES.lock().unwrap_or_else(|e| e.into_inner());
    *t.entry(part.to_string()).or_insert(0.0) += since.elapsed().as_secs_f64();
}

/// Seconds spent in each part of segment builds since the last call, by
/// part: `<field> (<type>)` for an index (`emb (vector)` includes the SQ8
/// codes and the ANN graph), `doc store`, `keys`. Resets the counters.
pub fn take_build_times() -> BTreeMap<String, f64> {
    std::mem::take(&mut *BUILD_TIMES.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Builds of fewer documents run on the calling thread alone.
const PARALLEL_MIN_DOCS: usize = 1024;
/// A TEXT field is analysed in runs of at least this many documents, one
/// run per thread.
const TEXT_RUN_DOCS: usize = 4096;

/// Threads one segment build uses: `build.threads`, by default the
/// machine's cores, at most 8. Fields build at once, and TEXT fields analyse
/// runs of documents at once; the image is the same bytes whatever the
/// count. A vector field's graph uses `vector.threads` of its own.
pub fn build_threads(opts: &BuildOptions) -> Result<usize> {
    match opts.get("build.threads") {
        Some(s) => s.parse().ok().filter(|&n: &usize| n > 0).ok_or_else(|| Error::Schema(format!("build.threads: bad value {s:?}"))),
        None => {
            // Asked once: on Linux it reads the cgroup's CPU quota.
            static CORES: OnceLock<usize> = OnceLock::new();
            Ok(*CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()).min(8)))
        }
    }
}

/// One piece of a build, run on any thread.
enum Part {
    Field(usize, Box<dyn FieldIndexBuilder>),
    /// Documents `range` of TEXT field `.0`, run `.1` of the field's runs.
    TextRun(usize, usize, Range<usize>, TextFieldBuilder),
    DocStore,
    Keys,
}

fn kind(ty: &FieldType) -> &'static str {
    match ty {
        FieldType::Text { .. } => "text",
        FieldType::Keyword => "keyword",
        FieldType::Int => "int",
        FieldType::Float => "float",
        FieldType::Vector { .. } => "vector",
        FieldType::Sparse => "sparse",
        FieldType::Edges => "edges",
    }
}

/// Builds the segment image for `docs` (local docIDs follow slice order) on
/// [`build_threads`] threads.
pub fn build_segment(schema: &Schema, docs: &[Document], opts: &BuildOptions) -> Result<Vec<u8>> {
    build_segment_from(schema, docs, opts, &[])
}

/// A vector field's HNSW graph to start a build from: `segment`'s graph of
/// the field, whose documents are the build's from docID `base` on, all of
/// them, with the same vectors. A merge starts from its largest input that
/// lost no documents.
#[derive(Clone)]
pub struct GraphStart {
    pub field: usize,
    pub segment: SegmentFile,
    pub base: u32,
}

/// [`build_segment`], with the vector graphs `starts` names started from
/// those segments' graphs (ignored where the field's graph does not fit:
/// another `M`, no graph, other documents).
pub fn build_segment_from(schema: &Schema, docs: &[Document], opts: &BuildOptions, starts: &[GraphStart]) -> Result<Vec<u8>> {
    for d in docs {
        crate::value::validate(schema, d)?;
    }
    let threads = build_threads(opts)?;
    let threads = if docs.len() < PARALLEL_MIN_DOCS { 1 } else { threads };
    // In the order their sections are written.
    let mut parts: Vec<Part> = Vec::new();
    for (i, f) in schema.fields.iter().enumerate() {
        let b: Option<Box<dyn FieldIndexBuilder>> = match &f.ty {
            FieldType::Text { .. } if f.indexed => {
                let runs = (docs.len() / TEXT_RUN_DOCS).clamp(1, threads);
                let per = docs.len().div_ceil(runs).max(1);
                for (r, at) in (0..docs.len().max(1)).step_by(per).enumerate() {
                    parts.push(Part::TextRun(i, r, at..(at + per).min(docs.len()), crate::text::text_builder(f, opts)?));
                }
                None
            }
            FieldType::Text { .. } => None,
            FieldType::Sparse if f.indexed => Some(crate::text::sparse::field_builder(f, opts)?),
            FieldType::Sparse => None,
            FieldType::Keyword | FieldType::Int | FieldType::Float if f.indexed => {
                Some(crate::forward::field_builder(f, opts)?)
            }
            FieldType::Keyword | FieldType::Int | FieldType::Float => None,
            // Vector fields always store their vectors; `indexed` adds the ANN index.
            FieldType::Vector { .. } => Some(match starts.iter().find(|g| g.field == i) {
                Some(g) => crate::vector::field_builder_from(f, opts, g.segment.clone(), g.base)?,
                None => crate::vector::field_builder(f, opts)?,
            }),
            FieldType::Edges if f.indexed => {
                let keys = schema.primary_key.map_or_else(Vec::new, |pk| docs.iter().map(|d| d[pk].clone()).collect());
                Some(crate::graph::field_builder(f, schema, keys)?)
            }
            FieldType::Edges => None,
        };
        if let Some(b) = b {
            parts.push(Part::Field(i, b));
        }
    }
    parts.push(Part::DocStore);
    parts.push(Part::Keys);
    // A TEXT field's runs, kept until the last one is analysed; that
    // run's thread writes the field.
    let mut text_runs: Vec<Mutex<Vec<Option<TextFieldBuilder>>>> = (0..schema.fields.len()).map(|_| Mutex::new(Vec::new())).collect();
    for p in &parts {
        if let Part::TextRun(i, ..) = p {
            text_runs[*i].get_mut().unwrap_or_else(|e| e.into_inner()).push(None);
        }
    }
    // A part's sections; none for the TEXT runs whose thread did not write
    // their field.
    let run = |part: Part| -> Result<Option<SegmentWriter>> {
        let t = Instant::now();
        let mut out = SegmentWriter::new(docs.len());
        let name = match part {
            Part::Field(i, mut b) => {
                for (d, doc) in docs.iter().enumerate() {
                    b.add(d as DocId, &doc[i])?;
                }
                b.finish(&mut out)?;
                format!("{} ({})", schema.fields[i].name, kind(&schema.fields[i].ty))
            }
            Part::TextRun(i, r, range, mut b) => {
                for (d, doc) in docs[range].iter().enumerate() {
                    b.add(d as DocId, &doc[i])?;
                }
                b.seal();
                add_build_time(&format!("{} (text)", schema.fields[i].name), t);
                let mut runs = text_runs[i].lock().unwrap_or_else(|e| e.into_inner());
                runs[r] = Some(b);
                if runs.iter().any(Option::is_none) {
                    return Ok(None);
                }
                let runs: Vec<TextFieldBuilder> = runs.drain(..).flatten().collect();
                let t = Instant::now();
                TextFieldBuilder::write_joined(runs, threads, &mut out)?;
                add_build_time(&format!("{} (text)", schema.fields[i].name), t);
                return Ok(Some(out));
            }
            Part::DocStore => {
                crate::forward::build_doc_store(schema, docs, opts, &mut out)?;
                "doc store".to_string()
            }
            Part::Keys => {
                crate::storage::build_pk_index(schema, docs, &mut out)?;
                "keys".to_string()
            }
        };
        add_build_time(&name, t);
        Ok(Some(out))
    };
    // Longest first, so that no thread starts a vector graph last.
    let cost = |p: &Part| match p {
        Part::Field(i, _) if matches!(schema.fields[*i].ty, FieldType::Vector { .. }) => 0,
        Part::TextRun(..) => 1,
        Part::DocStore => 2,
        _ => 3,
    };
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&j| cost(&parts[j]));
    let n_parts = parts.len();
    let parts: Vec<Mutex<Option<Part>>> = parts.into_iter().map(|p| Mutex::new(Some(p))).collect();
    let built: Vec<Mutex<Option<Result<Option<SegmentWriter>>>>> = (0..n_parts).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let work = || loop {
        let Some(&j) = order.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
        let part = parts[j].lock().unwrap_or_else(|e| e.into_inner()).take().expect("each part runs once");
        let r = run(part);
        *built[j].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    };
    if threads > 1 {
        std::thread::scope(|s| {
            for _ in 0..threads.min(n_parts) {
                s.spawn(work);
            }
        });
    } else {
        work();
    }
    // A TEXT field's runs are adjacent parts, so its sections, written by
    // one of them, land where the field's go.
    let mut out = SegmentWriter::new(docs.len());
    for b in built {
        if let Some(s) = b.into_inner().unwrap_or_else(|e| e.into_inner()).expect("every part ran")? {
            out.append(s);
        }
    }
    Ok(out.finish())
}
