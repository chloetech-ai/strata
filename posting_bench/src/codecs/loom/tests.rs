use super::*;
use crate::codecs::conformance::{self, clustered, random_sorted};
use crate::codecs::pef::PartitionedEF;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::collections::BTreeSet;

fn reference_and(lists: &[&Vec<u32>]) -> Vec<u32> {
    let mut acc = lists[0].clone();
    for l in &lists[1..] {
        acc.retain(|x| l.binary_search(x).is_ok());
    }
    acc
}

fn reference_or(lists: &[&Vec<u32>]) -> Vec<u32> {
    let set: BTreeSet<u32> = lists.iter().flat_map(|l| l.iter().copied()).collect();
    set.into_iter().collect()
}

/// Check AND / OR (materialised and count-only) against the references.
fn check_ops(lists: &[&Vec<u32>], params: Params, what: &str) {
    let built: Vec<Loom> = lists.iter().map(|l| Loom::build_with(l, params)).collect();
    let refs: Vec<&Loom> = built.iter().collect();
    let (ea, eo) = (reference_and(lists), reference_or(lists));
    let mut out = vec![7, 7]; // results are appended
    Loom::intersect(&refs, &mut out);
    assert!(out[2..] == ea[..], "{what}: AND mismatch ({} vs {} docs)", out.len() - 2, ea.len());
    assert_eq!(Loom::and_count(&refs), ea.len() as u64, "{what}: AND count");
    out.truncate(2);
    Loom::union(&refs, &mut out);
    assert!(out[2..] == eo[..], "{what}: OR mismatch ({} vs {} docs)", out.len() - 2, eo.len());
    assert_eq!(Loom::or_count(&refs), eo.len() as u64, "{what}: OR count");
}

fn kinds(l: &Loom) -> [usize; 3] {
    l.partitions().fold([0; 3], |mut k, (_, _, kind, _)| {
        k[kind as usize] += 1;
        k
    })
}

#[test]
fn conformance() {
    conformance::check_roundtrip::<Loom>();
    conformance::check_cursor::<Loom>();
    conformance::check_setops::<Loom>();
}

#[test]
fn conformance_other_params() {
    // Coarser and finer grids, and bitvector-friendly λ. Grids below 8
    // postings make the DP quadratic-ish (4096 n / chunk^2 candidates), so
    // they only see the shorter lists.
    let mut rng = StdRng::seed_from_u64(5);
    for params in [
        Params { chunk: 32, lambda: (1, 1) },
        Params { chunk: 8, lambda: (2, 1) },
        Params { chunk: 2, lambda: (5, 4) },
    ] {
        let max_len = if params.chunk >= 8 { usize::MAX } else { 300_000 };
        for (docs, _) in conformance::sample_lists() {
            if docs.len() > max_len {
                continue;
            }
            let l = Loom::build_with(&docs, params);
            let mut out = Vec::new();
            l.decode_into(&mut out);
            assert!(out == docs, "{params:?}: decode");
            let mut c = l.cursor();
            for (i, &d) in docs.iter().enumerate().step_by(7) {
                assert_eq!(c.next_geq(d), d, "{params:?}: next_geq #{i}");
            }
        }
        for (lists, _) in conformance::sample_groups() {
            for _ in 0..10 {
                let k = rng.gen_range(1..=4);
                let sel: Vec<&Vec<u32>> =
                    (0..k).map(|_| &lists[rng.gen_range(0..lists.len())]).filter(|l| l.len() <= max_len).collect();
                if !sel.is_empty() {
                    check_ops(&sel, params, &format!("{params:?}"));
                }
            }
        }
    }
}

