//! Relevance judgments by a Claude model: how well each of a query's rows
//! answers it, graded 0 to 3, the scale of TREC's judgments and of the
//! relevance lab's grades.
//!
//! The prompt is UMBRELA's (Upadhyay et al., "UMBRELA: UMbrela is the
//! (Open-Source Reproduction of the) Bing RELevance Assessor", 2024), the
//! open reproduction of the prompt with which Thomas et al. (SIGIR 2024)
//! found an LLM agreeing with Bing's searchers as well as its human
//! labellers did, and the one TREC 2024's RAG track judged with. One
//! request per row, as UMBRELA asks: each row is graded alone, so a grade
//! does not depend on the other rows shown or their order. The row is its
//! stored text fields (or the fields named), each cut at
//! [`CHARS_PER_FIELD`] characters.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{json, Value as Json};
use strata::{FieldType, Value};

use crate::agent;
use crate::service::{QueryReq, Service};
use crate::ApiError;

/// Rows graded at once.
const CONCURRENCY: usize = 8;
/// Rows a request grades at most.
pub const MAX_ROWS: usize = 200;
/// Characters of each field a judgment sees.
pub const CHARS_PER_FIELD: usize = 4000;

pub struct JudgeReq {
    /// The table the rows are in (the first table when `None`).
    pub table: Option<String>,
    pub query: String,
    /// The rows' primary keys.
    pub keys: Vec<Json>,
    /// The fields shown to the model; the table's stored TEXT fields when
    /// empty.
    pub fields: Vec<String>,
    pub model: Option<String>,
    /// Overrides the server's key for this request (never stored).
    pub api_key: Option<String>,
}

/// What a judge is asked when its answer holds no grade.
const FOLLOW_UP: &str = "Provide only the final score, in the format: ##final score: score";

/// UMBRELA's prompt for a query and a passage.
pub fn prompt(query: &str, passage: &str) -> String {
    format!(
        "Given a query and a passage, you must provide a score on an integer scale of 0 to 3 with the following meanings:\n\
0 = represent that the passage has nothing to do with the query,\n\
1 = represents that the passage seems related to the query but does not answer it,\n\
2 = represents that the passage has some answer for the query, but the answer may be a bit unclear, or hidden amongst extraneous information and\n\
3 = represents that the passage is dedicated to the query and contains the exact answer.\n\n\
Important Instruction: Assign category 1 if the passage is somewhat related to the topic but not completely, category 2 if passage presents something very important related to the entire topic but also has some extra information and category 3 if the passage only and entirely refers to the topic. If none of the above satisfies give it category 0.\n\n\
Query: {query}\n\
Passage: {passage}\n\n\
Split this problem into steps:\n\
Consider the underlying intent of the search.\n\
Measure how well the content matches a likely intent of the query (M).\n\
Measure how trustworthy the passage is (T).\n\
Consider the aspects above and the relative importance of each, and decide on a final score (O). Final score must be an integer value only.\n\
Do not provide any code in result. Provide each score in the format of: ##final score: score without providing any reasoning."
    )
}

/// The grade in a model's answer: the digit after its last "final score",
/// or the answer's only digit from 0 to 3.
pub fn grade_of(answer: &str) -> Option<u8> {
    let lower = answer.to_lowercase();
    let digit = |s: &str| s.chars().find(|c| c.is_ascii_digit()).and_then(|c| c.to_digit(10)).filter(|&d| d <= 3).map(|d| d as u8);
    if let Some(i) = lower.rfind("final score") {
        return digit(&lower[i + "final score".len()..]);
    }
    let mut ds = lower.chars().filter(|c| c.is_ascii_digit());
    match (ds.next(), ds.next()) {
        (Some(c), None) => digit(&c.to_string()),
        _ => None,
    }
}

/// A row as a passage: its fields' values, each on a line after its name
/// when there are several.
fn passage(fields: &[String], row: &[Value]) -> String {
    let text = |v: &Value| -> String {
        let s = match v {
            Value::List(xs) => xs.iter().map(value_text).collect::<Vec<_>>().join(", "),
            v => value_text(v),
        };
        match s.char_indices().nth(CHARS_PER_FIELD) {
            Some((i, _)) => format!("{}…", &s[..i]),
            None => s,
        }
    };
    if fields.len() == 1 {
        return text(&row[0]);
    }
    fields.iter().zip(row).filter(|(_, v)| !matches!(v, Value::Null)).map(|(f, v)| format!("{f}: {}", text(v))).collect::<Vec<_>>().join("\n")
}

