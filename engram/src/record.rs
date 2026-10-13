//! Memory records and the Strata table that holds them.

use serde::{Deserialize, Serialize};
use strata::graph::{edges_value, parse_edges, Edge};
use strata::{Field, FieldType, Schema, Value, VectorIndexKind};

use crate::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Prompt,
    Answer,
    ToolCall,
    ToolResult,
    Error,
    Episode,
    Summary,
    Decision,
    Preference,
    Fact,
    Todo,
    Note,
    /// A thing records are about: a person, place, product, topic or file.
    /// Records link to it, so records about the same thing are one hop
    /// apart through it.
    Entity,
}

impl Kind {
    pub const ALL: [Kind; 13] = [
        Kind::Prompt,
        Kind::Answer,
        Kind::ToolCall,
        Kind::ToolResult,
        Kind::Error,
        Kind::Episode,
        Kind::Summary,
        Kind::Decision,
        Kind::Preference,
        Kind::Fact,
        Kind::Todo,
        Kind::Note,
        Kind::Entity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Prompt => "prompt",
            Kind::Answer => "answer",
            Kind::ToolCall => "tool_call",
            Kind::ToolResult => "tool_result",
            Kind::Error => "error",
            Kind::Episode => "episode",
            Kind::Summary => "summary",
            Kind::Decision => "decision",
            Kind::Preference => "preference",
            Kind::Fact => "fact",
            Kind::Todo => "todo",
            Kind::Note => "note",
            Kind::Entity => "entity",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Salience prior ι in [0, 1].
    pub fn prior(self) -> f64 {
        match self {
            Kind::Preference => 1.0,
            Kind::Decision => 0.9,
            Kind::Summary | Kind::Fact | Kind::Todo | Kind::Note => 0.8,
            Kind::Prompt => 0.7,
            Kind::Error | Kind::Episode => 0.6,
            Kind::Answer => 0.5,
            Kind::ToolCall | Kind::ToolResult => 0.3,
            Kind::Entity => 0.0,
        }
    }

    /// Durable knowledge (shown in briefings, pinned).
    pub fn is_knowledge(self) -> bool {
        matches!(self, Kind::Preference | Kind::Decision | Kind::Fact | Kind::Todo | Kind::Note)
    }
}

/// An edge from one record to another: `about` an entity, `from` the turn a
/// note was written from, `supersedes` an older note, `touches` a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub label: String,
    pub to: String,
}

impl Link {
    pub fn new(label: &str, to: impl Into<String>) -> Link {
        Link { label: label.to_string(), to: to.into() }
    }
}

/// The id of the entity named `name`: `e:` and the name lowercased, without
/// a leading article or punctuation, with a plural `s` dropped, so that
/// "Houseplants" and "the houseplant" meet in one node. Files get `f:` and
/// their path as is.
pub fn entity_id(name: &str) -> Option<String> {
    let words: Vec<String> = name
        .split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-' || c == '.'))
        .map(|w| w.trim_matches(|c: char| c == '\'' || c == '-' || c == '.').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let mut words: Vec<String> = match words.first().map(|w| w.as_str()) {
        Some("the" | "a" | "an" | "my" | "our" | "their" | "his" | "her") if words.len() > 1 => words[1..].to_vec(),
        _ => words,
    };
    if let Some(last) = words.last_mut() {
        *last = singular(last);
    }
    let s = words.join(" ");
    (!s.is_empty() && s.len() <= 80).then(|| format!("e:{s}"))
}

pub fn file_entity_id(path: &str) -> String {
    format!("f:{path}")
}

fn singular(w: &str) -> String {
    if let Some(stem) = w.strip_suffix("'s") {
        return stem.to_string();
    }
    let n = w.chars().count();
    if n <= 3 || !w.ends_with('s') || w.ends_with("ss") || w.ends_with("us") || w.ends_with("is") {
        return w.to_string();
    }
    if let Some(stem) = w.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if w.ends_with("ches") || w.ends_with("shes") || w.ends_with("xes") || w.ends_with("sses") {
        return w[..w.len() - 2].to_string();
    }
    w[..w.len() - 1].to_string()
}

/// A record before the daemon assigns its sequence number.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    pub kind: Kind,
    pub tag: String,
    pub files: Vec<String>,
    pub text: String,
    /// Context in the record's lexical key (BM25) but not in its vector nor
    /// shown as its content: the turn's prompt for tool records, the section
    /// for summaries.
    pub ctx: String,
    /// Overrides the kind's prior.
    pub importance: Option<f64>,
    pub links: Vec<Link>,
    /// Names of the entities the record is about; the store links the
    /// record to them and creates the ones it has not seen.
    pub entities: Vec<String>,
}

