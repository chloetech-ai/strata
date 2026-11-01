//! The manifest: one committed version of the database's on-disk state,
//! stored as JSON in its own extent and named by a meta slot.
//!
//! A database holds one or more tables. With one table (the one it was
//! created with) the manifest keeps format 1, the shape every database had
//! before tables were added: `{schema, txn, wal_txn, next_segment_id,
//! segments}`. With more it is format 2: `{format, txn, wal_txn,
//! next_segment_id, next_table_id, tables: [{id, schema, segments}]}`.
//! Segment ids are unique across tables, so segment files, delete sets and
//! caches keyed by segment id never collide.
//!
//! Format 3 is format 2 with the features the database uses, `{format: 3,
//! features: {required: [..], optional: [..]}, ...}`, and is the last
//! format number: later changes add features instead. A reader refuses a
//! manifest that requires a feature it does not know, naming it, and
//! ignores optional ones.
//!
//! Every database this version creates is format 3 with feature
//! [`FIELD_IDS`]: each field of each table has an id
//! ([`crate::schema::Field::id`]), and the database's segments, doc stores
//! and values written in place name fields by it. A database written
//! before field ids (format 1, 2, or 3 without the feature) opens only to
//! be copied into a new one ([`crate::upgrade`]); written to meanwhile, it
//! stays in its format, which it takes as the versions before did: format 3
//! only while it uses a feature ([`Manifest::features`]: a BOOL or
//! TIMESTAMP field, a NOT NULL field or a field with a DEFAULT, sequence
//! numbers, or a rank profile), else 1 while it holds only its first table,
//! else 2.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{corrupt, Error, Result};
use crate::schema::Schema;
use crate::storage::pager::Extent;

/// The on-disk state named by a meta slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Manifest {
    /// The commit that wrote this manifest.
    pub txn: u64,
    /// WAL transactions up to this txn are contained in the segments and
    /// skipped on replay (they can survive a crash between a flush's commit
    /// and the WAL truncation that follows it).
    pub wal_txn: u64,
    pub next_segment_id: u64,
    /// The next new table's id. Ids are not reused while a database is
    /// open; dropping a table first flushes the WAL, so no logged operation
    /// can outlive its table and land in a later one with the same id.
    pub next_table_id: u32,
    /// In creation order. The first is the table the database was created
    /// with: its id is 0 and it cannot be dropped.
    pub tables: Vec<TableMeta>,
    /// Set once the database numbers its writes
    /// ([`crate::storage::Database::start_seq_no`]). Feature `seq_no`.
    pub seq_no: Option<Numbering>,
}

/// How a database that numbers its writes stands, as a version records
/// it: the term writes are numbered under, the last sequence number given
/// when the version was committed (those of transactions still in the WAL
/// come back with them), and every term so far with the first number it
/// gave, oldest first. A primary's history is the terms' runs of numbers:
/// a replica that comes back after a failover finds where its own log
/// left the new primary's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Numbering {
    pub term: u64,
    pub seq: u64,
    /// (term, first sequence number), rising in both; the last is `term`.
    pub terms: Vec<(u64, u64)>,
}

impl Numbering {
    /// Numbering from now on: term 1, from sequence number 1.
    pub fn start() -> Numbering {
        Numbering { term: 1, seq: 0, terms: vec![(1, 1)] }
    }

    /// The term and the last sequence number.
    pub fn at(&self) -> SeqNo {
        SeqNo { term: self.term, seq: self.seq }
    }

    /// The term of the transaction numbered `seq`: that of the last term
    /// whose first number is at most `seq`; 0 for 0, which is no
    /// transaction (the history before numbering).
    pub fn term_at(&self, seq: u64) -> u64 {
        self.terms.iter().rev().find(|&&(_, from)| from <= seq).map_or(0, |&(t, _)| t)
    }

    /// The last sequence number and its term, where a primary ships from.
    pub fn last(&self) -> SeqNo {
        SeqNo { term: self.term_at(self.seq), seq: self.seq }
    }

