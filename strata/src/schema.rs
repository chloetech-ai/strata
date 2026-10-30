//! Tables and field types.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
pub use crate::media::MediaKind;
pub use crate::text::analyzer::Analyzer;

/// How a vector field is indexed (every vector field also keeps float32
/// vectors and compressed codes in the forward index, so `Flat` adds
/// nothing).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VectorIndexKind {
    Flat,
    /// Inverted file: `nlist` k-means clusters, members stored as posting
    /// lists with their codes.
    Ivf { nlist: usize },
    /// Hierarchical navigable small world graph.
    Hnsw { m: usize, ef_construction: usize },
}

/// The compressed codes a vector field is searched with before the best
/// candidates are rescored with its float32 vectors (see
/// [`crate::vector::codes`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorCodes {
    /// A byte per dimension: a quarter of the float32 vectors.
    #[default]
    Sq8,
    /// A bit per dimension: 1/32 of the float32 vectors. Searches rescore
    /// [`crate::vector::codes::BINARY_RERANK`] times more candidates.
    Binary,
}

impl VectorCodes {
    pub fn as_str(&self) -> &'static str {
        match self {
            VectorCodes::Sq8 => "sq8",
            VectorCodes::Binary => "binary",
        }
    }

    pub fn parse(s: &str) -> Option<VectorCodes> {
        [VectorCodes::Sq8, VectorCodes::Binary].into_iter().find(|k| k.as_str().eq_ignore_ascii_case(s))
    }

    /// Bytes per code row of `dim` dimensions: SQ8 rows are padded to 64
    /// bytes, binary rows to 8.
    pub fn stride(&self, dim: usize) -> usize {
        match self {
            VectorCodes::Sq8 => dim.next_multiple_of(64),
            VectorCodes::Binary => crate::vector::bits::stride(dim),
        }
    }

    fn is_sq8(&self) -> bool {
        *self == VectorCodes::Sq8
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FieldType {
    /// Analysed full text. `positions` enables phrase queries.
    Text { positions: bool },
    /// An exact string value: equality, `IN`, facets.
    Keyword,
    Int,
    Float,
    /// A fixed-dimension float vector; similarity is the dot product.
    Vector { dim: usize, index: VectorIndexKind },
    /// Outgoing edges to other rows of the table, by primary key: a JSON
    /// array (see [`crate::graph`]). Indexed: out-edges and a reverse index.
    Edges,
    /// Learned sparse term weights (SPLADE, BGE-M3 sparse): a JSON object
    /// `{"term": weight}`; similarity is the dot product (see
    /// [`crate::text::sparse`]). Indexed: a text index of the terms with
    /// the weights as frequencies.
    Sparse,
}

/// What an INT column's integers stand for (format 3; a database is
/// written in format 3 only while a table has such a field). Queries and
/// writes see [`Value::Bool`](crate::value::Value::Bool) and
/// [`Value::Timestamp`](crate::value::Value::Timestamp); the column, its
/// filters, sorts and aggregates work on the integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Logical {
    /// `BOOL`: 1 for true, 0 for false.
    Bool,
    /// `TIMESTAMP`: microseconds since 1970-01-01T00:00:00Z
    /// ([`crate::timestamp`]).
    Timestamp,
}

impl Logical {
    /// The type's name in SQL.
    pub fn type_name(self) -> &'static str {
        match self {
            Logical::Bool => "BOOL",
            Logical::Timestamp => "TIMESTAMP",
        }
    }

    /// What a value of this type is, for error messages.
    pub fn expects(self) -> &'static str {
        match self {
            Logical::Bool => "a BOOL is true or false",
            Logical::Timestamp => "a TIMESTAMP is ISO 8601 text such as 2026-10-01 or 2026-10-01T12:30:00Z",
        }
    }

    /// The format 3 feature a field of this type needs
    /// ([`crate::storage::manifest`]).
    pub fn feature(self) -> &'static str {
        match self {
            Logical::Bool => "bool",
            Logical::Timestamp => "timestamp",
        }
    }

    /// A stored integer as queries return it; other values as they are.
    pub fn output(self, v: crate::value::Value) -> crate::value::Value {
        use crate::value::Value;
        match (self, v) {
            (Logical::Bool, Value::Int(x)) => Value::Bool(x != 0),
            (Logical::Timestamp, Value::Int(t)) => Value::Timestamp(t),
            (_, v) => v,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    /// Kept in the doc store and returned by `SELECT`. (Vector fields are
    /// always retrievable from the vector sections.)
    pub stored: bool,
    /// Indexed for search (text: postings; keyword: postings + column;
    /// numbers: column + range postings; vectors: the ANN index).
    pub indexed: bool,
    /// A generated TEXT column (like SQLite's `GENERATED ALWAYS AS`): the
    /// non-empty values of these fields joined by newlines. Merges recompute
    /// it from the stored sources, so it need not be stored itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_from: Vec<String>,
    /// A KEYWORD or INT field holding any number of values per document
    /// (`Value::List`); a predicate matches when any value matches. Indexed
    /// as a keyword column (INT values in an order-preserving encoding, so
    /// ranges are runs of codes), and stored. A multi-valued VECTOR field
    /// (`VECTOR(d)[]`, manifest feature `multi_vector`) holds a list of
    /// vectors per row, such as a page's patches, which `maxsim` scores
    /// ([`crate::vector::multi`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi: bool,
    /// A multi-vector field keeps its float32 vectors too, besides the
    /// codes it searches and scores with (four times the bytes of SQ8
    /// codes); without them, rows read back and merges use the SQ8 codes'
    /// values.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keep_f32: bool,
    /// A multi-vector field merges a row's vectors whose cosine similarity
    /// is at least this into their mean when the row is written (token
    /// pooling), until no two are that similar
    /// ([`crate::vector::multi::pool`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<f32>,
    /// A VECTOR field's compressed codes (default SQ8).
    #[serde(default, skip_serializing_if = "VectorCodes::is_sq8")]
    pub codes: VectorCodes,
    /// What a VECTOR field's vectors are: a model's embedding of other
    /// fields. Recorded so readers know what a vector means; a database
    /// given the model ([`crate::db::Db::set_embedder`]) also computes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed: Option<Embed>,
    /// The table whose primary keys this KEYWORD or INT field holds (SQL
    /// `REFERENCES`), recorded for readers and joins but not enforced: a
    /// value need not be a live key of that table. An EDGES field holds
    /// keys of its own table, and may say so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<String>,
    /// A single-valued INT field that holds booleans or timestamps
    /// ([`Logical`]). Neither the primary key nor a reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical: Option<Logical>,
    /// Every row holds a value (SQL `NOT NULL`): a write that leaves the
    /// field NULL is refused. (The primary key always holds one, and does
    /// not say so.)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_null: bool,
    /// The value a written row takes when it leaves the field out (SQL
    /// `DEFAULT`), as JSON: a value of the field's type, written as a row
    /// would give it, or for a TIMESTAMP `"now"`, the time of the write.
    /// A row that gives NULL keeps it ([`Field::default_value`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// How a TEXT field's text becomes terms; `None` is `english`
    /// ([`Analyzer`]), which every version of Strata has, and another makes
    /// the database format 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub analyzer: Option<Analyzer>,
    /// A MEDIA field ([`crate::media`]): a stored KEYWORD column whose
    /// value is a file's [`crate::media::Media`] as JSON, of this kind,
    /// indexed by its `sha256`. Neither the key, a reference, a list nor a
    /// source of other fields. Makes the database format 3 (feature
    /// `media`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaKind>,
    /// A KEYWORD[] that holds the semantic ID of the row's vector, every
    /// prefix of it (feature `semantic_id`; [`crate::sid`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_id: Option<SemanticId>,
    /// A TEXT field that is a model's text of a MEDIA field (a caption of
    /// its image; [`Derive`]). Makes the database format 3 (feature
    /// `derive`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derive: Option<Derive>,
}

/// A TEXT field made by `model` from the file MEDIA field `from` holds
/// ([`crate::derive`]): a caption of an image. Writes that leave the text
/// out get it made before the writer, and rewrites that change the file
/// make it again; a text a write supplies is kept as given. The text is
/// stored, so merges and replicas never run the model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Derive {
    /// A MEDIA field.
    pub from: String,
    /// The model, by the name [`crate::derive::open`] finds it under, such
    /// as `blip-image-captioning-base` or `colors`.
    pub model: String,
}

