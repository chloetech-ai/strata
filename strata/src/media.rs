//! MEDIA fields: references to images, sounds, videos and PDFs, never
//! their bytes.
//!
//! A MEDIA field ([`crate::schema::Field::media`]) holds a JSON object,
//! [`Media`]: `{uri, sha256, mime, bytes, width, height, duration_ms,
//! pages}`. It is stored, and indexed as a keyword on its `sha256`, so
//! that `WHERE image = '<sha256>'` finds every row of one file in one
//! lookup. Bytes pass through a write to be hashed and described, and are
//! never kept in the database: merges rewrite every row about log n
//! times.
//!
//! A write gives a MEDIA value in one of four forms, which
//! [`Resolver::resolve`] turns into the stored one before the writer
//! (so replicas, which apply what their primary logged, never fetch):
//!
//! | a write gives | Strata | stores |
//! |---|---|---|
//! | `uri`, `sha256`, `mime` and `bytes` (and what else it knows) | keeps it as given | the value |
//! | `{"uri": .., "data": "<base64>"}` | hashes and describes the bytes, fetches nothing | uri, hash, metadata |
//! | `{"uri": ..}`, or the URI alone as text | fetches it, if the [`Allowlist`] has it | uri, hash, metadata |
//! | `{"data": ..}` alone | keeps the bytes in the media store, `DIR/<sha256>`, if there is one | `media:<sha256>`, hash, metadata |
//!
//! The allowlist names `file:` roots (a path inside one, after symbolic
//! links and `..` are resolved) and `https:` hosts (`http:` only when
//! spelled out); fetches over HTTP are a [`Fetcher`]'s, which a server
//! gives the resolver with its timeout, and follow no redirect off the
//! list. Every source is capped at [`Resolver::max_bytes`].
//!
//! [`describe`] reads the type of a file from its first bytes, and what
//! its headers say cheaply: the size of PNG, JPEG, GIF and WebP images,
//! the duration of WAV, FLAC and MP4 files, the pages of a PDF.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::error::{Error, Result};

/// What a MEDIA field holds (SQL `MEDIA OPTIONS(kind = 'image')`): a
/// value's `mime` must be of this kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// Any file.
    #[default]
    Any,
    /// `image/*`.
    Image,
    /// `audio/*`.
    Audio,
    /// `video/*`.
    Video,
    /// `application/pdf`.
    Pdf,
}

impl MediaKind {
    pub const ALL: [MediaKind; 5] = [MediaKind::Any, MediaKind::Image, MediaKind::Audio, MediaKind::Video, MediaKind::Pdf];

    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Any => "any",
            MediaKind::Image => "image",
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
            MediaKind::Pdf => "pdf",
        }
    }

    pub fn parse(s: &str) -> Option<MediaKind> {
        MediaKind::ALL.into_iter().find(|k| k.as_str().eq_ignore_ascii_case(s))
    }

    /// Whether a file of type `mime` is of this kind.
    pub fn accepts(self, mime: &str) -> bool {
        let mime = mime.to_ascii_lowercase();
        match self {
            MediaKind::Any => true,
            MediaKind::Image => mime.starts_with("image/"),
            MediaKind::Audio => mime.starts_with("audio/"),
            MediaKind::Video => mime.starts_with("video/"),
            MediaKind::Pdf => mime == "application/pdf",
        }
    }
}

/// A MEDIA field's value as it is stored: where the file is, its SHA-256
/// in lowercase hex, its type and size, and what its headers say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Media {
    pub uri: String,
    pub sha256: String,
    pub mime: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
}

/// The keys of a MEDIA value, as a write may give them (`data` is never
/// stored).
pub const KEYS: [&str; 9] = ["uri", "data", "sha256", "mime", "bytes", "width", "height", "duration_ms", "pages"];

impl Media {
    /// A stored value from its JSON text, checked ([`Media::check`]).
    pub fn parse(text: &str) -> std::result::Result<Media, String> {
        let m: Media = serde_json::from_str(text).map_err(|e| format!("a MEDIA value is a JSON object of {}: {e}", KEYS.join(", ")))?;
        m.check()?;
        Ok(m)
    }

    /// The value's JSON text, keys in [`KEYS`] order and absent ones left
    /// out: what a MEDIA field stores.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("a Media serializes")
    }

    fn check(&self) -> std::result::Result<(), String> {
        if self.uri.is_empty() {
            return Err("a MEDIA value needs its uri".into());
        }
        if !is_sha256(&self.sha256) {
            return Err(format!("sha256 is 64 lowercase hex digits, not {:?}", self.sha256));
        }
        if self.mime.is_empty() || !self.mime.contains('/') {
            return Err(format!("mime is a media type such as image/jpeg, not {:?}", self.mime));
        }
        Ok(())
    }
}

/// Whether `s` is a SHA-256 in lowercase hex.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The keyword a stored MEDIA value is indexed under: its `sha256`.
pub fn hash_of(text: &str) -> Option<String> {
    let j: Json = serde_json::from_str(text).ok()?;
    j.get("sha256")?.as_str().map(String::from)
}

/// The JSON text of what a write gives a MEDIA field (see the module's
/// table): an object of [`KEYS`], or a string holding one or a URI alone.
/// It is resolved before it is stored ([`Resolver::resolve`]).
pub fn input_text(j: &Json) -> std::result::Result<String, String> {
    let obj = match j {
        Json::String(s) if s.trim_start().starts_with('{') => serde_json::from_str(s).map_err(|e| format!("a MEDIA value's JSON text: {e}"))?,
        Json::String(s) if !s.trim().is_empty() => return Ok(serde_json::json!({ "uri": s.trim() }).to_string()),
        o @ Json::Object(_) => o.clone(),
        _ => return Err(format!("a MEDIA value is a URI or an object of {}", KEYS.join(", "))),
    };
    let Json::Object(m) = &obj else { return Err(format!("a MEDIA value is a URI or an object of {}", KEYS.join(", "))) };
    if let Some(k) = m.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!("{k:?} is not a key of a MEDIA value ({})", KEYS.join(", ")));
    }
    match Input::of(&obj)?.complete() {
        Some(m) => Ok(m.to_text()),
        None => Ok(obj.to_string()),
    }
}

