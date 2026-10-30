//! Text made from media: the models behind a TEXT field declared with
//! [`crate::schema::Derive`], such as a caption of each row's image. The
//! text is made before the writer, as embedded vectors are, and logged
//! with the row, so replicas and merges never run a model; BM25,
//! synonyms and text vectors then work on it as on any text.
//!
//! * [`BlipCaptioner`] (feature `vision`): BLIP's captioner through candle,
//!   `blip-image-captioning-base`. Seconds an image on a few cores.
//! * [`WhisperTranscriber`] (feature `audio`): Whisper's transcripts of
//!   speech through candle, `whisper-small`: about 5 s for each 30
//!   seconds of sound on 4 cores.
//! * [`ColorNamer`] (`colors`): names an image's main colours, for tests
//!   and demos without model files.
//!
//! [`open`] finds a model by the name a schema gives it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::embed::{Image, Input, Modality};
#[cfg(feature = "audio")]
use crate::embed::Audio;
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
pub const KNOWN_MODELS: &[(&str, &str, &str)] = &[
    ("blip-image-captioning-base", "Salesforce/blip-image-captioning-base", "BLIP, captions of images (feature vision)"),
    ("whisper-small", "openai/whisper-small", "Whisper, transcripts of speech in 99 languages (feature audio)"),
];

/// The files the model directory of `name` ([`KNOWN_MODELS`]) needs:
/// BLIP's repository ships its weights as a PyTorch file only; Whisper's
/// as safetensors, with the generation settings (languages, suppressed
/// tokens) in a file of their own.
pub fn model_files(name: &str) -> &'static [&'static str] {
    match name.starts_with("whisper") {
        true => &["config.json", "tokenizer.json", "generation_config.json", "model.safetensors"],
        false => &["config.json", "tokenizer.json", "pytorch_model.bin"],
    }
}

/// Opens the model a derived field names: `colors`, or a directory under
/// `models` (or an absolute path) holding a BLIP captioner or a Whisper
/// model.
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
    if config["model_type"] == "whisper" {
        #[cfg(feature = "audio")]
        return Ok(Arc::new(WhisperTranscriber::open(&dir)?));
        #[cfg(not(feature = "audio"))]
        return Err(model_err(format!("model {model} is a Whisper model: build with the `audio` feature")));
    }
    Err(model_err(format!("model {model} does not make text of media (a BLIP captioner or a Whisper model does)")))
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

// ---------------------------------------------------------------- whisper

/// Whisper's transcripts of speech (`openai/whisper-small`, Apache-2.0).
/// The sound is resampled to 16 kHz and read in 30-second windows, each a
/// log-mel spectrogram as the `transformers` library computes it. The
/// decoder writes a window's text greedily after `<|startoftranscript|>`,
/// the language it detects in the file's first window,
/// `<|transcribe|>` and `<|notimestamps|>`, without the tokens the
/// model's generation settings suppress, until `<|endoftext|>`: what
/// `transformers` generates for a window by default. A file's windows'
/// texts are joined by spaces. The model is Strata's own over the
/// repository's `model.safetensors`: windows run in batches, and the
/// decoder keeps each layer's keys and values of the sound and of the
/// tokens so far.
#[cfg(feature = "audio")]
pub struct WhisperTranscriber {
    model: whisper::Model,
    tokenizer: tokenizers::Tokenizer,
    /// `(mel bins, 201)`.
    filters: candle_core::Tensor,
    sot: u32,
    eot: u32,
    /// The tokens after `<|startoftranscript|>` and the language.
    task: Vec<u32>,
    languages: Vec<u32>,
    /// Added to the logits of every step, and of the first.
    suppress: candle_core::Tensor,
    suppress_first: candle_core::Tensor,
    max_tokens: usize,
    name: String,
}

#[cfg(feature = "audio")]
impl WhisperTranscriber {
    /// Windows transcribed at once.
    const BATCH: usize = 4;
    const RATE: u32 = 16_000;
    /// Samples a window holds: 30 seconds.
    const WINDOW: usize = 480_000;

