//! Embedding models behind an HTTP API ([`crate::embed`]): a field names
//! one as `<provider>/<model>`, such as `openai/text-embedding-3-small`,
//! `cohere/embed-v4.0`, `voyage/voyage-3.5`, `gemini/gemini-embedding-001`,
//! `mistral/mistral-embed`, `jina/jina-embeddings-v3` or
//! `ollama/nomic-embed-text`, and Strata calls the provider on every write
//! that leaves the vector out and for every query text, as it runs a local
//! model. Any other server of one of these APIs (vLLM, Hugging Face TEI,
//! LM Studio, LiteLLM, Azure OpenAI) is a model directory holding an
//! `endpoint.json` ([`Endpoint::from_config`]).
//!
//! Secrets come from the environment (`OPENAI_API_KEY`, `COHERE_API_KEY`,
//! ...), never from the schema, so a database names its model without its
//! key. Requests go in batches (by count and by characters), at most
//! `concurrency` at once per model across every write and query, and are
//! retried with backoff on a rate limit (429, honouring `Retry-After`), a
//! server error or a dropped connection. A text longer than the model takes
//! is cut. Documents and queries are told apart where the API asks
//! (Cohere's, Voyage's and Gemini's input types, Jina's tasks, the prefixes
//! of models such as nomic-embed-text). A field's `dim` is asked for when
//! it is fewer than the model makes and the model can shorten its vectors
//! (Matryoshka models: OpenAI's text-embedding-3, Cohere's embed-v4.0,
//! Voyage 3 and 3.5, Gemini, Jina v3 and v4). Vectors are made unit length,
//! as every vector Strata compares is. The last [`QUERY_CACHE`] query texts
//! are remembered, so a repeated search costs no request.

use std::time::Duration;

use serde_json::Value as J;

use crate::error::{Error, Result};

#[cfg(feature = "hosted")]
mod client;
#[cfg(feature = "hosted")]
pub use client::{HostedEmbedder, HostedReranker};

/// Query texts whose vectors a model keeps.
pub const QUERY_CACHE: usize = 4096;

/// The wire format of a hosted model's API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// `POST /embeddings` with `{"model", "input": [..]}`, answered with
    /// `{"data": [{"embedding", "index"}]}`: OpenAI's, and that of vLLM,
    /// TEI, LM Studio, LiteLLM, Azure OpenAI, Together, Fireworks and
    /// Mistral.
    OpenAi,
    /// Cohere's `POST /v2/embed`.
    Cohere,
    /// Voyage AI's `POST /v1/embeddings` (OpenAI's shape with input types).
    Voyage,
    /// Google's Gemini API, `POST /models/<model>:batchEmbedContents`.
    Gemini,
    /// Jina AI's `POST /v1/embeddings` (OpenAI's shape with tasks).
    Jina,
    /// Ollama's `POST /api/embed`.
    Ollama,
}

