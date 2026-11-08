//! Exact linked-worktree removal, reached only after plan revalidation and a durable intent.

use crate::executor::{DeleteTarget, check_target};
use crate::quickclean::process::{self, Ended, Spec};
use crate::revalidate::Revalidated;
use scull_core::{Action, Category, ErrorKind, ScullError, Tier};
use scull_engine::CancelToken;
use scull_git::{AnalyzeOptions, GitRunner, Observed, RunnerOptions, WorktreeKind};
use scull_platform::{EntryKind, Native, Platform};
use scull_safety::{NativeActivityProbe, Tri, check_in_use};
use std::path::Path;
use std::time::Duration;

pub(crate) struct Removed {
    pub bytes: u64,
    pub succeeded: bool,
    pub reason: Option<String>,
}

fn blocked(message: impl Into<String>) -> ScullError {
    ScullError::new(ErrorKind::SafetyBlocked, message)
}

fn git_error(error: impl std::fmt::Display) -> ScullError {
    blocked(format!("worktree removal refused: {error}"))
}

/// Recheck the complete tree without following links or crossing a volume. Git unlinks leaf
/// symlinks itself; directory junctions and special/unknown entries are refused.
fn tree_bytes(path: &Path, volume: u64, cancel: &CancelToken) -> Result<u64, ScullError> {
    let mut todo = vec![path.to_path_buf()];
    let mut bytes = 0u64;
    while let Some(path) = todo.pop() {
        if cancel.is_cancelled() {
            return Err(ScullError::new(
                ErrorKind::Cancelled,
                "worktree removal cancelled",
            ));
        }
        let md = Native.metadata(&path).map_err(git_error)?;
        if md.identity.is_none_or(|id| id.volume != volume)
            || md.kind.is_special()
            || (md.kind.is_link() && md.kind != EntryKind::Symlink)
            || scull_platform::CloudState::from_metadata(&md).is_some_and(|s| s.placeholder)
        {
            return Err(blocked(
                "worktree contains an unverified entry, junction or another volume",
            ));
        }
        // Hard links may retain their blocks outside this worktree; never count them freed.
        if md.nlink.is_some_and(|n| n <= 1) || md.kind == EntryKind::Dir {
            bytes = bytes.saturating_add(md.allocated_size.unwrap_or(0));
        }
        if md.kind == EntryKind::Dir {
            for entry in Native.read_dir(&path).map_err(git_error)? {
                todo.push(entry.map_err(git_error)?.path);
            }
        }
    }
    Ok(bytes)
}

fn check_path(ok: &Revalidated) -> Result<(), ScullError> {
    let relative = check_target(
        &ok.item,
        &DeleteTarget {
            anchor: &ok.anchor,
            anchor_identity: ok.anchor_identity,
            path: &ok.path,
        },
    )?;
    let anchor = Native.metadata(&ok.anchor).map_err(git_error)?;
    if anchor.kind != EntryKind::Dir
        || anchor.identity != ok.anchor_identity
        || anchor
            .identity
            .is_none_or(|id| id.volume != ok.item.identity.volume)
    {
        return Err(blocked(
            "the worktree's scan root changed or its volume is unverified",
        ));
    }
    let mut current = ok.anchor.clone();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(blocked("worktree path contains a non-normal component"));
        }
        current.push(component);
        let md = Native.metadata(&current).map_err(git_error)?;
        if md.kind != EntryKind::Dir
            || md
                .identity
                .is_none_or(|id| id.volume != ok.item.identity.volume)
        {
            return Err(blocked(
                "a folder above the worktree changed, is a link or crosses volumes",
            ));
        }
    }
    if Native.metadata(&ok.path).map_err(git_error)?.identity != Some(ok.item.identity)
        || Native
            .metadata(&ok.path.join(".git"))
            .map_err(git_error)?
            .kind
            != EntryKind::File
    {
        return Err(blocked(
            "the linked worktree's identity or .git file changed",
        ));
    }
    Ok(())
}

