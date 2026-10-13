//! Collections (sets of posting lists) and query workloads.
//!
//! Sources:
//! * [`synthetic`] — Zipf-distributed list lengths with either uniformly
//!   random docIDs or a *clustered* topic model that mimics a collection
//!   whose docIDs were reordered by URL/topic (dense regions and runs).
//! * [`load_ds2i`] — the binary `.docs` format used by ds2i / PISA
//!   (sequences of little-endian `u32`: `[len, values...]`, the first
//!   sequence being `[1, num_docs]`).
//! * [`load_text`] — one document per line; tokens are lowercase
//!   alphanumeric runs.
//!
//! Queries are either drawn from a file or generated: `cooccur` queries
//! take terms that co-occur in a randomly sampled document (so conjunctions
//! are non-empty, as for real queries); `random` queries pick terms
//! uniformly among lists that are long enough.

use rand::distributions::{Distribution as _, WeightedIndex};
use rand::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

/// A set of posting lists over docIDs `[0, universe)`.
pub struct Collection {
    pub name: String,
    pub universe: u32,
    pub lists: Vec<Vec<u32>>,
    /// Vocabulary (text collections only), indexed by term id.
    pub vocab: Option<Vec<String>>,
}

impl Collection {
    pub fn num_postings(&self) -> u64 {
        self.lists.iter().map(|l| l.len() as u64).sum()
    }

    /// Write the collection in ds2i `.docs` format.
    pub fn write_ds2i(&self, path: &Path) -> io::Result<()> {
        let mut w = BufWriter::new(File::create(path)?);
        for v in [1, self.universe] {
            w.write_all(&v.to_le_bytes())?;
        }
        for l in &self.lists {
            w.write_all(&(l.len() as u32).to_le_bytes())?;
            for &d in l {
                w.write_all(&d.to_le_bytes())?;
            }
        }
        w.flush()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Distribution {
    /// DocIDs uniformly at random (an index with random docID order).
    Uniform,
    /// Topic-clustered docIDs with short runs (a reordered index).
    Clustered,
}

#[derive(Clone, Debug)]
pub struct SyntheticConfig {
    pub universe: u32,
    pub terms: usize,
    /// Length of the longest list; list `r` gets `max_len / (r+1)^zipf`.
    pub max_len: usize,
    pub min_len: usize,
    pub zipf: f64,
    pub distribution: Distribution,
    /// Number of topic regions the docID space is split into (clustered).
    pub topics: usize,
    /// Fraction of a list's postings drawn from its preferred topics.
    pub topic_mass: f64,
    /// Mean length of runs of consecutive docIDs (clustered).
    pub run_len: f64,
    pub seed: u64,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            universe: 8_000_000,
            terms: 20_000,
            max_len: 2_000_000,
            min_len: 1,
            zipf: 1.0,
            distribution: Distribution::Clustered,
            topics: 1000,
            topic_mass: 0.85,
            run_len: 4.0,
            seed: 1,
        }
    }
}

/// Exactly `n` distinct uniform docIDs in `[lo, hi)`, appended sorted.
pub fn uniform_sample(rng: &mut StdRng, n: usize, lo: u32, hi: u32) -> Vec<u32> {
    let span = (hi - lo) as usize;
    let n = n.min(span);
    if n * 2 > span {
        // Dense: choose the complement instead.
        let skip = uniform_sample(rng, span - n, lo, hi);
        let mut out = Vec::with_capacity(n);
        let mut s = skip.iter().peekable();
        for d in lo..hi {
            if s.peek() == Some(&&d) {
                s.next();
            } else {
                out.push(d);
            }
        }
        return out;
    }
    let mut v: Vec<u32> = Vec::with_capacity(n);
    while v.len() < n {
        let missing = n - v.len();
        v.extend((0..missing).map(|_| rng.gen_range(lo..hi)));
        v.sort_unstable();
        v.dedup();
    }
    v
}

