//! Embedders: the models that turn a row's text or image, or a query,
//! into the vector of a VECTOR field declared with
//! [`crate::schema::Embed`]. Every vector is unit length, so Strata's dot
//! product is cosine similarity. Every model embeds text
//! ([`Embedder::embed`]); a model of images and text, such as SigLIP,
//! also embeds decoded images ([`Embedder::embed_inputs`]) into the same
//! space, so a text query finds images and an image finds images.
//!
//! * [`StaticEmbedder`] (feature `embed`, on by default): a Model2Vec model
//!   (token lookup and mean), such as `potion-retrieval-32M`. Tens of
//!   microseconds a row on one core.
//! * [`BertEmbedder`] (feature `bert`): a BERT encoder with CLS pooling
//!   through candle, such as `bge-small-en-v1.5` or
//!   `snowflake-arctic-embed-m-v1.5`. Better vectors, milliseconds a row.
//! * [`SiglipEmbedder`] (feature `vision`): SigLIP's image and text
//!   towers through candle, `siglip-base-patch16-224`. A tenth of a second
//!   an image on a few cores.
//! * [`HashEmbedder`]: feature hashing of words (`hash:<dim>`), and of an
//!   image's colours, for tests and demos without model files; it matches
//!   words and colours, not meanings.
//!
//! [`open`] finds a model by the name a schema gives it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};

pub trait Embedder: Send + Sync {
    fn name(&self) -> String;
    fn dim(&self) -> usize;
    /// Embeddings of documents.
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>>;
    /// Embedding of a search query (some models prefix an instruction).
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.embed(&[text]).pop().unwrap()
    }
    /// What it embeds: text, and images for a model of both.
    fn modalities(&self) -> &'static [Modality] {
        &[Modality::Text]
    }
    /// Embeddings of inputs of the kinds it takes ([`Embedder::modalities`]),
    /// in the space of its text vectors: text as documents
    /// ([`Embedder::embed`]), images with the model's image tower.
    fn embed_inputs(&self, inputs: &[Input<'_>]) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<&str> = inputs.iter().filter_map(|i| if let Input::Text(t) = i { Some(*t) } else { None }).collect();
        match inputs.iter().find(|i| !matches!(i, Input::Text(_))) {
            Some(i) => Err(model_err(format!("model {} does not embed {}s", self.name(), i.modality().as_str()))),
            None => Ok(self.embed(&texts)),
        }
    }
}

/// A kind of input a model embeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modality {
    Text,
    Image,
}

impl Modality {
    pub fn as_str(self) -> &'static str {
        match self {
            Modality::Text => "text",
            Modality::Image => "image",
        }
    }
}

/// An image decoded to 8-bit RGB, row by row
/// ([`crate::media::decode_image`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// What [`Embedder::embed_inputs`] embeds.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    Text(&'a str),
    Image(&'a Image),
}

impl Input<'_> {
    pub fn modality(&self) -> Modality {
        match self {
            Input::Text(_) => Modality::Text,
            Input::Image(_) => Modality::Image,
        }
    }
}

/// The vector of an image file (JPEG, PNG, WebP, GIF, BMP) by `e`, a
/// model of images: a query by image, as `{"embed": {"image": ...}}`
/// gives it.
pub fn embed_image_file(e: &dyn Embedder, data: &[u8]) -> Result<Vec<f32>> {
    let image = crate::media::decode_image(data)?;
    Ok(e.embed_inputs(&[Input::Image(&image)])?.pop().unwrap_or_default())
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

fn model_err(what: impl std::fmt::Display) -> Error {
    Error::Schema(what.to_string())
}

/// Where [`open`] looks for models by default: `$STRATA_MODELS`, else
/// `~/.local/share/strata/models`.
pub fn models_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("STRATA_MODELS") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/share/strata/models")
}

/// Models [`open`] knows how to fetch: name, Hugging Face repository, and
/// what it is.
pub const KNOWN_MODELS: &[(&str, &str, &str)] = &[
    ("potion-retrieval-32M", "minishlab/potion-retrieval-32M", "Model2Vec, 512 dimensions, fast (the model engram uses)"),
    ("potion-base-8M", "minishlab/potion-base-8M", "Model2Vec, 256 dimensions, smallest"),
    ("bge-small-en-v1.5", "BAAI/bge-small-en-v1.5", "BERT, 384 dimensions (feature bert)"),
    ("snowflake-arctic-embed-m-v1.5", "Snowflake/snowflake-arctic-embed-m-v1.5", "BERT, 768 dimensions (feature bert)"),
    ("siglip-base-patch16-224", "google/siglip-base-patch16-224", "SigLIP, images and text, 768 dimensions (feature vision)"),
];

