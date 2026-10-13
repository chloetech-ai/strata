//! Roaring bitmaps (Chambi, Lemire, Kaser and Godin, "Better bitmap
//! performance with Roaring bitmaps", SPE 2016; Lemire, Ssi-Yan-Kai and
//! Kaser, "Consistently faster and smaller compressed bitmaps with Roaring",
//! SPE 2016; Lemire et al., "Roaring Bitmaps: Implementation of an Optimized
//! Software Library", SPE 2018, i.e. CRoaring).
//!
//! The docID space is cut into chunks of 2^16 by the high 16 bits. Each
//! non-empty chunk stores its 16-bit key (keys are sorted) and a container of
//! the low 16 bits:
//! * array: sorted `u16`, at most 4096 of them;
//! * bitmap: 2^16 bits (1024 x `u64`) plus a cached cardinality;
//! * run: maximal `(start, length - 1)` pairs of `u16`.
//!
//! At build time each chunk takes its smallest serialized form: run if that
//! is smaller than both the array and the bitmap form, otherwise array when
//! the chunk holds at most 4096 values, otherwise bitmap.
//!
//! The cursor gallops over the keys only when the target leaves the current
//! chunk, then runs a container-local `next_geq`: galloping in arrays, a word
//! scan in bitmaps, galloping over the run ends in run containers.
//!
//! Set operations follow CRoaring:
//! * Intersection is SvS: inputs are sorted by cardinality and intersected
//!   pairwise, smallest first, with intermediate results kept as Roaring
//!   containers, stopping as soon as the result is empty. Container kernels:
//!   array ∩ array by galloping when one side is more than 64x larger, else
//!   by a SIMD block kernel (see [`simd`]); array ∩ bitmap by bit probes;
//!   bitmap ∩ bitmap by word AND with popcount, extracting an array when at
//!   most 4096 values remain; run ∩ array with a moving pointer; run ∩ bitmap
//!   by masking; run ∩ run by interval intersection.
//! * Union is a k-way merge over the keys. For each key a single container is
//!   copied, run-only groups union their intervals, groups of at most 4096
//!   values in total are merged as arrays (SIMD merge, see [`simd`]), and
//!   anything else is ORed into one bitmap (word OR, array values set bits,
//!   runs set ranges) whose cardinality is computed once at the end ("lazy"
//!   union) and which turns back into an array if it holds at most 4096.
//!
//! Turning bitmaps into values (decoding, bitmap ∩ bitmap into an array)
//! uses a branch-light extraction kernel, `vpcompressb`-based with
//! AVX512-VBMI2 (see [`simd::BitPos`]).
//!
//! [`Roaring::and_native`] and [`Roaring::or_native`] return Roaring results
//! without materialising `u32`s, as CRoaring benchmarks do. The [`SetOps`]
//! implementations stream the last step straight into the `u32` output.

pub mod container;
pub mod simd;

use super::{Cursor, CursorList, PostingList, SetOps};
use crate::END;
use container::{AndOut, Container, Run, Scratch, Words, WORDS};
use simd::{gallop, gallop_by};
use std::cell::RefCell;

#[derive(Clone, Debug, Default)]
pub struct Roaring {
    keys: Vec<u16>,
    containers: Vec<Container>,
    card: u64,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// Runs `f` with this thread's kernel buffers (fresh ones if already in use).
fn with_scratch<R>(f: impl FnOnce(&mut Scratch) -> R) -> R {
    SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut s) => f(&mut s),
        Err(_) => f(&mut Scratch::default()),
    })
}

/// Calls `f(key, a_container, b_container)` for every key present in both,
/// galloping over the keys of whichever side is behind.
#[inline]
fn for_each_common<'a>(a: &'a Roaring, b: &'a Roaring, mut f: impl FnMut(u16, &'a Container, &'a Container)) {
    let (ka, kb) = (&a.keys, &b.keys);
    let (mut i, mut j) = (0, 0);
    while i < ka.len() && j < kb.len() {
        let (x, y) = (ka[i], kb[j]);
        if x == y {
            f(x, &a.containers[i], &b.containers[j]);
            i += 1;
            j += 1;
        } else if x < y {
            i = gallop(ka, i + 1, y);
        } else {
            j = gallop(kb, j + 1, x);
        }
    }
}

