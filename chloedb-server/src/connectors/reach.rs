//! Which addresses a connector may reach (audit 4, X-N4), and where its
//! credentials may go.
//!
//! A connector asks from inside the database's network: beside a cluster,
//! next to instance metadata, the Kubernetes API, the pod network and the
//! server's own ports. So by default a connector reaches the public
//! internet only. An address on this machine, a private network, or
//! another reserved range is refused, checked on each URL before it is
//! asked and at connect time on every address a name resolves to
//! ([`Resolver`]), so a name that turns private between the check and the
//! connection is refused too. A connector whose system is inside the
//! network says so in its config, `"allow_internal": true`; even then the
//! link-local ranges (cloud metadata at 169.254.169.254) stay refused,
//! since no system a connector reads lives there.
//!
//! Its credentials (basic or bearer auth, and its `headers`, where
//! `{secret:NAME}` is expanded) go to its configured origin only
//! ([`same_origin`]): a redirect is followed only within that origin (and
//! at most [`MAX_REDIRECTS`] times), and a next page on another host, read
//! from the upstream's answer, is asked without them.
//!
//! The hosts of the HTTP(S)_PROXY settings are reachable: a proxy is the
//! operator's, and it resolves the names it is asked for.
//!
//! The ranges are those of the crawler's guard (`chloedb-crawl/src/reach.rs`,
//! audit 1's G-B3); the server does not depend on the crawler, so they are
//! written out here too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::Url;

/// The most redirects a connector's request follows (within its origin).
pub const MAX_REDIRECTS: usize = 5;

/// The words a refusal starts with, so that a resolver's refusal is told
/// apart from other failures to connect.
pub const REFUSED: &str = "not an address a connector may reach";

/// What a connector's requests may reach.
#[derive(Clone, Debug, Default)]
pub struct Reach {
    /// Private networks and this machine too (`allow_internal`); never
    /// link-local ones.
    pub internal: bool,
    /// Hosts reachable whatever their addresses: the proxies', with their
    /// ports.
    pub hosts: Vec<(String, Option<u16>)>,
}

impl Reach {
    /// The reach of a connector that does (or does not) allow internal
    /// addresses, with the proxies' hosts.
    pub fn new(internal: bool) -> Reach {
        Reach { internal, hosts: Vec::new() }.with_proxies()
    }

