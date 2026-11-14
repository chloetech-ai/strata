//! `LIKE` and `ILIKE` patterns: what a value must be to match, and which
//! byte strings a substring index is searched for to find the values that
//! may.
//!
//! The answer is defined by [`Like::matches`], which is the row
//! evaluator's `LIKE` ([`crate::query::ir::eval::like`]): `%` any run of
//! characters, `_` one character, and for `ILIKE` both sides lowercased
//! first. The index only narrows the values that are tested, or, for a
//! case-sensitive pattern of one literal, names them outright.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::error::Result;
use crate::segment::{DocId, DocSet};

use super::read::Part;
use super::{SubstringIndex, SEP};

/// A pattern's pieces.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// Literal text.
    Lit(String),
    /// `_`: one character.
    One,
    /// `%`: any run of characters.
    Any,
}

/// What a case-sensitive pattern of at most one literal asks of a value.
#[derive(Clone, Debug, PartialEq)]
enum Simple {
    Equals(String),
    Starts(String),
    Ends(String),
    Contains(String),
}

/// A `LIKE` (or, with `ci`, `ILIKE`) pattern.
#[derive(Clone, Debug)]
pub struct Like {
    ci: bool,
    /// The pattern's characters, lowercased for `ILIKE`.
    chars: Vec<char>,
    toks: Vec<Tok>,
    simple: Option<Simple>,
    /// The same shapes of an `ILIKE` pattern, for [`Like::matches`] alone:
    /// the index still looks at the candidates' values.
    folded: Option<Simple>,
}

/// How a part of a substring index answers a pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// Every value matches.
    All,
    /// The values in which these bytes occur are the matches.
    Exact(Vec<u8>),
    /// A value that matches holds every piece; a value that holds them is
    /// tested with [`Like::matches`]. Without pieces, every value is.
    Pieces(Vec<Piece>),
}

/// A string a matching value holds: one of the alternatives of each unit,
/// one unit after another.
#[derive(Clone, Debug, PartialEq)]
pub struct Piece {
    pub units: Vec<Vec<Vec<u8>>>,
}

impl Piece {
    fn bytes(b: Vec<u8>) -> Piece {
        Piece { units: vec![vec![b]] }
    }
}

/// For lowercase character `c`, the other characters whose lowercase starts
/// with it, and the characters that are a later part of some character's
/// lowercase (the dot above of `İ`, whose lowercase is `i` and the dot).
struct Folds {
    sources: HashMap<char, Vec<char>>,
    later: HashSet<char>,
}

fn folds() -> &'static Folds {
    static FOLDS: OnceLock<Folds> = OnceLock::new();
    FOLDS.get_or_init(|| {
        let mut f = Folds { sources: HashMap::new(), later: HashSet::new() };
        for x in (0..=char::MAX as u32).filter_map(char::from_u32) {
            let mut lower = x.to_lowercase();
            let first = lower.next().expect("a character's lowercase");
            if first != x {
                f.sources.entry(first).or_default().push(x);
            }
            f.later.extend(lower);
        }
        // ILIKE folds a final sigma as the other sigma
        // ([`crate::query::ir::eval::fold_case`]): a pattern's `σ` is a
        // value's `ς` too.
        f.sources.entry('σ').or_default().push('ς');
        f
    })
}

impl Like {
    pub fn new(pattern: &str, ci: bool) -> Like {
        let folded = if ci { crate::query::ir::eval::fold_case(pattern) } else { pattern.to_string() };
        let chars: Vec<char> = folded.chars().collect();
        let mut toks: Vec<Tok> = Vec::new();
        for &c in &chars {
            match (c, toks.last_mut()) {
                ('%', Some(Tok::Any)) => {}
                ('%', _) => toks.push(Tok::Any),
                ('_', _) => toks.push(Tok::One),
                (c, Some(Tok::Lit(s))) => s.push(c),
                (c, _) => toks.push(Tok::Lit(c.to_string())),
            }
        }
        let shape = match &toks[..] {
            [] => Some(Simple::Equals(String::new())),
            [Tok::Lit(a)] => Some(Simple::Equals(a.clone())),
            [Tok::Lit(a), Tok::Any] => Some(Simple::Starts(a.clone())),
            [Tok::Any, Tok::Lit(a)] => Some(Simple::Ends(a.clone())),
            [Tok::Any, Tok::Lit(a), Tok::Any] => Some(Simple::Contains(a.clone())),
            _ => None,
        };
        let (simple, folded) = if ci { (None, shape) } else { (shape, None) };
        Like { ci, chars, toks, simple, folded }
    }

