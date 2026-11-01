//! Orphaned package caches (D-36, provisional; #217, PRD QC-03, FX-QC-05).
//!
//! # The decision
//!
//! A package cache is S1 (QC-02) and D-3/D-28 allow `Delete` at S0 only. Quarantining a cache
//! frees nothing and there is nothing to restore (QC-02), and when its tool is uninstalled
//! there is no tool command to run either. D-36 (provisional, maintainer may overrule) lets
//! scull remove such a cache itself, **through the reviewed fd-relative removal walker**,
//! under strict conditions:
//!
//! * the item is an S1 [`Category::PackageCache`] candidate whose classified action is the
//!   tool's own command (so this is exactly a cache Quick Clean would otherwise clean);
//! * **no trusted tool binary** exists (SAFE-12 resolution; a shell-only shim counts as
//!   installed);
//! * the folder is a **validated cache root** as located by `scull_detectors::locate_caches`:
//!   layout recognized, no QC-05 problem, volume check ran;
//! * it is **not a link** (the folder and every component below its anchor, looked up
//!   no-follow), on the **same volume** as its anchor, and has the located identity;
//! * a **fresh** in-use probe says **No** (unknown counts as in use);
//! * the caller obtained a **per-tool confirmation** showing the not-undoable note
//!   ([`NOT_UNDOABLE_NOTE`]);
//! * the tool is **not excluded** ([`not_orphan_removable`]):
//!   * conda: environments hard-link (or, with `allow_softlinks`, symlink) into `pkgs/`, so it
//!     is never removed directly, whether conda is installed or not;
//!   * the Go module cache: Go writes its files and folders read-only, so a direct removal
//!     fails part way through (only `go clean -modcache` makes them writable first);
//!   * Gradle and Maven: inspect-only (S3, no command, so never this path's S1 tool-command
//!     candidate either), and Maven's repository may hold artifacts that exist nowhere else.
//!
//!   The Go build cache and the NuGet global packages folder are plain re-downloadable or
//!   rebuildable files, like npm's or pip's cache, and qualify.
//!
//! # Why a narrow path, not a core rule
//!
//! `scull_core::Action::allowed_for` stays category-blind: making `Delete` legal at S1 for a
//! category there would make it legal in **every** plan (CLI, desktop, MCP) and for every cache
//! the planner classifies, with none of the conditions above. Instead the exception is one
//! explicit Executor entry point, [`Executor::delete_orphaned_cache`], that takes an
//! [`OrphanedCacheEvidence`]. Evidence can only be **gathered** inside this crate, by Quick
//! Clean from a located cache, right before the removal; it is consumed by the call and expires
//! after [`MAX_EVIDENCE_AGE`]. Nothing else in the Executor accepts S1 deletion.

use crate::executor::{DeleteReport, DeleteTarget, Executor};
use scull_core::{Action, Category, ErrorKind, FileIdentity, PlanItem, ScullError, Tier};
use scull_detectors::caches::CacheTool;
use scull_platform::EntryKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use scull_detectors::caches::NOT_UNDOABLE_NOTE;

/// How long gathered evidence stays valid: the in-use probe and the binary lookup must be
/// fresh when the walker starts.
pub const MAX_EVIDENCE_AGE: Duration = Duration::from_secs(30);

