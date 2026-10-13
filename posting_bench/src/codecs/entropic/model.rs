//! Gap symbolisation and context models.
//!
//! A gap `g = d[i] - d[i-1] - 1` becomes a *token* (one of [`NTOK`] = 64)
//! plus `NRAW[token]` raw bits: tokens 0..=3 are the exact values 0..=3;
//! larger gaps are classed by bit width `w` and the bit below the leading
//! one, `token = 4 + 2(w - 3) + bit`, leaving the `w - 2` low bits raw
//! ("hybrid uint" with split exponent 2 and one mantissa bit in the token).
//!
//! A [`Model`] has 1..=[`MAX_CTX`] contexts. The context of token `i` is a
//! function of token `i - 2` of the block (`ctx_of[t]`, a partition of the
//! token axis into contiguous ranges); the first two tokens of a block use
//! `start_ctx`. Skipping one token lets two tANS states decode alternate
//! tokens as independent chains (see [`LANES`]); on the query lists it
//! costs about 0.02 bits per posting of conditional entropy against
//! conditioning on token `i - 1`. Each context holds a frequency table
//! normalised to `2^r` for tANS, where the state size `r` is chosen per
//! list in [`R_MIN`]..=[`R_MAX`]: short lists take small tables (cheaper
//! initial states, and decode tables that stay in cache).
//!
//! Models come from one of three sources, chosen per list by total size:
//! * **Own** models, fitted to the list: the context partition is chosen by
//!   dynamic programming over the conditioning-token axis (minimising
//!   empirical conditional entropy plus model bits), and each context's
//!   frequencies are stored as 5-bit log-scale levels (half-bit steps) over
//!   the range of tokens it uses; see [`OwnModel`].
//! * **Shared** parametric models ([`SharedParams`]): two contexts (gap two
//!   back zero or not), each a zero-inflated geometric distribution. A list
//!   stores a 20-bit parameter index; decode tables are built once per
//!   process and shared.
//! * **Auto**: the shared family with parameters derived from the list
//!   length and the universe alone (a plain geometric), costing no index.

use crate::bits::BitVec;

/// Largest tANS state size (bits).
pub const R_MAX: u32 = 10;
/// Smallest tANS state size (bits).
pub const R_MIN: u32 = 7;
/// Largest tANS table (states per context).
pub const L_MAX: usize = 1 << R_MAX;
/// Number of tokens (fewer than `2^R_MIN`, so any table holds them all).
pub const NTOK: usize = 64;
/// Maximum number of contexts of a model (the largest decode table,
/// `MAX_CTX · L_MAX` entries of 8 bytes, then fits in 32 KiB of L1).
pub const MAX_CTX: usize = 4;
/// Interleaved tANS states per block. Token `i` is coded by state
/// `i % LANES`, in the context of token `i - LANES`, so every state is an
/// independent dependency chain.
pub const LANES: usize = 2;

/// Token of gap `g` and its number of raw bits.
#[inline(always)]
pub fn token(g: u32) -> (usize, u32) {
    if g < 4 {
        (g as usize, 0)
    } else {
        let w = 32 - g.leading_zeros();
        let top = (g >> (w - 2)) & 1;
        ((4 + 2 * (w - 3) + top) as usize, w - 2)
    }
}

/// Smallest gap of every token (its raw bits are ORed in).
pub const BASE: [u32; NTOK] = {
    let mut b = [0u32; NTOK];
    let mut t = 0;
    while t < NTOK {
        b[t] = if t < 4 {
            t as u32
        } else {
            let w = ((t - 4) / 2 + 3) as u32;
            let top = ((t - 4) & 1) as u32;
            (2 + top) << (w - 2)
        };
        t += 1;
    }
    b
};

/// Raw bits of every token.
pub const NRAW: [u32; NTOK] = {
    let mut r = [0u32; NTOK];
    let mut t = 4;
    while t < NTOK {
        r[t] = ((t - 4) / 2 + 1) as u32;
        t += 1;
    }
    r
};