/// A semantic-ID field's source and codebook ([`crate::sid`]): the writer
/// fills it from the vector of field `from` with codebook `codebook`, whose
/// file has SHA-256 `hash`. A new codebook means a new field: codes of two
/// codebooks never mix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticId {
    /// A VECTOR field.
    pub from: String,
    /// The codebook, by its name in the models directory.
    pub codebook: String,
    /// Hex SHA-256 of the codebook's file; filled in when a database
    /// creates the table ([`crate::sid::resolve`]).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hash: String,
}

/// A VECTOR field that is the embedding of other fields: `model`'s vector
/// of their non-empty values joined by newlines (the text a generated
/// field holds), or of the image a MEDIA field holds. Writes that leave
/// the vector out get it computed, and rewrites that change a source
/// recompute it; a vector a write supplies is kept as given. Queries embed
/// their text, or an image, with the same model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Embed {
    /// Stored or generated TEXT or KEYWORD fields, or one MEDIA field of
    /// kind image.
    pub from: Vec<String>,
    /// The model, by the name [`crate::embed::open`] finds it under, such
    /// as `potion-retrieval-32M`, `bge-small-en-v1.5` or `hash:64`; or the
    /// name of a model the database cannot run (vectors computed
    /// elsewhere, such as `cohere/embed-english-v3.0`), which only
    /// documents the field.
    pub model: String,
}

impl Field {
    pub fn new(name: &str, ty: FieldType) -> Self {
        Field {
            name: name.to_string(),
            ty,
            stored: true,
            indexed: true,
            generated_from: Vec::new(),
            multi: false,
            keep_f32: false,
            pool: None,
            codes: VectorCodes::Sq8,
            embed: None,
            references: None,
            logical: None,
            not_null: false,
            default: None,
            analyzer: None,
            media: None,
            semantic_id: None,
            derive: None,
        }
    }
    /// A BOOL field ([`Logical::Bool`]).
    pub fn boolean(name: &str) -> Self {
        Field { logical: Some(Logical::Bool), ..Field::new(name, FieldType::Int) }
    }
    /// A TIMESTAMP field ([`Logical::Timestamp`]).
    pub fn timestamp(name: &str) -> Self {
        Field { logical: Some(Logical::Timestamp), ..Field::new(name, FieldType::Int) }
    }
    /// A MEDIA field of `kind` ([`Field::media`]).
    pub fn media(name: &str, kind: MediaKind) -> Self {
        Field { media: Some(kind), ..Field::new(name, FieldType::Keyword) }
    }
    /// The field's type in SQL: `BOOL` and `TIMESTAMP` for those
    /// ([`Logical`]), `MEDIA` for a media field, else [`type_name`] of its
    /// type.
    pub fn type_name(&self) -> &'static str {
        if self.media.is_some() {
            return "MEDIA";
        }
        self.logical.map_or_else(|| type_name(&self.ty), Logical::type_name)
    }
    /// `v` in the form this field's column stores: a BOOL field's value
    /// as 1 or 0 (from a boolean, 1 or 0, or the text `true`, `false`,
    /// `t`, `f`, `1` or `0`), a TIMESTAMP field's as microseconds since
    /// the epoch (from a timestamp, that integer, or ISO 8601 text), and a
    /// boolean given to any other INT or FLOAT field as 1 or 0. Any other
    /// value is returned as it is, for [`crate::value::validate`] to
    /// judge.
    pub fn store(&self, v: crate::value::Value) -> Result<crate::value::Value> {
        use crate::value::Value;
        let bad = |v: &Value, want: Logical| Error::Schema(format!("bad value for {}: {v:?} ({})", self.name, want.expects()));
        const BOOL: Logical = Logical::Bool;
        const TIME: Logical = Logical::Timestamp;
        Ok(match (self.logical, v) {
            (_, Value::Null) => Value::Null,
            (Some(Logical::Bool), Value::Bool(b)) => Value::Int(b as i64),
            (Some(Logical::Bool), Value::Int(x @ (0 | 1))) => Value::Int(x),
            (Some(Logical::Bool), Value::Str(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => Value::Int(1),
                "false" | "f" | "0" => Value::Int(0),
                _ => return Err(bad(&Value::Str(s), BOOL)),
            },
            (Some(Logical::Bool), v) => return Err(bad(&v, BOOL)),
            (Some(Logical::Timestamp), Value::Timestamp(t) | Value::Int(t)) => Value::Int(t),
            (Some(Logical::Timestamp), Value::Str(s)) => match crate::timestamp::parse(&s) {
                Some(t) => Value::Int(t),
                None => return Err(bad(&Value::Str(s), TIME)),
            },
            (Some(Logical::Timestamp), v) => return Err(bad(&v, TIME)),
            (None, Value::Bool(b)) if matches!(self.ty, FieldType::Int | FieldType::Float) => Value::Int(b as i64),
            (None, Value::List(xs)) if self.ty == FieldType::Int => Value::List(
                xs.into_iter()
                    .map(|x| match x {
                        Value::Bool(b) => Value::Int(b as i64),
                        x => x,
                    })
                    .collect(),
            ),
            (None, v) => v,
        })
    }
    /// Makes the field `NOT NULL` ([`Field::not_null`]).
    pub fn not_null(mut self) -> Self {
        self.not_null = true;
        self
    }
    /// Gives a TEXT field an analyzer ([`Field::analyzer`]).
    pub fn analyzer(mut self, analyzer: Analyzer) -> Self {
        self.analyzer = Some(analyzer);
        self
    }
    /// The analyzer of a TEXT field's text ([`Field::analyzer`]).
    pub fn text_analyzer(&self) -> Analyzer {
        self.analyzer.unwrap_or_default()
    }
    /// Gives the field a default ([`Field::default`]).
    pub fn default(mut self, value: serde_json::Value) -> Self {
        self.default = Some(value);
        self
    }
    /// The value, in stored form, that a row written at `now`
    /// (microseconds since the epoch) takes when it leaves the field out:
    /// its default, or NULL without one.
    pub fn default_value(&self, now: i64) -> Result<crate::value::Value> {
        use crate::value::Value;
        match &self.default {
            None => Ok(Value::Null),
            Some(serde_json::Value::String(s)) if self.logical == Some(Logical::Timestamp) && s.eq_ignore_ascii_case("now") => Ok(Value::Int(now)),
            Some(j) => crate::ingest::field_value(self, false, j).map_err(|e| Error::Schema(format!("the default of {}: {e}", self.name))),
        }
    }
    /// A stored value as queries return it ([`Logical::output`]).
    pub fn output(&self, v: crate::value::Value) -> crate::value::Value {
        match self.logical {
            Some(l) => l.output(v),
            None => v,
        }
    }
    /// Makes a KEYWORD or INT field hold primary keys of `table`.
    pub fn references(mut self, table: &str) -> Self {
        self.references = Some(table.to_string());
        self
    }
    /// Sets a VECTOR field's compressed codes.
    pub fn codes(mut self, codes: VectorCodes) -> Self {
        self.codes = codes;
        self
    }
    /// Makes a KEYWORD or INT field multi-valued, or a VECTOR field a
    /// multi-vector field, which searches binary codes unless
    /// [`Field::codes`] says otherwise after this.
    pub fn multi(mut self) -> Self {
        self.multi = true;
        if matches!(self.ty, FieldType::Vector { .. }) {
            self.codes = VectorCodes::Binary;
        }
        self
    }
    /// Keeps a multi-vector field's float32 vectors ([`Field::keep_f32`]).
    pub fn keep_f32(mut self) -> Self {
        self.keep_f32 = true;
        self
    }
    /// Pools a multi-vector field's similar vectors at write
    /// ([`Field::pool`]).
    pub fn pooled(mut self, min_cosine: f32) -> Self {
        self.pool = Some(min_cosine);
        self
    }
    /// Whether this is a multi-vector field (`VECTOR(d)[]`).
    pub fn is_multi_vector(&self) -> bool {
        self.multi && matches!(self.ty, FieldType::Vector { .. })
    }
    pub fn generated(mut self, sources: &[&str]) -> Self {
        self.generated_from = sources.iter().map(|s| s.to_string()).collect();
        self
    }
    /// Makes a VECTOR field `model`'s embedding of `sources` ([`Embed`]).
    pub fn embedded(mut self, model: &str, sources: &[&str]) -> Self {
        self.embed = Some(Embed { from: sources.iter().map(|s| s.to_string()).collect(), model: model.to_string() });
        self
    }

    /// Makes a TEXT field `model`'s text of MEDIA field `from` ([`Derive`]).
    pub fn derived(mut self, model: &str, from: &str) -> Self {
        self.derive = Some(Derive { from: from.to_string(), model: model.to_string() });
        self
    }
    /// A semantic-ID field: a stored KEYWORD[] filled from the vector of
    /// field `from` with codebook `codebook` ([`SemanticId`]).
    pub fn semantic_id(name: &str, from: &str, codebook: &str) -> Self {
        Field { multi: true, semantic_id: Some(SemanticId { from: from.to_string(), codebook: codebook.to_string(), hash: String::new() }), ..Field::new(name, FieldType::Keyword) }
    }
    pub fn stored_only(mut self) -> Self {
        self.indexed = false;
        self
    }
    pub fn not_stored(mut self) -> Self {
        self.stored = false;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub table: String,
    pub fields: Vec<Field>,
    /// Index into `fields` of the primary key (a Keyword or Int field).
    pub primary_key: Option<usize>,
    /// The field whose value places a row on a shard instead of its key
    /// ([`Schema::with_shard_by`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_by: Option<String>,
    /// The field that orders each segment's rows ([`Schema::with_cluster_by`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_by: Option<String>,
    /// How long rows are kept ([`Schema::with_retain`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain: Option<Retain>,
    /// The fields that make each row an edge of a graph
    /// ([`Schema::with_edges`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges: Option<Edges>,
}

/// An edge table's fields ([`Schema::with_edges`]): each row is an edge
/// from the row whose key is in `src` to the row whose key is in `dst`,
/// weighted by `weight` (1 without one).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edges {
    pub src: String,
    pub dst: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<String>,
}

