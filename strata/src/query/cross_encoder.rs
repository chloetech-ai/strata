//! Cross-encoders: a model that reads a query and a row's text together
//! and scores how well the text answers the query,
//! `cross_encoder('name', field, ..., :q)`. Far better than BM25 at telling
//! the best of a hundred rows apart, and far slower (milliseconds a row on
//! a CPU), so a query ranks by one only the top rows of a cheaper ranking,
//! in a global rerank phase that runs once per query: on the database, or
//! on the coordinator over the text its shards fetched
//! ([`crate::cluster::rerank_models`]).
//!
//! A model is a Hugging Face directory under the models directory, as the
//! BERT embedders' are ([`crate::embed`]): `config.json`, `tokenizer.json`,
//! and its weights, `model.onnx` or `model.safetensors`. A `model.onnx`, any
//! sequence classifier exported to ONNX with one label (int8 ones, as
//! Hugging Face repositories ship them under `onnx/`, among them), runs
//! through ONNX Runtime (feature `onnx`) on the CPU, its rows sorted by
//! length into batches so that little is padding. A `model.safetensors`
//! runs through candle (feature `bert`), in `f32`: a
//! `BertForSequenceClassification`, such as `ms-marco-MiniLM-L-6-v2`, or an
//! `XLMRobertaForSequenceClassification`, such as `bge-reranker-base`, with
//! one label. The score is the label's logit, as sentence-transformers'
//! `CrossEncoder.predict` gives it with an identity activation (`sigmoid()`
//! of it is the probability). The text is the fields' values joined by a
//! space, NULLs left out, and the query and text together are cut to
//! [`MAX_TOKENS`] tokens, the longer first, unless the directory's
//! `cross_encoder.json` says `{"max_tokens": N}` (at most the model's
//! length).
//!
//! The model's hash, FNV-1a of its files, goes into the plans that use it,
//! as a GBDT model's does ([`crate::query::model`]).
//!
//! A model may instead be served elsewhere: a provider's, named
//! `cohere/rerank-v3.5`, `voyage/rerank-2.5` or
//! `jina/jina-reranker-v2-base-multilingual` (its key in `COHERE_API_KEY`,
//! `VOYAGE_API_KEY` or `JINA_API_KEY`), or a server of one of their APIs or
//! of Hugging Face TEI's (a GPU serving a model too slow for a CPU) that the
//! model directory's `endpoint.json` names
//! ([`crate::hosted::RerankEndpoint`]). Each request scores a batch of rows
//! (feature `hosted`, on by default). A provider's score is its
//! `relevance_score`, between 0 and 1; TEI's is the logit, as here. The
//! hash is of the endpoint: its API, URL and model, not its secret.
//!
//! `builtin:overlap` needs no files or feature: the share of the query's
//! distinct words that the text holds, for tests and demos.

use std::io::Read;
use std::path::Path;

use crate::error::{Error, Result};
use crate::value::Value;

/// The files of a cross-encoder run by candle, in its directory.
pub const FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// The weights of a cross-encoder run by ONNX Runtime, in its directory
/// in place of `model.safetensors`.
pub const ONNX: &str = "model.onnx";

/// A model directory's options for the model ([module docs](self)).
pub const OPTIONS: &str = "cross_encoder.json";

/// The tokens a query and a row's text are cut to together, unless a
/// model's options say otherwise: about the length of a passage's first
/// paragraph, and half the cost of BERT's 512.
pub const MAX_TOKENS: usize = 256;

/// The built-in cross-encoder ([module docs](self)).
pub const OVERLAP: &str = "builtin:overlap";

/// The file of a model directory naming a server that runs the model
/// ([module docs](self)).
pub const ENDPOINT: &str = "endpoint.json";

/// Whether the cross-encoder `name` of the directory `dir` is served
/// elsewhere: `dir` holds an `endpoint.json`, or holds no weights and
/// `name` is a provider's ([`crate::hosted::is_reranker_name`]).
pub fn is_hosted(name: &str, dir: &Path) -> bool {
    dir.join(ENDPOINT).exists() || (crate::hosted::is_reranker_name(name) && !dir.join("model.safetensors").exists())
}

/// The files the cross-encoder `name` of `dir` is read from: what a cache
/// of open models watches for changes. The options file is among them
/// whether or not it exists.
pub fn files(name: &str, dir: &Path) -> Vec<std::path::PathBuf> {
    if is_hosted(name, dir) {
        return vec![dir.join(ENDPOINT)];
    }
    let weights = if dir.join(ONNX).exists() { ONNX } else { FILES[2] };
    [FILES[0], FILES[1], weights, OPTIONS].iter().map(|f| dir.join(f)).collect()
}