/// Bitvector partitions start on a docID word: bit `i` of the payload is
/// docID `abase + i` with `abase % 64 == 0`, so equal docID words of two
/// lists sit at equal bit offsets.
#[test]
fn bitvectors_are_word_aligned() {
    let mut rng = StdRng::seed_from_u64(21);
    // Dense random regions starting at odd offsets.
    let mut a: Vec<u32> = Vec::new();
    let mut b: Vec<u32> = Vec::new();
    for r in 0..20u32 {
        let start = r * 100_003 + 17 + r;
        a.extend(random_sorted(&mut rng, 3000, 9000).into_iter().map(|x| x + start));
        b.extend(random_sorted(&mut rng, 2500, 9000).into_iter().map(|x| x + start + 5));
        a.push(start + 50_000); // isolated postings between the regions
        b.push(start + 60_000);
    }
    a.sort_unstable();
    a.dedup();
    b.sort_unstable();
    b.dedup();
    let (la, lb) = (Loom::build_with(&a, Params::DEFAULT), Loom::build_with(&b, Params::DEFAULT));
    for l in [&la, &lb] {
        assert!(kinds(l)[BV as usize] >= 20, "{:?}", kinds(l));
        for p in 0..l.num_partitions() {
            let part = Part::load(l, p);
            if part.kind != BV {
                continue;
            }
            assert_eq!(part.abase % 64, 0);
            assert!(part.abase <= part.base && part.base - part.abase < 64);
            // Every bit of the payload is the membership of docID abase + i.
            let words = ((part.last - part.abase) / 64 + 1) as usize;
            for i in 0..words * 64 {
                let d = part.abase as u64 + i as u64;
                let bit = unsafe { *part.bits.add(i / 64) >> (i % 64) & 1 == 1 };
                let inside = d >= part.base as u64 && d <= part.last as u64;
                let member = inside && l_contains(l, d as u32);
                assert_eq!(bit, member, "partition {p}, docID {d}");
            }
        }
    }
    // Word-by-word AND of overlapping bitvector partitions is the
    // intersection over the overlap, without shifting.
    for pa in (0..la.num_partitions()).map(|p| Part::load(&la, p)).filter(|p| p.kind == BV) {
        for pb in (0..lb.num_partitions()).map(|p| Part::load(&lb, p)).filter(|p| p.kind == BV) {
            let lo = pa.base.max(pb.base);
            let hi = pa.last.min(pb.last);
            if lo > hi {
                continue;
            }
            for w in (lo / 64)..=(hi / 64) {
                let x = unsafe { *pa.bits.add((w - pa.abase / 64) as usize) & *pb.bits.add((w - pb.abase / 64) as usize) };
                for bit in 0..64 {
                    let d = w * 64 + bit;
                    if d < lo || d > hi {
                        continue;
                    }
                    let both = a.binary_search(&d).is_ok() && b.binary_search(&d).is_ok();
                    assert_eq!(x >> bit & 1 == 1, both, "docID {d}");
                }
            }
        }
    }
    check_ops(&[&a, &b], Params::DEFAULT, "aligned regions");
}

fn l_contains(l: &Loom, d: u32) -> bool {
    let mut c = l.cursor();
    c.next_geq(d) == d
}

/// Bulk (word-parallel) AND: driver bitvector / run partitions against
/// bitvector, run and EF partitions of the other lists, overlapping in
/// every way, with partition edges inside words and lists that end inside
/// a bulk step.
#[test]
fn bulk_and_mixed_partitions() {
    let mut rng = StdRng::seed_from_u64(31);
    let mut checked = 0;
    for round in 0..40 {
        let mut lists: Vec<Vec<u32>> = Vec::new();
        for li in 0..rng.gen_range(2..=4) {
            let mut set = BTreeSet::new();
            let mut pos = rng.gen_range(0..300u32);
            while pos < 200_000 {
                let len = rng.gen_range(50..6000u32);
                match rng.gen_range(0..4) {
                    0 => set.extend(pos..pos + len), // run
                    1 => set.extend((pos..pos + len).filter(|_| rng.gen_bool(0.6))), // dense: bitvector
                    2 => set.extend((pos..pos + len).filter(|_| rng.gen_bool(0.02))), // sparse: EF
                    _ => {}                                                         // gap
                }
                pos += len + rng.gen_range(0..200);
            }
            if li == 0 && round % 3 == 0 {
                // A list ending early (inside another list's bitvector).
                let cut = rng.gen_range(1000..150_000);
                set.retain(|&d| d < cut);
            }
            if set.is_empty() {
                set.insert(5);
            }
            lists.push(set.into_iter().collect());
        }
        let refs: Vec<&Vec<u32>> = lists.iter().collect();
        for params in [Params::DEFAULT, Params { chunk: 8, lambda: (3, 2) }] {
            let built: Vec<Loom> = lists.iter().map(|l| Loom::build_with(l, params)).collect();
            let k: Vec<[usize; 3]> = built.iter().map(kinds).collect();
            assert!(k.iter().all(|k| k[RUN as usize] > 0 || k[BV as usize] > 0), "{k:?}");
            check_ops(&refs, params, &format!("round {round}"));
            checked += 1;
        }
        // Pairs and the list with itself.
        check_ops(&[refs[0], refs[1]], Params::DEFAULT, "pair");
        check_ops(&[refs[0], refs[0]], Params::DEFAULT, "self");
    }
    assert_eq!(checked, 80);
}

