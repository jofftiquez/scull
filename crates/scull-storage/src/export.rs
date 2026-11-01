//! Read-only access to every table for `scull data show|export` (DATA-07, INS-08).
//!
//! * [`Store::table_counts`]: rows per table.
//! * [`Store::export_table`]: every row of one table, in a stable order, as JSON values.
//! * [`Store::quarantine_outstanding`]: quarantine manifest entries neither restored nor
//!   purged (what a full purge of the database would orphan).
//!
//! Table names come from a fixed allow-list ([`EXPORT_TABLES`]) mapped to static SQL strings:
//! caller input is never interpolated into SQL, and an unknown name is rejected. Every call
//! runs inside one read transaction with `PRAGMA query_only` set, so it cannot write even by
//! mistake, and all tables are read from one consistent snapshot per call.
//!
//! Nothing secret is stored in the database (the MCP name-hashing salt is a separate file in
//! the data directory, never a column), so no column is withheld. BLOB values, if any, are
//! exported as lowercase hex strings.

use crate::error::{Result, StoreError};
use crate::store::Store;
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use scull_core::ErrorKind;
use serde::Serialize;
use serde_json::Value;

/// One exportable table: its name, a row count query and a full, ordered select.
struct TableSql {
    name: &'static str,
    count: &'static str,
    select: &'static str,
}

macro_rules! table {
    ($name:literal, $order:literal) => {
        TableSql {
            name: $name,
            count: concat!("SELECT count(*) FROM ", $name),
            select: concat!("SELECT * FROM ", $name, " ORDER BY ", $order),
        }
    };
}

/// Every table of the current schema, in export order.
const TABLES: &[TableSql] = &[
    table!("meta", "key"),
    table!("profile", "key"),
    table!("scans", "started_at, scan_id"),
    table!("scan_roots", "root, started_at, scan_id"),
    table!("scan_rollups", "scan_id, category, tier, ecosystem"),
    table!("free_space_samples", "sample_id"),
    table!("volumes", "volume"),
    table!("items", "item_id"),
    table!("item_events", "event_id"),
    table!("regrowth_events", "event_id"),
    table!("footprint_samples", "sample_id"),
    table!("plans", "created_at, plan_id"),
    table!("approvals", "approval_id"),
    table!("operations", "started_at, operation_id"),
    table!("audit", "seq"),
    table!("audit_v1", "seq"),
    table!("quarantine_manifest", "entry_id"),
    table!("operation_targets", "operation_id, item_id"),
    table!("quarantine_log", "seq"),
    table!("journal", "seq"),
    table!("events", "event_id"),
    table!("metrics", "metric_id"),
];

/// Names of the tables [`Store::table_counts`] and [`Store::export_table`] cover, in export
/// order.
pub const EXPORT_TABLES: &[&str] = &[
    "meta",
    "profile",
    "scans",
    "scan_roots",
    "scan_rollups",
    "free_space_samples",
    "volumes",
    "items",
    "item_events",
    "regrowth_events",
    "footprint_samples",
    "plans",
    "approvals",
    "operations",
    "audit",
    "audit_v1",
    "quarantine_manifest",
    "operation_targets",
    "quarantine_log",
    "journal",
    "events",
    "metrics",
];

/// Rows in one table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableCount {
    /// Table name (one of [`EXPORT_TABLES`]).
    pub table: &'static str,
    /// Number of rows.
    pub rows: u64,
}

/// Receives one table from [`Store::export_table`]: the column names once, then each row's
/// values in the same order.
pub trait ExportSink {
    /// Column names, called once before any row (also for an empty table).
    fn columns(&mut self, names: &[String]) -> std::io::Result<()>;
    /// One row. Integers and reals are JSON numbers, text is a string, NULL is `null` and a
    /// BLOB is a lowercase hex string.
    fn row(&mut self, values: &[Value]) -> std::io::Result<()>;
}

/// A quarantine manifest entry that is neither restored nor purged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuarantineSummary {
    /// Operation that quarantined it.
    pub operation_id: String,
    /// Path the item was moved from, as recorded.
    pub original_path: String,
    /// Where it lies in quarantine, as recorded.
    pub quarantine_path: String,
    /// Recorded size in bytes.
    pub bytes: u64,
}

