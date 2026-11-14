//! The substring index against brute force over the same values.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::like::{self, Like, Plan, Stats};
use super::*;
use crate::segment::{DocId, FieldIndexBuilder, SegmentFile, SegmentWriter};
use crate::value::Value;

/// A segment image with the index of `values` (`None`: no value), built
/// with `sample` and `part_bytes`.
fn image(values: &[Option<String>], sample: u32, part_bytes: usize) -> Vec<u8> {
    let mut b = Box::new(SubstringBuilder::new("f1", sample, part_bytes));
    for (d, v) in values.iter().enumerate() {
        b.add(d as DocId, &v.clone().map_or(Value::Null, Value::Str)).unwrap();
    }
    let mut w = SegmentWriter::new(values.len());
    b.finish(&mut w).unwrap();
    w.finish()
}

fn index(values: &[Option<String>], sample: u32, part_bytes: usize) -> SubstringIndex {
    let seg = SegmentFile::from_vec(image(values, sample, part_bytes)).unwrap();
    SubstringIndex::open(&seg, "f1").unwrap().expect("an index")
}

/// Random values over one of a few alphabets, some without a value.
fn values(rng: &mut StdRng) -> Vec<Option<String>> {
    let alphabets: [&[char]; 5] = [&['a', 'b'], &['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', ' ', '\n'], &['a', 'A', 'é', 'É', '東', '京', 'ไ', 'ท', ' ', ',', '.', 'x', 'ß', 'İ', 'i', 'I', '\u{307}', 'Σ', 'σ', 'ς'], &['\0', '\u{1}', '\u{7f}', '%', '_'], &['x']];
    let alpha = alphabets[rng.gen_range(0..alphabets.len())];
    let skew = rng.gen_range(1..4);
    (0..rng.gen_range(1..40))
        .map(|_| {
            if rng.gen_bool(0.15) {
                return None;
            }
            let len = [0, 1, 2, rng.gen_range(0..30), rng.gen_range(0..200)][rng.gen_range(0..5)];
            Some((0..len).map(|_| alpha[(alpha.len() as f64 * rng.gen::<f64>().powi(skew)) as usize]).collect())
        })
        .collect()
}

fn occurrences(hay: &[u8], needle: &[u8]) -> u64 {
    if needle.is_empty() || needle.len() > hay.len() {
        return 0;
    }
    hay.windows(needle.len()).filter(|w| *w == needle).count() as u64
}

/// The text of the values as one part holds it, and the transform of it.
fn text_of(values: &[Option<String>]) -> Vec<u8> {
    let mut t = vec![SEP];
    for v in values.iter().flatten() {
        t.extend_from_slice(v.as_bytes());
        t.push(SEP);
    }
    t
}

fn transform_of(t: &[u8]) -> (Vec<u8>, u64) {
    let mut order: Vec<usize> = (0..=t.len()).collect();
    order.sort_by(|&a, &b| t[a..].cmp(&t[b..]));
    let primary = order.iter().position(|&p| p == 0).unwrap() as u64;
    (order.iter().filter(|&&p| p > 0).map(|&p| t[p - 1]).collect(), primary)
}

