//! Emergency headroom (PRO-06, off by default): create and release the reserve file.
//!
//! What the reserve is, where it lives and its size limits are in `scull_proactive::headroom`
//! (read-only). This module is the only code that writes or removes it:
//!
//! * [`create`] writes `<data dir>/headroom/reserve.bin`. The file is created exclusively and
//!   owner-only (never through a link), filled with bytes that don't compress, and fsynced.
//!   Its allocated size is then checked: on a filesystem that keeps it sparse, compressed or
//!   deduplicated, the file is removed again and creation refuses. Last, the manifest
//!   (`headroom.json`) records its identity, size and allocated bytes. Creation refuses when
//!   free space is already below, or would drop below, the forecast reserve, and when the size
//!   is outside the limits.
//! * [`release`] removes the reserve only when the manifest describes it and its no-follow
//!   identity, kind and size still match. Removal goes through
//!   [`crate::own_data::purge_own_files`]: fd-relative, no-follow, anchored at the data
//!   directory with its identity, the same walker as every other deletion (SAFE-10). Anything
//!   else at the path is refused and left alone. The manifest is removed last, the same way.
//! * [`audit`] appends the outcome to the history database (an `headroom` event). The CLI calls
//!   it right **after** a release: on a full disk the database may need the space the release
//!   frees.
//!
//! Under the test harness (`SCULL_TEST_SANDBOX`), a data directory outside the sandbox is
//! refused, so tests can never create or release a reserve on the real machine.

use crate::own_data::{OwnFileOutcome, OwnFileTarget, purge_own_files};
use schemars::JsonSchema;
use scull_core::config::TEST_SANDBOX_ENV;
use scull_core::{ErrorKind, FileIdentity, FsPath, ScullError};
use scull_platform::{EntryKind, Native, Platform};
use scull_proactive::headroom::{
    DEFAULT_SIZE, HEADROOM_DIR, HeadroomManifest, MANIFEST_FILE, MANIFEST_VERSION, MAX_SIZE,
    MIN_SIZE, mismatch, read_manifest, really_allocated, reserve_relative, size_refusal_within,
};
use scull_storage::{Event, EventKind, Store, StoreError};
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write chunk size.
const CHUNK: usize = 1024 * 1024;

/// Size limits (the defaults are `scull_proactive::headroom`'s; tests use smaller ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Smallest reserve.
    pub min: u64,
    /// Largest reserve.
    pub max: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min: MIN_SIZE,
            max: MAX_SIZE,
        }
    }
}

/// What to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateOptions {
    /// Reserve size in bytes (default [`DEFAULT_SIZE`]).
    pub size_bytes: u64,
    /// Free space that must remain after creating it (the forecast reserve,
    /// `[proactive] reserve_gib`).
    pub keep_free_bytes: u64,
    /// Now (Unix seconds), recorded in the manifest.
    pub now: i64,
}

impl CreateOptions {
    /// The default size, keeping `keep_free_bytes` free.
    pub fn new(keep_free_bytes: u64, now: i64) -> Self {
        Self {
            size_bytes: DEFAULT_SIZE,
            keep_free_bytes,
            now,
        }
    }
}

/// How a release ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseOutcome {
    /// The reserve was removed; its space is free.
    Released,
    /// The file was already gone; the manifest was removed.
    AlreadyGone,
}

/// The result of a [`release`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Released {
    /// How it ended.
    pub outcome: ReleaseOutcome,
    /// Bytes freed (the reserve's allocated size; 0 when it was already gone).
    pub freed_bytes: u64,
    /// The manifest of the reserve that was released.
    pub manifest: HeadroomManifest,
}

/// An audit record of the headroom file (payload of an `headroom` event).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadroomEvent {
    /// `created`, `released`, `already_gone` or `refused`.
    pub action: String,
    /// The reserve's path.
    pub path: String,
    /// The manifest involved, if any.
    pub manifest: Option<HeadroomManifest>,
    /// Bytes freed (release) or reserved (creation).
    pub bytes: u64,
    /// Why it was refused.
    pub reason: Option<String>,
}