    /// The proxies' hosts (HTTP_PROXY, HTTPS_PROXY, ALL_PROXY, either case).
    fn with_proxies(mut self) -> Reach {
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
            let Ok(v) = std::env::var(k) else { continue };
            let v = if v.contains("://") { v } else { format!("http://{v}") };
            if let Some((h, p)) = Url::parse(&v).ok().and_then(|u| Some((u.host_str()?.trim_matches(['[', ']']).to_ascii_lowercase(), u.port_or_known_default()))) {
                self.hosts.push((h, p));
            }
        }
        self
    }

    fn listed(&self, host: &str, port: Option<u16>) -> bool {
        let host = host.trim_matches(['[', ']']).trim_end_matches('.');
        self.hosts.iter().any(|(h, p)| h.eq_ignore_ascii_case(host) && (p.is_none() || *p == port))
    }

    fn refuses(&self, ip: IpAddr) -> bool {
        link_local(ip) || (!self.internal && private(ip))
    }

    fn hint(&self, ip: IpAddr) -> &'static str {
        if link_local(ip) {
            "link-local addresses (cloud metadata) are never reached"
        } else {
            "set \"allow_internal\": true in the connector's config to reach a system inside this network"
        }
    }

    /// Whether a request may go to `url`: http or https, and an address
    /// this reach allows. A name is checked again when it resolves.
    pub fn check(&self, url: &str) -> Result<(), String> {
        let u = Url::parse(url).map_err(|e| format!("{}: not a URL ({e})", strip(url)))?;
        if u.scheme() != "http" && u.scheme() != "https" {
            return Err(format!("{}: a connector asks http and https addresses only", u.scheme()));
        }
        let host = u.host_str().ok_or_else(|| format!("{}: no host", strip(url)))?;
        if self.listed(host, u.port_or_known_default()) {
            return Ok(());
        }
        let bare = host.trim_matches(['[', ']']);
        match bare.parse::<IpAddr>() {
            Ok(ip) if self.refuses(ip) => Err(format!("{ip} is {REFUSED}: {}", self.hint(ip))),
            Ok(_) => Ok(()),
            Err(_) => {
                let d = bare.trim_end_matches('.').to_ascii_lowercase();
                if !self.internal && (d == "localhost" || d.ends_with(".localhost")) {
                    return Err(format!("{d} is {REFUSED}: {}", self.hint(IpAddr::V4(Ipv4Addr::LOCALHOST))));
                }
                Ok(())
            }
        }
    }

    /// The addresses of `name` a connection may use.
    fn addresses(&self, name: &str, addrs: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
        if self.hosts.iter().any(|(h, _)| h.eq_ignore_ascii_case(name.trim_end_matches('.'))) {
            return Ok(addrs);
        }
        let ok: Vec<SocketAddr> = addrs.iter().copied().filter(|a| !self.refuses(a.ip())).collect();
        if ok.is_empty() && !addrs.is_empty() {
            let ip = addrs[0].ip();
            return Err(format!("{name} resolves to {ip}, {REFUSED}: {}", self.hint(ip)));
        }
        Ok(ok)
    }

    /// A client that reaches what this reach allows: its resolver refuses
    /// the other addresses, and it follows redirects within the origin it
    /// was asked only.
    pub fn client(self) -> reqwest::Client {
        let me = Arc::new(self);
        let for_redirects = me.clone();
        let policy = reqwest::redirect::Policy::custom(move |a| {
            if a.previous().len() > MAX_REDIRECTS {
                return a.error(format!("more than {MAX_REDIRECTS} redirects"));
            }
            let first = a.previous().first().map(Url::as_str).unwrap_or_default().to_string();
            if !same_origin(&first, a.url().as_str()) {
                // Answered as it came: the run says where it led.
                return a.stop();
            }
            match for_redirects.check(a.url().as_str()) {
                Ok(()) => a.follow(),
                Err(e) => a.error(e),
            }
        });
        crate::http_client()
            .connect_timeout(std::time::Duration::from_secs(10))
            .user_agent(concat!("chloedb-connectors/", env!("CARGO_PKG_VERSION")))
            .redirect(policy)
            .dns_resolver(Arc::new(Resolver(me)))
            .build()
            .unwrap_or_default()
    }
}

/// A URL without its query, for messages.
fn strip(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Whether `a` and `b` have one origin: scheme, host and port.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(a), Ok(b)) => a.scheme() == b.scheme() && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase) && a.port_or_known_default() == b.port_or_known_default(),
        _ => false,
    }
}

/// Whether an address is link local (cloud metadata lives there).
fn link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [x, y, ..] = a.octets();
            x == 169 && y == 254
        }
        IpAddr::V6(a) => {
            let w = a.segments();
            if let Some(v4) = a.to_ipv4_mapped() {
                return link_local(IpAddr::V4(v4));
            }
            (w[0] & 0xffc0) == 0xfe80 || (w[0] == 0xfd00 && w[1] == 0x0ec2) // fe80::/10, AWS's fd00:ec2::254
        }
    }
}

/// Whether an address is on this machine, a private network or another
/// range that is not the public internet.
pub fn private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => private_v4(a),
        IpAddr::V6(a) => private_v6(a),
    }
}

fn private_v4(a: Ipv4Addr) -> bool {
    let [x, y, z, _] = a.octets();
    x == 0 // "this network"
        || x == 10
        || (x == 100 && (64..128).contains(&y)) // carrier-grade NAT
        || x == 127
        || (x == 169 && y == 254) // link local, cloud metadata
        || (x == 172 && (16..32).contains(&y))
        || (x == 192 && y == 0 && z == 0) // IETF protocol assignments
        || (x == 192 && y == 0 && z == 2) // documentation
        || (x == 192 && y == 88 && z == 99) // 6to4 relays
        || (x == 192 && y == 168)
        || (x == 198 && (18..20).contains(&y)) // benchmarking
        || (x == 198 && y == 51 && z == 100) // documentation
        || (x == 203 && y == 0 && z == 113) // documentation
        || x >= 224 // multicast, reserved, broadcast
}

