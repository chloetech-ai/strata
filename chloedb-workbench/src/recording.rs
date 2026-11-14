//! Terminal recordings: every chunk of a session's output appended to
//! `<data-dir>/sessions/<id>/term.cast` as asciicast v2 (a header line,
//! then `[seconds, "o", text]` lines, and `[seconds, "r", "COLSxROWS"]` for
//! a resize), and the windows of it that Rewind plays.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Instant;

use serde_json::{json, Value as Json};

/// The most a session's recording grows to; then it stops, and says so.
pub const MAX_BYTES: u64 = 64 << 20;
/// The most of events one window answers with.
pub const WINDOW_MAX: usize = 8 << 20;
/// The most of the screen before a window that it answers with.
pub const BEFORE_MAX: usize = 256 * 1024;

/// A session's recording, as it is written.
pub struct Recorder {
    file: Option<File>,
    start: Instant,
    bytes: u64,
    max: u64,
    /// The end of the last chunk when it stopped inside a UTF-8 character.
    carry: Vec<u8>,
}

/// What a write did.
#[derive(Debug, PartialEq)]
pub enum Wrote {
    Ok,
    /// It reached its cap and recorded no more.
    Full,
    /// It could not write (or had stopped before).
    Off,
}

impl Recorder {
    /// Creates `path` with its header; `None` when it cannot be written.
    pub fn create(path: &Path, cols: u16, rows: u16, start: Instant, started_ms: i64, max: u64) -> Option<Recorder> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).ok()?;
        let header = format!("{}\n", json!({ "version": 2, "width": cols, "height": rows, "timestamp": started_ms.div_euclid(1000) }));
        file.write_all(header.as_bytes()).ok()?;
        Some(Recorder { file: Some(file), start, bytes: header.len() as u64, max, carry: Vec::new() })
    }

    fn line(&mut self, code: &str, data: &str) -> Wrote {
        let Some(f) = self.file.as_mut() else { return Wrote::Off };
        let t = self.start.elapsed().as_secs_f64();
        let line = format!("{}\n", json!([(t * 1e6).round() / 1e6, code, data]));
        if self.bytes + line.len() as u64 > self.max {
            self.file = None;
            return Wrote::Full;
        }
        if f.write_all(line.as_bytes()).is_err() {
            self.file = None;
            return Wrote::Off;
        }
        self.bytes += line.len() as u64;
        Wrote::Ok
    }

    /// Records output. A UTF-8 character split between two chunks is kept
    /// whole, in the second.
    pub fn output(&mut self, bytes: &[u8]) -> Wrote {
        if self.file.is_none() {
            return Wrote::Off;
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(bytes);
        let keep = incomplete_tail(&buf);
        self.carry = buf.split_off(buf.len() - keep);
        if buf.is_empty() {
            return Wrote::Ok;
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        self.line("o", &text)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Wrote {
        self.line("r", &format!("{cols}x{rows}"))
    }
}

/// How many bytes at the end of `b` begin a UTF-8 character that is not
/// complete yet (0 to 3).
fn incomplete_tail(b: &[u8]) -> usize {
    for back in 1..=3.min(b.len()) {
        let c = b[b.len() - back];
        if c & 0xC0 == 0x80 {
            continue; // a continuation byte: look further back
        }
        let need = if c >= 0xF0 {
            4
        } else if c >= 0xE0 {
            3
        } else if c >= 0xC0 {
            2
        } else {
            1
        };
        return if need > back { back } else { 0 };
    }
    0
}

fn trim_front(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = s.len() - max;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s.drain(..cut);
    }
}

/// The window `[from_ms, to_ms]` of a recording: the terminal's size at
/// `from_ms`, its events in the window (milliseconds since the start), at
/// most [`WINDOW_MAX`] of them, and the screen's bytes before it (the last
/// [`BEFORE_MAX`]).
pub fn window(path: &Path, from_ms: u64, to_ms: u64) -> std::io::Result<Json> {
    let f = BufReader::new(File::open(path)?);
    let mut lines = f.lines();
    let header: Json = match lines.next() {
        Some(l) => serde_json::from_str(&l?).unwrap_or(Json::Null),
        None => Json::Null,
    };
    let (mut width, mut height) = (header["width"].as_u64().unwrap_or(120), header["height"].as_u64().unwrap_or(32));
    let mut before = String::new();
    let mut events = Vec::new();
    let mut size = 0usize;
    let mut truncated = false;
    let mut last_ms = 0u64;
    for line in lines {
        let line = line?;
        let Ok(Json::Array(ev)) = serde_json::from_str::<Json>(&line) else { continue };
        let (Some(t), Some(code), Some(data)) = (ev.first().and_then(Json::as_f64), ev.get(1).and_then(Json::as_str), ev.get(2).and_then(Json::as_str)) else { continue };
        let ms = (t * 1000.0).max(0.0) as u64;
        last_ms = last_ms.max(ms);
        if ms < from_ms {
            match code {
                "o" => {
                    before.push_str(data);
                    if before.len() > 2 * BEFORE_MAX {
                        trim_front(&mut before, BEFORE_MAX);
                    }
                }
                "r" => {
                    if let Some((c, r)) = data.split_once('x') {
                        if let (Ok(c), Ok(r)) = (c.parse(), r.parse()) {
                            (width, height) = (c, r);
                        }
                    }
                }
                _ => {}
            }
        } else if ms <= to_ms {
            if size + data.len() > WINDOW_MAX {
                truncated = true;
                break;
            }
            size += data.len();
            events.push(json!([ms, code, data]));
        }
    }
    trim_front(&mut before, BEFORE_MAX);
    Ok(json!({ "width": width, "height": height, "events": events, "before": before, "truncated": truncated, "duration_ms": last_ms }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_split_characters_whole_and_stops_at_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("term.cast");
        let mut r = Recorder::create(&p, 80, 24, Instant::now(), 1_700_000_000_123, 400).unwrap();
        let e = "é".as_bytes();
        assert_eq!(r.output(&[b'a', e[0]]), Wrote::Ok);
        assert_eq!(r.output(&[e[1], b'b']), Wrote::Ok);
        assert_eq!(r.resize(100, 30), Wrote::Ok);
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<Json> = body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0], json!({"version": 2, "width": 80, "height": 24, "timestamp": 1_700_000_000}));
        assert_eq!(lines[1][1], "o");
        assert_eq!(lines[1][2], "a");
        assert_eq!(lines[2][2], "éb");
        assert_eq!(lines[3][2], "100x30");
        let mut full = Wrote::Ok;
        for _ in 0..100 {
            full = r.output(b"0123456789");
            if full != Wrote::Ok {
                break;
            }
        }
        assert_eq!(full, Wrote::Full);
        assert_eq!(r.output(b"more"), Wrote::Off);
        assert!(std::fs::metadata(&p).unwrap().len() <= 400);

        let w = window(&p, 0, u64::MAX).unwrap();
        assert_eq!(w["width"], 80);
        assert_eq!(w["events"][1][2], "éb");
        assert_eq!(w["before"], "");
        let w = window(&p, u64::MAX - 1, u64::MAX).unwrap();
        assert_eq!(w["width"], 100);
        assert!(w["before"].as_str().unwrap().starts_with("aéb0123"));
        assert!(w["events"].as_array().unwrap().is_empty());
    }
}