/// What a write gives a MEDIA field, before it is resolved.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    uri: Option<String>,
    data: Option<String>,
    sha256: Option<String>,
    mime: Option<String>,
    bytes: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    duration_ms: Option<u64>,
    pages: Option<u32>,
}

impl Input {
    fn of(j: &Json) -> std::result::Result<Input, String> {
        let i: Input = serde_json::from_value(j.clone()).map_err(|e| format!("a MEDIA value: {e}"))?;
        if i.uri.is_none() && i.data.is_none() {
            return Err("a MEDIA value needs a uri, or the file's bytes as base64 data".into());
        }
        if let Some(s) = &i.sha256 {
            if !is_sha256(s) {
                return Err(format!("sha256 is 64 lowercase hex digits, not {s:?}"));
            }
        }
        Ok(i)
    }

    /// The stored value this input already is: everything but `data`, with
    /// the hash, type and size.
    fn complete(&self) -> Option<Media> {
        if self.data.is_some() {
            return None;
        }
        let m = Media {
            uri: self.uri.clone()?,
            sha256: self.sha256.clone()?,
            mime: self.mime.clone()?,
            bytes: self.bytes?,
            width: self.width,
            height: self.height,
            duration_ms: self.duration_ms,
            pages: self.pages,
        };
        m.check().ok()?;
        Some(m)
    }
}

// ---------------------------------------------------------------- bytes

/// The SHA-256 of `data`, in lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let d = sha2::Sha256::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a file's bytes say about it ([`describe`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Described {
    pub sha256: String,
    /// The type its first bytes show, if they are of one [`describe`] knows.
    pub mime: Option<&'static str>,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    pub pages: Option<u32>,
}

/// Hashes `data` and reads what its headers say: its type (JPEG, PNG,
/// GIF, WebP, AVIF, HEIC, BMP, TIFF; WAV, FLAC, MP3, Ogg, M4A; MP4,
/// QuickTime, WebM, Matroska; PDF), an image's width and height, a WAV,
/// FLAC or MP4 file's duration, a PDF's pages. What it cannot tell cheaply
/// it leaves out; the bytes are not decoded.
pub fn describe(data: &[u8]) -> Described {
    let mut d = Described { sha256: sha256_hex(data), bytes: data.len() as u64, ..Described::default() };
    let at = |i: usize, n: usize| data.get(i..i + n);
    let be32 = |i: usize| at(i, 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let le16 = |i: usize| at(i, 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32);
    let le24 = |i: usize| at(i, 3).map(|b| b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16);
    let le32 = |i: usize| at(i, 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        d.mime = Some("image/png");
        if at(12, 4) == Some(b"IHDR") {
            (d.width, d.height) = (be32(16), be32(20));
        }
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        d.mime = Some("image/jpeg");
        (d.width, d.height) = jpeg_size(data).unzip();
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        d.mime = Some("image/gif");
        (d.width, d.height) = (le16(6), le16(8));
    } else if data.starts_with(b"RIFF") && at(8, 4) == Some(b"WEBP") {
        d.mime = Some("image/webp");
        match at(12, 4) {
            Some(b"VP8 ") => (d.width, d.height) = (le16(26).map(|w| w & 0x3fff), le16(28).map(|h| h & 0x3fff)),
            Some(b"VP8L") => {
                if let Some(b) = at(21, 4) {
                    let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                    (d.width, d.height) = (Some((bits & 0x3fff) + 1), Some(((bits >> 14) & 0x3fff) + 1));
                }
            }
            Some(b"VP8X") => (d.width, d.height) = (le24(24).map(|w| w + 1), le24(27).map(|h| h + 1)),
            _ => {}
        }
    } else if data.starts_with(b"RIFF") && at(8, 4) == Some(b"WAVE") {
        d.mime = Some("audio/wav");
        d.duration_ms = wav_duration(data);
    } else if data.starts_with(b"BM") && le32(14).is_some_and(|h| [12, 40, 52, 56, 108, 124].contains(&h)) {
        d.mime = Some("image/bmp");
        let signed = |v: Option<u32>| v.map(|x| (x as i32).unsigned_abs());
        (d.width, d.height) = if le32(14) == Some(12) { (le16(18), le16(20)) } else { (signed(le32(18)), signed(le32(22))) };
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        d.mime = Some("image/tiff");
    } else if data.starts_with(b"fLaC") {
        d.mime = Some("audio/flac");
        // STREAMINFO, the first metadata block: 20 bits of sample rate and
        // 36 of samples, 10 bytes into it.
        if data.get(4).is_some_and(|b| b & 0x7f == 0) {
            if let Some(b) = at(18, 8) {
                let x = u64::from_be_bytes(b.try_into().unwrap());
                let (rate, samples) = (x >> 44, x & ((1 << 36) - 1));
                d.duration_ms = (rate > 0 && samples > 0).then(|| samples * 1000 / rate);
            }
        }
    } else if data.starts_with(b"ID3") || (data.len() > 1 && data[0] == 0xff && data[1] & 0xe6 == 0xe2) {
        d.mime = Some("audio/mpeg");
    } else if data.starts_with(b"OggS") {
        d.mime = Some("audio/ogg");
    } else if at(4, 4) == Some(b"ftyp") {
        d.mime = Some(match at(8, 4) {
            Some(b"avif" | b"avis") => "image/avif",
            Some(b"heic" | b"heix" | b"mif1" | b"msf1") => "image/heic",
            Some(b"M4A " | b"M4B ") => "audio/mp4",
            Some(b"qt  ") => "video/quicktime",
            _ => "video/mp4",
        });
        if d.mime.is_some_and(|m| m.starts_with("video/") || m == "audio/mp4") {
            d.duration_ms = mp4_duration(data);
        }
    } else if data.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        let head = &data[..data.len().min(64)];
        d.mime = Some(if head.windows(4).any(|w| w == b"webm") { "video/webm" } else { "video/x-matroska" });
    } else if data.starts_with(b"%PDF-") {
        d.mime = Some("application/pdf");
        d.pages = pdf_pages(data);
    }
    d
}