    /// Whether the terms rise, their first numbers do not fall, and the
    /// last is the current term.
    fn valid(&self) -> bool {
        self.terms.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1) && self.terms.last().is_some_and(|&(t, from)| t == self.term && from <= self.seq + 1)
    }
}

/// A write transaction's place in a database's history: the term of the
/// primary that numbered it and its sequence number, one more than the
/// transaction before it. Sequence numbers start at 1 and never go back,
/// across terms too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqNo {
    pub term: u64,
    pub seq: u64,
}

/// One table of a version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableMeta {
    /// Names the table in the WAL; never changes.
    pub id: u32,
    pub schema: Schema,
    /// Oldest first.
    pub segments: Vec<SegmentMeta>,
    /// The table's rank profiles, by name. Feature `rank_profiles`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<RankProfile>,
}

/// A named ranking of a table's rows, kept with the table
/// (`CREATE RANK PROFILE`, [`crate::query::profile`]): the query that
/// ranks, as SQL, and default values of its parameters. A query ranks by
/// it with `ORDER BY rank_profile('name', ...)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RankProfile {
    pub name: String,
    /// `SELECT * FROM table ORDER BY ...`, or the `WITH` form of rerank
    /// phases, as the binder prints it.
    pub ranking: String,
    /// Parameters the ranking reads and their values when a query does not
    /// set them: numbers, strings and booleans.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<String, serde_json::Value>,
}

impl RankProfile {
    /// Names this definition: two profiles have the same hash when they
    /// rank alike (FNV-1a of its JSON, as 16 hex digits). A plan made with
    /// a profile carries it, so a database whose profile of that name
    /// differs refuses the plan.
    pub fn hash(&self) -> String {
        format!("{:016x}", crate::embed::fnv(&serde_json::to_string(self).expect("a rank profile serialises")))
    }
}

/// Where a segment and its delete set live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentMeta {
    pub id: u64,
    pub extent: Extent,
    /// crc32 of the segment image (checked by `Database::check`).
    pub crc: u32,
    pub n_docs: u32,
    pub n_deleted: u32,
    /// Bitset of deleted docIDs, `n_docs.div_ceil(64)` little-endian words.
    pub deletes: Option<Extent>,
    pub deletes_crc: u32,
    /// The commit that wrote the segment.
    pub txn: u64,
    /// Directory layout: the segment's own file in the database directory
    /// (`extent` then spans the whole file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Directory layout: the delete set's file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletes_file: Option<String>,
    /// In-place updates of numeric fields ([`crate::storage::updates`]),
    /// as an extent or (directory layout) a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<Extent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates_file: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub updates_crc: u32,
}

fn is_zero(x: &u32) -> bool {
    *x == 0
}

impl SegmentMeta {
    pub fn n_live(&self) -> u32 {
        self.n_docs - self.n_deleted
    }
}

impl Manifest {
    /// A new database's manifest: one empty table.
    pub fn new(schema: Schema) -> Manifest {
        Manifest { txn: 1, wal_txn: 0, next_segment_id: 1, next_table_id: 1, tables: vec![TableMeta { id: 0, schema, segments: Vec::new(), profiles: Vec::new() }], seq_no: None }
    }

    /// 3 while the database uses a feature ([`Manifest::features`]), as
    /// every database with field ids does; for one written before them, 1
    /// while it holds only the table it was created with, else 2.
    pub fn format(&self) -> u32 {
        if !self.is_legacy() || self.seq_no.is_some() || self.tables.iter().any(|t| t.schema.features().next().is_some() || !t.profiles.is_empty()) {
            3
        } else if self.tables.len() == 1 && self.tables[0].id == 0 {
            1
        } else {
            2
        }
    }

    /// The format 3 features the database needs, sorted: [`FIELD_IDS`]
    /// unless it was written before field ids, its tables'
    /// ([`crate::schema::Schema::features`]), `seq_no` once it numbers its
    /// writes, `rank_profiles` while a table has a rank profile, and
    /// `sources` once it has [`SOURCES`].
    pub fn features(&self) -> Vec<String> {
        let mut used: BTreeSet<String> = self.tables.iter().flat_map(|t| t.schema.features()).collect();
        if !self.is_legacy() {
            used.insert(FIELD_IDS.to_string());
        }
        if self.seq_no.is_some() {
            used.insert("seq_no".to_string());
        }
        if self.tables.iter().any(|t| !t.profiles.is_empty()) {
            used.insert(RANK_PROFILES.to_string());
        }
        if self.table(SOURCES_ID).is_some() {
            used.insert("sources".to_string());
        }
        used.into_iter().collect()
    }

