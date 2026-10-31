//! Transcripts of speech made by the database, against what people said:
//! LibriSpeech's `test-clean` read into a table whose `TEXT` field is a
//! Whisper model's transcript of each row's `MEDIA` file.
//!
//! ```text
//! cargo run --release --features audio --bin strata-asr-eval -- \
//!     --librispeech ~/data/librispeech/LibriSpeech/test-clean --limit 200 --out results/media/librispeech-whisper-small.json
//! ```
//!
//! The utterances are `--limit` of the split's, spread evenly over it by
//! ID (every 13th of `test-clean`'s 2,620 for 200), so that every speaker
//! is in. The transcripts are made as the rows load (`"derive": {"from": "audio", "model":
//! "whisper-small"}`) and read back through SQL. Word error rate is over
//! words as [`words`] normalizes them, for the transcripts and, with
//! `--reference`, for another model's (the `transformers` library's, as
//! `whisper_reference.py` writes them), with how many of the two agree
//! word for word. Then each utterance is searched for by what was said,
//! `ORDER BY bm25(transcript, :t)`, and ranked among the others.
//! `--transcripts` gives the transcripts of an earlier run
//! (`--transcripts-out`) with the rows, so that the model makes none.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use serde_json::json;
use strata::db::Db;
use strata::media::{Allowlist, MediaKind, Resolver};
use strata::query::Params;
use strata::schema::{Field, FieldType, Schema};
use strata::storage::DbOptions;
use strata::value::Value;

#[derive(Parser)]
struct Args {
    /// A LibriSpeech split: `<speaker>/<chapter>/` directories of FLAC
    /// files and their `.trans.txt`.
    #[arg(long)]
    librispeech: PathBuf,
    /// The model, by the name a schema gives it.
    #[arg(long, default_value = "whisper-small")]
    model: String,
    /// Where models are (default $STRATA_MODELS or ~/.local/share/strata/models).
    #[arg(long)]
    models: Option<PathBuf>,
    /// Keep n utterances, spread evenly over the split by ID.
    #[arg(long, default_value_t = 200)]
    limit: usize,
    /// Give these transcripts (JSON, utterance ID to text) with the rows:
    /// a derived field keeps the text a write gives.
    #[arg(long)]
    transcripts: Option<PathBuf>,
    /// Another model's transcripts (JSON, utterance ID to text), to score
    /// alike and compare.
    #[arg(long)]
    reference: Option<PathBuf>,
    /// Write the transcripts as JSON (utterance ID to text) here.
    #[arg(long)]
    transcripts_out: Option<PathBuf>,
    /// Write the results as JSON here.
    #[arg(long)]
    out: Option<PathBuf>,
}

const KS: [usize; 3] = [1, 5, 10];