/// A value as text: a string as it is, anything else as JSON.
fn value_text(v: &Value) -> String {
    match crate::json::to_json(v) {
        Json::Null => String::new(),
        Json::String(s) => s,
        j => j.to_string(),
    }
}

fn quote_name(n: &str) -> String {
    format!("\"{}\"", n.replace('"', "\"\""))
}

/// The SQL literal of a key of the table.
fn key_literal(k: &Json, ty: &FieldType) -> Result<String, ApiError> {
    match (ty, k) {
        (FieldType::Int, Json::Number(n)) if n.is_i64() => Ok(n.to_string()),
        (FieldType::Int, Json::String(s)) => s.parse::<i64>().map(|x| x.to_string()).map_err(|_| ApiError::bad(format!("key {s:?} is not a whole number"))),
        (FieldType::Keyword, Json::String(s)) => Ok(format!("'{}'", s.replace('\'', "''"))),
        (FieldType::Keyword, Json::Number(n)) => Ok(format!("'{n}'")),
        (_, k) => Err(ApiError::bad(format!("key {k} is not a key of the table"))),
    }
}

pub async fn run(svc: &Arc<Service>, req: JudgeReq) -> Result<Json, ApiError> {
    let key = req.api_key.or_else(|| svc.agent.api_key.clone()).ok_or_else(|| ApiError {
        status: 400,
        message: "no Anthropic API key: start the server with ANTHROPIC_API_KEY set, or enter a key in the UI".into(),
    })?;
    if req.query.trim().is_empty() {
        return Err(ApiError::bad("judge needs a query"));
    }
    if req.keys.is_empty() || req.keys.len() > MAX_ROWS {
        return Err(ApiError::bad(format!("judge grades 1 to {MAX_ROWS} rows, not {}", req.keys.len())));
    }
    let model = req.model.unwrap_or_else(|| svc.agent.model.clone());
    let schema = match &req.table {
        Some(t) => svc.table(t).await?,
        None => Arc::new(svc.schema.clone()),
    };
    let pk = schema.primary_key.ok_or_else(|| ApiError::bad(format!("{} has no primary key to name its rows by", schema.table)))?;
    let fields: Vec<String> = match req.fields.is_empty() {
        true => schema.fields.iter().filter(|f| matches!(f.ty, FieldType::Text { .. }) && f.stored).map(|f| f.name.clone()).collect(),
        false => req.fields,
    };
    for f in &fields {
        let field = schema.field(f).ok_or_else(|| ApiError::bad(format!("{} has no field {f}", schema.table)))?;
        if !field.stored {
            return Err(ApiError::bad(format!("{f} is not stored, so a judge cannot read it")));
        }
    }
    if fields.is_empty() {
        return Err(ApiError::bad(format!("{} has no stored TEXT field: name the fields a judge reads", schema.table)));
    }
    // The rows' text, in one query.
    let lits = req.keys.iter().map(|k| key_literal(k, &schema.fields[pk].ty)).collect::<Result<Vec<_>, _>>()?;
    let key_name = &schema.fields[pk].name;
    let sql = format!(
        "SELECT {} FROM {} WHERE {} IN ({}) LIMIT {}",
        fields.iter().map(|f| quote_name(f)).collect::<Vec<_>>().join(", "),
        quote_name(&schema.table),
        quote_name(key_name),
        lits.join(", "),
        lits.len()
    );
    let a = svc.query(QueryReq { sql, ..Default::default() }).await?;
    let mut text = std::collections::HashMap::new();
    for (k, row) in a.rs.keys.iter().zip(&a.rs.rows) {
        text.insert(value_text(k), passage(&fields, row));
    }

    let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().map_err(ApiError::internal)?;
    let t0 = Instant::now();
    // Each request owns what it sends: a future that borrows across the
    // stream is not `Send` for every lifetime, as the router needs.
    let jobs: Vec<(String, Option<String>)> = req
        .keys
        .iter()
        .map(|k| {
            let name = match k {
                Json::String(s) => s.clone(),
                k => k.to_string(),
            };
            let p = text.get(&name).map(|t| prompt(&req.query, t));
            (name, p)
        })
        .collect();
    let base = svc.agent.base_url.clone();
    let done: Vec<_> = futures::stream::iter(jobs)
        .map(|(name, p)| {
            let (client, key, model, base) = (client.clone(), key.clone(), model.clone(), base.clone());
            async move {
                let Some(p) = p else { return (name, Err("no such row".to_string()), None) };
                let mut messages = vec![json!({"role": "user", "content": p})];
                let mut usage = Vec::new();
                // An answer that reasons instead of grading (one row in
                // seventy in the TREC-COVID study) is asked once for its
                // grade.
                for turn in 0..2 {
                    let body = json!({"model": model, "max_tokens": 1024, "messages": messages});
                    let r = match agent::call(&client, &base, &key, &body).await {
                        Ok(r) => r,
                        Err(e) => return (name, Err(e.message), (!usage.is_empty()).then_some(usage)),
                    };
                    usage.push(r["usage"].clone());
                    let answer: String = r["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                    if let Some(g) = grade_of(&answer) {
                        return (name, Ok(g), Some(usage));
                    }
                    if turn == 1 {
                        return (name, Err(format!("no grade in the answer {:?}", answer.chars().take(200).collect::<String>())), Some(usage));
                    }
                    messages.push(json!({"role": "assistant", "content": answer}));
                    messages.push(json!({"role": "user", "content": FOLLOW_UP}));
                }
                unreachable!("the loop returns")
            }
        })
        .buffered(CONCURRENCY)
        .collect()
        .await;
    let (mut input_tokens, mut output_tokens) = (0u64, 0u64);
    let mut grades = Vec::with_capacity(done.len());
    let mut failed = 0;
    for (k, g, usage) in done {
        for u in usage.iter().flatten() {
            input_tokens += u["input_tokens"].as_u64().unwrap_or(0);
            output_tokens += u["output_tokens"].as_u64().unwrap_or(0);
        }
        grades.push(match g {
            Ok(g) => json!({"key": k, "grade": g}),
            Err(e) => {
                failed += 1;
                json!({"key": k, "grade": null, "error": e})
            }
        });
    }
    // Every row failed alike (a bad key, an unknown model): an error.
    if failed == grades.len() {
        let first = grades[0]["error"].as_str().unwrap_or("").to_string();
        if grades.iter().all(|g| g["error"] == first.as_str()) && first != "no such row" {
            return Err(ApiError { status: 502, message: first });
        }
    }
    Ok(json!({
        "grades": grades,
        "model": model,
        "prompt": "umbrela",
        "fields": fields,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        "took_ms": t0.elapsed().as_secs_f64() * 1e3,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_are_read_from_answers() {
        assert_eq!(grade_of("##final score: 2"), Some(2));
        assert_eq!(grade_of("M: 3, T: 2\n##Final Score: 3"), Some(3));
        assert_eq!(grade_of("final score: 1 (the passage is related)"), Some(1));
        assert_eq!(grade_of("0"), Some(0));
        // Out of range, absent or ambiguous.
        assert_eq!(grade_of("##final score: 7"), None);
        assert_eq!(grade_of("I cannot judge this."), None);
        assert_eq!(grade_of("M 2 T 3"), None);
    }

    #[test]
    fn passages_name_their_fields_when_there_are_several() {
        let one = passage(&["body".into()], &[Value::Str("Tents for two".into())]);
        assert_eq!(one, "Tents for two");
        let two = passage(&["title".into(), "body".into()], &[Value::Str("Tent".into()), Value::Null]);
        assert_eq!(two, "title: Tent");
        let long = passage(&["body".into()], &[Value::Str("é".repeat(CHARS_PER_FIELD + 5))]);
        assert_eq!(long.chars().count(), CHARS_PER_FIELD + 1);
        assert!(prompt("tents", "Tents for two").contains("Query: tents\nPassage: Tents for two\n"));
    }
}
