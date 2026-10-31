-- scull local store, schema version 1 (PRD §18.3, DATA-02).
--
-- Conventions:
--   * Timestamps are Unix seconds (UTC) supplied by the caller (`*_at` columns).
--   * Byte counts are non-negative INTEGERs.
--   * Tiers are 'S0'..'S3'; categories and other enums are snake_case strings matching the
--     serde names in scull-core.
--   * Rollups only: no table stores one row per scanned file (§18.3 "rollups rather than
--     every file"). `items` holds detected candidates (directories such as node_modules),
--     not their contents.
--   * Plans, approvals, operations and audit are append-only, enforced by triggers below.
--     The quarantine manifest is the record of truth (SAFE-16): rows are never deleted and
--     recorded paths never change.
--
-- This file runs inside one transaction; `user_version` is set by the migration runner.

PRAGMA application_id = 1396921676; -- 0x5343554C, "SCUL": marks the file as a scull store.

-- config / profile -------------------------------------------------------------------------

CREATE TABLE meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE profile (
    key        TEXT PRIMARY KEY NOT NULL,
    value_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

-- scans and snapshots (rollups) -----------------------------------------------------------

CREATE TABLE scans (
    scan_id           TEXT PRIMARY KEY NOT NULL,
    started_at        INTEGER NOT NULL,
    finished_at       INTEGER,
    roots_json        TEXT NOT NULL,
    status            TEXT NOT NULL
        CHECK (status IN ('running', 'completed', 'partial', 'failed', 'cancelled')),
    entries_scanned   INTEGER CHECK (entries_scanned >= 0),
    candidates        INTEGER CHECK (candidates >= 0),
    logical_bytes     INTEGER CHECK (logical_bytes >= 0),
    allocated_bytes   INTEGER CHECK (allocated_bytes >= 0),
    reclaimable_bytes INTEGER CHECK (reclaimable_bytes >= 0),
    errors            INTEGER CHECK (errors >= 0),
    CHECK (finished_at IS NULL OR finished_at >= started_at),
    CHECK ((status = 'running') = (finished_at IS NULL))
) STRICT;

CREATE INDEX scans_started ON scans (started_at);

-- Per-scan byte rollups by category, tier and ecosystem ('' when unknown).
CREATE TABLE scan_rollups (
    scan_id           TEXT NOT NULL REFERENCES scans (scan_id),
    category          TEXT NOT NULL,
    tier              TEXT NOT NULL CHECK (tier IN ('S0', 'S1', 'S2', 'S3')),
    ecosystem         TEXT NOT NULL DEFAULT '',
    items             INTEGER NOT NULL CHECK (items >= 0),
    logical_bytes     INTEGER NOT NULL CHECK (logical_bytes >= 0),
    allocated_bytes   INTEGER NOT NULL CHECK (allocated_bytes >= 0),
    reclaimable_bytes INTEGER NOT NULL CHECK (reclaimable_bytes >= 0),
    PRIMARY KEY (scan_id, category, tier, ecosystem)
) STRICT;

-- Free space per volume, sampled at each scan or scheduled check (INS-01, INS-06).
CREATE TABLE free_space_samples (
    sample_id   INTEGER PRIMARY KEY,
    sampled_at  INTEGER NOT NULL,
    scan_id     TEXT REFERENCES scans (scan_id),
    volume      TEXT NOT NULL,
    total_bytes INTEGER NOT NULL CHECK (total_bytes >= 0),
    free_bytes  INTEGER NOT NULL CHECK (free_bytes >= 0)
) STRICT;

CREATE INDEX free_space_samples_volume ON free_space_samples (volume, sampled_at);

-- items (candidate lifecycle) --------------------------------------------------------------

CREATE TABLE items (
    item_id           TEXT PRIMARY KEY NOT NULL,
    path              TEXT NOT NULL,
    category          TEXT NOT NULL,
    artifact          TEXT NOT NULL,
    ecosystem         TEXT,
    project_root      TEXT,
    tier              TEXT NOT NULL CHECK (tier IN ('S0', 'S1', 'S2', 'S3')),
    first_seen_at     INTEGER NOT NULL,
    last_seen_at      INTEGER NOT NULL,
    first_seen_scan   TEXT NOT NULL REFERENCES scans (scan_id),
    last_seen_scan    TEXT NOT NULL REFERENCES scans (scan_id),
    logical_bytes     INTEGER NOT NULL CHECK (logical_bytes >= 0),
    allocated_bytes   INTEGER NOT NULL CHECK (allocated_bytes >= 0),
    reclaimable_bytes INTEGER NOT NULL CHECK (reclaimable_bytes >= 0),
    CHECK (last_seen_at >= first_seen_at)
) STRICT;

CREATE INDEX items_last_seen ON items (last_seen_at);

-- plans (append-only) ----------------------------------------------------------------------

-- `canonical_json` is exactly the byte string whose SHA-256 is `content_hash`
-- (scull_core::Plan::canonical_json), so the record can be verified with standard tools.
CREATE TABLE plans (
    plan_id           TEXT PRIMARY KEY NOT NULL,
    content_hash      TEXT NOT NULL CHECK (length(content_hash) = 64),
    origin            TEXT NOT NULL,
    scan_id           TEXT NOT NULL,
    created_at        INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL,
    item_count        INTEGER NOT NULL CHECK (item_count >= 0),
    reclaimable_bytes INTEGER NOT NULL CHECK (reclaimable_bytes >= 0),
    canonical_json    TEXT NOT NULL,
    CHECK (expires_at > created_at)
) STRICT;

CREATE TRIGGER plans_no_update BEFORE UPDATE ON plans
BEGIN
    SELECT RAISE(ABORT, 'plans are append-only');
END;

CREATE TRIGGER plans_no_delete BEFORE DELETE ON plans
BEGIN
    SELECT RAISE(ABORT, 'plans are append-only');
END;

-- approvals (append-only; `used_at` may be set exactly once) -------------------------------

-- One approval per plan: approvals are single-use, so a plan can be executed at most once.
CREATE TABLE approvals (
    approval_id  INTEGER PRIMARY KEY,
    plan_id      TEXT NOT NULL UNIQUE REFERENCES plans (plan_id),
    content_hash TEXT NOT NULL CHECK (length(content_hash) = 64),
    method       TEXT NOT NULL,
    approver     TEXT NOT NULL,
    approved_at  INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    used_at      INTEGER,
    CHECK (expires_at > approved_at),
    CHECK (used_at IS NULL OR used_at >= approved_at)
) STRICT;

-- An approval must bind to the exact plan content it was shown (§13.3, hash binding).
CREATE TRIGGER approvals_hash_matches_plan BEFORE INSERT ON approvals
WHEN NEW.content_hash IS NOT (SELECT content_hash FROM plans WHERE plan_id = NEW.plan_id)
BEGIN
    SELECT RAISE(ABORT, 'approval hash does not match the recorded plan');
END;

CREATE TRIGGER approvals_insert_unused BEFORE INSERT ON approvals
WHEN NEW.used_at IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'a new approval cannot already be used');
END;

CREATE TRIGGER approvals_update_only_use BEFORE UPDATE ON approvals
WHEN OLD.used_at IS NOT NULL
  OR NEW.used_at IS NULL
  OR NEW.approval_id IS NOT OLD.approval_id
  OR NEW.plan_id IS NOT OLD.plan_id
  OR NEW.content_hash IS NOT OLD.content_hash
  OR NEW.method IS NOT OLD.method
  OR NEW.approver IS NOT OLD.approver
  OR NEW.approved_at IS NOT OLD.approved_at
  OR NEW.expires_at IS NOT OLD.expires_at
BEGIN
    SELECT RAISE(ABORT, 'approvals are append-only (only used_at may be set, once)');
END;

CREATE TRIGGER approvals_no_delete BEFORE DELETE ON approvals
BEGIN
    SELECT RAISE(ABORT, 'approvals are append-only');
END;

-- operations and audit (append-only, SAFE-08) ----------------------------------------------

CREATE TABLE operations (
    operation_id TEXT PRIMARY KEY NOT NULL,
    plan_id      TEXT NOT NULL REFERENCES plans (plan_id),
    content_hash TEXT NOT NULL CHECK (length(content_hash) = 64),
    approver     TEXT NOT NULL,
    method       TEXT NOT NULL,
    started_at   INTEGER NOT NULL
) STRICT;

CREATE TRIGGER operations_hash_matches_plan BEFORE INSERT ON operations
WHEN NEW.content_hash IS NOT (SELECT content_hash FROM plans WHERE plan_id = NEW.plan_id)
BEGIN
    SELECT RAISE(ABORT, 'operation hash does not match the recorded plan');
END;

CREATE TRIGGER operations_no_update BEFORE UPDATE ON operations
BEGIN
    SELECT RAISE(ABORT, 'operations are append-only');
END;

CREATE TRIGGER operations_no_delete BEFORE DELETE ON operations
BEGIN
    SELECT RAISE(ABORT, 'operations are append-only');
END;

-- One row per item outcome (item_id set) or operation-level event (item_id NULL).
-- Plan hash, approver and method come from the operation row, which is itself append-only.
CREATE TABLE audit (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id    TEXT NOT NULL REFERENCES operations (operation_id),
    recorded_at     INTEGER NOT NULL,
    item_id         TEXT,
    path            TEXT,
    action          TEXT,
    outcome         TEXT NOT NULL,
    reason          TEXT,
    predicted_bytes INTEGER CHECK (predicted_bytes >= 0),
    measured_bytes  INTEGER CHECK (measured_bytes >= 0)
) STRICT;

CREATE INDEX audit_operation ON audit (operation_id, seq);

CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

-- quarantine manifest of record (SAFE-16) --------------------------------------------------

CREATE TABLE quarantine_manifest (
    entry_id        INTEGER PRIMARY KEY,
    operation_id    TEXT NOT NULL REFERENCES operations (operation_id),
    item_id         TEXT NOT NULL,
    original_path   TEXT NOT NULL,
    quarantine_path TEXT NOT NULL UNIQUE,
    bytes           INTEGER NOT NULL CHECK (bytes >= 0),
    quarantined_at  INTEGER NOT NULL,
    expires_at      INTEGER,
    restored_at     INTEGER,
    restored_to     TEXT,
    purged_at       INTEGER,
    CHECK ((restored_at IS NULL) = (restored_to IS NULL)),
    CHECK (restored_at IS NULL OR purged_at IS NULL)
) STRICT;

-- Recorded paths and facts never change; restore/purge markers are set once.
CREATE TRIGGER quarantine_manifest_update BEFORE UPDATE ON quarantine_manifest
WHEN NEW.entry_id IS NOT OLD.entry_id
  OR NEW.operation_id IS NOT OLD.operation_id
  OR NEW.item_id IS NOT OLD.item_id
  OR NEW.original_path IS NOT OLD.original_path
  OR NEW.quarantine_path IS NOT OLD.quarantine_path
  OR NEW.bytes IS NOT OLD.bytes
  OR NEW.quarantined_at IS NOT OLD.quarantined_at
  OR NEW.expires_at IS NOT OLD.expires_at
  OR (OLD.restored_at IS NOT NULL AND NEW.restored_at IS NOT OLD.restored_at)
  OR (OLD.restored_to IS NOT NULL AND NEW.restored_to IS NOT OLD.restored_to)
  OR (OLD.purged_at IS NOT NULL AND NEW.purged_at IS NOT OLD.purged_at)
BEGIN
    SELECT RAISE(ABORT, 'quarantine manifest entries are immutable once recorded');
END;

CREATE TRIGGER quarantine_manifest_no_delete BEFORE DELETE ON quarantine_manifest
BEGIN
    SELECT RAISE(ABORT, 'quarantine manifest entries are never deleted');
END;

-- events (alerts, usage, agent activity) and metrics (performance) -------------------------

CREATE TABLE events (
    event_id     INTEGER PRIMARY KEY,
    occurred_at  INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    source       TEXT NOT NULL,
    payload_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

CREATE INDEX events_kind ON events (kind, occurred_at);

CREATE TABLE metrics (
    metric_id   INTEGER PRIMARY KEY,
    recorded_at INTEGER NOT NULL,
    name        TEXT NOT NULL,
    value       REAL NOT NULL,
    unit        TEXT NOT NULL DEFAULT '',
    labels_json TEXT NOT NULL DEFAULT '{}'
) STRICT;

CREATE INDEX metrics_name ON metrics (name, recorded_at);