/// A JPEG's width and height, from its first start-of-frame marker.
fn jpeg_size(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xff {
            return None;
        }
        let marker = data[i + 1];
        // Fill bytes, and markers without a length.
        if marker == 0xff {
            i += 1;
            continue;
        }
        if (0xd0..=0xd9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        let sof = (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker);
        if sof {
            let b = data.get(i + 5..i + 9)?;
            return Some((u16::from_be_bytes([b[2], b[3]]) as u32, u16::from_be_bytes([b[0], b[1]]) as u32));
        }
        i += 2 + len;
    }
    None
}

/// A WAV file's duration: its data chunk's size over its byte rate.
fn wav_duration(data: &[u8]) -> Option<u64> {
    let (mut i, mut rate) = (12usize, None);
    while i + 8 <= data.len() {
        let id = &data[i..i + 4];
        let len = u32::from_le_bytes(data[i + 4..i + 8].try_into().unwrap()) as u64;
        if id == b"fmt " {
            rate = data.get(i + 16..i + 20).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as u64);
        } else if id == b"data" {
            return rate.filter(|&r| r > 0).map(|r| len * 1000 / r);
        }
        i = i.checked_add(8 + len as usize + (len as usize & 1))?;
    }
    None
}

/// An MP4 or QuickTime file's duration, from the movie header box
/// (`moov/mvhd`).
fn mp4_duration(data: &[u8]) -> Option<u64> {
    let boxes = |from: usize, to: usize| {
        let mut i = from;
        std::iter::from_fn(move || {
            let head = data.get(i..i + 8)?;
            let mut size = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
            let mut body = i + 8;
            if size == 1 {
                size = u64::from_be_bytes(data.get(i + 8..i + 16)?.try_into().unwrap()) as usize;
                body = i + 16;
            } else if size == 0 {
                size = to - i;
            }
            let end = i.checked_add(size).filter(|&e| e <= to && size >= body - i)?;
            let kind = [head[4], head[5], head[6], head[7]];
            i = end;
            Some((kind, body, end))
        })
    };
    let (_, moov, moov_end) = boxes(0, data.len()).find(|(k, ..)| k == b"moov")?;
    let (_, h, end) = boxes(moov, moov_end).find(|(k, ..)| k == b"mvhd")?;
    let b = data.get(h..end)?;
    let (scale, dur) = match b.first()? {
        0 => (u32::from_be_bytes(b.get(12..16)?.try_into().ok()?) as u64, u32::from_be_bytes(b.get(16..20)?.try_into().ok()?) as u64),
        1 => (u32::from_be_bytes(b.get(20..24)?.try_into().ok()?) as u64, u64::from_be_bytes(b.get(24..32)?.try_into().ok()?)),
        _ => return None,
    };
    (scale > 0 && dur != u64::MAX && dur != u32::MAX as u64).then(|| dur.saturating_mul(1000) / scale)
}

/// A PDF's pages: the largest `/Count` of its page tree (the root's), or
/// failing that the page objects it shows. A file whose objects are all
/// compressed shows neither, and gets none.
fn pdf_pages(data: &[u8]) -> Option<u32> {
    let find = |needle: &'static [u8]| data.windows(needle.len()).enumerate().filter(move |(_, w)| *w == needle).map(|(i, _)| i);
    let count = find(b"/Count")
        .filter_map(|i| {
            let rest = &data[i + 6..data.len().min(i + 24)];
            let digits: Vec<u8> = rest.iter().copied().skip_while(|b| b.is_ascii_whitespace()).take_while(u8::is_ascii_digit).collect();
            std::str::from_utf8(&digits).ok()?.parse::<u32>().ok()
        })
        .max();
    if count.is_some_and(|c| c > 0) {
        return count;
    }
    let pages = find(b"/Type")
        .filter(|&i| {
            let rest = &data[i + 5..data.len().min(i + 16)];
            let rest: Vec<u8> = rest.iter().copied().skip_while(|b| b.is_ascii_whitespace()).collect();
            rest.starts_with(b"/Page") && !rest.get(5).is_some_and(|b| b.is_ascii_alphanumeric())
        })
        .count() as u32;
    (pages > 0).then_some(pages)
}

// ---------------------------------------------------------------- sources

/// The default of [`Resolver::max_bytes`]: 50 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 50 << 20;

/// A URI a write names, taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A `file:` URI's path, percent-decoded.
    File(PathBuf),
    /// An `https:` or `http:` URL: its scheme, host (lowercase) and port.
    Http { scheme: &'static str, host: String, port: u16 },
    /// `media:<sha256>`: bytes kept in the media store.
    Stored(String),
}

impl Source {
    pub fn parse(uri: &str) -> std::result::Result<Source, String> {
        let (scheme, rest) = uri.split_once(':').ok_or_else(|| format!("{uri:?} is not a URI (file:, https: or media:)"))?;
        match scheme.to_ascii_lowercase().as_str() {
            "file" => {
                let path = match rest.strip_prefix("//") {
                    Some(r) if r.starts_with('/') => r,
                    Some(r) => match r.split_once('/') {
                        Some((host, _)) if host.eq_ignore_ascii_case("localhost") => &r[host.len()..],
                        _ => return Err(format!("{uri}: a file: URI names a path on this machine (file:///path)")),
                    },
                    None => rest,
                };
                let path = path.split(['?', '#']).next().unwrap_or_default();
                if !path.starts_with('/') {
                    return Err(format!("{uri}: a file: URI's path is absolute"));
                }
                Ok(Source::File(PathBuf::from(percent_decode(path).map_err(|e| format!("{uri}: {e}"))?)))
            }
            s @ ("https" | "http") => {
                let scheme = if s == "https" { "https" } else { "http" };
                let rest = rest.strip_prefix("//").ok_or_else(|| format!("{uri}: an {scheme}: URL is {scheme}://host/path"))?;
                let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
                let (host, port) = host_port(authority, scheme).map_err(|e| format!("{uri}: {e}"))?;
                Ok(Source::Http { scheme, host, port })
            }
            "media" => {
                let h = rest.trim_start_matches('/');
                if !is_sha256(h) {
                    return Err(format!("{uri}: a media: URI is media:<sha256 in lowercase hex>"));
                }
                Ok(Source::Stored(h.to_string()))
            }
            _ => Err(format!("{uri}: Strata fetches file:, https: and media: URIs, not {scheme}:")),
        }
    }
}