#[test]
fn the_transform_counts_and_documents_are_brute_force() {
    for seed in 0..300 {
        let mut rng = StdRng::seed_from_u64(seed);
        let vals = values(&mut rng);
        let sample = [1, 2, 4, 8, 32][rng.gen_range(0..5)];
        let present: Vec<(DocId, &String)> = vals.iter().enumerate().filter_map(|(d, v)| v.as_ref().map(|v| (d as DocId, v))).collect();
        // One part: the transform itself.
        let ix = index(&vals, sample, MAX_PART_BYTES);
        if present.is_empty() {
            assert!(ix.parts().is_empty());
            continue;
        }
        let t = text_of(&vals);
        let (l, _) = transform_of(&t);
        assert_eq!(ix.parts().len(), 1);
        assert_eq!(ix.parts()[0].transform(), l, "seed {seed}");
        assert_eq!((ix.docs(), ix.bytes()), (present.len() as u64, present.iter().map(|(_, v)| v.len() as u64).sum()));
        // Several parts: counts and documents.
        let ix = index(&vals, sample, [1, 16, 64, 300][rng.gen_range(0..4)]);
        for _ in 0..40 {
            let src = present[rng.gen_range(0..present.len())].1.as_bytes();
            let a = rng.gen_range(0..=src.len());
            let b = rng.gen_range(a..=src.len().min(a + 6));
            let kind = rng.gen_range(0..5);
            let body: &[u8] = match kind {
                1 => &src[..b - a],
                2 => &src[src.len() - (b - a)..],
                3 => src,
                4 => b"ab\0",
                _ => &src[a..b],
            };
            // Cut at a character boundary, as a pattern is.
            let Ok(body) = std::str::from_utf8(body) else { continue };
            let mut pat = Vec::new();
            pat.extend((kind == 1 || kind == 3).then_some(SEP));
            pat.extend_from_slice(body.as_bytes());
            pat.extend((kind == 2 || kind == 3).then_some(SEP));
            // The separator alone is no pattern.
            if body.is_empty() && kind != 3 {
                continue;
            }
            let holds = |v: &str| match kind {
                1 => v.starts_with(body),
                2 => v.ends_with(body),
                3 => v == body,
                _ => v.contains(body),
            };
            let want_docs: Vec<DocId> = present.iter().filter(|(_, v)| holds(v)).map(|(d, _)| *d).collect();
            let want_count: u64 = match kind {
                1..=3 => want_docs.len() as u64,
                _ => present.iter().map(|(_, v)| occurrences(v.as_bytes(), body.as_bytes())).sum(),
            };
            let mut got_docs = Vec::new();
            let mut got_count = 0;
            for part in ix.parts() {
                let (lo, hi) = part.search(&pat).unwrap();
                got_count += hi - lo;
                for row in lo..hi {
                    got_docs.push(part.doc(part.ordinal(row).unwrap()).unwrap().unwrap());
                }
            }
            got_docs.sort();
            got_docs.dedup();
            assert_eq!(got_count, want_count, "seed {seed} pattern {pat:?}");
            assert_eq!(got_docs, want_docs, "seed {seed} pattern {pat:?}");
            if kind == 0 && !body.is_empty() {
                assert_eq!(ix.count(body.as_bytes()).unwrap(), want_count);
            }
        }
    }
}

/// A pattern made from a value: pieces of it joined by wildcards, some
/// letters changed in case, or a pattern of its own.
fn pattern(rng: &mut StdRng, vals: &[Option<String>]) -> String {
    let present: Vec<&String> = vals.iter().flatten().collect();
    if present.is_empty() || rng.gen_bool(0.1) {
        return ["%", "", "_", "%_%", "a%", "%a", "__", "%ab%", "x_x"][rng.gen_range(0..9)].to_string();
    }
    let src: Vec<char> = present[rng.gen_range(0..present.len())].chars().collect();
    let mut out = String::new();
    if rng.gen_bool(0.6) {
        out.push('%');
    }
    let mut at = if rng.gen_bool(0.5) { 0 } else { rng.gen_range(0..=src.len()) };
    for _ in 0..rng.gen_range(1..4) {
        let end = rng.gen_range(at..=src.len().min(at + 5));
        for &c in &src[at..end] {
            match rng.gen_range(0..12) {
                0 => out.push('_'),
                1 => out.extend(c.to_uppercase()),
                2 => out.extend(c.to_lowercase()),
                _ if c == '%' || c == '_' => out.push('x'),
                _ => out.push(c),
            }
        }
        at = end;
        if rng.gen_bool(0.5) {
            out.push(if rng.gen_bool(0.8) { '%' } else { '_' });
            at = rng.gen_range(at..=src.len());
        }
    }
    if rng.gen_bool(0.3) && at < src.len() {
        out.extend(&src[at..]);
    }
    out
}

