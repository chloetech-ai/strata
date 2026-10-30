//! Text made from media: the models behind a TEXT field declared with
//! [`crate::schema::Derive`], such as a caption of each row's image. The
//! text is made before the writer, as embedded vectors are, and logged
//! with the row, so replicas and merges never run a model; BM25,
//! synonyms and text vectors then work on it as on any text.
//!
//! * [`BlipCaptioner`] (feature `vision`): BLIP's captioner through candle,
//!   `blip-image-captioning-base`. Seconds an image on a few cores.
//! * [`ColorNamer`] (`colors`): names an image's main colours, for tests
//!   and demos without model files.
//!
//! [`open`] finds a model by the name a schema gives it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::embed::{Image, Input, Modality};
use crate::error::{Error, Result};

/// A model that makes text of media.
pub trait Deriver: Send + Sync {
    fn name(&self) -> String;
    /// What it makes text of.
    fn modality(&self) -> Modality;
    /// The text of each input, all of [`Deriver::modality`].
    fn derive(&self, inputs: &[Input<'_>]) -> Result<Vec<String>>;
}

fn model_err(what: impl std::fmt::Display) -> Error {
    Error::Schema(what.to_string())
}

/// Models [`open`] knows how to fetch: name, Hugging Face repository, and
/// what it is.
pub const KNOWN_MODELS: &[(&str, &str, &str)] = &[("blip-image-captioning-base", "Salesforce/blip-image-captioning-base", "BLIP, captions of images (feature vision)")];

/// The files a model directory of [`KNOWN_MODELS`] needs: BLIP's
/// repository ships its weights as a PyTorch file only.
pub const MODEL_FILES: &[&str] = &["config.json", "tokenizer.json", "pytorch_model.bin"];

/// Opens the model a derived field names: `colors`, or a directory under
/// `models` (or an absolute path) holding a BLIP captioner.
pub fn open(model: &str, models: &Path) -> Result<Arc<dyn Deriver>> {
    if model == "colors" {
        return Ok(Arc::new(ColorNamer));
    }
    let dir = if Path::new(model).is_absolute() { PathBuf::from(model) } else { models.join(model) };
    let config: serde_json::Value = std::fs::read_to_string(dir.join("config.json"))
        .map_err(|e| model_err(format!("model {model}: {}: {e} (fetch it first)", dir.join("config.json").display())))
        .and_then(|t| serde_json::from_str(&t).map_err(|e| model_err(format!("model {model}: config.json: {e}"))))?;
    if config["model_type"] == "blip" {
        #[cfg(feature = "vision")]
        return Ok(Arc::new(BlipCaptioner::open(&dir)?));
        #[cfg(not(feature = "vision"))]
        return Err(model_err(format!("model {model} is a BLIP captioner: build with the `vision` feature")));
    }
    Err(model_err(format!("model {model} does not make text of media (a BLIP captioner does)")))
}

// ---------------------------------------------------------------- colors

/// Names the colours of an image's four quarters, most covered first
/// ("red blue"): words BM25 can match, for tests without model files.
pub struct ColorNamer;

impl ColorNamer {
    const NAMES: [(&'static str, [u8; 3]); 8] =
        [("black", [0, 0, 0]), ("white", [255, 255, 255]), ("red", [220, 30, 30]), ("green", [30, 170, 30]), ("blue", [30, 30, 220]), ("yellow", [240, 220, 30]), ("gray", [128, 128, 128]), ("orange", [250, 140, 20])];

    fn name_of(image: &Image) -> String {
        let (w, h) = (image.width.max(1) as usize, image.height.max(1) as usize);
        let mut sums = [[0u64; 4]; 4];
        for y in 0..h {
            for x in 0..w {
                let q = (2 * y / h) * 2 + 2 * x / w;
                let p = &image.rgb[(y * w + x) * 3..][..3];
                (0..3).for_each(|c| sums[q][c] += p[c] as u64);
                sums[q][3] += 1;
            }
        }
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for s in sums.iter().filter(|s| s[3] > 0) {
            let mean = [0, 1, 2].map(|c| (s[c] / s[3]) as i64);
            let near = Self::NAMES.iter().min_by_key(|(_, c)| (0..3).map(|i| (mean[i] - c[i] as i64).pow(2)).sum::<i64>()).expect("names").0;
            match counts.iter_mut().find(|(n, _)| *n == near) {
                Some(c) => c.1 += 1,
                None => counts.push((near, 1)),
            }
        }
        counts.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        counts.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" ")
    }
}

impl Deriver for ColorNamer {
    fn name(&self) -> String {
        "colors".into()
    }
    fn modality(&self) -> Modality {
        Modality::Image
    }
    fn derive(&self, inputs: &[Input<'_>]) -> Result<Vec<String>> {
        inputs
            .iter()
            .map(|i| match i {
                Input::Image(m) => Ok(Self::name_of(m)),
                i => Err(model_err(format!("model colors makes text of images, not {}s", i.modality().as_str()))),
            })
            .collect()
    }
}

// ---------------------------------------------------------------- blip

/// BLIP's captioner (`Salesforce/blip-image-captioning-base`): a vision
/// transformer reads the image, resized to the model's square (bicubic,
/// without keeping the aspect ratio) and normalised as CLIP's; a text
/// decoder writes the caption greedily from `[DEC]` until `[SEP]`, at
/// most as many tokens as the model's `max_length` (20), as the
/// `transformers` library generates by default. The decoder is Strata's
/// own, over the same weights as candle's: it runs a batch of images in
/// step and projects each image's keys and values once rather than at
/// every token. The weights are read from `model.safetensors`, or else
/// from `pytorch_model.bin` (the only file the repository ships), without
/// running any of its code.
#[cfg(feature = "vision")]
pub struct BlipCaptioner {
    vision: candle_transformers::models::blip::BlipForConditionalGeneration,
    decoder: blip::Decoder,
    tokenizer: tokenizers::Tokenizer,
    side: usize,
    bos: u32,
    eos: u32,
    max_tokens: usize,
    name: String,
}

#[cfg(feature = "vision")]
impl BlipCaptioner {
    /// Images captioned at once.
    const BATCH: usize = 8;
    const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
    const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

    pub fn open(dir: &Path) -> Result<BlipCaptioner> {
        use candle_nn::VarBuilder;
        use candle_transformers::models::blip::{BlipForConditionalGeneration, Config};
        let e = |x: String| model_err(format!("{}: {x}", dir.display()));
        let text = std::fs::read_to_string(dir.join("config.json")).map_err(|x| e(x.to_string()))?;
        let mut json: serde_json::Value = serde_json::from_str(&text).map_err(|x| e(x.to_string()))?;
        // The decoder attends to the vision tower's states, of its width.
        if json["text_config"].get("encoder_hidden_size").is_none() {
            json["text_config"]["encoder_hidden_size"] = json["vision_config"]["hidden_size"].clone();
        }
        let cfg: Config = serde_json::from_value(json.clone()).map_err(|x| e(x.to_string()))?;
        let token = |k: &str, d: u64| json["text_config"][k].as_u64().unwrap_or(d) as u32;
        let (bos, eos) = (token("bos_token_id", 30522), token("sep_token_id", 102));
        let max_tokens = json["text_config"]["max_length"].as_u64().unwrap_or(20) as usize;
        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|x| e(x.to_string()))?;
        let dev = candle_core::Device::Cpu;
        // Read once: candle's model and the decoder share the tensors.
        let safetensors = dir.join("model.safetensors");
        let tensors: std::collections::HashMap<String, candle_core::Tensor> = if safetensors.exists() {
            candle_core::safetensors::load(&safetensors, &dev)
        } else {
            candle_core::pickle::read_all(dir.join("pytorch_model.bin")).map(|t| t.into_iter().collect())
        }
        .map_err(|x| e(x.to_string()))?;
        let vb = VarBuilder::from_tensors(tensors, candle_core::DType::F32, &dev);
        let vision = BlipForConditionalGeneration::new(&cfg, vb.clone()).map_err(|x| e(x.to_string()))?;
        let decoder = blip::Decoder::new(&cfg.text_config, vb.pp("text_decoder")).map_err(|x| e(x.to_string()))?;
        let name = format!("blip:{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(BlipCaptioner { vision, decoder, tokenizer, side: cfg.vision_config.image_size, bos, eos, max_tokens, name })
    }

    /// The pixels the vision tower takes: `side` by `side`, channels first,
    /// normalised.
    fn pixels(&self, image: &Image) -> Result<Vec<f32>> {
        let rgb = image::RgbImage::from_raw(image.width, image.height, image.rgb.clone()).ok_or_else(|| model_err("an image's pixels do not fill its size"))?;
        let side = self.side as u32;
        let small = image::imageops::resize(&rgb, side, side, image::imageops::FilterType::CatmullRom);
        let n = self.side * self.side;
        let mut out = vec![0f32; 3 * n];
        for (i, p) in small.pixels().enumerate() {
            for c in 0..3 {
                out[c * n + i] = (p[c] as f32 / 255.0 - Self::MEAN[c]) / Self::STD[c];
            }
        }
        Ok(out)
    }

    /// Captions a batch: the sequences run in step, and one that has ended
    /// takes `[SEP]` until all have.
    fn captions(&self, images: &[&Image]) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        use candle_core::{Module, Tensor, D};
        let dev = candle_core::Device::Cpu;
        let pixels = images.iter().map(|i| self.pixels(i)).collect::<Result<Vec<_>>>()?.concat();
        let t = Tensor::from_vec(pixels, (images.len(), 3, self.side, self.side), &dev)?;
        let embeds = self.vision.vision_model().forward(&t)?;
        let mut state = self.decoder.start(&embeds)?;
        let n = images.len();
        let mut tokens: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut done = vec![false; n];
        let mut next = vec![self.bos; n];
        for pos in 0..self.max_tokens - 1 {
            let logits = self.decoder.step(&Tensor::new(next.as_slice(), &dev)?, pos, &mut state)?;
            next = logits.argmax(D::Minus1)?.to_vec1::<u32>()?;
            for j in 0..n {
                if done[j] || next[j] == self.eos {
                    done[j] = true;
                    next[j] = self.eos;
                } else {
                    tokens[j].push(next[j]);
                }
            }
            if done.iter().all(|&d| d) {
                break;
            }
        }
        tokens.iter().map(|t| self.tokenizer.decode(t, true).map(|s| s.trim().to_string())).collect()
    }
}

#[cfg(feature = "vision")]
impl Deriver for BlipCaptioner {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn modality(&self) -> Modality {
        Modality::Image
    }
    fn derive(&self, inputs: &[Input<'_>]) -> Result<Vec<String>> {
        let images = inputs
            .iter()
            .map(|i| match i {
                Input::Image(m) => Ok(*m),
                i => Err(model_err(format!("model {} makes text of images, not {}s", self.name, i.modality().as_str()))),
            })
            .collect::<Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(images.len());
        for batch in images.chunks(Self::BATCH) {
            out.extend(self.captions(batch).map_err(|e| model_err(format!("{}: {e}", self.name)))?);
        }
        Ok(out)
    }
}

/// BLIP's text decoder (a BERT with cross-attention to the image, and a
/// language-model head), one token of each sequence at a time.
#[cfg(feature = "vision")]
mod blip {
    use candle_core::{Module, Result, Tensor, D};
    use candle_nn::{embedding, layer_norm, linear, Activation, Embedding, LayerNorm, Linear, VarBuilder};
    use candle_transformers::models::blip_text::Config;

    struct Attention {
        query: Linear,
        key: Linear,
        value: Linear,
        dense: Linear,
        norm: LayerNorm,
    }

    impl Attention {
        fn new(cfg: &Config, from: usize, vb: VarBuilder) -> Result<Attention> {
            let (h, s) = (cfg.hidden_size, vb.pp("self"));
            Ok(Attention {
                query: linear(h, h, s.pp("query"))?,
                key: linear(from, h, s.pp("key"))?,
                value: linear(from, h, s.pp("value"))?,
                dense: linear(h, h, vb.pp("output").pp("dense"))?,
                norm: layer_norm(h, cfg.layer_norm_eps, vb.pp("output").pp("LayerNorm"))?,
            })
        }
    }

    struct Layer {
        attention: Attention,
        cross: Attention,
        intermediate: Linear,
        output: Linear,
        norm: LayerNorm,
    }

    pub struct Decoder {
        words: Embedding,
        positions: Embedding,
        norm: LayerNorm,
        layers: Vec<Layer>,
        head_dense: Linear,
        head_norm: LayerNorm,
        head: Linear,
        act: Activation,
        heads: usize,
    }

    /// A batch's keys and values: of the image, projected once per layer,
    /// and of the tokens so far.
    pub struct State {
        image: Vec<(Tensor, Tensor)>,
        tokens: Vec<Option<(Tensor, Tensor)>>,
    }

    impl Decoder {
        pub fn new(cfg: &Config, vb: VarBuilder) -> Result<Decoder> {
            let (h, bert) = (cfg.hidden_size, vb.pp("bert"));
            let e = bert.pp("embeddings");
            let layers = (0..cfg.num_hidden_layers)
                .map(|i| {
                    let l = bert.pp("encoder").pp("layer").pp(i);
                    Ok(Layer {
                        attention: Attention::new(cfg, h, l.pp("attention"))?,
                        cross: Attention::new(cfg, cfg.encoder_hidden_size, l.pp("crossattention"))?,
                        intermediate: linear(h, cfg.intermediate_size, l.pp("intermediate").pp("dense"))?,
                        output: linear(cfg.intermediate_size, h, l.pp("output").pp("dense"))?,
                        norm: layer_norm(h, cfg.layer_norm_eps, l.pp("output").pp("LayerNorm"))?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let p = vb.pp("cls").pp("predictions");
            let head = Linear::new(p.get((cfg.vocab_size, h), "decoder.weight")?, Some(p.get(cfg.vocab_size, "bias")?));
            Ok(Decoder {
                words: embedding(cfg.vocab_size, h, e.pp("word_embeddings"))?,
                positions: embedding(cfg.max_position_embeddings, h, e.pp("position_embeddings"))?,
                norm: layer_norm(h, cfg.layer_norm_eps, e.pp("LayerNorm"))?,
                layers,
                head_dense: linear(h, h, p.pp("transform").pp("dense"))?,
                head_norm: layer_norm(h, cfg.layer_norm_eps, p.pp("transform").pp("LayerNorm"))?,
                head,
                act: cfg.hidden_act,
                heads: cfg.num_attention_heads,
            })
        }

        /// `(batch, len, hidden)` as `(batch, heads, len, hidden / heads)`.
        fn split(&self, x: &Tensor) -> Result<Tensor> {
            let (b, n, h) = x.dims3()?;
            x.reshape((b, n, self.heads, h / self.heads))?.transpose(1, 2)?.contiguous()
        }

        /// The state of a batch whose images' states (from the vision
        /// tower) are `image`.
        pub fn start(&self, image: &Tensor) -> Result<State> {
            let image = self.layers.iter().map(|l| Ok((self.split(&l.cross.key.forward(image)?)?, self.split(&l.cross.value.forward(image)?)?))).collect::<Result<Vec<_>>>()?;
            Ok(State { image, tokens: (0..self.layers.len()).map(|_| None).collect() })
        }

        fn attend(&self, a: &Attention, x: &Tensor, k: &Tensor, v: &Tensor) -> Result<Tensor> {
            let q = self.split(&a.query.forward(x)?)?;
            let scale = 1.0 / ((q.dim(D::Minus1)?) as f64).sqrt();
            let w = candle_nn::ops::softmax_last_dim(&(q.matmul(&k.t()?)? * scale)?)?;
            let (b, _, n, _) = q.dims4()?;
            let out = w.matmul(v)?.transpose(1, 2)?.reshape((b, n, ()))?;
            a.norm.forward(&(a.dense.forward(&out)? + x)?)
        }

        /// The next token's logits `(batch, vocabulary)` after `ids`, the
        /// batch's tokens at position `pos`.
        pub fn step(&self, ids: &Tensor, pos: usize, state: &mut State) -> Result<Tensor> {
            let at = Tensor::new(&[pos as u32], ids.device())?;
            let x = self.words.forward(&ids.unsqueeze(1)?)?.broadcast_add(&self.positions.forward(&at)?)?;
            let mut x = self.norm.forward(&x)?;
            for (i, l) in self.layers.iter().enumerate() {
                let (k, v) = (self.split(&l.attention.key.forward(&x)?)?, self.split(&l.attention.value.forward(&x)?)?);
                let (k, v) = match state.tokens[i].take() {
                    Some((pk, pv)) => (Tensor::cat(&[&pk, &k], 2)?, Tensor::cat(&[&pv, &v], 2)?),
                    None => (k, v),
                };
                x = self.attend(&l.attention, &x, &k, &v)?;
                state.tokens[i] = Some((k, v));
                let (ik, iv) = &state.image[i];
                x = self.attend(&l.cross, &x, ik, iv)?;
                let y = l.output.forward(&l.intermediate.forward(&x)?.apply(&self.act)?)?;
                x = l.norm.forward(&(y + x)?)?;
            }
            let x = self.head_norm.forward(&self.head_dense.forward(&x)?.apply(&self.act)?)?;
            self.head.forward(&x)?.squeeze(1)
        }
    }
}