fn default_port(scheme: &str) -> u16 {
    if scheme == "https" {
        443
    } else {
        80
    }
}

/// An authority's host (lowercase, without a trailing dot) and port.
fn host_port(authority: &str, scheme: &str) -> std::result::Result<(String, u16), String> {
    if authority.contains('@') {
        return Err("a URL with a user name or password is not fetched".into());
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, rest) = v6.split_once(']').ok_or("an IPv6 host is [address]")?;
            (format!("[{}]", h.to_ascii_lowercase()), rest.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_ascii_lowercase(), Some(p)),
            None => (authority.to_ascii_lowercase(), None),
        },
    };
    let host = host.trim_end_matches('.').to_string();
    if host.is_empty() {
        return Err("no host".into());
    }
    let port = match port {
        None | Some("") => default_port(scheme),
        Some(p) => p.parse().map_err(|_| format!("port {p:?} is not a number"))?,
    };
    Ok((host, port))
}

fn percent_decode(s: &str) -> std::result::Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
            out.push(hex.ok_or("a % is followed by two hex digits")?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "the path is not UTF-8".into())
}

/// What Strata may fetch: `file:` roots and HTTP hosts (`--media-fetch`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowlist {
    /// Canonical directories.
    roots: Vec<PathBuf>,
    /// Scheme, host (or `*.domain`) and port.
    hosts: Vec<(&'static str, String, u16)>,
}

impl Allowlist {
    /// An allowlist of `entries`, each a `file:` URI or absolute path of a
    /// directory (`file:/data/media`, `/data/media`), a host (fetched over
    /// HTTPS: `cdn.example.com`, `*.example.com` for its subdomains,
    /// `cdn.example.com:8443`), or a URL naming one (`https://cdn.example.com`,
    /// and `http://` only when it is spelled out). A directory must exist.
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Allowlist> {
        let mut a = Allowlist::default();
        for e in entries {
            a.add(e.as_ref())?;
        }
        Ok(a)
    }

    /// Adds one entry ([`Allowlist::parse`]).
    pub fn add(&mut self, entry: &str) -> Result<()> {
        let bad = |m: String| Error::Schema(format!("media fetch entry {entry:?}: {m}"));
        let entry = entry.trim();
        if entry.is_empty() {
            return Ok(());
        }
        if entry.starts_with('/') || entry.to_ascii_lowercase().starts_with("file:") {
            let path = if entry.starts_with('/') { PathBuf::from(entry) } else { match Source::parse(entry).map_err(bad)? { Source::File(p) => p, _ => unreachable!("a file: URI") } };
            let root = std::fs::canonicalize(&path).map_err(|e| bad(format!("{}: {e}", path.display())))?;
            if !root.is_dir() {
                return Err(bad(format!("{} is not a directory", root.display())));
            }
            self.roots.push(root);
            return Ok(());
        }
        let (scheme, authority) = match entry.split_once("://") {
            Some((s, rest)) if s.eq_ignore_ascii_case("https") => ("https", rest),
            Some((s, rest)) if s.eq_ignore_ascii_case("http") => ("http", rest),
            Some((s, _)) => return Err(bad(format!("Strata fetches file:, https: and http: sources, not {s}:"))),
            None => ("https", entry),
        };
        let authority = authority.trim_end_matches('/');
        if authority.contains(['/', '?', '#']) {
            return Err(bad("an entry names a host, not a path on it".into()));
        }
        let (host, port) = host_port(authority, scheme).map_err(bad)?;
        self.hosts.push((scheme, host, port));
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty() && self.hosts.is_empty()
    }

    /// The entries, as [`Allowlist::parse`] takes them.
    pub fn entries(&self) -> Vec<String> {
        let roots = self.roots.iter().map(|r| format!("file:{}", r.display()));
        let hosts = self.hosts.iter().map(|(s, h, p)| if *p == default_port(s) { format!("{s}://{h}") } else { format!("{s}://{h}:{p}") });
        roots.chain(hosts).collect()
    }

    /// Whether an HTTP URL's scheme, host and port are on the list.
    pub fn allows_http(&self, url: &str) -> bool {
        match Source::parse(url) {
            Ok(Source::Http { scheme, host, port }) => self.hosts.iter().any(|(s, h, p)| {
                let host_ok = match h.strip_prefix("*.") {
                    Some(domain) => host.len() > domain.len() + 1 && host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.',
                    None => *h == host,
                };
                *s == scheme && *p == port && host_ok
            }),
            _ => false,
        }
    }

    /// The canonical path of a file a `file:` URI names, if it lies inside
    /// one of the roots once symbolic links and `..` are resolved.
    pub fn allows_file(&self, path: &Path) -> std::result::Result<PathBuf, String> {
        if self.roots.is_empty() {
            return Err("no file: root is on the media fetch list".into());
        }
        let real = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !self.roots.iter().any(|r| real.starts_with(r)) {
            return Err(format!("{} is not inside a file: root on the media fetch list", path.display()));
        }
        if !real.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        Ok(real)
    }
}

/// Why a [`Fetcher`] did not return a file's bytes.
#[derive(Debug)]
pub enum FetchError {
    /// The fetch took longer than the fetcher's timeout.
    Timeout(String),
    Failed(String),
}