impl Api {
    pub fn parse(s: &str) -> Option<Api> {
        Some(match s.to_ascii_lowercase().as_str() {
            "openai" | "mistral" => Api::OpenAi,
            "cohere" => Api::Cohere,
            "voyage" => Api::Voyage,
            "gemini" => Api::Gemini,
            "jina" => Api::Jina,
            "ollama" => Api::Ollama,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Api::OpenAi => "openai",
            Api::Cohere => "cohere",
            Api::Voyage => "voyage",
            Api::Gemini => "gemini",
            Api::Jina => "jina",
            Api::Ollama => "ollama",
        }
    }
}

/// A provider a model's name can start with: `<prefix>/<model>`.
struct Provider {
    prefix: &'static str,
    api: Api,
    /// The base URL requests go under, unless `<PREFIX>_BASE_URL` (or
    /// [`Provider::base_env`]) says another.
    base: &'static str,
    base_env: &'static str,
    /// The variables that may hold the secret, the first set winning.
    keys: &'static [&'static str],
    /// The header the secret goes in: `authorization` takes `Bearer <key>`.
    key_header: &'static str,
    /// Texts and characters a request carries at most.
    batch: usize,
    batch_chars: usize,
    /// Characters of a text sent at most (longer texts are cut); APIs that
    /// cut what is too long themselves get the whole text.
    max_chars: usize,
    concurrency: usize,
    /// The name of the parameter that asks for fewer dimensions.
    dim_param: &'static str,
    /// Its cross-encoders' API, if it has one: the path under `base`, the
    /// wire format, and the texts a request carries at most.
    rerank: Option<(&'static str, RerankApi, usize)>,
}

const PROVIDERS: &[Provider] = &[
    Provider { prefix: "openai", api: Api::OpenAi, base: "https://api.openai.com/v1", base_env: "OPENAI_BASE_URL", keys: &["OPENAI_API_KEY"], key_header: "authorization", batch: 256, batch_chars: 400_000, max_chars: 24_000, concurrency: 4, dim_param: "dimensions", rerank: None },
    Provider { prefix: "cohere", api: Api::Cohere, base: "https://api.cohere.com", base_env: "COHERE_BASE_URL", keys: &["COHERE_API_KEY", "CO_API_KEY"], key_header: "authorization", batch: 96, batch_chars: usize::MAX, max_chars: usize::MAX, concurrency: 4, dim_param: "output_dimension", rerank: Some(("/v2/rerank", RerankApi::Cohere, 1000)) },
    Provider { prefix: "voyage", api: Api::Voyage, base: "https://api.voyageai.com/v1", base_env: "VOYAGE_BASE_URL", keys: &["VOYAGE_API_KEY"], key_header: "authorization", batch: 128, batch_chars: 300_000, max_chars: usize::MAX, concurrency: 4, dim_param: "output_dimension", rerank: Some(("/rerank", RerankApi::Voyage, 1000)) },
    Provider { prefix: "gemini", api: Api::Gemini, base: "https://generativelanguage.googleapis.com/v1beta", base_env: "GEMINI_BASE_URL", keys: &["GEMINI_API_KEY", "GOOGLE_API_KEY"], key_header: "x-goog-api-key", batch: 100, batch_chars: usize::MAX, max_chars: 8_000, concurrency: 4, dim_param: "outputDimensionality", rerank: None },
    Provider { prefix: "mistral", api: Api::OpenAi, base: "https://api.mistral.ai/v1", base_env: "MISTRAL_BASE_URL", keys: &["MISTRAL_API_KEY"], key_header: "authorization", batch: 64, batch_chars: 32_000, max_chars: 24_000, concurrency: 4, dim_param: "output_dimension", rerank: None },
    Provider { prefix: "jina", api: Api::Jina, base: "https://api.jina.ai/v1", base_env: "JINA_BASE_URL", keys: &["JINA_API_KEY"], key_header: "authorization", batch: 128, batch_chars: 400_000, max_chars: usize::MAX, concurrency: 4, dim_param: "dimensions", rerank: Some(("/rerank", RerankApi::Jina, 1000)) },
    Provider { prefix: "ollama", api: Api::Ollama, base: "http://localhost:11434", base_env: "OLLAMA_HOST", keys: &[], key_header: "authorization", batch: 32, batch_chars: usize::MAX, max_chars: usize::MAX, concurrency: 2, dim_param: "dimensions", rerank: None },
];

/// What Strata knows of a hosted model: its dimensions, whether it makes
/// fewer on request, and the prefixes its documents and queries take.
pub struct Known {
    pub name: &'static str,
    pub dim: usize,
    pub shortens: bool,
    pub document_prefix: &'static str,
    pub query_prefix: &'static str,
}

const fn known(name: &'static str, dim: usize, shortens: bool) -> Known {
    Known { name, dim, shortens, document_prefix: "", query_prefix: "" }
}

const SEARCH_PASSAGES: &str = "Represent this sentence for searching relevant passages: ";

/// Hosted models Strata knows by name.
pub const KNOWN: &[Known] = &[
    known("openai/text-embedding-3-small", 1536, true),
    known("openai/text-embedding-3-large", 3072, true),
    known("openai/text-embedding-ada-002", 1536, false),
    known("cohere/embed-v4.0", 1536, true),
    known("cohere/embed-english-v3.0", 1024, false),
    known("cohere/embed-multilingual-v3.0", 1024, false),
    known("cohere/embed-english-light-v3.0", 384, false),
    known("cohere/embed-multilingual-light-v3.0", 384, false),
    known("voyage/voyage-3.5", 1024, true),
    known("voyage/voyage-3.5-lite", 1024, true),
    known("voyage/voyage-3-large", 1024, true),
    known("voyage/voyage-code-3", 1024, true),
    known("voyage/voyage-3", 1024, false),
    known("voyage/voyage-3-lite", 512, false),
    known("voyage/voyage-multilingual-2", 1024, false),
    known("voyage/voyage-finance-2", 1024, false),
    known("voyage/voyage-law-2", 1024, false),
    known("gemini/gemini-embedding-001", 3072, true),
    known("gemini/text-embedding-004", 768, true),
    known("mistral/mistral-embed", 1024, false),
    known("mistral/codestral-embed", 1536, true),
    known("jina/jina-embeddings-v3", 1024, true),
    known("jina/jina-embeddings-v4", 2048, true),
    known("jina/jina-clip-v2", 1024, true),
    Known { name: "ollama/nomic-embed-text", dim: 768, shortens: false, document_prefix: "search_document: ", query_prefix: "search_query: " },
    Known { name: "ollama/mxbai-embed-large", dim: 1024, shortens: false, document_prefix: "", query_prefix: SEARCH_PASSAGES },
    Known { name: "ollama/snowflake-arctic-embed", dim: 1024, shortens: false, document_prefix: "", query_prefix: SEARCH_PASSAGES },
    known("ollama/all-minilm", 384, false),
    known("ollama/bge-m3", 1024, false),
];

/// What Strata knows of hosted model `name` (an Ollama tag, `:latest`,
/// left out).
pub fn known_model(name: &str) -> Option<&'static Known> {
    let base = match name.split_once('/') {
        Some(("ollama", m)) => format!("ollama/{}", m.split(':').next().unwrap_or(m)),
        _ => name.to_string(),
    };
    KNOWN.iter().find(|k| k.name.eq_ignore_ascii_case(&base))
}