/// Union windows: postings on both sides of window edges, partitions of
/// every kind crossing them, and the last window below u32::MAX.
#[test]
fn union_window_boundaries() {
    let w = or::WIN as u32;
    let mut lists: Vec<Vec<u32>> = vec![
        // Single postings on window edges.
        vec![0, w - 1, w, 2 * w - 1, 2 * w, 5 * w + 63, 5 * w + 64],
        // A run crossing two window edges.
        (w - 100..3 * w + 100).collect(),
        // A dense bitvector crossing a window edge, starting mid-word.
        (4 * w - 3001..4 * w + 2999).filter(|d| d % 3 != 0).collect(),
        // A sparse EF partition crossing many windows.
        (0..200u32).map(|i| i * 7919 + 13).collect(),
        // The top of the docID range: last window below u32::MAX.
        (u32::MAX - 5000..u32::MAX).filter(|d| d % 2 == 0).collect(),
        vec![u32::MAX - 2, u32::MAX - 1],
        (u32::MAX - 70_000..u32::MAX - 1).collect(),
        vec![1, u32::MAX - 1],
    ];
    let mut rng = StdRng::seed_from_u64(41);
    // Random lists concentrated around window edges.
    for _ in 0..6 {
        let mut set = BTreeSet::new();
        for k in 1..6u32 {
            let c = k * w;
            for d in c - 300..c + 300 {
                if rng.gen_bool(0.4) {
                    set.insert(d);
                }
            }
        }
        lists.push(set.into_iter().collect());
    }
    for i in 0..lists.len() {
        for j in 0..lists.len() {
            check_ops(&[&lists[i], &lists[j]], Params::DEFAULT, &format!("lists {i}, {j}"));
        }
    }
    let all: Vec<&Vec<u32>> = lists.iter().collect();
    check_ops(&all, Params::DEFAULT, "all");
    check_ops(&all[..8], Params { chunk: 1, lambda: (4, 1) }, "all, λ = 4, chunk 1");
    // The top-window lists contain u32::MAX - 1, the largest docID.
    let top = Loom::build(&lists[5], u32::MAX);
    let mut out = Vec::new();
    Loom::union(&[&top, &top], &mut out);
    assert_eq!(out, [u32::MAX - 2, u32::MAX - 1]);
}

/// Unions (and intersections) where one list alone covers long stretches
/// of runs, bitvectors and EF partitions, separated by sparse postings of
/// other lists (equal or adjacent to its own), and continues alone to the
/// end.
#[test]
fn union_single_list_stretches() {
    let mut rng = StdRng::seed_from_u64(81);
    let mut a: Vec<u32> = Vec::new();
    let mut pos = 10u32;
    for _ in 0..60 {
        let len = rng.gen_range(100..20_000u32);
        match rng.gen_range(0..3) {
            0 => a.extend(pos..pos + len),
            1 => a.extend((pos..pos + len).filter(|_| rng.gen_bool(0.5))),
            _ => a.extend((pos..pos + len).filter(|_| rng.gen_bool(0.01))),
        }
        pos += len + rng.gen_range(0..5000);
    }
    a.dedup();
    let last = *a.last().unwrap();
    // Sparse separators, some equal or adjacent to postings of `a`.
    let mut b: Vec<u32> = (0..40).map(|_| rng.gen_range(0..last / 2)).collect();
    b.extend(a.iter().step_by(9973).flat_map(|&d| [d, d + 1, d.saturating_sub(1)]));
    b.sort_unstable();
    b.dedup();
    let c: Vec<u32> = vec![last / 3, last / 3 + 64, last / 3 + 128];
    check_ops(&[&a, &b], Params::DEFAULT, "a, b");
    check_ops(&[&b, &a], Params::DEFAULT, "b, a");
    check_ops(&[&a, &c], Params::DEFAULT, "a, c");
    check_ops(&[&a, &b, &c], Params::DEFAULT, "a, b, c");
    check_ops(&[&c, &b], Params::DEFAULT, "c, b");
}

