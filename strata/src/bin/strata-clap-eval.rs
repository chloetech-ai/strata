//! Text-to-audio and audio-to-text retrieval by a model of sound and text,
//! through the database: Clotho's evaluation split, 1,045 recordings of
//! 15 to 30 seconds with five captions each.
//!
//! ```text
//! cargo run --release --features audio --bin strata-clap-eval -- \
//!     --audio ~/data/clotho/audio/evaluation --captions ~/data/clotho/clotho_captions_evaluation.csv \
//!     --out results/media/clotho-clap.json
//! ```
//!
//! The recordings load as a MEDIA field of kind audio read from their
//! directory (`file:` on the fetch list), whose vectors the database
//! computes as they load; the captions load into a second table with
//! their text vectors from the same model. Text-to-audio retrieval ranks
//! the recordings by each caption's vector, audio-to-text the captions by
//! each recording's; search is exact (flat). `--vectors-out` writes the
//! vectors, and `--reference` compares them with another implementation's
//! (`clap_reference.py`); `--wav-out` writes the first recordings as the
//! model hears them (one channel, 48 kHz, 32-bit float), for that
//! implementation to read.

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use serde_json::json;
use strata::db::Db;
use strata::media::{Allowlist, MediaKind, Resolver};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;
use strata::VectorIndexKind;

#[derive(Parser)]
struct Args {
    /// The directory of the split's recordings (WAV).
    #[arg(long)]
    audio: PathBuf,
    /// The split's captions: a CSV of `file_name` and `caption_1` to
    /// `caption_5`.
    #[arg(long)]
    captions: PathBuf,
    /// The model, by the name a schema gives it.
    #[arg(long, default_value = "clap-htsat-unfused")]
    model: String,
    /// Where models are (default $STRATA_MODELS or ~/.local/share/strata/models).
    #[arg(long)]
    models: Option<PathBuf>,
    /// Keep the first n recordings.
    #[arg(long)]
    limit: Option<usize>,
    /// Write the vectors as JSON here: {"audio": {file: vector}, "text":
    /// {caption: vector}}.
    #[arg(long)]
    vectors_out: Option<PathBuf>,
    /// Compare the vectors with these, as `--vectors-out` writes them.
    #[arg(long)]
    reference: Option<PathBuf>,
    /// Write the first `--wav-count` recordings here as the model hears
    /// them.
    #[arg(long)]
    wav_out: Option<PathBuf>,
    #[arg(long, default_value_t = 20)]
    wav_count: usize,
    /// Write the results as JSON here.
    #[arg(long)]
    out: Option<PathBuf>,
}

const KS: [usize; 3] = [1, 5, 10];

/// A WAV file of one channel of 32-bit float samples.
fn float_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data = samples.len() as u32 * 4;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend(b"RIFF");
    out.extend((36 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(3u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * 4).to_le_bytes());
    out.extend(4u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data.to_le_bytes());
    samples.iter().for_each(|x| out.extend(x.to_le_bytes()));
    out
}

/// A `file:` URI of a path, its bytes other than letters, digits and
/// `/-._~` percent-encoded (Clotho's file names hold `#`, `&` and `'`).
fn file_uri(path: &std::path::Path) -> String {
    let mut out = String::from("file:");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let n = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (n(a) * n(b)).max(1e-12)
}

