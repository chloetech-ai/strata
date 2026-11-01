// Scratch probe (not to commit): how well MUVERA encodings rank real
// ColBERT text vectors, and whether hashing centred vectors helps.
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::Path;

use strata::embed::{ColbertEmbedder, Embedder};

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

struct Mix(u64);
impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn normal(&mut self) -> f32 {
        let mut s = 0f64;
        for _ in 0..12 {
            s += (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        }
        (s - 6.0) as f32
    }
}

struct F {
    reps: usize,
    ksim: usize,
    dp: usize,
    dim: usize,
    planes: Vec<f32>,
    proj: Vec<f32>,
    center: Option<Vec<f32>>,
}

impl F {
    fn new(dim: usize, reps: usize, ksim: usize, dproj: usize, center: Option<Vec<f32>>) -> F {
        let mut g = Mix(0x6D75_7665_7261);
        let planes = (0..reps * ksim * dim).map(|_| g.normal()).collect();
        let s = 1.0 / (dproj as f32).sqrt();
        let proj = (0..reps * dproj * dim).map(|_| if g.next() >> 63 == 1 { s } else { -s }).collect();
        F { reps, ksim, dp: dproj, dim, planes, proj, center }
    }
    fn dims(&self) -> usize {
        self.reps * (1 << self.ksim) * self.dp
    }
    fn bucket(&self, r: usize, v: &[f32]) -> usize {
        let k = self.ksim;
        let c: Vec<f32> = match &self.center {
            Some(m) => v.iter().zip(m).map(|(x, y)| x - y).collect(),
            None => v.to_vec(),
        };
        (0..k).fold(0, |b, h| b | ((dot(&self.planes[(r * k + h) * self.dim..][..self.dim], &c) > 0.0) as usize) << h)
    }
    fn add(&self, r: usize, v: &[f32], out: &mut [f32]) {
        for (j, o) in out.iter_mut().enumerate() {
            *o += dot(&self.proj[(r * self.dp + j) * self.dim..][..self.dim], v);
        }
    }
    fn row(&self, vs: &[Vec<f32>], occupancy: &mut Vec<f64>) -> Vec<f32> {
        let (bc, dp) = (1usize << self.ksim, self.dp);
        let mut out = vec![0f32; self.dims()];
        let mut count = vec![0u32; bc];
        let mut buckets = vec![0usize; vs.len()];
        for r in 0..self.reps {
            count.iter_mut().for_each(|c| *c = 0);
            let base = r * bc * dp;
            for (i, v) in vs.iter().enumerate() {
                let b = self.bucket(r, v);
                buckets[i] = b;
                count[b] += 1;
                self.add(r, v, &mut out[base + b * dp..base + (b + 1) * dp]);
            }
            occupancy.push(*count.iter().max().unwrap() as f64 / vs.len() as f64);
            for b in 0..bc {
                let cell = &mut out[base + b * dp..base + (b + 1) * dp];
                if count[b] > 0 {
                    let inv = 1.0 / count[b] as f32;
                    cell.iter_mut().for_each(|x| *x *= inv);
                    continue;
                }
                let near = (0..vs.len()).min_by_key(|&i| ((buckets[i] ^ b).count_ones(), i)).unwrap();
                self.add(r, &vs[near], cell);
            }
        }
        out
    }
    fn query(&self, vs: &[Vec<f32>]) -> Vec<f32> {
        let (bc, dp) = (1usize << self.ksim, self.dp);
        let mut out = vec![0f32; self.dims()];
        for r in 0..self.reps {
            let base = r * bc * dp;
            for v in vs {
                let b = self.bucket(r, v);
                self.add(r, v, &mut out[base + b * dp..base + (b + 1) * dp]);
            }
        }
        out
    }
}

fn maxsim(q: &[Vec<f32>], d: &[Vec<f32>]) -> f32 {
    q.iter().map(|x| d.iter().map(|v| dot(x, v)).fold(f32::MIN, f32::max)).sum()
}