/// The dimensions of a model [`open`] knows by name (`hash:N` included),
/// without opening it.
pub fn known_dim(model: &str) -> Option<usize> {
    if let Some(n) = model.strip_prefix("hash:") {
        return n.parse().ok();
    }
    let dims = [("potion-retrieval-32M", 512), ("potion-base-8M", 256), ("bge-small-en-v1.5", 384), ("snowflake-arctic-embed-m-v1.5", 768), ("siglip-base-patch16-224", 768), ("cohere/embed-english-v3.0", 1024)];
    dims.iter().find(|(m, _)| m.eq_ignore_ascii_case(model)).map(|&(_, d)| d)
}

/// The files a model directory needs.
pub const MODEL_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];

/// Opens the model a schema names: `hash:<dim>`, or a directory under
/// `models` (or an absolute path) holding a Model2Vec, BERT or SigLIP
/// model ([`MODEL_FILES`]).
pub fn open(model: &str, models: &Path) -> Result<Arc<dyn Embedder>> {
    if let Some(dim) = model.strip_prefix("hash:") {
        let dim = dim.parse().map_err(|_| model_err(format!("model {model}: hash:<dimensions>")))?;
        return Ok(Arc::new(HashEmbedder { dim }));
    }
    let dir = if Path::new(model).is_absolute() { PathBuf::from(model) } else { models.join(model) };
    if !dir.join("model.safetensors").exists() {
        return Err(model_err(format!("model {model}: no {} (fetch it first)", dir.join("model.safetensors").display())));
    }
    let config: serde_json::Value = std::fs::read_to_string(dir.join("config.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if config["model_type"] == "bert" {
        #[cfg(feature = "bert")]
        return Ok(Arc::new(BertEmbedder::open(&dir)?));
        #[cfg(not(feature = "bert"))]
        return Err(model_err(format!("model {model} is a BERT model: build with the `bert` feature")));
    }
    if config["model_type"] == "siglip" {
        #[cfg(feature = "vision")]
        return Ok(Arc::new(SiglipEmbedder::open(&dir)?));
        #[cfg(not(feature = "vision"))]
        return Err(model_err(format!("model {model} is a SigLIP model: build with the `vision` feature")));
    }
    #[cfg(feature = "embed")]
    return Ok(Arc::new(StaticEmbedder::open(&dir)?));
    #[cfg(not(feature = "embed"))]
    Err(model_err(format!("model {model}: built without the `embed` feature")))
}

// ---------------------------------------------------------------- static

/// Model2Vec: the embedding of a text is the mean of its tokens' rows.
#[cfg(feature = "embed")]
pub struct StaticEmbedder {
    tokenizer: tokenizers::Tokenizer,
    table: Table,
    dim: usize,
    unk: Option<u32>,
    name: String,
    max_tokens: usize,
}

#[cfg(feature = "embed")]
impl StaticEmbedder {
    pub fn open(dir: &Path) -> Result<StaticEmbedder> {
        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| model_err(format!("{}: {e}", dir.join("tokenizer.json").display())))?;
        let table = Table::open(&dir.join("model.safetensors"), "embeddings")?;
        let (rows, dim) = (table.rows, table.dim);
        if rows < tokenizer.get_vocab_size(true) {
            return Err(model_err(format!("{}: embedding table has {rows} rows, tokenizer {} tokens", dir.display(), tokenizer.get_vocab_size(true))));
        }
        let unk = tokenizer.token_to_id("[UNK]");
        let name = format!("static:{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(StaticEmbedder { tokenizer, table, dim, unk, name, max_tokens: 512 })
    }

    fn embed_ids(&self, ids: &[u32]) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let mut n = 0usize;
        for &id in ids.iter().filter(|&&id| Some(id) != self.unk).take(self.max_tokens) {
            if self.table.add_row(id as usize, &mut v) {
                n += 1;
            }
        }
        if n > 0 {
            v.iter_mut().for_each(|x| *x /= n as f32);
            normalize(&mut v);
        }
        v
    }
}

#[cfg(feature = "embed")]
impl Embedder for StaticEmbedder {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| match self.tokenizer.encode(*t, false) {
                Ok(e) => self.embed_ids(e.get_ids()),
                Err(_) => vec![0f32; self.dim],
            })
            .collect()
    }
}

