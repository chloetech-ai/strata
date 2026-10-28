//! Tables and field types.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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
        Field { name: name.to_string(), ty, stored: true, indexed: true, generated_from: Vec::new(), multi: false, codes: VectorCodes::Sq8, embed: None }
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
        let mut names: Vec<&str> = s.fields.iter().map(|f| f.name.as_str()).collect();
        names.sort();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::Schema("duplicate field name".into()));
        }
        Ok(s)
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