impl Store {
    /// Row counts of every table in [`EXPORT_TABLES`], in that order, from one snapshot.
    pub fn table_counts(&self) -> Result<Vec<TableCount>> {
        self.read_only(|conn| {
            TABLES
                .iter()
                .map(|t| {
                    let n: i64 = conn.query_row(t.count, [], |r| r.get(0))?;
                    Ok(TableCount {
                        table: t.name,
                        rows: u64::try_from(n).unwrap_or(0),
                    })
                })
                .collect()
        })
    }

    /// Stream every row of `table` (one of [`EXPORT_TABLES`]) to `sink`, in a stable order.
    /// Returns the number of rows. An unknown table name is rejected with
    /// [`ErrorKind::Usage`]; sink errors are returned as [`StoreError::Io`].
    pub fn export_table(&self, table: &str, sink: &mut dyn ExportSink) -> Result<u64> {
        let Some(t) = TABLES.iter().find(|t| t.name == table) else {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                format!("unknown table {table:?}"),
            ));
        };
        let sink_err = |e: std::io::Error| StoreError::io(t.name, e);
        self.read_only(|conn| {
            let mut stmt = conn.prepare(t.select)?;
            let names: Vec<String> = stmt.column_names().map(String::from).collect();
            sink.columns(&names).map_err(sink_err)?;
            let width = names.len();
            let mut rows = stmt.query([])?;
            let mut n = 0u64;
            let mut values = Vec::with_capacity(width);
            while let Some(row) = rows.next()? {
                values.clear();
                for i in 0..width {
                    values.push(json_value(row.get_ref(i)?));
                }
                sink.row(&values).map_err(sink_err)?;
                n += 1;
            }
            Ok(n)
        })
    }

    /// Quarantine manifest entries neither restored nor purged, oldest first. Deleting the
    /// database while any exist would lose the only record of where those items came from.
    pub fn quarantine_outstanding(&self) -> Result<Vec<QuarantineSummary>> {
        self.read_only(|conn| {
            let mut stmt = conn.prepare(
                "SELECT operation_id, original_path, quarantine_path, bytes \
                 FROM quarantine_manifest \
                 WHERE restored_at IS NULL AND purged_at IS NULL ORDER BY entry_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(QuarantineSummary {
                    operation_id: r.get(0)?,
                    original_path: r.get(1)?,
                    quarantine_path: r.get(2)?,
                    bytes: u64::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
                })
            })?;
            rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
        })
    }

    /// Run `f` in a read transaction with `PRAGMA query_only` on; the pragma is switched back
    /// off afterwards whatever `f` returned.
    fn read_only<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.read(|conn| {
            conn.pragma_update(None, "query_only", true)?;
            let result: Result<T> = (|| {
                let tx = conn.unchecked_transaction()?;
                let out = f(&tx)?;
                tx.rollback()?;
                Ok(out)
            })();
            let reset = conn.pragma_update(None, "query_only", false);
            let out = result?;
            reset?;
            Ok(out)
        })
    }
}

