//! Who may call the server. A bearer token guards the HTTP API and every
//! gRPC call, compared in constant time; a server refuses to listen beyond
//! loopback without one. A browser on another site is kept out by the Host
//! check (a DNS-rebinding page names its own host) and the Origin check (a
//! page's cross-site POST names its origin), and an import must say what
//! it sends with a content type a page cannot send without asking first.
//! Both listeners serve TLS with a certificate and key.

use std::net::IpAddr;
use std::sync::Arc;

/// How the server is reached and what it asks of callers.
#[derive(Clone, Default)]
pub struct Access {
    /// The token every API route and every gRPC call needs, as
    /// `Authorization: Bearer <token>`.
    pub token: Option<Secret>,
    /// The host names the HTTP server answers to (`None`: any).
    pub hosts: Option<Vec<String>>,
    /// A certificate chain and its key, as PEM, for both listeners.
    pub tls: Option<Tls>,
}

#[derive(Clone)]
pub struct Tls {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

/// A token, kept out of `Debug` output and logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub Arc<str>);

impl Secret {
    pub fn new(s: impl Into<Arc<str>>) -> Secret {
        Secret(s.into())
    }

    /// Whether a request's `Authorization` header value carries this token.
    pub fn admits(&self, authorization: Option<&str>) -> bool {
        let given = authorization.and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        same_secret(given.as_bytes(), self.0.as_bytes())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Equal byte strings, in a time that depends on their lengths but not on
/// where they differ, so that the answer's timing does not give a token
/// away byte by byte.
pub fn same_secret(given: &[u8], want: &[u8]) -> bool {
    let mut diff = (given.len() != want.len()) as u8;
    for (i, w) in want.iter().enumerate() {
        diff |= w ^ given.get(i).copied().unwrap_or(!w);
    }
    std::hint::black_box(diff) == 0
}

/// The names of the loopback addresses, which a server on loopback
/// answers to.
pub const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The host names an HTTP server bound to `ip` answers to: on loopback,
/// the loopback names and `extra`; elsewhere `extra` and the loopback
/// names if `extra` names any, else every name (the token guards it).
pub fn allowed_hosts(ip: IpAddr, extra: &[String]) -> Option<Vec<String>> {
    if !ip.is_loopback() && extra.is_empty() {
        return None;
    }
    let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| h.to_string()).collect();
    hosts.extend(extra.iter().map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase()));
    Some(hosts)
}

/// The host of a `Host` header or of an origin's authority, without its
/// port, brackets or trailing dot, lowercased.
pub fn host_name(authority: &str) -> String {
    let a = authority.trim();
    let h = match a.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => match a.rsplit_once(':') {
            Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => a,
        },
    };
    h.trim_end_matches('.').to_ascii_lowercase()
}

/// Why a request from a browser is refused, if it is: a `Host` the server
/// does not answer to, or, for a request that can change something, an
/// `Origin` other than the server's own.
pub fn browser_check(hosts: Option<&[String]>, host: Option<&str>, origin: Option<&str>, safe_method: bool) -> Option<String> {
    if let Some(hosts) = hosts {
        let name = host_name(host.unwrap_or(""));
        if !hosts.iter().any(|h| *h == name) {
            return Some(format!("this server does not answer to the host {name:?} (start it with --allow-host {name} to allow it)"));
        }
    }
    if let (false, Some(origin)) = (safe_method, origin) {
        let authority = origin.split_once("://").map_or("", |(_, a)| a);
        if !host.is_some_and(|h| h.trim().eq_ignore_ascii_case(authority)) {
            return Some(format!("a page from {origin} cannot change this server"));
        }
    }
    None
}

/// Whether a content type is one a page may send to another site without
/// asking first (a CORS "simple" request), or no content type at all.
pub fn simple_content_type(ct: Option<&str>) -> bool {
    let ct = ct.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(ct.as_str(), "" | "text/plain" | "application/x-www-form-urlencoded" | "multipart/form-data")
}

/// A server configuration for TLS from a PEM certificate chain and key.
pub fn server_config(tls: &Tls) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs = rustls_pemfile::certs(&mut &tls.cert[..]).collect::<Result<Vec<_>, _>>().map_err(|e| format!("the TLS certificate: {e}"))?;
    if certs.is_empty() {
        return Err("the TLS certificate file holds no certificate".into());
    }
    let key = rustls_pemfile::private_key(&mut &tls.key[..]).map_err(|e| format!("the TLS key: {e}"))?.ok_or("the TLS key file holds no private key")?;
    let mut c = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("the TLS certificate and key: {e}"))?;
    c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret(b"s3cret", b"s3cret"));
        assert!(!same_secret(b"s3cre", b"s3cret"));
        assert!(!same_secret(b"s3crets", b"s3cret"));
        assert!(!same_secret(b"", b"s3cret"));
        assert!(!same_secret(b"S3cret", b"s3cret"));
        let t = Secret::new("tok");
        assert!(t.admits(Some("Bearer tok")));
        assert!(!t.admits(Some("Bearer to")) && !t.admits(Some("tok")) && !t.admits(None));
        assert_eq!(format!("{t:?}"), "Secret(..)");
    }

    #[test]
    fn hosts_and_origins() {
        assert_eq!(host_name("LocalHost:7700"), "localhost");
        assert_eq!(host_name("[::1]:7700"), "::1");
        assert_eq!(host_name("example.com."), "example.com");
        let loop_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let hosts = allowed_hosts(loop_ip, &["db.internal".into()]).unwrap();
        let check = |host: &str, origin: Option<&str>, safe| browser_check(Some(&hosts), Some(host), origin, safe);
        assert!(check("127.0.0.1:7700", None, false).is_none());
        assert!(check("db.internal:7700", None, true).is_none());
        // DNS rebinding: the page's own name reaches the loopback address.
        assert!(check("evil.example:7700", None, true).is_some());
        // The UI posts with its own origin; another site's page cannot.
        assert!(check("localhost:7700", Some("http://localhost:7700"), false).is_none());
        assert!(check("localhost:7700", Some("https://evil.example"), false).is_some());
        assert!(check("localhost:7700", Some("null"), false).is_some());
        assert!(check("localhost:7700", Some("https://evil.example"), true).is_none());
        assert!(allowed_hosts("10.0.0.5".parse().unwrap(), &[]).is_none());
        assert!(simple_content_type(Some("text/plain; charset=utf-8")) && simple_content_type(None));
        assert!(!simple_content_type(Some("text/csv")));
    }
}
