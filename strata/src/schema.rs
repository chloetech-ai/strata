//! Tables and field types.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
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
    /// ranges are runs of codes), and stored.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi: bool,
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
}

/// A VECTOR field that is the embedding of other fields: `model`'s vector
/// of their non-empty values joined by newlines (the text a generated
/// field holds). Writes that leave the vector out get it computed, and
/// rewrites that change a source recompute it; a vector a write supplies
/// is kept as given. Queries embed their text with the same model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Embed {
    /// Stored or generated TEXT or KEYWORD fields.
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
            codes: VectorCodes::Sq8,
            embed: None,
            references: None,
            logical: None,
            not_null: false,
            default: None,
            analyzer: None,
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
    /// The field's type in SQL: `BOOL` and `TIMESTAMP` for those
    /// ([`Logical`]), else [`type_name`] of its type.
    pub fn type_name(&self) -> &'static str {
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
    /// Makes a KEYWORD or INT field multi-valued.
    pub fn multi(mut self) -> Self {
        self.multi = true;
        self
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
}

impl Schema {
    pub fn new(table: &str, fields: Vec<Field>, primary_key: Option<&str>) -> Result<Self> {
        let s = Schema { table: table.to_string(), fields, primary_key: None };
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
            if f.multi && (!matches!(f.ty, FieldType::Keyword | FieldType::Int) || !f.stored || s.primary_key == Some(i)) {
                return Err(Error::Schema(format!("multi-valued field {} must be a stored KEYWORD or INT, not the primary key", f.name)));
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
            for src in &e.from {
                let ok = s.field(src).is_some_and(|g| (g.stored || !g.generated_from.is_empty()) && matches!(g.ty, FieldType::Text { .. } | FieldType::Keyword) && !g.multi);
                if !ok {
                    return bad(format!("source {src} must be a stored or generated TEXT or KEYWORD field"));
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
        s.check_references()?;
        // Names are matched in any case, so they must differ in more than case.
        let mut names: Vec<String> = s.fields.iter().map(|f| f.name.to_ascii_lowercase()).collect();
        names.sort();
        if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(Error::Schema(format!("duplicate field name {}", w[0])));
        }
        Ok(s)
    }

    /// An indexed TEXT or SPARSE field that is neither stored nor
    /// generated: nothing can rebuild its index when segments merge.
    /// [`Schema::new`] refuses them; databases created before it did can
    /// still hold one.
    pub fn unrebuildable(&self) -> Option<&Field> {
        self.fields.iter().find(|f| f.indexed && !f.stored && f.generated_from.is_empty() && matches!(f.ty, FieldType::Text { .. } | FieldType::Sparse))
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

    /// The format 3 features this table's fields need, as the manifest
    /// names them: [`Logical::feature`], `not_null`, `default` and
    /// [`Analyzer::feature`].
    pub fn features(&self) -> impl Iterator<Item = String> + '_ {
        self.fields.iter().flat_map(|f| {
            let named = [f.logical.map(Logical::feature), f.not_null.then_some("not_null"), f.default.as_ref().map(|_| "default")];
            named.into_iter().flatten().map(String::from).chain(f.analyzer.and_then(Analyzer::feature))
        })
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
    pub fn embed_text(&self, f: usize, doc: &[crate::value::Value]) -> String {
        self.fields[f].embed.as_ref().map(|e| self.joined(&e.from, doc)).unwrap_or_default()
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
    /// `vector`, `sparse` and `edges` (`bool` and `timestamp` hold one
    /// value, and make the database format 3: [`Logical`]). A vector `embed`ded by a model Strata knows may leave out
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
    /// format 3 ([`Field::not_null`], [`Field::default`]).
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
        Schema::new(table, fields, key.as_deref())
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
                        if !f.codes.is_sq8() {
                            m.insert("codes".into(), json!(f.codes.as_str()));
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
        out.insert("fields".into(), J::Array(fields));
        J::Object(out)
    }
}

/// One field of the short JSON form.
fn short_field(j: &serde_json::Value) -> Result<Field> {
    use serde_json::Value as J;
    let name = j.get("name").and_then(J::as_str).ok_or_else(|| Error::Schema(format!("a field needs a \"name\": {j}")))?;
    let bad = |m: &str| Error::Schema(format!("field {name}: {m}"));
    let known = ["name", "type", "stored", "indexed", "multi", "positions", "from", "dim", "index", "codes", "embed", "references", "not_null", "default", "analyzer"];
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
    let ty_name = j.get("type").and_then(J::as_str).ok_or_else(|| bad("needs a \"type\" (text, keyword, int, float, bool, timestamp, vector, sparse or edges)"))?;
    let mut logical = None;
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
        other => return Err(bad(&format!("unknown type {other:?} (text, keyword, int, float, bool, timestamp, vector, sparse or edges)"))),
    };
    let is_vector = matches!(ty, FieldType::Vector { .. });
    let mut f = Field::new(name, ty);
    f.logical = logical;
    f.generated_from = from;
    f.stored = flag("stored")?.unwrap_or(!is_vector && f.generated_from.is_empty());
    f.indexed = flag("indexed")?.unwrap_or(true);
    f.multi = flag("multi")?.unwrap_or(false);
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
