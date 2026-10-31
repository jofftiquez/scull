-- scull local store, schema version 5: the quarantine log (SAFE-08, SAFE-16, P2-EXEC-06).
--
-- Restores and purges are not plan executions, so they are not in the per-operation audit
-- chain; every attempt (done, no-op or refused) is recorded here instead, append-only. The
-- manifest row's `restored_*` / `purged_*` markers say what the entry's state is; this log
-- says who did what when, including refusals.
--
-- This file runs inside one transaction; `user_version` is set by the migration runner.

CREATE TABLE quarantine_log (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id    INTEGER NOT NULL REFERENCES quarantine_manifest (entry_id),
    recorded_at INTEGER NOT NULL,
    event       TEXT NOT NULL CHECK (event IN (
        'restored', 'restore_noop', 'restore_refused', 'purged', 'purge_kept')),
    path        TEXT,
    reason      TEXT,
    bytes       INTEGER CHECK (bytes >= 0)
) STRICT;

CREATE INDEX quarantine_log_entry ON quarantine_log (entry_id, seq);

CREATE TRIGGER quarantine_log_no_update BEFORE UPDATE ON quarantine_log
BEGIN
    SELECT RAISE(ABORT, 'the quarantine log is append-only');
END;

CREATE TRIGGER quarantine_log_no_delete BEFORE DELETE ON quarantine_log
BEGIN
    SELECT RAISE(ABORT, 'the quarantine log is append-only');
END;