impl Edges {
    /// `text` as SQL's option spells it: `'src, dst'` or `'src, dst, w'`.
    pub fn parse(text: &str) -> Result<Edges> {
        let names: Vec<&str> = text.split(',').map(str::trim).collect();
        match names[..] {
            [src, dst] if !src.is_empty() && !dst.is_empty() => Ok(Edges { src: src.into(), dst: dst.into(), weight: None }),
            [src, dst, w] if !src.is_empty() && !dst.is_empty() && !w.is_empty() => Ok(Edges { src: src.into(), dst: dst.into(), weight: Some(w.into()) }),
            _ => Err(Error::Schema(format!("edges {text:?}: name the source and target fields and, if edges have weights, the weight's, as 'src, dst, w'"))),
        }
    }

    /// The option as [`Edges::parse`] reads it.
    pub fn text(&self) -> String {
        match &self.weight {
            Some(w) => format!("{}, {}, {w}", self.src, self.dst),
            None => format!("{}, {}", self.src, self.dst),
        }
    }
}

/// A table's retention ([`Schema::with_retain`]): rows whose TIMESTAMP
/// field `by` is more than `seconds` old are dropped as merges rewrite
/// their segments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retain {
    pub by: String,
    pub seconds: i64,
}

impl Retain {
    /// `text` as a retention period: a whole number and a unit, `400 days`,
    /// `12 hours`, `90d`, `1 week`.
    pub fn parse_period(text: &str) -> Result<i64> {
        let t = text.trim();
        let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
        let bad = || Error::Schema(format!("retain {text:?}: a period is a whole number and a unit (seconds, minutes, hours, days or weeks), as '400 days'"));
        let n: i64 = t[..digits].parse().map_err(|_| bad())?;
        let unit = match t[digits..].trim().to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 604_800,
            _ => return Err(bad()),
        };
        n.checked_mul(unit).filter(|&s| s > 0).ok_or_else(bad)
    }

    /// The period as text, in the largest unit it is a whole number of.
    pub fn period(&self) -> String {
        let (n, unit) = [(604_800, "weeks"), (86_400, "days"), (3600, "hours"), (60, "minutes")].into_iter().find(|(u, _)| self.seconds % u == 0).map_or((self.seconds, "seconds"), |(u, name)| (self.seconds / u, name));
        format!("{n} {unit}")
    }
}