/// The lists sorted by cardinality, smallest first.
fn by_cardinality<'a>(lists: &[&'a Roaring]) -> Vec<&'a Roaring> {
    let mut v = lists.to_vec();
    v.sort_by_key(|l| l.card);
    v
}

impl Roaring {
    /// Number of docIDs.
    #[inline]
    pub fn cardinality(&self) -> u64 {
        self.card
    }

    /// Number of array, bitmap and run containers.
    pub fn container_counts(&self) -> [usize; 3] {
        let mut n = [0; 3];
        for c in &self.containers {
            n[match c {
                Container::Array(_) => 0,
                Container::Bitmap(..) => 1,
                Container::Run(_) => 2,
            }] += 1;
        }
        n
    }

    /// Size of the portable Roaring serialization (RoaringFormatSpec), in
    /// bytes, for `n` containers:
    /// * cookie: 4 bytes, plus a 4-byte container count when there are no
    ///   run containers;
    /// * run-flag bitset: `ceil(n / 8)` bytes, only with run containers;
    /// * descriptive header: 4 bytes per container (key, cardinality - 1);
    /// * offset header: 4 bytes per container, omitted when there are run
    ///   containers and `n < 4`;
    /// * payload: array `2 * card`, bitmap 8192, run `2 + 4 * runs` bytes.
    pub fn serialized_bytes(&self) -> usize {
        let n = self.containers.len();
        let has_runs = self.containers.iter().any(|c| matches!(c, Container::Run(_)));
        let cookie = if has_runs { 4 + n.div_ceil(8) } else { 8 };
        let offsets = if has_runs && n < 4 { 0 } else { 4 * n };
        let payload: usize = self.containers.iter().map(Container::serialized_bytes).sum();
        cookie + 4 * n + offsets + payload
    }

    #[inline]
    fn push(&mut self, key: u16, c: Container) {
        self.card += c.card() as u64;
        self.keys.push(key);
        self.containers.push(c);
    }

    /// Intersection of all `lists` as a Roaring bitmap (empty for no lists).
    pub fn and_native(lists: &[&Roaring]) -> Roaring {
        match lists {
            [] => Roaring::default(),
            [l] => (*l).clone(),
            _ => with_scratch(|s| {
                let order = by_cardinality(lists);
                let mut acc = and2(order[0], order[1], s);
                for l in &order[2..] {
                    if acc.keys.is_empty() {
                        break;
                    }
                    and_assign(&mut acc, l, s);
                }
                acc
            }),
        }
    }

    /// Union of all `lists` as a Roaring bitmap (empty for no lists).
    pub fn or_native(lists: &[&Roaring]) -> Roaring {
        match lists {
            [] => Roaring::default(),
            [l] => (*l).clone(),
            _ => with_scratch(|s| {
                let mut r = Roaring::default();
                union_groups(lists, s, |key, res, s| r.push(key, res.materialize(s)));
                r
            }),
        }
    }
}

/// `a ∩ b` as a new bitmap.
fn and2(a: &Roaring, b: &Roaring, s: &mut Scratch) -> Roaring {
    let mut r = Roaring::default();
    for_each_common(a, b, |key, x, y| {
        if let Some(c) = container::and(x, y, s).materialize(s) {
            r.push(key, c);
        }
    });
    r
}

/// `acc &= b`, reusing `acc`'s array buffers.
fn and_assign(acc: &mut Roaring, b: &Roaring, s: &mut Scratch) {
    let (mut w, mut j, mut card) = (0, 0, 0u64);
    for i in 0..acc.keys.len() {
        let key = acc.keys[i];
        j = gallop(&b.keys, j, key);
        match b.keys.get(j) {
            None => break,
            Some(&k) if k != key => continue,
            Some(_) => {}
        }
        let c = &mut acc.containers[i];
        match container::and(c, &b.containers[j], s) {
            AndOut::Empty => continue,
            AndOut::Array => match c {
                Container::Array(v) => {
                    v.clear();
                    v.extend_from_slice(&s.arr);
                }
                _ => *c = Container::Array(s.arr.clone()),
            },
            AndOut::Same(x) => {
                if !std::ptr::eq(x, c) {
                    *c = x.clone();
                }
            }
            other => *c = other.materialize(s).unwrap(),
        }
        card += c.card() as u64;
        acc.containers.swap(w, i);
        acc.keys[w] = key;
        w += 1;
    }
    acc.keys.truncate(w);
    acc.containers.truncate(w);
    acc.card = card;
}

