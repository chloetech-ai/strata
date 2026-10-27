//! "Ask an agent": a Claude model answers a question about the table by
//! writing and running read-only SQL against it.
//!
//! A plain tool-use loop over the Messages API (raw HTTP: there is no
//! official Rust SDK). The model gets the table's schema and Strata's SQL
//! dialect in the system prompt and one tool, `run_sql`; it queries until it
//! can answer, then answers in words and names the SQL it relied on. The
//! answer comes back with every query the model ran, so the UI can show
//! them and open any of them in the editor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use strata::FieldType;

use crate::service::{QueryReq, Service};
use crate::{json as js, ApiError};

/// The default model; any model id the API accepts can be passed instead.
pub const DEFAULT_MODEL: &str = "claude-haiku-5-5";
/// Offered by the UI.
pub const MODELS: [&str; 3] = ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"];
const API_VERSION: &str = "2026-10-21";
/// Rows of a query result the model sees.
const ROWS_SHOWN: usize = 30;
/// Characters of a string value the model sees.
const CHARS_SHOWN: usize = 300;

pub struct AgentReq {
    pub question: String,
    pub model: Option<String>,
    /// low, medium, high, xhigh or max; the model's default when unset.
    pub effort: Option<String>,
    /// Overrides the server's key for this request (never stored).
    pub api_key: Option<String>,
    /// Model calls at most.
    pub max_steps: usize,
}

fn field_line(svc: &Service, f: &strata::Field, i: usize) -> String {
    let ty = match &f.ty {
        FieldType::Text { .. } => "TEXT".to_string(),
        FieldType::Keyword => "KEYWORD".to_string(),
        FieldType::Int => "INT".to_string(),
        FieldType::Float => "FLOAT".to_string(),
        FieldType::Vector { dim, .. } => format!("VECTOR({dim})"),
        FieldType::Edges => "EDGES".to_string(),
        FieldType::Sparse => "SPARSE (learned term weights)".to_string(),
    };
    let mut notes = Vec::new();
    if svc.schema.primary_key == Some(i) {
        notes.push("primary key".to_string());
    }
    if f.multi {
        notes.push("multi-valued: a row matches if any of its values does".into());
    }
    if matches!(f.ty, FieldType::Text { .. }) {
        if !f.indexed {
            notes.push("returned, not searchable".into());
        } else if !f.stored {
            notes.push("searchable, not returned".into());
        }
        if !f.generated_from.is_empty() {
            notes.push(format!("indexes {}", f.generated_from.join(" + ")));
        }
    }
    let notes = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join("; ")) };
    format!("- {} {ty}{notes}", f.name)
}