impl Schema {
    pub fn new(table: &str, fields: Vec<Field>, primary_key: Option<&str>) -> Result<Self> {
        let s = Schema { table: table.to_string(), fields, primary_key: None, shard_by: None, cluster_by: None, retain: None, edges: None };
        let mut s = s;
        if let Some(pk) = primary_key {
            let i = s.field_index(pk).ok_or_else(|| Error::Schema(format!("no primary key field {pk}")))?;
            if !matches!(s.fields[i].ty, FieldType::Keyword | FieldType::Int) {
                return Err(Error::Schema("primary key must be KEYWORD or INT".into()));
            }
            s.primary_key = Some(i);
        }
        for f in &s.fields {
            for src in &f.generated_from {
                let ok = s.field(src).is_some_and(|g| g.stored && matches!(g.ty, FieldType::Text { .. } | FieldType::Keyword));
                if !ok || !matches!(f.ty, FieldType::Text { .. }) {
                    return Err(Error::Schema(format!("{} must be TEXT generated from stored text fields", f.name)));
                }
            }
        }
        if let Some(f) = s.unrebuildable() {
            return Err(Error::Schema(format!(
                "{} must be stored (or generated from stored fields): merges rebuild a TEXT or SPARSE index from the stored values",
                f.name
            )));
        }
        for (i, f) in s.fields.iter().enumerate() {
            if f.is_multi_vector() {
                if f.embed.is_some() || f.references.is_some() || f.not_null || f.default.is_some() {
                    return Err(Error::Schema(format!("multi-vector field {} takes its vectors from writes: no embed, references, NOT NULL or default", f.name)));
                }
                if f.pool.is_some_and(|p| !(p > 0.0 && p <= 1.0)) {
                    return Err(Error::Schema(format!("field {}: pool is a cosine similarity above 0 and at most 1", f.name)));
                }
                continue;
            }
            if f.keep_f32 || f.pool.is_some() {
                return Err(Error::Schema(format!("field {}: keep_f32 and pool are options of a multi-vector field (VECTOR(d)[])", f.name)));
            }
            if f.multi && (!matches!(f.ty, FieldType::Keyword | FieldType::Int) || !f.stored || s.primary_key == Some(i)) {
                return Err(Error::Schema(format!("multi-valued field {} must be a stored KEYWORD or INT, or a VECTOR, and not the primary key", f.name)));
            }
        }
        for f in s.fields.iter() {
            let Some(e) = &f.embed else { continue };
            let bad = |m: String| Err(Error::Schema(format!("field {}: {m}", f.name)));
            if !matches!(f.ty, FieldType::Vector { .. }) {
                return bad("only a VECTOR field can be embedded".into());
            }
            if e.from.is_empty() || e.model.is_empty() {
                return bad("an embedded field needs source fields and a model".into());
            }
            if let Some(m) = e.from.iter().find_map(|src| s.field(src).filter(|g| g.media.is_some())) {
                if e.from.len() > 1 {
                    return bad(format!("an embedded field embeds one MEDIA field ({}), or text fields, not both", m.name));
                }
                if !matches!(m.media, Some(MediaKind::Image | MediaKind::Audio)) {
                    return bad(format!("source {}: the MEDIA field an embedded field embeds is of kind image or audio", m.name));
                }
                continue;
            }
            for src in &e.from {
                let ok = s.field(src).is_some_and(|g| (g.stored || !g.generated_from.is_empty()) && matches!(g.ty, FieldType::Text { .. } | FieldType::Keyword) && !g.multi);
                if !ok {
                    return bad(format!("source {src} must be a stored or generated TEXT or KEYWORD field, or a MEDIA field of images or sound"));
                }
            }
        }
        if let Some(f) = s.fields.iter().find(|f| !f.codes.is_sq8() && !matches!(f.ty, FieldType::Vector { .. })) {
            return Err(Error::Schema(format!("field {} has vector codes but is not a VECTOR", f.name)));
        }
        if s.primary_key.is_none() {
            if let Some(f) = s.fields.iter().find(|f| f.ty == FieldType::Edges) {
                return Err(Error::Schema(format!("EDGES field {} needs a primary key", f.name)));
            }
        }
        for (i, f) in s.fields.iter().enumerate() {
            let Some(l) = f.logical else { continue };
            if f.ty != FieldType::Int {
                return Err(Error::Schema(format!("field {}: only an INT column holds {} values", f.name, l.type_name())));
            }
            if f.multi || s.primary_key == Some(i) || f.references.is_some() {
                return Err(Error::Schema(format!("field {}: a {} field holds one value, and is neither the primary key nor a reference", f.name, l.type_name())));
            }
        }
        s.check_media()?;
        s.check_derived()?;
        for (i, f) in s.fields.iter_mut().enumerate() {
            // The key always holds a value: saying so needs no feature.
            if s.primary_key == Some(i) {
                f.not_null = false;
            }
            if f.default.as_ref().is_some_and(serde_json::Value::is_null) {
                f.default = None;
            }
            // Every version analyses `english`: naming it needs no feature.
            if f.analyzer == Some(Analyzer::English) {
                f.analyzer = None;
            }
            if f.analyzer.is_some() && !matches!(f.ty, FieldType::Text { .. }) {
                return Err(Error::Schema(format!("field {}: only a TEXT field has an analyzer", f.name)));
            }
        }
        for (i, f) in s.fields.iter().enumerate() {
            if f.default.is_none() {
                continue;
            }
            let bad = |m: &str| Err(Error::Schema(format!("field {}: {m}", f.name)));
            if s.primary_key == Some(i) || f.multi || !f.generated_from.is_empty() || !matches!(f.ty, FieldType::Text { .. } | FieldType::Keyword | FieldType::Int | FieldType::Float) {
                return bad("only a single-valued TEXT, KEYWORD, INT, FLOAT, BOOL or TIMESTAMP field that is not the key or generated takes a default");
            }
            // A value of the field's type (ingest checks it as a row's).
            f.default_value(0)?;
        }
        s.check_semantic_ids()?;
        s.check_references()?;
        // Names are matched in any case, so they must differ in more than case.
        let mut names: Vec<String> = s.fields.iter().map(|f| f.name.to_ascii_lowercase()).collect();
        names.sort();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(Error::Schema(format!("duplicate field name {}", w[0])));
        }
        Ok(s)
    }

    /// Places this table's rows on shards by the value of `field` instead
    /// of their key: a coordinator writes each row to the shard that holds
    /// its value's slot, so that rows sharing a value (a user's events)
    /// live on one shard. `field` is a stored, single-valued KEYWORD or INT
    /// other than the key, and NOT NULL, so that every row has a place. It
    /// makes the database format 3 (feature `shard_by`).
    pub fn with_shard_by(mut self, field: &str) -> Result<Schema> {
        let i = self.field_index(field).ok_or_else(|| Error::Schema(format!("shard_by names {field}, which is not a field of {}", self.table)))?;
        let f = &self.fields[i];
        let bad = |m: &str| Err(Error::Schema(format!("shard_by {}: {m}", f.name)));
        if self.primary_key == Some(i) {
            return bad("rows are placed by their key already; leave shard_by out");
        }
        if f.multi || f.logical.is_some() || f.media.is_some() || !matches!(f.ty, FieldType::Keyword | FieldType::Int) || !f.stored {
            return bad("rows are placed by a stored, single-valued KEYWORD or INT field");
        }
        if !f.not_null {
            return bad("every row needs a place: declare the field NOT NULL (\"not_null\": true)");
        }
        self.shard_by = Some(f.name.clone());
        Ok(self)
    }

    /// The position of the field that places rows on shards
    /// ([`Schema::with_shard_by`]).
    pub fn shard_by_field(&self) -> Option<usize> {
        self.shard_by.as_deref().and_then(|f| self.field_index(f))
    }

    /// Checks the MEDIA fields ([`Field::media`]).
    fn check_media(&self) -> Result<()> {
        for (i, f) in self.fields.iter().enumerate() {
            if f.media.is_none() {
                continue;
            }
            let bad = |m: &str| Err(Error::Schema(format!("field {}: {m}", f.name)));
            if f.ty != FieldType::Keyword || f.logical.is_some() || f.analyzer.is_some() || !f.generated_from.is_empty() {
                return bad("a MEDIA field is a KEYWORD column of JSON values");
            }
            if f.multi || self.primary_key == Some(i) || f.references.is_some() || f.default.is_some() {
                return bad("a MEDIA field holds one file per row, and is neither the key, a reference nor given a default");
            }
            if !f.stored {
                return bad("a MEDIA field is stored: its value is the file's reference");
            }
            if let Some(g) = self.fields.iter().find(|g| g.generated_from.iter().any(|n| self.field_index(n) == Some(i))) {
                return Err(Error::Schema(format!("field {}: a MEDIA field ({}) is not a source of text", g.name, f.name)));
            }
        }
        Ok(())
    }

    /// Orders the rows of each of this table's segments by the value of
    /// `field` (rows without one first), when a flush writes the segment
    /// and when a merge rewrites it; rows of one value keep their order
    /// of writing. A user's events become one run of rows per segment, so
    /// reading a history takes a range of each segment rather than rows
    /// all over it. `field` is an indexed, single-valued KEYWORD or INT
    /// (TIMESTAMP included). It makes the database format 3 (feature
    /// `cluster_by`). Rows read without an ORDER BY come in this order.
    pub fn with_cluster_by(mut self, field: &str) -> Result<Schema> {
        let i = self.field_index(field).ok_or_else(|| Error::Schema(format!("cluster_by names {field}, which is not a field of {}", self.table)))?;
        let f = &self.fields[i];
        if f.multi || !f.indexed || f.media.is_some() || !matches!(f.ty, FieldType::Keyword | FieldType::Int) || matches!(f.logical, Some(Logical::Bool)) {
            return Err(Error::Schema(format!("cluster_by {}: rows are ordered by an indexed, single-valued KEYWORD, INT or TIMESTAMP field", f.name)));
        }
        self.cluster_by = Some(f.name.clone());
        Ok(self)
    }

    /// The position of the field that orders segments' rows
    /// ([`Schema::with_cluster_by`]).
    pub fn cluster_by_field(&self) -> Option<usize> {
        self.cluster_by.as_deref().and_then(|f| self.field_index(f))
    }

    /// Keeps this table's rows for `seconds` after the time in TIMESTAMP
    /// field `by`: older rows are dropped when a merge rewrites their
    /// segment (and not written by a flush), and segments that hold only
    /// such rows are dropped whole within seconds by the background merger.
    /// It is eventual: a query that needs an exact window filters on `by`.
    /// Rows without a time are kept. `by` is an indexed TIMESTAMP field. It
    /// makes the database format 3 (feature `retain`).
    pub fn with_retain(mut self, by: &str, seconds: i64) -> Result<Schema> {
        let i = self.field_index(by).ok_or_else(|| Error::Schema(format!("retain_by names {by}, which is not a field of {}", self.table)))?;
        let f = &self.fields[i];
        if f.logical != Some(Logical::Timestamp) || !f.indexed {
            return Err(Error::Schema(format!("retain_by {}: rows are kept by the time in an indexed TIMESTAMP field", f.name)));
        }
        if seconds <= 0 {
            return Err(Error::Schema(format!("retain: rows are kept for a positive time, not {seconds} seconds")));
        }
        self.retain = Some(Retain { by: f.name.clone(), seconds });
        Ok(self)
    }

    /// The position of the retention's TIMESTAMP field, and the time
    /// (microseconds since the epoch) before which rows expire at `now`
    /// ([`Schema::with_retain`]).
    pub fn retention(&self, now: i64) -> Option<(usize, i64)> {
        let r = self.retain.as_ref()?;
        Some((self.field_index(&r.by)?, now.saturating_sub(r.seconds.saturating_mul(1_000_000))))
    }

    /// Checks the derived fields ([`Field::derive`]): stored TEXT fields of
    /// a MEDIA field's file, with nothing else to say.
    fn check_derived(&self) -> Result<()> {
        for (i, f) in self.fields.iter().enumerate() {
            let Some(d) = &f.derive else { continue };
            let bad = |m: &str| Err(Error::Schema(format!("field {}: {m}", f.name)));
            if !matches!(f.ty, FieldType::Text { .. }) || !f.stored || !f.generated_from.is_empty() {
                return bad("a derived field is a stored TEXT field, not generated from others");
            }
            if self.primary_key == Some(i) || f.default.is_some() || f.not_null {
                return bad("a derived field is made from a file: it is not the key, and has no default or NOT NULL");
            }
            if d.model.is_empty() {
                return bad("a derived field names its model");
            }
            if !self.field(&d.from).is_some_and(|m| m.media.is_some()) {
                return bad(&format!("derive names {}, which is not a MEDIA field", d.from));
            }
        }
        Ok(())
    }

    /// Makes each row of this table an edge of a graph over the rows of
    /// the table that `src` and `dst` hold keys of: from the row whose key
    /// is in `src` to the row whose key is in `dst`, weighted by `weight`.
    /// A walk over the other table follows it as it follows an EDGES field
    /// (`ppr(cooc, seeds)` in SQL), so one edge is written, updated in
    /// place or deleted without rewriting the rows it links. Each segment
    /// holds a graph section of its rows, which flushes and merges build.
    /// `src` and `dst` are stored, single-valued KEYWORD or INT fields of
    /// one type; `weight` a stored, single-valued FLOAT or INT field, which
    /// may change in place (an edge is followed while its weight is
    /// positive). It makes the database format 3 (feature `edge_table`).
    pub fn with_edges(mut self, src: &str, dst: &str, weight: Option<&str>) -> Result<Schema> {
        let find = |name: &str, what: &str| self.field_index(name).ok_or_else(|| Error::Schema(format!("edges: the {what} {name} is not a field of {}", self.table)));
        let (s, d) = (find(src, "source")?, find(dst, "target")?);
        let w = weight.map(|w| find(w, "weight")).transpose()?;
        let plain = |f: &Field| f.stored && !f.multi && f.media.is_none() && f.logical.is_none();
        let (fs, fd) = (&self.fields[s], &self.fields[d]);
        if s == d || !plain(fs) || !plain(fd) || fs.ty != fd.ty || !matches!(fs.ty, FieldType::Keyword | FieldType::Int) {
            return Err(Error::Schema(format!("edges {src}, {dst}: the source and target are two stored, single-valued KEYWORD or INT fields of one type")));
        }
        if let Some(w) = w {
            let f = &self.fields[w];
            if w == s || w == d || !plain(f) || !matches!(f.ty, FieldType::Float | FieldType::Int) {
                return Err(Error::Schema(format!("edges: the weight {} is a stored, single-valued FLOAT or INT field", f.name)));
            }
        }
        let name = |i: usize| self.fields[i].name.clone();
        self.edges = Some(Edges { src: name(s), dst: name(d), weight: w.map(name) });
        Ok(self)
    }

    /// The positions of an edge table's source, target and weight fields
    /// ([`Schema::with_edges`]).
    pub fn edge_fields(&self) -> Option<(usize, usize, Option<usize>)> {
        let e = self.edges.as_ref()?;
        let w = match &e.weight {
            Some(w) => Some(self.field_index(w)?),
            None => None,
        };
        Some((self.field_index(&e.src)?, self.field_index(&e.dst)?, w))
    }

    /// An indexed TEXT or SPARSE field that is neither stored nor
    /// generated: nothing can rebuild its index when segments merge.
    /// [`Schema::new`] refuses them; databases created before it did can
    /// still hold one.
    pub fn unrebuildable(&self) -> Option<&Field> {
        self.fields.iter().find(|f| f.indexed && !f.stored && f.generated_from.is_empty() && matches!(f.ty, FieldType::Text { .. } | FieldType::Sparse))
    }

    /// Checks the semantic-ID fields: stored, indexed KEYWORD[] fields of a
    /// VECTOR field's codes, with nothing else to say.
    fn check_semantic_ids(&self) -> Result<()> {
        for (i, f) in self.fields.iter().enumerate() {
            let Some(sid) = &f.semantic_id else { continue };
            let bad = |m: &str| Err(Error::Schema(format!("field {}: {m}", f.name)));
            if f.ty != FieldType::Keyword || !f.multi || !f.stored || !f.indexed {
                return bad("a semantic-ID field is a stored, indexed KEYWORD[]");
            }
            if self.primary_key == Some(i) || f.references.is_some() || f.default.is_some() || f.not_null || !f.generated_from.is_empty() {
                return bad("a semantic-ID field is derived: it is not the key, references nothing, and has no default or NOT NULL");
            }
            if sid.codebook.is_empty() {
                return bad("a semantic-ID field names its codebook");
            }
            if !self.field(&sid.from).is_some_and(|v| matches!(v.ty, FieldType::Vector { .. })) {
                return bad(&format!("semantic_id names {}, which is not a VECTOR field", sid.from));
            }
            if self.field(&sid.from).is_some_and(Field::is_multi_vector) {
                return bad(&format!("semantic_id codes one vector a row: {} is a multi-vector field", sid.from));
            }
        }
        Ok(())
    }

    /// Checks the fields that reference a table, and spells a reference to
    /// this table as its name. References to other tables are checked
    /// against them when the table joins a database
    /// ([`crate::storage::Database::create_table`]).
    fn check_references(&mut self) -> Result<()> {
        let pk_ty = self.primary_key.map(|k| self.fields[k].ty.clone());
        let table = self.table.clone();
        for f in &mut self.fields {
            let Some(r) = &f.references else { continue };
            let bad = |m: String| Err(Error::Schema(format!("field {}: {m}", f.name)));
            if r.is_empty() {
                return bad("references names no table".into());
            }
            let own = r.eq_ignore_ascii_case(&table);
            match f.ty {
                FieldType::Keyword | FieldType::Int => {}
                FieldType::Edges if own => {}
                FieldType::Edges => return bad(format!("an EDGES field links rows of its own table, not of {r}")),
                _ => return bad("only a KEYWORD, INT or EDGES field references a table".into()),
            }
            if own {
                match &pk_ty {
                    None => return bad(format!("references {table}, which has no primary key")),
                    Some(t) if f.ty != FieldType::Edges && *t != f.ty => return bad(format!("references {table}, whose primary key is not {}", type_name(&f.ty))),
                    _ => {}
                }
                f.references = Some(table.clone());
            }
        }
        Ok(())
    }

    /// The format 3 features this table needs, as the manifest names them:
    /// its fields' [`Logical::feature`], `not_null`, `default`,
    /// `multi_vector`, `semantic_id`, [`Analyzer::feature`], `media`
    /// ([`Field::media`]) and `derive` ([`Field::derive`]), and its options
    /// `shard_by`, `cluster_by`, `retain` and `edge_table`
    /// ([`Schema::with_shard_by`], [`Schema::with_cluster_by`],
    /// [`Schema::with_retain`], [`Schema::with_edges`]).
    pub fn features(&self) -> impl Iterator<Item = String> + '_ {
        let fields = self.fields.iter().flat_map(|f| {
            let named = [
                f.logical.map(Logical::feature),
                f.not_null.then_some("not_null"),
                f.default.as_ref().map(|_| "default"),
                f.is_multi_vector().then_some("multi_vector"),
                f.semantic_id.as_ref().map(|_| "semantic_id"),
                f.media.map(|_| "media"),
                f.derive.as_ref().map(|_| "derive"),
            ];
            named.into_iter().flatten().map(String::from).chain(f.analyzer.and_then(Analyzer::feature))
        });
        let options = [self.shard_by.as_ref().map(|_| "shard_by"), self.cluster_by.as_ref().map(|_| "cluster_by"), self.retain.as_ref().map(|_| "retain"), self.edges.as_ref().map(|_| "edge_table")];
        fields.chain(options.into_iter().flatten().map(String::from))
    }

    /// A row that holds each field's default, written at `now`
    /// (microseconds since the epoch), and NULL in the others: what a write
    /// that gives no values stores. SQL `INSERT`s and rows read from JSON
    /// or CSV start from it; a row given to the Rust API holds what it
    /// holds.
    pub fn defaults(&self, now: i64) -> Result<Vec<crate::value::Value>> {
        self.fields.iter().map(|f| f.default_value(now)).collect()
    }

    /// Whether any field has a default ([`Schema::defaults`]).
    pub fn has_defaults(&self) -> bool {
        self.fields.iter().any(|f| f.default.is_some())
    }

    /// The fields that reference table `table` (any case), by position.
    pub fn referencing(&self, table: &str) -> impl Iterator<Item = usize> + '_ {
        let table = table.to_string();
        self.fields.iter().enumerate().filter(move |(_, f)| f.references.as_ref().is_some_and(|r| r.eq_ignore_ascii_case(&table))).map(|(i, _)| i)
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.field_index(name).map(|i| &self.fields[i])
    }

    /// Fills generated fields of `doc` from their sources.
    pub fn fill_generated(&self, doc: &mut [crate::value::Value]) {
        use crate::value::Value;
        for (i, f) in self.fields.iter().enumerate() {
            if f.generated_from.is_empty() {
                continue;
            }
            doc[i] = Value::Str(self.joined(&f.generated_from, doc));
        }
    }

    /// The text embedded field `f` is the embedding of, in `doc` (generated
    /// fields filled in).
    /// For a field that embeds a MEDIA field ([`Schema::embeds_media`]),
    /// that field's value.
    pub fn embed_text(&self, f: usize, doc: &[crate::value::Value]) -> String {
        self.fields[f].embed.as_ref().map(|e| self.joined(&e.from, doc)).unwrap_or_default()
    }

    /// The MEDIA field that field `f` is the embedding of, if it embeds
    /// one (images or sound) rather than text.
    pub fn embeds_media(&self, f: usize) -> Option<usize> {
        let e = self.fields[f].embed.as_ref()?;
        let i = self.field_index(e.from.first()?)?;
        self.fields[i].media.is_some().then_some(i)
    }

    /// The MEDIA field that field `f` is derived from ([`Field::derive`]).
    pub fn derives_from(&self, f: usize) -> Option<usize> {
        self.field_index(&self.fields[f].derive.as_ref()?.from)
    }

    /// The non-empty values of `sources` in `doc`, joined by newlines.
    fn joined(&self, sources: &[String], doc: &[crate::value::Value]) -> String {
        let parts: Vec<&str> = sources.iter().filter_map(|src| self.field_index(src).and_then(|j| doc[j].as_str())).filter(|s| !s.is_empty()).collect();
        parts.join("\n")
    }
}

