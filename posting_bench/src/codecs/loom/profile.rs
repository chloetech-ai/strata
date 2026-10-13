//! Profiling harness (ignored tests) on the benchmark's workloads:
//! `LOOM_DIST=clustered|uniform|text [LOOM_INPUT=path] [LOOM_CHUNK=16]
//! [LOOM_LAMBDA=num/den] cargo test --release loom::profile -- --ignored --nocapture`.

use super::*;
use crate::algorithms::cursor;
use crate::codecs::pef::PartitionedEF;
use crate::data::{self, Collection, Distribution, QueryConfig, QueryMode, SyntheticConfig};
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

fn workload() -> (Collection, Vec<Vec<u32>>) {
    let dist = std::env::var("LOOM_DIST").unwrap_or_else(|_| "clustered".into());
    let (coll, min_len) = match dist.as_str() {
        "text" => {
            let p = std::env::var("LOOM_INPUT").expect("LOOM_INPUT");
            (data::load_text(std::path::Path::new(&p), None).unwrap(), 100)
        }
        d => {
            let distribution = if d == "uniform" { Distribution::Uniform } else { Distribution::Clustered };
            (data::synthetic(&SyntheticConfig { distribution, ..Default::default() }), 1000)
        }
    };
    let queries = data::generate_queries(
        &coll,
        &QueryConfig {
            num: 300,
            min_terms: 2,
            max_terms: 4,
            min_list_len: min_len,
            max_list_len: None,
            mode: QueryMode::Cooccur,
            seed: 1001,
        },
    );
    (coll, queries)
}

fn params() -> Params {
    let mut p = Params::DEFAULT;
    if let Ok(c) = std::env::var("LOOM_CHUNK") {
        p.chunk = c.parse().unwrap();
    }
    if let Ok(l) = std::env::var("LOOM_LAMBDA") {
        let (a, b) = l.split_once('/').unwrap();
        p.lambda = (a.parse().unwrap(), b.parse().unwrap());
    }
    p
}

fn best_of(reps: usize, mut f: impl FnMut()) -> f64 {
    (0..reps)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64()
        })
        .fold(f64::INFINITY, f64::min)
}

/// Dump the query lists and queries of the workload (`LOOM_DUMP=path`), so
/// that [`and_from_dump`] can run under valgrind without generating data.
#[test]
#[ignore]
fn dump_workload() {
    use std::io::Write;
    let (coll, queries) = workload();
    let path = std::env::var("LOOM_DUMP").expect("LOOM_DUMP");
    let mut w = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut put = |v: u32| w.write_all(&v.to_le_bytes()).unwrap();
    let mut terms: Vec<u32> = queries.iter().flatten().copied().collect();
    terms.sort_unstable();
    terms.dedup();
    put(terms.len() as u32);
    for &t in &terms {
        let l = &coll.lists[t as usize];
        put(l.len() as u32);
        l.iter().for_each(|&d| put(d));
    }
    put(queries.len() as u32);
    for q in &queries {
        put(q.len() as u32);
        for t in q {
            put(terms.binary_search(t).unwrap() as u32);
        }
    }
}

