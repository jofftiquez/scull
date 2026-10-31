-- scull local store, schema version 2: history recording (INS-01, INS-06, DATA-05).
--
-- Adds what the history queries need to stay cheap as scans accumulate:
--   * `scan_roots`: one row per (root, scan) so "latest scan of this root" is an index walk,
--     not a JSON scan of every `scans` row.
--   * `items.disappeared_at` / `items.disappeared_scan`: item lifecycle. Set only by a
--     *completed* scan of a root that contains the item and did not see it.
--   * `item_events`: per-candidate lifecycle and size/tier changes. Only changes are recorded
--     (first seen, significant size change, tier change, disappeared, reappeared), so it grows
--     with churn, not with the number of scans. Candidates only: never one row per file.
--   * `regrowth_events`: an artifact reappearing at a path where one disappeared (INS-03
--     regrowth, INS-06 "rebuild events").
--   * `volumes`: latest free-space sample per volume, the cache read by `scull status --short`.
--   * `footprint_samples`: scull's own disk use (DATA-05).
--
-- None of these tables is append-only: retention and downsampling (DATA-06, INS-02) may thin
-- them. Plans, approvals, operations and audit keep their append-only triggers from v1.
--
-- This file runs inside one transaction; `user_version` is set by the migration runner.

-- scan roots ------------------------------------------------------------------------------

CREATE TABLE scan_roots (
    root       TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    scan_id    TEXT NOT NULL REFERENCES scans (scan_id),
    PRIMARY KEY (root, started_at, scan_id)
) STRICT, WITHOUT ROWID;

INSERT INTO scan_roots (root, started_at, scan_id)
SELECT DISTINCT j.value, s.started_at, s.scan_id
FROM scans s, json_each(s.roots_json) j
WHERE j.type = 'text';

-- History queries select scans by finish time.
CREATE INDEX scans_finished ON scans (finished_at);
-- "Latest scan" walks start times backwards; the ID breaks ties without a sort.
DROP INDEX scans_started;
CREATE INDEX scans_started ON scans (started_at, scan_id);

-- item lifecycle --------------------------------------------------------------------------

ALTER TABLE items ADD COLUMN disappeared_at INTEGER;
ALTER TABLE items ADD COLUMN disappeared_scan TEXT REFERENCES scans (scan_id);

-- Live items (not marked gone), looked up by path for disappearance and regrowth.
CREATE INDEX items_live_path ON items (path) WHERE disappeared_at IS NULL;
-- Items marked gone, looked up by path when an artifact reappears there.
CREATE INDEX items_gone_path ON items (path, disappeared_at) WHERE disappeared_at IS NOT NULL;

CREATE TABLE item_events (
    event_id          INTEGER PRIMARY KEY,
    item_id           TEXT NOT NULL REFERENCES items (item_id),
    occurred_at       INTEGER NOT NULL,
    scan_id           TEXT NOT NULL REFERENCES scans (scan_id),
    kind              TEXT NOT NULL
        CHECK (kind IN ('first_seen', 'changed', 'disappeared', 'reappeared')),
    tier              TEXT NOT NULL CHECK (tier IN ('S0', 'S1', 'S2', 'S3')),
    logical_bytes     INTEGER NOT NULL CHECK (logical_bytes >= 0),
    allocated_bytes   INTEGER NOT NULL CHECK (allocated_bytes >= 0),
    reclaimable_bytes INTEGER NOT NULL CHECK (reclaimable_bytes >= 0)
) STRICT;

CREATE INDEX item_events_item ON item_events (item_id, event_id);

-- Existing items get a baseline event (their current sizes, at first sighting).
INSERT INTO item_events (item_id, occurred_at, scan_id, kind, tier, logical_bytes,
                         allocated_bytes, reclaimable_bytes)
SELECT item_id, first_seen_at, first_seen_scan, 'first_seen', tier, logical_bytes,
       allocated_bytes, reclaimable_bytes
FROM items
ORDER BY first_seen_at, item_id;

CREATE TABLE regrowth_events (
    event_id         INTEGER PRIMARY KEY,
    item_id          TEXT NOT NULL REFERENCES items (item_id),
    previous_item_id TEXT NOT NULL REFERENCES items (item_id),
    path             TEXT NOT NULL,
    category         TEXT NOT NULL,
    artifact         TEXT NOT NULL,
    -- Last sighting of the previous item; with `disappeared_at` it bounds when it went away.
    previous_last_seen_at INTEGER NOT NULL,
    disappeared_at   INTEGER NOT NULL,
    reappeared_at    INTEGER NOT NULL,
    scan_id          TEXT NOT NULL REFERENCES scans (scan_id)
) STRICT;

CREATE INDEX regrowth_events_category ON regrowth_events (category, reappeared_at);

-- free space ------------------------------------------------------------------------------

-- Latest sample per volume (kept in step with `free_space_samples` by the store).
CREATE TABLE volumes (
    volume      TEXT PRIMARY KEY NOT NULL,
    sampled_at  INTEGER NOT NULL,
    scan_id     TEXT REFERENCES scans (scan_id),
    total_bytes INTEGER NOT NULL CHECK (total_bytes >= 0),
    free_bytes  INTEGER NOT NULL CHECK (free_bytes >= 0)
) STRICT;

INSERT INTO volumes (volume, sampled_at, scan_id, total_bytes, free_bytes)
SELECT volume, max(sampled_at), scan_id, total_bytes, free_bytes
FROM free_space_samples
GROUP BY volume;

-- scull's own footprint (DATA-05) ---------------------------------------------------------

CREATE TABLE footprint_samples (
    sample_id      INTEGER PRIMARY KEY,
    sampled_at     INTEGER NOT NULL,
    scan_id        TEXT REFERENCES scans (scan_id),
    database_bytes INTEGER NOT NULL CHECK (database_bytes >= 0),
    wal_bytes      INTEGER NOT NULL CHECK (wal_bytes >= 0),
    backup_bytes   INTEGER NOT NULL CHECK (backup_bytes >= 0),
    damaged_bytes  INTEGER NOT NULL CHECK (damaged_bytes >= 0)
) STRICT;

CREATE INDEX footprint_samples_at ON footprint_samples (sampled_at);