/// Whether `name` is a hosted model's by its prefix (`openai/...`).
pub fn is_hosted_name(name: &str) -> bool {
    provider(name).is_some()
}

fn provider(name: &str) -> Option<(&'static Provider, &str)> {
    let (prefix, model) = name.split_once('/')?;
    PROVIDERS.iter().find(|p| p.prefix.eq_ignore_ascii_case(prefix)).map(|p| (p, model)).filter(|(_, m)| !m.is_empty())
}

/// Where and how a hosted model is called.
#[derive(Clone)]
pub struct Endpoint {
    /// The name the schema gives the model.
    pub name: String,
    pub api: Api,
    /// The URL requests are POSTed to.
    pub url: String,
    /// The model as the API names it.
    pub model: String,
    /// The secret, if the server takes one.
    pub key: Option<String>,
    /// The header the secret goes in (`authorization`: `Bearer <key>`).
    pub key_header: String,
    /// Where a secret is looked for, for errors (`$OPENAI_API_KEY`).
    pub key_from: String,
    /// The dimensions of its vectors as the model makes them, if known.
    pub native_dim: Option<usize>,
    /// Whether it makes fewer dimensions when asked (`dim_param`).
    pub shortens: bool,
    pub dim_param: String,
    pub document_prefix: String,
    pub query_prefix: String,
    pub batch: usize,
    pub batch_chars: usize,
    pub max_chars: usize,
    pub concurrency: usize,
    /// Tries after the first, on a rate limit or a server's failure.
    pub retries: u32,
    pub timeout: Duration,
    /// Headers every request carries.
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for Endpoint {
    /// Everything but the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("name", &self.name).field("api", &self.api).field("url", &self.url).field("model", &self.model).field("key", &self.key.as_ref().map(|_| "<set>")).field("native_dim", &self.native_dim).finish_non_exhaustive()
    }
}