/// The tokens a model's options cut a query and text to ([module docs](self)).
fn max_tokens(name: &str, dir: &Path) -> Result<usize> {
    let path = dir.join(OPTIONS);
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(MAX_TOKENS) };
    let bad = |m: &str| model_err(name, format!("{}: {m}", path.display()));
    let j: serde_json::Value = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
    let o = j.as_object().ok_or_else(|| bad("not an object"))?;
    if let Some(k) = o.keys().find(|k| *k != "max_tokens") {
        return Err(bad(&format!("unknown key {k:?} (max_tokens)")));
    }
    match o.get("max_tokens") {
        None => Ok(MAX_TOKENS),
        Some(v) => v.as_u64().filter(|&n| (8..=1 << 16).contains(&n)).map(|n| n as usize).ok_or_else(|| bad("max_tokens is a number of tokens, from 8")),
    }
}

/// Cross-encoders `strata-server fetch-model` knows by name: name, Hugging
/// Face repository, and what it is.
pub const KNOWN_MODELS: &[(&str, &str, &str)] = &[
    ("ms-marco-MiniLM-L-6-v2", "cross-encoder/ms-marco-MiniLM-L6-v2", "BERT cross-encoder, 22M parameters, English (feature bert)"),
    ("ms-marco-MiniLM-L-6-v2-int8", "cross-encoder/ms-marco-MiniLM-L6-v2", "the same in int8, from its repository's ONNX exports (feature onnx)"),
    ("bge-reranker-base", "BAAI/bge-reranker-base", "XLM-RoBERTa cross-encoder, 278M parameters, English and Chinese (feature bert)"),
    ("gte-reranker-modernbert-base-int8", "Alibaba-NLP/gte-reranker-modernbert-base", "ModernBERT cross-encoder, 149M parameters, English, in int8 (feature onnx)"),
];

/// The files `strata-server fetch-model` downloads of the cross-encoder
/// `name` of [`KNOWN_MODELS`]: each one's path in the repository, and its
/// name in the model's directory. An int8 model's export is the one made
/// for this CPU: ARM64's on ARM, AVX-512 VNNI's on an x86-64 that has
/// VNNI, AVX2's (unsigned activations, which do not saturate without
/// VNNI) on one that does not.
pub fn fetch_files(name: &str) -> Option<Vec<(&'static str, &'static str)>> {
    #[cfg(target_arch = "x86_64")]
    let vnni = std::arch::is_x86_feature_detected!("avx512vnni");
    #[cfg(not(target_arch = "x86_64"))]
    let vnni = false;
    let onnx = match name {
        "ms-marco-MiniLM-L-6-v2-int8" if cfg!(target_arch = "aarch64") => "onnx/model_qint8_arm64.onnx",
        "ms-marco-MiniLM-L-6-v2-int8" if vnni => "onnx/model_qint8_avx512_vnni.onnx",
        "ms-marco-MiniLM-L-6-v2-int8" => "onnx/model_quint8_avx2.onnx",
        "gte-reranker-modernbert-base-int8" => "onnx/model_int8.onnx",
        n if KNOWN_MODELS.iter().any(|m| m.0 == n) => return Some(FILES.iter().map(|&f| (f, f)).collect()),
        _ => return None,
    };
    Some(vec![(FILES[0], FILES[0]), (FILES[1], FILES[1]), (onnx, ONNX)])
}