/// Generate a synthetic collection.
pub fn synthetic(cfg: &SyntheticConfig) -> Collection {
    let mut rng = StdRng::seed_from_u64(cfg.seed);
    let u = cfg.universe;
    let topics = cfg.topics.clamp(1, u as usize);
    let region = (u as usize / topics) as u32;
    // Topic popularity ~ Zipf(0.8), in random docID-space order.
    let weights: Vec<f64> = (0..topics).map(|t| 1.0 / ((t + 1) as f64).powf(0.8)).collect();
    let mut topic_ids: Vec<usize> = (0..topics).collect();
    topic_ids.shuffle(&mut rng);
    let topic_dist = WeightedIndex::new(&weights).unwrap();

    let mut lists = Vec::with_capacity(cfg.terms);
    for r in 0..cfg.terms {
        let n = ((cfg.max_len as f64 / ((r + 1) as f64).powf(cfg.zipf)) as usize)
            .max(cfg.min_len)
            .clamp(1, u as usize);
        let list = match cfg.distribution {
            Distribution::Uniform => uniform_sample(&mut rng, n, 0, u),
            Distribution::Clustered => {
                // Preferred topics: enough regions to hold the topical mass
                // at <= 50% density, plus a little randomness.
                let mass = ((n as f64) * cfg.topic_mass) as usize;
                let need = (mass as f64 / (region as f64 * 0.5)).ceil() as usize;
                let k = (need.max(1) + rng.gen_range(0..=2)).min(topics);
                let mut chosen = Vec::with_capacity(k);
                while chosen.len() < k {
                    let t = topic_ids[topic_dist.sample(&mut rng)];
                    if !chosen.contains(&t) {
                        chosen.push(t);
                    }
                    if chosen.len() < k && chosen.len() * 4 > topics * 3 {
                        // Nearly all topics: fill deterministically.
                        let rest: Vec<usize> =
                            (0..topics).filter(|t| !chosen.contains(t)).take(k - chosen.len()).collect();
                        chosen.extend(rest);
                    }
                }
                let mut docs = Vec::with_capacity(n + 64);
                // Topical postings: runs of consecutive docIDs inside the
                // chosen regions (geometric run lengths, mean `run_len`).
                let p_stop = 1.0 / cfg.run_len.max(1.0);
                while docs.len() < mass {
                    let t = chosen[rng.gen_range(0..k)] as u32;
                    let start = t * region + rng.gen_range(0..region);
                    let end = if t as usize == topics - 1 { u } else { (t + 1) * region };
                    let mut d = start;
                    loop {
                        docs.push(d);
                        d += 1;
                        if d >= end || docs.len() >= mass || rng.gen_bool(p_stop) {
                            break;
                        }
                    }
                }
                docs.extend(uniform_sample(&mut rng, n - mass.min(n), 0, u));
                docs.sort_unstable();
                docs.dedup();
                // Top up duplicates with uniform docIDs until exactly n.
                while docs.len() < n {
                    let missing = n - docs.len();
                    docs.extend((0..missing).map(|_| rng.gen_range(0..u)));
                    docs.sort_unstable();
                    docs.dedup();
                }
                docs
            }
        };
        lists.push(list);
    }
    let name = format!(
        "synthetic-{:?} (u={}, terms={}, max_len={}, zipf={})",
        cfg.distribution, cfg.universe, cfg.terms, cfg.max_len, cfg.zipf
    )
    .to_lowercase();
    Collection { name, universe: u, lists, vocab: None }
}

fn read_u32(r: &mut impl Read) -> io::Result<Option<u32>> {
    let mut b = [0u8; 4];
    match r.read_exact(&mut b) {
        Ok(()) => Ok(Some(u32::from_le_bytes(b))),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Load a ds2i/PISA `.docs` file. Lists shorter than `min_len` are kept
/// empty (so term ids stay aligned with query files); at most `max_lists`
/// lists are read.
pub fn load_ds2i(path: &Path, min_len: usize, max_lists: Option<usize>) -> io::Result<Collection> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    if read_u32(&mut r)? != Some(1) {
        return Err(bad("expected a [1, num_docs] header sequence"));
    }
    let universe = read_u32(&mut r)?.ok_or_else(|| bad("truncated header"))?;
    let mut lists = Vec::new();
    while max_lists.is_none_or(|m| lists.len() < m) {
        let Some(n) = read_u32(&mut r)? else { break };
        let mut buf = vec![0u8; 4 * n as usize];
        r.read_exact(&mut buf)?;
        if (n as usize) < min_len {
            lists.push(Vec::new());
            continue;
        }
        let l: Vec<u32> = buf.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        if l.windows(2).any(|w| w[0] >= w[1]) || l.last().is_some_and(|&d| d >= universe) {
            return Err(bad("posting list not strictly increasing or out of range"));
        }
        lists.push(l);
    }
    Ok(Collection {
        name: path.file_name().map_or_else(|| path.display().to_string(), |f| f.to_string_lossy().into_owned()),
        universe,
        lists,
        vocab: None,
    })
}

fn tokens(line: &str) -> impl Iterator<Item = String> + '_ {
    line.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
}

