//! [`Store`]: the one way any scull interface reads or writes the local database (DATA-01).

use crate::backup::{list_backups, with_suffix};
use crate::error::{Result, StoreError, is_corruption};
use crate::migrate::{MIGRATIONS, Migration, check_compatible, migrate, user_version};
use crate::perms::OwnerOnly;
use crate::records::{Approval, ApprovalMethod, NewApproval, ScanFinish, ScanStart};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use scull_core::{Candidate, ContentHash, ErrorKind, Plan, PlanId, PlanOrigin};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Options for [`Store::open_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOptions {
    /// How long a write waits for another process's write transaction before failing with
    /// [`StoreError::Busy`]. Readers never wait in WAL mode.
    pub busy_timeout: Duration,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            busy_timeout: Duration::from_secs(5),
        }
    }
}

/// An open scull database.
///
/// Connection settings (applied on every open):
///
/// * `journal_mode=WAL`: readers proceed while one writer commits (FX-DATA-05, SAFE-09).
/// * `synchronous=FULL`: every commit is fsynced to the WAL. `NORMAL` is crash-safe but can
///   lose the last commits on power loss; for approvals that would mean a consumed (single-use)
///   approval could become unused again, and audit rows of a completed deletion could vanish.
///   Writes are small and infrequent (scan results are batched in one transaction), so the cost
///   of one fsync per commit is negligible.
/// * `foreign_keys=ON`, `busy_timeout` from [`StoreOptions`], `trusted_schema=OFF`.
#[derive(Debug)]
pub struct Store {
    pub(crate) conn: Connection,
    path: PathBuf,
    backups_taken: Vec<PathBuf>,
}

impl Store {
    /// Open (creating if needed) the database at `path` with default options. See
    /// [`Store::open_with`].
    pub fn open(path: &Path, perms: &dyn OwnerOnly) -> Result<Store> {
        Self::open_with(path, perms, &StoreOptions::default())
    }

    /// Open (creating if needed) the database at `path` and bring its schema up to date.
    ///
    /// `path` is the full database file path (normally `<local data dir>/`[`crate::DB_FILE_NAME`]);
    /// it must be on a local, **non-roaming** location (e.g. `%LOCALAPPDATA%` on Windows, never
    /// `%APPDATA%\Roaming`: WAL needs local file locking and the data is machine-bound).
    ///
    /// Steps, in order:
    /// 1. If the parent directory is missing it is created owner-only (its own parents with OS
    ///    defaults); a missing database file is created empty and owner-only through `perms`.
    /// 2. An existing database is probed **read-only**: damaged → [`StoreError::Corrupt`] (with
    ///    the backups found); newer schema → [`StoreError::SchemaTooNew`]; another
    ///    application's database → [`StoreError::NotScullDatabase`]. In all three cases the file
    ///    is left byte-identical.
    /// 3. Database, `-wal` and `-shm` files are restricted owner-only, connection settings are
    ///    applied (see [`Store`]), and pending migrations run, each preceded by a backup. The
    ///    `-wal` and `-shm` files SQLite creates while opening are restricted too.
    pub fn open_with(path: &Path, perms: &dyn OwnerOnly, options: &StoreOptions) -> Result<Store> {
        Self::open_inner(path, perms, options, MIGRATIONS)
    }

    /// Open an existing database **read-only**, for viewers that must never change it (the
    /// desktop History screen, SAFE-10).
    ///
    /// The connection is opened with `SQLITE_OPEN_READ_ONLY` and `query_only` on: no file or
    /// folder is created, no permission is changed, nothing is migrated or written (SQLite may
    /// still create the WAL index sidecar a reader needs). The file must be a regular file
    /// holding a scull database at exactly this build's schema version; otherwise it is
    /// refused and left untouched: missing → [`ErrorKind::NotFound`]; a link or not a regular
    /// file, or an older schema (pending migration) → [`ErrorKind::Config`]; newer →
    /// [`StoreError::SchemaTooNew`]; another application's → [`StoreError::NotScullDatabase`];
    /// damaged → [`StoreError::Corrupt`].
    pub fn open_read_only(path: &Path, options: &StoreOptions) -> Result<Store> {
        let known = u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX);
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => {
                return Err(StoreError::rejected(
                    ErrorKind::Config,
                    format!(
                        "{} is not a regular file; refusing to read it",
                        path.display()
                    ),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::rejected(
                    ErrorKind::NotFound,
                    format!("no database at {}", path.display()),
                ));
            }
            Err(e) => return Err(StoreError::io(path, e)),
        }
        let cls = |e: StoreError| classify(path, e);
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(StoreError::from)
        .map_err(cls)?;
        check_compatible(&conn, path, known).map_err(cls)?;
        let version = user_version(&conn).map_err(StoreError::from).map_err(cls)?;
        if version != known {
            return Err(StoreError::rejected(
                ErrorKind::Config,
                format!(
                    "{} has schema version {version} (this build reads {known}); it is migrated \
                     the next time scull opens it for writing",
                    path.display()
                ),
            ));
        }
        (|| -> rusqlite::Result<()> {
            conn.busy_timeout(options.busy_timeout)?;
            conn.pragma_update(None, "query_only", true)?;
            conn.pragma_update(None, "trusted_schema", false)?;
            conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
                r.get::<_, i64>(0)
            })?;
            Ok(())
        })()
        .map_err(StoreError::from)
        .map_err(cls)?;
        Ok(Store {
            conn,
            path: path.to_owned(),
            backups_taken: Vec::new(),
        })
    }

    pub(crate) fn open_inner(
        path: &Path,
        perms: &dyn OwnerOnly,
        options: &StoreOptions,
        migrations: &[Migration],
    ) -> Result<Store> {
        let known = u32::try_from(migrations.len()).unwrap_or(u32::MAX);
        prepare_location(path, perms)?;
        probe(path, known)?;
        for p in [
            path.to_owned(),
            with_suffix(path, "-wal"),
            with_suffix(path, "-shm"),
        ] {
            if std::fs::symlink_metadata(&p).is_ok() {
                perms
                    .restrict(&p)
                    .map_err(|source| StoreError::Permissions { path: p, source })?;
            }
        }

        let cls = |e: StoreError| classify(path, e);
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(StoreError::from)
        .map_err(cls)?;
        configure(&conn, options).map_err(cls)?;
        let backups_taken = migrate(&mut conn, path, migrations, perms).map_err(cls)?;
        // Read the whole schema once so a damaged schema page is reported now, not mid-scan.
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(StoreError::from)
        .map_err(cls)?;
        // The `-wal` and `-shm` files SQLite created just now, in WAL mode. On Unix they take
        // the database file's mode; on Windows they inherit the data folder's owner-only DACL
        // but are owned by the process's default owner, which for an elevated administrator is
        // the Administrators group, not the user. Restricting them makes the user the owner.
        for p in [with_suffix(path, "-wal"), with_suffix(path, "-shm")] {
            if std::fs::symlink_metadata(&p).is_ok() {
                perms
                    .restrict(&p)
                    .map_err(|source| StoreError::Permissions { path: p, source })?;
            }
        }
        for b in &backups_taken {
            perms
                .restrict(b)
                .map_err(|source| StoreError::Permissions {
                    path: b.clone(),
                    source,
                })?;
        }
        Ok(Store {
            conn,
            path: path.to_owned(),
            backups_taken,
        })
    }

    /// Database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Backups written by migrations during this open (empty if none ran).
    pub fn backups_taken(&self) -> &[PathBuf] {
        &self.backups_taken
    }

    /// Schema version of the open database (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<u32> {
        user_version(&self.conn)
            .map_err(StoreError::from)
            .map_err(|e| classify(&self.path, e))
    }

    /// Full read of every page (`PRAGMA quick_check`). O(database size): run it from
    /// `scull doctor` or scheduled maintenance, not on every open. Damage →
    /// [`StoreError::Corrupt`].
    pub fn check_integrity(&self) -> Result<()> {
        let path = &self.path;
        let result: Result<()> = (|| {
            let mut stmt = self.conn.prepare("PRAGMA quick_check")?;
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            if rows.len() == 1 && rows[0] == "ok" {
                Ok(())
            } else {
                Err(StoreError::Corrupt {
                    path: path.clone(),
                    detail: rows.join("; "),
                    backups: list_backups(path),
                })
            }
        })();
        result.map_err(|e| classify(path, e))
    }

    /// Record the start of a scan.
    pub fn record_scan_start(&mut self, scan: &ScanStart) -> Result<()> {
        self.write(|tx| {
            let roots = serde_json::to_string(&scan.roots).map_err(internal)?;
            tx.execute(
                "INSERT INTO scans (scan_id, started_at, roots_json, status) \
                 VALUES (?1, ?2, ?3, 'running')",
                params![scan.scan_id.as_str(), scan.started_at, roots],
            )?;
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO scan_roots (root, started_at, scan_id) \
                 VALUES (?1, ?2, ?3)",
            )?;
            for root in &scan.roots {
                stmt.execute(params![
                    root.as_str(),
                    scan.started_at,
                    scan.scan_id.as_str()
                ])?;
            }
            Ok(())
        })
    }

    /// Record the end of a running scan with its totals and byte rollups, atomically.
    /// [`Store::record_scan`] does this and records the history that goes with it; prefer it.
    pub fn record_scan_finish(&mut self, finish: &ScanFinish) -> Result<()> {
        self.write(|tx| finish_scan_tx(tx, finish))
    }

    /// Insert or refresh the lifecycle rows of candidates seen by `scan_id` at `seen_at`.
    /// First-seen time and scan are kept; everything else reflects the latest sighting. Item
    /// events and regrowth are recorded as by [`Store::record_scan`], which also marks absent
    /// items gone and should normally be used instead.
    pub fn upsert_items(
        &mut self,
        scan_id: &scull_core::ScanId,
        seen_at: i64,
        candidates: &[Candidate],
    ) -> Result<()> {
        self.write(|tx| {
            let mut out = crate::history::ScanRecorded::default();
            crate::history::observe_items(tx, scan_id, seen_at, candidates, &mut out)
        })
    }

    /// Record an immutable plan. The stored `canonical_json` is exactly what `content_hash`
    /// covers. Re-inserting the identical plan is a no-op; a *different* plan under an existing
    /// ID is rejected as tampering (plan substitution, §13.4).
    pub fn insert_plan(&mut self, plan: &Plan) -> Result<()> {
        let row = PlanRow::of(plan)?;
        self.write(|tx| row.insert(tx, plan))
    }

    /// Agent (MCP) plans of `machine_id` and `user` still awaiting an approval at `now`:
    /// unexpired, without any approval record, and not denied by the human (#299: an
    /// `approval_denied` entry with reason `denied` or `wrong_code` from `scull approve` or
    /// the desktop dialog; a refused attempt such as `no_terminal` does not count as one).
    pub fn pending_agent_plans(&self, machine_id: &str, user: &str, now: i64) -> Result<u64> {
        self.read(|conn| pending_agent_plans(conn, machine_id, user, now))
    }

    /// Record the agent plan `plan` (origin `mcp`) only if its machine and user have fewer
    /// than `max_pending` plans awaiting approval at `now` ([`Store::pending_agent_plans`]):
    /// the count and the insert are one transaction, so concurrent sessions can't exceed the
    /// cap (#299). Returns `false`, recording nothing, when the cap is reached. Otherwise as
    /// [`Store::insert_plan`].
    pub fn insert_agent_plan(&mut self, plan: &Plan, now: i64, max_pending: u32) -> Result<bool> {
        if plan.origin() != PlanOrigin::Mcp {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                "insert_agent_plan records agent (MCP) plans only",
            ));
        }
        let row = PlanRow::of(plan)?;
        self.write(|tx| {
            let pending = pending_agent_plans(tx, plan.machine_id(), plan.user(), now)?;
            if pending >= u64::from(max_pending) {
                return Ok(false);
            }
            row.insert(tx, plan)?;
            Ok(true)
        })
    }

    /// Load a recorded plan. The record is re-verified against its hash; a mismatch (the file
    /// was edited behind scull's back) is [`ErrorKind::PlanTampered`].
    pub fn get_plan(&self, id: &PlanId) -> Result<Option<Plan>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT content_hash, canonical_json FROM plans WHERE plan_id = ?1",
                [id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)
            .map_err(|e| classify(&self.path, e))?;
        let Some((hash, canonical)) = row else {
            return Ok(None);
        };
        let tampered = |m: String| StoreError::rejected(ErrorKind::PlanTampered, m);
        let mut value: serde_json::Value = serde_json::from_str(&canonical)
            .map_err(|e| tampered(format!("recorded plan {id} is not valid JSON: {e}")))?;
        let Some(map) = value.as_object_mut() else {
            return Err(tampered(format!("recorded plan {id} is not an object")));
        };
        map.insert(
            "content_hash".into(),
            serde_json::Value::String(hash.clone()),
        );
        let plan: Plan = serde_json::from_value(value)
            .map_err(|e| tampered(format!("recorded plan {id} does not parse: {e}")))?;
        if plan.compute_hash().map_err(core_err)?.as_str() != hash || plan.id() != id {
            return Err(tampered(format!(
                "recorded plan {id} does not match its hash"
            )));
        }
        Ok(Some(plan))
    }

    /// Record a human approval of a recorded plan (§13.3 step 3). Rejected if the plan is
    /// unknown, the hash differs from the recorded plan, the approval would outlive the plan,
    /// the plan already expired, or the plan already has an approval (single use: one approval
    /// per plan). Returns the approval's row ID.
    pub fn insert_approval(&mut self, a: &NewApproval) -> Result<i64> {
        self.write(|tx| {
            let plan: Option<(String, i64)> = tx
                .query_row(
                    "SELECT content_hash, expires_at FROM plans WHERE plan_id = ?1",
                    [a.plan_id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((hash, plan_expires)) = plan else {
                return Err(StoreError::rejected(
                    ErrorKind::NotFound,
                    format!("no recorded plan {}", a.plan_id),
                ));
            };
            if hash != a.content_hash.as_str() {
                return Err(StoreError::rejected(
                    ErrorKind::PlanTampered,
                    "approval hash does not match the recorded plan",
                ));
            }
            if a.approved_at >= plan_expires {
                return Err(StoreError::rejected(
                    ErrorKind::PlanExpired,
                    "the plan expired before it was approved",
                ));
            }
            if a.expires_at <= a.approved_at || a.expires_at > plan_expires {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    "approval expiry must be after approval time and no later than the plan's",
                ));
            }
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM approvals WHERE plan_id = ?1)",
                [a.plan_id.as_str()],
                |r| r.get(0),
            )?;
            if exists {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!("plan {} already has an approval (single use)", a.plan_id),
                ));
            }
            tx.execute(
                "INSERT INTO approvals (plan_id, content_hash, method, approver, approved_at, \
                 expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    a.plan_id.as_str(),
                    a.content_hash.as_str(),
                    a.method.as_str(),
                    a.approver,
                    a.approved_at,
                    a.expires_at,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    /// The approval recorded for `plan_id`, if any.
    pub fn get_approval(&self, plan_id: &PlanId) -> Result<Option<Approval>> {
        let row = self
            .conn
            .query_row(
                "SELECT approval_id, plan_id, content_hash, method, approver, approved_at, \
                 expires_at, used_at FROM approvals WHERE plan_id = ?1",
                [plan_id.as_str()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(StoreError::from)
            .map_err(|e| classify(&self.path, e))?;
        let Some((approval_id, pid, hash, method, approver, approved_at, expires_at, used_at)) =
            row
        else {
            return Ok(None);
        };
        let bad = |m: String| StoreError::rejected(ErrorKind::Internal, m);
        Ok(Some(Approval {
            approval_id,
            plan_id: PlanId::try_from(pid).map_err(bad)?,
            content_hash: ContentHash::try_from(hash).map_err(bad)?,
            method: ApprovalMethod::parse(&method)
                .ok_or_else(|| bad(format!("unknown approval method {method:?}")))?,
            approver,
            approved_at,
            expires_at,
            used_at,
        }))
    }

    /// Atomically consume the approval for `plan_id` (§13.3 step 4). Returns `true` only if a
    /// matching approval exists whose hash equals `hash` and the recorded plan's hash, that is
    /// unused, already given (`approved_at <= now`), and unexpired (`now` before both the
    /// approval's and the plan's expiry). It is then marked used at `now` and can never be used
    /// again. Every other case returns `false` (fail closed).
    pub fn consume_approval(
        &mut self,
        plan_id: &PlanId,
        hash: &ContentHash,
        now: i64,
    ) -> Result<bool> {
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE approvals SET used_at = ?3 \
                 WHERE plan_id = ?1 AND content_hash = ?2 AND used_at IS NULL \
                   AND approved_at <= ?3 AND ?3 < expires_at \
                   AND EXISTS (SELECT 1 FROM plans p WHERE p.plan_id = approvals.plan_id \
                               AND p.content_hash = approvals.content_hash \
                               AND ?3 < p.expires_at)",
                params![plan_id.as_str(), hash.as_str(), now],
            )?;
            Ok(changed == 1)
        })
    }

    /// Plans made by `origin` that are still awaiting an approval at `now`: unexpired
    /// (`now < expires_at`), without any approval record, and not denied by the human (as in
    /// [`Store::pending_agent_plans`]). Newest first, at most `limit`.
    /// Used to list what a human can approve (`scull approve`, the desktop dialog; §13.3).
    pub fn plans_awaiting_approval(
        &self,
        origin: PlanOrigin,
        now: i64,
        limit: u32,
    ) -> Result<Vec<PlanId>> {
        let origin = serde_name(&origin)?;
        let ids: Vec<String> = self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT plan_id FROM plans p WHERE p.origin = ?1 AND ?2 < p.expires_at \
                 AND NOT EXISTS (SELECT 1 FROM approvals a WHERE a.plan_id = p.plan_id) \
                 AND NOT EXISTS (SELECT 1 FROM events e WHERE e.kind = 'approval_denied' \
                     AND e.source IN ('cli', 'desktop') \
                     AND json_extract(e.payload_json, '$.plan_id') = p.plan_id \
                     AND json_extract(e.payload_json, '$.reason') IN ('denied', 'wrong_code')) \
                 ORDER BY p.created_at DESC, p.plan_id DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![origin, now, limit], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        ids.into_iter()
            .map(|id| {
                PlanId::try_from(id).map_err(|e| StoreError::rejected(ErrorKind::Internal, e))
            })
            .collect()
    }

    /// Plans made by `origin` with an approval that is unused and unexpired at `now`
    /// (approved, not yet executed or revoked; #298). Newest first, at most `limit`.
    pub fn plans_with_live_approval(
        &self,
        origin: PlanOrigin,
        now: i64,
        limit: u32,
    ) -> Result<Vec<PlanId>> {
        let origin = serde_name(&origin)?;
        let ids: Vec<String> = self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT p.plan_id FROM plans p JOIN approvals a ON a.plan_id = p.plan_id \
                 WHERE p.origin = ?1 AND ?2 < p.expires_at AND a.used_at IS NULL \
                 AND a.approved_at <= ?2 AND ?2 < a.expires_at \
                 ORDER BY p.created_at DESC, p.plan_id DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![origin, now, limit], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        ids.into_iter()
            .map(|id| {
                PlanId::try_from(id).map_err(|e| StoreError::rejected(ErrorKind::Internal, e))
            })
            .collect()
    }

    /// Run `f` in a `BEGIN IMMEDIATE` transaction (waits up to the busy timeout for the write
    /// lock up front, so it never fails half-way on lock upgrade) and commit if it succeeds.
    pub(crate) fn write<T>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let path = &self.path;
        let result = (|| {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })();
        result.map_err(|e| classify(path, e))
    }

    /// Run a read-only query `f`, classifying corruption errors.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.conn).map_err(|e| classify(&self.path, e))
    }
}

/// What [`Store::insert_plan`] writes, computed and checked before the transaction.
struct PlanRow {
    canonical: String,
    reclaimable: u64,
}

impl PlanRow {
    fn of(plan: &Plan) -> Result<Self> {
        let canonical = plan.canonical_json().map_err(core_err)?;
        if plan.compute_hash().map_err(core_err)? != *plan.content_hash() {
            return Err(StoreError::rejected(
                ErrorKind::PlanTampered,
                "plan content does not match its hash",
            ));
        }
        let reclaimable = plan
            .items()
            .iter()
            .fold(0u64, |a, i| a.saturating_add(i.reclaimable_bytes));
        Ok(Self {
            canonical,
            reclaimable,
        })
    }

    /// Insert `plan` (idempotent for the same content; a different plan under its ID is
    /// [`ErrorKind::PlanTampered`]).
    fn insert(&self, tx: &rusqlite::Transaction<'_>, plan: &Plan) -> Result<()> {
        let existing: Option<String> = tx
            .query_row(
                "SELECT content_hash FROM plans WHERE plan_id = ?1",
                [plan.id().as_str()],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(h) if h == plan.content_hash().as_str() => return Ok(()),
            Some(_) => {
                return Err(StoreError::rejected(
                    ErrorKind::PlanTampered,
                    format!("a different plan is already recorded as {}", plan.id()),
                ));
            }
            None => {}
        }
        tx.execute(
            "INSERT INTO plans (plan_id, content_hash, origin, scan_id, created_at, \
             expires_at, item_count, reclaimable_bytes, canonical_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                plan.id().as_str(),
                plan.content_hash().as_str(),
                serde_name(&plan.origin())?,
                plan.scan_id().as_str(),
                plan.created_unix(),
                plan.expires_unix(),
                int(plan.items().len() as u64)?,
                int(self.reclaimable)?,
                self.canonical,
            ],
        )?;
        Ok(())
    }
}

/// See [`Store::pending_agent_plans`]. Machine and user come from the plan's canonical JSON
/// (the record verified against its hash on every read).
fn pending_agent_plans(conn: &Connection, machine_id: &str, user: &str, now: i64) -> Result<u64> {
    let origin = serde_name(&PlanOrigin::Mcp)?;
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM plans p WHERE p.origin = ?1 AND ?2 < p.expires_at \
         AND NOT EXISTS (SELECT 1 FROM approvals a WHERE a.plan_id = p.plan_id) \
         AND NOT EXISTS (SELECT 1 FROM events e WHERE e.kind = 'approval_denied' \
             AND e.source IN ('cli', 'desktop') \
             AND json_extract(e.payload_json, '$.plan_id') = p.plan_id \
             AND json_extract(e.payload_json, '$.reason') IN ('denied', 'wrong_code')) \
         AND json_extract(p.canonical_json, '$.machine_id') = ?3 \
         AND json_extract(p.canonical_json, '$.user') = ?4",
        params![origin, now, machine_id, user],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Mark a running scan finished and insert its rollups (inside the caller's transaction).
pub(crate) fn finish_scan_tx(tx: &rusqlite::Transaction<'_>, finish: &ScanFinish) -> Result<()> {
    let t = &finish.totals;
    let changed = tx.execute(
        "UPDATE scans SET finished_at = ?2, status = ?3, entries_scanned = ?4, \
             candidates = ?5, logical_bytes = ?6, allocated_bytes = ?7, \
             reclaimable_bytes = ?8, errors = ?9 \
             WHERE scan_id = ?1 AND status = 'running'",
        params![
            finish.scan_id.as_str(),
            finish.finished_at,
            finish.status.as_str(),
            int(t.entries_scanned)?,
            int(t.candidates)?,
            int(t.logical_bytes)?,
            int(t.allocated_bytes)?,
            int(t.reclaimable_bytes)?,
            int(t.errors)?,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::rejected(
            ErrorKind::NotFound,
            format!("no running scan {}", finish.scan_id),
        ));
    }
    let mut stmt = tx.prepare(
        "INSERT INTO scan_rollups (scan_id, category, tier, ecosystem, items, \
             logical_bytes, allocated_bytes, reclaimable_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for r in &finish.rollups {
        stmt.execute(params![
            finish.scan_id.as_str(),
            serde_name(&r.category)?,
            r.tier.to_string(),
            r.ecosystem.as_deref().unwrap_or(""),
            int(r.items)?,
            int(r.logical_bytes)?,
            int(r.allocated_bytes)?,
            int(r.reclaimable_bytes)?,
        ])?;
    }
    Ok(())
}

/// Create the parent directory and an empty database file owner-only if they are missing.
fn prepare_location(path: &Path, perms: &dyn OwnerOnly) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && std::fs::symlink_metadata(parent).is_err()
    {
        if let Some(grand) = parent.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(grand).map_err(|e| StoreError::io(grand, e))?;
        }
        match perms.create_dir(parent) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(StoreError::Permissions {
                    path: parent.to_owned(),
                    source,
                });
            }
        }
    }
    match std::fs::symlink_metadata(path) {
        // Never follow a planted link to some other file (T-50).
        Ok(m) if !m.is_file() => Err(StoreError::rejected(
            ErrorKind::Config,
            format!(
                "{} is not a regular file; refusing to use it",
                path.display()
            ),
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match perms.create_file(path) {
            Ok(()) => Ok(()),
            // Another process created it first; fine, it is probed next.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(source) => Err(StoreError::Permissions {
                path: path.to_owned(),
                source,
            }),
        },
        Err(e) => Err(StoreError::io(path, e)),
    }
}

/// Read-only compatibility and damage check of an existing database. Writes nothing.
fn probe(path: &Path, known: u32) -> Result<()> {
    let len = std::fs::metadata(path)
        .map_err(|e| StoreError::io(path, e))?
        .len();
    if len == 0 {
        return Ok(()); // freshly created (by us or a racing scull): nothing to check.
    }
    let result = (|| {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        check_compatible(&conn, path, known)?;
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })?;
        Ok(())
    })();
    result.map_err(|e| classify(path, e))
}

fn configure(conn: &Connection, options: &StoreOptions) -> Result<()> {
    conn.busy_timeout(options.busy_timeout)?;
    // WAL is persistent in the file; switching only when needed avoids taking a lock on
    // every open.
    let current: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    let mode = if current.eq_ignore_ascii_case("wal") {
        current
    } else {
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?
    };
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::rejected(
            ErrorKind::UnsupportedPlatform,
            format!(
                "the database location does not support WAL mode (got {mode:?}); use a local, \
                 non-network data directory"
            ),
        ));
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "trusted_schema", false)?;
    Ok(())
}

/// Turn SQLite corruption errors into [`StoreError::Corrupt`] with the backups on offer.
fn classify(path: &Path, e: StoreError) -> StoreError {
    match e {
        StoreError::Sqlite(ref inner) if is_corruption(inner) => StoreError::Corrupt {
            path: path.to_owned(),
            detail: inner.to_string(),
            backups: list_backups(path),
        },
        other => other,
    }
}

pub(crate) fn int(v: u64) -> Result<i64> {
    i64::try_from(v)
        .map_err(|_| StoreError::rejected(ErrorKind::Usage, format!("value {v} is too large")))
}

/// The serde (snake_case) name of a unit enum variant, as used in scull's JSON.
pub(crate) fn serde_name<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v).map_err(internal)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(StoreError::rejected(
            ErrorKind::Internal,
            format!("expected a string enum, got {other}"),
        )),
    }
}

pub(crate) fn internal(e: serde_json::Error) -> StoreError {
    StoreError::rejected(ErrorKind::Internal, e.to_string())
}

fn core_err(e: scull_core::ScullError) -> StoreError {
    StoreError::rejected(e.kind, e.message)
}