/// The row evaluator's answer, written out.
fn reference(pattern: &str, ci: bool, value: &str) -> bool {
    let fold = |s: &str| if ci { crate::query::ir::eval::fold_case(s) } else { s.to_string() };
    crate::query::ir::eval::like(&fold(value).chars().collect::<Vec<_>>(), &fold(pattern).chars().collect::<Vec<_>>())
}

#[test]
fn like_and_ilike_from_the_index_are_the_row_evaluators() {
    let mut answered = Stats::default();
    for seed in 0..400 {
        let mut rng = StdRng::seed_from_u64(1000 + seed);
        let vals = values(&mut rng);
        let ix = index(&vals, [1, 3, 32][rng.gen_range(0..3)], [8, 64, MAX_PART_BYTES][rng.gen_range(0..3)]);
        for _ in 0..30 {
            let (p, ci) = (pattern(&mut rng, &vals), rng.gen_bool(0.5));
            let like = Like::new(&p, ci);
            let want: Vec<DocId> = vals.iter().enumerate().filter(|(_, v)| v.as_ref().is_some_and(|v| reference(&p, ci, v))).map(|(d, _)| d as DocId).collect();
            for v in vals.iter().flatten() {
                assert_eq!(like.matches(v), reference(&p, ci, v), "{p:?} ci {ci} on {v:?}");
            }
            let mut value = |d: DocId| Ok(vals[d as usize].clone());
            let locate = [0, 16, like::LOCATE_BYTES][rng.gen_range(0..3)];
            let got = like::eval(&ix, &like, vals.len() as u32, &mut value, locate, &mut answered).unwrap();
            assert_eq!(got.to_sorted(), want, "seed {seed}: {p:?} ci {ci}");
            assert_eq!(like::scan(&like, vals.len() as u32, &mut value).unwrap().to_sorted(), want);
        }
    }
    // Each way of answering was taken.
    assert!(answered.exact > 0 && answered.tested > 0 && answered.scanned > 0 && answered.located > 0, "{answered:?}");
}

#[test]
fn ilike_finds_the_capitals_the_lowercase_hides() {
    let vals: Vec<Option<String>> = ["İstanbul", "istanbul", "ISTANBUL", "ΟΔΟΣ", "οδος", "ΟΔΟΣ ΑΘΗΝΑΣ", "Straße", "STRASSE", "ǅ ǆ Ǆ", "ﬁle"].iter().map(|s| Some(s.repeat(40))).collect();
    let ix = index(&vals, 4, MAX_PART_BYTES);
    for p in ["%İstanbul%", "%istanbul%", "%i\u{307}stanbul%", "%tanbul", "%ΟΔΟΣ", "%οδος%", "%οδοσ %", "%straße%", "%STRASSE%", "%ǆ%", "%ǅ %", "%ﬁle%", "i%", "%ς"] {
        let like = Like::new(p, true);
        let want: Vec<DocId> = vals.iter().enumerate().filter(|(_, v)| reference(p, true, v.as_ref().unwrap())).map(|(d, _)| d as DocId).collect();
        let mut stats = Stats::default();
        let got = like::eval(&ix, &like, vals.len() as u32, &mut |d| Ok(vals[d as usize].clone()), 0, &mut stats).unwrap();
        assert_eq!(got.to_sorted(), want, "{p:?}");
    }
    // A capital İ lowercases to an i and a dot above: the pattern's i is
    // searched as the capital too, and the dot is where its literal is cut.
    let Plan::Pieces(pieces) = Like::new("%i\u{307}s%", true).plan() else { panic!("pieces") };
    assert_eq!(pieces.len(), 2);
    assert!(pieces[0].units[0].contains(&"İ".as_bytes().to_vec()) && pieces[0].units[0].contains(&b"I".to_vec()));
}