impl Draft {
    pub fn new(kind: Kind, text: impl Into<String>) -> Draft {
        Draft { kind, tag: "-".into(), files: Vec::new(), text: text.into(), ctx: String::new(), importance: None, links: Vec::new(), entities: Vec::new() }
    }
    pub fn link(mut self, label: &str, to: impl Into<String>) -> Draft {
        self.links.push(Link::new(label, to));
        self
    }
    pub fn entities(mut self, names: Vec<String>) -> Draft {
        self.entities = names;
        self
    }
    pub fn tag(mut self, t: impl Into<String>) -> Draft {
        let t = t.into();
        self.tag = if t.is_empty() { "-".into() } else { t };
        self
    }
    pub fn files(mut self, f: Vec<String>) -> Draft {
        self.files = f;
        self
    }
    pub fn ctx(mut self, c: impl Into<String>) -> Draft {
        self.ctx = c.into();
        self
    }
    pub fn importance(mut self, x: f64) -> Draft {
        self.importance = Some(x);
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub session: String,
    pub kind: Kind,
    pub tag: String,
    pub turn: i64,
    pub seq: i64,
    pub ts: i64,
    pub importance: f64,
    pub files: String,
    pub text: String,
    pub ctx: String,
    pub links: Vec<Link>,
}

impl Record {
    pub fn from_draft(d: Draft, session: &str, turn: i64, seq: i64, ts: i64) -> Record {
        Record {
            id: record_id(session, seq),
            session: session.to_string(),
            kind: d.kind,
            tag: d.tag,
            turn,
            seq,
            ts,
            importance: d.importance.unwrap_or_else(|| d.kind.prior()).clamp(0.0, 1.0),
            files: d.files.join(" "),
            text: d.text,
            ctx: d.ctx,
            links: d.links,
        }
    }

    /// The Strata row, in [`schema`] field order.
    pub fn to_doc(&self, emb: Vec<f32>) -> Vec<Value> {
        vec![
            Value::Str(self.id.clone()),
            Value::Str(self.session.clone()),
            Value::Str(self.kind.as_str().into()),
            Value::Str(self.tag.clone()),
            Value::Int(self.turn),
            Value::Int(self.seq),
            Value::Int(self.ts),
            Value::Float((self.importance * 100.0).round() / 100.0),
            Value::Str(self.files.clone()),
            Value::Str(self.text.clone()),
            Value::Str(self.ctx.clone()),
            Value::Str(String::new()),
            Value::Vector(emb.into()),
            self.links_value(),
        ]
    }

    pub fn links_value(&self) -> Value {
        let edges: Vec<Edge> = self.links.iter().map(|l| Edge::labeled(&l.label, l.to.as_str())).collect();
        edges_value(&edges)
    }

    /// Ids this record links to with `label`.
    pub fn linked<'a>(&'a self, label: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.links.iter().filter(move |l| l.label == label).map(|l| l.to.as_str())
    }

    /// What the embedding is computed from: the text alone. The context
    /// joins the lexical key only; in a vector it pulls a response towards
    /// its request, which then crowds out the request itself
    /// (results/structure: MRR 0.60 → 0.66 on LongMemEval-S).
    pub fn embed_text(&self) -> String {
        self.text.clone()
    }
}

/// `<first 8 characters of the session>:<seq>`.
pub fn record_id(session: &str, seq: i64) -> String {
    let s: String = session.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    format!("{}:{seq}", if s.is_empty() { "x".to_string() } else { s })
}

/// Columns every query projects, in this order (see [`crate::store::Hit`]).
pub const COLUMNS: &str = "id, session, kind, tag, turn, seq, ts, importance, files, text, ctx, links";

pub fn schema(dim: usize, index: VectorIndexKind) -> Result<Schema> {
    Ok(Schema::new(
        "memory",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("session", FieldType::Keyword),
            Field::new("kind", FieldType::Keyword),
            Field::new("tag", FieldType::Keyword),
            Field::new("turn", FieldType::Int),
            Field::new("seq", FieldType::Int),
            Field::new("ts", FieldType::Int),
            Field::new("importance", FieldType::Float),
            Field::new("files", FieldType::Text { positions: false }),
            Field::new("text", FieldType::Text { positions: true }).stored_only(),
            Field::new("ctx", FieldType::Text { positions: false }).stored_only(),
            // What MATCH and bm25 search: the text and its context.
            Field::new("body", FieldType::Text { positions: true }).not_stored().generated(&["text", "ctx"]),
            Field::new("emb", FieldType::Vector { dim, index }).not_stored(),
            // Edges to other records (see [`Link`]); traversed by recall.
            Field::new("links", FieldType::Edges),
        ],
        Some("id"),
    )?)
}

/// The links stored in an EDGES value.
pub fn links_of(v: &Value) -> Vec<Link> {
    parse_edges(v, false)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| Some(Link { label: e.label.unwrap_or_default(), to: e.to.as_str()?.to_string() }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_ids_meet() {
        assert_eq!(entity_id("Houseplants"), entity_id("the houseplant"));
        assert_eq!(entity_id("Honda Civic").as_deref(), Some("e:honda civic"));
        assert_eq!(entity_id("bus").as_deref(), Some("e:bus"));
        assert_eq!(entity_id("glasses").as_deref(), Some("e:glass"));
        assert_eq!(entity_id("berries").as_deref(), Some("e:berry"));
        assert_eq!(entity_id("Dr. Smith's").as_deref(), Some("e:dr smith"));
        assert_eq!(entity_id("  "), None);
    }

    #[test]
    fn links_round_trip() {
        let r = Record::from_draft(Draft::new(Kind::Fact, "x").link("about", "e:car").link("from", "s:1"), "s", 1, 2, 3);
        assert_eq!(links_of(&r.links_value()), r.links);
        assert_eq!(r.linked("about").collect::<Vec<_>>(), vec!["e:car"]);
    }
}