fn system_prompt(svc: &Service, docs: u64) -> String {
    let s = &svc.schema;
    let fields: Vec<String> = s.fields.iter().enumerate().map(|(i, f)| field_line(svc, f, i)).collect();
    let vf = svc.vector_field();
    let vector_help = match (&vf, &svc.embedder) {
        (Some(v), Some(e)) => format!(
            "- similarity({v}, :p) ranks by vector similarity. Bind :p in `params` as {{\"vector_of\": \"<key>\"}} (that row's vector, for \"more like this\") or {{\"embed\": \"some text\"}} (the server's embedder: {}).",
            e.name
        ),
        (Some(v), None) => format!("- similarity({v}, :p) ranks by vector similarity. Bind :p in `params` as {{\"vector_of\": \"<key>\"}}, the vector of that row (\"more like this\")."),
        (None, _) => "- The table has no VECTOR field.".to_string(),
    };
    let sparse_help = match s.fields.iter().find(|f| f.ty == FieldType::Sparse) {
        Some(f) => format!(
            "\n- similarity({}, :w) ranks by learned term weights (a dot product). Bind :w in `params` as an object of lower-case terms and weights, e.g. {{\"boots\": 1.0, \"hiking\": 0.7}}.",
            f.name
        ),
        None => String::new(),
    };
    format!(
        r#"You answer questions about the data in one Strata table by querying it with SQL through the run_sql tool. Strata is a search database: a single table queried with SELECT plus search functions.

Table `{table}`, {docs} rows. Fields:
{fields}

Strata SQL (read-only here):
SELECT <fields | * | score() | COUNT(*)> FROM {table} [WHERE <predicate>] [GROUP BY <field>] [ORDER BY <one score or numeric field> [ASC|DESC]] [LIMIT n [OFFSET m]]
- MATCH(text_field, 'query') is a boolean full-text filter: terms, "phrases", AND, OR, NOT, -term, prefix*, parentheses; words side by side mean AND.
- bm25(text_field, 'words') ranks by text relevance (a row scores if it has any of the words). Put the user's key words here.
- Comparisons (=, <, <=, >, >=, !=), BETWEEN, IN (...), AND, OR, NOT on KEYWORD, INT and FLOAT fields.
{vector_help}{sparse_help}
- rrf(score1, score2) fuses two rankings by rank; 0.7 * bm25(...) + 0.3 * similarity(...) is a weighted sum of normalised scores.
- decay(numeric_field, origin, scale) is 1 at the origin and 0.5 at origin ± scale (options: offset => d, decay => 0.5, shape => 'exp' | 'gauss' | 'linear'; origin may be now() for Unix-second timestamps). Multiply to boost relevance: ORDER BY bm25(body, 'boots') * decay(year, 2026, 2) DESC.
- ORDER BY a numeric field sorts by it (ORDER BY price ASC). score() returns the ranking score.
- Facets: SELECT f, COUNT(*) FROM {table} WHERE ... GROUP BY f ORDER BY 2 DESC LIMIT 10, where f is a KEYWORD field (or a multi-valued INT field). COUNT(*) alone counts rows.
- Not supported: JOIN, other aggregates (SUM, AVG, MIN, MAX), expressions in SELECT, ORDER BY two scores. For a minimum or maximum, ORDER BY the field and LIMIT 1. For an average, fetch the rows and compute it yourself only if there are few.

How to work:
- Query before you answer, and base every claim on rows you saw. Never invent rows or values.
- Unsure of a field's values? Run a facet query first.
- Use LIMIT 20 or less unless you only count.
- If a query fails, read the error and fix the SQL.
- Stop querying as soon as you can answer.
- Answer in a few plain sentences or a short list. Name the keys of the rows you rely on. End with the one query that best supports the answer, in a ```sql block."#,
        table = s.table,
        fields = fields.join("\n"),
    )
}

fn tools() -> Json {
    json!([{
        "name": "run_sql",
        "description": "Runs one read-only Strata SQL SELECT on the table and returns its columns and rows (at most 30 rows shown; long text cut), the total row count returned, and how long it took. Use it to search, filter, rank, count and facet. Writes are refused.",
        "input_schema": {
            "type": "object",
            "properties": {
                "sql": {"type": "string", "description": "One SELECT statement in Strata's dialect."},
                "params": {"type": "object", "description": "Values for :name placeholders, e.g. {\"q\": {\"vector_of\": \"p0000042\"}}."}
            },
            "required": ["sql"]
        }
    }])
}

/// A value as the model sees it: long strings cut, vectors summarised.
fn shown(v: &strata::Value) -> Json {
    match v {
        strata::Value::Str(s) if s.chars().count() > CHARS_SHOWN => json!(format!("{}…", s.chars().take(CHARS_SHOWN).collect::<String>())),
        strata::Value::Vector(x) => json!(format!("<vector of {}>", x.len())),
        v => js::to_json(v),
    }
}

/// Runs a `run_sql` call; the model's view of the result, and the UI's.
async fn run_sql(svc: &Arc<Service>, input: &Json) -> (Result<Json, String>, Json) {
    let sql = input.get("sql").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let t0 = Instant::now();
    let r: Result<_, ApiError> = async {
        if strata::query::bind_write(&sql, &svc.schema, &strata::query::Params::new()).ok().flatten().is_some() {
            return Err(ApiError::bad("run_sql is read-only"));
        }
        let params = svc.params(input.get("params")).await?;
        svc.query(QueryReq { sql: sql.clone(), params, ..Default::default() }).await
    }
    .await;
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    match r {
        Ok(a) => {
            let rows: Vec<Json> = a.rs.rows.iter().take(ROWS_SHOWN).map(|r| Json::Array(r.iter().map(shown).collect())).collect();
            let model = json!({"columns": a.rs.columns, "rows": rows, "row_count": a.rs.rows.len(), "took_ms": (ms * 10.0).round() / 10.0});
            let ui = json!({"tool": "run_sql", "sql": sql, "params": input.get("params"), "ok": true, "rows": a.rs.rows.len(), "took_ms": ms, "kind": a.kind});
            (Ok(model), ui)
        }
        Err(e) => {
            let ui = json!({"tool": "run_sql", "sql": sql, "params": input.get("params"), "ok": false, "error": e.message, "took_ms": ms});
            (Err(e.message), ui)
        }
    }
}

/// One Messages API call, retried on 429, 5xx and network errors.
async fn call(client: &reqwest::Client, base: &str, key: &str, body: &Json) -> Result<Json, ApiError> {
    let mut wait = Duration::from_millis(500);
    for attempt in 0..3 {
        let r = client
            .post(format!("{}/v1/messages", base.trim_end_matches('/')))
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .json(body)
            .send()
            .await;
        let retry = match r {
            Ok(resp) => {
                let status = resp.status();
                let j: Json = resp.json().await.unwrap_or(Json::Null);
                if status.is_success() {
                    return Ok(j);
                }
                let msg = j["error"]["message"].as_str().unwrap_or("no message").to_string();
                if status.as_u16() != 429 && !status.is_server_error() {
                    let code = if status.as_u16() == 401 { 401 } else { 502 };
                    return Err(ApiError { status: code, message: format!("the Claude API answered {status}: {msg}") });
                }
                format!("{status}: {msg}")
            }
            Err(e) => e.to_string(),
        };
        if attempt == 2 {
            return Err(ApiError { status: 502, message: format!("the Claude API failed: {retry}") });
        }
        tokio::time::sleep(wait).await;
        wait *= 3;
    }
    unreachable!()
}

pub async fn run(svc: &Arc<Service>, req: AgentReq) -> Result<Json, ApiError> {
    let key = req.api_key.or_else(|| svc.agent.api_key.clone()).ok_or_else(|| ApiError {
        status: 400,
        message: "no Anthropic API key: start the server with ANTHROPIC_API_KEY set, or enter a key in the UI".into(),
    })?;
    let model = req.model.unwrap_or_else(|| svc.agent.model.clone());
    let docs = svc.health().await["docs"].as_u64().unwrap_or(0);
    let client = reqwest::Client::builder().timeout(Duration::from_secs(300)).build().map_err(ApiError::internal)?;
    let t0 = Instant::now();
    let mut messages = vec![json!({"role": "user", "content": req.question})];
    let mut steps = Vec::new();
    let (mut input_tokens, mut output_tokens, mut cache_read) = (0u64, 0u64, 0u64);
    let mut answer = String::new();
    let mut stop = String::new();
    for step in 0..req.max_steps {
        let last = step + 1 == req.max_steps;
        let mut body = json!({
            "model": model,
            "max_tokens": 16000,
            "system": system_prompt(svc, docs),
            "tools": tools(),
            "messages": messages,
            "cache_control": {"type": "ephemeral"},
        });
        if last {
            // Out of steps: answer from what was found.
            body["tool_choice"] = json!({"type": "none"});
        }
        if let Some(e) = &req.effort {
            body["output_config"] = json!({"effort": e});
        }
        let resp = call(&client, &svc.agent.base_url, &key, &body).await?;
        let u = &resp["usage"];
        input_tokens += u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        output_tokens += u["output_tokens"].as_u64().unwrap_or(0);
        stop = resp["stop_reason"].as_str().unwrap_or("").to_string();
        let content = resp["content"].as_array().cloned().unwrap_or_default();
        // The whole content goes back unchanged, thinking blocks included.
        messages.push(json!({"role": "assistant", "content": content}));
        let text: Vec<&str> = content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
        match stop.as_str() {
            "tool_use" => {
                let mut results = Vec::new();
                for b in content.iter().filter(|b| b["type"] == "tool_use") {
                    let id = b["id"].as_str().unwrap_or("");
                    let (r, ui) = match b["name"].as_str() {
                        Some("run_sql") => run_sql(svc, &b["input"]).await,
                        other => (Err(format!("unknown tool {other:?}")), json!({"tool": other, "ok": false})),
                    };
                    steps.push(ui);
                    results.push(match r {
                        Ok(j) => json!({"type": "tool_result", "tool_use_id": id, "content": j.to_string()}),
                        Err(e) => json!({"type": "tool_result", "tool_use_id": id, "content": e, "is_error": true}),
                    });
                }
                if step + 2 == req.max_steps {
                    results.push(json!({"type": "text", "text": "That was the last query: answer now from what you found."}));
                }
                messages.push(json!({"role": "user", "content": results}));
            }
            "pause_turn" => continue,
            "refusal" => {
                answer = "The model declined to answer this question.".into();
                break;
            }
            _ => {
                answer = text.join("\n");
                if stop == "max_tokens" {
                    answer.push_str("\n\n(The answer was cut off at the output limit.)");
                }
                break;
            }
        }
    }
    if answer.is_empty() {
        answer = "No answer within the step limit; the queries the agent ran are listed below.".into();
    }
    Ok(json!({
        "answer": answer,
        "steps": steps,
        "model": model,
        "stop_reason": stop,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens, "cache_read_input_tokens": cache_read},
        "took_ms": t0.elapsed().as_secs_f64() * 1e3,
    }))
}
