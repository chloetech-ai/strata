//! Graph experiments (DESIGN.md §8, item 9).
//!
//! * `load`: arXiv papers (ogbn-arxiv: text, 512-d vectors, citations) into
//!   a Strata file, holding out a sample of recent papers as queries; build
//!   time and size by component.
//! * `latency`: traversals, paths, PageRank and vector-seeded expansion
//!   through SQL, against an in-memory CSR (the lower bound: no keys, no
//!   segments) and, with `--features baselines`, SQLite recursive CTEs.
//! * `quality`: does the citation graph help find a new paper's references
//!   from its text? Vector, BM25 and hybrid search against the same with
//!   graph expansion and personalized PageRank from vector seeds.
//! * `synthetic`: the same latency study on a power-law graph of any size.
//!
//! Results go to `results/graph/`.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::Deserialize;
use serde_json::{json, Value as Json};
use strata::db::Db;
use strata::graph::traverse::{self, Adjacency, Node};
use strata::metrics::{mean, percentile};
use strata::query::Params;
use strata::segment::build::BuildOptions;
use strata::storage::DbOptions;
use strata::{Field, FieldType, Schema, Value, VectorIndexKind};

#[derive(Parser)]
#[command(about = "Graph index and graph query experiments")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Load {
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.jsonl")]
        data: PathBuf,
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.f32")]
        vectors: PathBuf,
        #[arg(long, default_value_t = 512)]
        dim: usize,
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.strata")]
        db: PathBuf,
        /// flat | hnsw
        #[arg(long, default_value = "hnsw")]
        index: String,
        /// Recent papers (OGB's test split) held out as queries.
        #[arg(long, default_value_t = 1000)]
        holdout: usize,
        /// Held-out papers cite at least this many papers.
        #[arg(long, default_value_t = 5)]
        min_refs: usize,
        #[arg(long, default_value_t = 7)]
        seed: u64,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        #[arg(long, default_value = "results/graph")]
        out: PathBuf,
    },
    Latency {
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.strata")]
        db: PathBuf,
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.jsonl")]
        data: PathBuf,
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.f32")]
        vectors: PathBuf,
        #[arg(long, default_value_t = 512)]
        dim: usize,
        #[arg(long, default_value_t = 200)]
        queries: usize,
        #[arg(long, default_value_t = 3)]
        seed: u64,
        /// SQLite baseline database (built if missing; needs `--features baselines`).
        #[arg(long)]
        sqlite: Option<PathBuf>,
        #[arg(long, default_value = "results/graph")]
        out: PathBuf,
    },
    Quality {
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.strata")]
        db: PathBuf,
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.jsonl")]
        data: PathBuf,
        #[arg(long, default_value = "/home/user/data/graph/arxiv/arxiv.f32")]
        vectors: PathBuf,
        #[arg(long, default_value_t = 512)]
        dim: usize,
        /// Results per query.
        #[arg(long, default_value_t = 20)]
        k: usize,
        /// Seeds taken from the vector search, comma-separated values to sweep.
        #[arg(long, value_delimiter = ',', default_value = "10")]
        seeds: Vec<usize>,
        /// Only the first N held-out papers.
        #[arg(long)]
        limit: Option<usize>,
        /// Only these methods (comma-separated names).
        #[arg(long, value_delimiter = ',')]
        methods: Vec<String>,
        #[arg(long, default_value = "results/graph")]
        out: PathBuf,
    },
    /// Runs one query many times with random integer `:s` (for profiling).
    Repeat {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        sql: String,
        /// `:s` is drawn from 0..max.
        #[arg(long)]
        max: i64,
        #[arg(long, default_value_t = 200)]
        n: usize,
    },
    Synthetic {
        #[arg(long, default_value_t = 1_000_000)]
        nodes: usize,
        /// Out-edges per node.
        #[arg(long, default_value_t = 10)]
        degree: usize,
        #[arg(long, default_value_t = 11)]
        seed: u64,
        #[arg(long, default_value = "/home/user/data/graph/synthetic.strata")]
        db: PathBuf,
        #[arg(long, default_value_t = 200)]
        queries: usize,
        #[arg(long)]
        sqlite: Option<PathBuf>,
        #[arg(long, default_value = "results/graph")]
        out: PathBuf,
    },
}