/// A field type's name in SQL and in the short JSON form.
pub fn type_name(t: &FieldType) -> &'static str {
    match t {
        FieldType::Text { .. } => "TEXT",
        FieldType::Keyword => "KEYWORD",
        FieldType::Int => "INT",
        FieldType::Float => "FLOAT",
        FieldType::Vector { .. } => "VECTOR",
        FieldType::Edges => "EDGES",
        FieldType::Sparse => "SPARSE",
    }
}

// ------------------------------------------------------------- JSON forms

/// The HNSW parameters a vector field gets when its JSON names `"hnsw"`
/// without them.
pub const DEFAULT_HNSW: VectorIndexKind = VectorIndexKind::Hnsw { m: 16, ef_construction: 200 };

impl Schema {
    /// A schema from JSON, in either form, validated as [`Schema::new`] does.
    ///
    /// The short form names types and the key, and leaves defaults out:
    ///
    /// ```json
    /// {"table": "products", "key": "id", "fields": [
    ///   {"name": "id", "type": "keyword"},
    ///   {"name": "title", "type": "text"},
    ///   {"name": "tags", "type": "keyword", "multi": true},
    ///   {"name": "price", "type": "float"},
    ///   {"name": "body", "type": "text", "from": ["title", "description"]},
    ///   {"name": "emb", "type": "vector", "dim": 512,
    ///    "embed": {"from": ["title"], "model": "potion-retrieval-32M"}}
    /// ]}
    /// ```
    ///
    /// Types are `text`, `keyword`, `int`, `float`, `bool`, `timestamp`,
    /// `vector`, `sparse`, `edges` and `media` (`bool` and `timestamp` hold
    /// one value, and make the database format 3: [`Logical`]; `media`
    /// holds a file's reference, of the `"kind"` `"image"`, `"audio"`,
    /// `"video"` or `"pdf"` when it says so, and makes the database format
    /// 3: [`Field::media`]). A vector `embed`ded by a model Strata knows may leave out
    /// `dim`. Every field is stored and indexed unless it says otherwise
    /// (`"stored": false`, `"indexed": false`), except that vectors and
    /// generated text (`from`) are not stored. Text keeps positions (for
    /// phrases) unless `"positions": false`, and is analysed by
    /// `"analyzer"`: `"english"` (the default) or `"simple"`, version 1, or
    /// `{"name": .., "version": ..}` ([`Analyzer`]; another than `english`
    /// makes the database format 3). A vector's `index` is
    /// `"hnsw"` (the default, m 16 and ef_construction 200), `"flat"`,
    /// `{"hnsw": {"m": .., "ef_construction": ..}}` or
    /// `{"ivf": {"nlist": ..}}`, and its `codes` `"sq8"` (default) or
    /// `"binary"`. A KEYWORD or INT field may name the table whose primary
    /// keys it holds (`"references": "authors"`; see [`Field::references`]).
    /// `"not_null": true` refuses rows without a value, and `"default"` is
    /// the value a row that leaves the field out takes (`"now"` for a
    /// TIMESTAMP is the time of the write); either makes the database
    /// format 3 ([`Field::not_null`], [`Field::default`]). `"shard_by":
    /// "usr"` places the table's rows on shards by that field rather than
    /// their key ([`Schema::with_shard_by`]), `"cluster_by": "usr"` orders
    /// each segment's rows by it ([`Schema::with_cluster_by`]), and
    /// `"retain": "400 days"` keeps rows for that long after the time in
    /// `"retain_by"` (default: the table's only TIMESTAMP field;
    /// [`Schema::with_retain`]). `"edges": "src, dst, w"` makes each row an
    /// edge ([`Schema::with_edges`]).
    /// A text field may be made from a media field's file by a model,
    /// `"derive": {"from": "image", "model": "blip-image-captioning-base"}`,
    /// which makes the database format 3 ([`Field::derive`]).
    ///
    /// The long form is this type's own serialization (what
    /// `GET /v1/schema` returns), with `primary_key` the key field's
    /// position or name.
    pub fn from_json(text: &str) -> Result<Schema> {
        let j: serde_json::Value = serde_json::from_str(text).map_err(|e| Error::Schema(format!("schema JSON: {e}")))?;
        Schema::from_json_value(&j)
    }