impl Endpoint {
    /// The endpoint of the model a schema names `name`: a provider's
    /// prefix and the API's name of the model, with `config` (the model
    /// directory's `endpoint.json`, if it has one) over the provider's
    /// defaults, and secrets and base URLs from `env`.
    ///
    /// `endpoint.json` may give `api` (`openai`, `cohere`, `voyage`,
    /// `gemini`, `jina`, `ollama`; needed when the name has no provider's
    /// prefix), `url` (the URL requests go to) or `base_url` (the API's
    /// path is added, as an SDK's base URL), `model`, `dim`, `key_env` (the
    /// variable holding the secret), `key_header` (`api-key` for Azure),
    /// `headers`, `document_prefix`, `query_prefix`, `dimensions` (whether
    /// to ask for a field's fewer dimensions), `batch`, `batch_chars`,
    /// `max_chars`, `concurrency`, `retries` and `timeout_s`.
    pub fn from_config(name: &str, config: Option<&J>, env: &dyn Fn(&str) -> Option<String>) -> Result<Endpoint> {
        let empty = J::Object(Default::default());
        let c = Conf { name, c: config.unwrap_or(&empty) };
        c.check(&["api", "url", "base_url", "model", "dim", "key_env", "key_header", "headers", "document_prefix", "query_prefix", "dimensions", "batch", "batch_chars", "max_chars", "concurrency", "retries", "timeout_s"])?;
        let prov = provider(name);
        let api = match c.text("api")? {
            Some(a) => Api::parse(&a).ok_or_else(|| c.bad(format!("endpoint.json: api {a:?} is one of openai, cohere, voyage, gemini, jina, ollama")))?,
            None => prov.map(|(p, _)| p.api).ok_or_else(|| c.bad("endpoint.json needs \"api\" (openai, cohere, voyage, gemini, jina or ollama), as the name has no provider's prefix".into()))?,
        };
        // A provider's defaults, or those of the API's own provider.
        let p = prov.map(|(p, _)| p).unwrap_or_else(|| PROVIDERS.iter().find(|p| p.api == api).expect("every API has a provider"));
        let model = c.model(prov)?;
        let path = match api {
            Api::OpenAi | Api::Voyage | Api::Jina => "/embeddings".to_string(),
            Api::Cohere => "/v2/embed".to_string(),
            Api::Gemini => format!("/models/{model}:batchEmbedContents"),
            Api::Ollama => "/api/embed".to_string(),
        };
        let (url, own_server) = c.url(Some(p), &path, env)?;
        let (key, key_from) = c.key(Some(p), own_server, env)?;
        let k = known_model(name).or_else(|| known_model(&format!("{}/{model}", p.prefix)));
        let shortens = match c.c.get("dimensions") {
            None | Some(J::Null) => k.is_some_and(|k| k.shortens),
            Some(J::Bool(b)) => *b,
            Some(_) => return Err(c.bad("endpoint.json: dimensions is true or false".into())),
        };
        Ok(Endpoint {
            name: name.to_string(),
            api,
            url,
            model,
            key,
            key_header: c.text("key_header")?.unwrap_or_else(|| p.key_header.to_string()).to_ascii_lowercase(),
            key_from,
            native_dim: c.count("dim")?.or(k.map(|k| k.dim)),
            shortens,
            dim_param: p.dim_param.to_string(),
            document_prefix: c.text("document_prefix")?.unwrap_or_else(|| k.map_or("", |k| k.document_prefix).to_string()),
            query_prefix: c.text("query_prefix")?.unwrap_or_else(|| k.map_or("", |k| k.query_prefix).to_string()),
            batch: c.count("batch")?.unwrap_or(p.batch),
            batch_chars: c.count("batch_chars")?.unwrap_or(p.batch_chars),
            max_chars: c.count("max_chars")?.unwrap_or(p.max_chars),
            concurrency: c.count("concurrency")?.unwrap_or(p.concurrency),
            retries: c.retries()?,
            timeout: Duration::from_secs(c.count("timeout_s")?.unwrap_or(60) as u64),
            headers: c.headers()?,
        })
    }
}

/// The wire format of a hosted cross-encoder's API ([`RerankEndpoint`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RerankApi {
    /// `{"model", "query", "documents": [..]}`, answered with
    /// `{"results": [{"index", "relevance_score"}]}`: Cohere's
    /// `POST /v2/rerank`, and the `/rerank` of vLLM, Infinity and LiteLLM.
    Cohere,
    /// Jina AI's `POST /v1/rerank`: Cohere's shape, asked not to send the
    /// texts back.
    Jina,
    /// Voyage AI's `POST /v1/rerank`, answered with
    /// `{"data": [{"index", "relevance_score"}]}`.
    Voyage,
    /// Hugging Face Text Embeddings Inference's `POST /rerank`, with
    /// `{"query", "texts", "raw_scores": true, "truncate": true}`,
    /// answered with `[{"index", "score"}]`: the model's logit, as a
    /// cross-encoder run here gives it.
    Tei,
}

impl RerankApi {
    pub fn parse(s: &str) -> Option<RerankApi> {
        Some(match s.to_ascii_lowercase().as_str() {
            "cohere" | "vllm" | "infinity" | "litellm" => RerankApi::Cohere,
            "jina" => RerankApi::Jina,
            "voyage" => RerankApi::Voyage,
            "tei" => RerankApi::Tei,
            _ => return None,
        })
    }
}

/// Whether `name` is a hosted cross-encoder's by its prefix (`cohere/...`,
/// `voyage/...`, `jina/...`).
pub fn is_reranker_name(name: &str) -> bool {
    provider(name).is_some_and(|(p, _)| p.rerank.is_some())
}

