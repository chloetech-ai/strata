//! AWS Signature Version 4: the signature every AWS API call carries
//! (<https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>).
//!
//! HMAC-SHA256 over a canonical form of the request, with a key derived
//! from the secret, the day, the region and the service. Checked against
//! AWS's own test suite (`get-vanilla` and others) below.

use sha2::{Digest, Sha256};

/// HMAC-SHA256 (RFC 2104).
pub fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let (mut inner, mut outer) = (Sha256::new(), Sha256::new());
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

/// URI encoding as SigV4 wants it: every byte but `A-Z a-z 0-9 - _ . ~`
/// as `%XY` (upper case); `/` kept when `path`.
pub fn encode(s: &str, path: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if path => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `k=v&k=v` of `params`, encoded and sorted: a canonical query string,
/// and a form body.
pub fn query(params: &[(&str, String)]) -> String {
    let mut p: Vec<(String, String)> = params.iter().map(|(k, v)| (encode(k, false), encode(v, false))).collect();
    p.sort();
    p.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// What a request is signed for.
pub struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// The path, already encoded.
    pub path: &'a str,
    /// The canonical query string ([`query`]).
    pub query: &'a str,
    /// Headers besides `host` and `x-amz-date`, names in lower case.
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

/// The key a day's signatures in one region and service are made with.
fn signing_key(secret: &str, day: &str, region: &str, service: &str) -> [u8; 32] {
    let k = hmac(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, service.as_bytes());
    hmac(&k, b"aws4_request")
}

/// The headers that sign `r` at `amz_date` (`YYYYMMDDTHHMMSSZ`):
/// `x-amz-date`, `x-amz-security-token` when there is a session token,
/// and `authorization`.
pub fn sign(r: &Request, key_id: &str, secret: &str, token: Option<&str>, region: &str, service: &str, amz_date: &str) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = r.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string())).collect();
    h.push(("host".into(), r.host.into()));
    h.push(("x-amz-date".into(), amz_date.into()));
    if let Some(t) = token {
        h.push(("x-amz-security-token".into(), t.into()));
    }
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let creq = format!("{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}", r.method, r.path, r.query, sha256_hex(r.body));
    let day = &amz_date[..8];
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(creq.as_bytes()));
    let sig = hex(&hmac(&signing_key(secret, day, region, service), to_sign.as_bytes()));
    let mut out = vec![("x-amz-date".to_string(), amz_date.to_string())];
    if let Some(t) = token {
        out.push(("x-amz-security-token".into(), t.into()));
    }
    out.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed}, Signature={sig}")));
    out
}

/// `YYYYMMDDTHHMMSSZ` of a time in milliseconds since 1970 (UTC).
pub fn amz_date(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil(days as i64);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The calendar date of a day since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS's SigV4 test suite: its example credentials, date and host.
    const KEY: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const DATE: &str = "20150830T123600Z";

    fn signature(r: &Request, token: Option<&str>) -> String {
        let h = sign(r, KEY, SECRET, token, "us-east-1", "service", DATE);
        let a = &h.iter().find(|(k, _)| k == "authorization").unwrap().1;
        a.rsplit("Signature=").next().unwrap().to_string()
    }

    #[test]
    fn signatures_are_the_ones_aws_test_suite_gives() {
        fn get(query: &str) -> Request<'_> {
            Request { method: "GET", host: "example.amazonaws.com", path: "/", query, headers: &[], body: b"" }
        }
        // get-vanilla
        assert_eq!(signature(&get(""), None), "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        // get-vanilla-query-order-key (sorted by `query`)
        let q = query(&[("Param2", "value2".into()), ("Param1", "value1".into())]);
        assert_eq!(q, "Param1=value1&Param2=value2");
        assert_eq!(signature(&get(&q), None), "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500");
        // post-x-www-form-urlencoded
        let post = Request { method: "POST", host: "example.amazonaws.com", path: "/", query: "", headers: &[("Content-Type", "application/x-www-form-urlencoded")], body: b"Param1=value1" };
        assert_eq!(signature(&post, None), "ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a");
        // A session token is a signed header.
        let token = "AQoDYXdzEPT//////////wEXAMPLEtc764bNrC9SAPBSM22wDOk4x4HIZ8j4FZTwdQWLWsKWHGBuFqwAeMicRXmxfpSPfIeoIYRqTflfKD8YUuwthAx7mSEI/qkPpKPi/kMcGdQrmGdeehM4IC1NtBmUpp2wUE8phUZampKsburEDy0KPkyQDYwT7WZ0wq5VSXDvp75YU9HFvlRd8Tx6q6fE8YQcHNVXAkiY9q6d+xo0rKwT38xVqr7ZD0u0iPPkUL64lIZbqBAz+scqKmlzm8FDrypNC9Yjc8fPOLn9FX9KSYvKTr4rvx3iSIlTJabIQwj2ICCR/oLxBA==";
        assert_eq!(signature(&get(""), Some(token)), "c8db8b9676d526f735dac5330f17623554c6cad1e2980d321903e9a3884c051b");
        let h = sign(&get(""), KEY, SECRET, Some(token), "us-east-1", "service", DATE);
        assert!(h.iter().any(|(k, v)| k == "x-amz-security-token" && v == token));
        assert!(h[2].1.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token, "), "{h:?}");
    }

    #[test]
    fn hmac_and_dates_are_right() {
        // RFC 4231, test case 2.
        assert_eq!(hex(&hmac(b"Jefe", b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // A key longer than the block is hashed first (RFC 4231, case 6).
        assert_eq!(hex(&hmac(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
        assert_eq!(amz_date(1_440_938_160_000), DATE);
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_709_164_800_000), "20240229T000000Z");
        assert_eq!(encode("a b/c~*", false), "a%20b%2Fc~%2A");
        assert_eq!(encode("/clusters/my cluster", true), "/clusters/my%20cluster");
    }
}