    pub fn open(dir: &Path) -> Result<WhisperTranscriber> {
        use candle_core::{Device, Tensor};
        use candle_nn::VarBuilder;
        let e = |x: String| model_err(format!("{}: {x}", dir.display()));
        let json = |f: &str| -> Result<serde_json::Value> {
            let text = std::fs::read_to_string(dir.join(f)).map_err(|x| e(format!("{f}: {x}")))?;
            serde_json::from_str(&text).map_err(|x| e(format!("{f}: {x}")))
        };
        let (config, generation) = (json("config.json")?, json("generation_config.json")?);
        let cfg: whisper::Config = serde_json::from_value(config).map_err(|x| e(x.to_string()))?;
        let id = |v: &serde_json::Value, what: &str| v.as_u64().map(|i| i as u32).ok_or_else(|| e(format!("generation_config.json has no {what}")));
        let (sot, eot) = (id(&generation["decoder_start_token_id"], "decoder_start_token_id")?, id(&generation["eos_token_id"], "eos_token_id")?);
        let no_timestamps = id(&generation["no_timestamps_token_id"], "no_timestamps_token_id")?;
        let mut languages: Vec<u32> = generation["lang_to_id"].as_object().map(|m| m.values().filter_map(|v| v.as_u64()).map(|v| v as u32).collect()).unwrap_or_default();
        languages.sort();
        // A multilingual model is told the language and the task; an
        // English one only that there are no timestamps.
        let task = match languages.is_empty() {
            true => vec![no_timestamps],
            false => vec![id(&generation["task_to_id"]["transcribe"], "task_to_id.transcribe")?, no_timestamps],
        };
        let ids = |k: &str| generation[k].as_array().map(|a| a.iter().filter_map(|v| v.as_u64()).map(|v| v as usize).collect::<Vec<_>>()).unwrap_or_default();
        let dev = Device::Cpu;
        let mask = |ids: &[usize]| {
            let mut m = vec![0f32; cfg.vocab_size];
            for &i in ids.iter().filter(|&&i| i < cfg.vocab_size) {
                m[i] = f32::NEG_INFINITY;
            }
            Tensor::from_vec(m, cfg.vocab_size, &dev)
        };
        let suppress = mask(&ids("suppress_tokens")).map_err(|x| e(x.to_string()))?;
        let suppress_first = mask(&[ids("suppress_tokens"), ids("begin_suppress_tokens")].concat()).map_err(|x| e(x.to_string()))?;
        let filters = crate::audio::mel_filters(Self::RATE, 400, cfg.num_mel_bins, 0.0, Self::RATE as f64 / 2.0, false, true);
        let filters = Tensor::from_vec(filters.concat(), (cfg.num_mel_bins, 201), &dev).map_err(|x| e(x.to_string()))?;
        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|x| e(x.to_string()))?;
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], candle_core::DType::F32, &dev) }.map_err(|x| e(x.to_string()))?;
        let model = whisper::Model::new(&cfg, vb.pp("model")).map_err(|x| e(x.to_string()))?;
        let name = format!("whisper:{}", dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(WhisperTranscriber { model, tokenizer, filters, sot, eot, task, languages, suppress, suppress_first, max_tokens: cfg.max_target_positions, name })
    }

    /// The log-mel spectrogram of a window of 16 kHz sound, `(mel bins,
    /// 3000)`: the window padded with silence to 30 seconds, frames 10 ms
    /// apart, log10 of the mel power, floored 8 below its peak and scaled
    /// as Whisper's feature extractor does.
    fn mel(&self, window: &[f32]) -> candle_core::Result<candle_core::Tensor> {
        let mut x = window.to_vec();
        x.resize(Self::WINDOW, 0.0);
        let power = crate::audio::power_spectrogram(&x, 400, 160, self.filters.device())?.narrow(0, 0, Self::WINDOW / 160)?;
        let mel = self.filters.matmul(&power.t()?)?;
        let log = (mel.maximum(1e-10)?.log()? / std::f64::consts::LN_10)?;
        let peak = log.flatten_all()?.max(0)?.to_scalar::<f32>()? as f64;
        (log.maximum(peak - 8.0)? + 4.0)? / 4.0
    }

    /// Transcribes a batch of windows (file, samples), each sequence in
    /// step; one that has ended takes `<|endoftext|>` until all have.
    /// `language` holds each file's language, found here for a file whose
    /// first window this is.
    fn windows(&self, batch: &[(usize, &[f32])], language: &mut [Option<u32>]) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        use candle_core::{Tensor, D};
        let dev = candle_core::Device::Cpu;
        let n = batch.len();
        let mels = batch.iter().map(|(_, w)| self.mel(w)).collect::<candle_core::Result<Vec<_>>>()?;
        let sound = self.model.encode(&Tensor::stack(&mels, 0)?)?;
        let mut state = self.model.start(&sound)?;
        let mut logits = self.model.step(&Tensor::new(vec![self.sot; n], &dev)?, 0, &mut state)?;
        let mut prompt: Vec<Vec<u32>> = Vec::with_capacity(n);
        for (j, (file, _)) in batch.iter().enumerate() {
            if !self.languages.is_empty() && language[*file].is_none() {
                let row: Vec<f32> = logits.get(j)?.to_vec1()?;
                language[*file] = self.languages.iter().copied().max_by(|a, b| row[*a as usize].total_cmp(&row[*b as usize]));
            }
            prompt.push(language[*file].into_iter().chain(self.task.iter().copied()).collect());
        }
        let mut pos = 1;
        for p in 0..prompt[0].len() {
            logits = self.model.step(&Tensor::new(prompt.iter().map(|t| t[p]).collect::<Vec<_>>(), &dev)?, pos, &mut state)?;
            pos += 1;
        }
        let mut tokens: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut done = vec![false; n];
        while pos < self.max_tokens {
            let mask = if tokens.iter().all(|t| t.is_empty()) { &self.suppress_first } else { &self.suppress };
            let mut next = logits.broadcast_add(mask)?.argmax(D::Minus1)?.to_vec1::<u32>()?;
            for j in 0..n {
                if done[j] || next[j] == self.eot {
                    done[j] = true;
                    next[j] = self.eot;
                } else {
                    tokens[j].push(next[j]);
                }
            }
            if done.iter().all(|&d| d) {
                break;
            }
            logits = self.model.step(&Tensor::new(next, &dev)?, pos, &mut state)?;
            pos += 1;
        }
        tokens.iter().map(|t| self.tokenizer.decode(t, true).map(|s| clean_up(&s).trim().to_string())).collect()
    }
}