/// Whether the model directory `dir` holds a cross-encoder: its
/// `config.json` names a sequence classifier.
pub fn is_cross_encoder(dir: &Path) -> bool {
    let config = std::fs::read_to_string(dir.join("config.json")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    config.is_some_and(|c| c["architectures"][0].as_str().is_some_and(|a| a.ends_with("ForSequenceClassification")))
}

/// Pairs scored at a time: a model's dense layers run over the tokens of
/// a whole batch at once, and the deadline is checked between batches.
const BATCH: usize = 16;

/// A cross-encoder ([module docs](self)).
pub struct CrossEncoder {
    pub name: String,
    /// FNV-1a of the model's files, as 16 hex digits.
    pub hash: String,
    net: Net,
}

enum Net {
    Overlap,
    #[cfg(feature = "bert")]
    Bert(bert::Bert),
    #[cfg(feature = "onnx")]
    Onnx(onnx::Onnx),
    #[cfg(feature = "hosted")]
    Hosted(crate::hosted::HostedReranker),
}

fn model_err(name: &str, what: impl std::fmt::Display) -> Error {
    Error::Query(format!("cross-encoder {name}: {what}"))
}

impl CrossEncoder {
    /// The cross-encoder `name` of the directory `dir` (`<models>/<name>`),
    /// or the built-in one.
    pub fn open(name: &str, dir: &Path) -> Result<CrossEncoder> {
        if name == OVERLAP {
            return Ok(CrossEncoder { name: name.into(), hash: "builtin".into(), net: Net::Overlap });
        }
        if is_hosted(name, dir) {
            return Self::hosted(name, dir);
        }
        let mut h = Fnv::default();
        for path in files(name, dir) {
            match std::fs::File::open(&path) {
                Ok(file) => h.read(file)?,
                Err(_) if path.ends_with(OPTIONS) => {}
                Err(_) => return Err(Error::Query(format!("no cross-encoder named {name}: no file {}", path.display()))),
            }
        }
        let hash = format!("{:016x}", h.0);
        let max_tokens = max_tokens(name, dir)?;
        if dir.join(ONNX).exists() {
            #[cfg(feature = "onnx")]
            return Ok(CrossEncoder { name: name.into(), hash, net: Net::Onnx(onnx::Onnx::open(dir, max_tokens).map_err(|e| model_err(name, e))?) });
            #[cfg(not(feature = "onnx"))]
            return Err(model_err(name, "a model.onnx runs through ONNX Runtime: build Strata with the `onnx` feature"));
        }
        #[cfg(feature = "bert")]
        return Ok(CrossEncoder { name: name.into(), hash, net: Net::Bert(bert::Bert::open(name, dir, max_tokens)?) });
        #[cfg(not(feature = "bert"))]
        {
            let _ = (hash, max_tokens);
            Err(model_err(name, "a model.safetensors runs through candle: build Strata with the `bert` feature"))
        }
    }

    /// A model served elsewhere ([module docs](self)).
    fn hosted(name: &str, dir: &Path) -> Result<CrossEncoder> {
        let path = dir.join(ENDPOINT);
        let config = match path.exists() {
            true => {
                let text = std::fs::read_to_string(&path).map_err(|e| model_err(name, format!("{}: {e}", path.display())))?;
                Some(serde_json::from_str::<serde_json::Value>(&text).map_err(|e| model_err(name, format!("{}: {e}", path.display())))?)
            }
            false => None,
        };
        let ep = crate::hosted::RerankEndpoint::from_config(name, config.as_ref(), &|k| std::env::var(k).ok()).map_err(|e| model_err(name, e.to_string().trim_start_matches(&format!("model {name}: "))))?;
        let mut h = Fnv::default();
        h.read(format!("{:?}\n{}\n{}", ep.api, ep.url, ep.model).as_bytes())?;
        let hash = format!("{:016x}", h.0);
        #[cfg(feature = "hosted")]
        return Ok(CrossEncoder { name: name.into(), hash, net: Net::Hosted(crate::hosted::HostedReranker::new(ep)) });
        #[cfg(not(feature = "hosted"))]
        {
            let _ = hash;
            Err(model_err(name, "a model served elsewhere is called over HTTP: build Strata with the `hosted` feature"))
        }
    }

    /// The model's score of each text for `query`, in order. `check` runs
    /// between batches (a query's deadline).
    pub fn score(&self, query: &str, texts: &[&str], check: &mut dyn FnMut() -> Result<()>) -> Result<Vec<f64>> {
        #[cfg(feature = "hosted")]
        if let Net::Hosted(m) = &self.net {
            return m.score(query, texts, check);
        }
        #[cfg(feature = "onnx")]
        if let Net::Onnx(m) = &self.net {
            let mut out = vec![0.0; texts.len()];
            for batch in m.batches(query, texts, BATCH).map_err(|e| model_err(&self.name, e))? {
                check()?;
                for (i, x) in m.run(&batch).map_err(|e| model_err(&self.name, e))? {
                    out[i] = x;
                }
            }
            return Ok(out);
        }
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            check()?;
            match &self.net {
                Net::Overlap => out.extend(batch.iter().map(|t| overlap(query, t))),
                #[cfg(feature = "bert")]
                Net::Bert(m) => out.extend(m.score(query, batch).map_err(|e| model_err(&self.name, e))?),
                #[cfg(feature = "hosted")]
                Net::Hosted(_) => unreachable!("scored above"),
                #[cfg(feature = "onnx")]
                Net::Onnx(_) => unreachable!("scored above"),
            }
        }
        Ok(out)
    }
}

/// The text a cross-encoder reads of a row: its values of the fields it
/// names, in order, joined by a space; NULLs add nothing and a list adds
/// each of its values. A database and a coordinator over shards read it
/// alike.
pub fn text_of(values: impl IntoIterator<Item = Value>) -> String {
    let mut parts = Vec::new();
    for v in values {
        match v {
            Value::Str(t) => parts.push(t),
            Value::List(vs) => parts.extend(vs.into_iter().filter_map(|v| match v {
                Value::Str(t) => Some(t),
                _ => None,
            })),
            _ => {}
        }
    }
    parts.join(" ")
}

/// The share of the distinct words of `query` (runs of letters and digits,
/// lowercased) that `text` holds; 0 for a query without words.
fn overlap(query: &str, text: &str) -> f64 {
    use std::collections::HashSet;
    let words = |s: &str| s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect::<HashSet<_>>();
    let q = words(query);
    if q.is_empty() {
        return 0.0;
    }
    let t = words(text);
    q.iter().filter(|w| t.contains(*w)).count() as f64 / q.len() as f64
}

