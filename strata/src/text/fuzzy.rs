//! Typo tolerance: the edit distance between a query term and the indexed
//! terms, counting an insertion, a deletion, a substitution or a swap of two
//! neighbouring letters as one edit (optimal string alignment, the
//! Damerau-Levenshtein distance most spelling tools use).

/// The distance between `a` and `b` if it is at most `k`.
pub fn within(a: &[char], b: &str, k: u8) -> Option<u8> {
    let k = k as usize;
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > k {
        return None;
    }
    // Three rows of the dynamic programme: i-2, i-1 and i.
    let mut prev2 = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = (a[i - 1] != b[j - 1]) as usize;
            let mut d = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            cur[j] = d;
            row_min = row_min.min(d);
        }
        if row_min > k {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[m] <= k).then_some(prev[m] as u8)
}

/// The weight of a term `edits` away from a query term of `len` letters in
/// a ranked query, as Lucene's fuzzy queries boost theirs: `1 − edits / len`.
pub fn weight(edits: u8, len: usize) -> f32 {
    if edits == 0 {
        return 1.0;
    }
    (1.0 - edits as f32 / len.max(1) as f32).max(0.1)
}

/// Terms in byte order, read one after another, that can jump ahead: a
/// sorted dictionary seen as an implicit trie, whose nodes are the runs of
/// terms sharing a prefix ([`walk`]).
pub trait Sorted {
    /// The current term, `None` past the last.
    fn term(&self) -> Option<&[u8]>;
    /// The current term's position: the number of terms before it.
    fn pos(&self) -> usize;
    /// Moves to the next term.
    fn next(&mut self);
    /// Moves to the first term at or after `t` (never back).
    fn seek(&mut self, t: &[u8]);
}

/// Sorted terms in a slice.
pub struct SliceTerms<'a, T: AsRef<[u8]>> {
    terms: &'a [T],
    pos: usize,
}

impl<'a, T: AsRef<[u8]>> SliceTerms<'a, T> {
    /// A cursor on the first of `terms`, which are in byte order.
    pub fn new(terms: &'a [T]) -> Self {
        SliceTerms { terms, pos: 0 }
    }
}

impl<T: AsRef<[u8]>> Sorted for SliceTerms<'_, T> {
    fn term(&self) -> Option<&[u8]> {
        self.terms.get(self.pos).map(|t| t.as_ref())
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn next(&mut self) {
        self.pos = (self.pos + 1).min(self.terms.len());
    }

    fn seek(&mut self, t: &[u8]) {
        self.pos += self.terms[self.pos.min(self.terms.len())..].partition_point(|x| x.as_ref() < t);
    }
}

/// The first byte string after every string starting with `p`, or `None`
/// when there is none (`p` is all `0xff`).
pub fn past_prefix(p: &[u8]) -> Option<Vec<u8>> {
    let mut s = p.to_vec();
    while let Some(last) = s.pop() {
        if last < 0xff {
            s.push(last + 1);
            return Some(s);
        }
    }
    None
}

/// What [`walk`] matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Match {
    /// Terms within `k` edits of the query, as [`within`] decides.
    Whole,
    /// Terms with a prefix within `k` edits of the query, whose first
    /// letter is the query's: a typed prefix with typos.
    Prefix,
}

/// Calls `hit(terms, positions, edits)` for the terms that `m` matches
/// against `q` within `k` edits, in order: each matching term for
/// [`Match::Whole`] (`terms` still on it, `positions` its own), each run
/// of terms starting with a prefix within `k` edits for [`Match::Prefix`]
/// (`terms` past it; the edits are that prefix's).
///
/// The dictionary is walked as a trie: one row of the edit-distance table
/// per letter of the path from the root, kept while the next term shares
/// that path, so a term costs only the rows of the letters after the ones
/// it shares with the previous term. A path whose row has no cell within
/// `k` cannot lead to a match, so the walk jumps past every term starting
/// with it, and a prefix match jumps past its run: the cost follows the
/// trie's nodes within `k` edits of `q`, not the dictionary's size.
pub fn walk<S: Sorted>(q: &[char], k: u8, m: Match, terms: &mut S, mut hit: impl FnMut(&S, std::ops::Range<usize>, u8)) {
    let (n, k) = (q.len(), k as usize);
    // The path's letters, their ends in bytes, and per depth j a row of
    // `w` cells: the edits from the path's first j letters to each prefix
    // of `q`, capped at k + 1 (all that matters past k).
    let w = n + 1;
    let cap = |x: usize| x.min(k + 1) as u8;
    let mut path: Vec<char> = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();
    let mut ends: Vec<usize> = vec![0];
    let mut rows: Vec<u8> = (0..=n).map(cap).collect();
    let first = match (m, q.first()) {
        (Match::Whole, _) => None,
        (Match::Prefix, None) => {
            // The empty prefix: every term.
            let start = terms.pos();
            skip_rest(terms);
            if terms.pos() > start {
                hit(terms, start..terms.pos(), 0);
            }
            return;
        }
        (Match::Prefix, Some(&c)) => {
            let first = c.encode_utf8(&mut [0; 4]).as_bytes().to_vec();
            terms.seek(&first);
            Some(first)
        }
    };
    while let Some(t) = terms.term() {
        if first.as_ref().is_some_and(|f| !t.starts_with(f)) {
            return;
        }
        // Keep the rows of the letters the path shares with this term.
        let shared = bytes.iter().zip(t).take_while(|(a, b)| a == b).count();
        let j = ends.partition_point(|&e| e <= shared) - 1;
        path.truncate(j);
        ends.truncate(j + 1);
        rows.truncate((j + 1) * w);
        bytes.truncate(ends[j]);
        let Ok(rest) = std::str::from_utf8(&t[ends[j]..]) else {
            // Not text: no letters to match.
            terms.next();
            continue;
        };
        // Past: no term starting with the path can match. Run: every term
        // starting with it does, with these edits.
        let (mut past, mut run) = (false, None);
        for c in rest.chars() {
            let d = path.len();
            let (at, prev) = (rows.len(), rows.len() - w);
            rows.push(cap(d + 1));
            let mut min = rows[at];
            for i in 1..=n {
                let mut x = (rows[prev + i] + 1).min(rows[at + i - 1] + 1).min(rows[prev + i - 1] + (q[i - 1] != c) as u8);
                // A swap of the path's last letter and this one.
                if d >= 1 && i >= 2 && q[i - 1] == path[d - 1] && q[i - 2] == c {
                    x = x.min(rows[prev - w + i - 2] + 1);
                }
                let x = x.min(k as u8 + 1);
                rows.push(x);
                min = min.min(x);
            }
            let (min, last) = (min as usize, rows[at + n] as usize);
            path.push(c);
            bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            ends.push(bytes.len());
            if min > k {
                past = true;
                break;
            }
            if m == Match::Prefix && last <= k {
                run = Some(last as u8);
                break;
            }
        }
        if !past && run.is_none() {
            // The whole term was read; a prefix match would have stopped.
            let d = rows[path.len() * w + n] as usize;
            if m == Match::Whole && d <= k {
                let p = terms.pos();
                hit(terms, p..p + 1, d as u8);
            }
            terms.next();
            continue;
        }
        let start = terms.pos();
        match past_prefix(&bytes) {
            Some(after) => terms.seek(&after),
            None => skip_rest(terms),
        }
        if let Some(d) = run {
            hit(terms, start..terms.pos(), d);
        }
    }
}