    pub fn from_json_value(j: &serde_json::Value) -> Result<Schema> {
        use serde_json::Value as J;
        let bad = |m: String| Error::Schema(m);
        let obj = j.as_object().ok_or_else(|| bad("a schema is a JSON object".into()))?;
        let table = obj.get("table").and_then(J::as_str).ok_or_else(|| bad("a schema needs a \"table\" name".into()))?;
        let fields = obj.get("fields").and_then(J::as_array).ok_or_else(|| bad("a schema needs a \"fields\" array".into()))?;
        let long = fields.iter().any(|f| f.get("ty").is_some());
        let fields: Vec<Field> = if long {
            fields.iter().map(|f| serde_json::from_value(f.clone()).map_err(|e| bad(format!("field {}: {e}", f.get("name").unwrap_or(&J::Null))))).collect::<Result<_>>()?
        } else {
            fields.iter().map(short_field).collect::<Result<_>>()?
        };
        let key = match obj.get("key").or_else(|| obj.get("primary_key")) {
            None | Some(J::Null) => None,
            Some(J::String(s)) => Some(s.clone()),
            Some(J::Number(n)) => {
                let i = n.as_u64().map(|i| i as usize).filter(|&i| i < fields.len()).ok_or_else(|| bad(format!("primary_key {n} is not a field's position")))?;
                Some(fields[i].name.clone())
            }
            Some(other) => return Err(bad(format!("the key is a field name, not {other}"))),
        };
        let mut schema = Schema::new(table, fields, key.as_deref())?;
        let name = |k: &str| match obj.get(k) {
            None | Some(J::Null) => Ok(None),
            Some(J::String(f)) => Ok(Some(f.as_str())),
            Some(other) => Err(bad(format!("{k} is a field name, not {other}"))),
        };
        if let Some(f) = name("shard_by")? {
            schema = schema.with_shard_by(f)?;
        }
        if let Some(f) = name("cluster_by")? {
            schema = schema.with_cluster_by(f)?;
        }
        match obj.get("retain") {
            None | Some(J::Null) => {
                if obj.get("retain_by").is_some_and(|v| !v.is_null()) {
                    return Err(bad("retain_by names the field that retain keeps rows by: give retain too, as \"400 days\"".into()));
                }
            }
            // The short form: a period, and the field (or the table's only
            // TIMESTAMP).
            Some(J::String(period)) => {
                let seconds = Retain::parse_period(period)?;
                let by = match name("retain_by")? {
                    Some(f) => f.to_string(),
                    None => {
                        let times: Vec<&Field> = schema.fields.iter().filter(|f| f.logical == Some(Logical::Timestamp)).collect();
                        match times[..] {
                            [f] => f.name.clone(),
                            _ => return Err(bad(format!("retain keeps rows by a TIMESTAMP field: name it with retain_by ({} has {})", schema.table, times.len()))),
                        }
                    }
                };
                schema = schema.with_retain(&by, seconds)?;
            }
            Some(r @ J::Object(_)) => {
                let r: Retain = serde_json::from_value(r.clone()).map_err(|e| bad(format!("retain: {e}")))?;
                schema = schema.with_retain(&r.by, r.seconds)?;
            }
            Some(other) => return Err(bad(format!("retain is a period, as \"400 days\", not {other}"))),
        }
        match obj.get("edges") {
            None | Some(J::Null) => {}
            Some(J::String(text)) => {
                let e = Edges::parse(text)?;
                schema = schema.with_edges(&e.src, &e.dst, e.weight.as_deref())?;
            }
            Some(e @ J::Object(_)) => {
                let e: Edges = serde_json::from_value(e.clone()).map_err(|e| bad(format!("edges: {e}")))?;
                schema = schema.with_edges(&e.src, &e.dst, e.weight.as_deref())?;
            }
            Some(other) => return Err(bad(format!("edges names the source, target and weight fields, as \"src, dst, w\", not {other}"))),
        }
        Ok(schema)
    }