/// Where and how a hosted cross-encoder is called: one of a provider's, as
/// `cohere/rerank-v3.5`, `voyage/rerank-2.5` or
/// `jina/jina-reranker-v2-base-multilingual`, or a server of one of their
/// APIs or of TEI's that a model directory's `endpoint.json` names (a GPU
/// serving a model too slow for a CPU).
#[derive(Clone)]
pub struct RerankEndpoint {
    pub name: String,
    pub api: RerankApi,
    pub url: String,
    pub model: String,
    pub key: Option<String>,
    pub key_header: String,
    pub key_from: String,
    /// Texts a request carries at most.
    pub batch: usize,
    /// Characters of a text sent at most.
    pub max_chars: usize,
    pub concurrency: usize,
    /// Tries after the first, on a rate limit or a server's failure.
    pub retries: u32,
    pub timeout: Duration,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for RerankEndpoint {
    /// Everything but the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RerankEndpoint").field("name", &self.name).field("api", &self.api).field("url", &self.url).field("model", &self.model).field("key", &self.key.as_ref().map(|_| "<set>")).field("batch", &self.batch).finish_non_exhaustive()
    }
}

impl RerankEndpoint {
    /// The endpoint of the cross-encoder a query names `name`, as
    /// [`Endpoint::from_config`] reads an embedding model's: `endpoint.json`
    /// may give `api` (`cohere`, `jina`, `voyage`, or a server's: `vllm`,
    /// `infinity` and `litellm` take Cohere's shape at `/rerank`, `tei`
    /// TEI's; needed when the name has no provider's prefix), `url` or
    /// `base_url` (a server's needs one), `model`, `key_env`,
    /// `key_header`, `headers`, `batch`, `max_chars`, `concurrency`,
    /// `retries` and `timeout_s`. A query waits on it, so the default
    /// timeout is 10 s and a failed request is tried twice more.
    pub fn from_config(name: &str, config: Option<&J>, env: &dyn Fn(&str) -> Option<String>) -> Result<RerankEndpoint> {
        let empty = J::Object(Default::default());
        let c = Conf { name, c: config.unwrap_or(&empty) };
        c.check(&["api", "url", "base_url", "model", "key_env", "key_header", "headers", "batch", "max_chars", "concurrency", "retries", "timeout_s"])?;
        let prov = provider(name).filter(|(p, _)| p.rerank.is_some());
        let (api, p) = match c.text("api")? {
            Some(a) => {
                let api = RerankApi::parse(&a).ok_or_else(|| c.bad(format!("endpoint.json: api {a:?} is one of cohere, jina, voyage, vllm, infinity, litellm, tei")))?;
                // The defaults of the name's provider, or of the provider
                // the API is named for; none for a server's own (vLLM,
                // Infinity, LiteLLM, TEI).
                let p = prov.map(|(p, _)| p).filter(|p| p.rerank.is_some_and(|r| r.1 == api)).or_else(|| PROVIDERS.iter().find(|p| p.prefix.eq_ignore_ascii_case(&a) && p.rerank.is_some()));
                (api, p)
            }
            None => match prov {
                Some((p, _)) => (p.rerank.expect("a reranking provider").1, Some(p)),
                None => return Err(c.bad("endpoint.json needs \"api\" (cohere, jina, voyage, vllm, infinity, litellm or tei), as the name has no reranking provider's prefix".into())),
            },
        };
        let model = c.model(prov)?;
        let (path, batch) = match p.and_then(|p| p.rerank) {
            Some((path, _, batch)) => (path, batch),
            None => ("/rerank", 32),
        };
        let (url, own_server) = c.url(p, path, env)?;
        let (key, key_from) = c.key(p, own_server, env)?;
        Ok(RerankEndpoint {
            name: name.to_string(),
            api,
            url,
            model,
            key,
            key_header: c.text("key_header")?.unwrap_or_else(|| p.map_or("authorization", |p| p.key_header).to_string()).to_ascii_lowercase(),
            key_from,
            batch: c.count("batch")?.unwrap_or(batch),
            max_chars: c.count("max_chars")?.unwrap_or(usize::MAX),
            concurrency: c.count("concurrency")?.unwrap_or(4),
            retries: match c.c.get("retries") {
                None | Some(J::Null) => 2,
                Some(_) => c.retries()?,
            },
            timeout: Duration::from_secs(c.count("timeout_s")?.unwrap_or(10) as u64),
            headers: c.headers()?,
        })
    }
}

/// An `endpoint.json` being read, its errors naming the model.
struct Conf<'a> {
    name: &'a str,
    c: &'a J,
}