fn skip_rest(terms: &mut impl Sorted) {
    while terms.term().is_some() {
        terms.next();
    }
}

/// Whether `v` starts with a prefix within `k` edits of `q` whose first
/// letter is `q`'s ([`Match::Prefix`] for one value).
pub fn prefix_within(q: &[char], v: &str, k: u8) -> bool {
    let mut found = false;
    walk(q, k, Match::Prefix, &mut SliceTerms::new(&[v]), |_, _, _| found = true);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(a: &str, b: &str, k: u8) -> Option<u8> {
        within(&a.chars().collect::<Vec<_>>(), b, k)
    }

    #[test]
    fn distances() {
        assert_eq!(d("neural", "neural", 2), Some(0));
        assert_eq!(d("nueral", "neural", 2), Some(1)); // a swap
        assert_eq!(d("netwrk", "network", 2), Some(1));
        assert_eq!(d("grpah", "graph", 1), Some(1));
        assert_eq!(d("kitten", "sitting", 2), None);
        assert_eq!(d("kitten", "sitting", 3), Some(3));
        assert_eq!(d("ab", "abcd", 1), None);
        assert_eq!(d("", "ab", 2), Some(2));
    }

    /// Every term of a random dictionary, in order, that the walk matches
    /// is the full scan's, with its distance; and prefix runs are exactly
    /// the terms with a prefix within k.
    #[test]
    fn walks_equal_full_scans() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let letters: Vec<char> = "abcdeé日".chars().collect();
        let word = |next: &mut dyn FnMut() -> u64, max: u64| -> String { (0..next() % max).map(|_| letters[(next() % letters.len() as u64) as usize]).collect() };
        for round in 0..40 {
            let mut terms: Vec<String> = (0..300).map(|_| word(&mut next, 7)).collect();
            if round % 5 == 0 {
                terms.push(String::from_utf8_lossy(&[0xffu8, 0x61]).into_owned());
            }
            terms.sort();
            terms.dedup();
            for _ in 0..20 {
                let q: Vec<char> = word(&mut next, 6).chars().collect();
                for k in 0..=2u8 {
                    let want: Vec<(usize, u8)> = terms.iter().enumerate().filter_map(|(i, t)| within(&q, t, k).map(|d| (i, d))).collect();
                    let mut got = Vec::new();
                    walk(&q, k, Match::Whole, &mut SliceTerms::new(&terms), |_, r, d| got.push((r.start, d)));
                    assert_eq!(got, want, "{q:?} within {k}");
                    let want: Vec<usize> = (0..terms.len())
                        .filter(|&i| {
                            let t: Vec<char> = terms[i].chars().collect();
                            t.first() == q.first() && (1..=t.len()).any(|j| within(&q, &t[..j].iter().collect::<String>(), k).is_some())
                        })
                        .collect();
                    let mut got = Vec::new();
                    walk(&q, k, Match::Prefix, &mut SliceTerms::new(&terms), |_, r, _| got.extend(r));
                    let want = if q.is_empty() { (0..terms.len()).collect() } else { want };
                    assert_eq!(got, want, "{q:?} prefix within {k}");
                }
            }
        }
        assert!(prefix_within(&"hwo to ins".chars().collect::<Vec<_>>(), "how to install engram", 2));
        assert!(!prefix_within(&"wo to".chars().collect::<Vec<_>>(), "how to install", 2));
        assert_eq!(past_prefix(b"ab"), Some(b"ac".to_vec()));
        assert_eq!(past_prefix(&[0x61, 0xff]), Some(b"b".to_vec()));
        assert_eq!(past_prefix(&[0xff]), None);
    }
}
