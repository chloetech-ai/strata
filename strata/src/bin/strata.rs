//! Strata command-line shell: run SQL against a database file.
//!
//! strata <file> "SELECT ..."             run one statement
//! strata <file> < queries.sql            run statements from stdin, one per line or ';'-terminated
//!   --param name=value                   bind :name (a number, a string, or a JSON array as a vector)
//!   --plan                               print the planner's choices
//!   --repeat N                           run each statement N times and report the median time
//!   .schema / .segments                  print the schema / the snapshot's segments

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use strata::db::Db;
use strata::query::sql::Params;
use strata::storage::DbOptions;
use strata::Value;

#[derive(Parser)]
#[command(name = "strata", version, about = "Run SQL against a Strata database file")]
struct Args {
    /// Database file.
    file: PathBuf,
    /// Statement to run; without one, statements are read from stdin.
    sql: Option<String>,
    /// Bind a placeholder: name=value (number, string, or JSON array for a vector).
    #[arg(long = "param", value_name = "NAME=VALUE")]
    params: Vec<String>,
    /// Print the plan lines of each statement.
    #[arg(long)]
    plan: bool,
    /// Run each statement this many times and report the median.
    #[arg(long, default_value_t = 1)]
    repeat: usize,
}

fn parse_value(s: &str) -> Value {
    if s.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Vec<f32>>(s) {
            return Value::Vector(Arc::from(v));
        }
    }
    if let Ok(i) = s.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return Value::Float(f);
    }
    Value::Str(s.to_string())
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Int(x) => x.to_string(),
        Value::Float(x) => format!("{x:.4}"),
        Value::Str(s) => {
            let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if one_line.chars().count() > 60 {
                format!("{}…", one_line.chars().take(59).collect::<String>())
            } else {
                one_line
            }
        }
        Value::Vector(v) => format!("vector({})", v.len()),
    }
}

fn run(db: &Db, sql: &str, params: &Params, args: &Args, out: &mut impl Write) -> io::Result<()> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    match trimmed {
        ".schema" => return writeln!(out, "{}", serde_json::to_string_pretty(db.schema()).unwrap_or_default()),
        ".segments" => {
            let snap = db.snapshot();
            for s in snap.segments() {
                writeln!(out, "segment {}: {} live docs{}", s.id, s.n_live, if s.in_memory { " (memtable)" } else { "" })?;
            }
            return Ok(());
        }
        _ => {}
    }
    let mut times = Vec::with_capacity(args.repeat.max(1));
    let mut result = None;
    for _ in 0..args.repeat.max(1) {
        let t = Instant::now();
        let r = db.query(trimmed, params);
        times.push(t.elapsed().as_secs_f64() * 1e3);
        result = Some(r);
    }
    match result.unwrap() {
        Err(e) => writeln!(out, "error: {e}"),
        Ok(rs) => {
            writeln!(out, "{}", rs.columns.join(" | "))?;
            for row in &rs.rows {
                writeln!(out, "{}", row.iter().map(show).collect::<Vec<_>>().join(" | "))?;
            }
            if args.plan {
                for line in &rs.stats.plan {
                    writeln!(out, "  plan: {line}")?;
                }
            }
            times.sort_by(|a, b| a.total_cmp(b));
            writeln!(out, "({} row{}, {:.3} ms)", rs.rows.len(), if rs.rows.len() == 1 { "" } else { "s" }, times[times.len() / 2])
        }
    }
}

fn main() {
    let args = Args::parse();
    let db = match Db::open(&args.file, DbOptions::default()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("strata: cannot open {}: {e}", args.file.display());
            std::process::exit(1);
        }
    };
    let mut params = Params::new();
    for p in &args.params {
        let Some((name, value)) = p.split_once('=') else {
            eprintln!("strata: --param expects NAME=VALUE, got {p}");
            std::process::exit(2);
        };
        params = params.set(name, parse_value(value));
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if let Some(sql) = &args.sql {
        run(&db, sql, &params, &args, &mut out).ok();
        return;
    }
    // Statements are ';'-terminated when the input has any ';', else one per line.
    let mut input = String::new();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let l = line.trim();
        if l.starts_with('.') {
            run(&db, l, &params, &args, &mut out).ok();
        } else if !l.is_empty() && !l.starts_with("--") {
            input.push_str(l);
            input.push('\n');
        }
    }
    let sep = if input.contains(';') { ';' } else { '\n' };
    for stmt in input.split(sep) {
        run(&db, &stmt.replace('\n', " "), &params, &args, &mut out).ok();
    }
}