/// One F32 tensor of a safetensors file, mapped rather than read: a process
/// starts without loading the table, and only the rows of tokens it meets
/// are paged in.
#[cfg(feature = "embed")]
struct Table {
    map: memmap2::Mmap,
    /// Byte offset of row 0 in the file.
    start: usize,
    rows: usize,
    dim: usize,
}

#[cfg(feature = "embed")]
impl Table {
    fn open(path: &Path, tensor: &str) -> Result<Table> {
        let file = std::fs::File::open(path).map_err(|e| model_err(format!("{}: {e}", path.display())))?;
        // SAFETY: the model file is not modified while mapped (fetching a
        // model writes a new file).
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| model_err(format!("{}: {e}", path.display())))?;
        let bad = |m: String| model_err(format!("{}: {m}", path.display()));
        if map.len() < 8 {
            return Err(bad("not a safetensors file".into()));
        }
        let hlen = u64::from_le_bytes(map[..8].try_into().unwrap()) as usize;
        let header: serde_json::Value = serde_json::from_slice(map.get(8..8 + hlen).ok_or_else(|| bad("truncated header".into()))?).map_err(|e| bad(e.to_string()))?;
        let t = header.get(tensor).ok_or_else(|| bad(format!("no tensor {tensor}")))?;
        if t["dtype"] != "F32" {
            return Err(bad(format!("{tensor}: dtype {} (need F32)", t["dtype"])));
        }
        let shape: Vec<usize> = t["shape"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()).map(|x| x as usize).collect();
        let offs: Vec<usize> = t["data_offsets"].as_array().into_iter().flatten().filter_map(|x| x.as_u64()).map(|x| x as usize).collect();
        let [rows, dim] = shape[..] else { return Err(bad(format!("{tensor}: shape {shape:?}"))) };
        let (a, b) = (8 + hlen + offs.first().copied().unwrap_or(0), 8 + hlen + offs.get(1).copied().unwrap_or(0));
        if b > map.len() || b - a != rows * dim * 4 {
            return Err(bad(format!("{tensor}: {} bytes for shape {rows}x{dim}", b.saturating_sub(a))));
        }
        Ok(Table { map, start: a, rows, dim })
    }

    /// Adds row `id` to `acc`; false if there is no such row.
    fn add_row(&self, id: usize, acc: &mut [f32]) -> bool {
        if id >= self.rows {
            return false;
        }
        let off = self.start + id * self.dim * 4;
        for (a, c) in acc.iter_mut().zip(self.map[off..off + self.dim * 4].chunks_exact(4)) {
            *a += f32::from_le_bytes(c.try_into().unwrap());
        }
        true
    }
}

// ---------------------------------------------------------------- hash

/// Lowercased words hashed into `dim` buckets (signed), for tests. An
/// image is the words of its colours: each cell of a 4 by 4 grid is a word
/// of its mean colour, a few levels a channel, so equal images have equal
/// vectors and images of like colours are near.
pub struct HashEmbedder {
    pub dim: usize,
}

impl HashEmbedder {
    fn of_words<'a>(&self, words: impl Iterator<Item = &'a str>) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for w in words {
            let h = fnv(&w.to_lowercase());
            v[(h % self.dim as u64) as usize] += if h >> 63 == 0 { 1.0 } else { -1.0 };
        }
        normalize(&mut v);
        v
    }

    /// The words of an image's colours.
    fn colour_words(image: &Image) -> Vec<String> {
        let (w, h) = (image.width.max(1) as usize, image.height.max(1) as usize);
        let mut sums = [[0u64; 4]; 16];
        for y in 0..image.height as usize {
            for x in 0..image.width as usize {
                let (cell, at) = ((y * 4 / h) * 4 + x * 4 / w, (y * w + x) * 3);
                let Some(p) = image.rgb.get(at..at + 3) else { continue };
                for c in 0..3 {
                    sums[cell][c] += p[c] as u64;
                }
                sums[cell][3] += 1;
            }
        }
        sums.iter().enumerate().filter(|(_, s)| s[3] > 0).map(|(i, s)| format!("c{i}r{}g{}b{}", s[0] / s[3] / 64, s[1] / s[3] / 64, s[2] / s[3] / 64)).collect()
    }
}

impl Embedder for HashEmbedder {
    fn name(&self) -> String {
        format!("hash:{}", self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.of_words(t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()))).collect()
    }
    fn modalities(&self) -> &'static [Modality] {
        &[Modality::Text, Modality::Image]
    }
    fn embed_inputs(&self, inputs: &[Input<'_>]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs
            .iter()
            .map(|i| match i {
                Input::Text(t) => self.embed(&[t]).pop().unwrap(),
                Input::Image(image) => self.of_words(Self::colour_words(image).iter().map(String::as_str)),
            })
            .collect())
    }
}

