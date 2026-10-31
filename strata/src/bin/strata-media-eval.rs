//! Image and text retrieval by an image model's vectors, through the
//! database, against the recall its authors report: Flickr30K's 1,000
//! test images and COCO's 5,000 (Karpathy's splits), five captions each.
//!
//! ```text
//! cargo run --release --features vision --bin strata-media-eval -- \
//!     --csv ~/data/flickr1k/test_1k_flickr.csv --images ~/data/flickr1k/images_flickr_1k_test --out results/media/flickr1k-siglip.json
//! ```
//!
//! The CSV is a split as nlphuji's datasets on Hugging Face give it
//! (`flickr_1k_test_image_text_retrieval`,
//! `mscoco_2014_5k_test_image_text_retrieval`): a row per image, its
//! `filename` and its captions as a JSON list in `raw`. The images load as
//! a MEDIA field read from their directory (`file:` on the fetch list),
//! whose vectors the database computes as they load; the captions load
//! into a second table with their text vectors. Text-to-image retrieval
//! ranks the images by each caption's vector, image-to-text the captions
//! by each image's; search is exact (flat), so that the numbers are of
//! the vectors, not of an index.
//!
//! With `--captioner blip-image-captioning-base`, the images table also
//! gets a caption of each image, made by that model as the images load
//! (a derived TEXT field), and each direction is measured four ways: by
//! the vectors alone, by BM25 alone (a caption against the images'
//! captions, or an image's caption against the captions), and by both,
//! fused by `rrf` and by `w * minmax(similarity) + (1 - w) * minmax(bm25)`
//! for w of 0.8, 0.9 and 0.95. `--captions-out` keeps the captions, and
//! `--captions` gives them back with the rows of a later run, which then
//! runs no captioner for them.

use std::collections::HashMap;
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
    /// The split: a CSV with `filename` and `raw` (the captions).
    #[arg(long)]
    csv: PathBuf,
    /// The directory of its images.
    #[arg(long)]
    images: PathBuf,
    /// The model, by the name a schema gives it.
    #[arg(long, default_value = "siglip-base-patch16-224")]
    model: String,
    /// Where models are (default $STRATA_MODELS or ~/.local/share/strata/models).
    #[arg(long)]
    models: Option<PathBuf>,
    /// Keep the first n images.
    #[arg(long)]
    limit: Option<usize>,
    /// Caption the images with this model (a derived field), and measure
    /// BM25 over the captions, alone and fused with the vectors.
    #[arg(long)]
    captioner: Option<String>,
    /// Write the images' captions as JSON (file name to caption) here.
    #[arg(long)]
    captions_out: Option<PathBuf>,
    /// Give the images' captions with their rows, from JSON as
    /// `--captions-out` writes it: a derived field keeps the text a write
    /// gives, so the captioner makes only those of images the file lacks.
    #[arg(long)]
    captions: Option<PathBuf>,
    /// Write the results as JSON here.
    #[arg(long)]
    out: Option<PathBuf>,
}

const KS: [usize; 3] = [1, 5, 10];