/// Takes out the spaces before punctuation and English contractions, as
/// the `transformers` library does to a decoded text when the tokenizer's
/// `clean_up_tokenization_spaces` is set, as Whisper's is.
#[cfg(feature = "audio")]
fn clean_up(s: &str) -> String {
    [(" .", "."), (" ?", "?"), (" !", "!"), (" ,", ","), (" ' ", "'"), (" n't", "n't"), (" 'm", "'m"), (" 's", "'s"), (" 've", "'ve"), (" 're", "'re")].iter().fold(s.to_string(), |s, (a, b)| s.replace(a, b))
}

#[cfg(feature = "audio")]
impl Deriver for WhisperTranscriber {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn modality(&self) -> Modality {
        Modality::Audio
    }
    fn derive(&self, inputs: &[Input<'_>]) -> Result<Vec<String>> {
        let sounds: Vec<&Audio> = inputs
            .iter()
            .map(|i| match i {
                Input::Audio(a) => Ok(*a),
                i => Err(model_err(format!("model {} makes text of audio, not {}s", self.name, i.modality().as_str()))),
            })
            .collect::<Result<_>>()?;
        let pcm: Vec<Vec<f32>> = sounds.iter().map(|a| crate::audio::resample(&a.samples, a.rate, Self::RATE)).collect();
        // Windows of less than a tenth of a second hold no speech.
        let windows: Vec<(usize, &[f32])> = pcm.iter().enumerate().flat_map(|(i, x)| x.chunks(Self::WINDOW).filter(|w| w.len() >= 1_600).map(move |w| (i, w))).collect();
        let mut language = vec![None; sounds.len()];
        let mut texts: Vec<Vec<String>> = vec![Vec::new(); sounds.len()];
        for batch in windows.chunks(Self::BATCH) {
            let out = self.windows(batch, &mut language).map_err(|e| model_err(format!("{}: {e}", self.name)))?;
            for ((i, _), t) in batch.iter().zip(out) {
                if !t.is_empty() {
                    texts[*i].push(t);
                }
            }
        }
        Ok(texts.into_iter().map(|t| t.join(" ")).collect())
    }
}

/// Whisper's encoder and decoder (pre-norm transformers, the decoder with
/// cross-attention to the encoder's states), the decoder one token of
/// each sequence at a time.
#[cfg(feature = "audio")]
mod whisper {
    use candle_core::{Module, Result, Tensor, D};
    use candle_nn::{conv1d, embedding, layer_norm, linear, linear_no_bias, Conv1d, Conv1dConfig, Embedding, LayerNorm, Linear, VarBuilder};