    /// Whether `value` matches.
    pub fn matches(&self, value: &str) -> bool {
        if self.every_value() {
            return true;
        }
        // ILIKE compares lowercase with lowercase, as the row evaluator
        // does; ASCII's lowercase needs no table.
        let lower;
        let (value, shape) = match self.ci {
            true => {
                lower = if value.is_ascii() { value.to_ascii_lowercase() } else { crate::query::ir::eval::fold_case(value) };
                (lower.as_str(), &self.folded)
            }
            false => (value, &self.simple),
        };
        match shape {
            Some(Simple::Equals(a)) => value == a,
            Some(Simple::Starts(a)) => value.starts_with(a.as_str()),
            Some(Simple::Ends(a)) => value.ends_with(a.as_str()),
            Some(Simple::Contains(a)) => value.contains(a.as_str()),
            None => crate::query::ir::eval::like(&value.chars().collect::<Vec<_>>(), &self.chars),
        }
    }

    /// Whether every value matches (`%`).
    pub fn every_value(&self) -> bool {
        self.toks == [Tok::Any]
    }

    /// How an index of the values' bytes answers the pattern.
    pub fn plan(&self) -> Plan {
        if self.every_value() {
            return Plan::All;
        }
        let around = |a: &str, before: bool, after: bool| {
            let mut b = Vec::with_capacity(a.len() + 2);
            b.extend(before.then_some(SEP));
            b.extend_from_slice(a.as_bytes());
            b.extend(after.then_some(SEP));
            b
        };
        match &self.simple {
            Some(Simple::Equals(a)) => return Plan::Exact(around(a, true, true)),
            Some(Simple::Starts(a)) => return Plan::Exact(around(a, true, false)),
            Some(Simple::Ends(a)) => return Plan::Exact(around(a, false, true)),
            Some(Simple::Contains(a)) => return Plan::Exact(around(a, false, false)),
            None => {}
        }
        // The empty pattern, in any case: the empty value alone.
        let Some(last) = self.toks.len().checked_sub(1) else { return Plan::Exact(around("", true, true)) };
        let mut pieces = Vec::new();
        for (i, t) in self.toks.iter().enumerate() {
            let Tok::Lit(a) = t else { continue };
            if !self.ci {
                pieces.push(Piece::bytes(around(a, i == 0, i == last)));
                continue;
            }
            // A value's text matches the lowercase literal where each of
            // its characters is one whose lowercase starts with the
            // literal's. The later parts of a lowercase of several
            // characters have no character of their own: the literal is
            // cut there.
            let f = folds();
            let chars: Vec<char> = a.chars().collect();
            let mut units: Vec<Vec<Vec<u8>>> = Vec::new();
            let mut start = i == 0;
            for (j, &c) in chars.iter().enumerate() {
                if f.later.contains(&c) {
                    if !units.is_empty() {
                        pieces.push(Piece { units: std::mem::take(&mut units) });
                    }
                    start = false;
                    continue;
                }
                if units.is_empty() && start && j == 0 {
                    units.push(vec![vec![SEP]]);
                }
                let mut alts = vec![c.to_string().into_bytes()];
                alts.extend(f.sources.get(&c).into_iter().flatten().map(|x| x.to_string().into_bytes()));
                units.push(alts);
            }
            if !units.is_empty() {
                if i == last {
                    units.push(vec![vec![SEP]]);
                }
                pieces.push(Piece { units });
            }
        }
        Plan::Pieces(pieces)
    }
}

/// The rows of `part` whose suffixes start with `piece`, as ranges. Past
/// `max_ranges` alternatives still alive, the rest of the piece is left
/// unread: the ranges then hold more rows, of suffixes that start with the
/// piece's end only.
pub fn rows(part: &Part, piece: &Piece, max_ranges: usize) -> Result<Vec<(u64, u64)>> {
    let mut ranges = vec![part.all()];
    for (i, unit) in piece.units.iter().enumerate().rev() {
        let mut next = Vec::new();
        for &r in &ranges {
            for alt in unit {
                let e = part.extend(r, alt)?;
                if e.0 < e.1 {
                    next.push(e);
                }
            }
        }
        if next.len() > max_ranges && i + 1 < piece.units.len() {
            break;
        }
        ranges = next;
        if ranges.is_empty() {
            break;
        }
    }
    Ok(ranges)
}

/// How a pattern was answered in a segment, for a query's plan.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    /// Parts answered from the index alone.
    pub exact: u32,
    /// Parts whose candidates were tested against their values.
    pub tested: u32,
    /// Parts scanned: too many occurrences to list, or no literal.
    pub scanned: u32,
    /// Rows of the index walked to a sample.
    pub located: u64,
    /// Values read and tested.
    pub read: u64,
}

