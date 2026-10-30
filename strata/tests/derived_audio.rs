//! Text made from sound (feature `audio`): a TEXT field derived from a
//! MEDIA field of kind audio, its files decoded (WAV here; FLAC, MP3, Ogg
//! Vorbis and AAC too) into one channel at the file's rate before the
//! model reads them; and what is refused. The test model names the pitch
//! it hears, so a file of a 440 Hz tone's text is "tone 440".
#![cfg(feature = "audio")]

use std::sync::Arc;

use base64::Engine;
use serde_json::json;
use strata::db::{Db, SqlResult};
use strata::derive::Deriver;
use strata::embed::{Audio, Input, Modality};
use strata::media::MediaKind;
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

/// A 16-bit PCM WAV file of `seconds` of a tone of `hz` at `rate`, in
/// `channels` channels: the first holds the tone and the others silence.
fn wav(hz: f32, rate: u32, channels: u16, seconds: f32) -> Vec<u8> {
    let frames = (rate as f32 * seconds) as u32;
    let mut pcm = Vec::new();
    for i in 0..frames {
        let x = (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5;
        pcm.extend(((x * 32767.0) as i16).to_le_bytes());
        for _ in 1..channels {
            pcm.extend(0i16.to_le_bytes());
        }
    }
    let mut out = Vec::new();
    out.extend(b"RIFF");
    out.extend((36 + pcm.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(channels.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * 2 * channels as u32).to_le_bytes());
    out.extend((2 * channels).to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend((pcm.len() as u32).to_le_bytes());
    out.extend(pcm);
    out
}

/// Names the pitch of a tone, to the nearest 10 Hz, from how often the
/// sound crosses zero.
struct Pitch;

fn pitch(a: &Audio) -> f32 {
    let crossings = a.samples.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
    (crossings as f32 / 2.0 / (a.samples.len() as f32 / a.rate as f32) / 10.0).round() * 10.0
}

impl Deriver for Pitch {
    fn name(&self) -> String {
        "pitch".into()
    }
    fn modality(&self) -> Modality {
        Modality::Audio
    }
    fn derive(&self, inputs: &[Input<'_>]) -> strata::Result<Vec<String>> {
        Ok(inputs
            .iter()
            .map(|i| match i {
                Input::Audio(a) => format!("tone {}", pitch(a)),
                _ => panic!("pitch hears audio only"),
            })
            .collect())
    }
}

fn file(hz: f32, rate: u32) -> String {
    json!({"data": base64::engine::general_purpose::STANDARD.encode(wav(hz, rate, 2, 0.5)), "uri": format!("mem:{hz}@{rate}")}).to_string()
}

fn schema(kind: MediaKind) -> Schema {
    let fields = vec![Field::new("id", FieldType::Keyword), Field::media("sound", kind), Field::new("heard", FieldType::Text { positions: true }).derived("pitch", "sound")];
    Schema::new("clips", fields, Some("id")).unwrap()
}

fn sql(db: &Db, q: &str) -> Vec<Vec<Value>> {
    match db.sql(q, &Params::new()).unwrap_or_else(|e| panic!("{q}: {e}")) {
        SqlResult::Rows(r) => r.rows,
        SqlResult::Written(_) => Vec::new(),
    }
}

fn heard(db: &Db) -> Vec<(String, Option<String>)> {
    let mut v: Vec<_> = sql(db, "SELECT id, heard FROM clips LIMIT 100").into_iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_str().map(String::from))).collect();
    v.sort();
    v
}

#[test]
fn rows_get_the_text_of_their_sounds() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::create(&dir.path().join("c.strata"), schema(MediaKind::Audio), DbOptions { sync: false, ..DbOptions::default() }).unwrap();
    db.set_deriver_in("clips", "heard", Arc::new(Pitch)).unwrap();
    // A load of more sounds than the model takes at once, at 16 kHz,
    // 44.1 kHz and 96 kHz (read at 48 kHz).
    let tones = [(220.0, 16_000), (440.0, 44_100), (880.0, 96_000), (330.0, 22_050), (550.0, 8_000), (660.0, 48_000)];
    let value = |f: &str| strata::media::input_text(&serde_json::from_str(f).unwrap()).unwrap();
    db.load(tones.iter().enumerate().map(|(i, (hz, rate))| vec![Value::Str(format!("c{i}")), Value::Str(value(&file(*hz, *rate))), Value::Null]).collect::<Vec<_>>()).unwrap();
    sql(&db, &format!("INSERT INTO clips (id, sound) VALUES ('s', '{}')", file(1000.0, 44_100)));
    let want: Vec<(String, Option<String>)> = tones.iter().enumerate().map(|(i, (hz, _))| (format!("c{i}"), Some(format!("tone {hz}")))).chain([("s".to_string(), Some("tone 1000".to_string()))]).collect();
    assert_eq!(heard(&db), want);
    let top = sql(&db, "SELECT id FROM clips ORDER BY bm25(heard, '880') DESC LIMIT 1");
    assert_eq!(top[0][0], Value::Str("c2".into()));
    // An UPDATE of the file makes the text again.
    sql(&db, &format!("UPDATE clips SET sound = '{}' WHERE id = 's'", file(250.0, 16_000)));
    assert_eq!(heard(&db).last().unwrap().1.as_deref(), Some("tone 250"));
    // A file that is not sound is refused, naming the field.
    let png = json!({"data": base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\n not really"), "uri": "mem:png"}).to_string();
    let e = db.sql(&format!("INSERT INTO clips (id, sound) VALUES ('p', '{png}')"), &Params::new()).unwrap_err().to_string();
    assert!(e.contains("sound"), "{e}");
}

#[test]
fn sounds_decode_to_one_channel() {
    // Stereo, the tone in one channel: the mean of the two.
    let a = strata::audio::decode(&wav(440.0, 22_050, 2, 0.5)).unwrap();
    assert_eq!((a.rate, a.samples.len()), (22_050, 11_025));
    let peak = a.samples.iter().fold(0f32, |m, x| m.max(x.abs()));
    assert!((peak - 0.25).abs() < 0.01, "{peak}");
    assert_eq!(pitch(&a), 440.0);
    // Above 48 kHz, resampled to it.
    let a = strata::audio::decode(&wav(440.0, 96_000, 1, 0.5)).unwrap();
    assert_eq!((a.rate, a.samples.len()), (48_000, 24_000));
    assert_eq!(pitch(&a), 440.0);
    assert!(strata::audio::decode(b"not a sound").is_err());
}

#[test]
fn an_audio_model_needs_a_field_of_audio() {
    let dir = tempfile::tempdir().unwrap();
    for kind in [MediaKind::Image, MediaKind::Any] {
        let db = Db::create(&dir.path().join(format!("{}.strata", kind.as_str())), schema(kind), DbOptions::default()).unwrap();
        let e = db.set_deriver_in("clips", "heard", Arc::new(Pitch)).unwrap_err().to_string();
        assert!(e.contains("declare kind = 'audio'"), "{e}");
    }
}