/// Appends `a ∩ b` to `out` without building the result bitmap.
fn and_decode(a: &Roaring, b: &Roaring, s: &mut Scratch, out: &mut Vec<u32>) {
    for_each_common(a, b, |key, x, y| container::and(x, y, s).decode((key as u32) << 16, s, out));
}

/// k-way merge over the keys of `lists`: calls `emit(key, union, scratch)`
/// once per key, in order.
fn union_groups<'a>(
    lists: &[&'a Roaring],
    s: &mut Scratch,
    mut emit: impl FnMut(u16, container::OrOut<'a>, &mut Scratch),
) {
    let mut pos = vec![0usize; lists.len()];
    let mut group: Vec<&'a Container> = Vec::with_capacity(lists.len());
    loop {
        let mut min = u32::MAX;
        for (l, &p) in lists.iter().zip(&pos) {
            if let Some(&k) = l.keys.get(p) {
                min = min.min(k as u32);
            }
        }
        if min == u32::MAX {
            return;
        }
        let key = min as u16;
        group.clear();
        for (l, p) in lists.iter().zip(pos.iter_mut()) {
            if l.keys.get(*p) == Some(&key) {
                group.push(&l.containers[*p]);
                *p += 1;
            }
        }
        let res = container::or_many(&group, s);
        emit(key, res, s);
    }
}

impl PostingList for Roaring {
    const NAME: &'static str = "Roaring";

    fn build(docs: &[u32], _universe: u32) -> Self {
        let mut r = Roaring::default();
        let mut i = 0;
        while i < docs.len() {
            let key = docs[i] >> 16;
            let end = i + docs[i..].partition_point(|&d| d >> 16 == key);
            r.push(key as u16, Container::from_chunk(&docs[i..end]));
            i = end;
        }
        r
    }

    fn len(&self) -> usize {
        self.card as usize
    }

    /// `8 *` [`Roaring::serialized_bytes`].
    fn size_bits(&self) -> u64 {
        8 * self.serialized_bytes() as u64
    }

    fn decode_into(&self, out: &mut Vec<u32>) {
        out.reserve(self.card as usize);
        for (&k, c) in self.keys.iter().zip(&self.containers) {
            c.decode((k as u32) << 16, out);
        }
    }
}

impl SetOps for Roaring {
    fn intersect(lists: &[&Self], out: &mut Vec<u32>) {
        match lists {
            [] => {}
            [l] => l.decode_into(out),
            _ => with_scratch(|s| {
                let order = by_cardinality(lists);
                let (last, rest) = order.split_last().unwrap();
                if let [first] = rest {
                    return and_decode(first, last, s, out);
                }
                let mut acc = and2(rest[0], rest[1], s);
                for l in &rest[2..] {
                    if acc.keys.is_empty() {
                        return;
                    }
                    and_assign(&mut acc, l, s);
                }
                and_decode(&acc, last, s, out);
            }),
        }
    }

    fn union(lists: &[&Self], out: &mut Vec<u32>) {
        match lists {
            [] => {}
            [l] => l.decode_into(out),
            _ => with_scratch(|s| {
                out.reserve(lists.iter().map(|l| l.card).max().unwrap_or(0) as usize);
                union_groups(lists, s, |key, res, s| res.decode((key as u32) << 16, s, out));
            }),
        }
    }
}

/// Position inside the current container.
enum State<'a> {
    Array {
        data: &'a [u16],
        pos: usize,
    },
    /// `rest`: the bits of `words[wi]` above the current value.
    Bitmap {
        words: &'a Words,
        wi: usize,
        rest: u64,
    },
    /// `last`: the last docID of the current run.
    Run {
        runs: &'a [Run],
        ri: usize,
        last: u32,
    },
    Done,
}

pub struct RoaringCursor<'a> {
    list: &'a Roaring,
    /// Current container index.
    ci: usize,
    /// Current key << 16.
    base: u32,
    cur: u32,
    state: State<'a>,
}