/// 64-bit FNV-1a over everything read, as a GBDT model's hash is.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Fnv {
        Fnv(0xcbf29ce484222325)
    }
}

impl Fnv {
    fn read(&mut self, mut r: impl Read) -> Result<()> {
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            self.0 = buf[..n].iter().fold(self.0, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
        }
    }
}

#[cfg(feature = "onnx")]
mod onnx {
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use ort::environment::Environment;
    use ort::session::builder::GraphOptimizationLevel;
    use ort::session::Session;
    use ort::value::Tensor;
    use serde_json::Value as Json;

    type Res<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    /// ONNX Runtime's environment, made once in a process.
    fn environment() -> Res<&'static Environment> {
        static ENV: OnceLock<std::result::Result<Environment, String>> = OnceLock::new();
        ENV.get_or_init(|| ort::init().with_name("strata").build().map_err(|e| e.to_string())).as_ref().map_err(|e| e.clone().into())
    }

    /// A sequence classifier exported to ONNX, and its tokenizer.
    pub struct Onnx {
        tokenizer: tokenizers::Tokenizer,
        /// Runs take turns: each runs on every core.
        session: Mutex<Session>,
        /// Whether the graph takes `token_type_ids` (BERT's do, XLM-R's not).
        types: bool,
        pad: u32,
    }

    /// Pairs scored in one run: their indices, and their tokens.
    pub struct Batch {
        rows: Vec<usize>,
        encodings: Vec<tokenizers::Encoding>,
    }

    impl Onnx {
        /// The model of `dir`, cutting a query and a text to `max_tokens`
        /// together (or the model's own length, if shorter).
        pub fn open(dir: &Path, max_tokens: usize) -> Res<Onnx> {
            let c: Json = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json"))?)?;
            let roberta = c["model_type"].as_str().is_some_and(|t| t.contains("roberta"));
            let pad = c["pad_token_id"].as_u64().unwrap_or(if roberta { 1 } else { 0 }) as u32;
            // XLM-RoBERTa's positions start after its padding id.
            let positions = c["max_position_embeddings"].as_u64().map_or(512, |n| n as usize);
            let max_tokens = if roberta { positions.saturating_sub(pad as usize + 1) } else { positions }.min(max_tokens);
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
            let session = Session::builder(environment()?)
                .map_err(|e| e.to_string())?
                .with_optimization_level(GraphOptimizationLevel::Level3)
                .map_err(|e| e.to_string())?
                .with_intra_threads(threads)
                .map_err(|e| e.to_string())?
                .commit_from_file(dir.join(super::ONNX))
                .map_err(|e| format!("model.onnx: {e}"))?;
            let names: Vec<&str> = session.inputs().iter().map(|i| i.name()).collect();
            if let Some(other) = names.iter().find(|n| !["input_ids", "attention_mask", "token_type_ids"].contains(n)) {
                return Err(format!("model.onnx takes {other:?}: a cross-encoder's graph takes input_ids, attention_mask and token_type_ids").into());
            }
            if !names.contains(&"input_ids") || !names.contains(&"attention_mask") {
                return Err("model.onnx does not take input_ids and attention_mask".into());
            }
            let types = names.contains(&"token_type_ids");
            if session.outputs().is_empty() {
                return Err("model.onnx has no output".into());
            }
            let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|x| format!("tokenizer.json: {x}"))?;
            tokenizer.with_truncation(Some(tokenizers::TruncationParams { max_length: max_tokens, strategy: tokenizers::TruncationStrategy::LongestFirst, ..Default::default() })).map_err(|x| x.to_string())?;
            tokenizer.with_padding(None);
            Ok(Onnx { tokenizer, session: Mutex::new(session), types, pad })
        }

        /// The pairs of `query` and `texts`, tokenized, in batches of at
        /// most `size` of about the same length, so that little of a run
        /// is padding.
        pub fn batches(&self, query: &str, texts: &[&str], size: usize) -> Res<Vec<Batch>> {
            // An empty text is no second segment, as transformers'
            // tokenizers read it.
            let pairs: Vec<tokenizers::EncodeInput> = texts.iter().map(|t| if t.is_empty() { query.into() } else { (query, *t).into() }).collect();
            let mut enc: Vec<(usize, tokenizers::Encoding)> = self.tokenizer.encode_batch(pairs, true)?.into_iter().enumerate().collect();
            enc.sort_by_key(|(i, e)| (e.len(), *i));
            let mut out = Vec::new();
            let mut it = enc.into_iter().peekable();
            while it.peek().is_some() {
                let (rows, encodings) = it.by_ref().take(size.max(1)).unzip();
                out.push(Batch { rows, encodings });
            }
            Ok(out)
        }

        /// The logit of each pair of `b`, with its row.
        pub fn run(&self, b: &Batch) -> Res<Vec<(usize, f64)>> {
            let (n, len) = (b.encodings.len(), b.encodings.iter().map(|e| e.len()).max().unwrap_or(0));
            let (mut ids, mut mask, mut types) = (vec![self.pad as i64; n * len], vec![0i64; n * len], vec![0i64; n * len]);
            for (k, e) in b.encodings.iter().enumerate() {
                for (j, (&id, &t)) in e.get_ids().iter().zip(e.get_type_ids()).enumerate() {
                    ids[k * len + j] = id as i64;
                    mask[k * len + j] = 1;
                    types[k * len + j] = t as i64;
                }
            }
            let shape = vec![n as i64, len as i64];
            let tensor = |v: Vec<i64>| Tensor::from_array((shape.clone(), v)).map_err(|e| e.to_string());
            let (ids, mask) = (tensor(ids)?, tensor(mask)?);
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let outputs = match self.types {
                true => session.run(ort::inputs!["input_ids" => ids, "attention_mask" => mask, "token_type_ids" => tensor(types)?]),
                false => session.run(ort::inputs!["input_ids" => ids, "attention_mask" => mask]),
            }
            .map_err(|e| e.to_string())?;
            let (_, logits) = outputs[0].try_extract_tensor::<f32>().map_err(|e| format!("the model's output: {e}"))?;
            if logits.len() != n {
                return Err(format!("the model gives {} numbers for {n} pairs: a cross-encoder scores with one label", logits.len()).into());
            }
            Ok(b.rows.iter().zip(logits).map(|(&i, &x)| (i, x as f64)).collect())
        }
    }
}