    /// What of `config.json` the model needs.
    #[derive(serde::Deserialize)]
    pub struct Config {
        pub num_mel_bins: usize,
        pub d_model: usize,
        pub encoder_layers: usize,
        pub encoder_attention_heads: usize,
        pub decoder_layers: usize,
        pub decoder_attention_heads: usize,
        pub encoder_ffn_dim: usize,
        pub decoder_ffn_dim: usize,
        pub max_source_positions: usize,
        pub max_target_positions: usize,
        pub vocab_size: usize,
    }

    /// GELU (the exact one, of `erf`) of `x`, its rows split among the
    /// cores: candle computes it on one.
    fn gelu(x: &Tensor) -> Result<Tensor> {
        let dims = x.dims().to_vec();
        let width = *dims.last().expect("a tensor of rows");
        let rows = x.elem_count() / width.max(1);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(rows).max(1);
        if threads == 1 {
            return x.gelu_erf();
        }
        let flat = x.reshape((rows, width))?;
        let per = rows.div_ceil(threads);
        let parts = std::thread::scope(|s| {
            let flat = &flat;
            let jobs: Vec<_> = (0..rows).step_by(per).map(|at| s.spawn(move || flat.narrow(0, at, per.min(rows - at))?.gelu_erf())).collect();
            jobs.into_iter().map(|j| j.join().expect("a GELU thread")).collect::<Result<Vec<_>>>()
        })?;
        Tensor::cat(&parts, 0)?.reshape(dims)
    }

    struct Attention {
        query: Linear,
        key: Linear,
        value: Linear,
        out: Linear,
        heads: usize,
    }

    impl Attention {
        fn new(d: usize, heads: usize, vb: VarBuilder) -> Result<Attention> {
            Ok(Attention { query: linear(d, d, vb.pp("q_proj"))?, key: linear_no_bias(d, d, vb.pp("k_proj"))?, value: linear(d, d, vb.pp("v_proj"))?, out: linear(d, d, vb.pp("out_proj"))?, heads })
        }

        /// `(batch, len, d)` as `(batch, heads, len, d / heads)`.
        fn split(&self, x: &Tensor) -> Result<Tensor> {
            let (b, n, d) = x.dims3()?;
            x.reshape((b, n, self.heads, d / self.heads))?.transpose(1, 2)?.contiguous()
        }

        /// The keys of `x`, transposed, `(batch, heads, d / heads, len)`,
        /// and its values, `(batch, heads, len, d / heads)`.
        fn keys(&self, x: &Tensor) -> Result<(Tensor, Tensor)> {
            let k = self.key.forward(x)?;
            let (b, n, d) = k.dims3()?;
            let k = k.reshape((b, n, self.heads, d / self.heads))?.permute((0, 2, 3, 1))?.contiguous()?;
            Ok((k, self.split(&self.value.forward(x)?)?))
        }

        fn attend(&self, x: &Tensor, k: &Tensor, v: &Tensor) -> Result<Tensor> {
            let q = self.split(&self.query.forward(x)?)?;
            let q = (&q * (1.0 / (q.dim(D::Minus1)? as f64).sqrt()))?;
            let w = candle_nn::ops::softmax_last_dim(&q.matmul(k)?)?;
            let (b, _, n, _) = q.dims4()?;
            self.out.forward(&w.matmul(v)?.transpose(1, 2)?.reshape((b, n, ()))?)
        }
    }

    struct Block {
        attention: Attention,
        attention_norm: LayerNorm,
        cross: Option<(Attention, LayerNorm)>,
        fc1: Linear,
        fc2: Linear,
        mlp_norm: LayerNorm,
    }

    impl Block {
        fn new(d: usize, heads: usize, ffn: usize, cross: bool, vb: VarBuilder) -> Result<Block> {
            let cross = match cross {
                true => Some((Attention::new(d, heads, vb.pp("encoder_attn"))?, layer_norm(d, 1e-5, vb.pp("encoder_attn_layer_norm"))?)),
                false => None,
            };
            Ok(Block {
                attention: Attention::new(d, heads, vb.pp("self_attn"))?,
                attention_norm: layer_norm(d, 1e-5, vb.pp("self_attn_layer_norm"))?,
                cross,
                fc1: linear(d, ffn, vb.pp("fc1"))?,
                fc2: linear(ffn, d, vb.pp("fc2"))?,
                mlp_norm: layer_norm(d, 1e-5, vb.pp("final_layer_norm"))?,
            })
        }

        fn mlp(&self, x: &Tensor) -> Result<Tensor> {
            x + self.fc2.forward(&gelu(&self.fc1.forward(&self.mlp_norm.forward(x)?)?)?)?
        }
    }