fn blocked(msg: impl Into<String>) -> ScullError {
    ScullError::new(ErrorKind::SafetyBlocked, msg)
}

/// Under the test harness, refuse a data directory outside the sandbox.
fn sandbox_guard(data_dir: &Path) -> Result<(), ScullError> {
    sandbox_guard_for(data_dir, std::env::var_os(TEST_SANDBOX_ENV).as_deref())
}

/// [`sandbox_guard`] with the sandbox given (`None` or empty: not under the test harness).
fn sandbox_guard_for(data_dir: &Path, sandbox: Option<&std::ffi::OsStr>) -> Result<(), ScullError> {
    let Some(sandbox) = sandbox.filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let sandbox = PathBuf::from(sandbox);
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if canon(data_dir).starts_with(canon(&sandbox)) {
        Ok(())
    } else {
        Err(blocked(format!(
            "{} is outside the test sandbox {} ({TEST_SANDBOX_ENV}): the headroom file is \
             never created or released there",
            data_dir.display(),
            sandbox.display()
        )))
    }
}

/// The data directory's no-follow identity; it must be a real directory.
fn data_dir_identity(native: &Native, data_dir: &Path) -> Result<FileIdentity, ScullError> {
    let md = native.metadata(data_dir).map_err(|e| {
        ScullError::new(ErrorKind::Io, e.to_string()).with_path(FsPath::from(data_dir))
    })?;
    if md.kind != EntryKind::Dir {
        return Err(blocked(format!(
            "scull's data folder {} is a {}, not a real folder",
            data_dir.display(),
            md.kind.as_str()
        )));
    }
    md.identity
        .ok_or_else(|| blocked("this OS reports no identity for scull's data folder"))
}

/// Create the reserve in `data_dir` with the default [`Limits`]. See the module docs.
///
/// # Errors
///
/// [`ErrorKind::SafetyBlocked`] when a reserve or anything at its path already exists, the size
/// is outside the limits, free space would drop below `keep_free_bytes`, the filesystem
/// doesn't allocate it for real, or (tests) the data directory is outside the sandbox; I/O
/// errors otherwise. A partly written file is removed before returning.
pub fn create(data_dir: &Path, opts: &CreateOptions) -> Result<HeadroomManifest, ScullError> {
    create_with(data_dir, opts, Limits::default())
}

