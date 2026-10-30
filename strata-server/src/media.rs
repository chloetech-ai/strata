//! Fetching the files MEDIA fields name over HTTP(S) ([`strata::media`]):
//! the [`strata::media::Fetcher`] a server gives its database with
//! `--media-fetch`.
//!
//! A fetch has a timeout (`--media-timeout-s`), reads at most
//! `--media-max-bytes`, and follows at most [`MAX_REDIRECTS`] redirects,
//! each to a URL the allowlist has: a redirect off the list fails the
//! fetch, so a listed host cannot send the server to another one.

use std::time::Duration;

use strata::media::{Allowlist, FetchError, Fetcher};

/// Redirects a fetch follows.
pub const MAX_REDIRECTS: usize = 5;

/// The default of `--media-timeout-s`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct HttpFetcher {
    client: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

impl HttpFetcher {
    /// A fetcher of the URLs `allow` has, running on the current Tokio
    /// runtime.
    pub fn new(allow: Allowlist, timeout: Duration) -> Result<HttpFetcher, String> {
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error(format!("more than {MAX_REDIRECTS} redirects"))
            } else if allow.allows_http(attempt.url().as_str()) {
                attempt.follow()
            } else {
                let to = attempt.url().to_string();
                attempt.error(format!("a redirect to {to}, which is not on the media fetch list"))
            }
        });
        let client = reqwest::Client::builder().redirect(policy).timeout(timeout).build().map_err(|e| format!("the media fetch client: {e}"))?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| format!("the media fetch client needs a Tokio runtime: {e}"))?;
        Ok(HttpFetcher { client, runtime })
    }
}

impl Fetcher for HttpFetcher {
    /// Called by a write before the writer, on a thread that may block
    /// (never a runtime worker): the fetch runs on the runtime while it
    /// waits.
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (client, url) = (self.client.clone(), url.to_string());
        self.runtime.spawn(async move {
            let _ = tx.send(fetch(&client, &url, max_bytes).await);
        });
        rx.recv().unwrap_or_else(|_| Err(FetchError::Failed("the fetch stopped (the server is shutting down)".into())))
    }
}

async fn fetch(client: &reqwest::Client, url: &str, max: u64) -> Result<Vec<u8>, FetchError> {
    let mut r = client.get(url).send().await.map_err(failed)?;
    if !r.status().is_success() {
        return Err(FetchError::Failed(format!("GET returned {}", r.status())));
    }
    let too_big = || FetchError::Failed(format!("larger than the media size cap of {max} bytes (--media-max-bytes)"));
    if r.content_length().is_some_and(|n| n > max) {
        return Err(too_big());
    }
    let mut out = Vec::new();
    while let Some(c) = r.chunk().await.map_err(failed)? {
        if (out.len() + c.len()) as u64 > max {
            return Err(too_big());
        }
        out.extend_from_slice(&c);
    }
    Ok(out)
}

/// A reqwest error with its causes (a redirect refused says why).
fn failed(e: reqwest::Error) -> FetchError {
    let mut m = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        m.push_str(&format!(": {s}"));
        src = s.source();
    }
    if e.is_timeout() {
        FetchError::Timeout(format!("the fetch took longer than --media-timeout-s ({m})"))
    } else {
        FetchError::Failed(m)
    }
}