/// Fetches HTTP(S) URLs for a [`Resolver`]: a server's client, with its
/// timeout. It must refuse a redirect to a URL the resolver's allowlist
/// does not have ([`Allowlist::allows_http`]), and a body larger than
/// `max_bytes`.
pub trait Fetcher: Send + Sync {
    fn get(&self, url: &str, max_bytes: u64) -> std::result::Result<Vec<u8>, FetchError>;
}

/// Turns what writes give MEDIA fields into the values they store (see
/// the module's table). The default fetches nothing and has no store: it
/// takes complete values, and bytes given with a URI.
#[derive(Clone)]
pub struct Resolver {
    pub allow: Allowlist,
    /// The largest file taken, fetched or given (default
    /// [`DEFAULT_MAX_BYTES`]).
    pub max_bytes: u64,
    /// Where bytes given without a URI are kept, as `<sha256>`.
    pub store: Option<PathBuf>,
    pub http: Option<Arc<dyn Fetcher>>,
}

impl Default for Resolver {
    fn default() -> Resolver {
        Resolver { allow: Allowlist::default(), max_bytes: DEFAULT_MAX_BYTES, store: None, http: None }
    }
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").field("allow", &self.allow).field("max_bytes", &self.max_bytes).field("store", &self.store).field("http", &self.http.is_some()).finish()
    }
}

/// A resolved MEDIA value, and the bytes it was made from if they passed
/// through (for models to embed).
#[derive(Clone, Debug)]
pub struct Resolved {
    pub media: Media,
    pub data: Option<Arc<[u8]>>,
}

/// MEDIA values resolved at once ([`Resolver::resolve_all`]): file
/// fetches wait on the network, not the cores.
pub const PARALLEL_FETCHES: usize = 16;

impl Resolver {
    /// The stored value of what a write gave a MEDIA field of `kind` (the
    /// JSON text [`input_text`] made). A complete value is kept as given;
    /// otherwise the bytes are taken from `data`, the file, the URL or the
    /// media store, hashed and described ([`describe`]), and a `sha256`
    /// the write gave must match them. The type is the one the bytes show,
    /// else the one the write gave.
    pub fn resolve(&self, kind: MediaKind, input: &str) -> std::result::Result<Resolved, Error> {
        let j: Json = serde_json::from_str(input).map_err(|e| Error::Schema(format!("a MEDIA value's JSON text: {e}")))?;
        let i = Input::of(&j).map_err(Error::Schema)?;
        if let Some(m) = i.complete() {
            check_kind(kind, &m.mime)?;
            return Ok(Resolved { media: m, data: None });
        }
        let data: Vec<u8> = match (&i.data, &i.uri) {
            (Some(b64), _) => {
                // Base64 is 4/3 the size of its bytes.
                if b64.len() as u64 / 4 * 3 > self.max_bytes + 3 {
                    return Err(too_big("the data", self.max_bytes));
                }
                base64_bytes(b64).map_err(|e| Error::Schema(format!("a MEDIA value's data is base64: {e}")))?
            }
            (None, Some(uri)) => self.fetch(uri)?,
            (None, None) => unreachable!("Input::of checks"),
        };
        if data.len() as u64 > self.max_bytes {
            return Err(too_big("the data", self.max_bytes));
        }
        let d = describe(&data);
        if let Some(s) = &i.sha256 {
            if *s != d.sha256 {
                return Err(Error::Schema(format!("the bytes of {} hash to sha256 {}, not the {s} given", i.uri.as_deref().unwrap_or("the data"), d.sha256)));
            }
        }
        let uri = match &i.uri {
            Some(u) => u.clone(),
            None => {
                let dir = self.store.as_ref().ok_or_else(|| Error::Schema("a MEDIA value of data without a uri needs a media store (--media-store DIR) to keep the bytes in".into()))?;
                keep(dir, &d.sha256, &data)?;
                format!("media:{}", d.sha256)
            }
        };
        let mime = d.mime.map(String::from).or(i.mime).unwrap_or_else(|| "application/octet-stream".into());
        check_kind(kind, &mime)?;
        let media = Media {
            uri,
            sha256: d.sha256,
            mime,
            bytes: d.bytes,
            width: d.width.or(i.width),
            height: d.height.or(i.height),
            duration_ms: d.duration_ms.or(i.duration_ms),
            pages: d.pages.or(i.pages),
        };
        Ok(Resolved { media, data: Some(data.into()) })
    }

    /// [`Resolver::resolve`] of many values, [`PARALLEL_FETCHES`] at a time.
    pub fn resolve_all(&self, kind: MediaKind, inputs: &[&str]) -> Vec<std::result::Result<Resolved, Error>> {
        let threads = PARALLEL_FETCHES.min(inputs.len());
        if threads <= 1 {
            return inputs.iter().map(|t| self.resolve(kind, t)).collect();
        }
        let next = AtomicUsize::new(0);
        let mut out: Vec<(usize, std::result::Result<Resolved, Error>)> = std::thread::scope(|s| {
            let parts: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let mut mine = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(t) = inputs.get(i) else { break mine };
                            mine.push((i, self.resolve(kind, t)));
                        }
                    })
                })
                .collect();
            parts.into_iter().flat_map(|p| p.join().expect("media thread")).collect()
        });
        out.sort_by_key(|(i, _)| *i);
        out.into_iter().map(|(_, r)| r).collect()
    }

    /// The bytes of a stored value, from where its URI says (the media
    /// store, or a file or URL the allowlist has), checked against its
    /// hash: what embedding a value written whole needs.
    pub fn bytes_of(&self, m: &Media) -> std::result::Result<Vec<u8>, Error> {
        let data = self.fetch(&m.uri)?;
        let sha256 = sha256_hex(&data);
        if sha256 != m.sha256 {
            return Err(Error::Schema(format!("the bytes of {} hash to sha256 {sha256}, not the {} its value has", m.uri, m.sha256)));
        }
        Ok(data)
    }

    /// The bytes a URI names, if the allowlist (or the media store) has
    /// them.
    fn fetch(&self, uri: &str) -> std::result::Result<Vec<u8>, Error> {
        let refuse = |m: String| Error::Schema(format!("{uri}: {m}"));
        match Source::parse(uri).map_err(Error::Schema)? {
            Source::File(path) => {
                let real = self.allow.allows_file(&path).map_err(|m| refuse(format!("not fetched: {m} (--media-fetch)")))?;
                read_capped(&real, self.max_bytes)
            }
            Source::Http { .. } => {
                if !self.allow.allows_http(uri) {
                    return Err(refuse("not fetched: its host is not on the media fetch list (--media-fetch)".into()));
                }
                let http = self.http.as_ref().ok_or_else(|| refuse("not fetched: this database fetches no URLs (a server with --media-fetch does)".into()))?;
                http.get(uri, self.max_bytes).map_err(|e| match e {
                    FetchError::Timeout(m) => Error::Timeout(format!("{uri}: {m}")),
                    FetchError::Failed(m) => refuse(m),
                })
            }
            Source::Stored(h) => {
                let dir = self.store.as_ref().ok_or_else(|| refuse("this database has no media store (--media-store DIR)".into()))?;
                read_capped(&dir.join(&h), self.max_bytes)
            }
        }
    }
}

