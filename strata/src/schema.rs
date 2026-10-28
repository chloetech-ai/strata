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
        let mut names: Vec<&str> = s.fields.iter().map(|f| f.name.as_str()).collect();
        names.sort();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::Schema("duplicate field name".into()));
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
    /// Types are `text`, `keyword`, `int`, `float`, `vector`, `sparse` and
    /// `edges`. A vector `embed`ded by a model Strata knows may leave out
    /// `dim`. Every field is stored and indexed unless it says otherwise
    /// (`"stored": false`, `"indexed": false`), except that vectors and
    /// generated text (`from`) are not stored. Text keeps positions (for
    /// phrases) unless `"positions": false`. A vector's `index` is
    /// `"hnsw"` (the default, m 16 and ef_construction 200), `"flat"`,
    /// `{"hnsw": {"m": .., "ef_construction": ..}}` or
    /// `{"ivf": {"nlist": ..}}`, and its `codes` `"sq8"` (default) or
    /// `"binary"`.
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
                    FieldType::Int => ("int", true),
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
    let known = ["name", "type", "stored", "indexed", "multi", "positions", "from", "dim", "index", "codes", "embed"];
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
    let ty_name = j.get("type").and_then(J::as_str).ok_or_else(|| bad("needs a \"type\" (text, keyword, int, float, vector, sparse or edges)"))?;
    let from = names("from")?;
    let ty = match ty_name.to_ascii_lowercase().as_str() {
        "text" => FieldType::Text { positions: flag("positions")?.unwrap_or(true) },
        "keyword" => FieldType::Keyword,
        "int" | "integer" => FieldType::Int,
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
        other => return Err(bad(&format!("unknown type {other:?} (text, keyword, int, float, vector, sparse or edges)"))),
    };
    let is_vector = matches!(ty, FieldType::Vector { .. });
    let mut f = Field::new(name, ty);
    f.generated_from = from;
    f.stored = flag("stored")?.unwrap_or(!is_vector && f.generated_from.is_empty());
    f.indexed = flag("indexed")?.unwrap_or(true);
    f.multi = flag("multi")?.unwrap_or(false);
    if let Some(c) = j.get("codes").filter(|c| !c.is_null()) {
        f.codes = c.as_str().and_then(VectorCodes::parse).ok_or_else(|| bad("codes is \"sq8\" or \"binary\""))?;
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