/// Why D-36 never removes `tool`'s cache itself, or `None` when it may (subject to every other
/// condition in the [module docs](self)).
///
/// conda's `pkgs/` is excluded: environments hard-link (or symlink) into it, and only conda can
/// tell which packages are still in use (`conda clean --packages`). So are the Go module cache
/// (read-only files) and the inspect-only Gradle and Maven caches (see the module docs).
pub fn not_orphan_removable(tool: CacheTool) -> Option<&'static str> {
    match tool {
        CacheTool::Conda => Some(
            "conda's package cache is never removed directly: environments hard-link into \
             pkgs/ (D-36 excludes it); only `conda clean --packages` may prune it",
        ),
        CacheTool::GoMod => Some(
            "the Go module cache is never removed directly: Go writes it read-only, so a \
             plain delete fails part way through (D-36 excludes it); only \
             `go clean -modcache` may clean it",
        ),
        CacheTool::Gradle | CacheTool::Maven => Some(
            "inspect-only: this cache has no command that cleans it, and scull never removes \
             it itself (D-36 excludes it)",
        ),
        // The long-tail tool caches (P5-DET-05): not run or removed on this branch yet.
        CacheTool::Ccache
        | CacheTool::Conan
        | CacheTool::Vcpkg
        | CacheTool::PlatformIo
        | CacheTool::Arduino
        | CacheTool::EspIdf
        | CacheTool::SwiftPm
        | CacheTool::CocoaPods
        | CacheTool::Coursier
        | CacheTool::Deno
        | CacheTool::Zig
        | CacheTool::Playwright
        | CacheTool::Cypress
        | CacheTool::Puppeteer
        | CacheTool::Electron
        | CacheTool::Homebrew
        | CacheTool::Nix => Some(
            "not supported yet: Quick Clean does not handle this tool's cache in this build, \
             and scull never removes it itself (D-36 excludes it)",
        ),
        CacheTool::Npm
        | CacheTool::Pnpm
        | CacheTool::Bun
        | CacheTool::YarnClassic
        | CacheTool::YarnBerry
        | CacheTool::Pip
        | CacheTool::Uv
        | CacheTool::GoBuild
        | CacheTool::NuGet => None,
    }
}

/// Proof, gathered right before the removal, that a package cache is orphaned and safe to
/// remove under D-36 (see the module docs). Only this crate can gather it (Quick Clean, from a
/// located cache); [`Executor::delete_orphaned_cache`] consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedCacheEvidence {
    tool: CacheTool,
    cache: PathBuf,
    anchor: PathBuf,
    identity: FileIdentity,
    anchor_identity: FileIdentity,
    searched: usize,
    gathered: Instant,
}

impl OrphanedCacheEvidence {
    /// Records evidence the caller has just checked (crate-internal: see
    /// `quickclean::QuickClean`'s gathering, which performs every check).
    pub(crate) fn checked(
        tool: CacheTool,
        cache: PathBuf,
        anchor: PathBuf,
        identity: FileIdentity,
        anchor_identity: FileIdentity,
        searched: usize,
    ) -> Self {
        Self {
            tool,
            cache,
            anchor,
            identity,
            anchor_identity,
            searched,
            gathered: Instant::now(),
        }
    }

    /// The tool whose binary was not found.
    pub fn tool(&self) -> CacheTool {
        self.tool
    }

    /// The cache folder.
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// The folder the cache was located from (the walk's anchor).
    pub fn anchor(&self) -> &Path {
        &self.anchor
    }

    /// The cache's no-follow identity when the evidence was gathered.
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// The anchor's identity when the evidence was gathered (the walk starts there and checks
    /// it).
    pub fn anchor_identity(&self) -> FileIdentity {
        self.anchor_identity
    }

    /// How many trusted directories were searched for the binary.
    pub fn directories_searched(&self) -> usize {
        self.searched
    }

    /// Whether the evidence is still fresh enough to act on.
    pub fn is_fresh(&self) -> bool {
        self.gathered.elapsed() <= MAX_EVIDENCE_AGE
    }
}