/// [`create`] with explicit size limits.
pub fn create_with(
    data_dir: &Path,
    opts: &CreateOptions,
    limits: Limits,
) -> Result<HeadroomManifest, ScullError> {
    sandbox_guard(data_dir)?;
    let native = Native::new();
    let data_identity = data_dir_identity(&native, data_dir)?;
    if read_manifest(data_dir).map_err(blocked)?.is_some() {
        return Err(blocked(
            "a headroom file already exists; release it first (`scull headroom release`)",
        ));
    }
    let path = data_dir.join(reserve_relative());
    if native.metadata(&path).is_ok() {
        return Err(blocked(format!(
            "{} exists but scull has no record of it; it is left alone",
            path.display()
        )));
    }
    let volume = scull_platform::volume_of(data_dir)
        .map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))?;
    let free = scull_platform::free_space(data_dir)
        .ok()
        .or(volume.free_bytes)
        .ok_or_else(|| blocked("free space on scull's data volume is unknown"))?;
    let total = volume
        .total_bytes
        .ok_or_else(|| blocked("the size of scull's data volume is unknown"))?;
    if let Some(why) = size_refusal_within(
        opts.size_bytes,
        total,
        free,
        opts.keep_free_bytes,
        limits.min,
        limits.max,
    ) {
        return Err(blocked(why));
    }

    // The folder: created owner-only, or an existing real folder.
    let dir = data_dir.join(HEADROOM_DIR);
    match native.metadata(&dir) {
        Ok(md) if md.kind == EntryKind::Dir => {}
        Ok(md) => {
            return Err(blocked(format!(
                "{} is a {}, not a folder",
                dir.display(),
                md.kind.as_str()
            )));
        }
        Err(_) => scull_platform::create_dir_owner_only(&dir)
            .map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))?,
    }

    let file = scull_platform::create_file_owner_only(&path)
        .map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))?;
    let written = fill(file, opts.size_bytes);
    let md = native.metadata(&path).ok();
    let identity = md.as_ref().and_then(|m| m.identity);
    let discard = |why: String| -> ScullError {
        if let Some(id) = identity {
            let _ = purge_own_files(
                data_dir,
                data_identity,
                &[OwnFileTarget {
                    relative: reserve_relative(),
                    expected: id,
                    kind: EntryKind::File,
                }],
            );
        }
        blocked(why)
    };
    if let Err(e) = written {
        return Err(discard(format!(
            "writing the reserve failed ({e}); the partial file was removed"
        )));
    }
    let (Some(md), Some(identity)) = (md, identity) else {
        return Err(discard("the reserve's identity can't be read".into()));
    };
    let allocated = md.allocated_size.unwrap_or(0);
    if !really_allocated(opts.size_bytes, allocated) {
        return Err(discard(format!(
            "this filesystem keeps the file at {allocated} allocated bytes of {} (sparse, \
             compressed or deduplicated), so it would reserve nothing; it was removed",
            opts.size_bytes
        )));
    }
    let manifest = HeadroomManifest {
        version: MANIFEST_VERSION,
        relative: reserve_relative().to_string_lossy().into_owned(),
        identity,
        size_bytes: opts.size_bytes,
        allocated_bytes: allocated,
        volume_id: volume.mount_point.to_string_lossy().into_owned(),
        created_at: opts.now,
    };
    if let Err(e) = write_manifest(data_dir, &manifest) {
        return Err(discard(format!(
            "recording the reserve failed ({e}); the file was removed"
        )));
    }
    Ok(manifest)
}

/// Fill `file` with `size` bytes that don't compress or deduplicate, then fsync.
fn fill(mut file: std::fs::File, size: u64) -> std::io::Result<()> {
    let mut buf = vec![0u8; CHUNK];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut left = size;
    while left > 0 {
        for word in buf.chunks_exact_mut(8) {
            // xorshift64*: a different pseudo-random block every chunk.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            word.copy_from_slice(&state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
        }
        let n = usize::try_from(left.min(CHUNK as u64)).unwrap_or(CHUNK);
        file.write_all(&buf[..n])?;
        left -= n as u64;
    }
    file.sync_all()
}

/// Write the manifest owner-only and exclusively (a temp name, then a rename).
fn write_manifest(data_dir: &Path, m: &HeadroomManifest) -> std::io::Result<()> {
    let tmp = data_dir.join(format!("{MANIFEST_FILE}.tmp"));
    let _ = std::fs::remove_file(&tmp).or_else(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        }
    });
    let mut f = scull_platform::create_file_owner_only(&tmp).map_err(std::io::Error::other)?;
    let text = serde_json::to_string_pretty(m).map_err(std::io::Error::other)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, data_dir.join(MANIFEST_FILE))
}

