-- scull local store, schema version 6: the write-ahead operation journal (SAFE-19, T-15,
-- P2-EXEC-08).
--
-- Before each mutating step of an operation (a deletion walk, a quarantine move, a move to the
-- Trash), the Executor appends an `intent` row: which item, which action, the identity, kind
-- and path of the source and, for moves, the exact destination. The commit is fsynced
-- (`synchronous=FULL`) before the disk is touched. After the step it appends a `done` row that
-- closes the intent with the outcome it is about to audit. On the next start, crash recovery
-- reconciles every intent without a close against the disk and closes it with a `recovered`
-- row. Recovery never resumes a deletion or a move: it only records what the disk shows.
--
-- A close with no outcome means "this attempt changed nothing; another attempt follows" (a
-- quarantine entry name that was already taken).
--
-- Append-only (triggers); one close per intent; a close references an intent of the same
-- operation and item.
--
-- This file runs inside one transaction; `user_version` is set by the migration runner.

CREATE TABLE journal (
    seq                      INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id             TEXT NOT NULL REFERENCES operations (operation_id),
    item_id                  TEXT NOT NULL,
    kind                     TEXT NOT NULL CHECK (kind IN ('intent', 'done', 'recovered')),
    intent_seq               INTEGER REFERENCES journal (seq),
    recorded_at              INTEGER NOT NULL,
    -- intent rows
    action                   TEXT CHECK (action IN ('delete', 'quarantine', 'trash')),
    source_path              TEXT,
    source_identity          TEXT,
    source_kind              TEXT,
    source_parent_identity   TEXT,
    destination              TEXT,
    destination_dir_identity TEXT,
    tree_entries             INTEGER CHECK (tree_entries >= 0),
    tree_bytes               INTEGER CHECK (tree_bytes >= 0),
    -- close rows (`done`, `recovered`): the item outcome, as audited
    outcome                  TEXT CHECK (outcome IN (
        'removed', 'quarantined', 'moved_to_trash', 'tool_run', 'partial', 'failed', 'refused',
        'skipped')),
    reason                   TEXT,
    moved_bytes              INTEGER CHECK (moved_bytes >= 0),
    freed_bytes              INTEGER CHECK (freed_bytes >= 0),
    entries_removed          INTEGER CHECK (entries_removed >= 0),
    entries_kept             INTEGER CHECK (entries_kept >= 0),
    CHECK ((kind = 'intent') = (intent_seq IS NULL)),
    CHECK (kind <> 'intent' OR (action IS NOT NULL AND source_path IS NOT NULL
                                AND source_identity IS NOT NULL AND source_kind IS NOT NULL
                                AND outcome IS NULL AND reason IS NULL)),
    CHECK (kind = 'intent' OR (action IS NULL AND source_path IS NULL
                               AND source_identity IS NULL AND destination IS NULL)),
    CHECK (outcome IS NOT NULL OR (moved_bytes IS NULL AND freed_bytes IS NULL
                                   AND entries_removed IS NULL AND entries_kept IS NULL))
) STRICT;

CREATE INDEX journal_operation ON journal (operation_id, seq);

CREATE UNIQUE INDEX journal_one_close ON journal (intent_seq) WHERE intent_seq IS NOT NULL;

CREATE TRIGGER journal_close_matches_intent BEFORE INSERT ON journal
WHEN NEW.intent_seq IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM journal j
    WHERE j.seq = NEW.intent_seq AND j.kind = 'intent'
      AND j.operation_id = NEW.operation_id AND j.item_id = NEW.item_id)
BEGIN
    SELECT RAISE(ABORT, 'a journal close must reference an intent of the same operation and item');
END;

CREATE TRIGGER journal_no_update BEFORE UPDATE ON journal
BEGIN
    SELECT RAISE(ABORT, 'the journal is append-only');
END;

CREATE TRIGGER journal_no_delete BEFORE DELETE ON journal
BEGIN
    SELECT RAISE(ABORT, 'the journal is append-only');
END;