#[cfg(feature = "bert")]
mod bert {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use candle_core::{CpuStorage, DType, Device, InplaceOp1, InplaceOp2, Layout, Module, Tensor};
    use candle_nn::{embedding, layer_norm, Embedding, LayerNorm, VarBuilder};
    use serde_json::Value as Json;

    use super::model_err;
    use crate::error::Result;

    type Res<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    /// A BERT or XLM-RoBERTa encoder and its classification head. A batch
    /// of pairs runs packed, one pair's tokens after another's and none
    /// padded: the layers that read a token at a time run over every
    /// token of the batch at once, and attention runs over each pair's
    /// own tokens, so no mask is added (candle broadcasts one slowly).
    pub struct Bert {
        tokenizer: tokenizers::Tokenizer,
        word: Embedding,
        position: Embedding,
        types: Embedding,
        norm: LayerNorm,
        layers: Vec<Layer>,
        heads: usize,
        act: Act,
        /// XLM-RoBERTa's padding id: its positions count from after it.
        roberta_pad: Option<u32>,
        /// Over the first token: BERT's pooler or XLM-RoBERTa's dense
        /// layer, then tanh and the classifier.
        pool: Dense,
        classifier: Dense,
    }

    struct Layer {
        /// The query, key and value projections as one, the query's
        /// scaled by 1/√(head size) as attention scales its products.
        qkv: Dense,
        out: Dense,
        out_norm: LayerNorm,
        up: Dense,
        down: Dense,
        down_norm: LayerNorm,
    }

    /// A dense layer, `x·wᵀ + b`. The bias goes in with the activation or
    /// the residual that follows, in one pass over every core (candle adds
    /// a bias on one).
    struct Dense {
        w: Tensor,
        b: Vec<f32>,
    }

    impl Dense {
        fn load(vb: VarBuilder, i: usize, o: usize, scale: f64) -> candle_core::Result<Dense> {
            Ok(Dense { w: (vb.get((o, i), "weight")? * scale)?, b: (vb.get(o, "bias")? * scale)?.to_vec1()? })
        }

        /// The query, key and value projections as one.
        fn concat(parts: [Dense; 3]) -> candle_core::Result<Dense> {
            let w = Tensor::cat(&parts.iter().map(|p| &p.w).collect::<Vec<_>>(), 0)?;
            Ok(Dense { w, b: parts.into_iter().flat_map(|p| p.b).collect() })
        }

        fn forward(&self, x: &Tensor, act: Option<Act>) -> candle_core::Result<Tensor> {
            let y = x.matmul(&self.w.t()?)?;
            y.inplace_op1(&Pass { bias: &self.b, act })?;
            Ok(y)
        }

        /// `x·wᵀ + b + residual`.
        fn forward_add(&self, x: &Tensor, residual: &Tensor) -> candle_core::Result<Tensor> {
            let y = x.matmul(&self.w.t()?)?;
            y.inplace_op2(residual, &Pass { bias: &self.b, act: None })?;
            Ok(y)
        }
    }

    #[derive(Clone, Copy)]
    enum Act {
        Gelu,
        GeluTanh,
        Relu,
        Tanh,
    }