impl Executor {
    /// Removes an **orphaned package cache** (D-36): an S1 [`Category::PackageCache`] item
    /// whose tool is not installed, through the fd-relative, no-follow removal walker. See the
    /// [module docs](crate::orphaned) for the conditions. Needs the execution lock.
    ///
    /// Refused with [`ErrorKind::SafetyBlocked`] (nothing removed) unless:
    ///
    /// * the item is S1, [`Category::PackageCache`], with a tool-command action, for the
    ///   evidence's tool (rule ID), and that tool is not excluded ([`not_orphan_removable`]:
    ///   conda);
    /// * the evidence is fresh ([`MAX_EVIDENCE_AGE`]) and is for this very folder: same
    ///   identity, `target.path` is the evidence's cache and `target.anchor` its anchor, with
    ///   the anchor identity the evidence recorded;
    /// * `target.path` hashes with the item's identity to its item ID
    ///   ([`ErrorKind::PlanInvalidated`] otherwise).
    ///
    /// The walker then verifies identity and kind (a directory) at open time, never follows a
    /// link and never crosses a mount. The removal is **not undoable** ([`NOT_UNDOABLE_NOTE`]):
    /// the caller must have shown that note and obtained a confirmation for this tool.
    pub fn delete_orphaned_cache(
        &self,
        item: &PlanItem,
        target: &DeleteTarget<'_>,
        evidence: OrphanedCacheEvidence,
    ) -> Result<DeleteReport, ScullError> {
        let refuse = |msg: String| {
            Err(ScullError::new(ErrorKind::SafetyBlocked, msg)
                .with_item(item.item_id.clone())
                .with_path(item.path.clone()))
        };
        if self.lock().is_none() {
            return refuse("removing an orphaned cache needs the execution lock (SAFE-09)".into());
        }
        if item.tier != Tier::S1 || item.category != Category::PackageCache {
            return refuse(format!(
                "D-36 covers S1 package caches only (this item is {} {:?})",
                item.tier, item.category
            ));
        }
        if !matches!(item.action, Action::ToolCommand { .. }) {
            return refuse(
                "D-36 covers caches classified for their tool's own command only".into(),
            );
        }
        if let Some(why) = not_orphan_removable(evidence.tool)
            .or_else(|| CacheTool::from_rule_id(&item.rule.id).and_then(not_orphan_removable))
        {
            return refuse(why.into());
        }
        if item.rule.id != evidence.tool.rule_id() {
            return refuse(format!(
                "the evidence is for {} but the item was classified by rule {}",
                evidence.tool.binary(),
                item.rule.id
            ));
        }
        if !evidence.is_fresh() {
            return refuse(
                "the orphaned-cache evidence is stale: gather it again right before removing"
                    .into(),
            );
        }
        if evidence.identity != item.identity
            || target.path != evidence.cache
            || target.anchor != evidence.anchor
            || target.anchor_identity != Some(evidence.anchor_identity)
        {
            return refuse("the evidence is for another folder or anchor".into());
        }
        if evidence.identity.volume != evidence.anchor_identity.volume {
            return refuse("the cache is not on its anchor's volume".into());
        }
        let relative = crate::executor::check_target(item, target)?;
        self.walk(
            item,
            target.anchor,
            target.anchor_identity,
            relative,
            Some(EntryKind::Dir),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    //! The D-36 entry point's own refusals. Evidence can only be built inside this crate, so
    //! these tests construct it directly; every case runs in a `scull-fixtures` temp tree with
    //! a canary and checks the invariants.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::lock::LockOptions;
    use scull_core::{FsPath, ItemId, RuleRef};
    use scull_fixtures::{Fixture, InvariantInputs};
    use scull_platform::{Native, Platform, native_path_bytes};

    const CACHE: &str = "home/.bun/install/cache";

    fn identity(p: &Path) -> FileIdentity {
        Native.metadata(p).unwrap().identity.unwrap()
    }

    fn fixture() -> Option<Fixture> {
        if !Native.capabilities().file_identity {
            return None;
        }
        let mut fx = Fixture::new().unwrap();
        fx.file(format!("{CACHE}/a1b2c3.npm"), "manifest")
            .unwrap()
            .file(format!("{CACHE}/pkg/package.json"), "{}")
            .unwrap();
        fx.checkpoint().unwrap();
        Some(fx)
    }

    fn item(fx: &Fixture, tier: Tier, category: Category, action: Action) -> PlanItem {
        let path = fx.root().join(CACHE);
        let id = identity(&path);
        PlanItem {
            item_id: ItemId::derive(id, &native_path_bytes(&path)),
            identity: id,
            path: FsPath::from_path(&path),
            category,
            rule: RuleRef {
                id: CacheTool::Bun.rule_id().into(),
                version: 1,
            },
            tier,
            action,
            reclaimable_bytes: 0,
        }
    }

    fn tool_action() -> Action {
        Action::ToolCommand {
            tool: "bun".into(),
            args: vec!["pm".into(), "cache".into(), "rm".into()],
        }
    }

    fn evidence(fx: &Fixture) -> OrphanedCacheEvidence {
        let anchor = fx.root().join("home");
        OrphanedCacheEvidence::checked(
            CacheTool::Bun,
            fx.root().join(CACHE),
            anchor.clone(),
            identity(&fx.root().join(CACHE)),
            identity(&anchor),
            1,
        )
    }

    fn locked(fx: &Fixture) -> Executor {
        Executor::acquire(&LockOptions::at(fx.base().join("state/execution.lock"))).unwrap()
    }

    fn assert_refused(r: Result<DeleteReport, ScullError>, kind: ErrorKind, what: &str) {
        let e = r.expect_err(what);
        assert_eq!(e.kind, kind, "{what}: {e}");
    }

    #[test]
    fn d36_refuses_anything_but_a_fresh_matching_s1_package_cache() {
        let Some(fx) = fixture() else { return };
        let cache = fx.root().join(CACHE);
        let anchor = fx.root().join("home");
        let target = DeleteTarget {
            anchor: &anchor,
            anchor_identity: Some(identity(&anchor)),
            path: &cache,
        };
        let good = item(&fx, Tier::S1, Category::PackageCache, tool_action());
        let exec = locked(&fx);
        let blocked = ErrorKind::SafetyBlocked;

        // No lock.
        assert_refused(
            Executor::new().delete_orphaned_cache(&good, &target, evidence(&fx)),
            blocked,
            "no lock",
        );
        // Wrong tier, category or action: the exception is S1 package caches only.
        for (tier, category, action) in [
            (Tier::S2, Category::PackageCache, tool_action()),
            (Tier::S3, Category::PackageCache, tool_action()),
            (Tier::S0, Category::PackageCache, tool_action()),
            (Tier::S1, Category::ProjectArtifact, tool_action()),
            (Tier::S1, Category::PackageCache, Action::Quarantine),
        ] {
            let it = item(&fx, tier, category, action.clone());
            assert_refused(
                exec.delete_orphaned_cache(&it, &target, evidence(&fx)),
                blocked,
                &format!("{tier} {category:?} {action:?}"),
            );
        }
        // Evidence for another tool's rule.
        let mut npm = good.clone();
        npm.rule.id = CacheTool::Npm.rule_id().into();
        assert_refused(
            exec.delete_orphaned_cache(&npm, &target, evidence(&fx)),
            blocked,
            "other rule",
        );
        // Stale evidence.
        let mut stale = evidence(&fx);
        stale.gathered = Instant::now()
            .checked_sub(MAX_EVIDENCE_AGE + Duration::from_secs(1))
            .unwrap();
        assert_refused(
            exec.delete_orphaned_cache(&good, &target, stale),
            blocked,
            "stale",
        );
        // Evidence for another anchor or identity.
        let other_anchor = fx.root().to_path_buf();
        let moved_anchor = DeleteTarget {
            anchor: &other_anchor,
            anchor_identity: Some(identity(&other_anchor)),
            path: &cache,
        };
        assert_refused(
            exec.delete_orphaned_cache(&good, &moved_anchor, evidence(&fx)),
            blocked,
            "other anchor",
        );
        let mut wrong_id = evidence(&fx);
        wrong_id.identity = FileIdentity::new(wrong_id.identity.volume, 1);
        assert_refused(
            exec.delete_orphaned_cache(&good, &target, wrong_id),
            blocked,
            "other identity",
        );
        // Another volume than the anchor.
        let mut other_volume = evidence(&fx);
        other_volume.anchor_identity = FileIdentity::new(
            other_volume.anchor_identity.volume ^ 1,
            other_volume.anchor_identity.file,
        );
        let t = DeleteTarget {
            anchor_identity: Some(other_volume.anchor_identity),
            ..target
        };
        assert_refused(
            exec.delete_orphaned_cache(&good, &t, other_volume),
            blocked,
            "other volume",
        );
        assert!(cache.join("a1b2c3.npm").is_file(), "nothing removed");
        let inv = fx.check_invariants(&InvariantInputs::new());
        println!("{inv}");
        inv.assert_ok();

        // The matching case goes through the walker.
        let r = exec
            .delete_orphaned_cache(&good, &target, evidence(&fx))
            .unwrap();
        assert_eq!(r.report.outcome, crate::RemoveOutcome::Complete);
        assert!(!cache.exists());
        let inv = fx.check_invariants(&InvariantInputs::new().allow(CACHE));
        println!("{inv}");
        inv.assert_ok();
    }

    /// D-36 never covers conda's `pkgs/` (environments hard-link into it), the read-only Go
    /// module cache or the inspect-only Gradle and Maven caches: refused whether the evidence
    /// or only the item names the tool, with the cache left intact.
    #[test]
    fn d36_refuses_conda_pkgs() {
        let Some(fx) = fixture() else { return };
        assert!(not_orphan_removable(CacheTool::Conda).is_some());
        let excluded = [
            CacheTool::Conda,
            CacheTool::GoMod,
            CacheTool::Gradle,
            CacheTool::Maven,
        ];
        for t in CacheTool::ALL {
            // The long-tail tools (P5-DET-05) are not supported on this branch at all.
            let unsupported = !crate::quickclean::is_supported(t);
            assert_eq!(
                not_orphan_removable(t).is_some(),
                excluded.contains(&t) || unsupported,
                "{t:?}"
            );
            if unsupported {
                assert!(
                    not_orphan_removable(t).is_some_and(|w| w.contains("not supported yet")),
                    "{t:?}"
                );
            }
        }
        assert!(not_orphan_removable(CacheTool::GoMod).is_some_and(|w| w.contains("read-only")));
        for t in [CacheTool::Gradle, CacheTool::Maven] {
            assert!(not_orphan_removable(t).is_some_and(|w| w.contains("inspect-only")));
        }
        let cache = fx.root().join(CACHE);
        let anchor = fx.root().join("home");
        let target = DeleteTarget {
            anchor: &anchor,
            anchor_identity: Some(identity(&anchor)),
            path: &cache,
        };
        let exec = locked(&fx);
        for tool in excluded {
            let action = Action::ToolCommand {
                tool: tool.binary().into(),
                args: vec!["clean".into()],
            };
            // The item classified by the excluded tool's rule, evidence for Bun (forged as an
            // S1 tool-command item even for the inspect-only tools, which are never S1).
            let mut excluded_item = item(&fx, Tier::S1, Category::PackageCache, action);
            excluded_item.rule.id = tool.rule_id().into();
            let e = exec
                .delete_orphaned_cache(&excluded_item, &target, evidence(&fx))
                .expect_err("excluded item");
            assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{tool:?}: {e}");
            assert!(e.message.contains("D-36 excludes it"), "{tool:?}: {e}");
            // Evidence gathered for the excluded tool, matching everything else.
            let mut ev = evidence(&fx);
            ev.tool = tool;
            let good = item(&fx, Tier::S1, Category::PackageCache, tool_action());
            let e = exec
                .delete_orphaned_cache(&good, &target, ev)
                .expect_err("excluded evidence");
            assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{tool:?}: {e}");
            assert!(e.message.contains("D-36 excludes it"), "{tool:?}: {e}");
        }
        assert!(cache.join("a1b2c3.npm").is_file(), "nothing removed");
        let inv = fx.check_invariants(&InvariantInputs::new());
        println!("{inv}");
        inv.assert_ok();
    }

    /// A tool command Quick Clean does not run (`ollama rm` for Ollama models, P4-DET-01;
    /// `xcrun simctl delete unavailable` for simulators, P5-DET-02) is refused by every
    /// Executor path with the explicit reason, and nothing changes.
    #[test]
    fn non_quick_clean_tool_commands_are_skipped_with_a_reason() {
        let Some(fx) = fixture() else { return };
        assert!(!crate::is_quick_clean_tool("ollama"));
        for t in CacheTool::ALL {
            // Gradle and Maven are inspect-only, and the long-tail tools (P5-DET-05) are not
            // supported on this branch: their binaries are never run either.
            let never = matches!(t, CacheTool::Gradle | CacheTool::Maven)
                || !crate::quickclean::is_supported(t);
            assert_eq!(crate::is_quick_clean_tool(t.binary()), !never, "{t:?}");
        }
        let cache = fx.root().join(CACHE);
        let anchor = fx.root().join("home");
        let target = DeleteTarget {
            anchor: &anchor,
            anchor_identity: Some(identity(&anchor)),
            path: &cache,
        };
        let ollama = Action::ToolCommand {
            tool: "ollama".into(),
            args: vec!["rm".into(), "llama3:latest".into()],
        };
        let exec = locked(&fx);
        let mut ollama_item = item(&fx, Tier::S1, Category::MlCache, ollama);
        ollama_item.rule.id = "ollama.models".into();
        // `xcrun simctl delete unavailable` (P5-DET-02's simulator action) has no device-set
        // argument, so it would act on whatever set `xcrun` resolves, not the located one: no
        // Executor path runs it, scoped or not.
        assert!(!crate::is_quick_clean_tool("xcrun"));
        let simctl = Action::ToolCommand {
            tool: "xcrun".into(),
            args: scull_detectors::xcode::SIMCTL_DELETE_UNAVAILABLE
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        };
        let mut simctl_item = item(&fx, Tier::S1, Category::Other, simctl);
        simctl_item.rule.id = "xcode.simulators".into();
        for it in [ollama_item, simctl_item] {
            let e = exec
                .delete(&it, &target)
                .expect_err("tool command via delete");
            assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
            assert!(e.message.contains(crate::TOOL_NATIVE_UNSUPPORTED), "{e}");
            for executing in [Action::Quarantine, Action::Trash, Action::Delete] {
                let e = crate::executor::check_item_for(&it, &executing)
                    .expect_err("tool command via any action");
                assert!(e.message.contains(crate::TOOL_NATIVE_UNSUPPORTED), "{e}");
            }
            let e = exec
                .delete_orphaned_cache(&it, &target, evidence(&fx))
                .expect_err("tool command via D-36");
            assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        }
        // Docker's prunes (P5-DET-03) act on the whole daemon: refused by every path with the
        // Docker reason, whatever the item's category.
        for kind in [
            scull_detectors::DockerKind::BuildCache,
            scull_detectors::DockerKind::DanglingImages,
        ] {
            let it = item(&fx, Tier::S0, Category::PackageCache, kind.action());
            let e = exec.delete(&it, &target).expect_err("docker via delete");
            assert_eq!(e.message, crate::DOCKER_UNSUPPORTED, "{e}");
            for executing in [
                Action::Quarantine,
                Action::Trash,
                Action::Delete,
                it.action.clone(),
            ] {
                let e = crate::executor::check_item_for(&it, &executing)
                    .expect_err("docker via any action");
                assert_eq!(e.message, crate::DOCKER_UNSUPPORTED, "{e}");
            }
            let e = exec
                .delete_orphaned_cache(&it, &target, evidence(&fx))
                .expect_err("docker via D-36");
            assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        }
        assert!(cache.join("a1b2c3.npm").is_file(), "nothing removed");
        let inv = fx.check_invariants(&InvariantInputs::new());
        println!("{inv}");
        inv.assert_ok();
    }

    #[test]
    fn d36_a_link_at_the_cache_path_is_never_followed() {
        let Some(mut fx) = fixture() else { return };
        let cache = fx.root().join(CACHE);
        let anchor = fx.root().join("home");
        let good = item(&fx, Tier::S1, Category::PackageCache, tool_action());
        let ev = evidence(&fx);
        // Swap the cache for a link to the canary after the evidence was gathered.
        std::fs::rename(&cache, fx.root().join("home/moved")).unwrap();
        let canary = fx.canary().dir.clone();
        if !fx.symlink(CACHE, &canary).unwrap().is_created() {
            std::fs::rename(fx.root().join("home/moved"), &cache).unwrap();
            fx.check_invariants(&InvariantInputs::new()).assert_ok();
            return;
        }
        let target = DeleteTarget {
            anchor: &anchor,
            anchor_identity: Some(identity(&anchor)),
            path: &cache,
        };
        let e = locked(&fx)
            .delete_orphaned_cache(&good, &target, ev)
            .expect_err("a link is refused");
        assert!(
            matches!(
                e.kind,
                ErrorKind::PlanInvalidated | ErrorKind::SafetyBlocked
            ),
            "{e}"
        );
        let inv = fx.check_invariants(&InvariantInputs::new().allow(CACHE).allow("home/moved"));
        println!("{inv}");
        inv.assert_ok();
    }
}