/// The utterances of a split, by ID: ID, file, what was said.
fn utterances(split: &Path) -> Vec<(String, PathBuf, String)> {
    let mut out = Vec::new();
    let dirs = |p: &Path| -> Vec<PathBuf> {
        let mut d: Vec<PathBuf> = std::fs::read_dir(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).map(|e| e.unwrap().path()).collect();
        d.sort();
        d
    };
    for speaker in dirs(split).into_iter().filter(|p| p.is_dir()) {
        for chapter in dirs(&speaker).into_iter().filter(|p| p.is_dir()) {
            for t in dirs(&chapter).into_iter().filter(|p| p.to_string_lossy().ends_with(".trans.txt")) {
                for line in std::fs::read_to_string(&t).unwrap().lines() {
                    if let Some((id, text)) = line.split_once(' ') {
                        out.push((id.to_string(), chapter.join(format!("{id}.flac")), text.to_string()));
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Lower case words: letters, digits and apostrophes inside a word; the
/// rest is space. Simpler than Whisper's English normalizer (which also
/// spells out `Mr.` and numbers), so the error rates are higher than the
/// ones its authors report.
fn words(s: &str) -> Vec<String> {
    let s: String = s.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' }).collect();
    s.split_whitespace().map(|w| w.trim_matches('\'').to_string()).filter(|w| !w.is_empty()).collect()
}

/// Edit distance between two word sequences.
fn edits(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let next = (prev + (x != y) as usize).min(row[j] + 1).min(row[j + 1] + 1);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

/// Word error rate of `hyp` against `truth` over the utterances both have,
/// and how many match word for word.
fn wer(truth: &HashMap<String, String>, hyp: &HashMap<String, String>) -> (f64, usize, usize) {
    let (mut errors, mut total, mut exact, mut n) = (0, 0, 0, 0);
    for (id, t) in truth {
        if let Some(h) = hyp.get(id) {
            let (t, h) = (words(t), words(h));
            let e = edits(&t, &h);
            errors += e;
            total += t.len();
            exact += (e == 0) as usize;
            n += 1;
        }
    }
    (errors as f64 / total.max(1) as f64, exact, n)
}

fn main() {
    let a = Args::parse();
    let models = a.models.clone().unwrap_or_else(strata::embed::models_dir);
    let all = utterances(&a.librispeech);
    let step = (all.len() / a.limit.max(1)).max(1);
    let split: Vec<_> = all.into_iter().step_by(step).take(a.limit).collect();
    eprintln!("{}: {} utterances; model {}", a.librispeech.display(), split.len(), a.model);
    let model = strata::derive::open(&a.model, &models).unwrap_or_else(|e| panic!("{e}"));

    let schema = Schema::new(
        "utterances",
        vec![Field::new("id", FieldType::Keyword), Field::media("audio", MediaKind::Audio), Field::new("transcript", FieldType::Text { positions: false }).derived(&a.model, "audio")],
        Some("id"),
    )
    .expect("valid schema");
    let dir = std::env::temp_dir().join(format!("strata-asr-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a temporary directory");
    let db = Db::create(&dir.join("eval.strata"), schema, DbOptions { sync: false, ..DbOptions::default() }).expect("create");
    db.set_deriver_in("utterances", "transcript", model).expect("the model transcribes audio");
    let root = std::fs::canonicalize(&a.librispeech).expect("the split's directory");
    db.set_media_resolver(Resolver { allow: Allowlist::parse(&[format!("file:{}", root.display())]).unwrap(), ..Resolver::default() });

    let given: HashMap<String, String> = match &a.transcripts {
        Some(p) => serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).expect("transcripts as a JSON object of ID to text"),
        None => HashMap::new(),
    };
    let t0 = Instant::now();
    db.load_into(
        "utterances",
        split.iter().map(|(id, f, _)| vec![Value::Str(id.clone()), Value::Str(json!({"uri": format!("file:{}", f.display())}).to_string()), given.get(id).map_or(Value::Null, |t| Value::Str(t.clone()))]),
    )
    .expect("load the utterances");
    let made_n = split.iter().filter(|(id, _, _)| !given.contains_key(id)).count();
    let load_s = t0.elapsed().as_secs_f64();
    let rows = db.query(&format!("SELECT id, transcript, audio FROM utterances LIMIT {}", split.len()), &Params::new()).expect("read the transcripts").rows;
    let audio_s: f64 = rows.iter().map(|r| serde_json::from_str::<serde_json::Value>(r[2].as_str().unwrap()).unwrap()["duration_ms"].as_f64().unwrap_or(0.0) / 1000.0).sum();
    eprintln!("transcribed {made_n} of {} utterances, {audio_s:.0} s of speech, in {load_s:.1} s ({:.2} s a second of speech)", rows.len(), load_s / audio_s);

    let truth: HashMap<String, String> = split.iter().map(|(id, _, t)| (id.clone(), t.clone())).collect();
    let made: HashMap<String, String> = rows.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_str().unwrap_or("").to_string())).collect();
    if let Some(out) = &a.transcripts_out {
        let sorted: std::collections::BTreeMap<_, _> = made.iter().collect();
        std::fs::write(out, serde_json::to_string_pretty(&sorted).unwrap()).expect("write the transcripts");
    }
    let (w, exact, n) = wer(&truth, &made);
    println!("{:<44} {:>7.2}%  {exact} of {n} exact", "WER, strata", w * 100.0);
    let mut report = json!({
        "split": a.librispeech.file_name().map(|f| f.to_string_lossy().into_owned()), "utterances": split.len(), "audio_seconds": audio_s,
        "model": a.model, "machine": std::env::var("STRATA_MACHINE").ok(), "load_seconds": load_s,
        "wer": w, "exact": exact, "transcribed": made_n, "transcripts_given": split.len() - made_n,
    });
    if let Some(p) = &a.reference {
        let theirs: HashMap<String, String> = serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).expect("transcripts as a JSON object of ID to text");
        let (rw, rexact, rn) = wer(&truth, &theirs);
        println!("{:<44} {:>7.2}%  {rexact} of {rn} exact", "WER, reference", rw * 100.0);
        // The same text, word for word, after normalizing.
        let same = made.iter().filter(|(id, t)| theirs.get(*id).is_some_and(|r| words(r) == words(t))).count();
        let (between, _, _) = wer(&theirs, &made);
        println!("{:<44} {same} of {rn}, {:.2}% of words differ", "strata and the reference agree", between * 100.0);
        let o = report.as_object_mut().unwrap();
        o.insert("reference".into(), json!({"file": p.file_name().map(|f| f.to_string_lossy().into_owned()), "utterances": rn, "wer": rw, "exact": rexact, "same_as_strata": same, "word_difference": between}));
    }

    // Each utterance found by what was said, among the others.
    let t0 = Instant::now();
    let sql = "SELECT id FROM utterances ORDER BY bm25(transcript, :t) DESC LIMIT 10";
    let ranks: Vec<Option<usize>> = split
        .iter()
        .map(|(id, _, said)| {
            let got = db.query(sql, &Params::new().set("t", Value::Str(words(said).join(" ")))).unwrap_or_else(|e| panic!("{sql}: {e}")).rows;
            let rank = got.iter().position(|r| r[0].as_str() == Some(id));
            if rank != Some(0) {
                eprintln!("{id} at {}: {said}", rank.map_or("none of the first 10".into(), |r| format!("rank {}", r + 1)));
            }
            rank
        })
        .collect();
    let recall: serde_json::Map<String, serde_json::Value> = KS.iter().map(|&k| (format!("R@{k}"), json!(ranks.iter().filter(|r| r.is_some_and(|r| r < k)).count() as f64 / ranks.len() as f64))).collect();
    println!("{:<44} {:>7.4} {:>7.4} {:>7.4}  (R@1, R@5, R@10)", "found by what was said, bm25(transcript)", recall["R@1"].as_f64().unwrap(), recall["R@5"].as_f64().unwrap(), recall["R@10"].as_f64().unwrap());
    let o = report.as_object_mut().unwrap();
    o.insert("search_by_reference".into(), json!(recall));
    o.insert("search_seconds".into(), json!(t0.elapsed().as_secs_f64()));
    if let Some(out) = &a.out {
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).expect("create the output directory");
        }
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap()).expect("write the results");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