/// Bytes of a part's text that reading and testing the part's values is
/// taken to cost as much as walking one row to its sample and testing its
/// document's value (`locate_bytes` of [`eval`]): a pattern with more
/// candidates than one per this many bytes is answered by a scan of the
/// part. Rows the index answers alone ([`Plan::Exact`]) read no value, and
/// an `ILIKE`'s scan folds the case of every value: both are listed up to
/// [`EXACT_ROWS`] times as many.
///
/// Measured on Loghub's SSH and Android logs (`results/substring/`): a scan
/// reads 2 to 3 ns a byte (6 for `ILIKE`), a row costs 0.4 to 2 µs to walk
/// and a candidate 7 to 10 µs with its value.
pub const LOCATE_BYTES: u64 = 4096;

/// How many more rows are listed when no value is read for them, or when
/// the scan is an `ILIKE`'s.
pub const EXACT_ROWS: u64 = 4;

/// The alternatives of an `ILIKE` literal followed at most.
pub const MAX_RANGES: usize = 64;

/// The documents of a segment of `n_docs` documents whose value matches
/// `like`, from `index`; `value` reads a document's value where the index
/// alone does not say (`None` for a document without one). A part in which
/// the pattern has more candidates than one per `locate_bytes` bytes is
/// scanned ([`LOCATE_BYTES`]; 0 never scans for that).
pub fn eval(index: &SubstringIndex, like: &Like, n_docs: u32, value: &mut dyn FnMut(DocId) -> Result<Option<String>>, locate_bytes: u64, stats: &mut Stats) -> Result<DocSet> {
    eval_with(index, like, n_docs, value, locate_bytes, stats, &mut || Ok(()))
}

/// [`eval`], calling `tick` once a document it tests or locates, so that a
/// query's deadline and cancel flag stop it inside a part: an error from
/// `tick` ends it with that error.
pub fn eval_with(index: &SubstringIndex, like: &Like, n_docs: u32, value: &mut dyn FnMut(DocId) -> Result<Option<String>>, locate_bytes: u64, stats: &mut Stats, tick: &mut dyn FnMut() -> Result<()>) -> Result<DocSet> {
    let mut bits = vec![0u64; (n_docs as usize).div_ceil(64)];
    let plan = like.plan();
    for part in index.parts() {
        tick()?;
        eval_part(part, like, &plan, n_docs, value, locate_bytes, stats, &mut bits, tick)?;
    }
    Ok(DocSet::from_bits(bits, n_docs))
}

fn set(bits: &mut [u64], d: DocId, n_docs: u32) -> Result<()> {
    if d >= n_docs {
        return Err(crate::error::corrupt("substring index: a document the segment does not have"));
    }
    bits[d as usize / 64] |= 1 << (d % 64);
    Ok(())
}

/// Tests each of the part's documents.
fn scan_part(part: &Part, like: &Like, n_docs: u32, value: &mut dyn FnMut(DocId) -> Result<Option<String>>, stats: &mut Stats, bits: &mut [u64], tick: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    stats.scanned += 1;
    for o in 0..part.n_docs() {
        tick()?;
        let Some(d) = part.doc(o)? else { continue };
        stats.read += 1;
        if value(d)?.is_some_and(|v| like.matches(&v)) {
            set(bits, d, n_docs)?;
        }
    }
    Ok(())
}

/// Where a part's matches are, by a pattern's plan.
enum Rows {
    /// Every document of the part.
    All,
    /// No literal to search: any document may match.
    Any,
    /// The rows of the candidates, and whether each is a match without a
    /// look at its value.
    Ranges(Vec<(u64, u64)>, bool),
}

fn candidates(part: &Part, plan: &Plan) -> Result<Rows> {
    Ok(match plan {
        Plan::All => Rows::All,
        Plan::Exact(bytes) => Rows::Ranges(vec![part.search(bytes)?], true),
        Plan::Pieces(pieces) if pieces.is_empty() => Rows::Any,
        Plan::Pieces(pieces) => {
            // The piece with the fewest rows; a piece with none means no
            // value of the part matches.
            let total = |r: &[(u64, u64)]| r.iter().map(|x| x.1 - x.0).sum::<u64>();
            let mut best: Option<Vec<(u64, u64)>> = None;
            for piece in pieces {
                let r = rows(part, piece, MAX_RANGES)?;
                if best.as_ref().is_none_or(|b| total(&r) < total(b)) {
                    best = Some(r);
                }
                if best.as_ref().is_some_and(|b| total(b) == 0) {
                    break;
                }
            }
            Rows::Ranges(best.unwrap_or_default(), false)
        }
    })
}