fn check_git_metadata(reviewed: &crate::revalidate::WorktreeBasis) -> Result<(), ScullError> {
    for (dir, expected) in [
        (
            reviewed.state.repo.common_dir.to_path_buf(),
            reviewed.common_identity,
        ),
        (reviewed.state.git_dir.to_path_buf(), reviewed.git_identity),
    ] {
        for ancestor in dir.ancestors() {
            if Native.metadata(ancestor).map_err(git_error)?.kind != EntryKind::Dir {
                return Err(blocked(
                    "a Git metadata ancestor is a link or no longer a directory",
                ));
            }
        }
        let metadata = Native.metadata(&dir).map_err(git_error)?;
        if metadata.kind != EntryKind::Dir || expected.is_none() || metadata.identity != expected {
            return Err(blocked(
                "the repository or linked-worktree metadata was replaced or is a link",
            ));
        }
        for name in [
            "index.lock",
            "HEAD.lock",
            "config.lock",
            "packed-refs.lock",
            "shallow.lock",
        ] {
            match Native.metadata(&dir.join(name)) {
                Err(e) if e.is_not_found() => {}
                _ => {
                    return Err(blocked(format!(
                        "Git metadata is locked or unreadable: {}",
                        dir.join(name).display()
                    )));
                }
            }
        }
    }
    Ok(())
}

fn runner_options(roots: &[std::path::PathBuf], path: &Path) -> RunnerOptions {
    let mut excluded_roots = roots.to_vec();
    excluded_roots.push(path.to_path_buf());
    RunnerOptions {
        excluded_roots,
        ..RunnerOptions::default()
    }
}