fn main() {
    let a = Args::parse();
    let models = a.models.clone().unwrap_or_else(strata::embed::models_dir);
    let model = strata::embed::open(&a.model, &models).unwrap_or_else(|e| panic!("{e}"));
    let mut split: Vec<(String, Vec<String>)> = Vec::new();
    let mut r = csv::Reader::from_path(&a.csv).unwrap_or_else(|e| panic!("{}: {e}", a.csv.display()));
    let head = r.headers().expect("a header row").clone();
    let col = |name: &str| head.iter().position(|h| h == name).unwrap_or_else(|| panic!("no column {name}"));
    let (file, raw) = (col("filename"), col("raw"));
    for row in r.records() {
        let row = row.expect("a CSV row");
        let captions: Vec<String> = serde_json::from_str(&row[raw]).expect("raw is a JSON list of captions");
        split.push((row[file].to_string(), captions));
    }
    split.truncate(a.limit.unwrap_or(usize::MAX));
    let n_captions: usize = split.iter().map(|s| s.1.len()).sum();
    eprintln!("{}: {} images, {n_captions} captions; model {} ({} dimensions)", a.csv.display(), split.len(), model.name(), model.dim());

    let dim = model.dim();
    let captioner = a.captioner.as_ref().map(|c| (c.clone(), strata::derive::open(c, &models).unwrap_or_else(|e| panic!("{e}"))));
    let mut fields = vec![Field::new("id", FieldType::Keyword), Field::media("image", MediaKind::Image)];
    if let Some((c, _)) = &captioner {
        fields.push(Field::new("caption", FieldType::Text { positions: false }).derived(c, "image"));
    }
    fields.push(Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat }).embedded(&a.model, &["image"]));
    let images = Schema::new("images", fields, Some("id")).expect("valid schema");
    let captions = Schema::new(
        "captions",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("img", FieldType::Keyword),
            Field::new("text", FieldType::Text { positions: false }),
            Field::new("emb", FieldType::Vector { dim, index: VectorIndexKind::Flat }).embedded(&a.model, &["text"]),
        ],
        Some("id"),
    )
    .expect("valid schema");
    let dir = std::env::temp_dir().join(format!("strata-media-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a temporary directory");
    let db = Db::create(&dir.join("eval.strata"), images, DbOptions { sync: false, ..DbOptions::default() }).expect("create");
    db.create_table(captions).expect("create the captions table");
    db.set_embedder_in("images", "emb", model.clone()).expect("the model embeds images");
    db.set_embedder_in("captions", "emb", model.clone()).expect("the model fits the field");
    if let Some((_, d)) = &captioner {
        db.set_deriver_in("images", "caption", d.clone()).expect("the model captions images");
    }
    let root = std::fs::canonicalize(&a.images).expect("the images directory");
    db.set_media_resolver(Resolver { allow: Allowlist::parse(&[format!("file:{}", root.display())]).unwrap(), ..Resolver::default() });

    let given: HashMap<String, String> = match &a.captions {
        Some(p) => serde_json::from_slice(&std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).expect("captions as a JSON object of file name to caption"),
        None => HashMap::new(),
    };
    let t0 = Instant::now();
    db.load_into(
        "images",
        split.iter().map(|(f, _)| {
            let mut row = vec![Value::Str(f.clone()), Value::Str(json!({"uri": format!("file:{}", root.join(f).display())}).to_string())];
            if captioner.is_some() {
                row.push(given.get(f).map_or(Value::Null, |c| Value::Str(c.clone())));
            }
            row.push(Value::Null);
            row
        }),
    )
    .expect("load the images");
    let images_s = t0.elapsed().as_secs_f64();
    let made = if captioner.is_some() { split.iter().filter(|(f, _)| !given.contains_key(f)).count() } else { 0 };
    let what = if made > 0 { format!("captioned {made} and embedded") } else { "embedded".into() };
    eprintln!("read, described, {what} {} images in {images_s:.1} s ({:.0} ms an image)", split.len(), images_s * 1000.0 / split.len() as f64);
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

    // The vectors the database holds, as queries.
    let all = |sql: &str| db.query(sql, &Params::new()).unwrap_or_else(|e| panic!("{sql}: {e}")).rows;
    let caption_col = if captioner.is_some() { "caption" } else { "''" };
    let image_rows = all(&format!("SELECT id, emb, {caption_col} FROM images LIMIT {}", split.len()));
    let caption_rows = all(&format!("SELECT img, emb, text FROM captions LIMIT {n_captions}"));
    if let Some(out) = &a.captions_out {
        let mut made: Vec<(&str, &str)> = image_rows.iter().map(|r| (r[0].as_str().unwrap(), r[2].as_str().unwrap_or(""))).collect();
        made.sort();
        let made: serde_json::Map<String, serde_json::Value> = made.into_iter().map(|(f, c)| (f.to_string(), json!(c))).collect();
        std::fs::write(out, serde_json::to_string_pretty(&made).unwrap()).expect("write the captions");
    }
    // A query by an image file gives the vector its row holds.
    let (id, stored) = (image_rows[0][0].as_str().unwrap(), image_rows[0][1].as_vector().unwrap());
    let fresh = strata::embed::embed_file(model.as_ref(), &std::fs::read(root.join(id)).unwrap()).expect("embed an image file");
    let cos: f32 = fresh.iter().zip(stored).map(|(x, y)| x * y).sum();
    assert!(cos > 0.999, "a query by {id} gives its row's vector (cosine {cos})");

    let ranks = |sql: &str, rows: &[Vec<Value>], hit: &dyn Fn(&Value, &[Vec<Value>]) -> Option<usize>| -> Vec<Option<usize>> {
        rows.iter()
            .map(|row| {
                let p = Params::new().set("q", row[1].clone()).set("t", row[2].clone());
                let got = db.query(sql, &p).unwrap_or_else(|e| panic!("{sql}: {e}")).rows;
                hit(&row[0], &got)
            })
            .collect()
    };
    let recall = |ranks: &[Option<usize>]| -> HashMap<String, f64> { KS.iter().map(|&k| (format!("R@{k}"), ranks.iter().filter(|r| r.is_some_and(|r| r < k)).count() as f64 / ranks.len() as f64)).collect() };
    // How each direction ranks: by the vectors, and with captions by BM25
    // and both fused, by rrf and by weights of the vectors' 0.8, 0.9 and
    // 0.95.
    let mut ways = vec![("".to_string(), "similarity(emb, :q)".to_string())];
    if captioner.is_some() {
        ways.extend([("_bm25", "bm25(TEXT, :t)"), ("_rrf", "rrf(similarity(emb, :q), bm25(TEXT, :t))")].map(|(s, o)| (s.to_string(), o.to_string())));
        for w in [0.8, 0.9, 0.95] {
            ways.push((format!("_weighted_{w}"), format!("{w} * minmax(similarity(emb, :q)) + {:.2} * minmax(bm25(TEXT, :t))", 1.0 - w)));
        }
    }
    let t0 = Instant::now();
    let mut results = serde_json::Map::new();
    println!("{:<28} {:>7} {:>7} {:>7}", a.csv.file_stem().unwrap().to_string_lossy(), "R@1", "R@5", "R@10");
    for (name, table, key, text, rows) in [("text_to_image", "images", "id", "caption", &caption_rows), ("image_to_text", "captions", "img", "text", &image_rows)] {
        for (suffix, order) in &ways {
            let sql = format!("SELECT {key} FROM {table} ORDER BY {} DESC LIMIT 10", order.replace("TEXT", text));
            let r = recall(&ranks(&sql, rows, &|img, got| got.iter().position(|r| r[0] == *img)));
            println!("{:<28} {:>7.4} {:>7.4} {:>7.4}", format!("{name}{suffix}"), r["R@1"], r["R@5"], r["R@10"]);
            results.insert(format!("{name}{suffix}"), json!(r));
        }
    }
    let search_s = t0.elapsed().as_secs_f64();
    eprintln!("{} searches in {search_s:.1} s", (caption_rows.len() + image_rows.len()) * ways.len());
    if let Some(out) = &a.out {
        let report = json!({
            "split": a.csv.file_name().map(|f| f.to_string_lossy().into_owned()), "images": split.len(), "captions": n_captions,
            "model": a.model, "dimensions": dim, "captioner": a.captioner, "machine": std::env::var("STRATA_MACHINE").ok(),
            "images_load_seconds": images_s, "captions_load_seconds": captions_s, "search_seconds": search_s,
            "captioned": made, "captions_given": if captioner.is_some() { split.len() - made } else { 0 },
        });
        let mut report = report;
        report.as_object_mut().unwrap().extend(results);
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p).expect("create the output directory");
        }
        std::fs::write(out, serde_json::to_string_pretty(&report).unwrap()).expect("write the results");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