/// Largest gap of token `t`.
#[inline]
pub fn token_max(t: usize) -> u32 {
    if NRAW[t] == 0 {
        BASE[t]
    } else {
        BASE[t] | ((1u64 << NRAW[t]) - 1) as u32
    }
}

/// A context model with normalised tANS frequencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    /// Context of the token [`LANES`] positions after token `t`.
    pub ctx_of: [u8; NTOK],
    /// Context of the first [`LANES`] tokens of a block.
    pub start_ctx: u8,
    /// Per context, frequencies summing to `2^r` (0 = token not codable).
    pub freq: Vec<[u16; NTOK]>,
    /// tANS state bits.
    pub r: u32,
}

impl Model {
    pub fn nctx(&self) -> usize {
        self.freq.len()
    }
}

/// Weight of level `q` (1..=31): `64 · 2^((q-1)/2)`, rounded.
#[inline]
fn level_weight(q: u8) -> u64 {
    debug_assert!((1..=31).contains(&q));
    let k = (q - 1) / 2;
    if (q - 1) % 2 == 0 {
        64u64 << k
    } else {
        // 64·√2 ≈ 92682 / 1024
        (92682u64 << k) >> 10
    }
}

/// Normalise non-negative integer weights to frequencies summing to `2^r`,
/// every positive weight getting at least 1. Deterministic. At most `2^r`
/// weights may be positive.
pub fn normalize(weights: &[u64; NTOK], r: u32) -> [u16; NTOK] {
    let l = 1u64 << r;
    let total: u64 = weights.iter().sum();
    let mut f = [0u16; NTOK];
    if total == 0 {
        return f;
    }
    let mut sum = 0i64;
    for t in 0..NTOK {
        if weights[t] > 0 {
            let v = ((weights[t] as u128 * l as u128 + total as u128 / 2) / total as u128) as u64;
            f[t] = v.clamp(1, l) as u16;
            sum += f[t] as i64;
        }
    }
    let mut diff = l as i64 - sum;
    while diff != 0 {
        // Adjust the largest frequency (first on ties).
        let (mut best, mut bf) = (0usize, 0u16);
        for (t, &v) in f.iter().enumerate() {
            if v > bf {
                best = t;
                bf = v;
            }
        }
        if diff > 0 {
            f[best] += diff as u16;
            diff = 0;
        } else {
            let take = (-diff).min((bf as i64 - 1) / 2 + 1).min(bf as i64 - 1);
            debug_assert!(take > 0);
            f[best] -= take as u16;
            diff += take;
        }
    }
    f
}

/// Cost in bits of coding `counts` with frequencies `f` summing to `2^r`
/// (∞ if a counted token has frequency 0).
pub fn coding_cost(counts: &[u32; NTOK], f: &[u16; NTOK], r: u32) -> f64 {
    let mut c = 0.0;
    for t in 0..NTOK {
        if counts[t] > 0 {
            if f[t] == 0 {
                return f64::INFINITY;
            }
            c += counts[t] as f64 * (r as f64 - (f[t] as f64).log2());
        }
    }
    c
}

/// Empirical entropy (bits) of a histogram.
fn entropy(counts: &[u32; NTOK]) -> f64 {
    let n: u64 = counts.iter().map(|&c| c as u64).sum();
    if n == 0 {
        return 0.0;
    }
    let nf = n as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| c as f64 * (nf / c as f64).log2()).sum()
}

/// Token histograms of a list: `joint[prev][t]` for tokens `t` that follow
/// `prev` by [`LANES`] positions inside a block (their context), `start[t]`
/// for the first [`LANES`] tokens of blocks.
pub struct Stats {
    pub joint: Box<[[u32; NTOK]; NTOK]>,
    pub start: [u32; NTOK],
    pub symbols: u64,
    /// Lanes started over all blocks (one initial state each).
    pub lanes: u64,
}

impl Stats {
    pub fn new() -> Self {
        Self { joint: Box::new([[0; NTOK]; NTOK]), start: [0; NTOK], symbols: 0, lanes: 0 }
    }

