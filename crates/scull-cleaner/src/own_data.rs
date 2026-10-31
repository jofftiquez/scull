//! Removal of scull's **own** files for `scull data purge` (P2-CLI-04, FX-DATA-04, SAFE-10).
//!
//! The CLI plans the purge: it lists the footprint files, checks that each lies inside scull's
//! data directory, and captures the data directory's identity and every file's no-follow
//! identity and kind. [`purge_own_files`] then removes each file through the same fd-relative,
//! no-follow walker as the [`Executor`](crate::Executor) (`scull_platform::remove`), anchored
//! at the data directory with its planned identity, so deletion still has a single code path:
//!
//! * only regular files and symlinks are accepted; a symlink is unlinked as a link and its
//!   target is never reached;
//! * every component between the data directory and the file must be a real directory (a link
//!   there is refused, never followed);
//! * the file must still have the planned identity and kind, or it is refused untouched;
//! * an anchor that is no longer the planned data directory, or a walk that sees an anomaly,
//!   **aborts** the purge: the remaining files are not attempted.
//!
//! This never touches quarantine contents, the configuration, or anything outside the data
//! directory, and never deletes database rows (plans, approvals and audit are append-only).

use scull_core::{ErrorKind, FileIdentity, ScullError};
use scull_platform::EntryKind;
use scull_platform::remove::{RemoveError, RemoveOutcome, RemoveRequest, remove_tree};
use std::path::{Component, Path, PathBuf};

/// One planned file: where it is below the data directory and what it must still be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnFileTarget {
    /// Path relative to the data directory (plain names only).
    pub relative: PathBuf,
    /// No-follow identity captured when the purge was planned.
    pub expected: FileIdentity,
    /// No-follow kind captured when the purge was planned: [`EntryKind::File`] or
    /// [`EntryKind::Symlink`].
    pub kind: EntryKind,
}

/// What happened to one planned file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnFileOutcome {
    /// Removed (a symlink as a link).
    Removed,
    /// Already gone.
    AlreadyGone,
    /// Refused before anything was removed (changed, linked path, wrong kind, ...).
    Refused,
    /// The walk ran but could not remove it.
    Failed,
    /// Not attempted: an earlier file aborted the purge.
    NotAttempted,
}

/// One planned file and its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnFileResult {
    /// The planned file.
    pub relative: PathBuf,
    /// What happened.
    pub outcome: OwnFileOutcome,
    /// Why, unless it was removed or already gone.
    pub reason: Option<String>,
}

/// The result of [`purge_own_files`], in planned order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnPurgeReport {
    /// One entry per planned file.
    pub files: Vec<OwnFileResult>,
    /// Why the purge stopped early, if it did.
    pub aborted: Option<String>,
}

/// Remove scull's own files `files` from `data_dir` (which must still have
/// `data_dir_identity`), fd-relatively and without following links. See the module docs.
///
/// # Errors
///
/// [`ErrorKind::UnsupportedPlatform`] (nothing removed) where the OS has no removal walker yet
/// (Windows: P2-EXEC-02, #68).
pub fn purge_own_files(
    data_dir: &Path,
    data_dir_identity: FileIdentity,
    files: &[OwnFileTarget],
) -> Result<OwnPurgeReport, ScullError> {
    let mut report = OwnPurgeReport {
        files: Vec::with_capacity(files.len()),
        aborted: None,
    };
    for target in files {
        let result = |outcome, reason: Option<String>| OwnFileResult {
            relative: target.relative.clone(),
            outcome,
            reason,
        };
        if report.aborted.is_some() {
            report
                .files
                .push(result(OwnFileOutcome::NotAttempted, None));
            continue;
        }
        if !matches!(target.kind, EntryKind::File | EntryKind::Symlink) {
            report.files.push(result(
                OwnFileOutcome::Refused,
                Some(format!("is a {}, not a file", target.kind.as_str())),
            ));
            continue;
        }
        if !plain_relative(&target.relative) {
            report.files.push(result(
                OwnFileOutcome::Refused,
                Some("not a plain path inside the data directory".to_owned()),
            ));
            continue;
        }
        let req = RemoveRequest {
            anchor: data_dir,
            anchor_identity: Some(data_dir_identity),
            relative: &target.relative,
            expected: target.expected,
            expected_kind: Some(target.kind),
        };
        match remove_tree(&req) {
            Ok(r) => match r.outcome {
                RemoveOutcome::Complete => report.files.push(result(OwnFileOutcome::Removed, None)),
                RemoveOutcome::Aborted => {
                    let why = r.aborted.map_or_else(
                        || "the walk saw an anomaly".to_owned(),
                        |a| a.anomaly.to_string(),
                    );
                    report
                        .files
                        .push(result(OwnFileOutcome::Failed, Some(why.clone())));
                    report.aborted = Some(why);
                }
                _ => {
                    let why = r.skipped.first().map_or_else(
                        || "could not be removed".to_owned(),
                        |s| s.reason.as_str().to_owned(),
                    );
                    report.files.push(result(OwnFileOutcome::Failed, Some(why)));
                }
            },
            Err(RemoveError::TargetMissing) => {
                report.files.push(result(OwnFileOutcome::AlreadyGone, None));
            }
            Err(RemoveError::Unsupported) => {
                return Err(ScullError::new(
                    ErrorKind::UnsupportedPlatform,
                    format!("{}; nothing was deleted", RemoveError::Unsupported),
                ));
            }
            Err(
                e @ (RemoveError::AnchorChanged { .. } | RemoveError::AnchorUnavailable { .. }),
            ) => {
                let why = format!("scull's data directory changed: {e}");
                report
                    .files
                    .push(result(OwnFileOutcome::Refused, Some(why.clone())));
                report.aborted = Some(why);
            }
            Err(e) => report
                .files
                .push(result(OwnFileOutcome::Refused, Some(e.to_string()))),
        }
    }
    Ok(report)
}