#[derive(Deserialize)]
struct Paper {
    id: i64,
    year: i64,
    category: String,
    title: String,
    #[serde(rename = "abstract")]
    abstract_: String,
    cites: Vec<i64>,
    split: String,
}

fn read_papers(path: &Path) -> Vec<Paper> {
    let f = BufReader::new(std::fs::File::open(path).expect("open data"));
    f.lines().map(|l| serde_json::from_str(&l.unwrap()).expect("paper")).collect()
}

fn read_vectors(path: &Path, dim: usize) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("read vectors");
    let v: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    assert_eq!(v.len() % dim, 0, "vector file is not n × {dim}");
    v
}

fn arxiv_schema(dim: usize, index: &str) -> Schema {
    let index = match index {
        "flat" => VectorIndexKind::Flat,
        _ => VectorIndexKind::Hnsw { m: 16, ef_construction: 100 },
    };
    Schema::new(
        "papers",
        vec![
            Field::new("id", FieldType::Int),
            Field::new("title", FieldType::Text { positions: false }).stored_only(),
            Field::new("abstract", FieldType::Text { positions: false }).stored_only(),
            Field::new("body", FieldType::Text { positions: false }).generated(&["title", "abstract"]).not_stored(),
            Field::new("year", FieldType::Int),
            Field::new("category", FieldType::Keyword),
            Field::new("emb", FieldType::Vector { dim, index }),
            Field::new("cites", FieldType::Edges),
        ],
        Some("id"),
    )
    .unwrap()
}

fn cites_json(ids: &[i64]) -> Value {
    Value::Str(serde_json::to_string(ids).unwrap())
}

fn holdout_path(db: &Path) -> PathBuf {
    db.with_extension("holdout.json")
}

/// File and section sizes by component.
fn sizes(db: &Db) -> Json {
    let snap = db.snapshot();
    let mut by: HashMap<String, usize> = HashMap::new();
    for v in snap.segments() {
        for (name, len) in v.file.section_sizes() {
            let comp = match name.as_str() {
                n if n.starts_with("graph.") => "graph",
                n if n.starts_with("vec.") => "vectors",
                n if n.starts_with("text.") => "text",
                n if n.starts_with("store.") => "doc store",
                "pk" => "primary key",
                _ => "columns",
            };
            *by.entry(comp.to_string()).or_default() += len;
        }
    }
    let stats = db.storage().file_stats();
    json!({"components": by, "file_bytes": stats.file_bytes, "segments": snap.segments().len()})
}