/// Randomised differential test over clustered and uniform collections.
#[test]
fn random_queries_match_reference() {
    let mut rng = StdRng::seed_from_u64(51);
    let u = 2_000_000;
    let mut lists: Vec<Vec<u32>> = Vec::new();
    for n in [300, 1000, 5000, 20_000, 100_000, 400_000, 900_000] {
        lists.push(random_sorted(&mut rng, n, u));
        lists.push(clustered(&mut rng, n, u));
    }
    for _ in 0..150 {
        let k = rng.gen_range(2..=5);
        let sel: Vec<&Vec<u32>> = (0..k).map(|_| &lists[rng.gen_range(0..lists.len())]).collect();
        check_ops(&sel, Params::DEFAULT, "random");
    }
}

/// Every way the AND filter can treat an EF partition (one probe stream,
/// two or four lockstep streams, independent far probes, decode + SIMD
/// merge) gives the same result, at size ratios from 1 to 1000 and with
/// candidates both denser and sparser than the probed list.
#[test]
fn every_ef_filter_mode_is_correct() {
    use std::sync::atomic::Ordering;
    let mut rng = StdRng::seed_from_u64(71);
    let u = 3_000_000;
    let mut lists: Vec<Vec<u32>> = Vec::new();
    for n in [800, 3000, 12_000, 40_000, 150_000, 600_000] {
        lists.push(random_sorted(&mut rng, n, u));
        lists.push(clustered(&mut rng, n, u));
    }
    // A list with long runs and dense stretches (bulk mode drivers).
    let mut dense: Vec<u32> = (100_000..104_000).collect();
    dense.extend(random_sorted(&mut rng, 20_000, 60_000).into_iter().map(|d| d + 500_000));
    lists.push(dense);
    let mut queries: Vec<Vec<usize>> = Vec::new();
    for _ in 0..40 {
        let k = rng.gen_range(2..=4);
        queries.push((0..k).map(|_| rng.gen_range(0..lists.len())).collect());
    }
    for mode in 0..6u8 {
        and::FORCE_MODE.store(mode, Ordering::Relaxed);
        for q in &queries {
            let sel: Vec<&Vec<u32>> = q.iter().map(|&i| &lists[i]).collect();
            check_ops(&sel, Params::DEFAULT, &format!("mode {mode}, lists {q:?}"));
        }
    }
    and::FORCE_MODE.store(0, Ordering::Relaxed);
}

/// Loom pays for alignment and its upper level, but on these lists stays
/// within 2% of PEF (and below it on clustered lists).
#[test]
fn space_close_to_pef() {
    let mut rng = StdRng::seed_from_u64(61);
    for (name, docs, u) in [
        ("uniform 100k / 10M", random_sorted(&mut rng, 100_000, 10_000_000), 10_000_000u32),
        ("uniform 1M / 4M", random_sorted(&mut rng, 1_000_000, 4_000_000), 4_000_000),
        ("clustered 300k / 10M", clustered(&mut rng, 300_000, 10_000_000), 10_000_000),
    ] {
        let (l, p) = (Loom::build(&docs, u), PartitionedEF::build(&docs, u));
        let ratio = l.size_bits() as f64 / p.size_bits() as f64;
        assert!(ratio < 1.02, "{name}: Loom / PEF = {ratio:.4}");
    }
}