fn plain_relative(p: &Path) -> bool {
    let mut comps = p.components().peekable();
    comps.peek().is_some() && comps.all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use scull_platform::{Native, Platform};

    fn identity(p: &Path) -> FileIdentity {
        Native.metadata(p).unwrap().identity.unwrap()
    }

    #[test]
    fn rejects_non_plain_paths_and_directories_without_removing() {
        let mut fx = scull_fixtures::Fixture::new().unwrap();
        fx.dir("sub").unwrap().checkpoint().unwrap();
        let root = fx.root().to_path_buf();
        let id = identity(&root.join("sub"));
        let targets = [
            OwnFileTarget {
                relative: PathBuf::from("../canary"),
                expected: id,
                kind: EntryKind::File,
            },
            OwnFileTarget {
                relative: PathBuf::from("sub"),
                expected: id,
                kind: EntryKind::Dir,
            },
        ];
        let r = purge_own_files(&root, identity(&root), &targets).unwrap();
        assert!(
            r.files.iter().all(|f| f.outcome == OwnFileOutcome::Refused),
            "{r:?}"
        );
        assert!(root.join("sub").is_dir());
        fx.check_invariants(&scull_fixtures::InvariantInputs::new())
            .assert_ok();
    }

    #[test]
    fn a_changed_file_is_refused_and_the_rest_still_removed() {
        let mut fx = scull_fixtures::Fixture::new().unwrap();
        fx.file("a", b"a")
            .unwrap()
            .file("b", b"b")
            .unwrap()
            .file("audit/op.ndjson", b"{}")
            .unwrap()
            .checkpoint()
            .unwrap();
        let root = fx.root().to_path_buf();
        let targets = [
            // Planned with b's identity: no longer the object that was planned.
            OwnFileTarget {
                relative: PathBuf::from("a"),
                expected: identity(&root.join("b")),
                kind: EntryKind::File,
            },
            OwnFileTarget {
                relative: PathBuf::from("audit/op.ndjson"),
                expected: identity(&root.join("audit/op.ndjson")),
                kind: EntryKind::File,
            },
            OwnFileTarget {
                relative: PathBuf::from("gone"),
                expected: identity(&root.join("b")),
                kind: EntryKind::File,
            },
        ];
        match purge_own_files(&root, identity(&root), &targets) {
            Ok(r) => {
                let outcomes: Vec<OwnFileOutcome> = r.files.iter().map(|f| f.outcome).collect();
                assert_eq!(
                    outcomes,
                    [
                        OwnFileOutcome::Refused,
                        OwnFileOutcome::Removed,
                        OwnFileOutcome::AlreadyGone
                    ],
                    "{r:?}"
                );
                assert!(root.join("a").is_file());
                assert!(!root.join("audit/op.ndjson").exists());
            }
            // No walker on this OS yet (#68): refused as a whole, nothing removed.
            Err(e) => {
                assert_eq!(e.kind, ErrorKind::UnsupportedPlatform);
                assert!(root.join("audit/op.ndjson").is_file());
            }
        }
        fx.check_invariants(&scull_fixtures::InvariantInputs::new().allow("audit"))
            .assert_ok();
    }

    #[test]
    fn plain_relative_paths() {
        assert!(plain_relative(Path::new("a")));
        assert!(plain_relative(Path::new("audit/op.ndjson")));
        for bad in ["", "..", "a/../b", "/abs", "./a"] {
            assert!(!plain_relative(Path::new(bad)), "{bad}");
        }
    }
}