/// Build a collection from a text file with one document per line.
pub fn load_text(path: &Path, max_docs: Option<usize>) -> io::Result<Collection> {
    let r = BufReader::new(File::open(path)?);
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut vocab = Vec::new();
    let mut lists: Vec<Vec<u32>> = Vec::new();
    let mut docs = 0u32;
    for line in r.lines() {
        if max_docs.is_some_and(|m| docs as usize >= m) {
            break;
        }
        let line = line?;
        for tok in tokens(&line) {
            let id = *ids.entry(tok).or_insert_with_key(|k| {
                vocab.push(k.clone());
                lists.push(Vec::new());
                (vocab.len() - 1) as u32
            });
            let l = &mut lists[id as usize];
            if l.last() != Some(&docs) {
                l.push(docs);
            }
        }
        docs += 1;
    }
    Ok(Collection {
        name: path.file_name().map_or_else(|| path.display().to_string(), |f| f.to_string_lossy().into_owned()),
        universe: docs.max(1),
        lists,
        vocab: Some(vocab),
    })
}

/// Read queries, one per line. Numeric tokens are term ids; for text
/// collections, words are looked up in the vocabulary. Unknown terms and
/// terms with empty lists are dropped; empty queries are skipped.
pub fn load_queries(path: &Path, coll: &Collection) -> io::Result<Vec<Vec<u32>>> {
    let vocab: Option<HashMap<&str, u32>> = coll
        .vocab
        .as_ref()
        .map(|v| v.iter().enumerate().map(|(i, t)| (t.as_str(), i as u32)).collect());
    let mut queries = Vec::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let mut q: Vec<u32> = match &vocab {
            Some(v) => tokens(&line).filter_map(|t| v.get(t.as_str()).copied()).collect(),
            None => line.split_whitespace().filter_map(|t| t.parse().ok()).collect(),
        };
        q.retain(|&t| (t as usize) < coll.lists.len() && !coll.lists[t as usize].is_empty());
        q.sort_unstable();
        q.dedup();
        if !q.is_empty() {
            queries.push(q);
        }
    }
    Ok(queries)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum QueryMode {
    /// Terms co-occurring in a sampled document.
    Cooccur,
    /// Terms drawn uniformly among eligible lists.
    Random,
}

#[derive(Clone, Debug)]
pub struct QueryConfig {
    pub num: usize,
    pub min_terms: usize,
    pub max_terms: usize,
    /// Only lists with at least this many postings are used as query terms.
    pub min_list_len: usize,
    /// Only lists with at most this many postings are used as query terms.
    pub max_list_len: Option<usize>,
    pub mode: QueryMode,
    pub seed: u64,
}