/// ILIKE folds a character at a time, as PostgreSQL does (finding N-B5):
/// a capital sigma before a wildcard is the sigma inside a word, and a
/// capital İ is one character, an i.
#[test]
fn ilike_folds_each_character_alone() {
    for (value, pattern) in [("ΟΔΟΣΑ", "ΟΔΟΣ%"), ("ΟΔΟΣ", "%οσ"), ("ΟΔΟΣ", "%ος"), ("οδος", "ΟΔΟΣ"), ("ΟΔΟΣ ΑΘΗΝΑΣ", "%Σ_Α%"), ("İ", "_"), ("İstanbul", "istanbul")] {
        assert!(Like::new(pattern, true).matches(value), "{value:?} ILIKE {pattern:?}");
        assert!(reference(pattern, true, value), "{value:?} ILIKE {pattern:?} by the row evaluator");
        // The index finds it, and only the values that match.
        let vals = vec![Some(value.to_string()), Some("ΟΔΟ".to_string()), Some("x".to_string())];
        let ix = index(&vals, 4, MAX_PART_BYTES);
        let want: Vec<DocId> = vals.iter().enumerate().filter(|(_, v)| reference(pattern, true, v.as_ref().unwrap())).map(|(d, _)| d as DocId).collect();
        assert!(want.contains(&0));
        let got = like::eval(&ix, &Like::new(pattern, true), vals.len() as u32, &mut |d| Ok(vals[d as usize].clone()), 0, &mut Stats::default()).unwrap();
        assert_eq!(got.to_sorted(), want, "{value:?} ILIKE {pattern:?} from the index");
    }
    assert!(!Like::new("ΟΔΟΣ%", false).matches("ΟΔΟσΑ"), "LIKE keeps case");
}

#[test]
fn plans() {
    let exact = |p: &str| match Like::new(p, false).plan() {
        Plan::Exact(b) => b,
        other => panic!("{p}: {other:?}"),
    };
    assert_eq!(exact("abc"), [&[SEP][..], b"abc", &[SEP]].concat());
    assert_eq!(exact("abc%"), [&[SEP][..], b"abc"].concat());
    assert_eq!(exact("%abc"), [b"abc", &[SEP][..]].concat());
    assert_eq!(exact("%%abc%"), b"abc");
    assert_eq!(exact(""), [SEP, SEP]);
    assert_eq!(Like::new("%%", false).plan(), Plan::All);
    assert_eq!(Like::new("%", true).plan(), Plan::All);
    // No token at all, which `ILIKE` reaches without a simple shape.
    assert_eq!(Like::new("", true).plan(), Like::new("", false).plan());
    assert!(matches!(Like::new("", true).plan(), Plan::Exact(b) if b.len() == 2));
    assert_eq!(Like::new("_%", false).plan(), Plan::Pieces(Vec::new()));
    let Plan::Pieces(p) = Like::new("ab%cd_e", false).plan() else { panic!() };
    assert_eq!(p.iter().map(|p| p.units[0][0].clone()).collect::<Vec<_>>(), [[&[SEP][..], b"ab"].concat(), b"cd".to_vec(), [b"e", &[SEP][..]].concat()]);
}

#[test]
fn documents_without_a_value_take_no_ordinal() {
    let vals = vec![None, Some("abc".to_string()), None, None, Some(String::new()), Some("xabcx".to_string()), None];
    let seg = SegmentFile::from_vec(image(&vals, 2, MAX_PART_BYTES)).unwrap();
    // The ordinals are not consecutive docIDs: a docs section says which.
    assert!(seg.has("sub.f1.p0.docs"));
    let ix = SubstringIndex::open(&seg, "f1").unwrap().unwrap();
    let docs = |p: &str| like::eval(&ix, &Like::new(p, false), 7, &mut |d| Ok(vals[d as usize].clone()), 0, &mut Stats::default()).unwrap().to_sorted();
    assert_eq!(docs("%abc%"), [1, 5]);
    assert_eq!(docs("abc"), [1]);
    assert_eq!(docs(""), [4]);
    assert_eq!(docs("%"), [1, 4, 5]);
    // Consecutive documents need none.
    let seg = SegmentFile::from_vec(image(&vals[4..6], 2, MAX_PART_BYTES)).unwrap();
    assert!(!seg.has("sub.f1.p0.docs"));
    // No values at all: an index of no parts.
    let ix = index(&[None, None], 2, 64);
    assert_eq!((ix.parts().len(), ix.docs()), (0, 0));
}