    /// The short JSON form ([`Schema::from_json`]), defaults left out.
    pub fn to_short_json(&self) -> serde_json::Value {
        use serde_json::{json, Map, Value as J};
        let fields: Vec<J> = self
            .fields
            .iter()
            .map(|f| {
                let mut m = Map::new();
                m.insert("name".into(), json!(f.name));
                let (ty, stored_default) = match &f.ty {
                    FieldType::Text { positions } => {
                        if !positions {
                            m.insert("positions".into(), json!(false));
                        }
                        ("text", f.generated_from.is_empty())
                    }
                    FieldType::Keyword if f.media.is_some() => {
                        if let Some(k) = f.media.filter(|k| *k != MediaKind::Any) {
                            m.insert("kind".into(), json!(k.as_str()));
                        }
                        ("media", true)
                    }
                    FieldType::Keyword => ("keyword", true),
                    FieldType::Int => (f.logical.map_or("int", Logical::feature), true),
                    FieldType::Float => ("float", true),
                    FieldType::Vector { dim, index } => {
                        // A known model's vectors have its dimension.
                        if f.embed.as_ref().and_then(|e| crate::embed::known_dim(&e.model)) != Some(*dim) {
                            m.insert("dim".into(), json!(dim));
                        }
                        if *index != DEFAULT_HNSW {
                            m.insert(
                                "index".into(),
                                match index {
                                    VectorIndexKind::Flat => json!("flat"),
                                    VectorIndexKind::Hnsw { m, ef_construction } => json!({"hnsw": {"m": m, "ef_construction": ef_construction}}),
                                    VectorIndexKind::Ivf { nlist } => json!({"ivf": {"nlist": nlist}}),
                                },
                            );
                        }
                        // A multi-vector field's codes are binary unless it says.
                        if f.codes != if f.multi { VectorCodes::Binary } else { VectorCodes::Sq8 } {
                            m.insert("codes".into(), json!(f.codes.as_str()));
                        }
                        if f.keep_f32 {
                            m.insert("keep_f32".into(), json!(true));
                        }
                        if let Some(p) = f.pool {
                            m.insert("pool".into(), json!(p));
                        }
                        if let Some(e) = &f.embed {
                            m.insert("embed".into(), json!({"from": e.from, "model": e.model}));
                        }
                        ("vector", false)
                    }
                    FieldType::Sparse => ("sparse", true),
                    FieldType::Edges => ("edges", true),
                };
                m.insert("type".into(), json!(ty));
                if f.multi {
                    m.insert("multi".into(), json!(true));
                }
                if !f.generated_from.is_empty() {
                    m.insert("from".into(), json!(f.generated_from));
                }
                if f.stored != stored_default {
                    m.insert("stored".into(), json!(f.stored));
                }
                if !f.indexed {
                    m.insert("indexed".into(), json!(false));
                }
                if let Some(r) = &f.references {
                    m.insert("references".into(), json!(r));
                }
                if f.not_null {
                    m.insert("not_null".into(), json!(true));
                }
                if let Some(d) = &f.default {
                    m.insert("default".into(), d.clone());
                }
                if let Some(s) = &f.semantic_id {
                    m.insert("semantic_id".into(), serde_json::to_value(s).expect("serialises"));
                }
                if let Some(d) = &f.derive {
                    m.insert("derive".into(), json!({"from": d.from, "model": d.model}));
                }
                if let Some(a) = f.analyzer {
                    // Version 1 by its name alone, as short_field reads it.
                    m.insert("analyzer".into(), if a.version() == 1 { json!(a.name()) } else { json!(a) });
                }
                // "name" and "type" first, for people reading it.
                let mut out = Map::new();
                for k in ["name", "type"] {
                    out.insert(k.into(), m.remove(k).unwrap());
                }
                out.extend(m);
                J::Object(out)
            })
            .collect();
        let mut out = Map::new();
        out.insert("table".into(), json!(self.table));
        if let Some(k) = self.primary_key {
            out.insert("key".into(), json!(self.fields[k].name));
        }
        if let Some(f) = &self.shard_by {
            out.insert("shard_by".into(), json!(f));
        }
        if let Some(f) = &self.cluster_by {
            out.insert("cluster_by".into(), json!(f));
        }
        if let Some(r) = &self.retain {
            out.insert("retain".into(), json!(r.period()));
            out.insert("retain_by".into(), json!(r.by));
        }
        if let Some(e) = &self.edges {
            out.insert("edges".into(), json!(e.text()));
        }
        out.insert("fields".into(), J::Array(fields));
        J::Object(out)
    }
}

