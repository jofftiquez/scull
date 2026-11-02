//! The write-ahead journal on the Executor's side (SAFE-19, T-15, P2-EXEC-08).
//!
//! Within an [`Operation`], every mutating step is announced before the disk is touched and
//! closed after it, in the database's append-only journal
//! ([`scull_storage::journal`]; every commit is fsynced, `synchronous=FULL`):
//!
//! 1. **intent**: item, action, the source's path, identity and kind, its parent directory's
//!    identity, and for a quarantine move the exact destination; for a removal walk, the
//!    tree's entry count and logical bytes ([`TreeFingerprint`]);
//! 2. the step (removal walk, quarantine move + manifest entry);
//! 3. **close**: the outcome that is about to be audited;
//! 4. the audit row (SAFE-08).
//!
//! A crash anywhere leaves either no intent (nothing was touched), an open intent (the step
//! may or may not have happened: [`Executor::recover`](crate::Executor::recover) reconciles it
//! with the disk), or a closed intent without an audit row (recovery replays the close).
//!
//! [`StepPoint`] / [`StepHook`]: a hook the Executor calls at each of these points, so tests can
//! kill the process at a chosen point (the SAFE-19 kill tests run the Executor in a child
//! process and SIGKILL it there). Production code installs no hook.

use crate::operation::Operation;
use scull_core::{ErrorKind, ItemId, ScullError};
use scull_platform::{EntryKind, Native, Platform};
use scull_storage::{ItemRecord, JournalClose, JournalCloseKind, JournalIntent, TreeFingerprint};
use std::path::Path;
use std::sync::Arc;

/// A point in the execution of one item at which the [`StepHook`] is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StepPoint<'a> {
    /// Revalidation passed; the intent is about to be journaled.
    BeforeIntent(&'a ItemId),
    /// The intent is durable; the disk has not been touched for this step yet.
    AfterIntent(&'a ItemId),
    /// During a removal walk, right after the walker's `n`-th event (1-based): the walker has
    /// examined an entry it is about to remove, or opened or listed a directory.
    Walk(&'a ItemId, u64),
    /// A quarantine move is done; its manifest entry is not recorded yet.
    AfterMove(&'a ItemId),
    /// The quarantine manifest entry is recorded; the step is not closed yet.
    AfterManifest(&'a ItemId),
    /// The step is closed in the journal; the audit row is not appended yet.
    AfterClose(&'a ItemId),
    /// The item's audit row is appended.
    AfterAudit(&'a ItemId),
}

impl StepPoint<'_> {
    /// Stable name of the point (`after_intent`, `walk`, ...), for test harnesses.
    pub fn name(&self) -> &'static str {
        match self {
            StepPoint::BeforeIntent(_) => "before_intent",
            StepPoint::AfterIntent(_) => "after_intent",
            StepPoint::Walk(..) => "walk",
            StepPoint::AfterMove(_) => "after_move",
            StepPoint::AfterManifest(_) => "after_manifest",
            StepPoint::AfterClose(_) => "after_close",
            StepPoint::AfterAudit(_) => "after_audit",
        }
    }

    /// The item.
    pub fn item(&self) -> &ItemId {
        match self {
            StepPoint::BeforeIntent(i)
            | StepPoint::AfterIntent(i)
            | StepPoint::Walk(i, _)
            | StepPoint::AfterMove(i)
            | StepPoint::AfterManifest(i)
            | StepPoint::AfterClose(i)
            | StepPoint::AfterAudit(i) => i,
        }
    }
}

/// Called at every [`StepPoint`] (fault injection for crash-recovery tests).
pub type StepHook = Arc<dyn Fn(&StepPoint<'_>) + Send + Sync>;

/// Calls `hook` at `point`, if there is a hook.
pub(crate) fn at(hook: Option<&StepHook>, point: StepPoint<'_>) {
    if let Some(h) = hook {
        h(&point);
    }
}

/// Entries, logical and allocated bytes of the tree at `path`, never following links.
pub(crate) fn tree_stats(path: &Path) -> (u64, u64, u64) {
    let (mut entries, mut logical, mut allocated) = (0u64, 0u64, 0u64);
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(md) = Native.metadata(&p) else {
            continue;
        };
        entries = entries.saturating_add(1);
        logical = logical.saturating_add(md.logical_size);
        allocated = allocated.saturating_add(md.allocated_size.unwrap_or(md.logical_size));
        if md.kind == EntryKind::Dir
            && let Ok(list) = Native.read_dir(&p)
        {
            for e in list.flatten() {
                stack.push(e.path);
            }
        }
    }
    (entries, logical, allocated)
}

/// The fingerprint of the tree at `path` (see [`TreeFingerprint`]).
pub(crate) fn fingerprint(path: &Path) -> TreeFingerprint {
    let (entries, bytes, _) = tree_stats(path);
    TreeFingerprint { entries, bytes }
}

impl Operation<'_> {
    /// Journals `intent` durably and remembers it as this operation's open step. Nothing may
    /// touch the disk for the step unless this succeeds.
    pub(crate) fn journal_intent(&mut self, intent: &JournalIntent) -> Result<(), ScullError> {
        let seq = self.store.journal_intent(intent).map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("the step could not be journaled, so it was not run (SAFE-19): {e}"),
            )
        })?;
        self.open_step = Some(seq);
        Ok(())
    }

    /// Closes the open step, if any, with `outcome` (`None`: this attempt changed nothing and
    /// another attempt follows).
    pub(crate) fn journal_close(
        &mut self,
        outcome: Option<&ItemRecord>,
        at: i64,
    ) -> Result<(), ScullError> {
        let Some(seq) = self.open_step.take() else {
            return Ok(());
        };
        self.store
            .journal_close(
                seq,
                &JournalClose {
                    kind: JournalCloseKind::Done,
                    recorded_at: at,
                    outcome: outcome.cloned(),
                },
            )
            .map_err(ScullError::from)
    }
}
