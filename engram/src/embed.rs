//! Text embedders: Strata's ([`strata::embed`]), opened from engram's model
//! directory. Every vector is unit length, so Strata's dot product is cosine
//! similarity.
//!
//! * [`StaticEmbedder`]: a Model2Vec model (token lookup and mean). Fast
//!   enough to embed every hook payload inline.
//! * `BertEmbedder` (feature `bert`): BGE-small through candle.
//! * [`HashEmbedder`]: feature hashing of words, for tests without model files.

use std::path::PathBuf;

#[cfg(feature = "bert")]
pub use strata::embed::BertEmbedder;
pub use strata::embed::{fnv, Embedder, HashEmbedder, StaticEmbedder};

use crate::{err, Result};

/// Where models live: `$ENGRAM_MODELS`, else `$ENGRAM_HOME/models`.
pub fn models_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ENGRAM_MODELS") {
        return PathBuf::from(d);
    }
    crate::paths::engram_home().join("models")
}

/// Opens an embedder from a spec: `static[:<dir>]`, `bert[:<dir>]` or `hash[:<dim>]`.
pub fn open(spec: &str) -> Result<Box<dyn Embedder>> {
    let (kind, arg) = spec.split_once(':').unwrap_or((spec, ""));
    match kind {
        "static" => {
            let dir = if arg.is_empty() { models_dir().join("potion-retrieval-32M") } else { PathBuf::from(arg) };
            Ok(Box::new(StaticEmbedder::open(&dir).map_err(|e| err(format!("{e} (run `engram fetch-model`)")))?))
        }
        "hash" => Ok(Box::new(HashEmbedder { dim: arg.parse().unwrap_or(256) })),
        #[cfg(feature = "bert")]
        "bert" => {
            let dir = if arg.is_empty() { models_dir().join("bge-small-en-v1.5") } else { PathBuf::from(arg) };
            Ok(Box::new(BertEmbedder::open(&dir).map_err(|e| err(format!("{e} (run `engram fetch-model bert`)")))?))
        }
        _ => Err(err(format!("unknown embedder {spec} (static, hash{})", if cfg!(feature = "bert") { ", bert" } else { "" }))),
    }
}