pub(crate) fn remove(
    ok: &Revalidated,
    cancel: &CancelToken,
    hook: Option<&crate::journal::StepHook>,
) -> Result<Removed, ScullError> {
    if scull_platform::is_elevated().blocks_destructive() {
        return Err(blocked(
            "worktree removal is refused while elevated or elevation is unknown",
        ));
    }
    if ok.item.tier != Tier::S1
        || ok.item.category != Category::Worktree
        || ok.item.rule.id != scull_git::WORKTREE_RULE_ID
        || ok.item.action != Action::GitWorktreeRemove
    {
        return Err(blocked(
            "only a revalidated S1 linked worktree can be removed",
        ));
    }
    let reviewed = ok
        .worktree
        .as_ref()
        .ok_or_else(|| blocked("fresh worktree analysis is missing"))?;
    check_path(ok)?;
    let repo = scull_git::discover(&ok.path).map_err(git_error)?;
    if repo.common_dir != reviewed.state.repo.common_dir || !repo.problems.is_empty() {
        return Err(blocked(
            "the worktree's repository registration changed or is inconsistent",
        ));
    }
    let worktree = repo
        .worktrees
        .iter()
        .find(|w| {
            w.path == ok.item.path
                && w.kind == WorktreeKind::Linked
                && !w.lock.is_locked()
                && w.prunable.is_none()
        })
        .ok_or_else(|| {
            blocked("the exact linked worktree is missing, locked or no longer registered")
        })?;
    if worktree.git_dir != reviewed.state.git_dir {
        return Err(blocked("the linked worktree's registration changed"));
    }
    check_git_metadata(reviewed)?;
    let runner =
        GitRunner::with_options(runner_options(&ok.scan_roots, &ok.path)).map_err(git_error)?;
    let bytes = tree_bytes(&ok.path, ok.item.identity.volume, cancel)?;
    let probe = NativeActivityProbe::capture(scull_platform::activity::Limits::default());
    if check_in_use(&ok.path, &[], &probe).in_use != Tri::No {
        return Err(blocked(
            "the worktree is in use or its activity could not be verified",
        ));
    }
    check_path(ok)?;
    let state = scull_git::analyze_worktree(
        &runner,
        &repo,
        worktree,
        Observed::unknown("activity is checked by the execution probe"),
        &AnalyzeOptions::default(),
    );
    let artifacts = |_: &Path, path: &str| {
        reviewed
            .decision
            .artifacts
            .iter()
            .find(|artifact| artifact.path == path)
            .map(|artifact| artifact.rule.clone())
    };
    let decision = scull_git::classify_worktree(&state, scull_git::Tri::No, &artifacts);
    if decision.tier != Tier::S1
        || decision.action != Action::GitWorktreeRemove
        || state.head != reviewed.state.head
    {
        return Err(blocked(format!(
            "the worktree changed since revalidation: {}",
            decision
                .reasons
                .iter()
                .map(|r| r.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }
    if state
        .submodules
        .value()
        .is_none_or(|subs| subs.iter().any(|s| s.initialized))
    {
        return Err(blocked(
            "Git requires force to remove initialized submodules; scull never forces removal",
        ));
    }
    let command = runner
        .prepare_worktree_removal(&ok.path)
        .map_err(git_error)?;
    let args: Vec<_> = command
        .get_args()
        .map(std::ffi::OsStr::to_os_string)
        .collect();
    let env: Vec<_> = command
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_string_lossy().into_owned(), v.to_os_string())))
        .collect();
    let cwd = command
        .get_current_dir()
        .ok_or_else(|| blocked("Git sandbox is missing"))?;
    crate::journal::at(
        hook,
        crate::journal::StepPoint::BeforeWorktreeRemove(&ok.item.item_id),
    );
    check_path(ok)?;
    check_git_metadata(reviewed)?;
    if runner
        .index_has_hidden_entries(&ok.path)
        .map_err(git_error)?
    {
        return Err(blocked(
            "the Git index has assume-unchanged or skip-worktree entries; local changes may be hidden",
        ));
    }
    let status = runner
        .status(
            &ok.path,
            &scull_git::StatusOptions {
                untracked: scull_git::UntrackedMode::All,
                ignored: scull_git::IgnoredMode::Matching,
            },
        )
        .map_err(git_error)?;
    let same_head = reviewed.state.head.value().is_some_and(|head| {
        status.branch.oid.as_ref().map(|oid| oid.as_str()) == head.oid()
            && status.branch.head.as_deref() == head.branch()
    });
    let ignored_are_artifacts = status.ignored().all(|entry| {
        std::str::from_utf8(entry.path.as_bytes()).is_ok_and(|path| {
            reviewed
                .decision
                .artifacts
                .iter()
                .any(|artifact| artifact.path == path)
        })
    });
    if !status.is_clean() || !same_head || !ignored_are_artifacts {
        return Err(blocked(
            "the worktree acquired changes, untracked/unique ignored files or a different HEAD; nothing was removed",
        ));
    }
    let fresh_command = runner
        .prepare_worktree_removal(&ok.path)
        .map_err(git_error)?;
    if !fresh_command.get_args().eq(command.get_args())
        || !fresh_command.get_envs().eq(command.get_envs())
        || fresh_command.get_program() != command.get_program()
        || fresh_command.get_current_dir() != command.get_current_dir()
    {
        return Err(blocked(
            "Git command hardening changed during final checks; refresh before removing the worktree",
        ));
    }
    // Git's own clean check omits ignored files; bind our explicit final inventory to the
    // same directory and registration after the final read-only subprocesses have exited.
    check_path(ok)?;
    check_git_metadata(reviewed)?;
    let final_repo = scull_git::discover(&ok.path).map_err(git_error)?;
    if final_repo.common_dir != reviewed.state.repo.common_dir
        || !final_repo.problems.is_empty()
        || !final_repo.worktrees.iter().any(|w| w == worktree)
    {
        return Err(blocked(
            "the exact worktree registration changed during final checks",
        ));
    }
    let ran = process::run(&Spec {
        binary: Path::new(command.get_program()),
        args: &args,
        env: &env,
        cwd,
        timeout: Duration::from_secs(60),
        max_output: 64 * 1024,
        cancel,
    });
    let absent = Native.metadata(&ok.path).is_err_and(|e| e.is_not_found());
    let unregistered = scull_git::list_worktrees(&repo.common_dir.to_path_buf())
        .is_ok_and(|r| !r.worktrees.iter().any(|w| w.path == ok.item.path));
    let succeeded = ran.ended == Ended::Exited(Some(0)) && absent && unregistered;
    Ok(Removed {
        bytes: if succeeded { bytes } else { 0 },
        succeeded,
        reason: (!succeeded).then(|| {
            format!(
                "git worktree remove did not complete; partial changes may have occurred ({:?}): {}",
                ran.ended,
                crate::quickclean::escape_line(&String::from_utf8_lossy(&ran.stderr))
            )
        }),
    })
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn git_resolution_excludes_every_scan_root() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first-root");
        let second = temp.path().join("second-root");
        let tools = second.join("tools");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let fake = tools.join("git");
        let marker = temp.path().join("untrusted-git-ran");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\ntouch '{}'\nprintf 'git version 2.50.0\\n'\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![tools];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut options = runner_options(&[first.clone(), second.clone()], &first.join("worktree"));
        options.path_env = Some(std::env::join_paths(paths).unwrap());
        assert!(options.excluded_roots.contains(&first));
        assert!(options.excluded_roots.contains(&second));
        let runner = GitRunner::with_options(options).unwrap();
        assert!(!runner.resolved_binary().starts_with(&second));
        assert!(
            !marker.exists(),
            "the untrusted second-root Git must never start"
        );
    }
}
