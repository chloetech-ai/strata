//! Generic correctness checks shared by every codec's unit tests.
//!
//! Each codec module calls [`check_roundtrip`], plus [`check_cursor`] and/or
//! [`check_setops`] depending on which traits it implements.

use super::{Cursor, CursorList, PostingList, SetOps};
use crate::END;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::collections::BTreeSet;

/// A uniformly random sorted set of about `n` distinct docIDs in `[0, universe)`.
pub fn random_sorted(rng: &mut StdRng, n: usize, universe: u32) -> Vec<u32> {
    let n = n.min(universe as usize);
    if n as u64 * 2 >= universe as u64 {
        let p = n as f64 / universe as f64;
        return (0..universe).filter(|_| rng.gen_bool(p)).collect();
    }
    let mut set = BTreeSet::new();
    while set.len() < n {
        set.insert(rng.gen_range(0..universe));
    }
    set.into_iter().collect()
}

/// About `n` docIDs grouped in runs of 1..=32 consecutive values.
pub fn clustered(rng: &mut StdRng, n: usize, universe: u32) -> Vec<u32> {
    let mut set = BTreeSet::new();
    while set.len() < n {
        let start = rng.gen_range(0..universe);
        let run = rng.gen_range(1..=32u32);
        for d in start..start.saturating_add(run).min(universe) {
            set.insert(d);
        }
    }
    set.into_iter().collect()
}

/// Edge-case and random lists, each with its universe.
pub fn sample_lists() -> Vec<(Vec<u32>, u32)> {
    let mut rng = StdRng::seed_from_u64(42);
    let mut out: Vec<(Vec<u32>, u32)> = vec![
        (vec![], 1000),
        (vec![0], 1),
        (vec![5], 6),
        (vec![u32::MAX - 1], u32::MAX),
        ((0..128).collect(), 128),
        ((0..129).collect(), 1000),
        ((0..127).map(|x| x * 3).collect(), 1000),
        ((0..100_000).collect(), 100_000),
        ((0..70_000).map(|i| i * 2 + 1).collect(), 150_000),
        (vec![0, 1, 1 << 20, (1 << 31) + 5, u32::MAX - 2, u32::MAX - 1], u32::MAX),
    ];
    for (n, u) in [
        (10, 1_000_000),
        (255, 1000),
        (256, 300),
        (257, 1 << 20),
        (1000, 1 << 31),
        (5000, 6000),
        (20_000, 50_000_000),
        (100_000, 200_000),
        (200_000, u32::MAX),
    ] {
        out.push((random_sorted(&mut rng, n, u), u));
    }
    out.push((clustered(&mut rng, 50_000, 10_000_000), 10_000_000));
    // Dense prefix, sparse middle, strided tail: exercises encodings that
    // switch representation per block / partition / container.
    let mut mixed: Vec<u32> = (0..70_000).collect();
    mixed.extend(random_sorted(&mut rng, 3_000, 5_000_000).into_iter().map(|x| x + 100_000));
    mixed.extend((6_000_000..6_009_000).step_by(3));
    out.push((mixed, 8_000_000));
    out
}

/// Groups of lists over a shared universe, for set-operation tests.
pub fn sample_groups() -> Vec<(Vec<Vec<u32>>, u32)> {
    let mut rng = StdRng::seed_from_u64(4242);
    let mut groups = Vec::new();
    for universe in [1000u32, 70_000, 1 << 20, 50_000_000] {
        let mut lists = Vec::new();
        for density in [0.5, 0.1, 0.01, 0.001, 0.0001] {
            let n = ((universe as f64 * density) as usize).max(1);
            lists.push(random_sorted(&mut rng, n, universe));
        }
        let n = (universe as usize / 20).max(1);
        lists.push(clustered(&mut rng, n, universe));
        lists.push((0..universe.min(200_000)).collect());
        groups.push((lists, universe));
    }
    groups
}

fn reference_intersection(lists: &[&Vec<u32>]) -> Vec<u32> {
    let mut acc = lists[0].clone();
    for l in &lists[1..] {
        acc.retain(|x| l.binary_search(x).is_ok());
    }
    acc
}

fn reference_union(lists: &[&Vec<u32>]) -> Vec<u32> {
    let set: BTreeSet<u32> = lists.iter().flat_map(|l| l.iter().copied()).collect();
    set.into_iter().collect()
}

/// `build` + `decode_into` + `len` round-trip on every sample list.
pub fn check_roundtrip<C: PostingList>() {
    for (docs, universe) in sample_lists() {
        let c = C::build(&docs, universe);
        assert_eq!(c.len(), docs.len(), "{}: len (n={})", C::NAME, docs.len());
        let mut out = Vec::new();
        c.decode_into(&mut out);
        assert!(out == docs, "{}: decode mismatch (n={}, u={universe})", C::NAME, docs.len());
        if !docs.is_empty() {
            assert!(c.size_bits() > 0, "{}: size_bits must be positive", C::NAME);
        }
    }
}