fn load(data: &Path, vectors: &Path, dim: usize, dbp: &Path, index: &str, holdout: usize, min_refs: usize, seed: u64, threads: usize, out: &Path) {
    let papers = read_papers(data);
    let vecs = read_vectors(vectors, dim);
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut cands: Vec<i64> = papers.iter().filter(|p| p.split == "test" && p.cites.len() >= min_refs).map(|p| p.id).collect();
    cands.shuffle(&mut rng);
    let held: HashSet<i64> = cands.into_iter().take(holdout).collect();
    let mut held_sorted: Vec<i64> = held.iter().copied().collect();
    held_sorted.sort();
    std::fs::write(holdout_path(dbp), serde_json::to_string(&held_sorted).unwrap()).unwrap();
    let _ = std::fs::remove_file(dbp);
    let _ = std::fs::remove_file(dbp.with_extension("strata-wal"));
    let build = BuildOptions::default().set("vector.threads", threads);
    let opts = DbOptions { build, sync: false, ..DbOptions::default() };
    let db = Db::create(dbp, arxiv_schema(dim, index), opts).unwrap();
    let t0 = Instant::now();
    let docs = papers.iter().filter(|p| !held.contains(&p.id)).map(|p| {
        let v = &vecs[p.id as usize * dim..(p.id as usize + 1) * dim];
        vec![
            Value::Int(p.id),
            Value::Str(p.title.clone()),
            Value::Str(p.abstract_.clone()),
            Value::Null,
            Value::Int(p.year),
            Value::Str(p.category.clone()),
            Value::Vector(v.to_vec().into()),
            cites_json(&p.cites),
        ]
    });
    db.load(docs).unwrap();
    let secs = t0.elapsed().as_secs_f64();
    let n_edges: usize = papers.iter().filter(|p| !held.contains(&p.id)).map(|p| p.cites.len()).sum();
    let s = sizes(&db);
    eprintln!("loaded {} papers, {n_edges} edges in {secs:.1} s: {s}", papers.len() - held.len());
    // The graph section alone: load the same rows without text and vectors.
    let gpath = dbp.with_extension("edges-only.strata");
    let _ = std::fs::remove_file(&gpath);
    let gschema = Schema::new("papers", vec![Field::new("id", FieldType::Int), Field::new("cites", FieldType::Edges).not_stored()], Some("id")).unwrap();
    let gdb = Db::create(&gpath, gschema, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let t1 = Instant::now();
    gdb.load(papers.iter().filter(|p| !held.contains(&p.id)).map(|p| vec![Value::Int(p.id), cites_json(&p.cites)])).unwrap();
    let gsecs = t1.elapsed().as_secs_f64();
    let gs = sizes(&gdb);
    drop(gdb);
    let _ = std::fs::remove_file(&gpath);
    std::fs::create_dir_all(out).unwrap();
    let res = json!({"papers": papers.len() - held.len(), "held_out": held.len(), "edges": n_edges, "index": index, "load_s": secs, "sizes": s,
        "edges_only": {"load_s": gsecs, "sizes": gs}});
    std::fs::write(out.join("load_arxiv.json"), serde_json::to_string_pretty(&res).unwrap()).unwrap();
}

/// In-memory CSR over dense node numbers: the traversal lower bound.
struct Csr {
    out_off: Vec<u32>,
    out: Vec<u32>,
    in_off: Vec<u32>,
    inn: Vec<u32>,
    dir: u8, // 0 out, 1 in, 2 both
}

impl Csr {
    fn new(n: usize, edges: &[(u32, u32)]) -> Csr {
        let build = |key: &dyn Fn(&(u32, u32)) -> (u32, u32)| {
            let mut e: Vec<(u32, u32)> = edges.iter().map(key).collect();
            e.sort_unstable();
            let mut off = vec![0u32; n + 1];
            for &(s, _) in &e {
                off[s as usize + 1] += 1;
            }
            for i in 0..n {
                off[i + 1] += off[i];
            }
            (off, e.into_iter().map(|x| x.1).collect::<Vec<u32>>())
        };
        let (out_off, out) = build(&|&(s, t)| (s, t));
        let (in_off, inn) = build(&|&(s, t)| (t, s));
        Csr { out_off, out, in_off, inn, dir: 0 }
    }
}

impl Adjacency for Csr {
    fn neighbors(&mut self, n: Node, reverse: bool, out: &mut Vec<(Node, f32)>) -> strata::Result<()> {
        let u = n as usize;
        let dir = match (self.dir, reverse) {
            (0, true) => 1,
            (1, true) => 0,
            (d, _) => d,
        };
        if dir != 1 {
            out.extend(self.out[self.out_off[u] as usize..self.out_off[u + 1] as usize].iter().map(|&v| (v as Node, 1.0)));
        }
        if dir != 0 {
            out.extend(self.inn[self.in_off[u] as usize..self.in_off[u + 1] as usize].iter().map(|&v| (v as Node, 1.0)));
        }
        Ok(())
    }
}

fn dir_code(d: &str) -> u8 {
    match d {
        "out" => 0,
        "in" => 1,
        _ => 2,
    }
}

fn stats_ms(xs: &[f64]) -> Json {
    json!({"p50": percentile(xs, 50.0), "p95": percentile(xs, 95.0), "mean": mean(xs)})
}

/// Times `run` on each query, returning per-query milliseconds and outputs.
fn time_all<T>(n: usize, mut run: impl FnMut(usize) -> T) -> (Vec<f64>, Vec<T>) {
    let mut ms = Vec::with_capacity(n);
    let mut outs = Vec::with_capacity(n);
    for i in 0..n {
        let t = Instant::now();
        let o = run(i);
        ms.push(t.elapsed().as_secs_f64() * 1e3);
        outs.push(o);
    }
    (ms, outs)
}

fn count(db: &Db, sql: &str, params: &Params) -> i64 {
    db.query(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).rows[0][0].as_i64().unwrap()
}

/// Traversal latency through SQL, against the CSR and SQLite on the same
/// graph (`ids` are the stored primary keys, `edges` between them).
#[allow(clippy::too_many_arguments)]
fn traversal_study(db: &Db, table: &str, field: &str, ids: &[i64], edges: &[(i64, i64)], queries: usize, seed: u64, vectors: Option<(&[f32], usize)>, sqlite: Option<&Path>) -> Json {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let pos: HashMap<i64, u32> = ids.iter().enumerate().map(|(i, &k)| (k, i as u32)).collect();
    let dense: Vec<(u32, u32)> = edges.iter().filter_map(|(s, t)| Some((*pos.get(s)?, *pos.get(t)?))).collect();
    let mut csr = Csr::new(ids.len(), &dense);
    let seeds: Vec<i64> = (0..queries).map(|_| ids[rng.gen_range(0..ids.len())]).collect();
    let mut out = serde_json::Map::new();
    let warm = |db: &Db| {
        for s in seeds.iter().take(20) {
            count(db, &format!("SELECT COUNT(*) FROM {table} WHERE REACHABLE({field}, :s, hops => 2, dir => 'both')"), &Params::new().set("s", Value::Int(*s)));
        }
    };
    warm(db);
    #[cfg(feature = "baselines")]
    let lite = sqlite.map(|p| sqlite_graph::open_or_build(p, edges));
    #[cfg(not(feature = "baselines"))]
    let _ = sqlite;
    for (dir, hops) in [("out", 1u32), ("out", 2), ("out", 3), ("in", 1), ("both", 1), ("both", 2)] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE REACHABLE({field}, :s, hops => {hops}, dir => '{dir}')");
        let (ms, counts) = time_all(queries, |i| count(db, &sql, &Params::new().set("s", Value::Int(seeds[i]))));
        csr.dir = dir_code(dir);
        let (cms, ccounts) = time_all(queries, |i| {
            let r = traverse::bfs(&mut csr, &[pos[&seeds[i]] as Node], hops, usize::MAX).unwrap();
            r.dist.values().filter(|&&d| d >= 1).count() as i64
        });
        assert_eq!(counts, ccounts, "Strata and the CSR disagree on {dir} {hops}");
        let mut row = json!({"strata_ms": stats_ms(&ms), "csr_ms": stats_ms(&cms), "rows": mean(&counts.iter().map(|&c| c as f64).collect::<Vec<_>>())});
        #[cfg(feature = "baselines")]
        if let Some(l) = &lite {
            let (lms, lcounts) = time_all(queries, |i| l.reach_count(seeds[i], hops, dir));
            assert_eq!(counts, lcounts, "Strata and SQLite disagree on {dir} {hops}");
            row["sqlite_ms"] = stats_ms(&lms);
        }
        eprintln!("reach {dir} {hops}: {row}");
        out.insert(format!("reach_{dir}_{hops}"), row);
    }
    // Rows with their distance, nearest first (projection included).
    let sql = format!("SELECT id, hops() FROM {table} WHERE REACHABLE({field}, :s, hops => 2, dir => 'both') ORDER BY hops(), id LIMIT 20");
    let (ms, _) = time_all(queries, |i| db.query(&sql, &Params::new().set("s", Value::Int(seeds[i]))).unwrap().rows.len());
    out.insert("reach_both_2_top20".into(), json!({"strata_ms": stats_ms(&ms)}));
    // Shortest paths between random pairs.
    let pairs: Vec<(i64, i64)> = (0..queries).map(|_| (ids[rng.gen_range(0..ids.len())], ids[rng.gen_range(0..ids.len())])).collect();
    for dir in ["out", "both"] {
        let sql = format!("SELECT id FROM {table} WHERE PATH({field}, :a, :b, dir => '{dir}', max_hops => 10) ORDER BY hops() LIMIT 100");
        let (ms, lens) = time_all(queries, |i| db.query(&sql, &Params::new().set("a", Value::Int(pairs[i].0)).set("b", Value::Int(pairs[i].1))).unwrap().rows.len());
        csr.dir = dir_code(dir);
        let (cms, clens) = time_all(queries, |i| {
            traverse::shortest_path(&mut csr, &[pos[&pairs[i].0] as Node], &[pos[&pairs[i].1] as Node], 10, usize::MAX).unwrap().0.map_or(0, |p| p.len())
        });
        assert_eq!(lens, clens, "path lengths disagree ({dir})");
        let found = lens.iter().filter(|&&l| l > 0).count();
        let row = json!({"strata_ms": stats_ms(&ms), "csr_ms": stats_ms(&cms), "found": found, "mean_len": mean(&lens.iter().filter(|&&l| l > 0).map(|&l| (l - 1) as f64).collect::<Vec<_>>())});
        eprintln!("path {dir}: {row}");
        out.insert(format!("path_{dir}"), row);
    }
    // Personalized PageRank.
    for eps in [1e-4, 1e-5] {
        let sql = format!("SELECT id FROM {table} ORDER BY ppr({field}, :s, eps => {eps}) DESC LIMIT 20");
        let (ms, _) = time_all(queries, |i| db.query(&sql, &Params::new().set("s", Value::Int(seeds[i]))).unwrap().rows.len());
        csr.dir = 2;
        let (cms, pushes) = time_all(queries, |i| traverse::ppr(&mut csr, &[(pos[&seeds[i]] as Node, 1.0)], false, 0.15, eps, 1_000_000).unwrap().pushes as f64);
        let row = json!({"strata_ms": stats_ms(&ms), "csr_ms": stats_ms(&cms), "pushes": mean(&pushes)});
        eprintln!("ppr eps {eps}: {row}");
        out.insert(format!("ppr_{eps:e}"), row);
    }
    // Vector graph nodes: seeds from a vector search.
    if let Some((vecs, dim)) = vectors {
        let qv = |i: usize| -> Value {
            let p = pos[&seeds[i]] as usize;
            let id = ids[p] as usize;
            Value::Vector(vecs[id * dim..(id + 1) * dim].to_vec().into())
        };
        let cases = [
            ("vector_top20", format!("SELECT id FROM {table} ORDER BY similarity(emb, :q) DESC LIMIT 20")),
            (
                "vector_seeds_expand",
                format!("SELECT id FROM {table} WHERE REACHABLE({field}, (SELECT id FROM {table} ORDER BY similarity(emb, :q) DESC LIMIT 10), dir => 'both', min_hops => 0) ORDER BY similarity(emb, :q) DESC LIMIT 20"),
            ),
            (
                "rrf_vector_ppr",
                format!("SELECT id FROM {table} ORDER BY rrf(similarity(emb, :q), ppr({field}, (SELECT id FROM {table} ORDER BY similarity(emb, :q) DESC LIMIT 10))) DESC LIMIT 20"),
            ),
            ("semantic_knn_1hop", format!("SELECT COUNT(*) FROM {table} WHERE REACHABLE(emb, :s, k => 10)")),
            ("semantic_knn_2hop", format!("SELECT COUNT(*) FROM {table} WHERE REACHABLE(emb, :s, k => 10, hops => 2)")),
        ];
        for (name, sql) in cases {
            let (ms, _) = time_all(queries.min(100), |i| db.query(&sql, &Params::new().set("q", qv(i)).set("s", Value::Int(seeds[i]))).unwrap().rows.len());
            let row = json!({"strata_ms": stats_ms(&ms)});
            eprintln!("{name}: {row}");
            out.insert(name.into(), row);
        }
    }
    Json::Object(out)
}