impl Conf<'_> {
    fn bad(&self, m: String) -> Error {
        Error::Schema(format!("model {}: {m}", self.name))
    }

    /// Refuses a file that is not an object of `known` keys.
    fn check(&self, known: &[&str]) -> Result<()> {
        if !self.c.is_object() {
            return Err(self.bad("endpoint.json is not an object".into()));
        }
        if let Some(k) = self.c.as_object().into_iter().flat_map(|o| o.keys()).find(|k| !known.contains(&k.as_str())) {
            return Err(self.bad(format!("endpoint.json: unknown key {k:?} (one of {})", known.join(", "))));
        }
        Ok(())
    }

    fn text(&self, k: &str) -> Result<Option<String>> {
        match self.c.get(k) {
            None | Some(J::Null) => Ok(None),
            Some(J::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(self.bad(format!("endpoint.json: {k} is a string"))),
        }
    }

    fn count(&self, k: &str) -> Result<Option<usize>> {
        match self.c.get(k) {
            None | Some(J::Null) => Ok(None),
            Some(v) => v.as_u64().filter(|&n| n > 0).map(|n| Some(n as usize)).ok_or_else(|| self.bad(format!("endpoint.json: {k} is a positive number"))),
        }
    }

    fn retries(&self) -> Result<u32> {
        match self.c.get("retries") {
            None | Some(J::Null) => Ok(6),
            Some(v) => Ok(v.as_u64().ok_or_else(|| self.bad("endpoint.json: retries is a number".into()))? as u32),
        }
    }

    fn headers(&self) -> Result<Vec<(String, String)>> {
        match self.c.get("headers") {
            None | Some(J::Null) => Ok(Vec::new()),
            Some(J::Object(o)) => o.iter().map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())).ok_or_else(|| self.bad(format!("endpoint.json: header {k} is a string")))).collect(),
            Some(_) => Err(self.bad("endpoint.json: headers is an object of strings".into())),
        }
    }

    /// The API's name of the model: `model`, else the name without the
    /// provider's prefix, else its last part.
    fn model(&self, prov: Option<(&Provider, &str)>) -> Result<String> {
        Ok(match self.text("model")? {
            Some(m) => m,
            None => match prov {
                Some((_, m)) => m.to_string(),
                None => self.name.rsplit('/').next().unwrap_or(self.name).to_string(),
            },
        })
    }

    /// The URL requests go to, and whether it is a server of the API's
    /// rather than the provider's own service: `url`, else `base_url` or
    /// the provider's base URL variable with `path` added, else the
    /// provider's service. Without a provider, `url` or `base_url` is
    /// needed.
    fn url(&self, p: Option<&Provider>, path: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<(String, bool)> {
        let env_base = p.and_then(|p| env(p.base_env)).filter(|b| !b.trim().is_empty()).map(|b| base_url(&b));
        let (url, own) = match (self.text("url")?, self.text("base_url")?) {
            (Some(u), _) => (u, true),
            (None, Some(b)) => (format!("{}{path}", base_url(&b)), true),
            (None, None) => match (env_base, p) {
                (Some(b), _) => (format!("{b}{path}"), true),
                (None, Some(p)) => (format!("{}{path}", p.base), false),
                (None, None) => return Err(self.bad("endpoint.json needs \"url\" (or \"base_url\"): where the server listens".into())),
            },
        };
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(self.bad(format!("{url} is not an http: or https: URL")));
        }
        Ok((url, own))
    }

    /// The secret and where it was looked for: `key_env`, else the
    /// provider's variables. A provider's own service needs its secret; a
    /// server of its API named by URL may take none.
    fn key(&self, p: Option<&Provider>, own_server: bool, env: &dyn Fn(&str) -> Option<String>) -> Result<(Option<String>, String)> {
        let keys = p.map_or(&[][..], |p| p.keys);
        let (key, key_from) = match self.text("key_env")? {
            Some(var) => (env(&var).filter(|k| !k.is_empty()), format!("${var}")),
            None => (keys.iter().find_map(|v| env(v).filter(|k| !k.is_empty())), keys.iter().map(|k| format!("${k}")).collect::<Vec<_>>().join(" or ")),
        };
        if let (None, Some(p), false) = (&key, p, own_server) {
            if !p.keys.is_empty() {
                return Err(self.bad(format!("set {key_from} to the provider's API key (or {} for a server of its API that takes none)", p.base_env)));
            }
        }
        if key.is_none() && self.text("key_env")?.is_some() {
            return Err(self.bad(format!("{key_from} is not set")));
        }
        Ok((key, key_from))
    }
}