/// Moves a bitmap position to the lowest bit of `rest`, scanning the
/// following words if it is empty. Returns false past the last word.
#[inline]
fn bitmap_advance(base: u32, words: &Words, wi: &mut usize, rest: &mut u64, cur: &mut u32) -> bool {
    let mut w = *rest;
    while w == 0 {
        *wi += 1;
        if *wi == WORDS {
            return false;
        }
        w = words[*wi];
    }
    *cur = base | ((*wi as u32) << 6) | w.trailing_zeros();
    *rest = w & (w - 1);
    true
}

impl<'a> RoaringCursor<'a> {
    /// Positions the cursor on the first value of container `ci` (or END).
    #[inline]
    fn load(&mut self, ci: usize) {
        self.ci = ci;
        let Some(c) = self.list.containers.get(ci) else {
            self.cur = END;
            self.state = State::Done;
            return;
        };
        self.base = (self.list.keys[ci] as u32) << 16;
        match c {
            Container::Array(a) => {
                self.cur = self.base | a[0] as u32;
                self.state = State::Array { data: a, pos: 0 };
            }
            Container::Bitmap(w, _) => {
                let (mut wi, mut rest) = (0, w[0]);
                bitmap_advance(self.base, w, &mut wi, &mut rest, &mut self.cur);
                self.state = State::Bitmap { words: w, wi, rest };
            }
            Container::Run(r) => {
                self.cur = self.base | r[0].start as u32;
                self.state = State::Run { runs: r, ri: 0, last: self.base | r[0].last() as u32 };
            }
        }
    }
}

impl Cursor for RoaringCursor<'_> {
    #[inline]
    fn docid(&self) -> u32 {
        self.cur
    }

    #[inline]
    fn next(&mut self) -> u32 {
        let found = match &mut self.state {
            State::Array { data, pos } => {
                *pos += 1;
                match data.get(*pos) {
                    Some(&v) => {
                        self.cur = self.base | v as u32;
                        true
                    }
                    None => false,
                }
            }
            State::Bitmap { words, wi, rest } => bitmap_advance(self.base, words, wi, rest, &mut self.cur),
            State::Run { runs, ri, last } => {
                if self.cur < *last {
                    self.cur += 1;
                    true
                } else {
                    *ri += 1;
                    match runs.get(*ri) {
                        Some(r) => {
                            self.cur = self.base | r.start as u32;
                            *last = self.base | r.last() as u32;
                            true
                        }
                        None => false,
                    }
                }
            }
            State::Done => return END,
        };
        if !found {
            self.load(self.ci + 1);
        }
        self.cur
    }

    #[inline]
    fn next_geq(&mut self, target: u32) -> u32 {
        if target <= self.cur {
            return self.cur; // also covers END
        }
        if target >> 16 != self.base >> 16 {
            let ci = gallop(&self.list.keys, self.ci + 1, (target >> 16) as u16);
            self.load(ci);
            if target <= self.cur {
                return self.cur;
            }
        }
        // Same chunk as `target`, and the current value is below it.
        let low = target & 0xFFFF;
        let found = match &mut self.state {
            State::Array { data, pos } => {
                *pos = gallop(data, *pos + 1, low as u16);
                match data.get(*pos) {
                    Some(&v) => {
                        self.cur = self.base | v as u32;
                        true
                    }
                    None => false,
                }
            }
            State::Bitmap { words, wi, rest } => {
                let twi = (low >> 6) as usize;
                let w = if twi == *wi { *rest } else { words[twi] };
                *wi = twi;
                *rest = w & (!0u64 << (low & 63));
                bitmap_advance(self.base, words, wi, rest, &mut self.cur)
            }
            State::Run { runs, ri, last } => {
                if target <= *last {
                    self.cur = target;
                    true
                } else {
                    *ri = gallop_by(runs, *ri + 1, |r| (r.last() as u32) < low);
                    match runs.get(*ri) {
                        Some(r) => {
                            self.cur = self.base | low.max(r.start as u32);
                            *last = self.base | r.last() as u32;
                            true
                        }
                        None => false,
                    }
                }
            }
            State::Done => return END,
        };
        if !found {
            self.load(self.ci + 1);
        }
        self.cur
    }

    fn size(&self) -> usize {
        self.list.len()
    }
}

impl CursorList for Roaring {
    type Cursor<'a> = RoaringCursor<'a>;