/// Whether `total` candidate rows of `part` are too many to list: more
/// than one per `locate_bytes` bytes of its text, or per a quarter of that
/// where listing is cheap beside the scan (`cheap`: no value is read, or
/// the scan would fold every value's case).
fn too_many(part: &Part, total: u64, cheap: bool, locate_bytes: u64) -> bool {
    let per_row = if cheap { locate_bytes / EXACT_ROWS } else { locate_bytes };
    total > 0 && total.saturating_mul(per_row) > part.n()
}

/// What [`eval`] would do for a pattern, worked out from the index's
/// counts alone (a search per literal and part, no row walked).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Estimate {
    /// No more documents than this match.
    pub candidates: u64,
    /// Rows to walk to a sample.
    pub located: u64,
    /// Candidates the index does not settle, whose values are read and
    /// tested one by one.
    pub tested: u64,
    /// Documents of the parts that are scanned, value after value.
    pub scanned: u64,
}

/// What answering `like` from `index` takes, under [`eval`]'s rule for
/// `locate_bytes`.
pub fn estimate(index: &SubstringIndex, like: &Like, locate_bytes: u64) -> Result<Estimate> {
    let plan = like.plan();
    let mut e = Estimate::default();
    for part in index.parts() {
        match candidates(part, &plan)? {
            Rows::All => e.candidates += part.n_docs(),
            Rows::Any => {
                e.candidates += part.n_docs();
                e.scanned += part.n_docs();
            }
            Rows::Ranges(ranges, exact) => {
                let total: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
                let docs = total.min(part.n_docs());
                e.candidates += docs;
                if too_many(part, total, exact || like.ci, locate_bytes) {
                    e.scanned += part.n_docs();
                } else {
                    e.located += total;
                    e.tested += if exact { 0 } else { docs };
                }
            }
        }
    }
    Ok(e)
}

fn eval_part(part: &Part, like: &Like, plan: &Plan, n_docs: u32, value: &mut dyn FnMut(DocId) -> Result<Option<String>>, locate_bytes: u64, stats: &mut Stats, bits: &mut [u64], tick: &mut dyn FnMut() -> Result<()>) -> Result<()> {
    let (ranges, exact) = match candidates(part, plan)? {
        Rows::All => {
            for o in 0..part.n_docs() {
                if let Some(d) = part.doc(o)? {
                    set(bits, d, n_docs)?;
                }
            }
            stats.exact += 1;
            return Ok(());
        }
        Rows::Any => return scan_part(part, like, n_docs, value, stats, bits, tick),
        Rows::Ranges(ranges, exact) => (ranges, exact),
    };
    let total: u64 = ranges.iter().map(|r| r.1 - r.0).sum();
    if too_many(part, total, exact || like.ci, locate_bytes) {
        return scan_part(part, like, n_docs, value, stats, bits, tick);
    }
    match exact {
        true => stats.exact += 1,
        false => stats.tested += 1,
    }
    // Each row's document, once.
    let mut seen = vec![0u64; (part.n_docs() as usize).div_ceil(64)];
    for (lo, hi) in ranges {
        for row in lo..hi {
            tick()?;
            stats.located += 1;
            let o = part.ordinal(row)?;
            let (w, b) = ((o / 64) as usize, 1u64 << (o % 64));
            if seen[w] & b != 0 {
                continue;
            }
            seen[w] |= b;
            let Some(d) = part.doc(o)? else { continue };
            if exact {
                set(bits, d, n_docs)?;
            } else {
                stats.read += 1;
                if value(d)?.is_some_and(|v| like.matches(&v)) {
                    set(bits, d, n_docs)?;
                }
            }
        }
    }
    Ok(())
}

/// The documents of `0..n_docs` whose value matches `like`, each read with
/// `value`: what a segment without an index answers with.
pub fn scan(like: &Like, n_docs: u32, value: &mut dyn FnMut(DocId) -> Result<Option<String>>) -> Result<DocSet> {
    scan_with(like, n_docs, value, &mut || Ok(()))
}

/// [`scan`], calling `tick` once a document, as [`eval_with`] does.
pub fn scan_with(like: &Like, n_docs: u32, value: &mut dyn FnMut(DocId) -> Result<Option<String>>, tick: &mut dyn FnMut() -> Result<()>) -> Result<DocSet> {
    let mut bits = vec![0u64; (n_docs as usize).div_ceil(64)];
    for d in 0..n_docs {
        tick()?;
        if value(d)?.is_some_and(|v| like.matches(&v)) {
            bits[d as usize / 64] |= 1 << (d % 64);
        }
    }
    Ok(DocSet::from_bits(bits, n_docs))
}
