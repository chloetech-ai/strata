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

use serde::{Deserialize, Serialize};

use crate::error::{corrupt, Result};
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
}

/// One table of a version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableMeta {
    /// Names the table in the WAL; never changes.
    pub id: u32,
    pub schema: Schema,
    /// Oldest first.
    pub segments: Vec<SegmentMeta>,
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
        Manifest { txn: 1, wal_txn: 0, next_segment_id: 1, next_table_id: 1, tables: vec![TableMeta { id: 0, schema, segments: Vec::new() }] }
    }

    /// 1 while the database holds only the table it was created with, else 2.
    pub fn format(&self) -> u32 {
        if self.tables.len() == 1 && self.tables[0].id == 0 {
            1
        } else {
            2
        }
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
        let bytes = if self.format() == 1 {
            let t = &self.tables[0];
            serde_json::to_vec(&V1Ref { schema: &t.schema, txn: self.txn, wal_txn: self.wal_txn, next_segment_id: self.next_segment_id, segments: &t.segments })
        } else {
            serde_json::to_vec(&V2Ref {
                format: 2,
                txn: self.txn,
                wal_txn: self.wal_txn,
                next_segment_id: self.next_segment_id,
                next_table_id: self.next_table_id,
                tables: &self.tables,
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
                Ok(Manifest {
                    txn: m.txn,
                    wal_txn: m.wal_txn,
                    next_segment_id: m.next_segment_id,
                    next_table_id: 1,
                    tables: vec![TableMeta { id: 0, schema: m.schema, segments: m.segments }],
                })
            }
            Some(2) => {
                let m: V2 = serde_json::from_value(v).map_err(bad)?;
                if m.tables.first().is_none_or(|t| t.id != 0) {
                    return Err(corrupt("manifest: the first table must have id 0"));
                }
                Ok(Manifest { txn: m.txn, wal_txn: m.wal_txn, next_segment_id: m.next_segment_id, next_table_id: m.next_table_id, tables: m.tables })
            }
            Some(f) => Err(corrupt(format!("manifest format {f} is newer than this version of Strata reads"))),
        }
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

#[derive(Serialize)]
struct V2Ref<'a> {
    format: u32,
    txn: u64,
    wal_txn: u64,
    next_segment_id: u64,
    next_table_id: u32,
    tables: &'a [TableMeta],
}

#[derive(Deserialize)]
struct V2 {
    txn: u64,
    wal_txn: u64,
    next_segment_id: u64,
    next_table_id: u32,
    tables: Vec<TableMeta>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Field, FieldType};

    fn schema(name: &str) -> Schema {
        Schema::new(name, vec![Field::new("id", FieldType::Keyword), Field::new("body", FieldType::Text { positions: true })], Some("id")).unwrap()
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
        m.tables.push(TableMeta { id: 1, schema: schema("authors"), segments: vec![segment(2)] });
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
        let newer = br#"{"format":3,"tables":[]}"#;
        assert!(Manifest::decode(newer).unwrap_err().to_string().contains("newer"));
    }
}