    fn cursor(&self) -> RoaringCursor<'_> {
        let mut c = RoaringCursor { list: self, ci: 0, base: 0, cur: END, state: State::Done };
        c.load(0);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::conformance;
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::collections::BTreeSet;

    #[test]
    fn conformance() {
        conformance::check_roundtrip::<Roaring>();
        conformance::check_cursor::<Roaring>();
        conformance::check_setops::<Roaring>();
    }

    #[test]
    fn container_choice_and_size() {
        // One array container of 3 values, no runs: 8 + 4 + 4 + 6 bytes.
        let r = Roaring::build(&[1, 5, 9], 100);
        assert_eq!(r.container_counts(), [1, 0, 0]);
        assert_eq!(r.size_bits(), 8 * 22);
        // One run container, fewer than 4 containers: no offsets.
        // 4 + 1 (run flags) + 4 + (2 + 4) bytes.
        let r = Roaring::build(&(0..100).collect::<Vec<_>>(), 100);
        assert_eq!(r.container_counts(), [0, 0, 1]);
        assert_eq!(r.size_bits(), 8 * 15);
        // A full chunk and a partial one are runs.
        let r = Roaring::build(&(0..100_000).collect::<Vec<_>>(), 100_000);
        assert_eq!(r.container_counts(), [0, 0, 2]);
        // Dense random chunk: bitmap; sparse: array.
        let mut rng = StdRng::seed_from_u64(5);
        let dense = conformance::random_sorted(&mut rng, 30_000, 65_536);
        assert_eq!(Roaring::build(&dense, 65_536).container_counts(), [0, 1, 0]);
        let sparse = conformance::random_sorted(&mut rng, 4096, 65_536);
        assert_eq!(Roaring::build(&sparse, 65_536).container_counts(), [1, 0, 0]);
        let sparse = conformance::random_sorted(&mut rng, 4097, 65_536);
        assert_eq!(Roaring::build(&sparse, 65_536).container_counts(), [0, 1, 0]);
        // Runs win only when smaller than the alternatives: 1000 runs of 2
        // (2 + 4000 bytes) lose to an array of 2000 values (4000 bytes).
        let pairs: Vec<u32> = (0..1000).flat_map(|i| [4 * i, 4 * i + 1]).collect();
        assert_eq!(Roaring::build(&pairs, 65_536).container_counts(), [1, 0, 0]);
        let triples: Vec<u32> = (0..1000).flat_map(|i| [4 * i, 4 * i + 1, 4 * i + 2]).collect();
        assert_eq!(Roaring::build(&triples, 65_536).container_counts(), [0, 0, 1]);
    }

    /// Lists over 12 chunks where each list picks, per chunk, an array, a
    /// bitmap, runs (short or long), a full chunk, or nothing, so every pair
    /// of container types meets in some chunk.
    fn mixed_lists(rng: &mut StdRng, count: usize) -> Vec<Vec<u32>> {
        (0..count)
            .map(|_| {
                let mut docs = BTreeSet::new();
                for key in 0..12u32 {
                    let base = key << 16;
                    match rng.gen_range(0..6) {
                        0 => {
                            let n = rng.gen_range(1..4000);
                            for _ in 0..n {
                                docs.insert(base + rng.gen_range(0..65_536));
                            }
                        }
                        1 => {
                            let n = rng.gen_range(5000..40_000);
                            for _ in 0..n {
                                docs.insert(base + rng.gen_range(0..65_536));
                            }
                        }
                        2 => {
                            for _ in 0..rng.gen_range(1..200) {
                                let s = rng.gen_range(0..65_000);
                                docs.extend(base + s..base + s + rng.gen_range(1..400));
                            }
                        }
                        3 => {
                            for _ in 0..rng.gen_range(1..6) {
                                let s = rng.gen_range(0..60_000);
                                docs.extend(base + s..base + s + rng.gen_range(1000..5000));
                            }
                        }
                        4 => docs.extend(base..base + 65_536),
                        _ => {}
                    }
                }
                docs.into_iter().collect()
            })
            .collect()
    }

