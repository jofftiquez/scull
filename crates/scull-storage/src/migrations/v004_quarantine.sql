-- scull local store, schema version 4: operation targets (SAFE-09) and the quarantine manifest
-- facts restore needs (SAFE-16, D-3, P2-EXEC-05/06).
--
-- * `operation_targets`: the file identity of every item an operation acted on (or was about
--   to), recorded when the operation starts. A plan is invalidated when another operation that
--   started after the plan was created touched any of its targets. Append-only.
-- * `quarantine_manifest` gains the identity, kind and size of the quarantined entry and the
--   identity of the directory it came from, so restore can verify the entry before moving it
--   back and only ever into the same directory. Identities are `<volume hex>:<file id hex>`.
--   All recorded facts are immutable; `restored_*` and `purged_*` are set once, from NULL.
--
-- This file runs inside one transaction; `user_version` is set by the migration runner.

-- operation targets ---------------------------------------------------------------------------

CREATE TABLE operation_targets (
    operation_id TEXT NOT NULL REFERENCES operations (operation_id),
    item_id      TEXT NOT NULL,
    identity     TEXT NOT NULL CHECK (length(identity) > 0),
    recorded_at  INTEGER NOT NULL,
    PRIMARY KEY (operation_id, item_id)
) STRICT;

CREATE INDEX operation_targets_identity ON operation_targets (identity, operation_id);

CREATE TRIGGER operation_targets_no_update BEFORE UPDATE ON operation_targets
BEGIN
    SELECT RAISE(ABORT, 'operation targets are append-only');
END;

CREATE TRIGGER operation_targets_no_delete BEFORE DELETE ON operation_targets
BEGIN
    SELECT RAISE(ABORT, 'operation targets are append-only');
END;

-- quarantine manifest -------------------------------------------------------------------------

ALTER TABLE quarantine_manifest ADD COLUMN entry_identity TEXT;
ALTER TABLE quarantine_manifest ADD COLUMN entry_kind TEXT;
ALTER TABLE quarantine_manifest ADD COLUMN parent_identity TEXT;
ALTER TABLE quarantine_manifest ADD COLUMN logical_bytes INTEGER CHECK (logical_bytes >= 0);
ALTER TABLE quarantine_manifest ADD COLUMN purged_bytes INTEGER CHECK (purged_bytes >= 0);

CREATE INDEX quarantine_manifest_held ON quarantine_manifest (purged_at, restored_at, expires_at);

-- Rows written from v4 on carry the integrity facts.
CREATE TRIGGER quarantine_manifest_v4_facts BEFORE INSERT ON quarantine_manifest
WHEN NEW.entry_identity IS NULL OR NEW.entry_kind IS NULL OR NEW.parent_identity IS NULL
  OR NEW.logical_bytes IS NULL OR NEW.purged_bytes IS NOT NULL OR NEW.purged_at IS NOT NULL
  OR NEW.restored_at IS NOT NULL OR NEW.restored_to IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'a quarantine entry needs its identity, kind, parent and size, and starts held');
END;

DROP TRIGGER quarantine_manifest_update;

-- Recorded paths and facts never change; restore/purge markers are set once, from NULL, and a
-- purge records its freed bytes together with its time.
CREATE TRIGGER quarantine_manifest_update BEFORE UPDATE ON quarantine_manifest
WHEN NEW.entry_id IS NOT OLD.entry_id
  OR NEW.operation_id IS NOT OLD.operation_id
  OR NEW.item_id IS NOT OLD.item_id
  OR NEW.original_path IS NOT OLD.original_path
  OR NEW.quarantine_path IS NOT OLD.quarantine_path
  OR NEW.bytes IS NOT OLD.bytes
  OR NEW.quarantined_at IS NOT OLD.quarantined_at
  OR NEW.expires_at IS NOT OLD.expires_at
  OR NEW.entry_identity IS NOT OLD.entry_identity
  OR NEW.entry_kind IS NOT OLD.entry_kind
  OR NEW.parent_identity IS NOT OLD.parent_identity
  OR NEW.logical_bytes IS NOT OLD.logical_bytes
  OR (OLD.restored_at IS NOT NULL AND NEW.restored_at IS NOT OLD.restored_at)
  OR (OLD.restored_to IS NOT NULL AND NEW.restored_to IS NOT OLD.restored_to)
  OR (OLD.purged_at IS NOT NULL AND NEW.purged_at IS NOT OLD.purged_at)
  OR (OLD.purged_bytes IS NOT NULL AND NEW.purged_bytes IS NOT OLD.purged_bytes)
  OR ((NEW.purged_at IS NULL) <> (NEW.purged_bytes IS NULL))
BEGIN
    SELECT RAISE(ABORT, 'quarantine manifest entries are immutable once recorded');
END;