/// A base URL as SDKs take it (`OLLAMA_HOST` may leave out the scheme, and
/// name the address it listens on), without a trailing slash.
fn base_url(b: &str) -> String {
    let b = b.trim().trim_end_matches('/');
    let b = if b.contains("://") { b.to_string() } else { format!("http://{b}") };
    b.replace("://0.0.0.0", "://127.0.0.1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn providers_by_prefix() {
        let e = Endpoint::from_config("openai/text-embedding-3-small", None, &env(&[("OPENAI_API_KEY", "sk-1")])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.model.as_str(), e.key.as_deref(), e.native_dim, e.shortens), (Api::OpenAi, "https://api.openai.com/v1/embeddings", "text-embedding-3-small", Some("sk-1"), Some(1536), true));
        let e = Endpoint::from_config("gemini/gemini-embedding-001", None, &env(&[("GOOGLE_API_KEY", "g")])).unwrap();
        assert_eq!((e.url.as_str(), e.key_header.as_str()), ("https://generativelanguage.googleapis.com/v1beta/models/gemini-embedding-001:batchEmbedContents", "x-goog-api-key"));
        let e = Endpoint::from_config("cohere/embed-english-v3.0", None, &env(&[("CO_API_KEY", "c")])).unwrap();
        assert_eq!((e.url.as_str(), e.native_dim, e.shortens), ("https://api.cohere.com/v2/embed", Some(1024), false));
        let e = Endpoint::from_config("mistral/mistral-embed", None, &env(&[("MISTRAL_API_KEY", "m")])).unwrap();
        assert_eq!((e.api, e.url.as_str()), (Api::OpenAi, "https://api.mistral.ai/v1/embeddings"));
        // Ollama takes no key; OLLAMA_HOST may leave out the scheme.
        let e = Endpoint::from_config("ollama/nomic-embed-text:v1.5", None, &env(&[("OLLAMA_HOST", "0.0.0.0:11434")])).unwrap();
        assert_eq!((e.url.as_str(), e.model.as_str(), e.native_dim, e.query_prefix.as_str()), ("http://127.0.0.1:11434/api/embed", "nomic-embed-text:v1.5", Some(768), "search_query: "));
        assert!(Endpoint::from_config("ollama/nomic-embed-text", None, &env(&[])).is_ok());
    }

    #[test]
    fn a_missing_key_says_which() {
        let e = Endpoint::from_config("openai/text-embedding-3-small", None, &env(&[])).unwrap_err().to_string();
        assert!(e.contains("$OPENAI_API_KEY") && e.contains("OPENAI_BASE_URL"), "{e}");
        let e = Endpoint::from_config("cohere/embed-v4.0", None, &env(&[])).unwrap_err().to_string();
        assert!(e.contains("$COHERE_API_KEY or $CO_API_KEY"), "{e}");
        // A server of the API named by URL may take no key.
        let e = Endpoint::from_config("openai/bge-m3", None, &env(&[("OPENAI_BASE_URL", "http://gpu:8000/v1/")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_deref(), e.native_dim), ("http://gpu:8000/v1/embeddings", None, None));
        // A key_env that is not set is an error, whatever the URL.
        let c = json!({"api": "openai", "url": "http://gpu/v1/embeddings", "key_env": "GPU_KEY"});
        assert!(Endpoint::from_config("gpu", Some(&c), &env(&[])).unwrap_err().to_string().contains("$GPU_KEY is not set"));
    }

    #[test]
    fn endpoint_files() {
        let c = json!({"api": "openai", "base_url": "https://res.openai.azure.com/openai/deployments/emb", "model": "text-embedding-3-small", "key_env": "AZURE_KEY", "key_header": "api-key", "headers": {"x-team": "search"}, "batch": 16, "query_prefix": "q: "});
        let e = Endpoint::from_config("azure-small", Some(&c), &env(&[("AZURE_KEY", "az")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_deref(), e.key_header.as_str(), e.batch, e.query_prefix.as_str()), ("https://res.openai.azure.com/openai/deployments/emb/embeddings", Some("az"), "api-key", 16, "q: "));
        assert_eq!(e.headers, [("x-team".to_string(), "search".to_string())]);
        // Strata knows the model by the provider's name.
        assert_eq!((e.native_dim, e.shortens), (Some(1536), true));
        let c = json!({"url": "http://x/embed", "model": "m"});
        assert!(Endpoint::from_config("custom", Some(&c), &env(&[])).unwrap_err().to_string().contains("needs \"api\""));
        let c = json!({"api": "openai", "url": "http://x/embed", "batchsize": 4});
        assert!(Endpoint::from_config("custom", Some(&c), &env(&[])).unwrap_err().to_string().contains("unknown key \"batchsize\""));
        // The secret stays out of what is printed.
        let c = json!({"api": "openai", "url": "http://x/embed", "key_env": "K", "dim": 8});
        let e = Endpoint::from_config("custom", Some(&c), &env(&[("K", "sekrit")])).unwrap();
        assert!(!format!("{e:?}").contains("sekrit"));
    }

    #[test]
    fn rerankers_by_prefix_and_by_file() {
        let e = RerankEndpoint::from_config("cohere/rerank-v3.5", None, &env(&[("CO_API_KEY", "c")])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.model.as_str(), e.key.as_deref(), e.batch), (RerankApi::Cohere, "https://api.cohere.com/v2/rerank", "rerank-v3.5", Some("c"), 1000));
        let e = RerankEndpoint::from_config("voyage/rerank-2.5", None, &env(&[("VOYAGE_API_KEY", "v")])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.timeout, e.retries), (RerankApi::Voyage, "https://api.voyageai.com/v1/rerank", Duration::from_secs(10), 2));
        let e = RerankEndpoint::from_config("jina/jina-reranker-v2-base-multilingual", None, &env(&[("JINA_API_KEY", "j")])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.model.as_str()), (RerankApi::Jina, "https://api.jina.ai/v1/rerank", "jina-reranker-v2-base-multilingual"));
        assert!(is_reranker_name("cohere/rerank-v3.5") && is_reranker_name("Voyage/rerank-2"));
        assert!(!is_reranker_name("openai/text-embedding-3-small") && !is_reranker_name("ms-marco-MiniLM-L-6-v2") && !is_reranker_name("cohere/"));
        let e = RerankEndpoint::from_config("voyage/rerank-2.5", None, &env(&[])).unwrap_err().to_string();
        assert!(e.contains("$VOYAGE_API_KEY") && e.contains("VOYAGE_BASE_URL"), "{e}");
        // A provider's base URL variable names a server of its API.
        let e = RerankEndpoint::from_config("cohere/rerank-v3.5", None, &env(&[("COHERE_BASE_URL", "http://proxy:4000/")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_deref()), ("http://proxy:4000/v2/rerank", None));
    }

    #[test]
    fn reranking_servers_of_ones_own() {
        // TEI on a GPU: no provider's key goes to it, and requests carry
        // TEI's default batch.
        let c = json!({"api": "tei", "url": "http://gpu:8080/rerank"});
        let e = RerankEndpoint::from_config("bge-reranker-v2-m3", Some(&c), &env(&[("COHERE_API_KEY", "c")])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.key.as_deref(), e.batch, e.model.as_str()), (RerankApi::Tei, "http://gpu:8080/rerank", None, 32, "bge-reranker-v2-m3"));
        let c = json!({"api": "tei", "base_url": "http://gpu:8080/", "key_env": "TEI_KEY", "batch": 64, "timeout_s": 3});
        let e = RerankEndpoint::from_config("team/bge", Some(&c), &env(&[("TEI_KEY", "t")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_deref(), e.batch, e.timeout, e.model.as_str()), ("http://gpu:8080/rerank", Some("t"), 64, Duration::from_secs(3), "bge"));
        assert!(!format!("{e:?}").contains("\"t\""));
        let c = json!({"api": "tei"});
        assert!(RerankEndpoint::from_config("bge", Some(&c), &env(&[])).unwrap_err().to_string().contains("needs \"url\""));
        // vLLM, Infinity and LiteLLM answer in Cohere's shape at /rerank.
        let c = json!({"api": "vllm", "base_url": "http://gpu:8000", "model": "BAAI/bge-reranker-v2-m3"});
        let e = RerankEndpoint::from_config("bge-m3", Some(&c), &env(&[])).unwrap();
        assert_eq!((e.api, e.url.as_str(), e.model.as_str(), e.key), (RerankApi::Cohere, "http://gpu:8000/rerank", "BAAI/bge-reranker-v2-m3", None));
        // Cohere's API by name takes its defaults, and its key.
        let c = json!({"api": "cohere", "model": "rerank-v3.5"});
        let e = RerankEndpoint::from_config("ours", Some(&c), &env(&[("COHERE_API_KEY", "c")])).unwrap();
        assert_eq!((e.url.as_str(), e.key.as_deref()), ("https://api.cohere.com/v2/rerank", Some("c")));
        let c = json!({"url": "http://gpu/rerank"});
        assert!(RerankEndpoint::from_config("ours", Some(&c), &env(&[])).unwrap_err().to_string().contains("needs \"api\""));
        let c = json!({"api": "openai", "url": "http://gpu/rerank"});
        assert!(RerankEndpoint::from_config("ours", Some(&c), &env(&[])).unwrap_err().to_string().contains("is one of cohere"));
        let c = json!({"api": "tei", "url": "http://gpu/rerank", "dim": 3});
        assert!(RerankEndpoint::from_config("ours", Some(&c), &env(&[])).unwrap_err().to_string().contains("unknown key \"dim\""));
    }

}