/// The largest side of an image [`decode_image`] decodes.
pub const MAX_IMAGE_SIDE: u32 = 16_384;

/// The pixels of an image file (JPEG, PNG, WebP, GIF, BMP) as 8-bit RGB
/// (feature `vision`). An image larger than [`MAX_IMAGE_SIDE`] a side, or
/// whose pixels would take more than 512 MiB, is refused before it is
/// decoded.
#[cfg(feature = "vision")]
pub fn decode_image(data: &[u8]) -> Result<crate::embed::Image> {
    let bad = |e: image::ImageError| Error::Schema(format!("the image could not be decoded: {e}"));
    let mut r = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().map_err(|e| Error::Schema(format!("the image could not be read: {e}")))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE);
    limits.max_image_height = Some(MAX_IMAGE_SIDE);
    limits.max_alloc = Some(512 << 20);
    r.limits(limits);
    let rgb = r.decode().map_err(bad)?.into_rgb8();
    Ok(crate::embed::Image { width: rgb.width(), height: rgb.height(), rgb: rgb.into_raw() })
}

/// Without the `vision` feature, images are not decoded.
#[cfg(not(feature = "vision"))]
pub fn decode_image(_data: &[u8]) -> Result<crate::embed::Image> {
    Err(Error::Schema("decoding images needs Strata built with the `vision` feature".into()))
}

/// The sound of an audio file (WAV, FLAC, MP3, Ogg Vorbis, AAC in MP4) as
/// one channel at its own rate (feature `audio`, [`crate::audio::decode`]).
#[cfg(feature = "audio")]
pub fn decode_audio(data: &[u8]) -> Result<crate::embed::Audio> {
    crate::audio::decode(data)
}

/// Without the `audio` feature, audio is not decoded.
#[cfg(not(feature = "audio"))]
pub fn decode_audio(_data: &[u8]) -> Result<crate::embed::Audio> {
    Err(Error::Schema("decoding audio needs Strata built with the `audio` feature".into()))
}

/// The bytes of base64 text, in the standard alphabet or the URL-safe
/// one, padded or not, with any whitespace.
pub fn base64_bytes(text: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::Engine;
    let clean: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let engine = if clean.contains(['-', '_']) { &base64::engine::general_purpose::URL_SAFE_NO_PAD } else { &base64::engine::general_purpose::STANDARD_NO_PAD };
    engine.decode(clean.trim_end_matches('=')).map_err(|e| e.to_string())
}

fn check_kind(kind: MediaKind, mime: &str) -> Result<()> {
    if kind.accepts(mime) {
        Ok(())
    } else {
        Err(Error::Schema(format!("a MEDIA field of kind {} does not hold {mime}", kind.as_str())))
    }
}

fn too_big(what: &str, max: u64) -> Error {
    Error::Schema(format!("{what} is larger than the media size cap of {max} bytes (--media-max-bytes)"))
}

/// A file's bytes, refused past `max` bytes.
fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>> {
    let f = std::fs::File::open(path).map_err(|e| Error::Schema(format!("{}: {e}", path.display())))?;
    if f.metadata().map(|m| m.len()).unwrap_or(0) > max {
        return Err(too_big(&path.display().to_string(), max));
    }
    let mut data = Vec::new();
    f.take(max + 1).read_to_end(&mut data).map_err(|e| Error::Schema(format!("{}: {e}", path.display())))?;
    if data.len() as u64 > max {
        return Err(too_big(&path.display().to_string(), max));
    }
    Ok(data)
}