fn json_value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            let mut s = String::with_capacity(b.len() * 2);
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            Value::String(s)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::DefaultPermissions;

    #[derive(Default)]
    struct Collect {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    }

    impl ExportSink for Collect {
        fn columns(&mut self, names: &[String]) -> std::io::Result<()> {
            self.columns = names.to_vec();
            Ok(())
        }
        fn row(&mut self, values: &[Value]) -> std::io::Result<()> {
            self.rows.push(values.to_vec());
            Ok(())
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join(crate::DB_FILE_NAME), &DefaultPermissions).unwrap();
        (dir, s)
    }

    #[test]
    fn allow_list_matches_the_schema_exactly() {
        let (_d, s) = store();
        let mut actual: Vec<String> = s
            .conn
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        actual.sort();
        let mut listed: Vec<String> = EXPORT_TABLES.iter().map(|s| (*s).to_owned()).collect();
        listed.sort();
        assert_eq!(
            actual, listed,
            "a migration added or removed a table: update TABLES/EXPORT_TABLES in export.rs"
        );
        let names: Vec<&str> = TABLES.iter().map(|t| t.name).collect();
        assert_eq!(names, EXPORT_TABLES);
    }

    #[test]
    fn counts_cover_every_table_and_see_new_rows() {
        let (_d, mut s) = store();
        let before = s.table_counts().unwrap();
        assert_eq!(before.len(), EXPORT_TABLES.len());
        let meta = |c: &[TableCount]| c.iter().find(|t| t.table == "meta").unwrap().rows;
        let m0 = meta(&before);
        s.write(|tx| {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('export-test', 'x')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(meta(&s.table_counts().unwrap()), m0 + 1);
    }

    #[test]
    fn export_streams_columns_then_rows_and_hexes_blobs() {
        let (_d, mut s) = store();
        s.write(|tx| {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('zz-path', '/home/u/a b/ü')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let mut c = Collect::default();
        let n = s.export_table("meta", &mut c).unwrap();
        assert_eq!(c.columns, ["key", "value"]);
        assert_eq!(n as usize, c.rows.len());
        assert!(
            c.rows
                .contains(&vec![Value::from("zz-path"), Value::from("/home/u/a b/ü")])
        );
        assert_eq!(json_value(ValueRef::Blob(&[0, 0xab])), Value::from("00ab"));

        // Empty tables still report their columns.
        let mut e = Collect::default();
        assert_eq!(s.export_table("audit", &mut e).unwrap(), 0);
        assert!(e.columns.iter().any(|c| c == "operation_id"));
    }

    #[test]
    fn unknown_tables_are_rejected_without_running_sql() {
        let (_d, s) = store();
        for bad in ["nope", "meta; DROP TABLE meta", "sqlite_schema", "", "META"] {
            let err = s.export_table(bad, &mut Collect::default()).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Usage, "{bad}");
        }
        assert!(s.table_counts().unwrap().iter().any(|t| t.table == "meta"));
    }

    #[test]
    fn reads_leave_the_connection_writable_and_cannot_write() {
        let (_d, mut s) = store();
        s.table_counts().unwrap();
        // query_only is on inside the read: a write attempt there fails.
        let attempt = s.read_only(|conn| {
            conn.execute("INSERT INTO meta (key, value) VALUES ('w', 'w')", [])?;
            Ok(())
        });
        assert!(attempt.is_err());
        // ... and off again afterwards.
        s.write(|tx| {
            tx.execute("INSERT INTO meta (key, value) VALUES ('w', 'w')", [])?;
            Ok(())
        })
        .unwrap();
        assert!(s.quarantine_outstanding().unwrap().is_empty());
    }

    #[test]
    fn outstanding_quarantine_excludes_restored_and_purged_entries() {
        let (_d, s) = store();
        // Insert directly, without the plan and operation rows the foreign keys would require.
        // Entries start held (schema v4); purge and restore are set afterwards, once.
        let facts = "entry_identity, entry_kind, parent_identity, logical_bytes";
        let vals = "'7:1', 'dir', '7:2', 1";
        s.conn
            .execute_batch(&format!(
                "PRAGMA foreign_keys = OFF; \
                 INSERT INTO quarantine_manifest (operation_id, item_id, original_path, \
                   quarantine_path, bytes, quarantined_at, {facts}) \
                   VALUES ('op1', 'i1', '/p/a', '/q/a', 7, 1, {vals}), \
                          ('op1', 'i2', '/p/b', '/q/b', 9, 1, {vals}), \
                          ('op1', 'i3', '/p/c', '/q/c', 9, 1, {vals}); \
                 UPDATE quarantine_manifest SET purged_at = 2, purged_bytes = 9 \
                   WHERE item_id = 'i2'; \
                 UPDATE quarantine_manifest SET restored_at = 2, restored_to = '/p/c' \
                   WHERE item_id = 'i3'; \
                 PRAGMA foreign_keys = ON;"
            ))
            .unwrap();
        let out = s.quarantine_outstanding().unwrap();
        assert_eq!(
            out,
            vec![QuarantineSummary {
                operation_id: "op1".into(),
                original_path: "/p/a".into(),
                quarantine_path: "/q/a".into(),
                bytes: 7,
            }]
        );
    }
}