fn ndcg10(ranked: &[usize], ids: &[String], rel: &HashMap<String, f32>) -> f64 {
    let dcg: f64 = ranked.iter().take(10).enumerate().map(|(i, &d)| rel.get(&ids[d]).copied().unwrap_or(0.0) as f64 / ((i + 2) as f64).log2()).sum();
    let mut ideal: Vec<f32> = rel.values().copied().collect();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let idcg: f64 = ideal.iter().take(10).enumerate().map(|(i, &g)| g as f64 / ((i + 2) as f64).log2()).sum();
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn main() {
    let dir = Path::new("/home/user/data/beir/scifact");
    let n_extra: usize = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(700);
    let mut qrels: HashMap<String, HashMap<String, f32>> = HashMap::new();
    for line in std::io::BufReader::new(std::fs::File::open(dir.join("qrels/test.tsv")).unwrap()).lines().skip(1) {
        let line = line.unwrap();
        let f: Vec<&str> = line.split('\t').collect();
        qrels.entry(f[0].into()).or_default().insert(f[1].into(), f[2].parse().unwrap());
    }
    let mut queries = Vec::new();
    for line in std::io::BufReader::new(std::fs::File::open(dir.join("queries.jsonl")).unwrap()).lines() {
        let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = v["_id"].as_str().unwrap().to_string();
        if qrels.contains_key(&id) {
            queries.push((id, v["text"].as_str().unwrap().to_string()));
        }
    }
    let wanted: HashSet<String> = qrels.values().flat_map(|m| m.keys().cloned()).collect();
    let mut ids = Vec::new();
    let mut texts = Vec::new();
    let mut extra = 0;
    for line in std::io::BufReader::new(std::fs::File::open(dir.join("corpus.jsonl")).unwrap()).lines() {
        let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = v["_id"].as_str().unwrap().to_string();
        let keep = wanted.contains(&id) || (extra < n_extra && { extra += 1; true });
        if keep {
            texts.push(format!("{} {}", v["title"].as_str().unwrap_or(""), v["text"].as_str().unwrap_or("")));
            ids.push(id);
        }
    }
    eprintln!("{} queries, {} docs", queries.len(), ids.len());
    let m = ColbertEmbedder::open(Path::new("/home/user/data/models/answerai-colbert-small-v1")).unwrap();
    let t = std::time::Instant::now();
    let mut docs: Vec<Vec<Vec<f32>>> = Vec::new();
    for chunk in texts.chunks(16) {
        let refs: Vec<&str> = chunk.iter().map(|s| s.as_str()).collect();
        docs.extend(m.embed_tokens(&refs).unwrap());
    }
    eprintln!("embedded in {:?}; {:.0} vectors a doc", t.elapsed(), docs.iter().map(|d| d.len()).sum::<usize>() as f64 / docs.len() as f64);
    let qs: Vec<Vec<Vec<f32>>> = queries.iter().map(|(_, q)| m.embed_query_tokens(q).unwrap()).collect();
    let dim = docs[0][0].len();
    // Anisotropy: the mean doc vector's norm, and mean cosine between random pairs.
    let all: Vec<&Vec<f32>> = docs.iter().flatten().collect();
    let mut mean = vec![0f32; dim];
    for v in &all {
        for (m, x) in mean.iter_mut().zip(v.iter()) {
            *m += x / all.len() as f32;
        }
    }
    let mut g = Mix(7);
    let pairs: f32 = (0..20000).map(|_| dot(all[(g.next() % all.len() as u64) as usize], all[(g.next() % all.len() as u64) as usize])).sum::<f32>() / 20000.0;
    println!("dim {dim}, doc vectors {}, |mean| {:.3}, mean cosine of random pairs {:.3}", all.len(), dot(&mean, &mean).sqrt(), pairs);

    // Exact maxsim.
    let exact: Vec<Vec<f32>> = qs.iter().map(|q| docs.iter().map(|d| maxsim(q, d)).collect()).collect();
    let order = |s: &[f32]| {
        let mut o: Vec<usize> = (0..s.len()).collect();
        o.sort_by(|&a, &b| s[b].total_cmp(&s[a]));
        o
    };
    let exact_ord: Vec<Vec<usize>> = exact.iter().map(|s| order(s)).collect();
    let e_ndcg: f64 = queries.iter().zip(&exact_ord).map(|((id, _), o)| ndcg10(o, &ids, &qrels[id])).sum::<f64>() / queries.len() as f64;
    println!("exact maxsim nDCG@10 {e_ndcg:.4} over {} docs", ids.len());

    for &(reps, ksim, dproj) in &[(8, 4, 16), (8, 3, 16), (8, 5, 16), (20, 5, 16), (16, 4, 32)] {
        for centered in [false, true] {
            let f = F::new(dim, reps, ksim, dproj, centered.then(|| mean.clone()));
            let mut occ = Vec::new();
            let enc: Vec<Vec<f32>> = docs.iter().map(|d| f.row(d, &mut occ)).collect();
            let occ_mean = occ.iter().sum::<f64>() / occ.len() as f64;
            let mut line = format!("R{reps} k{ksim} d{dproj} ({} dims) {}: top bucket holds {:.0}% of a doc's vectors;", f.dims(), if centered { "centred" } else { "plain  " }, occ_mean * 100.0);
            for &n in &[10usize, 50, 100, 200] {
                let mut rec = 0f64;
                let mut nd = 0f64;
                for (qi, q) in qs.iter().enumerate() {
                    let qe = f.query(q);
                    let s: Vec<f32> = enc.iter().map(|e| dot(&qe, e)).collect();
                    let cand: Vec<usize> = order(&s).into_iter().take(n).collect();
                    let truth: HashSet<usize> = exact_ord[qi][..10].iter().copied().collect();
                    rec += cand.iter().filter(|c| truth.contains(c)).count() as f64 / 10.0;
                    let mut rr = cand.clone();
                    rr.sort_by(|&a, &b| exact[qi][b].total_cmp(&exact[qi][a]));
                    nd += ndcg10(&rr, &ids, &qrels[&queries[qi].0]);
                }
                line += &format!(" N={n}: recall {:.3} nDCG {:.4} |", rec / qs.len() as f64, nd / qs.len() as f64);
            }
            println!("{line}");
        }
    }
}