/// Run the dumped AND (or OR with `LOOM_OP=or`) queries `LOOM_REPS` times.
#[test]
#[ignore]
fn and_from_dump() {
    let path = std::env::var("LOOM_DUMP").expect("LOOM_DUMP");
    let bytes = std::fs::read(path).unwrap();
    let mut it = bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()));
    let nl = it.next().unwrap();
    let lists: Vec<Loom> = (0..nl)
        .map(|_| {
            let n = it.next().unwrap();
            let docs: Vec<u32> = (0..n).map(|_| it.next().unwrap()).collect();
            Loom::build_with(&docs, params())
        })
        .collect();
    let nq = it.next().unwrap();
    let queries: Vec<Vec<&Loom>> = (0..nq)
        .map(|_| {
            let k = it.next().unwrap();
            (0..k).map(|_| &lists[it.next().unwrap() as usize]).collect()
        })
        .collect();
    let reps: usize = std::env::var("LOOM_REPS").map(|r| r.parse().unwrap()).unwrap_or(1);
    let or = std::env::var("LOOM_OP").is_ok_and(|o| o == "or");
    // Each rep is one pass over the queries; report the best pass.
    let mut total = 0;
    let best = best_of(reps, || {
        for q in &queries {
            total += if or { run_or(q) } else { run_and(q) };
        }
    });
    println!("{total} results, best pass {:.4} ms/query", best * 1e3 / queries.len() as f64);
    if let Ok(qi) = std::env::var("LOOM_Q") {
        // One query: the time of every prefix of its lists (driver first).
        let mut q = queries[qi.parse::<usize>().unwrap()].clone();
        q.sort_by_key(|l| l.len());
        for mode in 0..6u8 {
            and::FORCE_MODE.store(mode, std::sync::atomic::Ordering::Relaxed);
            for m in 2..=q.len() {
                let t = best_of(20, || {
                    black_box(run_and(&q[..m]));
                });
                println!("  mode {mode}: first {m} lists: {:.1} us, {} results", t * 1e6, run_and(&q[..m]));
            }
        }
        and::FORCE_MODE.store(0, std::sync::atomic::Ordering::Relaxed);
        for (li, l) in q.iter().enumerate() {
            let parts: Vec<String> = l
                .partitions()
                .take(40)
                .map(|(b, e, k, n)| format!("{}[{b},{e}]n{n}", ["R", "B", "E"][k as usize]))
                .collect();
            println!("  list {li} ({} postings): {}", l.len(), parts.join(" "));
        }
    }
    if std::env::var("LOOM_TOP").is_ok() {
        // The slowest queries (best of `reps` each) and their shapes.
        let mut per: Vec<(f64, usize)> = queries
            .iter()
            .enumerate()
            .map(|(i, q)| (best_of(reps, || { black_box(if or { run_or(q) } else { run_and(q) }); }), i))
            .collect();
        let sum: f64 = per.iter().map(|p| p.0).sum();
        per.sort_by(|a, b| b.0.total_cmp(&a.0));
        for &(t, i) in per.iter().take(12) {
            let q = &queries[i];
            let shape: Vec<String> = q
                .iter()
                .map(|l| {
                    let k = l.partitions().fold([0; 3], |mut k, (_, _, kind, _)| {
                        k[kind as usize] += 1;
                        k
                    });
                    format!("{}(r{}/b{}/e{})", l.len(), k[0], k[1], k[2])
                })
                .collect();
            println!(
                "  q{i}: {:.1} us ({:.1}% of total), {} results: {}",
                t * 1e6,
                100.0 * t / sum,
                if or { run_or(q) } else { run_and(q) },
                shape.join(" ")
            );
        }
    }
}

#[inline(never)]
fn run_and(q: &[&Loom]) -> u64 {
    Loom::and_count(q)
}