/// A list read back from its bytes is a zero-copy view that decodes,
/// seeks, ranks and combines exactly as the built list.
#[test]
fn bytes_roundtrip_zero_copy() {
    use crate::bytes::{ByteReader, ByteWriter, OwnedBytes};
    let lists = conformance::sample_lists();
    // Several lists in one buffer, as an index stores them.
    let built: Vec<Loom> = lists.iter().map(|(d, u)| Loom::build(d, *u)).collect();
    let mut w = ByteWriter::new();
    for l in &built {
        l.write_to(&mut w);
    }
    let mut r = ByteReader::new(OwnedBytes::new(w.finish()));
    let views: Vec<Loom> = built.iter().map(|_| Loom::read_from(&mut r).unwrap()).collect();
    for ((docs, _), (b, v)) in lists.iter().zip(built.iter().zip(&views)) {
        assert!(v.is_shared() && !b.is_shared());
        assert_eq!((v.len(), v.size_bits(), v.num_partitions()), (b.len(), b.size_bits(), b.num_partitions()));
        let mut out = Vec::new();
        v.decode_into(&mut out);
        assert!(out == *docs, "decode of a view");
        let mut c = v.cursor();
        for (i, &d) in docs.iter().enumerate().step_by(5) {
            assert_eq!(c.next_geq(d), d);
            assert_eq!(c.rank(), i);
        }
    }
    // Set operations mixing built lists and views.
    for (lists, u) in conformance::sample_groups() {
        let built: Vec<Loom> = lists.iter().map(|l| Loom::build(l, u)).collect();
        let views: Vec<Loom> = built.iter().map(|l| Loom::from_bytes(OwnedBytes::new(l.to_bytes())).unwrap()).collect();
        for k in 1..=lists.len().min(4) {
            let refs: Vec<&Vec<u32>> = lists[..k].iter().collect();
            let mixed: Vec<&Loom> = (0..k).map(|i| if i % 2 == 0 { &views[i] } else { &built[i] }).collect();
            let mut out = Vec::new();
            Loom::intersect(&mixed, &mut out);
            assert!(out == reference_and(&refs), "AND of views");
            out.clear();
            Loom::union(&mixed, &mut out);
            assert!(out == reference_or(&refs), "OR of views");
        }
    }
    // Truncated or inconsistent bytes are rejected.
    let good = built.iter().find(|l| l.len() > 1000).unwrap().to_bytes();
    assert!(Loom::from_bytes(OwnedBytes::new(good[..good.len() - 8].to_vec())).is_err());
    let mut bad = good.clone();
    bad[0] ^= 1; // n no longer matches the ranks
    assert!(Loom::from_bytes(OwnedBytes::new(bad)).is_err());
}

/// `rank` is the posting's index after any mix of `next` and `next_geq`,
/// in every partition kind, for borrowed and owning cursors.
#[test]
fn rank_matches_index() {
    let mut rng = StdRng::seed_from_u64(91);
    let mut lists: Vec<Vec<u32>> = conformance::sample_lists().into_iter().map(|(d, _)| d).collect();
    // Long bitvector partitions (ranks counted over many words).
    lists.push((0..400_000u32).filter(|_| rng.gen_bool(0.3)).collect());
    for params in [Params::DEFAULT, Params { chunk: 8, lambda: (2, 1) }] {
        for docs in lists.iter().filter(|d| !d.is_empty()) {
            let l = Loom::build_with(docs, params);
            let mut c = l.cursor();
            for (i, &d) in docs.iter().enumerate() {
                assert_eq!((c.docid(), c.rank()), (d, i));
                c.next();
            }
            assert_eq!((c.docid(), c.rank()), (END, docs.len()));
            let gap = (*docs.last().unwrap() / docs.len() as u32).saturating_mul(4).max(1);
            for _ in 0..3 {
                let mut c = l.clone().into_cursor();
                loop {
                    if rng.gen_bool(0.3) {
                        c.next();
                    } else {
                        c.next_geq(c.docid().saturating_add(rng.gen_range(1..=gap)));
                    }
                    let d = c.docid();
                    if d == END {
                        assert_eq!(c.rank(), docs.len());
                        break;
                    }
                    if rng.gen_bool(0.7) {
                        assert_eq!(c.rank(), docs.partition_point(|&x| x < d), "docID {d}");
                    }
                }
            }
        }
    }
}
