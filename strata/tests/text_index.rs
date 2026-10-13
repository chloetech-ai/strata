//! Text index structure: dictionary lookups and prefixes, posting lists of
//! both formats against a naive inverted index (docIDs, frequencies,
//! positions, seeks, block bounds), norms and statistics.

mod text_common;

use rand::{rngs::StdRng, Rng, SeedableRng};
use strata::segment::build::{build_segment, BuildOptions};
use strata::segment::SegmentFile;
use strata::text::codec::{decode_norm, encode_norm};
use strata::text::postings::LIST_IMPACT;
use strata::text::{PostingsFormat, TextFieldReader, END};
use strata::{data, VectorIndexKind};
use text_common::*;

/// Every check of this file on one corpus and configuration.
fn check_corpus(texts: &[String], format: PostingsFormat, min_df: u32, seed: u64) {
    let what = format!("{format:?}/{min_df}");
    let naive = Naive::new(texts);
    let seg = build(texts, format, min_df, true);
    let r = TextFieldReader::open(&seg, "body").unwrap();
    assert_eq!(r.n_docs() as usize, texts.len());
    assert_eq!(r.n_terms() as usize, naive.index.len());
    assert_eq!(r.total_len(), naive.lens.iter().map(|&l| l as u64).sum::<u64>());
    for (d, &l) in naive.lens.iter().enumerate() {
        assert_eq!(r.doc_len(d as u32), decode_norm(encode_norm(l)));
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let mut loom_lists = 0;
    // Dictionary order and entries.
    let mut listed = Vec::new();
    r.for_each_term(|t, info| listed.push((String::from_utf8(t.to_vec()).unwrap(), info)));
    assert!(listed.iter().map(|(t, _)| t).eq(naive.index.keys()), "{what}: term order");
    for (ord, (term, info)) in listed.iter().enumerate() {
        assert_eq!(info.ord as usize, ord);
        assert_eq!(r.term_info(term), Some(*info), "{what}: lookup {term}");
        assert_eq!(r.doc_freq(term) as usize, naive.index[term].len());
    }
    for absent in ["", "0", "zzzzzz", "w1", "w99999z", "topi", "topicz", "~"].into_iter().filter(|t| !naive.index.contains_key(*t)) {
        assert_eq!(r.term_info(absent), None, "{what}: absent {absent:?}");
    }
    // Prefixes.
    for p in ["w1", "w12", "w", "sub", "t", "even", "zz", "", "w2999z"] {
        for limit in [1, 5, usize::MAX] {
            let want: Vec<&String> = naive.index.keys().filter(|t| t.starts_with(p)).take(limit).collect();
            assert!(r.expand_prefix(p, limit).iter().eq(want), "{what}: prefix {p:?} limit {limit}");
        }
    }
    // Every list: sequential scan with freqs and positions.
    let mut buf = Vec::new();
    for (term, postings) in &naive.index {
        let mut c = r.postings(term).unwrap();
        loom_lists += c.is_loom() as usize;
        assert_eq!(c.len() as usize, postings.len());
        for (i, (d, pos)) in postings.iter().enumerate() {
            assert_eq!(c.doc(), *d, "{what}: {term} #{i}");
            assert_eq!(c.rank(), i);
            // Read positions on a random subset (skipping exercises the
            // per-block offsets and frequency-driven skips).
            if i % 3 == 0 || rng.gen_bool(0.2) {
                c.positions(&mut buf);
                assert_eq!(&buf, pos, "{what}: positions of {term} in {d}");
            }
            assert_eq!(c.freq() as usize, pos.len(), "{what}: tf of {term} in {d}");
            c.next();
        }
        assert_eq!(c.doc(), END);
        assert_eq!(c.next(), END);
        // The frontier: every posting dominated, every pair attained.
        let pairs: Vec<(u32, u8)> = postings.iter().map(|(d, p)| (p.len() as u32, r.norms()[*d as usize])).collect();
        let f = c.frontier().to_vec();
        assert!(pairs.iter().all(|&(tf, n)| f.iter().any(|&(ftf, fnorm)| ftf >= tf && fnorm <= n)), "{what}: {term} frontier");
        assert!(f.iter().all(|p| pairs.contains(p)));
    }
    if format == PostingsFormat::Loom {
        let want = naive.index.values().filter(|p| p.len() >= min_df as usize).count();
        assert_eq!(loom_lists, want, "{what}: Loom lists");
    }
    // Random seeks (with freqs, positions and block bounds) on longer lists.
    for (term, postings) in naive.index.iter().filter(|(_, p)| p.len() > 20) {
        let docs: Vec<u32> = postings.iter().map(|p| p.0).collect();
        let last = *docs.last().unwrap();
        for _ in 0..3 {
            let mut c = r.postings(term).unwrap();
            let mut t = 0u32;
            let mut bound_target = 0u32;
            loop {
                t += rng.gen_range(1..=(2 * texts.len() as u32 / docs.len() as u32).max(2) * 4);
                let i = docs.partition_point(|&x| x < t);
                // Block bound of a target at or after the cursor.
                if rng.gen_bool(0.5) && t >= bound_target {
                    bound_target = t;
                    let (blast, imp) = c.block_bound(t);
                    if i == docs.len() {
                        assert_eq!(blast, END);
                    } else {
                        assert!(blast >= docs[i] && blast <= last, "{what}: {term} block last {blast} for {t}");
                        // The block covers docs[i]..=blast; its impact bounds them.
                        let j = docs.partition_point(|&x| x <= blast);
                        for (d, pos) in &postings[i..j] {
                            let (tf, n) = (pos.len() as u32, r.norms()[*d as usize]);
                            if imp == LIST_IMPACT {
                                assert!(c.frontier().iter().any(|&(ftf, fnorm)| ftf >= tf && fnorm <= n));
                            } else {
                                assert!(imp >> 8 >= tf && (imp & 0xff) as u8 <= n, "{what}: {term} impact {imp:x} vs ({tf}, {n})");
                            }
                        }
                    }
                }
                let got = c.seek(t);
                if i == docs.len() {
                    assert_eq!(got, END, "{what}: {term} seek {t} past the end");
                    break;
                }
                assert_eq!(got, docs[i], "{what}: {term} seek {t}");
                assert_eq!(c.rank(), i);
                if rng.gen_bool(0.5) {
                    c.positions(&mut buf);
                    assert_eq!(buf, postings[i].1, "{what}: {term} positions after seek");
                }
                assert_eq!(c.freq() as usize, postings[i].1.len());
                // Seeking backwards or in place does not move.
                assert_eq!(c.seek(t / 2), docs[i]);
                t = docs[i];
            }
        }
    }
}

#[test]
fn synthetic_corpus_all_formats() {
    let texts = synthetic(7, 6000);
    for (format, min_df) in configs() {
        check_corpus(&texts, format, min_df, 11);
    }
}

#[test]
fn small_and_degenerate_corpora() {
    let cases: Vec<Vec<String>> = vec![
        vec![],
        vec!["".into()],
        vec!["the and of".into(), "".into()],
        vec!["alpha".into()],
        vec!["alpha beta alpha".into(), "beta".into(), "gamma alpha".into()],
        (0..129).map(|i| format!("common r{i}")).collect(),
        (0..256).map(|i| if i % 2 == 0 { "x".to_string() } else { "y x".into() }).collect(),
    ];
    for texts in &cases {
        for (format, min_df) in configs() {
            check_corpus(texts, format, min_df, 3);
        }
    }
}

#[test]
fn scifact_all_formats() {
    let Some(texts) = scifact(1500) else { return };
    for (format, min_df) in [(PostingsFormat::Bp128, 0), (PostingsFormat::Loom, 32)] {
        check_corpus(&texts, format, min_df, 5);
    }
}

/// The field builder is reached through `build_segment` with the
/// `text.postings` option, and indexes only the `body` field.
#[test]
fn builds_through_the_segment_builder() {
    let Some(_) = scifact(10) else { return };
    let c = data::load("scifact", Some(300)).unwrap();
    let schema = data::beir_schema(c.dim, VectorIndexKind::Flat);
    for format in ["loom", "bp128"] {
        let opts = BuildOptions::default().set("text.postings", format).set("text.loom_min_df", 16);
        let seg = SegmentFile::from_vec(build_segment(&schema, &c.docs, &opts).unwrap()).unwrap();
        let r = TextFieldReader::open(&seg, "body").unwrap();
        assert_eq!(r.n_docs(), 300);
        assert_eq!(r.format(), PostingsFormat::parse(format).unwrap());
        assert!(TextFieldReader::open(&seg, "title").is_err(), "stored-only fields are not indexed");
        let s = r.sizes();
        assert!(s.docs > 0 && s.freqs > 0 && s.positions > 0 && s.dict > 0 && s.norms == 300);
    }
    let bad = BuildOptions::default().set("text.postings", "roaring");
    assert!(build_segment(&schema, &c.docs, &bad).is_err());
}