/// 64-bit FNV-1a.
pub fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// ---------------------------------------------------------------- bert

#[cfg(feature = "bert")]
pub struct BertEmbedder {
    model: candle_transformers::models::bert::BertModel,
    tokenizer: tokenizers::Tokenizer,
    dim: usize,
    name: String,
}

#[cfg(feature = "bert")]
impl BertEmbedder {
    /// The query instruction of BGE v1.5 and Arctic Embed v1.5.
    const QUERY_PREFIX: &'static str = "Represent this sentence for searching relevant passages: ";

    pub fn open(dir: &Path) -> Result<BertEmbedder> {
        use candle_nn::VarBuilder;
        use candle_transformers::models::bert::{BertModel, Config};
        let e = |x: String| model_err(format!("{}: {x}", dir.display()));
        let text = std::fs::read_to_string(dir.join("config.json")).map_err(|x| e(x.to_string()))?;
        let cfg: Config = serde_json::from_str(&text).map_err(|x| e(x.to_string()))?;
        let dim = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["hidden_size"].as_u64()).unwrap_or(384) as usize;
        let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|x| e(x.to_string()))?;
        tokenizer.with_truncation(Some(tokenizers::TruncationParams { max_length: 512, ..Default::default() })).map_err(|x| e(x.to_string()))?;
        tokenizer.with_padding(Some(tokenizers::PaddingParams::default()));
        let dev = candle_core::Device::Cpu;
        // SAFETY: the weights file is not modified while mapped.
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], candle_core::DType::F32, &dev) }.map_err(|x| e(x.to_string()))?;
        let model = BertModel::load(vb, &cfg).map_err(|x| e(x.to_string()))?;
        let name = format!("bert:{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(BertEmbedder { model, tokenizer, dim, name })
    }

    fn run(&self, texts: &[String]) -> Vec<Vec<f32>> {
        use candle_core::Tensor;
        let attempt = || -> std::result::Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
            let enc = self.tokenizer.encode_batch(texts.to_vec(), true).map_err(|e| e.to_string())?;
            let ids: Vec<Vec<u32>> = enc.iter().map(|e| e.get_ids().to_vec()).collect();
            let mask: Vec<Vec<u32>> = enc.iter().map(|e| e.get_attention_mask().to_vec()).collect();
            let dev = candle_core::Device::Cpu;
            let ids = Tensor::new(ids, &dev)?;
            let mask = Tensor::new(mask, &dev)?;
            let types = ids.zeros_like()?;
            let out = self.model.forward(&ids, &types, Some(&mask))?;
            let cls = out.narrow(1, 0, 1)?.squeeze(1)?;
            let mut v = cls.to_vec2::<f32>()?;
            v.iter_mut().for_each(|x| normalize(x));
            Ok(v)
        };
        attempt().unwrap_or_else(|_| texts.iter().map(|_| vec![0f32; self.dim]).collect())
    }
}

#[cfg(feature = "bert")]
impl Embedder for BertEmbedder {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        // Batches of similar length waste less on padding.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let mut out = vec![Vec::new(); texts.len()];
        for chunk in order.chunks(16) {
            let batch: Vec<String> = chunk.iter().map(|&i| texts[i].to_string()).collect();
            for (&i, v) in chunk.iter().zip(self.run(&batch)) {
                out[i] = v;
            }
        }
        out
    }
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.run(&[format!("{}{text}", Self::QUERY_PREFIX)]).pop().unwrap()
    }
}

// ---------------------------------------------------------------- siglip

/// SigLIP: a vision transformer and a text transformer trained so that an
/// image and its caption meet (`google/siglip-base-patch16-224`). Images
/// are resized to the model's square (bicubic, without keeping the aspect
/// ratio, as the model was trained) and scaled to [-1, 1]; texts are
/// lowercased without punctuation by the tokenizer, and padded to the 64
/// tokens the text tower pools the last of. Text is embedded alike for
/// documents and queries.
#[cfg(feature = "vision")]
pub struct SiglipEmbedder {
    model: candle_transformers::models::siglip::Model,
    tokenizer: tokenizers::Tokenizer,
    /// The side of the square images are resized to.
    side: usize,
    dim: usize,
    name: String,
}

#[cfg(feature = "vision")]
impl SiglipEmbedder {
    /// Images a forward pass takes at once.
    const BATCH: usize = 8;