/// Full traversal with `next`, and random monotone `next_geq` sequences
/// interleaved with `next`, compared against binary search on the input.
pub fn check_cursor<C: CursorList>() {
    let mut rng = StdRng::seed_from_u64(1);
    for (docs, universe) in sample_lists() {
        let c = C::build(&docs, universe);

        let mut cur = c.cursor();
        assert_eq!(cur.size(), docs.len(), "{}: cursor size", C::NAME);
        let mut got = Vec::new();
        let mut d = cur.docid();
        while d != END {
            got.push(d);
            d = cur.next();
        }
        assert!(got == docs, "{}: next() traversal mismatch (n={})", C::NAME, docs.len());
        assert_eq!(cur.next(), END);
        assert_eq!(cur.next_geq(0), END);

        for step in [1usize, 3, 50, 1000, 100_000] {
            let mut cur = c.cursor();
            let mut pos = 0usize; // index of the cursor's current posting
            for _ in 0..4 * docs.len() + 16 {
                // Target near the posting about `step` positions ahead: exactly
                // on it, just before it, or just after it; occasionally a
                // random value in a wide window, or past the end.
                let j = pos + rng.gen_range(0..=step);
                let t: u32 = if rng.gen_bool(0.1) {
                    let lo = docs.get(pos).copied().unwrap_or(0) as u64;
                    let hi = (lo + universe as u64 / 16 + 1).min(u32::MAX as u64 - 1);
                    rng.gen_range(lo..=hi) as u32
                } else if j < docs.len() {
                    match rng.gen_range(0..3) {
                        0 => docs[j],
                        1 => docs[j].saturating_sub(rng.gen_range(1..=3)),
                        _ => docs[j].saturating_add(1).min(u32::MAX - 1),
                    }
                } else {
                    docs.last().map_or(0, |&l| l.saturating_add(1).min(u32::MAX - 1))
                };
                pos = pos.max(docs.partition_point(|&x| x < t));
                let expected = docs.get(pos).copied().unwrap_or(END);
                assert_eq!(
                    cur.next_geq(t),
                    expected,
                    "{}: next_geq({t}) (n={}, u={universe}, step={step})",
                    C::NAME,
                    docs.len()
                );
                assert_eq!(cur.docid(), expected);
                if expected == END {
                    break;
                }
                if rng.gen_bool(0.25) {
                    pos += 1;
                    let e = docs.get(pos).copied().unwrap_or(END);
                    assert_eq!(cur.next(), e, "{}: next() after next_geq", C::NAME);
                    if e == END {
                        break;
                    }
                }
            }
        }

        // A single long jump straight to the last element, then past it.
        if let Some(&last) = docs.last() {
            let mut cur = c.cursor();
            assert_eq!(cur.next_geq(last), last);
            assert_eq!(cur.next(), END);
            let mut cur = c.cursor();
            if last < u32::MAX - 1 {
                assert_eq!(cur.next_geq(last + 1), END);
            }
        }
    }
}

/// Native intersection/union over many random subsets of list groups.
pub fn check_setops<C: SetOps>() {
    let mut rng = StdRng::seed_from_u64(3);
    for (lists, universe) in sample_groups() {
        let built: Vec<C> = lists.iter().map(|l| C::build(l, universe)).collect();
        for _ in 0..60 {
            let k = rng.gen_range(1..=4);
            let idx: Vec<usize> = (0..k).map(|_| rng.gen_range(0..lists.len())).collect();
            let refs: Vec<&Vec<u32>> = idx.iter().map(|&i| &lists[i]).collect();
            let codec_refs: Vec<&C> = idx.iter().map(|&i| &built[i]).collect();

            let mut out = Vec::new();
            C::intersect(&codec_refs, &mut out);
            assert!(
                out == reference_intersection(&refs),
                "{}: intersect mismatch, u={universe}, lists={idx:?}",
                C::NAME
            );
            out.clear();
            C::union(&codec_refs, &mut out);
            assert!(
                out == reference_union(&refs),
                "{}: union mismatch, u={universe}, lists={idx:?}",
                C::NAME
            );
        }
    }
    // Single sample lists: intersect/union of a list with itself.
    for (docs, universe) in sample_lists() {
        let c = C::build(&docs, universe);
        let mut out = Vec::new();
        C::intersect(&[&c, &c], &mut out);
        assert!(out == docs, "{}: self-intersection (n={})", C::NAME, docs.len());
        out.clear();
        C::union(&[&c], &mut out);
        assert!(out == docs, "{}: single union (n={})", C::NAME, docs.len());
    }
}
