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
}