    /// Largest token seen (0 if none).
    pub fn max_token(&self) -> usize {
        (0..NTOK)
            .rev()
            .find(|&t| self.start[t] > 0 || self.joint.iter().any(|row| row[t] > 0))
            .unwrap_or(0)
    }

    /// Add one block's token sequence.
    pub fn add_block(&mut self, tokens: &[u8]) {
        for &t in tokens.iter().take(LANES) {
            self.start[t as usize] += 1;
        }
        for w in tokens.windows(LANES + 1) {
            self.joint[w[0] as usize][w[LANES] as usize] += 1;
        }
        self.symbols += tokens.len() as u64;
        self.lanes += tokens.len().min(LANES) as u64;
    }

    /// Histogram of tokens in context `c` of `ctx_of`/`start_ctx`.
    pub fn context_counts(&self, ctx_of: &[u8; NTOK], start_ctx: u8, nctx: usize) -> Vec<[u32; NTOK]> {
        let mut h = vec![[0u32; NTOK]; nctx];
        for p in 0..NTOK {
            let c = ctx_of[p] as usize;
            for t in 0..NTOK {
                h[c][t] += self.joint[p][t];
            }
        }
        for t in 0..NTOK {
            h[start_ctx as usize][t] += self.start[t];
        }
        h
    }

    /// Bits of the lanes' initial states with `r`-bit states (an upper
    /// bound: the final states give some of them back).
    pub fn state_bits(&self, r: u32) -> f64 {
        (self.lanes * r as u64) as f64
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}

/// A per-list model as stored: state size, context boundaries, start
/// context and per-context frequency levels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnModel {
    /// tANS state bits.
    pub r: u32,
    /// Increasing token boundaries: context `c` covers conditioning tokens
    /// `[bounds[c-1], bounds[c])`.
    pub bounds: Vec<u8>,
    pub start_ctx: u8,
    /// Per context: level (0 = absent, 1..=31) of every token.
    pub levels: Vec<[u8; NTOK]>,
}

/// Frequencies of `levels` normalised to `2^r`.
fn level_freq(lv: &[u8; NTOK], r: u32) -> [u16; NTOK] {
    let mut w = [0u64; NTOK];
    for t in 0..NTOK {
        if lv[t] > 0 {
            w[t] = level_weight(lv[t]);
        }
    }
    normalize(&w, r)
}

impl OwnModel {
    pub fn nctx(&self) -> usize {
        self.levels.len()
    }

    fn ctx_of(bounds: &[u8]) -> [u8; NTOK] {
        let mut ctx_of = [0u8; NTOK];
        for (t, c) in ctx_of.iter_mut().enumerate() {
            *c = bounds.iter().filter(|&&b| b as usize <= t).count() as u8;
        }
        ctx_of
    }

    /// The normalised model.
    pub fn model(&self) -> Model {
        let freq = self.levels.iter().map(|lv| level_freq(lv, self.r)).collect();
        Model { ctx_of: Self::ctx_of(&self.bounds), start_ctx: self.start_ctx, freq, r: self.r }
    }

    /// Serialised size in bits (see [`OwnModel::write`]).
    pub fn size_bits(&self) -> u64 {
        let mut bits = 2 + 2 + 6 * self.bounds.len() as u64 + 2;
        for lv in &self.levels {
            bits += level_span_bits(lv);
        }
        bits
    }

    /// Serialise: `r - R_MIN` (2 bits), `nctx - 1` (2 bits), boundaries
    /// (6 bits each), start context (2 bits), then per context the first
    /// and last used token (6 + 6 bits) and a 5-bit level for every token in
    /// between.
    pub fn write(&self, out: &mut BitVec) {
        out.push_bits((self.r - R_MIN) as u64, 2);
        out.push_bits((self.nctx() - 1) as u64, 2);
        for &b in &self.bounds {
            out.push_bits(b as u64, 6);
        }
        out.push_bits(self.start_ctx as u64, 2);
        for lv in &self.levels {
            let lo = lv.iter().position(|&q| q > 0).unwrap_or(0);
            let hi = lv.iter().rposition(|&q| q > 0).unwrap_or(0);
            out.push_bits(lo as u64, 6);
            out.push_bits(hi as u64, 6);
            for &q in &lv[lo..=hi] {
                out.push_bits(q as u64, 5);
            }
        }
    }