    /// Whether the database was written before field ids: its tables'
    /// fields have none ([`crate::schema::Schema::is_legacy`]). Its tables
    /// are all alike: a table created in such a database gets none either.
    pub fn is_legacy(&self) -> bool {
        self.tables.first().is_some_and(|t| t.schema.is_legacy())
    }

    /// The table with this id.
    pub fn table(&self, id: u32) -> Option<&TableMeta> {
        self.tables.iter().find(|t| t.id == id)
    }

    /// Every segment of every table.
    pub fn segments(&self) -> impl Iterator<Item = &SegmentMeta> {
        self.tables.iter().flat_map(|t| &t.segments)
    }

    pub fn table_mut(&mut self, id: u32) -> Option<&mut TableMeta> {
        self.tables.iter_mut().find(|t| t.id == id)
    }

    /// The segment with this id, in whichever table holds it.
    pub fn segment_mut(&mut self, id: u64) -> Option<&mut SegmentMeta> {
        self.tables.iter_mut().flat_map(|t| &mut t.segments).find(|s| s.id == id)
    }

    pub fn encode(&self) -> Vec<u8> {
        let format = self.format();
        let bytes = if format == 1 {
            let t = &self.tables[0];
            serde_json::to_vec(&V1Ref { schema: &t.schema, txn: self.txn, wal_txn: self.wal_txn, next_segment_id: self.next_segment_id, segments: &t.segments })
        } else {
            serde_json::to_vec(&V2Ref {
                format,
                features: (format == 3).then(|| FeaturesRef { required: self.features() }),
                txn: self.txn,
                wal_txn: self.wal_txn,
                next_segment_id: self.next_segment_id,
                next_table_id: self.next_table_id,
                tables: &self.tables,
                seq_no: self.seq_no.as_ref(),
            })
        };
        bytes.expect("manifest serialises")
    }

    pub fn decode(b: &[u8]) -> Result<Manifest> {
        let bad = |e: serde_json::Error| corrupt(format!("manifest: {e}"));
        let v: serde_json::Value = serde_json::from_slice(b).map_err(bad)?;
        match v.get("format").and_then(|f| f.as_u64()) {
            None => {
                let m: V1 = serde_json::from_value(v).map_err(bad)?;
                Manifest {
                    txn: m.txn,
                    wal_txn: m.wal_txn,
                    next_segment_id: m.next_segment_id,
                    next_table_id: 1,
                    tables: vec![TableMeta { id: 0, schema: m.schema, segments: m.segments, profiles: Vec::new() }],
                    seq_no: None,
                }
                .checked(false)
            }
            Some(f @ (2 | 3)) => {
                let mut ids = false;
                if f == 3 {
                    let features: Features = serde_json::from_value(v.get("features").cloned().unwrap_or_default()).map_err(bad)?;
                    if let Some(x) = features.required.iter().find(|x| !known_feature(x)) {
                        return Err(Error::Schema(format!("the database uses feature {x:?}, which this version of Strata does not have: open it with a newer one")));
                    }
                    ids = features.required.iter().any(|x| x == FIELD_IDS);
                }
                let m: V2 = serde_json::from_value(v).map_err(bad)?;
                if m.tables.first().is_none_or(|t| t.id != 0) {
                    return Err(corrupt("manifest: the first table must have id 0"));
                }
                if m.seq_no.as_ref().is_some_and(|n| !n.valid()) {
                    return Err(corrupt("manifest: the terms of its sequence numbers are out of order"));
                }
                Manifest { txn: m.txn, wal_txn: m.wal_txn, next_segment_id: m.next_segment_id, next_table_id: m.next_table_id, tables: m.tables, seq_no: m.seq_no }.checked(ids)
            }
            Some(f) => Err(corrupt(format!("manifest format {f} is newer than this version of Strata reads"))),
        }
    }