#[inline(never)]
fn run_or(q: &[&Loom]) -> u64 {
    // Materialised into a reused buffer, as the benchmark does.
    thread_local! {
        static OUT: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    OUT.with(|o| {
        let mut o = o.borrow_mut();
        o.clear();
        Loom::union(q, &mut o);
        o.len() as u64
    })
}

/// Isolated EF probe cost: one 4096-posting partition (warm), probed with
/// sorted targets `ratio` postings apart on average.
#[test]
#[ignore]
fn ef_probe_micro() {
    use crate::codecs::conformance::random_sorted;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    let mut rng = StdRng::seed_from_u64(3);
    for span in [8_192u64, 65_536, 4_000_000] {
        let vals = random_sorted(&mut rng, 4096, span as u32);
        let mut words = Vec::new();
        ef::encode(&vals, 0, span, &mut words);
        words.extend([0; 9]);
        let lay = ef::Layout::new(span, vals.len() as u64);
        let mk = || unsafe { ef::EfCur::new(words.as_ptr(), lay.l, lay.low_words, lay.high_words) };
        let last = *vals.last().unwrap() as u64;
        for ratio in [1u64, 4, 16, 64, 256] {
            let gap = span / 4096 * ratio;
            let mut ts = Vec::new();
            let mut t = 0u64;
            loop {
                t += rng.gen_range(1..=2 * gap);
                if t > last {
                    break;
                }
                // Half the targets are members.
                let t2 = if rng.gen_bool(0.5) { vals[vals.partition_point(|&v| (v as u64) < t)] as u64 } else { t };
                if ts.last().is_none_or(|&p| t2 > p) {
                    ts.push(t2);
                }
                t = t2;
            }
            let reps = 200;
            let t_cur = best_of(5, || {
                for _ in 0..reps {
                    let mut c = mk();
                    let mut cur = u64::MAX;
                    let mut hits = 0;
                    for &x in &ts {
                        let v = if cur != u64::MAX && x <= cur { cur } else { unsafe { c.next_geq(x) } };
                        cur = v;
                        hits += (v == x) as u32;
                    }
                    black_box(hits);
                }
            });
            let t_probe = best_of(5, || {
                for _ in 0..reps {
                    let mut p = ef::EfProbe::new(&mk());
                    let mut hits = 0;
                    for &x in &ts {
                        hits += unsafe { p.probe(x) }.0 as u32;
                    }
                    black_box(hits);
                }
            });
            let per = |t: f64| t * 1e9 / (reps * ts.len()) as f64;
            println!(
                "span {span:>8} (l={}) ratio {ratio:>3}: {} probes, EfCur::next_geq {:.2} ns, EfProbe::probe {:.2} ns",
                lay.l,
                ts.len(),
                per(t_cur),
                per(t_probe)
            );
        }
    }
}

#[test]
#[ignore]
fn profile() {
    let (coll, queries) = workload();
    let params = params();
    let mut terms: Vec<u32> = queries.iter().flatten().copied().collect();
    terms.sort_unstable();
    terms.dedup();
    let t = Instant::now();
    let loom: HashMap<u32, Loom> = terms.iter().map(|&t| (t, Loom::build_with(&coll.lists[t as usize], params))).collect();
    let tb = t.elapsed();
    let pef: HashMap<u32, PartitionedEF> =
        terms.iter().map(|&t| (t, PartitionedEF::build(&coll.lists[t as usize], coll.universe))).collect();

    // Space and partition statistics over the query lists.
    let postings: u64 = terms.iter().map(|t| coll.lists[*t as usize].len() as u64).sum();
    let lb: u64 = loom.values().map(|l| l.size_bits()).sum();
    let pb: u64 = pef.values().map(|l| l.size_bits()).sum();
    let mut kinds = [(0usize, 0usize, 0u64); 3];
    for l in loom.values() {
        for (base, last, k, n) in l.partitions() {
            let e = &mut kinds[k as usize];
            e.0 += 1;
            e.1 += n;
            e.2 += (last - base) as u64 + 1;
        }
    }
    println!(
        "{params:?}: Loom {:.4} bits/posting, PEF {:.4} ({:+.2}%), build {:.2?}",
        lb as f64 / postings as f64,
        pb as f64 / postings as f64,
        100.0 * (lb as f64 / pb as f64 - 1.0),
        tb
    );
    for (name, k) in ["run", "bitvector", "EF"].iter().zip(kinds) {
        println!(
            "  {name}: {} partitions, {:.1}% of postings, {:.0} postings/partition, density {:.3}",
            k.0,
            100.0 * k.1 as f64 / postings as f64,
            k.1 as f64 / k.0.max(1) as f64,
            k.1 as f64 / k.2.max(1) as f64
        );
    }

    // Where AND probes land: partition kind of the other lists at every
    // driver posting.
    let mut land = [0u64; 3];
    for q in &queries {
        let mut ls: Vec<&Loom> = q.iter().map(|t| &loom[t]).collect();
        ls.sort_by_key(|l| l.len());
        let mut d = Vec::new();
        ls[0].decode_into(&mut d);
        for o in &ls[1..] {
            let mut p = 0;
            for &x in &d {
                while p < o.ends.len() && o.ends[p] < x {
                    p += 1;
                }
                if p == o.ends.len() {
                    break;
                }
                land[Part::load(o, p).kind as usize] += 1;
            }
        }
    }
    let tot: u64 = land.iter().sum();
    println!(
        "  AND probes (all candidates, no early exit) land in run {:.1}% / bitvector {:.1}% / EF {:.1}%",
        100.0 * land[0] as f64 / tot as f64,
        100.0 * land[1] as f64 / tot as f64,
        100.0 * land[2] as f64 / tot as f64
    );

    let reps = 5;
    let nq = queries.len() as f64;
    let qs: Vec<Vec<&Loom>> = queries.iter().map(|q| q.iter().map(|t| &loom[t]).collect()).collect();
    let qp: Vec<Vec<&PartitionedEF>> = queries.iter().map(|q| q.iter().map(|t| &pef[t]).collect()).collect();
    let mut out = Vec::new();
    let ms = |s: f64| s * 1e3 / nq;
    let t_and = best_of(reps, || {
        for q in &qs {
            out.clear();
            Loom::intersect(q, &mut out);
            black_box(&out);
        }
    });
    // Warm: every query repeated 10 times in a row (cache-resident lists).
    let t_warm = best_of(reps, || {
        for q in &qs {
            for _ in 0..10 {
                black_box(Loom::and_count(q));
            }
        }
    }) / 10.0;
    let t_warm_pef = best_of(reps, || {
        for q in &qp {
            for _ in 0..10 {
                out.clear();
                let mut c: Vec<_> = q.iter().map(|l| l.cursor()).collect();
                cursor::and_leapfrog(&mut c, &mut out);
                black_box(&out);
            }
        }
    }) / 10.0;
    println!("AND warm (each query 10x in a row): Loom count {:.4}, PEF leapfrog {:.4}", ms(t_warm), ms(t_warm_pef));
    let t_andc = best_of(reps, || {
        for q in &qs {
            black_box(Loom::and_count(q));
        }
    });
    let t_lf = best_of(reps, || {
        for q in &qs {
            out.clear();
            let mut c: Vec<_> = q.iter().map(|l| l.cursor()).collect();
            cursor::and_leapfrog(&mut c, &mut out);
            black_box(&out);
        }
    });
    let t_pef = best_of(reps, || {
        for q in &qp {
            out.clear();
            let mut c: Vec<_> = q.iter().map(|l| l.cursor()).collect();
            cursor::and_leapfrog(&mut c, &mut out);
            black_box(&out);
        }
    });
    println!(
        "AND ms/query: Loom native {:.4}, count {:.4}, Loom leapfrog {:.4}, PEF leapfrog {:.4}",
        ms(t_and),
        ms(t_andc),
        ms(t_lf),
        ms(t_pef)
    );
    let t_or = best_of(reps, || {
        for q in &qs {
            out.clear();
            Loom::union(q, &mut out);
            black_box(&out);
        }
    });
    let t_orc = best_of(reps, || {
        for q in &qs {
            black_box(Loom::or_count(q));
        }
    });
    let t_dec = best_of(reps, || {
        for q in &qs {
            for l in q {
                out.clear();
                l.decode_into(&mut out);
                black_box(&out);
            }
        }
    });
    println!("OR ms/query: Loom native {:.4}, count {:.4}; decode all query lists {:.4}", ms(t_or), ms(t_orc), ms(t_dec));
    let t_drv = best_of(reps, || {
        for q in &qs {
            out.clear();
            q.iter().min_by_key(|l| l.len()).unwrap().decode_into(&mut out);
            black_box(&out);
        }
    });
    let drv: usize = qs.iter().map(|q| q.iter().map(|l| l.len()).min().unwrap()).sum();
    println!("decode driver only {:.4} ms/query ({:.0} postings/query)", ms(t_drv), drv as f64 / nq);

    let sizes: Vec<u64> = qs.iter().map(|q| Loom::or_count(q)).collect();
    let t_w = best_of(reps, || {
        for &s in &sizes {
            out.clear();
            out.extend(0..s as u32);
            black_box(&out);
        }
    });
    println!("writing the OR results alone: {:.4} ms/query", ms(t_w));
}