    /// Inverse of [`OwnModel::write`]; returns the model and the bits read.
    pub fn read(words: &[u64], pos: usize) -> (Self, usize) {
        let mut rd = crate::bits::BitReader::new(words, pos);
        let r = rd.read(2) as u32 + R_MIN;
        let nctx = rd.read(2) as usize + 1;
        let bounds: Vec<u8> = (1..nctx).map(|_| rd.read(6) as u8).collect();
        let start_ctx = rd.read(2) as u8;
        let levels = (0..nctx)
            .map(|_| {
                let lo = rd.read(6) as usize;
                let hi = rd.read(6) as usize;
                let mut lv = [0u8; NTOK];
                for q in lv[lo..=hi].iter_mut() {
                    *q = rd.read(5) as u8;
                }
                lv
            })
            .collect();
        (Self { r, bounds, start_ctx, levels }, rd.position() - pos)
    }
}

/// Serialised bits of one context's levels.
fn level_span_bits(lv: &[u8; NTOK]) -> u64 {
    let lo = lv.iter().position(|&q| q > 0).unwrap_or(0);
    let hi = lv.iter().rposition(|&q| q > 0).unwrap_or(0);
    12 + 5 * (hi - lo + 1) as u64
}

/// Estimated serialised bits of a context with histogram `h`.
fn span_bits_of_counts(h: &[u32; NTOK]) -> f64 {
    match (h.iter().position(|&c| c > 0), h.iter().rposition(|&c| c > 0)) {
        (Some(lo), Some(hi)) => 12.0 + 5.0 * (hi - lo + 1) as f64,
        _ => 12.0 + 5.0,
    }
}

/// Levels approximating histogram `h` (0 for absent tokens).
fn levels_of(h: &[u32; NTOK]) -> [u8; NTOK] {
    let mut lv = [0u8; NTOK];
    let max = h.iter().copied().max().unwrap_or(0);
    if max == 0 {
        lv[0] = 31;
        return lv;
    }
    for t in 0..NTOK {
        if h[t] > 0 {
            let q = 31.0 + 2.0 * (h[t] as f64 / max as f64).log2();
            lv[t] = q.round().clamp(1.0, 31.0) as u8;
        }
    }
    lv
}