fn private_v6(a: Ipv6Addr) -> bool {
    let w = a.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if w[..7].iter().all(|&x| x == 0) {
        return true; // :: and ::1
    }
    if w[..5].iter().all(|&x| x == 0) && w[5] == 0xffff {
        return private_v4(v4(w[6], w[7])); // ::ffff:a.b.c.d
    }
    if w[..6].iter().all(|&x| x == 0) {
        return true; // ::a.b.c.d, deprecated
    }
    if w[0] == 0x64 && w[1] == 0xff9b {
        return private_v4(v4(w[6], w[7])); // NAT64
    }
    if w[0] == 0x2002 {
        return private_v4(v4(w[1], w[2])); // 6to4
    }
    (w[0] & 0xfe00) == 0xfc00 // unique local
        || (w[0] & 0xffc0) == 0xfe80 // link local
        || (w[0] & 0xff00) == 0xff00 // multicast
        || (w[0] == 0x2001 && w[1] == 0x0db8) // documentation
        || (w[0] == 0x0100 && w[1] == 0 && w[2] == 0 && w[3] == 0) // discard
}

/// The client's resolver: the system's, and then only the addresses the
/// reach allows.
struct Resolver(Arc<Reach>);

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let reach = self.0.clone();
        let name = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            let ok = reach.addresses(&name, addrs).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            Ok(Box::new(ok.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_metadata_addresses_are_refused() {
        let public = Reach::default();
        for u in [
            "http://127.0.0.1/",
            "http://localhost:7700/v1/tables",
            "http://api.localhost/",
            "http://10.1.2.3/",
            "http://172.20.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://2130706433/", // 127.0.0.1 as one number
            "http://0x7f.1/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[2002:a9fe:a9fe::]/",
        ] {
            assert!(public.check(u).is_err(), "{u}");
        }
        for u in ["https://example.com/", "http://93.184.215.14/", "http://[2606:4700::1111]/"] {
            assert!(public.check(u).is_ok(), "{u}");
        }
        assert!(public.check("file:///etc/passwd").is_err());
        assert!(public.check("ftp://example.com/").is_err());

        // Inside the network when it says so, but never metadata.
        let inside = Reach { internal: true, hosts: Vec::new() };
        for u in ["http://127.0.0.1:9090/", "http://prometheus:9090/", "http://10.0.0.5/", "http://localhost/"] {
            assert!(inside.check(u).is_ok(), "{u}");
        }
        for u in ["http://169.254.169.254/latest/meta-data/", "http://[fe80::1]/", "http://[::ffff:169.254.169.254]/", "http://[fd00:ec2::254]/"] {
            let e = inside.check(u).unwrap_err();
            assert!(e.contains("never"), "{u}: {e}");
        }
        assert!(inside.addresses("metadata.google.internal", vec!["169.254.169.254:0".parse().unwrap()]).is_err());
        assert!(public.addresses("intranet.example", vec!["10.0.0.5:0".parse().unwrap()]).is_err());
        assert_eq!(public.addresses("mixed.example", vec!["10.0.0.5:0".parse().unwrap(), "93.184.215.14:0".parse().unwrap()]).unwrap(), vec!["93.184.215.14:0".parse().unwrap()]);
    }

    #[test]
    fn origins() {
        assert!(same_origin("https://x.atlassian.net/rest/api/3/search", "https://X.atlassian.net:443/other?a=1"));
        assert!(!same_origin("https://x.atlassian.net/", "http://x.atlassian.net/"));
        assert!(!same_origin("https://x.atlassian.net/", "https://x.atlassian.net:8443/"));
        assert!(!same_origin("https://x.atlassian.net/", "https://attacker.example/"));
        assert!(!same_origin("not a url", "not a url"));
    }
}
