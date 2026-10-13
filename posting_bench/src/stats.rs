//! Structural statistics of a collection and its query log: run lengths,
//! gap distribution, density per 2^16 chunk and per 256-docID block, and
//! query shape (driver size, size ratios). Used to motivate and size the
//! data-structure designs; printed by `posting-bench stats`.

use crate::data::Collection;
use std::fmt::Write as _;

/// Histogram helper: counts of values falling into `[edges[i], edges[i+1])`.
fn bucket(v: u64, edges: &[u64]) -> usize {
    edges.iter().rposition(|&e| v >= e).unwrap_or(0)
}

fn pct(a: u64, b: u64) -> f64 {
    100.0 * a as f64 / b.max(1) as f64
}

struct ListStats {
    postings: u64,
    lists: u64,
    runs: u64,
    in_runs: [u64; 4], // postings in runs of length >= 2, 8, 64, 256
    gaps: [u64; 5],    // gaps (d[i] - d[i-1]) == 1, < 16, < 256, < 65536, larger
    ef_bits: f64,      // Σ n (2 + log2(u/n))
    gap_entropy_bits: f64,
    chunk_kinds: [u64; 3],      // chunks that Roaring stores as array / bitmap / run
    chunk_kind_post: [u64; 3],  // postings in those chunks
    block_hist: [u64; 6],       // non-empty 256-blocks with 1, 2-3, 4-15, 16-31, 32-255, 256 postings
    block_hist_post: [u64; 6],
}

impl ListStats {
    fn new() -> Self {
        Self {
            postings: 0,
            lists: 0,
            runs: 0,
            in_runs: [0; 4],
            gaps: [0; 5],
            ef_bits: 0.0,
            gap_entropy_bits: 0.0,
            chunk_kinds: [0; 3],
            chunk_kind_post: [0; 3],
            block_hist: [0; 6],
            block_hist_post: [0; 6],
        }
    }

    fn add(&mut self, l: &[u32], universe: u32) {
        if l.is_empty() {
            return;
        }
        let n = l.len() as u64;
        self.postings += n;
        self.lists += 1;
        self.ef_bits += n as f64 * (2.0 + (universe as f64 / n as f64).log2().max(0.0).ceil());

        // Runs and gaps.
        let mut run = 1u64;
        let flush = |run: u64, s: &mut Self| {
            s.runs += 1;
            for (i, &t) in [2u64, 8, 64, 256].iter().enumerate() {
                if run >= t {
                    s.in_runs[i] += run;
                }
            }
        };
        let mut gap_hist = std::collections::HashMap::<u32, u64>::new();
        for w in l.windows(2) {
            let g = w[1] - w[0];
            self.gaps[bucket(g as u64, &[0, 2, 16, 256, 65536])] += 1;
            *gap_hist.entry(32 - g.leading_zeros()).or_default() += 1;
            if g == 1 {
                run += 1;
            } else {
                flush(run, self);
                run = 1;
            }
        }
        flush(run, self);
        // Entropy of gap bit-widths + the bits below the leading one: a cheap
        // stand-in for the empirical entropy of an Elias-gamma-like code.
        let total: u64 = gap_hist.values().sum();
        for (&w, &c) in &gap_hist {
            let p = c as f64 / total as f64;
            self.gap_entropy_bits += c as f64 * (-p.log2() + w.saturating_sub(1) as f64);
        }

        // Chunks (2^16) and blocks (256).
        let mut i = 0;
        while i < l.len() {
            let key = l[i] >> 16;
            let j = i + l[i..].partition_point(|&d| d >> 16 == key);
            let chunk = &l[i..j];
            let c = chunk.len() as u64;
            let runs = 1 + chunk.windows(2).filter(|w| w[1] != w[0] + 1).count() as u64;
            let array = 2 * c;
            let bitmap = 8192;
            let run_bytes = 2 + 4 * runs;
            let kind = if run_bytes < array.min(bitmap) { 2 } else if c <= 4096 { 0 } else { 1 };
            self.chunk_kinds[kind] += 1;
            self.chunk_kind_post[kind] += c;
            let mut a = 0;
            while a < chunk.len() {
                let b = chunk[a] >> 8;
                let e = a + chunk[a..].partition_point(|&d| d >> 8 == b);
                let cnt = (e - a) as u64;
                let k = bucket(cnt, &[1, 2, 4, 16, 32, 256]);
                self.block_hist[k] += 1;
                self.block_hist_post[k] += cnt;
                a = e;
            }
            i = j;
        }
    }