/// Generate a query workload (each query is a sorted list of term ids).
pub fn generate_queries(coll: &Collection, cfg: &QueryConfig) -> Vec<Vec<u32>> {
    let mut rng = StdRng::seed_from_u64(cfg.seed);
    let eligible: Vec<u32> = (0..coll.lists.len() as u32)
        .filter(|&t| {
            let n = coll.lists[t as usize].len();
            n >= cfg.min_list_len.max(1) && cfg.max_list_len.is_none_or(|m| n <= m)
        })
        .collect();
    assert!(
        eligible.len() >= cfg.max_terms,
        "only {} lists have >= {} postings; lower --min-list-len",
        eligible.len(),
        cfg.min_list_len
    );
    let pick_k = |rng: &mut StdRng| rng.gen_range(cfg.min_terms..=cfg.max_terms);
    match cfg.mode {
        QueryMode::Random => (0..cfg.num)
            .map(|_| {
                let k = pick_k(&mut rng);
                let mut q: Vec<u32> = eligible.choose_multiple(&mut rng, k).copied().collect();
                q.sort_unstable();
                q
            })
            .collect(),
        QueryMode::Cooccur => {
            // Sample (seed term, document) pairs, then find every eligible
            // term containing each sampled document in one pass.
            let samples: Vec<(u32, u32)> = (0..cfg.num * 2)
                .map(|_| {
                    let t = *eligible.choose(&mut rng).unwrap();
                    let l = &coll.lists[t as usize];
                    (t, l[rng.gen_range(0..l.len())])
                })
                .collect();
            let mut docs: Vec<u32> = samples.iter().map(|&(_, d)| d).collect();
            docs.sort_unstable();
            docs.dedup();
            let mut doc_terms: HashMap<u32, Vec<u32>> = HashMap::new();
            for &t in &eligible {
                let l = &coll.lists[t as usize];
                let mut j = 0;
                for &d in &docs {
                    j = crate::codecs::uncompressed::gallop_geq(l, j, d);
                    if j == l.len() {
                        break;
                    }
                    if l[j] == d {
                        doc_terms.entry(d).or_default().push(t);
                    }
                }
            }
            let mut queries = Vec::with_capacity(cfg.num);
            for (t, d) in samples {
                if queries.len() == cfg.num {
                    break;
                }
                let terms = &doc_terms[&d];
                let k = pick_k(&mut rng);
                if terms.len() < k {
                    continue;
                }
                let mut q: Vec<u32> = vec![t];
                q.extend(terms.iter().filter(|&&x| x != t).copied().collect::<Vec<_>>().choose_multiple(&mut rng, k - 1));
                q.sort_unstable();
                queries.push(q);
            }
            // Top up with random queries if documents had too few terms.
            while queries.len() < cfg.num {
                let k = pick_k(&mut rng);
                let mut q: Vec<u32> = eligible.choose_multiple(&mut rng, k).copied().collect();
                q.sort_unstable();
                queries.push(q);
            }
            queries
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_lists_are_valid() {
        for distribution in [Distribution::Uniform, Distribution::Clustered] {
            let cfg = SyntheticConfig {
                universe: 200_000,
                terms: 300,
                max_len: 100_000,
                distribution,
                topics: 50,
                ..Default::default()
            };
            let c = synthetic(&cfg);
            assert_eq!(c.lists.len(), 300);
            for (r, l) in c.lists.iter().enumerate() {
                let expected = (100_000.0 / (r + 1) as f64) as usize;
                assert_eq!(l.len(), expected.max(1), "list {r}");
                assert!(l.windows(2).all(|w| w[0] < w[1]));
                assert!(*l.last().unwrap() < cfg.universe);
            }
            let q = generate_queries(
                &c,
                &QueryConfig {
                    num: 50,
                    min_terms: 2,
                    max_terms: 4,
                    min_list_len: 100,
                    max_list_len: None,
                    mode: QueryMode::Cooccur,
                    seed: 3,
                },
            );
            assert_eq!(q.len(), 50);
            for q in &q {
                assert!((2..=4).contains(&q.len()));
                assert!(q.windows(2).all(|w| w[0] < w[1]));
            }
        }
    }

    #[test]
    fn ds2i_roundtrip() {
        let cfg = SyntheticConfig { universe: 10_000, terms: 20, max_len: 5000, ..Default::default() };
        let c = synthetic(&cfg);
        let dir = std::env::temp_dir().join(format!("posting_bench_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.docs");
        c.write_ds2i(&p).unwrap();
        let d = load_ds2i(&p, 0, None).unwrap();
        assert_eq!(d.universe, c.universe);
        assert_eq!(d.lists, c.lists);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn text_collection() {
        let dir = std::env::temp_dir().join(format!("posting_bench_text_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("docs.txt");
        std::fs::write(&p, "the cat sat\nThe dog\n\ncat, cat and dog!\n").unwrap();
        let c = load_text(&p, None).unwrap();
        assert_eq!(c.universe, 4);
        let vocab = c.vocab.as_ref().unwrap();
        let id = |w: &str| vocab.iter().position(|t| t == w).unwrap();
        assert_eq!(c.lists[id("the")], vec![0, 1]);
        assert_eq!(c.lists[id("cat")], vec![0, 3]);
        assert_eq!(c.lists[id("dog")], vec![1, 3]);
        let qp = dir.join("q.txt");
        std::fs::write(&qp, "cat dog\nunknown\n").unwrap();
        let q = load_queries(&qp, &c).unwrap();
        assert_eq!(q.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