    /// Checks the tables' field ids against whether the manifest requires
    /// [`FIELD_IDS`] (`ids`): with it every field of every table has a
    /// valid id, without it none has one.
    fn checked(self, ids: bool) -> Result<Manifest> {
        for t in &self.tables {
            t.schema.check_ids().map_err(|e| corrupt(format!("manifest: {e}")))?;
            if t.schema.is_legacy() == ids {
                let has = if ids { "has no field ids, and the manifest requires them" } else { "has field ids, and the manifest does not require feature field_ids" };
                return Err(corrupt(format!("manifest: table {} {has}", t.schema.table)));
            }
        }
        Ok(self)
    }
}

/// Format 1, field for field as it has always been written.
#[derive(Serialize)]
struct V1Ref<'a> {
    schema: &'a Schema,
    txn: u64,
    wal_txn: u64,
    next_segment_id: u64,
    segments: &'a [SegmentMeta],
}

#[derive(Deserialize)]
struct V1 {
    schema: Schema,
    txn: u64,
    wal_txn: u64,
    next_segment_id: u64,
    segments: Vec<SegmentMeta>,
}

/// The features this version of Strata reads, besides its analyzers'
/// ([`known_feature`]).
pub const FEATURES: &[&str] = &[crate::schema::ADDED_FIELDS, "bool", "cluster_by", "default", "derive", "edge_table", FIELD_IDS, crate::schema::JSON, "media", "multi_vector", "not_null", RANK_PROFILES, "retain", "semantic_id", "seq_no", "shard_by", "sources", "timestamp"];

/// The feature of a database whose fields have ids
/// ([`crate::schema::Field::id`]): every database this version creates.
/// Versions of Strata from before it refuse such a database by name, and
/// this one opens a database without it only to copy it
/// ([`crate::upgrade`]).
pub const FIELD_IDS: &str = "field_ids";

/// The feature of a database with a rank profile ([`RankProfile`]), and of
/// a write-ahead log that may hold one ([`crate::storage::wal`]).
pub const RANK_PROFILES: &str = "rank_profiles";

/// The system table in which streams keep, per source (a Kafka partition,
/// say), the offset of the last event they wrote, in the transaction that
/// wrote it ([`crate::storage::Database::ensure_sources`]). The database
/// then needs feature `sources`.
pub const SOURCES: &str = "_sources";

/// [`SOURCES`]'s table id, the same in every copy of a database and never
/// one that `CREATE TABLE` gives, so that a replica applies its primary's
/// offsets to a `_sources` of its own, made when the first of them
/// arrives, without the table being shipped.
pub const SOURCES_ID: u32 = u32::MAX - 1;

/// [`SOURCES`]'s schema: a source's name, the last offset written, and
/// when.
pub fn sources_schema() -> Schema {
    Schema::from_json(
        r#"{"table": "_sources", "key": "name", "fields": [
            {"name": "name", "type": "keyword"},
            {"name": "last_offset", "type": "int", "not_null": true},
            {"name": "updated", "type": "timestamp"}]}"#,
    )
    .expect("the _sources schema is valid")
}

/// Whether this version of Strata reads a database that requires feature
/// `x`: one of [`FEATURES`], or a TEXT field's analyzer it has
/// ([`crate::text::analyzer::Analyzer::feature`]).
pub fn known_feature(x: &str) -> bool {
    FEATURES.contains(&x) || crate::text::analyzer::Analyzer::ALL.iter().any(|a| a.feature().as_deref() == Some(x))
}

/// Format 2, and format 3 with its features.
#[derive(Serialize)]
struct V2Ref<'a> {
    format: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    features: Option<FeaturesRef>,
    txn: u64,
    wal_txn: u64,
    next_segment_id: u64,
    next_table_id: u32,
    tables: &'a [TableMeta],
    #[serde(skip_serializing_if = "Option::is_none")]
    seq_no: Option<&'a Numbering>,
}