/// Keeps `data` in the media store as `<sha256>`, unless it is there.
fn keep(dir: &Path, sha256: &str, data: &[u8]) -> Result<()> {
    let path = dir.join(sha256);
    if path.exists() {
        return Ok(());
    }
    let tmp = dir.join(format!(".{sha256}.{}.tmp", std::process::id()));
    std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&tmp, data)).and_then(|_| std::fs::rename(&tmp, &path)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::Io(std::io::Error::new(e.kind(), format!("keep media in {}: {e}", dir.display())))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 PNG, a minimal JPEG header, a GIF, WebPs, a WAV and a PDF.
    pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut b = vec![0xff, 0xd8, 0xff, 0xe0, 0, 16];
        b.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        b.extend([0xff, 0xc0, 0, 17, 8]);
        b.extend(h.to_be_bytes());
        b.extend(w.to_be_bytes());
        b.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        b
    }

    fn wav(seconds: u32) -> Vec<u8> {
        let (rate, channels, bits) = (16_000u32, 1u16, 16u16);
        let byte_rate = rate * channels as u32 * bits as u32 / 8;
        let n = byte_rate * seconds;
        let mut b = b"RIFF".to_vec();
        b.extend((36 + n).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(channels.to_le_bytes());
        b.extend(rate.to_le_bytes());
        b.extend(byte_rate.to_le_bytes());
        b.extend((channels * bits / 8).to_le_bytes());
        b.extend(bits.to_le_bytes());
        b.extend(b"data");
        b.extend(n.to_le_bytes());
        b.resize(b.len() + n as usize, 0);
        b
    }

    #[test]
    fn files_are_described_by_their_headers() {
        let d = describe(&png(640, 480));
        assert_eq!((d.mime, d.width, d.height, d.bytes), (Some("image/png"), Some(640), Some(480), 33));
        assert_eq!(d.sha256, sha256_hex(&png(640, 480)));
        assert!(is_sha256(&d.sha256));
        let d = describe(&jpeg(1024, 768));
        assert_eq!((d.mime, d.width, d.height), (Some("image/jpeg"), Some(1024), Some(768)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x20, 0x01, 0x10, 0x00]);
        assert_eq!(describe(&gif).width.zip(describe(&gif).height), Some((288, 16)));
        let mut vp8x = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\0\0\0\0".to_vec();
        vp8x.extend([0x7f, 0x02, 0x00, 0xdf, 0x01, 0x00]);
        let d = describe(&vp8x);
        assert_eq!((d.mime, d.width, d.height), (Some("image/webp"), Some(640), Some(480)));
        let d = describe(&wav(3));
        assert_eq!((d.mime, d.duration_ms), (Some("audio/wav"), Some(3000)));
        let pdf = b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >>\n2 0 obj << /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>\n3 0 obj << /Type /Page >>\n4 0 obj << /Type/Page >>";
        assert_eq!((describe(pdf).mime, describe(pdf).pages), (Some("application/pdf"), Some(2)));
        let no_count = b"%PDF-1.4\n3 0 obj << /Type /Page >>\n4 0 obj << /Type/Page/Parent 2 0 R >>\n2 0 obj << /Type /Pages >>";
        assert_eq!(describe(no_count).pages, Some(2));
        // An MP4 with a movie header: 90 000 ticks a second, 4.5 seconds.
        let mut mp4 = Vec::new();
        mp4.extend(20u32.to_be_bytes());
        mp4.extend(b"ftypisom\0\0\x02\0isom");
        let mut mvhd = vec![0u8; 4 + 4 + 4];
        mvhd.extend(90_000u32.to_be_bytes());
        mvhd.extend(405_000u32.to_be_bytes());
        mvhd.resize(100, 0);
        mp4.extend((8 + 8 + mvhd.len() as u32).to_be_bytes());
        mp4.extend(b"moov");
        mp4.extend((8 + mvhd.len() as u32).to_be_bytes());
        mp4.extend(b"mvhd");
        mp4.extend(mvhd);
        let d = describe(&mp4);
        assert_eq!((d.mime, d.duration_ms), (Some("video/mp4"), Some(4500)));
        let d = describe(b"hello");
        assert_eq!((d.mime, d.bytes, d.width), (None, 5, None));
        // Truncated headers describe what they can, and never panic.
        for n in 0..40 {
            for f in [png(1, 1), jpeg(2, 2), wav(1), mp4.clone(), vp8x.clone()] {
                describe(&f[..n.min(f.len())]);
            }
        }
    }

    #[test]
    fn values_are_complete_or_inputs() {
        let h = sha256_hex(b"x");
        let full = format!(r#"{{"uri":"https://cdn.example.com/a.jpg","sha256":"{h}","mime":"image/jpeg","bytes":1}}"#);
        let m = Media::parse(&full).unwrap();
        assert_eq!(m.to_text(), full);
        assert_eq!(hash_of(&full), Some(h.clone()));
        for bad in [r#"{"uri":"a"}"#, r#"{"uri":"a","sha256":"ABC","mime":"image/jpeg","bytes":1}"#, r#"[1]"#] {
            assert!(Media::parse(bad).is_err(), "{bad}");
        }
        // A URI alone, an object, or the object's text.
        assert_eq!(input_text(&Json::from("https://x.org/a.png")).unwrap(), r#"{"uri":"https://x.org/a.png"}"#);
        assert_eq!(input_text(&serde_json::json!({"data": "AAAA"})).unwrap(), r#"{"data":"AAAA"}"#);
        assert_eq!(input_text(&Json::from(r#"{"uri": "file:/a"}"#)).unwrap(), r#"{"uri":"file:/a"}"#);
        for bad in [serde_json::json!(3), serde_json::json!({"color": "red", "uri": "a"}), serde_json::json!({"mime": "image/png"}), serde_json::json!({"uri": "a", "sha256": "x"})] {
            assert!(input_text(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn uris_are_taken_apart() {
        assert_eq!(Source::parse("file:///data/my%20cat.jpg").unwrap(), Source::File("/data/my cat.jpg".into()));
        assert_eq!(Source::parse("file:/data/a.jpg").unwrap(), Source::File("/data/a.jpg".into()));
        assert_eq!(Source::parse("file://localhost/data/a.jpg").unwrap(), Source::File("/data/a.jpg".into()));
        assert_eq!(Source::parse("https://CDN.Example.com./a?b=c").unwrap(), Source::Http { scheme: "https", host: "cdn.example.com".into(), port: 443 });
        assert_eq!(Source::parse("http://127.0.0.1:8080/x").unwrap(), Source::Http { scheme: "http", host: "127.0.0.1".into(), port: 8080 });
        assert_eq!(Source::parse("https://[::1]:9/x").unwrap(), Source::Http { scheme: "https", host: "[::1]".into(), port: 9 });
        let h = sha256_hex(b"");
        assert_eq!(Source::parse(&format!("media:{h}")).unwrap(), Source::Stored(h));
        for bad in ["file://elsewhere/a", "file:a.jpg", "s3://bucket/a", "https://user:pw@host/a", "https:///a", "nope", "media:abc", "https://host:http/"] {
            assert!(Source::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_allowlist_names_hosts_and_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.png"), png(1, 1)).unwrap();
        std::fs::write(dir.path().join("secret"), b"no").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("secret"), root.join("link")).unwrap();
        let a = Allowlist::parse(&[format!("file:{}", root.display()), "cdn.example.com".into(), "*.images.org".into(), "http://127.0.0.1:8080".into()]).unwrap();
        assert!(a.allows_file(&root.join("a.png")).is_ok());
        // Out of the root by `..` or a link, or not there.
        for p in [root.join("../secret"), root.join("link"), root.join("missing.png"), root.clone()] {
            assert!(a.allows_file(&p).is_err(), "{}", p.display());
        }
        assert!(a.allows_http("https://cdn.example.com/a.jpg"));
        assert!(a.allows_http("https://x.images.org/a.jpg") && a.allows_http("https://a.b.images.org/a.jpg"));
        assert!(a.allows_http("http://127.0.0.1:8080/a"));
        for url in ["http://cdn.example.com/a.jpg", "https://cdn.example.com:8443/a", "https://images.org/a", "https://evilimages.org/a", "https://cdn.example.com.evil.net/a", "https://127.0.0.1:8080/a", "http://127.0.0.1:8081/a", "file:///etc/passwd"] {
            assert!(!a.allows_http(url), "{url}");
        }
        assert_eq!(a.entries().len(), 4);
        for bad in ["ftp://host", "https://host/path", "/no/such/dir"] {
            assert!(Allowlist::parse(&[bad]).is_err(), "{bad}");
        }
        assert!(Allowlist::parse(&[root.join("a.png").display().to_string()]).is_err(), "a file is not a root");
    }

    #[test]
    fn writes_resolve_by_data_file_and_store() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("media");
        let store = dir.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        let img = png(4, 3);
        std::fs::write(root.join("a.png"), &img).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&img);
        let none = Resolver::default();
        // Bytes with a URI need nothing.
        let r = none.resolve(MediaKind::Image, &serde_json::json!({"uri": "https://cdn/a.png", "data": b64}).to_string()).unwrap();
        assert_eq!((r.media.uri.as_str(), r.media.mime.as_str(), r.media.width, r.media.bytes), ("https://cdn/a.png", "image/png", Some(4), img.len() as u64));
        assert_eq!(r.data.as_deref(), Some(&img[..]));
        // Nothing fetched or kept without a list or a store.
        let file = format!("file:{}", root.join("a.png").display());
        let e = none.resolve(MediaKind::Image, &serde_json::json!({"uri": file}).to_string()).unwrap_err().to_string();
        assert!(e.contains("--media-fetch"), "{e}");
        let e = none.resolve(MediaKind::Image, &serde_json::json!({"data": b64}).to_string()).unwrap_err().to_string();
        assert!(e.contains("--media-store"), "{e}");
        let e = none.resolve(MediaKind::Image, &serde_json::json!({"uri": "https://cdn.example.com/a.png"}).to_string()).unwrap_err().to_string();
        assert!(e.contains("not on the media fetch list"), "{e}");
        let r = Resolver { allow: Allowlist::parse(&[root.display().to_string(), "cdn.example.com".into()]).unwrap(), store: Some(store.clone()), ..Resolver::default() };
        assert_eq!(r.resolve(MediaKind::Image, &serde_json::json!({"uri": file}).to_string()).unwrap().media.sha256, sha256_hex(&img));
        let e = r.resolve(MediaKind::Image, &serde_json::json!({"uri": "https://cdn.example.com/a.png"}).to_string()).unwrap_err().to_string();
        assert!(e.contains("fetches no URLs"), "{e}");
        // Data alone goes to the store, and is read back by its media: URI.
        let kept = r.resolve(MediaKind::Any, &serde_json::json!({"data": b64}).to_string()).unwrap().media;
        assert_eq!(kept.uri, format!("media:{}", sha256_hex(&img)));
        assert_eq!(std::fs::read(store.join(sha256_hex(&img))).unwrap(), img);
        let again = r.resolve(MediaKind::Any, &serde_json::json!({"uri": kept.uri}).to_string()).unwrap().media;
        assert_eq!(again, kept);
        // A complete value is kept as given; a wrong hash, kind or size is refused.
        let full = serde_json::json!({"uri": "s3://b/a.jpg", "sha256": sha256_hex(b"x"), "mime": "image/jpeg", "bytes": 9, "width": 3});
        assert_eq!(r.resolve(MediaKind::Image, &full.to_string()).unwrap().media.to_text(), Media::parse(&full.to_string()).unwrap().to_text());
        let e = r.resolve(MediaKind::Audio, &full.to_string()).unwrap_err().to_string();
        assert!(e.contains("kind audio does not hold image/jpeg"), "{e}");
        let e = r.resolve(MediaKind::Image, &serde_json::json!({"uri": file, "sha256": sha256_hex(b"x")}).to_string()).unwrap_err().to_string();
        assert!(e.contains("hash to sha256"), "{e}");
        let small = Resolver { max_bytes: 10, ..r.clone() };
        for input in [serde_json::json!({"uri": file}), serde_json::json!({"uri": "a", "data": b64})] {
            let e = small.resolve(MediaKind::Image, &input.to_string()).unwrap_err().to_string();
            assert!(e.contains("media size cap of 10 bytes"), "{e}");
        }
        // Many at once, in order.
        let inputs: Vec<String> = (0..40).map(|i| serde_json::json!({"uri": format!("u{i}"), "data": base64::engine::general_purpose::STANDARD.encode(format!("{i}"))}).to_string()).collect();
        let refs: Vec<&str> = inputs.iter().map(|s| s.as_str()).collect();
        let all = r.resolve_all(MediaKind::Any, &refs);
        assert!(all.iter().enumerate().all(|(i, x)| x.as_ref().unwrap().media.uri == format!("u{i}")));
    }
}