#[cfg(feature = "baselines")]
mod sqlite_graph {
    use std::path::Path;

    use rusqlite::{params, Connection};

    pub struct Lite(Connection);

    /// An edge table with both directions indexed (WITHOUT ROWID: the
    /// primary key is the table).
    pub fn open_or_build(path: &Path, edges: &[(i64, i64)]) -> Lite {
        let fresh = !path.exists();
        let c = Connection::open(path).unwrap();
        if fresh {
            let t = std::time::Instant::now();
            c.execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF; CREATE TABLE edges (src INTEGER, dst INTEGER, PRIMARY KEY (src, dst)) WITHOUT ROWID;").unwrap();
            let tx = c.unchecked_transaction().unwrap();
            {
                let mut ins = tx.prepare("INSERT OR IGNORE INTO edges VALUES (?1, ?2)").unwrap();
                for (s, d) in edges {
                    ins.execute(params![s, d]).unwrap();
                }
            }
            tx.commit().unwrap();
            c.execute_batch("CREATE INDEX edges_dst ON edges (dst, src); ANALYZE;").unwrap();
            eprintln!("sqlite: {} edges in {:.1} s, {} bytes", edges.len(), t.elapsed().as_secs_f64(), std::fs::metadata(path).unwrap().len());
        }
        c.execute_batch("PRAGMA cache_size = -1000000; PRAGMA mmap_size = 4000000000;").unwrap();
        Lite(c)
    }