/// Every feature Strata writes so far is required. (A reader ignores the
/// `optional` list and any other key it does not know.)
#[derive(Serialize)]
struct FeaturesRef {
    /// A reader that does not know one of these refuses the database.
    required: Vec<String>,
}

#[derive(Deserialize)]
struct Features {
    required: Vec<String>,
}

#[derive(Deserialize)]
struct V2 {
    txn: u64,
    wal_txn: u64,
    next_segment_id: u64,
    next_table_id: u32,
    tables: Vec<TableMeta>,
    #[serde(default)]
    seq_no: Option<Numbering>,
}

/// The manifest of the newest version of the database at `path` whose
/// manifest reads (as opening falls back to the version before a damaged
/// one), without opening the database: a directory's `CURRENT`, or a
/// single file's meta slots.
pub fn read(path: &std::path::Path) -> Result<Manifest> {
    if path.is_dir() {
        return Ok(crate::storage::dir::read_current(path)?.1);
    }
    let file = std::fs::File::open(path)?;
    let mut last = None;
    for (meta, _) in crate::storage::pager::read_metas(&file)? {
        match crate::storage::pager::read_extent(&file, meta.manifest, meta.manifest_crc).and_then(|b| Manifest::decode(&b)) {
            Ok(m) => return Ok(m),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| corrupt("no valid meta slot")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Field, FieldType};

    /// A table as a version from before field ids wrote it, which these
    /// tests use to check the formats such a database stays in (1, 2 and 3
    /// as its features say); [`field_ids_are_format_3`] covers this
    /// version's.
    fn schema(name: &str) -> Schema {
        Schema::new(name, vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: true })], Some("id")).unwrap().without_ids()
    }

    fn segment(id: u64) -> SegmentMeta {
        SegmentMeta {
            id,
            extent: Extent { page: 7, len: 9000 },
            crc: 5,
            n_docs: 10,
            n_deleted: 0,
            deletes: None,
            deletes_crc: 0,
            txn: 3,
            file: None,
            deletes_file: None,
            updates: None,
            updates_file: None,
            updates_crc: 0,
        }
    }

    /// The manifest as every version before tables wrote it.
    #[derive(Serialize)]
    struct Before {
        schema: Schema,
        txn: u64,
        wal_txn: u64,
        next_segment_id: u64,
        segments: Vec<SegmentMeta>,
    }

    #[test]
    fn one_table_keeps_format_1_byte_for_byte() {
        let before = Before { schema: schema("docs"), txn: 4, wal_txn: 3, next_segment_id: 3, segments: vec![segment(1), segment(2)] };
        let bytes = serde_json::to_vec(&before).unwrap();
        let m = Manifest::decode(&bytes).unwrap();
        assert_eq!(m.format(), 1);
        assert_eq!((m.tables.len(), m.tables[0].id, m.next_table_id), (1, 0, 1));
        assert_eq!(m.encode(), bytes);
    }

    #[test]
    fn several_tables_use_format_2() {
        let mut m = Manifest::new(schema("papers"));
        m.tables[0].segments.push(segment(1));
        m.tables.push(TableMeta { id: 1, schema: schema("authors"), segments: vec![segment(2)], profiles: Vec::new() });
        m.next_table_id = 2;
        m.next_segment_id = 3;
        assert_eq!(m.format(), 2);
        let back = Manifest::decode(&m.encode()).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.segments().map(|s| s.id).collect::<Vec<_>>(), [1, 2]);
        // Dropping back to the first table alone writes format 1 again.
        m.tables.truncate(1);
        assert_eq!(m.format(), 1);
        assert!(!String::from_utf8(m.encode()).unwrap().contains("format"));
        let newer = br#"{"format":4,"tables":[]}"#;
        assert!(Manifest::decode(newer).unwrap_err().to_string().contains("newer"));
    }

    fn typed(name: &str) -> Schema {
        let fields = vec![Field::new("id", FieldType::Keyword), Field::timestamp("at"), Field::boolean("done"), Field::boolean("seen")];
        Schema::new(name, fields, Some("id")).unwrap().without_ids()
    }

    #[test]
    fn features_use_format_3_and_name_what_they_need() {
        // The first table alone, with a TIMESTAMP and two BOOLs.
        let mut m = Manifest::new(typed("tasks"));
        m.tables[0].segments.push(segment(1));
        assert_eq!((m.format(), m.features()), (3, strings(&["bool", "timestamp"])));
        let text = String::from_utf8(m.encode()).unwrap();
        assert!(text.starts_with(r#"{"format":3,"features":{"required":["bool","timestamp"]},"#), "{text}");
        assert_eq!(Manifest::decode(text.as_bytes()).unwrap(), m);

        // A second table that uses one: format 3 while it is there.
        let mut m = Manifest::new(schema("papers"));
        m.tables.push(TableMeta { id: 1, schema: typed("tasks"), segments: Vec::new(), profiles: Vec::new() });
        m.next_table_id = 2;
        assert_eq!(m.format(), 3);
        assert_eq!(Manifest::decode(&m.encode()).unwrap(), m);
        m.tables.push(TableMeta { id: 2, schema: schema("authors"), segments: Vec::new(), profiles: Vec::new() });
        m.tables.remove(1);
        assert_eq!((m.format(), m.features()), (2, strings(&[])));
        assert!(!String::from_utf8(m.encode()).unwrap().contains("features"));
    }

    fn strings(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn analyzers_are_features() {
        use crate::schema::Analyzer;
        let text = || Field::new("body", FieldType::Text { positions: true });
        // Saying english is saying nothing: every version has it.
        let english = Schema::new("t", vec![Field::new("id", FieldType::Keyword), text().analyzer(Analyzer::English)], Some("id")).unwrap();
        assert_eq!(english.fields[1].analyzer, None);
        assert_eq!(Manifest::new(english.without_ids()).format(), 1);
        let simple = Schema::new("t", vec![Field::new("id", FieldType::Keyword), text().analyzer(Analyzer::Simple)], Some("id")).unwrap();
        let m = Manifest::new(simple.without_ids());
        assert_eq!((m.format(), m.features()), (3, strings(&["analyzer:simple:1"])));
        let bytes = m.encode();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains(r#""analyzer":{"name":"simple","version":1}"#), "{text}");
        assert_eq!(Manifest::decode(&bytes).unwrap(), m);
        // A version that lacks an analyzer refuses the database by its
        // feature, and a field naming one it lacks even without it.
        let e = Manifest::decode(text.replace("analyzer:simple:1", "analyzer:simple:2").as_bytes()).unwrap_err().to_string();
        assert!(e.contains(r#"feature "analyzer:simple:2""#), "{e}");
        let e = Manifest::decode(text.replace(r#""version":1}"#, r#""version":2}"#).as_bytes()).unwrap_err().to_string();
        assert!(e.contains("analyzer simple version 2"), "{e}");
        assert!(Schema::new("t", vec![Field::new("k", FieldType::Keyword).analyzer(Analyzer::Simple)], None).is_err());
    }

    #[test]
    fn constraints_are_features() {
        let fields = vec![
            Field::new("id", FieldType::Keyword).not_null(),
            Field::new("state", FieldType::Keyword).default(serde_json::json!("open")),
            Field::new("n", FieldType::Int).not_null(),
        ];
        let mut m = Manifest::new(Schema::new("t", fields, Some("id")).unwrap().without_ids());
        assert_eq!((m.format(), m.features()), (3, strings(&["default", "not_null"])));
        assert_eq!(Manifest::decode(&m.encode()).unwrap(), m);
        m.tables.push(TableMeta { id: 1, schema: typed("tasks"), segments: Vec::new(), profiles: Vec::new() });
        assert_eq!(m.features(), ["bool", "default", "not_null", "timestamp"]);
        // A key said to be NOT NULL needs nothing: keys always are.
        let key = Schema::new("k", vec![Field::new("id", FieldType::Keyword).not_null()], Some("id")).unwrap();
        assert_eq!(Manifest::new(key.without_ids()).format(), 1);
    }

    #[test]
    fn sequence_numbers_are_a_feature() {
        let mut m = Manifest::new(schema("docs"));
        m.seq_no = Some(Numbering { term: 2, seq: 41, terms: vec![(1, 1), (2, 30)] });
        assert_eq!((m.format(), m.features()), (3, strings(&["seq_no"])));
        let text = String::from_utf8(m.encode()).unwrap();
        assert!(text.starts_with(r#"{"format":3,"features":{"required":["seq_no"]},"#) && text.ends_with(r#""seq_no":{"term":2,"seq":41,"terms":[[1,1],[2,30]]}}"#), "{text}");
        assert_eq!(Manifest::decode(text.as_bytes()).unwrap(), m);
        // Terms out of order, or a last term that is not the current one,
        // are damage.
        for bad in [r#""terms":[[2,30],[1,1]]"#, r#""terms":[[1,1],[3,30]]"#, r#""terms":[]"#, r#""terms":[[1,1],[2,43]]"#] {
            let e = Manifest::decode(text.replace(r#""terms":[[1,1],[2,30]]"#, bad).as_bytes()).unwrap_err().to_string();
            assert!(e.contains("out of order"), "{bad}: {e}");
        }
        // With another feature, and with several tables.
        m.tables[0].schema = typed("tasks");
        m.tables.push(TableMeta { id: 1, schema: schema("authors"), segments: Vec::new(), profiles: Vec::new() });
        assert_eq!(m.features(), ["bool", "seq_no", "timestamp"]);
        assert_eq!(Manifest::decode(&m.encode()).unwrap(), m);
        // A version from before sequence numbers refuses it by name.
        let e = Manifest::decode(String::from_utf8(m.encode()).unwrap().replace(r#""seq_no","#, r#""seq_no2","#).as_bytes()).unwrap_err().to_string();
        assert!(e.contains(r#"feature "seq_no2""#), "{e}");
    }

    /// Rank profiles are kept with their table, and need format 3 and
    /// `rank_profiles` while a table has one.
    #[test]
    fn rank_profiles_are_a_feature() {
        let mut m = Manifest::new(schema("papers"));
        m.tables.push(TableMeta { id: 1, schema: schema("authors"), segments: Vec::new(), profiles: Vec::new() });
        m.next_table_id = 2;
        let p = RankProfile { name: "recent".into(), ranking: "SELECT * FROM authors ORDER BY year DESC".into(), defaults: [("w".to_string(), serde_json::json!(0.5))].into() };
        m.tables[1].profiles.push(p.clone());
        assert_eq!((m.format(), m.features()), (3, strings(&[RANK_PROFILES])));
        let text = String::from_utf8(m.encode()).unwrap();
        assert!(text.starts_with(r#"{"format":3,"features":{"required":["rank_profiles"]},"#), "{text}");
        assert!(text.contains(r#""profiles":[{"name":"recent","ranking":"SELECT * FROM authors ORDER BY year DESC","defaults":{"w":0.5}}]"#), "{text}");
        assert_eq!(text.matches(r#""profiles""#).count(), 1, "a table without profiles writes none: {text}");
        assert_eq!(Manifest::decode(text.as_bytes()).unwrap(), m);
        // A version from before them refuses it by name.
        let e = Manifest::decode(text.replace(r#"["rank_profiles"]"#, r#"["rank_profiles2"]"#).as_bytes()).unwrap_err().to_string();
        assert!(e.contains(r#"feature "rank_profiles2""#), "{e}");
        // Without profiles, format 2 again.
        m.tables[1].profiles.clear();
        assert_eq!((m.format(), m.features()), (2, strings(&[])));
        assert!(!String::from_utf8(m.encode()).unwrap().contains("profiles"));
        // A hash names the definition.
        let mut q = p.clone();
        assert_eq!(q.hash(), p.hash());
        q.defaults.insert("w".into(), serde_json::json!(0.6));
        assert_ne!(q.hash(), p.hash());
        assert_eq!(p.hash().len(), 16);
    }

    #[test]
    fn an_unknown_required_feature_is_refused_by_name() {
        let m = Manifest::new(typed("tasks"));
        let mut v: serde_json::Value = serde_json::from_slice(&m.encode()).unwrap();
        // Optional features and keys this version does not know are ignored.
        v["features"]["optional"] = serde_json::json!(["zstd-pages"]);
        v["features"]["note"] = serde_json::json!("from a later version");
        assert_eq!(Manifest::decode(&serde_json::to_vec(&v).unwrap()).unwrap(), m);

        v["features"]["required"] = serde_json::json!(["bool", "vectors-f16", "timestamp"]);
        let err = Manifest::decode(&serde_json::to_vec(&v).unwrap()).unwrap_err();
        assert!(matches!(err, Error::Schema(_)), "{err}");
        assert!(err.to_string().contains(r#"feature "vectors-f16""#), "{err}");

        // Format 3 must say which features it needs.
        v.as_object_mut().unwrap().remove("features");
        assert!(Manifest::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    /// A database of this version names each field by its id: format 3,
    /// feature `field_ids`, each table's ids and the next to give in its
    /// schema. Ids that clash, a table without them (or with them, in a
    /// manifest that does not require them) are damage.
    #[test]
    fn field_ids_are_format_3() {
        let ids = |name: &str| schema(name).with_new_ids().unwrap();
        let mut m = Manifest::new(ids("papers"));
        m.tables.push(TableMeta { id: 1, schema: ids("authors"), segments: vec![segment(1)], profiles: Vec::new() });
        m.next_table_id = 2;
        assert!(!m.is_legacy());
        assert_eq!((m.format(), m.features()), (3, strings(&[FIELD_IDS])));
        let text = String::from_utf8(m.encode()).unwrap();
        assert!(text.starts_with(r#"{"format":3,"features":{"required":["field_ids"]},"#), "{text}");
        assert!(text.contains(r#""fields":[{"id":0,"name":"id","#) && text.contains(r#"{"id":1,"name":"body","#) && text.contains(r#""next_field_id":2"#), "{text}");
        assert_eq!(Manifest::decode(text.as_bytes()).unwrap(), m);
        // With other features, sorted.
        m.tables[1].schema = typed("tasks").with_new_ids().unwrap();
        assert_eq!(m.features(), ["bool", "field_ids", "timestamp"]);
        assert_eq!(Manifest::decode(&m.encode()).unwrap(), m);

        let corrupt = |text: String, why: &str| {
            let e = Manifest::decode(text.as_bytes()).unwrap_err();
            assert!(matches!(e, Error::Corrupt(_)) && e.to_string().contains(why), "{why}: {e}");
        };
        let text = String::from_utf8(Manifest { tables: m.tables[..1].to_vec(), ..m.clone() }.encode()).unwrap();
        corrupt(text.replacen(r#"{"id":1,"name":"body""#, r#"{"id":0,"name":"body""#, 1), "the same id");
        corrupt(text.replacen(r#""next_field_id":2"#, r#""next_field_id":1"#, 1), "next field id");
        corrupt(text.replacen(r#""next_field_id":2"#, r#""next_field_id":70000"#, 1), "past the most");
        corrupt(text.replacen(r#"{"id":1,"name":"body""#, r#"{"name":"body""#, 1), "next field id");
        corrupt(text.replacen(r#"["field_ids"]"#, "[]", 1), "does not require feature field_ids");
        let mixed = Manifest { tables: vec![m.tables[0].clone(), TableMeta { id: 1, schema: schema("authors"), segments: Vec::new(), profiles: Vec::new() }], ..m.clone() };
        corrupt(String::from_utf8(mixed.encode()).unwrap(), "table authors has no field ids");
        // A legacy manifest that names the feature is damage as well.
        let old = Manifest::new(schema("docs"));
        let mut v: serde_json::Value = serde_json::from_slice(&Manifest { tables: vec![TableMeta { id: 0, schema: schema("docs"), segments: Vec::new(), profiles: Vec::new() }, TableMeta { id: 1, schema: typed("t2"), segments: Vec::new(), profiles: Vec::new() }], next_table_id: 2, ..old }.encode()).unwrap();
        v["features"]["required"] = serde_json::json!(["bool", "field_ids", "timestamp"]);
        corrupt(serde_json::to_string(&v).unwrap(), "has no field ids, and the manifest requires them");
    }
}