/// Refine levels greedily: try moving each level by ±1 when it lowers the
/// coding cost of `h` with `2^r` states.
fn refine_levels(h: &[u32; NTOK], lv: &mut [u8; NTOK], r: u32) {
    let cost = |lv: &[u8; NTOK]| coding_cost(h, &level_freq(lv, r), r);
    let mut best = cost(lv);
    for _ in 0..2 {
        let mut improved = false;
        for t in 0..NTOK {
            if lv[t] == 0 {
                continue;
            }
            for d in [-1i32, 1] {
                let q = lv[t] as i32 + d;
                if !(1..=31).contains(&q) {
                    continue;
                }
                let old = lv[t];
                lv[t] = q as u8;
                let c = cost(lv);
                if c + 1e-9 < best {
                    best = c;
                    improved = true;
                } else {
                    lv[t] = old;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Fit an own model to `stats`; returns it with its exact total cost in bits
/// (coded tokens + model + initial states, excluding raw bits).
pub fn fit_own(stats: &Stats) -> (OwnModel, f64) {
    // Restrict the conditioning-token axis to tokens that occur there.
    let used: Vec<usize> = (0..NTOK).filter(|&p| stats.joint[p].iter().any(|&c| c > 0)).collect();
    let k = used.len().max(1);
    // cost[i][j]: entropy + model bits of merging used rows i..j (exclusive).
    let mut cost = vec![vec![0.0f64; k + 1]; k + 1];
    for i in 0..k {
        let mut acc = [0u32; NTOK];
        for j in i..k {
            if let Some(&p) = used.get(j) {
                for t in 0..NTOK {
                    acc[t] += stats.joint[p][t];
                }
            }
            cost[i][j + 1] = entropy(&acc) + span_bits_of_counts(&acc) + 6.0;
        }
    }
    // dp[c][j]: best cost of the first j used rows in c contexts.
    let inf = f64::INFINITY;
    let mut dp = vec![vec![inf; k + 1]; MAX_CTX + 1];
    let mut from = vec![vec![0usize; k + 1]; MAX_CTX + 1];
    dp[0][0] = 0.0;
    for c in 1..=MAX_CTX {
        for j in 1..=k {
            for i in (c - 1)..j {
                let v = dp[c - 1][i] + cost[i][j];
                if v < dp[c][j] {
                    dp[c][j] = v;
                    from[c][j] = i;
                }
            }
        }
    }
    let mut best: Option<(OwnModel, f64)> = None;
    for c in 1..=MAX_CTX.min(k) {
        if dp[c][k] == inf {
            continue;
        }
        if let Some((_, b)) = &best {
            // The estimate is a lower bound up to rounding; skip clearly worse.
            if dp[c][k] > b + 64.0 {
                continue;
            }
        }
        // Recover the cut points (indices into `used`).
        let mut cuts = Vec::with_capacity(c);
        let mut j = k;
        for cc in (1..=c).rev() {
            let i = from[cc][j];
            cuts.push(i);
            j = i;
        }
        cuts.reverse(); // cuts[0] == 0
        // Boundaries on the token axis: the first used token of each range.
        let bounds: Vec<u8> = cuts[1..].iter().map(|&i| used[i] as u8).collect();
        let ctx_of = OwnModel::ctx_of(&bounds);
        // Start context: the one whose histogram absorbs the starts cheapest.
        let hist = stats.context_counts(&ctx_of, 0, c);
        let mut no_start = hist.clone();
        for t in 0..NTOK {
            no_start[0][t] -= stats.start[t];
        }
        let mut start_ctx = 0u8;
        let mut best_inc = inf;
        for cc in 0..c {
            let mut h = no_start[cc];
            for t in 0..NTOK {
                h[t] += stats.start[t];
            }
            let inc = entropy(&h) - entropy(&no_start[cc]);
            if inc < best_inc {
                best_inc = inc;
                start_ctx = cc as u8;
            }
        }
        let hist = stats.context_counts(&ctx_of, start_ctx, c);
        // Levels refined for the largest table, then every state size.
        let levels: Vec<[u8; NTOK]> = hist
            .iter()
            .map(|h| {
                let mut lv = levels_of(h);
                refine_levels(h, &mut lv, R_MAX);
                lv
            })
            .collect();
        for r in R_MIN..=R_MAX {
            let m = OwnModel { r, bounds: bounds.clone(), start_ctx, levels: levels.clone() };
            let model = m.model();
            let data: f64 = hist.iter().zip(&model.freq).map(|(h, f)| coding_cost(h, f, r)).sum();
            let total = data + m.size_bits() as f64 + stats.state_bits(r);
            if best.as_ref().is_none_or(|(_, b)| total < *b) {
                best = Some((m, total));
            }
        }
    }
    best.expect("at least one context")
}

/// Shared parametric model: two contexts (gap two back zero / non-zero),
/// each a zero-inflated geometric distribution over gaps, with every token
/// up to `max_tok` codable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SharedParams {
    /// Geometric mean `2^(mean/2) - 1`, 0..=63.
    pub mean: u8,
    /// Zero-inflation level of each context, 0..=7.
    pub pi: [u8; 2],
    /// Largest codable token (the list's largest).
    pub max_tok: u8,
    /// tANS state bits.
    pub r: u8,
}

/// Zero-inflation probabilities.
const PI: [f64; 8] = [0.0, 0.2, 0.4, 0.55, 0.7, 0.82, 0.92, 0.97];
/// Bits of a shared-model index: mean (6), two inflation levels (2 × 3),
/// the largest token (6) and the state size (2).
pub const SHARED_INDEX_BITS: u64 = 20;

impl SharedParams {
    /// Frequencies of one context.
    pub fn freq(mean: u8, pi: u8, max_tok: u8, r: u32) -> [u16; NTOK] {
        let m = (2f64).powf(mean as f64 / 2.0) - 1.0 + 1e-3;
        let q = m / (1.0 + m);
        let ln_q = q.ln();
        let pi = PI[pi as usize];
        // P(g >= a) = q^a for the geometric part.
        let tail = |a: u64| -> f64 { (a as f64 * ln_q).exp() };
        let mut w = [0u64; NTOK];
        for t in 0..=max_tok as usize {
            let (a, b) = (BASE[t] as u64, token_max(t) as u64);
            let mut p = (1.0 - pi) * (tail(a) - tail(b + 1));
            if t == 0 {
                p += pi;
            }
            // Integer weights with a floor so every token stays codable.
            w[t] = ((p * (1u64 << 40) as f64) as u64).max(1);
        }
        normalize(&w, r)
    }

    pub fn model(&self) -> Model {
        let mut ctx_of = [1u8; NTOK];
        ctx_of[0] = 0;
        let r = self.r as u32;
        Model {
            ctx_of,
            start_ctx: 1,
            freq: vec![Self::freq(self.mean, self.pi[0], self.max_tok, r), Self::freq(self.mean, self.pi[1], self.max_tok, r)],
            r,
        }
    }

    /// Best parameters for `stats` and their cost in bits (coded tokens and
    /// initial states, excluding raw bits and the index).
    pub fn fit(stats: &Stats, max_tok: u8) -> (Self, f64) {
        let mut ctx_of = [1u8; NTOK];
        ctx_of[0] = 0;
        let h = stats.context_counts(&ctx_of, 1, 2);
        let nz: [Vec<(usize, f32)>; 2] =
            std::array::from_fn(|c| (0..NTOK).filter(|&t| h[c][t] > 0).map(|t| (t, h[c][t] as f32)).collect());
        let mut best = (SharedParams { mean: 0, pi: [0, 0], max_tok, r: R_MAX as u8 }, f64::INFINITY);
        for r in R_MIN..=R_MAX {
            let costs = shared_costs(max_tok, r);
            let states = stats.state_bits(r);
            for mean in 0..64u8 {
                let mut c = [f32::INFINITY; 2];
                let mut arg = [0u8; 2];
                for ctx in 0..2 {
                    for pi in 0..8u8 {
                        let tab = &costs[mean as usize * 8 + pi as usize];
                        let v: f32 = nz[ctx].iter().map(|&(t, n)| n * tab[t]).sum();
                        if v < c[ctx] {
                            c[ctx] = v;
                            arg[ctx] = pi;
                        }
                    }
                }
                let total = c[0] as f64 + c[1] as f64 + states;
                if total < best.1 {
                    best = (SharedParams { mean, pi: arg, max_tok, r: r as u8 }, total);
                }
            }
        }
        // Exact cost of the winner.
        best.1 = best.0.cost(stats);
        best
    }

    /// Parameter-free default for a list of `n` postings in `[0, universe)`:
    /// a plain geometric distribution with the list's mean gap, every token
    /// below the universe codable, and a state size growing with `n`. Costs
    /// no model bits beyond the selector.
    pub fn auto(universe: u32, n: usize) -> Self {
        let m = (universe as f64 - n as f64).max(0.0) / n.max(1) as f64;
        let mean = (2.0 * (m + 1.0).log2()).round().clamp(0.0, 63.0) as u8;
        let max_tok = token(universe.max(1) - 1).0 as u8;
        let r = match n {
            0..=64 => 7,
            65..=256 => 8,
            257..=1024 => 9,
            _ => 10,
        };
        SharedParams { mean, pi: [0, 0], max_tok, r: r as u8 }
    }

    /// Exact cost of `stats` (bits of coded tokens and initial states,
    /// excluding raw bits).
    pub fn cost(&self, stats: &Stats) -> f64 {
        let m = self.model();
        let h = stats.context_counts(&m.ctx_of, m.start_ctx, 2);
        coding_cost(&h[0], &m.freq[0], m.r) + coding_cost(&h[1], &m.freq[1], m.r) + stats.state_bits(m.r)
    }
}

/// Per-token code lengths (bits) of every context distribution of the
/// shared family, indexed by `mean · 8 + pi`; computed once per `max_tok`
/// and state size.
fn shared_costs(max_tok: u8, r: u32) -> &'static [[f32; NTOK]] {
    use std::sync::OnceLock;
    const NR: usize = (R_MAX - R_MIN + 1) as usize;
    static CACHE: OnceLock<Vec<OnceLock<Box<[[f32; NTOK]]>>>> = OnceLock::new();
    let cells = CACHE.get_or_init(|| (0..NTOK * NR).map(|_| OnceLock::new()).collect());
    cells[max_tok as usize * NR + (r - R_MIN) as usize].get_or_init(|| {
        (0..64 * 8)
            .map(|i| {
                let f = SharedParams::freq((i / 8) as u8, (i % 8) as u8, max_tok, r);
                std::array::from_fn(|t| if f[t] == 0 { f32::INFINITY } else { r as f32 - (f[t] as f32).log2() })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_cover_all_gaps() {
        for g in (0..10_000u32).chain([u32::MAX - 1, u32::MAX >> 1, 1 << 31, (1 << 31) - 1, 3 << 30]) {
            let (t, nraw) = token(g);
            assert!(t < NTOK);
            assert_eq!(nraw, NRAW[t], "g={g}");
            assert!(BASE[t] <= g && g <= token_max(t), "g={g} t={t}");
            let raw = if nraw == 0 { 0 } else { g & ((1u32 << nraw) - 1) };
            assert_eq!(BASE[t] | raw, g);
        }
        assert_eq!(token(u32::MAX - 1).0, NTOK - 1);
        for t in 1..NTOK {
            assert_eq!(BASE[t], token_max(t - 1) + 1);
        }
    }

    #[test]
    fn normalize_sums_to_table_size() {
        for r in R_MIN..=R_MAX {
            let mut w = [0u64; NTOK];
            w[0] = 1 << 40;
            for t in 1..NTOK {
                w[t] = 1;
            }
            let f = normalize(&w, r);
            assert_eq!(f.iter().map(|&x| x as usize).sum::<usize>(), 1 << r);
            assert!(f.iter().all(|&x| x >= 1));
            let mut w = [0u64; NTOK];
            w[5] = 3;
            let f = normalize(&w, r);
            assert_eq!(f[5] as usize, 1 << r);
        }
    }

    #[test]
    fn own_model_serialisation_roundtrip() {
        for len in [300u32, 5000, 100_000] {
            let mut s = Stats::new();
            let toks: Vec<u8> = (0..len).map(|i| ((i * 7919) % 97 % 40) as u8).collect();
            for b in toks.chunks(127) {
                s.add_block(b);
            }
            let (m, _) = fit_own(&s);
            let mut bv = BitVec::new();
            bv.push_bits(5, 3);
            m.write(&mut bv);
            assert_eq!(bv.len() as u64, 3 + m.size_bits());
            let (back, used) = OwnModel::read(bv.words(), 3);
            assert_eq!(used as u64, m.size_bits());
            assert_eq!(back, m);
            let model = m.model();
            for f in &model.freq {
                assert_eq!(f.iter().map(|&x| x as usize).sum::<usize>(), 1 << model.r);
            }
        }
    }

    #[test]
    fn shared_models_code_every_token() {
        for (universe, n) in [(1u32, 1usize), (1000, 10), (8_000_000, 3), (u32::MAX, 2), (u32::MAX, 100_000)] {
            let p = SharedParams::auto(universe, n);
            let m = p.model();
            for f in &m.freq {
                assert_eq!(f.iter().map(|&x| x as usize).sum::<usize>(), 1 << m.r);
                assert!(f[..=p.max_tok as usize].iter().all(|&x| x > 0));
            }
        }
    }
}