    pub fn open(dir: &Path) -> Result<SiglipEmbedder> {
        use candle_nn::VarBuilder;
        use candle_transformers::models::siglip::{Config, Model};
        let e = |x: String| model_err(format!("{}: {x}", dir.display()));
        let text = std::fs::read_to_string(dir.join("config.json")).map_err(|x| e(x.to_string()))?;
        let cfg: Config = serde_json::from_str(&text).map_err(|x| e(x.to_string()))?;
        let len = cfg.text_config.max_position_embeddings;
        let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|x| e(x.to_string()))?;
        tokenizer.with_truncation(Some(tokenizers::TruncationParams { max_length: len, ..Default::default() })).map_err(|x| e(x.to_string()))?;
        let pad = cfg.text_config.pad_token_id;
        tokenizer.with_padding(Some(tokenizers::PaddingParams { strategy: tokenizers::PaddingStrategy::Fixed(len), pad_id: pad, pad_token: "</s>".into(), ..Default::default() }));
        let dev = candle_core::Device::Cpu;
        // SAFETY: the weights file is not modified while mapped.
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], candle_core::DType::F32, &dev) }.map_err(|x| e(x.to_string()))?;
        let model = Model::new(&cfg, vb).map_err(|x| e(x.to_string()))?;
        let name = format!("siglip:{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(SiglipEmbedder { model, tokenizer, side: cfg.vision_config.image_size, dim: cfg.text_config.hidden_size, name })
    }

    fn texts(&self, texts: &[&str]) -> std::result::Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
        let enc = self.tokenizer.encode_batch(texts.to_vec(), true)?;
        let ids: Vec<Vec<u32>> = enc.iter().map(|e| e.get_ids().to_vec()).collect();
        let ids = candle_core::Tensor::new(ids, &candle_core::Device::Cpu)?;
        let mut v = self.model.get_text_features(&ids)?.to_vec2::<f32>()?;
        v.iter_mut().for_each(|x| normalize(x));
        Ok(v)
    }

    /// The pixels the image tower takes: `side` by `side`, channels first,
    /// in [-1, 1].
    fn pixels(&self, image: &Image) -> Result<Vec<f32>> {
        let rgb = image::RgbImage::from_raw(image.width, image.height, image.rgb.clone()).ok_or_else(|| model_err("an image's pixels do not fill its size"))?;
        let side = self.side as u32;
        let small = image::imageops::resize(&rgb, side, side, image::imageops::FilterType::CatmullRom);
        let n = self.side * self.side;
        let mut out = vec![0f32; 3 * n];
        for (i, p) in small.pixels().enumerate() {
            for c in 0..3 {
                out[c * n + i] = p[c] as f32 / 127.5 - 1.0;
            }
        }
        Ok(out)
    }

    fn images(&self, images: &[&Image]) -> Result<Vec<Vec<f32>>> {
        let e = |x: candle_core::Error| model_err(format!("{}: {x}", self.name));
        let mut out = Vec::with_capacity(images.len());
        for batch in images.chunks(Self::BATCH) {
            let pixels = batch.iter().map(|i| self.pixels(i)).collect::<Result<Vec<_>>>()?.concat();
            let t = candle_core::Tensor::from_vec(pixels, (batch.len(), 3, self.side, self.side), &candle_core::Device::Cpu).map_err(e)?;
            let mut v = self.model.get_image_features(&t).and_then(|f| f.to_vec2::<f32>()).map_err(e)?;
            v.iter_mut().for_each(|x| normalize(x));
            out.extend(v);
        }
        Ok(out)
    }
}

#[cfg(feature = "vision")]
impl Embedder for SiglipEmbedder {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts.chunks(32).flat_map(|c| self.texts(c).unwrap_or_else(|_| c.iter().map(|_| vec![0f32; self.dim]).collect())).collect()
    }
    fn modalities(&self) -> &'static [Modality] {
        &[Modality::Text, Modality::Image]
    }
    fn embed_inputs(&self, inputs: &[Input<'_>]) -> Result<Vec<Vec<f32>>> {
        let texts: Vec<&str> = inputs.iter().filter_map(|i| if let Input::Text(t) = i { Some(*t) } else { None }).collect();
        let images: Vec<&Image> = inputs.iter().filter_map(|i| if let Input::Image(m) = i { Some(*m) } else { None }).collect();
        let (mut texts, mut images) = (self.embed(&texts).into_iter(), self.images(&images)?.into_iter());
        Ok(inputs.iter().map(|i| if let Input::Text(_) = i { texts.next() } else { images.next() }.unwrap_or_default()).collect())
    }
}