#[test]
fn what_a_reader_does_not_understand_is_scanned() {
    let vals = vec![Some("abc".to_string())];
    let seg = SegmentFile::from_vec(image(&vals, 2, 64)).unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&seg.require("sub.f1.meta").unwrap()).unwrap();
    let with = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut m = meta.clone();
        change(&mut m);
        let mut w = SegmentWriter::new(1);
        w.put("sub.f1.meta", serde_json::to_vec(&m).unwrap());
        for s in ["sub.f1.p0.bwt", "sub.f1.p0.sa"] {
            w.put(s, seg.require(s).unwrap().to_vec());
        }
        SubstringIndex::open(&SegmentFile::from_vec(w.finish()).unwrap(), "f1")
    };
    assert!(with(&|_| {}).unwrap().is_some());
    // Another version, fold or encoding: no index, and no error.
    assert!(with(&|m| m["version"] = 2.into()).unwrap().is_none());
    assert!(with(&|m| m["fold"] = "nfkc_casefold".into()).unwrap().is_none());
    assert!(with(&|m| m["parts"][0]["encoding"] = "rl".into()).unwrap().is_none());
    // A field a later writer adds is passed over.
    assert!(with(&|m| m["parts"][0]["lists"] = 7.into()).unwrap().is_some());
    // What version 1 does understand and finds wrong is corruption.
    assert!(with(&|m| m["parts"][0]["n"] = 99.into()).is_err());
    assert!(with(&|m| m["parts"][0]["samples"] = 0.into()).is_err());
    assert!(with(&|m| m["docs"] = 5.into()).is_err());
    // A segment without the sections has no index.
    assert!(SubstringIndex::open(&SegmentFile::from_vec(SegmentWriter::new(1).finish()).unwrap(), "f1").unwrap().is_none());
}

#[test]
fn a_part_carried_into_another_segment_reads_through_its_docs_section() {
    // What a later writer may do at a merge: keep a part's tree and samples
    // and write only where its documents now are. Document 1 is gone.
    let vals = vec![Some("alpha".to_string()), Some("beta".to_string()), Some("alphabet".to_string())];
    let seg = SegmentFile::from_vec(image(&vals, 2, MAX_PART_BYTES)).unwrap();
    let mut meta: Meta = serde_json::from_slice(&seg.require("sub.f1.meta").unwrap()).unwrap();
    meta.parts[0].first_doc = None;
    let mut w = SegmentWriter::new(10);
    w.put("sub.f1.meta", serde_json::to_vec(&meta).unwrap());
    for s in ["sub.f1.p0.bwt", "sub.f1.p0.sa"] {
        w.put(s, seg.require(s).unwrap().to_vec());
    }
    w.put("sub.f1.p0.docs", build::encode_docs(&[Some(7), None, Some(2)], 10));
    let ix = SubstringIndex::open(&SegmentFile::from_vec(w.finish()).unwrap(), "f1").unwrap().unwrap();
    let docs = |p: &str| like::eval(&ix, &Like::new(p, false), 10, &mut |_| panic!("answered from the index"), 0, &mut Stats::default()).unwrap().to_sorted();
    assert_eq!(docs("%alpha%"), [2, 7]);
    assert_eq!(docs("%bet%"), [2]);
    assert_eq!(docs("beta"), [] as [DocId; 0]);
    assert_eq!(docs("%"), [2, 7]);
}