    impl Act {
        fn apply(self, x: f32) -> f32 {
            match self {
                Act::Gelu => (candle_core::cpu::erf::erf_f32(x * std::f32::consts::FRAC_1_SQRT_2) + 1.0) * 0.5 * x,
                Act::GeluTanh => 0.5 * x * (1.0 + ((2.0 / std::f32::consts::PI).sqrt() * (x + 0.044715 * x * x * x)).tanh()),
                Act::Relu => x.max(0.0),
                Act::Tanh => x.tanh(),
            }
        }
    }

    impl Bert {
        pub fn open(name: &str, dir: &Path, max_tokens: usize) -> Result<Bert> {
            let e = |x: String| model_err(name, x);
            let text = std::fs::read_to_string(dir.join("config.json")).map_err(|x| e(x.to_string()))?;
            let c: Json = serde_json::from_str(&text).map_err(|x| e(format!("config.json: {x}")))?;
            let labels = c["id2label"].as_object().map_or(1, |m| m.len());
            if labels != 1 {
                return Err(e(format!("a cross-encoder scores with one label; this model has {labels}")));
            }
            let roberta = match c["architectures"][0].as_str().unwrap_or_default() {
                "BertForSequenceClassification" => false,
                "XLMRobertaForSequenceClassification" => true,
                other => return Err(e(format!("architecture {other:?}: a cross-encoder is a BertForSequenceClassification or an XLMRobertaForSequenceClassification"))),
            };
            let num = |k: &str| c[k].as_u64().map(|v| v as usize).ok_or_else(|| e(format!("config.json has no {k}")));
            let (hidden, heads, inter) = (num("hidden_size")?, num("num_attention_heads")?, num("intermediate_size")?);
            let max_positions = num("max_position_embeddings")?;
            let pad = c["pad_token_id"].as_u64().unwrap_or(if roberta { 1 } else { 0 }) as u32;
            let eps = c["layer_norm_eps"].as_f64().unwrap_or(1e-12);
            let act = match c["hidden_act"].as_str().unwrap_or("gelu") {
                "gelu" => Act::Gelu,
                "gelu_new" | "gelu_pytorch_tanh" => Act::GeluTanh,
                "relu" => Act::Relu,
                other => return Err(e(format!("activation {other:?}: gelu, gelu_new or relu"))),
            };
            if heads == 0 || hidden % heads != 0 {
                return Err(e(format!("{heads} attention heads do not divide the hidden size {hidden}")));
            }
            let dev = Device::Cpu;
            // SAFETY: the weights file is not modified while mapped.
            let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, &dev) }.map_err(|x| e(x.to_string()))?;
            let w = |x: candle_core::Error| e(x.to_string());
            let prefix = if roberta { "roberta" } else { "bert" };
            let enc = if vb.contains_tensor(&format!("{prefix}.embeddings.word_embeddings.weight")) { vb.pp(prefix) } else { vb.clone() };
            let emb = enc.pp("embeddings");
            let word = embedding(num("vocab_size")?, hidden, emb.pp("word_embeddings")).map_err(w)?;
            let position = embedding(max_positions, hidden, emb.pp("position_embeddings")).map_err(w)?;
            let types = embedding(num("type_vocab_size")?, hidden, emb.pp("token_type_embeddings")).map_err(w)?;
            let norm = layer_norm(hidden, eps, emb.pp("LayerNorm")).map_err(w)?;
            let scale = 1.0 / ((hidden / heads) as f64).sqrt();
            let layers = (0..num("num_hidden_layers")?)
                .map(|i| -> candle_core::Result<Layer> {
                    let l = enc.pp(format!("encoder.layer.{i}"));
                    let a = l.pp("attention");
                    let s = a.pp("self");
                    let qkv = [Dense::load(s.pp("query"), hidden, hidden, scale)?, Dense::load(s.pp("key"), hidden, hidden, 1.0)?, Dense::load(s.pp("value"), hidden, hidden, 1.0)?];
                    Ok(Layer {
                        qkv: Dense::concat(qkv)?,
                        out: Dense::load(a.pp("output.dense"), hidden, hidden, 1.0)?,
                        out_norm: layer_norm(hidden, eps, a.pp("output.LayerNorm"))?,
                        up: Dense::load(l.pp("intermediate.dense"), hidden, inter, 1.0)?,
                        down: Dense::load(l.pp("output.dense"), inter, hidden, 1.0)?,
                        down_norm: layer_norm(hidden, eps, l.pp("output.LayerNorm"))?,
                    })
                })
                .collect::<candle_core::Result<Vec<_>>>()
                .map_err(w)?;
            let (pool, classifier) = if roberta {
                (Dense::load(vb.pp("classifier.dense"), hidden, hidden, 1.0), Dense::load(vb.pp("classifier.out_proj"), hidden, 1, 1.0))
            } else {
                (Dense::load(enc.pp("pooler.dense"), hidden, hidden, 1.0), Dense::load(vb.pp("classifier"), hidden, 1, 1.0))
            };
            let (pool, classifier) = (pool.map_err(w)?, classifier.map_err(w)?);
            // XLM-RoBERTa's positions start after its padding id.
            let max_tokens = if roberta { max_positions.saturating_sub(pad as usize + 1) } else { max_positions }.min(max_tokens);
            let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|x| e(format!("tokenizer.json: {x}")))?;
            tokenizer
                .with_truncation(Some(tokenizers::TruncationParams { max_length: max_tokens, strategy: tokenizers::TruncationStrategy::LongestFirst, ..Default::default() }))
                .map_err(|x| e(x.to_string()))?;
            tokenizer.with_padding(None);
            let roberta_pad = roberta.then_some(pad);
            Ok(Bert { tokenizer, word, position, types, norm, layers, heads, act, roberta_pad, pool, classifier })
        }

        /// The logit of each `(query, text)` pair, one batch.
        pub fn score(&self, query: &str, texts: &[&str]) -> Res<Vec<f64>> {
            // An empty text is no second segment, as transformers' tokenizers
            // read it.
            let pairs: Vec<tokenizers::EncodeInput> = texts.iter().map(|t| if t.is_empty() { query.into() } else { (query, *t).into() }).collect();
            let enc = self.tokenizer.encode_batch(pairs, true)?;
            let (mut ids, mut types, mut positions, mut seqs) = (vec![], vec![], vec![], vec![]);
            for e in &enc {
                seqs.push((ids.len(), e.len()));
                ids.extend_from_slice(e.get_ids());
                types.extend_from_slice(e.get_type_ids());
                match self.roberta_pad {
                    // As transformers counts them: the tokens that are not
                    // padding, after the padding id.
                    Some(pad) => positions.extend(e.get_ids().iter().scan(pad, |n, &id| {
                        if id == pad {
                            return Some(pad);
                        }
                        *n += 1;
                        Some(*n)
                    })),
                    None => positions.extend(0..e.len() as u32),
                }
            }
            let dev = Device::Cpu;
            let n = ids.len();
            let x = (self.word.forward(&Tensor::from_vec(ids, n, &dev)?)? + self.types.forward(&Tensor::from_vec(types, n, &dev)?)?)?;
            let x = (x + self.position.forward(&Tensor::from_vec(positions, n, &dev)?)?)?;
            let mut x = self.norm.forward(&x)?;
            let first: Vec<u32> = seqs.iter().map(|&(start, _)| start as u32).collect();
            let first = Tensor::from_vec(first, seqs.len(), &dev)?;
            for (i, l) in self.layers.iter().enumerate() {
                // The head reads the first token alone, so the last layer
                // works out its output alone.
                let last = (i + 1 == self.layers.len()).then_some(&first);
                x = self.layer(l, &x, &seqs, last)?;
            }
            let logits = self.classifier.forward(&self.pool.forward(&x, Some(Act::Tanh))?, None)?;
            Ok(logits.squeeze(1)?.to_vec1::<f32>()?.into_iter().map(f64::from).collect())
        }

        /// One layer over the packed tokens; `seqs` is each pair's first
        /// token and length. With `first` (the rows of the pairs' first
        /// tokens), the output of those tokens alone.
        fn layer(&self, l: &Layer, x: &Tensor, seqs: &[(usize, usize)], first: Option<&Tensor>) -> candle_core::Result<Tensor> {
            let h = x.dim(1)?;
            let d = h / self.heads;
            let qkv = l.qkv.forward(x, None)?;
            let attend = |&(start, len): &(usize, usize)| -> candle_core::Result<Tensor> {
                let pair = qkv.narrow(0, start, len)?;
                let part = |i: usize, rows: usize| pair.narrow(0, 0, rows)?.narrow(1, i * h, h)?.reshape((rows, self.heads, d))?.transpose(0, 1)?.contiguous();
                let rows = if first.is_some() { 1 } else { len };
                let (q, k, v) = (part(0, rows)?, part(1, len)?, part(2, len)?);
                let att = candle_nn::ops::softmax_last_dim(&q.matmul(&k.t()?)?)?;
                att.matmul(&v)?.transpose(0, 1)?.reshape((rows, h))
            };
            // Pairs attend on the query pool's threads: candle's products of
            // one pair's size barely use a second core.
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(seqs.len());
            let per = seqs.len().div_ceil(threads.max(1));
            let parts: Vec<&[(usize, usize)]> = seqs.chunks(per).collect();
            let outs: Vec<Mutex<Option<candle_core::Result<Vec<Tensor>>>>> = parts.iter().map(|_| Mutex::new(None)).collect();
            on_pool(parts.len(), &|i| {
                let r = parts[i].iter().map(attend).collect::<candle_core::Result<Vec<_>>>();
                *outs[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
            });
            let ctx = outs.into_iter().map(|o| o.into_inner().unwrap_or_else(|e| e.into_inner()).expect("every part ran")).collect::<candle_core::Result<Vec<_>>>()?;
            let x = match first {
                Some(rows) => x.index_select(rows, 0)?,
                None => x.clone(),
            };
            let x = l.out_norm.forward(&l.out.forward_add(&Tensor::cat(&ctx.concat(), 0)?, &x)?)?;
            let up = l.up.forward(&x, Some(self.act))?;
            l.down_norm.forward(&l.down.forward_add(&up, &x)?)
        }
    }

    /// Runs `task(i)` for each `i` below `n` on this thread and on the
    /// process's query pool ([`crate::query::pool::Pool::global`]), so
    /// that reranks running at once share its threads, one per core,
    /// rather than each starting a thread per core for every layer.
    fn on_pool(n: usize, task: &(dyn Fn(usize) + Sync)) {
        let next = AtomicUsize::new(0);
        let pool = crate::query::pool::Pool::global();
        pool.run(n.saturating_sub(1).min(pool.threads()), &|| loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            if i >= n {
                break;
            }
            task(i);
        });
    }

    /// A dense layer's bias, then its activation or the residual, in
    /// place over the layer's output.
    struct Pass<'a> {
        bias: &'a [f32],
        act: Option<Act>,
    }

    fn values<'a>(s: &'a CpuStorage, l: &Layout) -> candle_core::Result<&'a [f32]> {
        match (s, l.contiguous_offsets()) {
            (CpuStorage::F32(v), Some((a, b))) => Ok(&v[a..b]),
            _ => candle_core::bail!("a dense layer's output is a contiguous f32 tensor"),
        }
    }

    impl Pass<'_> {
        /// Over the rows of `out` in parallel, with the matching rows of
        /// `residual` if any.
        fn run(&self, out: &mut CpuStorage, l: &Layout, residual: Option<&[f32]>) -> candle_core::Result<()> {
            let (CpuStorage::F32(v), Some((a, b))) = (out, l.contiguous_offsets()) else {
                candle_core::bail!("a dense layer's output is a contiguous f32 tensor");
            };
            let v = &mut v[a..b];
            let width = self.bias.len();
            if residual.is_some_and(|r| r.len() != v.len()) || v.len() % width != 0 {
                candle_core::bail!("a dense layer's bias or residual does not fit its output");
            }
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
            let rows = (v.len() / width).div_ceil(threads).max((1 << 14) / width + 1);
            let run = |at: usize, part: &mut [f32]| {
                for (i, row) in part.chunks_mut(width).enumerate() {
                    for (j, (x, b)) in row.iter_mut().zip(self.bias).enumerate() {
                        let y = *x + b;
                        *x = match (self.act, residual) {
                            (Some(act), _) => act.apply(y),
                            (None, Some(r)) => y + r[(at + i) * width + j],
                            (None, None) => y,
                        };
                    }
                }
            };
            let parts: Vec<Mutex<&mut [f32]>> = v.chunks_mut(rows * width).map(Mutex::new).collect();
            on_pool(parts.len(), &|k| {
                let mut part = parts[k].lock().unwrap_or_else(|e| e.into_inner());
                run(k * rows, &mut **part);
            });
            Ok(())
        }
    }

    impl InplaceOp1 for Pass<'_> {
        fn name(&self) -> &'static str {
            "dense bias"
        }

        fn cpu_fwd(&self, s: &mut CpuStorage, l: &Layout) -> candle_core::Result<()> {
            self.run(s, l, None)
        }
    }

    impl InplaceOp2 for Pass<'_> {
        fn name(&self) -> &'static str {
            "dense bias and residual"
        }

        fn cpu_fwd(&self, s1: &mut CpuStorage, l1: &Layout, s2: &CpuStorage, l2: &Layout) -> candle_core::Result<()> {
            self.run(s1, l1, Some(values(s2, l2)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_the_share_of_query_words() {
        assert_eq!(overlap("Red apple, red pie", "an APPLE pie"), 2.0 / 3.0);
        assert_eq!(overlap("", "anything"), 0.0);
        assert_eq!(overlap("x", ""), 0.0);
        let m = CrossEncoder::open(OVERLAP, Path::new("/nonexistent")).unwrap();
        let texts = ["b", "a b", "", "a"];
        assert_eq!(m.score("a b", &texts, &mut || Ok(())).unwrap(), [0.5, 1.0, 0.0, 0.5]);
        let mut calls = 0;
        let many = vec!["a"; BATCH + 1];
        let stop = m.score("a", &many, &mut || {
            calls += 1;
            if calls > 1 { Err(Error::Query("deadline".into())) } else { Ok(()) }
        });
        assert!(stop.is_err(), "the check runs between batches");
    }

    #[test]
    fn a_missing_model_is_refused() {
        let e = CrossEncoder::open("nope", Path::new("/nonexistent/nope")).err().unwrap().to_string();
        assert!(e.contains("no cross-encoder named nope"), "{e}");
    }
}