/// One field of the short JSON form.
fn short_field(j: &serde_json::Value) -> Result<Field> {
    use serde_json::Value as J;
    let name = j.get("name").and_then(J::as_str).ok_or_else(|| Error::Schema(format!("a field needs a \"name\": {j}")))?;
    let bad = |m: &str| Error::Schema(format!("field {name}: {m}"));
    let known = ["name", "type", "stored", "indexed", "multi", "positions", "from", "dim", "index", "codes", "keep_f32", "pool", "embed", "references", "not_null", "default", "analyzer", "kind", "semantic_id", "derive"];
    if let Some(k) = j.as_object().into_iter().flat_map(|o| o.keys()).find(|k| !known.contains(&k.as_str())) {
        return Err(bad(&format!("unknown option {k:?} (known: {})", known.join(", "))));
    }
    let flag = |k: &str| -> Result<Option<bool>> {
        match j.get(k) {
            None | Some(J::Null) => Ok(None),
            Some(J::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(bad(&format!("{k} is true or false"))),
        }
    };
    let names = |k: &str| -> Result<Vec<String>> {
        match j.get(k) {
            None | Some(J::Null) => Ok(Vec::new()),
            Some(J::String(s)) => Ok(vec![s.clone()]),
            Some(J::Array(xs)) => xs.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| bad(&format!("{k} lists field names")))).collect(),
            Some(_) => Err(bad(&format!("{k} lists field names"))),
        }
    };
    let ty_name = j.get("type").and_then(J::as_str).ok_or_else(|| bad("needs a \"type\" (text, keyword, int, float, bool, timestamp, vector, sparse, edges or media)"))?;
    let mut logical = None;
    let mut media = None;
    if j.get("kind").is_some_and(|k| !k.is_null()) && !ty_name.eq_ignore_ascii_case("media") {
        return Err(bad("kind is an option of a media field"));
    }
    let from = names("from")?;
    let ty = match ty_name.to_ascii_lowercase().as_str() {
        "text" => FieldType::Text { positions: flag("positions")?.unwrap_or(true) },
        "keyword" => FieldType::Keyword,
        "int" | "integer" => FieldType::Int,
        "bool" | "boolean" => {
            logical = Some(Logical::Bool);
            FieldType::Int
        }
        "timestamp" => {
            logical = Some(Logical::Timestamp);
            FieldType::Int
        }
        "float" | "double" | "real" => FieldType::Float,
        "sparse" => FieldType::Sparse,
        "edges" => FieldType::Edges,
        "media" => {
            media = Some(match j.get("kind") {
                None | Some(J::Null) => MediaKind::Any,
                Some(k) => k.as_str().and_then(MediaKind::parse).ok_or_else(|| bad("kind is \"image\", \"audio\", \"video\", \"pdf\" or \"any\""))?,
            });
            FieldType::Keyword
        }
        "vector" => {
            let model_dim = j.get("embed").and_then(|e| e.get("model")).and_then(J::as_str).and_then(crate::embed::known_dim);
            let dim = match j.get("dim") {
                None | Some(J::Null) => model_dim.ok_or_else(|| bad("a vector needs \"dim\", its number of dimensions"))?,
                Some(d) => d.as_u64().filter(|&d| d > 0).ok_or_else(|| bad("dim is a positive number"))? as usize,
            };
            let index = match j.get("index") {
                None | Some(J::Null) => DEFAULT_HNSW,
                Some(J::String(s)) if s.eq_ignore_ascii_case("flat") => VectorIndexKind::Flat,
                Some(J::String(s)) if s.eq_ignore_ascii_case("hnsw") => DEFAULT_HNSW,
                Some(J::Object(o)) if o.contains_key("hnsw") => {
                    let p = &o["hnsw"];
                    let get = |k: &str, d: usize| p.get(k).and_then(J::as_u64).map_or(d, |v| v as usize);
                    VectorIndexKind::Hnsw { m: get("m", 16), ef_construction: get("ef_construction", 200) }
                }
                Some(J::Object(o)) if o.contains_key("ivf") => {
                    let nlist = o["ivf"].get("nlist").and_then(J::as_u64).ok_or_else(|| bad("ivf needs nlist"))? as usize;
                    VectorIndexKind::Ivf { nlist }
                }
                Some(other) => return Err(bad(&format!("index is \"hnsw\", \"flat\", {{\"hnsw\": {{..}}}} or {{\"ivf\": {{\"nlist\": n}}}}, not {other}"))),
            };
            FieldType::Vector { dim, index }
        }
        other => return Err(bad(&format!("unknown type {other:?} (text, keyword, int, float, bool, timestamp, vector, sparse, edges or media)"))),
    };
    let is_vector = matches!(ty, FieldType::Vector { .. });
    let mut f = Field::new(name, ty);
    f.logical = logical;
    f.media = media;
    f.generated_from = from;
    f.stored = flag("stored")?.unwrap_or(!is_vector && f.generated_from.is_empty());
    f.indexed = flag("indexed")?.unwrap_or(true);
    if flag("multi")?.unwrap_or(false) {
        f = f.multi();
    }
    f.keep_f32 = flag("keep_f32")?.unwrap_or(false);
    f.pool = match j.get("pool") {
        None | Some(J::Null) => None,
        Some(p) => Some(p.as_f64().ok_or_else(|| bad("pool is a cosine similarity, such as 0.9"))? as f32),
    };
    if let Some(c) = j.get("codes").filter(|c| !c.is_null()) {
        f.codes = c.as_str().and_then(VectorCodes::parse).ok_or_else(|| bad("codes is \"sq8\" or \"binary\""))?;
    }
    match j.get("references") {
        None | Some(J::Null) => {}
        Some(J::String(t)) => f.references = Some(t.clone()),
        Some(_) => return Err(bad("references names a table")),
    }
    f.not_null = flag("not_null")?.unwrap_or(false);
    f.default = j.get("default").filter(|d| !d.is_null()).cloned();
    f.analyzer = match j.get("analyzer") {
        None | Some(J::Null) => None,
        Some(J::String(n)) => Some(Analyzer::of(&n.to_ascii_lowercase(), 1).map_err(|e| bad(&e))?),
        Some(a @ J::Object(_)) => Some(serde_json::from_value(a.clone()).map_err(|e| bad(&e.to_string()))?),
        Some(_) => return Err(bad("analyzer is a name (\"english\" or \"simple\") or {\"name\": .., \"version\": ..}")),
    };
    match j.get("semantic_id") {
        None | Some(J::Null) => {}
        Some(s) => f.semantic_id = Some(serde_json::from_value(s.clone()).map_err(|e| bad(&format!("semantic_id is {{\"from\": vector field, \"codebook\": name}}: {e}")))?),
    }
    match j.get("derive") {
        None | Some(J::Null) => {}
        Some(d) => f.derive = Some(serde_json::from_value(d.clone()).map_err(|e| bad(&format!("derive is {{\"from\": media field, \"model\": name}}: {e}")))?),
    }
    match j.get("embed") {
        None | Some(J::Null) => {}
        Some(e) => {
            let from = match e.get("from") {
                Some(J::String(s)) => vec![s.clone()],
                Some(J::Array(xs)) => xs.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                _ => return Err(bad("embed needs \"from\", the fields it embeds")),
            };
            let model = e.get("model").and_then(J::as_str).ok_or_else(|| bad("embed needs a \"model\""))?;
            f.embed = Some(Embed { from, model: model.to_string() });
        }
    }
    Ok(f)
}