#[test]
fn long_codes_are_shortened() {
    // Counts that grow by a factor of four make a Huffman code as deep as
    // the alphabet: the lengths stay within the format's.
    let mut counts = [0u32; 256];
    for (i, c) in counts.iter_mut().enumerate().take(31) {
        *c = 1u32 << i;
    }
    let lens = build::code_lengths(&counts);
    assert!(lens.iter().all(|&l| l as usize <= MAX_LEN));
    assert!(Shape::from_lens(&lens).is_ok());
    // One byte alone has a code of one digit; none is not a code.
    let mut one = [0u32; 256];
    one[SEP as usize] = 9;
    assert_eq!(build::code_lengths(&one)[SEP as usize], 1);
    assert!(Shape::from_lens(&[0u8; 256]).is_err());
    let mut over = [0u8; 256];
    over[..5].fill(1);
    assert!(Shape::from_lens(&over).is_err());
}

#[test]
fn a_level_ends_on_a_block_boundary() {
    // 448 digits fill a block: the level has one more, for a rank at its
    // end.
    for n in [BLOCK_DIGITS - 3, BLOCK_DIGITS - 2, BLOCK_DIGITS - 1, 2 * BLOCK_DIGITS - 2] {
        let vals = vec![Some("ab".repeat(n / 2) + if n % 2 == 1 { "a" } else { "" })];
        let ix = index(&vals, 32, MAX_PART_BYTES);
        assert_eq!(ix.parts()[0].n(), n as u64 + 2);
        assert_eq!(ix.count(b"ab").unwrap(), (n / 2) as u64);
        assert_eq!(ix.count(b"ba").unwrap(), ((n - 1) / 2) as u64);
    }
}

#[test]
fn damaged_sections_fail_or_answer_but_never_read_outside() {
    let vals: Vec<Option<String>> = (0..60).map(|i| Some(format!("row {i} of the quick brown fox, {}", "ab".repeat(i % 7)))).collect();
    let good = image(&vals, 4, 256);
    let mut rng = StdRng::seed_from_u64(7);
    for _ in 0..if cfg!(miri) { 20 } else { 3000 } {
        let mut bad = good.clone();
        for _ in 0..rng.gen_range(1..4) {
            let at = rng.gen_range(0..bad.len());
            bad[at] ^= 1 << rng.gen_range(0..8);
        }
        let Ok(seg) = SegmentFile::from_vec(bad) else { continue };
        let Ok(Some(ix)) = SubstringIndex::open(&seg, "f1") else { continue };
        for p in ["%quick%", "row 1%", "%abab", "%o_ %", "%"] {
            for ci in [false, true] {
                let _ = like::eval(&ix, &Like::new(p, ci), vals.len() as u32, &mut |d| Ok(vals.get(d as usize).cloned().flatten()), [0, like::LOCATE_BYTES][ci as usize], &mut Stats::default());
            }
        }
    }
}

#[test]
fn miri_views_of_a_small_index() {
    let vals = vec![Some("banana".to_string()), None, Some("bandana".to_string()), Some(String::new())];
    let ix = index(&vals, 2, 8);
    assert!(ix.parts().len() > 1);
    assert_eq!(ix.count(b"ana").unwrap(), 3);
    let docs = |p: &str, ci: bool| like::eval(&ix, &Like::new(p, ci), 4, &mut |d| Ok(vals[d as usize].clone()), 0, &mut Stats::default()).unwrap().to_sorted();
    assert_eq!(docs("%ana", false), [0, 2]);
    assert_eq!(docs("ban%", false), [0, 2]);
    // The table of capitals ILIKE reads is every character's lowercase,
    // minutes of interpreting: left to the other tests.
    if !cfg!(miri) {
        assert_eq!(docs("%D_N%", true), [2]);
    }
    assert_eq!(docs("%d_n%", false), [2]);
    assert_eq!(docs("", false), [3]);
}