    pub struct Model {
        conv1: Conv1d,
        conv2: Conv1d,
        sound_positions: Tensor,
        encoder: Vec<Block>,
        encoder_norm: LayerNorm,
        words: Embedding,
        word_positions: Tensor,
        decoder: Vec<Block>,
        decoder_norm: LayerNorm,
    }

    /// A batch's keys and values: of the sound, projected once per layer,
    /// and of the tokens so far.
    pub struct State {
        sound: Vec<(Tensor, Tensor)>,
        tokens: Vec<Option<(Tensor, Tensor)>>,
    }

    impl Model {
        pub fn new(cfg: &Config, vb: VarBuilder) -> Result<Model> {
            let d = cfg.d_model;
            let (enc, dec) = (vb.pp("encoder"), vb.pp("decoder"));
            let conv = |stride| Conv1dConfig { padding: 1, stride, ..Default::default() };
            Ok(Model {
                conv1: conv1d(cfg.num_mel_bins, d, 3, conv(1), enc.pp("conv1"))?,
                conv2: conv1d(d, d, 3, conv(2), enc.pp("conv2"))?,
                sound_positions: enc.get((cfg.max_source_positions, d), "embed_positions.weight")?,
                encoder: (0..cfg.encoder_layers).map(|i| Block::new(d, cfg.encoder_attention_heads, cfg.encoder_ffn_dim, false, enc.pp("layers").pp(i))).collect::<Result<_>>()?,
                encoder_norm: layer_norm(d, 1e-5, enc.pp("layer_norm"))?,
                words: embedding(cfg.vocab_size, d, dec.pp("embed_tokens"))?,
                word_positions: dec.get((cfg.max_target_positions, d), "embed_positions.weight")?,
                decoder: (0..cfg.decoder_layers).map(|i| Block::new(d, cfg.decoder_attention_heads, cfg.decoder_ffn_dim, true, dec.pp("layers").pp(i))).collect::<Result<_>>()?,
                decoder_norm: layer_norm(d, 1e-5, dec.pp("layer_norm"))?,
            })
        }

        /// The encoder's states `(batch, frames / 2, d)` of log-mel
        /// spectrograms `(batch, mel bins, frames)`.
        pub fn encode(&self, mel: &Tensor) -> Result<Tensor> {
            let x = gelu(&self.conv2.forward(&gelu(&self.conv1.forward(mel)?)?)?)?.transpose(1, 2)?;
            let mut x = x.broadcast_add(&self.sound_positions.narrow(0, 0, x.dim(1)?)?)?;
            for b in &self.encoder {
                let h = b.attention_norm.forward(&x)?;
                let (k, v) = b.attention.keys(&h)?;
                x = b.mlp(&(&x + b.attention.attend(&h, &k, &v)?)?)?;
            }
            self.encoder_norm.forward(&x)
        }

        /// The state of a batch whose sound's states are `sound`.
        pub fn start(&self, sound: &Tensor) -> Result<State> {
            let sound = self.decoder.iter().map(|b| b.cross.as_ref().expect("a decoder block attends to the sound").0.keys(sound)).collect::<Result<Vec<_>>>()?;
            Ok(State { sound, tokens: (0..self.decoder.len()).map(|_| None).collect() })
        }

        /// The next token's logits `(batch, vocabulary)` after `ids`, the
        /// batch's tokens at position `pos`.
        pub fn step(&self, ids: &Tensor, pos: usize, state: &mut State) -> Result<Tensor> {
            let mut x = self.words.forward(&ids.unsqueeze(1)?)?.broadcast_add(&self.word_positions.narrow(0, pos, 1)?)?;
            for (i, b) in self.decoder.iter().enumerate() {
                let h = b.attention_norm.forward(&x)?;
                let (k, v) = b.attention.keys(&h)?;
                let (k, v) = match state.tokens[i].take() {
                    Some((pk, pv)) => (Tensor::cat(&[&pk, &k], 3)?, Tensor::cat(&[&pv, &v], 2)?),
                    None => (k, v),
                };
                x = (&x + b.attention.attend(&h, &k, &v)?)?;
                state.tokens[i] = Some((k, v));
                let (cross, norm) = b.cross.as_ref().expect("a decoder block attends to the sound");
                let (sk, sv) = &state.sound[i];
                x = (&x + cross.attend(&norm.forward(&x)?, sk, sv)?)?;
                x = b.mlp(&x)?;
            }
            let x = self.decoder_norm.forward(&x)?.squeeze(1)?;
            x.matmul(&self.words.embeddings().t()?)
        }
    }
}