    fn render(&self, title: &str, out: &mut String) {
        let p = self.postings;
        let _ = writeln!(out, "\n### {title}\n");
        let _ = writeln!(out, "| | |\n|---|---|");
        let _ = writeln!(out, "| lists / postings | {} / {} |", self.lists, p);
        let _ = writeln!(out, "| Elias-Fano bound | {:.2} bits/posting |", self.ef_bits / p.max(1) as f64);
        let _ = writeln!(out, "| gap-width entropy (approx.) | {:.2} bits/posting |", self.gap_entropy_bits / p.max(1) as f64);
        let _ = writeln!(out, "| maximal runs | {} (mean length {:.2}) |", self.runs, p as f64 / self.runs.max(1) as f64);
        let _ = writeln!(
            out,
            "| postings in runs of ≥2 / ≥8 / ≥64 / ≥256 | {:.1}% / {:.1}% / {:.1}% / {:.1}% |",
            pct(self.in_runs[0], p),
            pct(self.in_runs[1], p),
            pct(self.in_runs[2], p),
            pct(self.in_runs[3], p)
        );
        let g: u64 = self.gaps.iter().sum();
        let _ = writeln!(
            out,
            "| gaps =1 / 2–15 / 16–255 / 256–65535 / ≥65536 | {:.1}% / {:.1}% / {:.1}% / {:.1}% / {:.1}% |",
            pct(self.gaps[0], g),
            pct(self.gaps[1], g),
            pct(self.gaps[2], g),
            pct(self.gaps[3], g),
            pct(self.gaps[4], g)
        );
        let names = ["array", "bitmap", "run"];
        let kinds: Vec<String> = (0..3)
            .map(|k| format!("{} {} ({:.1}% of postings)", self.chunk_kinds[k], names[k], pct(self.chunk_kind_post[k], p)))
            .collect();
        let _ = writeln!(out, "| Roaring containers (2^16 chunks) | {} |", kinds.join(", "));
        let labels = ["1", "2–3", "4–15", "16–31", "32–255", "256"];
        let blocks: u64 = self.block_hist.iter().sum();
        let hist: Vec<String> = (0..6)
            .map(|k| format!("{}: {:.1}% / {:.1}%", labels[k], pct(self.block_hist[k], blocks), pct(self.block_hist_post[k], p)))
            .collect();
        let _ = writeln!(out, "| non-empty 256-blocks | {} (mean {:.1} postings) |", blocks, p as f64 / blocks.max(1) as f64);
        let _ = writeln!(out, "| blocks by postings (share of blocks / of postings) | {} |", hist.join("; "));
    }
}

fn percentile(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// Markdown report of collection and workload structure.
pub fn report(coll: &Collection, queries: &[Vec<u32>]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## Structure of {}\n", coll.name);
    let mut all = ListStats::new();
    for l in &coll.lists {
        all.add(l, coll.universe);
    }
    let mut terms: Vec<u32> = queries.iter().flatten().copied().collect();
    terms.sort_unstable();
    terms.dedup();
    let mut ql = ListStats::new();
    for &t in &terms {
        ql.add(&coll.lists[t as usize], coll.universe);
    }
    all.render("All lists", &mut out);
    ql.render("Lists used by queries", &mut out);

    let mut driver = Vec::new();
    let mut ratio = Vec::new();
    let mut second = Vec::new();
    let mut k = Vec::new();
    for q in queries {
        let mut lens: Vec<f64> = q.iter().map(|&t| coll.lists[t as usize].len() as f64).collect();
        lens.sort_by(|a, b| a.total_cmp(b));
        driver.push(lens[0]);
        ratio.push(lens[lens.len() - 1] / lens[0].max(1.0));
        if lens.len() > 1 {
            second.push(lens[1] / lens[0].max(1.0));
        }
        k.push(q.len() as f64);
    }
    let _ = writeln!(out, "\n### Query shape\n");
    let _ = writeln!(out, "| | p10 | p50 | p90 |\n|---|---:|---:|---:|");
    for (name, v) in [
        ("terms", &mut k),
        ("shortest list (postings)", &mut driver),
        ("2nd shortest / shortest", &mut second),
        ("longest / shortest", &mut ratio),
    ] {
        let (a, b, c) = (percentile(v, 0.1), percentile(v, 0.5), percentile(v, 0.9));
        let _ = writeln!(out, "| {name} | {a:.0} | {b:.0} | {c:.0} |");
    }
    out
}