fn main() {
    let a = Args::parse();
    let models = a.models.clone().unwrap_or_else(strata::embed::models_dir);
    let model = strata::embed::open(&a.model, &models).unwrap_or_else(|e| panic!("{e}"));
    let mut split: Vec<(String, Vec<String>)> = Vec::new();
    let mut r = csv::Reader::from_path(&a.captions).unwrap_or_else(|e| panic!("{}: {e}", a.captions.display()));
    let head = r.headers().expect("a header row").clone();
    let col = |name: &str| head.iter().position(|h| h == name).unwrap_or_else(|| panic!("no column {name}"));
    let (file, caps) = (col("file_name"), (1..=5).map(|i| col(&format!("caption_{i}"))).collect::<Vec<_>>());
    for row in r.records() {
        let row = row.expect("a CSV row");
        split.push((row[file].to_string(), caps.iter().map(|&c| row[c].to_string()).collect()));
    }
    split.sort();
    split.truncate(a.limit.unwrap_or(usize::MAX));
    let n_captions: usize = split.iter().map(|s| s.1.len()).sum();
    let dim = model.dim();
    eprintln!("{}: {} recordings, {n_captions} captions; model {} ({dim} dimensions)", a.captions.display(), split.len(), model.name());

    let root = std::fs::canonicalize(&a.audio).expect("the audio directory");
    if let Some(dir) = &a.wav_out {
        std::fs::create_dir_all(dir).expect("create the WAV directory");
        for (f, _) in split.iter().take(a.wav_count) {
            let s = strata::audio::decode(&std::fs::read(root.join(f)).unwrap()).expect("decode a recording");
            let x = strata::audio::resample(&s.samples, s.rate, 48_000);
            std::fs::write(dir.join(f), float_wav(&x, 48_000)).expect("write a WAV file");
        }
    }

    let clips = Schema::new(
        "clips",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::media("audio", MediaKind::Audio),
            Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat }).embedded(&a.model, &["audio"]),
        ],
        Some("id"),
    )
    .expect("valid schema");
    let captions = Schema::new(
        "captions",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("clip", FieldType::Keyword),
            Field::new("text", FieldType::Text { positions: false }),
            Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat }).embedded(&a.model, &["text"]),
        ],
        Some("id"),
    )
    .expect("valid schema");
    let dir = std::env::temp_dir().join(format!("strata-clap-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a temporary directory");
    let db = Db::create(&dir.join("eval.strata"), clips, DbOptions { sync: false, ..DbOptions::default() }).expect("create");
    db.create_table(captions).expect("create the captions table");
    db.set_embedder_in("clips", "emb", model.clone()).expect("the model embeds sound");
    db.set_embedder_in("captions", "emb", model.clone()).expect("the model fits the field");
    db.set_media_resolver(Resolver { allow: Allowlist::parse(&[format!("file:{}", root.display())]).unwrap(), ..Resolver::default() });

    let t0 = Instant::now();
    db.load_into("clips", split.iter().map(|(f, _)| vec![Value::Str(f.clone()), Value::Str(json!({"uri": file_uri(&root.join(f))}).to_string()), Value::Null]))
        .expect("load the recordings");
    let clips_s = t0.elapsed().as_secs_f64();
    let all = |sql: &str| db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows;
    let clip_rows = all(&format!("SELECT id, emb, audio FROM clips LIMIT {}", split.len()));
    let audio_s: f64 = clip_rows.iter().map(|r| serde_json::from_str::<serde_json::Value>(r[2].as_str().unwrap()).unwrap()["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0).sum();
    eprintln!("embedded {} recordings, {audio_s:.0} s of sound, in {clips_s:.1} s ({:.0} ms a recording)", split.len(), clips_s * 1000.0 / split.len() as f64);
    let t0 = Instant::now();
    let mut n = 0;
    db.load_into(
        "captions",
        split.iter().flat_map(|(f, cs)| cs.iter().map(move |c| (f, c))).map(|(f, c)| {
            n += 1;
            vec![Value::Str(format!("c{n}")), Value::Str(f.clone()), Value::Str(c.clone()), Value::Null]
        }),
    )
    .expect("load the captions");
    let captions_s = t0.elapsed().as_secs_f64();
    eprintln!("embedded {n_captions} captions in {captions_s:.1} s");
    let caption_rows = all(&format!("SELECT clip, emb, text FROM captions LIMIT {n_captions}"));

    let vec_of = |v: &Value| v.as_vector().unwrap().to_vec();
    let mut report = json!({
        "split": a.captions.file_name().map(|f| f.to_string_lossy().into_owned()), "recordings": split.len(), "captions": n_captions,
        "audio_seconds": audio_s, "model": a.model, "dimensions": dim, "machine": std::env::var("STRATA_MACHINE").ok(),
        "recordings_load_seconds": clips_s, "captions_load_seconds": captions_s,
    });
    if let Some(out) = &a.vectors_out {
        let audio: serde_json::Map<String, serde_json::Value> = clip_rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), json!(vec_of(&r[1])))).collect();
        let text: serde_json::Map<String, serde_json::Value> = caption_rows.iter().map(|r| (r[2].as_str().unwrap().to_string(), json!(vec_of(&r[1])))).collect();
        std::fs::write(out, serde_json::to_string(&json!({"audio": audio, "text": text})).unwrap()).expect("write the vectors");
    }
    if let Some(p) = &a.reference {
        let theirs: serde_json::Value = serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).expect("vectors as JSON");
        let compare = |what: &str, rows: &[Vec<Value>], key: usize| -> serde_json::Value {
            let mut cos: Vec<f32> = rows
                .iter()
                .filter_map(|r| {
                    let v: Vec<f32> = serde_json::from_value(theirs[what].get(r[key].as_str()?)?.clone()).ok()?;
                    Some(cosine(&vec_of(&r[1]), &v))
                })
                .collect();
            cos.sort_by(f32::total_cmp);
            let mean = cos.iter().sum::<f32>() / cos.len().max(1) as f32;
            let least = cos.first().copied().unwrap_or(f32::NAN);
            println!("{what:<6} vectors against the reference: {} compared, cosine mean {mean:.7}, least {least:.7} (1 - cosine at most {:.1e})", cos.len(), 1.0 - least);
            json!({"compared": cos.len(), "cosine_mean": mean, "cosine_least": cos.first()})
        };
        let (audio, text) = (compare("audio", &clip_rows, 0), compare("text", &caption_rows, 2));
        report.as_object_mut().unwrap().insert("reference".into(), json!({"file": p.file_name().map(|f| f.to_string_lossy().into_owned()), "audio": audio, "text": text}));
    }

    // Text to audio: the recording of each caption; audio to text: any of
    // a recording's five captions.
    let t0 = Instant::now();
    let rank_of = |sql: &str, q: &Value, hit: &dyn Fn(&[Vec<Value>]) -> Option<usize>| -> Option<usize> {
        let got = db.query(sql, &Params::new().set("q", q.clone())).unwrap_or_else(|e| panic!("{sql}: {e}")).rows;
        hit(&got)
    };
    let t2a: Vec<Option<usize>> = caption_rows.iter().map(|r| rank_of("SELECT id FROM clips ORDER BY similarity(emb, :q) DESC LIMIT 10", &r[1], &|got| got.iter().position(|g| g[0] == r[0]))).collect();
    let a2t: Vec<Option<usize>> = clip_rows.iter().map(|r| rank_of("SELECT clip FROM captions ORDER BY similarity(emb, :q) DESC LIMIT 10", &r[1], &|got| got.iter().position(|g| g[0] == r[0]))).collect();
    let search_s = t0.elapsed().as_secs_f64();
    let recall = |ranks: &[Option<usize>]| -> serde_json::Map<String, serde_json::Value> {
        let mut m: serde_json::Map<String, serde_json::Value> = KS.iter().map(|&k| (format!("R@{k}"), json!(ranks.iter().filter(|r| r.is_some_and(|r| r < k)).count() as f64 / ranks.len() as f64))).collect();
        m.insert("mAP@10".into(), json!(ranks.iter().map(|r| r.map_or(0.0, |r| 1.0 / (r + 1) as f64)).sum::<f64>() / ranks.len() as f64));
        m
    };
    println!("{:<16} {:>7} {:>7} {:>7} {:>7}", "clotho", "R@1", "R@5", "R@10", "mAP@10");
    let o = report.as_object_mut().unwrap();
    for (name, ranks) in [("text_to_audio", &t2a), ("audio_to_text", &a2t)] {
        let r = recall(ranks);
        println!("{name:<16} {:>7.4} {:>7.4} {:>7.4} {:>7.4}", r["R@1"].as_f64().unwrap(), r["R@5"].as_f64().unwrap(), r["R@10"].as_f64().unwrap(), r["mAP@10"].as_f64().unwrap());
        o.insert(name.into(), json!(r));
    }
    o.insert("search_seconds".into(), json!(search_s));
    if let Some(out) = &a.out {
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).expect("create the output directory");
        }
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap()).expect("write the results");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