    #[test]
    fn mixed_container_setops() {
        let mut rng = StdRng::seed_from_u64(11);
        let lists = mixed_lists(&mut rng, 8);
        let universe = 12 << 16;
        let built: Vec<Roaring> = lists.iter().map(|l| Roaring::build(l, universe)).collect();
        let totals = built.iter().fold([0; 3], |mut t, r| {
            for (x, y) in t.iter_mut().zip(r.container_counts()) {
                *x += y;
            }
            t
        });
        assert!(totals.iter().all(|&n| n > 5), "container mix {totals:?}");

        for _ in 0..200 {
            let k = rng.gen_range(1..=4);
            let idx: Vec<usize> = (0..k).map(|_| rng.gen_range(0..lists.len())).collect();
            let refs: Vec<&Roaring> = idx.iter().map(|&i| &built[i]).collect();
            let mut want_and = lists[idx[0]].clone();
            for &i in &idx[1..] {
                want_and.retain(|x| lists[i].binary_search(x).is_ok());
            }
            let want_or: Vec<u32> =
                idx.iter().flat_map(|&i| lists[i].iter().copied()).collect::<BTreeSet<_>>().into_iter().collect();

            let and = Roaring::and_native(&refs);
            assert_eq!(and.cardinality(), want_and.len() as u64, "and card {idx:?}");
            let mut got = Vec::new();
            and.decode_into(&mut got);
            assert!(got == want_and, "and_native {idx:?}");
            got.clear();
            Roaring::intersect(&refs, &mut got);
            assert!(got == want_and, "intersect {idx:?}");

            let or = Roaring::or_native(&refs);
            assert_eq!(or.cardinality(), want_or.len() as u64, "or card {idx:?}");
            got.clear();
            or.decode_into(&mut got);
            assert!(got == want_or, "or_native {idx:?}");
            got.clear();
            Roaring::union(&refs, &mut got);
            assert!(got == want_or, "union {idx:?}");

            // Native results are valid Roaring bitmaps: their cursors agree.
            for r in [&and, &or] {
                let mut cur = r.cursor();
                let mut n = 0u64;
                while cur.docid() != END {
                    n += 1;
                    cur.next();
                }
                assert_eq!(n, r.cardinality());
            }
        }
    }

    #[test]
    fn run_container_kernels() {
        use container::{and, or_many, Scratch};
        let runs = |v: &[(u16, u16)]| Container::Run(v.iter().map(|&(s, l)| Run { start: s, len: l }).collect());
        let values = |c: &Container| {
            let mut out = Vec::new();
            c.decode(0, &mut out);
            out
        };
        let a = runs(&[(0, 9), (20, 0), (100, 899), (65_000, 535)]);
        let b = runs(&[(5, 20), (500, 99), (65_530, 5)]);
        let arr = Container::Array(vec![0, 3, 10, 20, 21, 150, 999, 1000, 65_535]);
        let dense: Vec<u32> = (0..65_536).filter(|x| x % 3 != 0).collect();
        let bitmap = Container::from_chunk(&dense);
        assert!(matches!(bitmap, Container::Bitmap(..)));
        let full = Container::full();
        let mut s = Scratch::default();

        let set = |c: &Container| values(c).into_iter().collect::<BTreeSet<u32>>();
        for x in [&a, &b, &arr, &bitmap, &full] {
            for y in [&a, &b, &arr, &bitmap, &full] {
                let (sx, sy) = (set(x), set(y));
                let want: Vec<u32> = sx.intersection(&sy).copied().collect();
                let got = and(x, y, &mut s).materialize(&s).map_or(Vec::new(), |c| values(&c));
                assert_eq!(got, want);
                let mut got = Vec::new();
                and(x, y, &mut s).decode(0, &s, &mut got);
                assert_eq!(got, want);

                let want: Vec<u32> = sx.union(&sy).copied().collect();
                let got = values(&or_many(&[x, y], &mut s).materialize(&mut s));
                assert_eq!(got, want);
                let mut got = Vec::new();
                or_many(&[x, y], &mut s).decode(0, &s, &mut got);
                assert_eq!(got, want);
            }
        }

        // run ∩ run keeps runs; touching runs coalesce in unions.
        let r = and(&a, &b, &mut s).materialize(&s).unwrap();
        assert!(matches!(&r, Container::Run(v) if v.len() == 4), "{r:?}");
        let c = runs(&[(10, 9)]);
        let u = or_many(&[&a, &c], &mut s).materialize(&mut s);
        assert!(matches!(&u, Container::Run(v) if v[0] == Run { start: 0, len: 20 }), "{u:?}");
        // Unions that fill a chunk become a full run container.
        let evens = Container::from_chunk(&(0..65_536).step_by(2).collect::<Vec<_>>());
        let odds = Container::from_chunk(&(1..65_536).step_by(2).collect::<Vec<_>>());
        assert!(or_many(&[&evens, &odds], &mut s).materialize(&mut s).is_full());
    }