    impl Lite {
        /// Rows within `hops` of `seed`, the seed excluded (as REACHABLE).
        pub fn reach_count(&self, seed: i64, hops: u32, dir: &str) -> i64 {
            let step = match dir {
                "out" => "SELECT e.dst, r.d + 1 FROM r JOIN edges e ON e.src = r.n WHERE r.d < ?2",
                "in" => "SELECT e.src, r.d + 1 FROM r JOIN edges e ON e.dst = r.n WHERE r.d < ?2",
                _ => "SELECT e.dst, r.d + 1 FROM r JOIN edges e ON e.src = r.n WHERE r.d < ?2 \
                      UNION SELECT e.src, r.d + 1 FROM r JOIN edges e ON e.dst = r.n WHERE r.d < ?2",
            };
            let sql = format!("WITH RECURSIVE r(n, d) AS (SELECT ?1, 0 UNION {step}) SELECT COUNT(DISTINCT n) FROM r WHERE n <> ?1");
            self.0.prepare_cached(&sql).unwrap().query_row(params![seed, hops], |r| r.get(0)).unwrap()
        }
    }
}

fn latency(dbp: &Path, data: &Path, vectors: &Path, dim: usize, queries: usize, seed: u64, sqlite: Option<&Path>, out: &Path) {
    let papers = read_papers(data);
    let held: HashSet<i64> = serde_json::from_str::<Vec<i64>>(&std::fs::read_to_string(holdout_path(dbp)).unwrap()).unwrap().into_iter().collect();
    let ids: Vec<i64> = papers.iter().filter(|p| !held.contains(&p.id)).map(|p| p.id).collect();
    let edges: Vec<(i64, i64)> = papers.iter().filter(|p| !held.contains(&p.id)).flat_map(|p| p.cites.iter().filter(|t| !held.contains(t)).map(move |&t| (p.id, t))).collect();
    let vecs = read_vectors(vectors, dim);
    let db = Db::open(dbp, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    let res = traversal_study(&db, "papers", "cites", &ids, &edges, queries, seed, Some((&vecs, dim)), sqlite);
    std::fs::create_dir_all(out).unwrap();
    std::fs::write(out.join("latency_arxiv.json"), serde_json::to_string_pretty(&json!({"rows": ids.len(), "edges": edges.len(), "queries": queries, "results": res})).unwrap()).unwrap();
}

/// A power-law graph: node i links to `degree` earlier nodes, each the
/// target of a random earlier edge with probability 1/2 (preferential
/// attachment by copying) and a uniform earlier node otherwise.
fn synthetic(nodes: usize, degree: usize, seed: u64, dbp: &Path, queries: usize, sqlite: Option<&Path>, out: &Path) {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut targets: Vec<i64> = Vec::with_capacity(nodes * degree);
    let mut offsets = vec![0usize];
    for i in 0..nodes {
        if i > 0 {
            for _ in 0..degree.min(i) {
                let t = if !targets.is_empty() && rng.gen_bool(0.5) { targets[rng.gen_range(0..targets.len())] } else { rng.gen_range(0..i as i64) };
                targets.push(t);
            }
        }
        offsets.push(targets.len());
    }
    let edges: Vec<(i64, i64)> = (0..nodes).flat_map(|i| targets[offsets[i]..offsets[i + 1]].iter().map(move |&t| (i as i64, t))).collect();
    let _ = std::fs::remove_file(dbp);
    let _ = std::fs::remove_file(dbp.with_extension("strata-wal"));
    let schema = Schema::new("g", vec![Field::new("id", FieldType::Int), Field::new("grp", FieldType::Keyword), Field::new("links", FieldType::Edges).not_stored()], Some("id")).unwrap();
    let db = Db::create(dbp, schema, DbOptions { sync: false, segment_max_docs: 250_000, ..DbOptions::default() }).unwrap();
    let t0 = Instant::now();
    db.load((0..nodes).map(|i| {
        let mut ts: Vec<i64> = targets[offsets[i]..offsets[i + 1]].to_vec();
        ts.sort_unstable();
        ts.dedup();
        vec![Value::Int(i as i64), Value::Str(format!("g{}", i % 10)), cites_json(&ts)]
    }))
    .unwrap();
    let load_s = t0.elapsed().as_secs_f64();
    let s = sizes(&db);
    eprintln!("synthetic: {nodes} nodes, {} edges, loaded in {load_s:.1} s: {s}", edges.len());
    let mut uniq = edges.clone();
    uniq.sort_unstable();
    uniq.dedup();
    let ids: Vec<i64> = (0..nodes as i64).collect();
    let res = traversal_study(&db, "g", "links", &ids, &uniq, queries, seed + 1, None, sqlite);
    std::fs::create_dir_all(out).unwrap();
    let name = format!("synthetic_{nodes}.json");
    std::fs::write(out.join(name), serde_json::to_string_pretty(&json!({"nodes": nodes, "edges": uniq.len(), "load_s": load_s, "sizes": s, "results": res})).unwrap()).unwrap();
}

/// Ranking methods for the reference-finding task. `S` is replaced by the
/// number of vector seeds.
const METHODS: &[(&str, &str)] = &[
    ("bm25", "SELECT id FROM papers ORDER BY bm25(body, :t) DESC LIMIT :k"),
    ("vector", "SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT :k"),
    ("hybrid", "SELECT id FROM papers ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT :k"),
    // The seeds themselves are the vector search's own results; these
    // methods rank only what the graph adds.
    (
        "cited by vector seeds, by similarity",
        "SELECT id FROM papers WHERE REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), dir => 'out') \
         AND NOT REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), hops => 0, min_hops => 0) \
         ORDER BY similarity(emb, :q) DESC LIMIT :k",
    ),
    (
        "linked to vector seeds either way, by similarity",
        "SELECT id FROM papers WHERE REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), dir => 'both') \
         AND NOT REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), hops => 0, min_hops => 0) \
         ORDER BY similarity(emb, :q) DESC LIMIT :k",
    ),
    (
        "ppr from vector seeds",
        "SELECT id FROM papers WHERE NOT REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), hops => 0, min_hops => 0) \
         ORDER BY ppr(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), weighted => 1) DESC LIMIT :k",
    ),
    (
        "rrf(vector, ppr from vector seeds)",
        "SELECT id FROM papers ORDER BY rrf(similarity(emb, :q), ppr(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), weighted => 1)) DESC LIMIT :k",
    ),
    (
        "rrf(bm25, vector, ppr from hybrid seeds)",
        "SELECT id FROM papers ORDER BY rrf(bm25(body, :t), similarity(emb, :q), ppr(cites, (SELECT id FROM papers ORDER BY rrf(bm25(body, :t), similarity(emb, :q)) DESC LIMIT S), weighted => 1)) DESC LIMIT :k",
    ),
    (
        "semantic kNN graph from vector seeds",
        "SELECT id FROM papers WHERE REACHABLE(emb, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), k => 10) \
         AND NOT REACHABLE(cites, (SELECT id FROM papers ORDER BY similarity(emb, :q) DESC LIMIT S), hops => 0, min_hops => 0) \
         ORDER BY similarity(emb, :q) DESC LIMIT :k",
    ),
];

fn ndcg(ranked: &[i64], truth: &HashSet<i64>, k: usize) -> f64 {
    let dcg: f64 = ranked.iter().take(k).enumerate().filter(|(_, d)| truth.contains(d)).map(|(i, _)| 1.0 / ((i + 2) as f64).log2()).sum();
    let idcg: f64 = (0..truth.len().min(k)).map(|i| 1.0 / ((i + 2) as f64).log2()).sum();
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

#[allow(clippy::too_many_arguments)]
fn quality(dbp: &Path, data: &Path, vectors: &Path, dim: usize, k: usize, seeds: &[usize], limit: Option<usize>, only: &[String], out: &Path) {
    let papers = read_papers(data);
    let vecs = read_vectors(vectors, dim);
    let mut held: Vec<i64> = serde_json::from_str(&std::fs::read_to_string(holdout_path(dbp)).unwrap()).unwrap();
    let held_set: HashSet<i64> = held.iter().copied().collect();
    if let Some(l) = limit {
        held.truncate(l);
    }
    let db = Db::open(dbp, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    std::fs::create_dir_all(out).unwrap();
    let mut rows_out = std::io::BufWriter::new(std::fs::File::create(out.join("quality_arxiv.rows.jsonl")).unwrap());
    let mut summary = serde_json::Map::new();
    let methods: Vec<(String, String)> = METHODS
        .iter()
        .filter(|(n, _)| only.is_empty() || only.iter().any(|o| n.contains(o.as_str())))
        .flat_map(|(n, sql)| {
            if sql.contains(" S)") {
                seeds.iter().map(|s| (format!("{n} (S={s})"), sql.replace(" S)", &format!(" {s})")))).collect::<Vec<_>>()
            } else {
                vec![(n.to_string(), sql.to_string())]
            }
        })
        .collect();
    for (name, sql) in &methods {
        let sql = sql.replace(":k", &k.to_string());
        let (mut rec, mut nd, mut ms) = (Vec::new(), Vec::new(), Vec::new());
        for &q in &held {
            let p = &papers[q as usize];
            let truth: HashSet<i64> = p.cites.iter().copied().filter(|t| !held_set.contains(t)).collect();
            if truth.is_empty() {
                continue;
            }
            let params = Params::new()
                .set("q", Value::Vector(vecs[q as usize * dim..(q as usize + 1) * dim].to_vec().into()))
                .set("t", Value::Str(format!("{}\n{}", p.title, p.abstract_)));
            let t = Instant::now();
            let rs = db.query(&sql, &params).unwrap_or_else(|e| panic!("{name}: {e}"));
            ms.push(t.elapsed().as_secs_f64() * 1e3);
            let ranked: Vec<i64> = rs.rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
            let hit = ranked.iter().take(k).filter(|d| truth.contains(d)).count();
            let r = hit as f64 / truth.len() as f64;
            let n = ndcg(&ranked, &truth, k);
            rec.push(r);
            nd.push(n);
            writeln!(rows_out, "{}", json!({"method": name, "query": q, "refs": truth.len(), "hits": hit, "recall": r, "ndcg": n, "ms": ms.last()})).unwrap();
        }
        let row = json!({"queries": rec.len(), "recall": mean(&rec), "ndcg": mean(&nd), "ms": stats_ms(&ms)});
        eprintln!("{name:<58} recall@{k} {:.3}  nDCG@{k} {:.3}  {:.1} ms", mean(&rec), mean(&nd), percentile(&ms, 50.0));
        summary.insert(name.clone(), row);
    }
    std::fs::write(out.join("quality_arxiv.json"), serde_json::to_string_pretty(&json!({"k": k, "queries": held.len(), "methods": summary})).unwrap()).unwrap();
}

fn main() {
    match Cli::parse().cmd {
        Cmd::Load { data, vectors, dim, db, index, holdout, min_refs, seed, threads, out } => load(&data, &vectors, dim, &db, &index, holdout, min_refs, seed, threads, &out),
        Cmd::Latency { db, data, vectors, dim, queries, seed, sqlite, out } => latency(&db, &data, &vectors, dim, queries, seed, sqlite.as_deref(), &out),
        Cmd::Quality { db, data, vectors, dim, k, seeds, limit, methods, out } => quality(&db, &data, &vectors, dim, k, &seeds, limit, &methods, &out),
        Cmd::Repeat { db, sql, max, n } => {
            let db = Db::open(&db, DbOptions { sync: false, ..DbOptions::default() }).unwrap();
            let mut rng = rand::rngs::StdRng::seed_from_u64(1);
            let (ms, _) = time_all(n, |_| db.query(&sql, &Params::new().set("s", Value::Int(rng.gen_range(0..max)))).unwrap().rows.len());
            eprintln!("{}", stats_ms(&ms));
        }
        Cmd::Synthetic { nodes, degree, seed, db, queries, sqlite, out } => synthetic(nodes, degree, seed, &db, queries, sqlite.as_deref(), &out),
    }
}
