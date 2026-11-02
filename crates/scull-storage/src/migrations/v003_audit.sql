-- scull local store, schema version 3: hash-chained audit log (SAFE-08, DATA-02, PRD §13.3).
--
-- The v1 `audit` table is renamed `audit_v1` and frozen (no insert, update or delete). Its rows
-- are copied, in order, into the new chained `audit` table by the Rust step that runs right
-- after this file in the same transaction (`audit::migrate_v3_legacy`), as `legacy` rows after a
-- `started` row for their operation.
--
-- New `audit` table: one row per lifecycle event of an operation, in a per-operation chain.
--   * `op_seq` 0 is the `started` row: it repeats the operation's plan hash, approver, method
--     and start time, so editing the `operations` row is detected by `verify_chain`.
--   * `item` rows: exactly one outcome per plan item (unique index below).
--   * `finished` row: status and totals (predicted vs moved vs freed bytes, measured delta).
--     Nothing can be appended after it.
--   * `prev_hash` is the previous row's `row_hash` (the first row uses a genesis hash derived
--     from the operation ID); `row_hash` = sha256(prev_hash || canonical JSON of the row). The
--     canonical form is computed by the store (`audit::RowBody`), not stored, so editing any
--     column breaks the chain even if the triggers below are bypassed.
--
-- This file runs inside one transaction; `user_version` is set by the migration runner.

-- freeze the v1 table ----------------------------------------------------------------------

DROP TRIGGER audit_no_update;
DROP TRIGGER audit_no_delete;
DROP INDEX audit_operation;
ALTER TABLE audit RENAME TO audit_v1;

CREATE TRIGGER audit_v1_no_insert BEFORE INSERT ON audit_v1
BEGIN
    SELECT RAISE(ABORT, 'audit_v1 is frozen since schema v3; append to audit');
END;

CREATE TRIGGER audit_v1_no_update BEFORE UPDATE ON audit_v1
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TRIGGER audit_v1_no_delete BEFORE DELETE ON audit_v1
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

-- chained audit ----------------------------------------------------------------------------

CREATE TABLE audit (
    seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id         TEXT NOT NULL REFERENCES operations (operation_id),
    op_seq               INTEGER NOT NULL CHECK (op_seq >= 0),
    kind                 TEXT NOT NULL CHECK (kind IN ('started', 'item', 'finished', 'legacy')),
    recorded_at          INTEGER NOT NULL,
    -- started
    plan_id              TEXT,
    content_hash         TEXT,
    approver             TEXT,
    method               TEXT,
    -- item (and legacy)
    item_id              TEXT,
    path                 TEXT,
    action               TEXT,
    outcome              TEXT,
    reason               TEXT,
    -- item: per item; finished: totals
    predicted_bytes      INTEGER CHECK (predicted_bytes >= 0),
    moved_bytes          INTEGER CHECK (moved_bytes >= 0),
    freed_bytes          INTEGER CHECK (freed_bytes >= 0),
    entries_removed      INTEGER CHECK (entries_removed >= 0),
    entries_kept         INTEGER CHECK (entries_kept >= 0),
    -- finished
    status               TEXT,
    items_planned        INTEGER CHECK (items_planned >= 0),
    items_done           INTEGER CHECK (items_done >= 0),
    items_partial        INTEGER CHECK (items_partial >= 0),
    items_failed         INTEGER CHECK (items_failed >= 0),
    items_refused        INTEGER CHECK (items_refused >= 0),
    items_skipped        INTEGER CHECK (items_skipped >= 0),
    measured_delta_bytes INTEGER,
    -- chain
    prev_hash            TEXT NOT NULL CHECK (length(prev_hash) = 64),
    row_hash             TEXT NOT NULL UNIQUE CHECK (length(row_hash) = 64),
    UNIQUE (operation_id, op_seq),
    CHECK ((kind = 'started') = (op_seq = 0)),
    CHECK (kind <> 'started' OR (plan_id IS NOT NULL AND content_hash IS NOT NULL
                                 AND approver IS NOT NULL AND method IS NOT NULL)),
    CHECK (kind <> 'item' OR (item_id IS NOT NULL AND outcome IN (
        'removed', 'quarantined', 'moved_to_trash', 'tool_run',
        'partial', 'failed', 'refused', 'skipped'))),
    CHECK (kind <> 'finished' OR status IN (
        'completed', 'completed_with_errors', 'failed', 'cancelled', 'interrupted'))
) STRICT;

-- One outcome per plan item and operation (FX-EXEC-13, invariant I-4).
CREATE UNIQUE INDEX audit_one_outcome_per_item ON audit (operation_id, item_id)
    WHERE kind = 'item';

CREATE INDEX audit_kind ON audit (kind, operation_id);

-- Rows are appended at the end of their operation's chain, never in the middle.
CREATE TRIGGER audit_append_in_order BEFORE INSERT ON audit
WHEN NEW.op_seq IS NOT (SELECT coalesce(max(op_seq) + 1, 0) FROM audit
                        WHERE operation_id = NEW.operation_id)
BEGIN
    SELECT RAISE(ABORT, 'audit rows must be appended in order');
END;

-- A finished operation is closed.
CREATE TRIGGER audit_closed_after_finish BEFORE INSERT ON audit
WHEN EXISTS (SELECT 1 FROM audit WHERE operation_id = NEW.operation_id AND kind = 'finished')
BEGIN
    SELECT RAISE(ABORT, 'the operation is finished; its audit is closed');
END;

-- The started row repeats the operation row exactly.
CREATE TRIGGER audit_started_matches_operation BEFORE INSERT ON audit
WHEN NEW.kind = 'started' AND NOT EXISTS (
    SELECT 1 FROM operations o
    WHERE o.operation_id = NEW.operation_id AND o.plan_id IS NEW.plan_id
      AND o.content_hash IS NEW.content_hash AND o.approver IS NEW.approver
      AND o.method IS NEW.method AND o.started_at IS NEW.recorded_at)
BEGIN
    SELECT RAISE(ABORT, 'started row does not match the operation');
END;

CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit
BEGIN
    SELECT RAISE(ABORT, 'audit is append-only');
END;

-- `operations(since)` walks start times.
CREATE INDEX operations_started ON operations (started_at, operation_id);