    #[test]
    fn cursor_on_run_and_bitmap_edges() {
        // Runs ending at the chunk boundary and values at the top of the
        // docID space.
        let mut docs: Vec<u32> = (65_000..65_536).chain(65_540..65_600).collect();
        docs.extend((1 << 20..(1 << 20) + 30_000).filter(|x| x % 7 != 0));
        docs.extend([u32::MAX - 3, u32::MAX - 2, u32::MAX - 1]);
        let r = Roaring::build(&docs, u32::MAX);
        let mut cur = r.cursor();
        for &t in &[0, 65_535, 65_536, 65_539, 65_541, 1 << 20, (1 << 20) + 7, (1 << 20) + 29_999] {
            let want = docs[docs.partition_point(|&x| x < t)];
            assert_eq!(cur.next_geq(t), want, "next_geq({t})");
        }
        assert_eq!(cur.next_geq(u32::MAX - 2), u32::MAX - 2);
        assert_eq!(cur.next(), u32::MAX - 1);
        assert_eq!(cur.next(), END);
        assert_eq!(cur.next_geq(5), END);
    }

    /// Throwaway timing against the `roaring` crate on two random sets of
    /// 1M docIDs in a 10M universe.
    /// `cargo test --release -- --ignored --nocapture timing`
    #[test]
    #[ignore = "timing only"]
    fn timing_vs_roaring_crate() {
        use std::time::Instant;
        fn median_us<R>(reps: usize, mut f: impl FnMut() -> R) -> f64 {
            let mut t: Vec<f64> = (0..reps)
                .map(|_| {
                    let s = Instant::now();
                    std::hint::black_box(f());
                    s.elapsed().as_secs_f64() * 1e6
                })
                .collect();
            t.sort_by(f64::total_cmp);
            t[reps / 2]
        }
        let mut rng = StdRng::seed_from_u64(99);
        let (n, u) = (1_000_000, 10_000_000);
        let a = conformance::random_sorted(&mut rng, n, u);
        let b = conformance::random_sorted(&mut rng, n, u);
        let (ra, rb) = (Roaring::build(&a, u), Roaring::build(&b, u));
        let xa = ::roaring::RoaringBitmap::from_sorted_iter(a.iter().copied()).unwrap();
        let xb = ::roaring::RoaringBitmap::from_sorted_iter(b.iter().copied()).unwrap();
        assert_eq!(Roaring::and_native(&[&ra, &rb]).cardinality(), (&xa & &xb).len());
        assert_eq!(Roaring::or_native(&[&ra, &rb]).cardinality(), (&xa | &xb).len());
        println!("containers per list: {:?}", ra.container_counts());

        let reps = 200;
        let mut out = Vec::new();
        let rows = [
            ("ours  AND native", median_us(reps, || Roaring::and_native(&[&ra, &rb]))),
            ("crate AND (&a & &b)", median_us(reps, || &xa & &xb)),
            ("ours  OR  native", median_us(reps, || Roaring::or_native(&[&ra, &rb]))),
            ("crate OR  (&a | &b)", median_us(reps, || &xa | &xb)),
            (
                "ours  intersect -> Vec<u32>",
                median_us(reps, || {
                    out.clear();
                    Roaring::intersect(&[&ra, &rb], &mut out);
                    out.len()
                }),
            ),
            ("crate AND -> Vec<u32>", median_us(reps, || (&xa & &xb).iter().collect::<Vec<u32>>().len())),
            (
                "ours  union -> Vec<u32>",
                median_us(reps, || {
                    out.clear();
                    Roaring::union(&[&ra, &rb], &mut out);
                    out.len()
                }),
            ),
            ("crate OR  -> Vec<u32>", median_us(reps, || (&xa | &xb).iter().collect::<Vec<u32>>().len())),
        ];
        for (name, us) in rows {
            println!("{name:<30} {us:>9.1} us");
        }
    }
}