/// Release the reserve in `data_dir`. See the module docs.
///
/// # Errors
///
/// [`ErrorKind::NotFound`] when there is no reserve; [`ErrorKind::SafetyBlocked`] when the
/// manifest can't be read, the file at the path is not the recorded one (nothing is removed),
/// the walk refuses, or (tests) the data directory is outside the sandbox.
pub fn release(data_dir: &Path) -> Result<Released, ScullError> {
    sandbox_guard(data_dir)?;
    let native = Native::new();
    let data_identity = data_dir_identity(&native, data_dir)?;
    let manifest = read_manifest(data_dir)
        .map_err(|e| {
            blocked(format!(
                "the headroom record can't be read ({e}); nothing released"
            ))
        })?
        .ok_or_else(|| ScullError::new(ErrorKind::NotFound, "there is no headroom file"))?;
    let path = data_dir.join(reserve_relative());
    let (outcome, freed) = match native.metadata(&path) {
        Err(e) if e.is_not_found() => (ReleaseOutcome::AlreadyGone, 0),
        Err(e) => {
            return Err(blocked(format!(
                "{}: {e}; nothing released",
                path.display()
            )));
        }
        Ok(md) => {
            if let Some(why) = mismatch(&manifest, md.kind, md.identity, md.logical_size) {
                return Err(blocked(format!(
                    "{}: {why}; nothing released",
                    path.display()
                )));
            }
            let freed = md.allocated_size.unwrap_or(manifest.allocated_bytes);
            let report = purge_own_files(
                data_dir,
                data_identity,
                &[OwnFileTarget {
                    relative: reserve_relative(),
                    expected: manifest.identity,
                    kind: EntryKind::File,
                }],
            )?;
            match report.files.first().map(|r| (r.outcome, r.reason.clone())) {
                Some((OwnFileOutcome::Removed, _)) => (ReleaseOutcome::Released, freed),
                Some((OwnFileOutcome::AlreadyGone, _)) => (ReleaseOutcome::AlreadyGone, 0),
                Some((_, reason)) => {
                    return Err(blocked(format!(
                        "the reserve was not removed: {}",
                        reason
                            .or(report.aborted)
                            .unwrap_or_else(|| "refused".into())
                    )));
                }
                None => return Err(blocked("the reserve was not removed")),
            }
        }
    };
    // The manifest last, through the same walker (its identity checked as seen now).
    let mpath = data_dir.join(MANIFEST_FILE);
    if let Ok(md) = native.metadata(&mpath)
        && md.kind == EntryKind::File
        && let Some(id) = md.identity
    {
        let _ = purge_own_files(
            data_dir,
            data_identity,
            &[OwnFileTarget {
                relative: PathBuf::from(MANIFEST_FILE),
                expected: id,
                kind: EntryKind::File,
            }],
        )?;
    }
    Ok(Released {
        outcome,
        freed_bytes: freed,
        manifest,
    })
}

/// Append `event` to the history database's audit trail (`headroom` events), recorded by
/// `source` (`cli`, `desktop`) at `now`.
pub fn audit(
    store: &mut Store,
    event: &HeadroomEvent,
    source: &str,
    now: i64,
) -> Result<(), StoreError> {
    store.record_events(&[Event {
        event_id: 0,
        at: now,
        kind: EventKind::Headroom,
        source: source.to_owned(),
        payload: serde_json::to_value(event).unwrap_or_default(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_sandbox_confines_the_reserve() {
        let sandbox = tempfile::tempdir().unwrap();
        let inside = sandbox.path().join("data");
        std::fs::create_dir(&inside).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let s = Some(sandbox.path().as_os_str());
        assert!(sandbox_guard_for(&inside, s).is_ok());
        let e = sandbox_guard_for(outside.path(), s).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked);
        assert!(
            e.message.contains("outside the test sandbox"),
            "{}",
            e.message
        );
        // Not under the harness: no confinement.
        assert!(sandbox_guard_for(outside.path(), None).is_ok());
        assert!(sandbox_guard_for(outside.path(), Some(std::ffi::OsStr::new(""))).is_ok());
    }

    #[test]
    fn fill_writes_incompressible_distinct_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        fill(std::fs::File::create(&p).unwrap(), 3 * CHUNK as u64 + 5).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(bytes.len(), 3 * CHUNK + 5);
        assert_ne!(
            bytes[..CHUNK],
            bytes[CHUNK..2 * CHUNK],
            "chunks differ (no dedup)"
        );
        let zeros = bytes.iter().filter(|b| **b == 0).count();
        assert!(zeros < bytes.len() / 100, "not zero-filled");
    }
}
