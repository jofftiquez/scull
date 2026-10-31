//! The fixture registry: every fixture ID of `docs/FIXTURES.md` §3 that this lane tracks, with
//! its gate, OSs and mode, and where it is implemented (crate + test function) or which open
//! issue tracks it.
//!
//! `docs/FIXTURES.md` is the source of truth. The tests in `tests/registry.rs` parse its tables
//! ([`parse_fixture_matrix`]) and fail when:
//!
//! * a fixture of a [covered section](COVERED_SECTIONS) is missing here, or its gate, OS or mode
//!   differs from the document;
//! * an MVP-gated CI fixture is neither [`Status::Implemented`] nor [`Status::Tracked`] with an
//!   issue number and a reason;
//! * a registered test function does not exist in the file it names (a `#[test]` function, or
//!   an entry of a custom harness's `TESTS` table);
//! * a fixture's [`RegisteredFixture::pending_ci`] or [`RegisteredFixture::missing_os`] names
//!   an OS it doesn't run on, or an implemented fixture misses an OS.
//!
//! Per-OS status ([`RegisteredFixture::os_status`]): the OSs whose runs are still pending CI
//! (tests written and type-checked for every target, but not yet run green there, e.g. Phase 2
//! fixtures written while the 3-OS CI was unavailable) are listed in `pending_ci`; the OSs no
//! test covers yet in `missing_os` (tracked fixtures only); every other OS of the fixture has
//! run.
//!
//! Test paths are relative to the workspace root, so a registry entry is also a pointer for a
//! reader: `crates/scull-detectors/tests/rust_detector.rs` + `fx_cls_01_...`.

use std::fmt;

/// The release that first requires a fixture to pass (FIXTURES §2, "Gate column").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Gate {
    /// MVP release gate (PRD §21.3).
    Mvp,
    /// v1.0.
    V1_0,
    /// v1.x.
    V1x,
}

impl Gate {
    /// Parse a FIXTURES.md gate cell (`MVP`, `v1.0`, `v1.x`).
    pub fn parse(cell: &str) -> Option<Self> {
        match cell.trim() {
            "MVP" => Some(Self::Mvp),
            "v1.0" => Some(Self::V1_0),
            "v1.x" => Some(Self::V1x),
            _ => None,
        }
    }
}

/// Where a fixture runs (FIXTURES §2, "Modes").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Every pull request, all three OSs.
    Ci,
    /// Slower or privileged tests.
    Nightly,
    /// Needs a real desktop session or account state.
    Manual,
}

impl Mode {
    /// Parse a FIXTURES.md mode cell (`CI`, `Nightly`, `Manual`).
    pub fn parse(cell: &str) -> Option<Self> {
        match cell.trim() {
            "CI" => Some(Self::Ci),
            "Nightly" => Some(Self::Nightly),
            "Manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// A test function, by workspace-relative source file and function name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TestRef {
    /// Source file, relative to the workspace root, `/`-separated.
    pub file: &'static str,
    /// The `#[test]` function name.
    pub test_fn: &'static str,
}

impl fmt::Display for TestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{}", self.file, self.test_fn)
    }
}

/// How far a fixture is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Fully implemented by these tests. The first listed is the end-to-end fixture when one
    /// exists; the others cover the same scenario at one layer (detector, rubric, Git, ...).
    Implemented(&'static [TestRef]),
    /// Not (fully) implemented yet; an open issue owns the rest. `partial` lists tests that
    /// already cover part of the scenario.
    Tracked {
        /// The GitHub issue that will finish it.
        issue: u32,
        /// What is missing and why.
        reason: &'static str,
        /// Tests covering part of the scenario today.
        partial: &'static [TestRef],
    },
    /// Not required at the MVP CI gate (later gate, or nightly/manual mode) and not started.
    Deferred,
}

impl Status {
    /// Every test this status names.
    pub fn tests(&self) -> &'static [TestRef] {
        match self {
            Status::Implemented(t) => t,
            Status::Tracked { partial, .. } => partial,
            Status::Deferred => &[],
        }
    }
}

/// One registered fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredFixture {
    /// Fixture ID, e.g. `FX-CLS-01`.
    pub id: &'static str,
    /// Release gate.
    pub gate: Gate,
    /// OS cell as written in FIXTURES.md (`MLW`, `MW`, `L (M via disk image)`, ...).
    pub os: &'static str,
    /// Mode.
    pub mode: Mode,
    /// Implementation status.
    pub status: Status,
    /// OSs (letters of `M`, `L`, `W`) on which the registered tests have **not yet run
    /// green**: written (and type-checked for every target) but waiting for the 3-OS CI, e.g.
    /// a Windows- or macOS-only fixture written while CI was unavailable. Empty when every OS
    /// of the fixture has run. See [`RegisteredFixture::os_status`].
    pub pending_ci: &'static str,
    /// OSs on which **no registered test covers the scenario yet** (e.g. Windows before its
    /// quarantine moves exist). Only a [`Status::Tracked`] fixture may miss an OS; its reason
    /// says why.
    pub missing_os: &'static str,
}

/// Whether a fixture's tests have run green on one OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsRun {
    /// The registered tests ran green on this OS.
    Verified,
    /// Written, waiting for CI on this OS.
    PendingCi,
    /// No test covers this OS yet.
    Missing,
}

impl RegisteredFixture {
    /// Whether the MVP CI gate requires this fixture.
    pub fn is_mvp_ci(&self) -> bool {
        self.gate == Gate::Mvp && self.mode == Mode::Ci
    }

    /// Mark the tests as not yet run on the OSs in `os` (letters of `M`, `L`, `W`).
    pub const fn pending_ci(mut self, os: &'static str) -> Self {
        self.pending_ci = os;
        self
    }

    /// Mark the OSs in `os` as not covered by any registered test yet.
    pub const fn missing_os(mut self, os: &'static str) -> Self {
        self.missing_os = os;
        self
    }

    /// The OS letters (`M`, `L`, `W`, in that order) the fixture runs on, from its OS cell
    /// (`MLW`, `MW`, `L (M via disk image)` → `L`, ...): the letters before any parenthesis.
    pub fn os_letters(&self) -> Vec<char> {
        let cell = self.os.split('(').next().unwrap_or("");
        ['M', 'L', 'W']
            .into_iter()
            .filter(|c| cell.contains(*c))
            .collect()
    }

    /// Per-OS run status of an implemented or partly implemented fixture.
    pub fn os_status(&self) -> Vec<(char, OsRun)> {
        self.os_letters()
            .into_iter()
            .map(|c| {
                let run = if self.missing_os.contains(c) {
                    OsRun::Missing
                } else if self.pending_ci.contains(c) {
                    OsRun::PendingCi
                } else {
                    OsRun::Verified
                };
                (c, run)
            })
            .collect()
    }
}

/// Sections of FIXTURES.md §3 whose every fixture must be registered (ID prefix).
pub const COVERED_SECTIONS: &[&str] = &[
    "FX-CLS", "FX-GIT", "FX-FS", "FX-RACE", "FX-EXEC", "FX-PHY", "FX-QC", "FX-CFG",
];

const fn t(file: &'static str, test_fn: &'static str) -> TestRef {
    TestRef { file, test_fn }
}

const E2E_CLS: &str = "crates/scull-fixtures/tests/fx_cls.rs";
const RUST_DET: &str = "crates/scull-detectors/tests/rust_detector.rs";
const NODE_DET: &str = "crates/scull-detectors/tests/node_detector.rs";
const RUBRIC: &str = "crates/scull-safety/src/rubric.rs";
const ACTIVITY: &str = "crates/scull-safety/tests/activity.rs";
const CORE_CFG: &str = "crates/scull-core/tests/config.rs";
const HARDLINKS: &str = "crates/scull-scanner/tests/hardlinks.rs";
const PROTECT: &str = "crates/scull-safety/tests/protect.rs";
const E2E_GIT: &str = "crates/scull-fixtures/tests/fx_git.rs";
const GIT_CLS: &str = "crates/scull-git/tests/classify_fixtures.rs";
const GIT_STATE: &str = "crates/scull-git/tests/state.rs";
const GIT_DISC: &str = "crates/scull-git/tests/discovery.rs";
const GIT_HARD: &str = "crates/scull-git/tests/hardened.rs";
const E2E_FS: &str = "crates/scull-fixtures/tests/fx_fs.rs";
const SCAN_SPECIAL: &str = "crates/scull-scanner/tests/special_files.rs";
const SCAN_UNIX: &str = "crates/scull-scanner/tests/unix.rs";
const SCAN_WIN: &str = "crates/scull-scanner/tests/windows.rs";
const LARGE: &str = "crates/scull-detectors/tests/large_files.rs";
const EXEC_UNIX: &str = "crates/scull-cleaner/tests/executor_unix.rs";
const E2E_PHY: &str = "crates/scull-fixtures/tests/fx_phy.rs";
const MEASURE: &str = "crates/scull-cleaner/tests/measure.rs";
const MEASURE_UNIT: &str = "crates/scull-cleaner/src/measure.rs";
const TRASH: &str = "crates/scull-cleaner/tests/trash.rs";
const E2E_QC: &str = "crates/scull-fixtures/tests/fx_qc.rs";
/// Custom harness (`harness = false`): tests are listed in its `TESTS` table, not `#[test]`.
const QC: &str = "crates/scull-cleaner/tests/quickclean/main.rs";
const CACHES: &str = "crates/scull-detectors/tests/cache_locator.rs";
const ENGINE: &str = "crates/scull-engine/tests/pipeline.rs";
const RULES: &str = "crates/scull-detectors/tests/rules_loader.rs";
const E2E_RACE: &str = "crates/scull-fixtures/tests/fx_race.rs";
const E2E_EXEC: &str = "crates/scull-fixtures/tests/fx_exec.rs";
const EXEC_WIN: &str = "crates/scull-cleaner/tests/executor_windows.rs";
const REVAL: &str = "crates/scull-cleaner/tests/revalidation.rs";
const EXEC_LOCK: &str = "crates/scull-cleaner/tests/exec_lock.rs";
/// Unix only (`#![cfg(unix)]`), like `quarantine.rs` and `restore.rs`.
const JOURNAL: &str = "crates/scull-cleaner/tests/journal.rs";
const QUAR: &str = "crates/scull-cleaner/tests/quarantine.rs";
const RESTORE: &str = "crates/scull-cleaner/tests/restore.rs";
const AUDIT: &str = "crates/scull-storage/tests/audit.rs";

/// Phase 2 tests (on the `phase2` branch) have run green locally on Linux; the macOS and
/// Windows runs wait for the 3-OS CI (GitHub Actions minutes exhausted since 2026-10-31).
const PHASE2_PENDING: &str = "MW";

const fn fx(
    id: &'static str,
    gate: Gate,
    os: &'static str,
    mode: Mode,
    status: Status,
) -> RegisteredFixture {
    RegisteredFixture {
        id,
        gate,
        os,
        mode,
        status,
        pending_ci: "",
        missing_os: "",
    }
}

/// Every registered fixture, in FIXTURES.md order.
pub const REGISTRY: &[RegisteredFixture] = &[
    // §3.1 Classification (FX-CLS)
    fx(
        "FX-CLS-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_01_idle_cargo_target_with_lockfile_is_s0_in_plan",
            ),
            t(RUST_DET, "fx_cls_01_cargo_target_with_lockfile_idle_is_s0"),
            t(RUST_DET, "fx_cls_01_cargo_target_without_lockfile_is_s2"),
            t(RUBRIC, "fx_cls_01_cargo_target_with_lockfile_idle_is_s0"),
        ]),
    ),
    fx(
        "FX-CLS-02",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_02_target_full_of_photos_is_not_a_candidate",
            ),
            t(
                RUST_DET,
                "fx_cls_02_target_with_photos_and_no_cargo_toml_is_not_a_candidate",
            ),
            t(
                RUST_DET,
                "fx_cls_02_symlink_named_target_is_never_a_candidate",
            ),
            t(RUBRIC, "fx_cls_02_no_marker_is_not_a_candidate_and_s3"),
        ]),
    ),
    fx(
        "FX-CLS-03",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_03_git_tracked_dist_is_s3_and_never_planned",
            ),
            t(RUBRIC, "fx_cls_03_git_tracked_dist_is_s3"),
        ]),
    ),
    fx(
        "FX-CLS-04",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_04_shared_target_dir_is_one_candidate_counted_once",
            ),
            t(
                RUST_DET,
                "fx_cls_04_shared_cargo_target_dir_is_one_candidate_for_both_projects",
            ),
            t(
                RUST_DET,
                "fx_cls_04_project_target_used_by_another_project_is_merged_not_duplicated",
            ),
            t(
                RUST_DET,
                "fx_cls_04_declared_dirs_that_hold_projects_or_data_are_never_reported",
            ),
        ]),
    ),
    fx(
        "FX-CLS-05",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_05_node_modules_with_lockfile_s0_without_lockfile_s2",
            ),
            t(NODE_DET, "fx_cls_05_node_modules_with_lockfile_is_s0"),
            t(NODE_DET, "fx_cls_05_node_modules_without_lockfile_is_s2"),
            t(RUBRIC, "fx_cls_05_node_modules_with_lockfile_s0_without_s2"),
        ]),
    ),
    fx(
        "FX-CLS-06",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 83,
            reason: "classification and link recording are end-to-end; the removal half \
                     (\"deleting node_modules removes the links only\") runs against a \
                     std::fs stand-in until the Executor exists (#67 Unix, #68 Windows); \
                     the FX-EXEC fixtures (#83) re-run it through the real executor",
            partial: &[
                t(
                    E2E_CLS,
                    "fx_cls_06_linked_workspace_sources_survive_node_modules_removal",
                ),
                t(
                    NODE_DET,
                    "fx_cls_06_workspace_and_link_symlinks_are_recorded_never_followed",
                ),
            ],
        },
    ),
    fx("FX-CLS-07", Gate::V1_0, "MLW", Mode::Ci, Status::Deferred),
    fx(
        "FX-CLS-08",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_08_pnpm_monorepo_30_packages_found_once_totals_match",
            ),
            t(
                NODE_DET,
                "fx_cls_08_pnpm_monorepo_with_30_packages_every_candidate_found_once",
            ),
            t(
                HARDLINKS,
                "fx_cls_08_pnpm_monorepo_sizes_match_independent_dedup",
            ),
        ]),
    ),
    fx(
        "FX-CLS-09",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_09_target_touched_an_hour_ago_is_bumped_to_s1",
            ),
            t(
                RUST_DET,
                "fx_cls_09_target_with_file_modified_an_hour_ago_is_bumped_to_s1",
            ),
            t(ACTIVITY, "fx_cls_09_recent_file_in_target_bumps_s0_to_s1"),
            t(RUBRIC, "fx_cls_09_recent_activity_bumps_s0_to_s1"),
        ]),
    ),
    fx("FX-CLS-10", Gate::V1x, "MLW", Mode::Ci, Status::Deferred),
    fx(
        "FX-CLS-11",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_11_scull_protect_makes_every_candidate_inside_s3",
            ),
            t(PROTECT, "fx_cls_11_protect_marker_covers_everything_inside"),
            t(
                CORE_CFG,
                "fx_cls_11_scull_protect_marker_makes_everything_inside_s3",
            ),
            t(RUBRIC, "fx_cls_11_scull_protect_marker_is_s3"),
        ]),
    ),
    fx(
        "FX-CLS-12",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_CLS,
                "fx_cls_12_home_scan_needs_confirmation_and_secrets_are_never_candidates",
            ),
            t(
                PROTECT,
                "fx_cls_12_home_os_folders_and_secrets_are_protected",
            ),
            t(
                CORE_CFG,
                "fx_cls_12_home_and_filesystem_roots_require_confirmation",
            ),
        ]),
    ),
    fx("FX-CLS-13", Gate::V1_0, "MLW", Mode::Ci, Status::Deferred),
    fx("FX-CLS-14", Gate::V1x, "MW", Mode::Ci, Status::Deferred),
    // §3.2 Git worktrees (FX-GIT)
    fx(
        "FX-GIT-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_GIT,
                "fx_git_01_clean_merged_pushed_worktree_is_s1_branch_kept",
            ),
            t(
                GIT_CLS,
                "fx_git_01_clean_merged_pushed_worktree_is_s1_never_s0",
            ),
            t(
                GIT_STATE,
                "fx_git_01_merged_and_pushed_worktree_is_clean_and_proven_merged",
            ),
        ]),
    ),
    fx(
        "FX-GIT-02",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_GIT,
                "fx_git_02_modified_tracked_file_is_s3_artifacts_only",
            ),
            t(
                GIT_CLS,
                "fx_git_02_modified_tracked_file_is_s3_with_artifact_cleanup_allowed",
            ),
            t(GIT_STATE, "fx_git_02_modified_tracked_file_is_reported"),
        ]),
    ),
    fx(
        "FX-GIT-03",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_03_untracked_file_is_s3"),
            t(GIT_CLS, "fx_git_03_untracked_file_is_s3"),
            t(GIT_STATE, "fx_git_03_untracked_file_is_reported"),
        ]),
    ),
    fx(
        "FX-GIT-04",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_GIT,
                "fx_git_04_unique_ignored_files_are_s2_and_listed_by_name",
            ),
            t(
                GIT_CLS,
                "fx_git_04_unique_ignored_files_are_s2_and_listed_by_name",
            ),
            t(GIT_STATE, "fx_git_04_ignored_files_are_listed_by_name"),
        ]),
    ),
    fx(
        "FX-GIT-05",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_05_unpushed_commits_are_s3"),
            t(GIT_CLS, "fx_git_05_unpushed_commits_are_s3"),
            t(GIT_STATE, "fx_git_05_unpushed_commits_are_counted"),
        ]),
    ),
    fx(
        "FX-GIT-06",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_GIT,
                "fx_git_06_squash_merged_branch_is_s2_merge_unproven",
            ),
            t(
                GIT_CLS,
                "fx_git_06_squash_merge_without_forge_is_s2_merge_unproven",
            ),
            t(GIT_STATE, "fx_git_06_squash_merged_branch_is_patch_id_only"),
        ]),
    ),
    fx(
        "FX-GIT-07",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_07_orphan_detached_head_is_s3"),
            t(GIT_CLS, "fx_git_07_orphan_detached_head_is_s3"),
            t(GIT_STATE, "fx_git_07_detached_head_reachable_from_no_ref"),
        ]),
    ),
    fx(
        "FX-GIT-08",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_08_detached_head_on_pushed_commit_is_s1"),
            t(
                GIT_CLS,
                "fx_git_08_detached_head_on_pushed_commit_clean_is_s1",
            ),
            t(
                GIT_STATE,
                "fx_git_08_detached_head_reachable_from_pushed_ref",
            ),
        ]),
    ),
    fx(
        "FX-GIT-09",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_09_locked_worktree_is_s3_with_reason"),
            t(GIT_CLS, "fx_git_09_locked_worktree_is_s3_with_reason_shown"),
            t(GIT_STATE, "fx_git_09_lock_and_reason_are_reported"),
            t(GIT_DISC, "fx_git_09_locked_worktree"),
        ]),
    ),
    fx(
        "FX-GIT-10",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_10_registration_without_folder_is_s0_prune"),
            t(
                GIT_CLS,
                "fx_git_10_stale_registration_is_s0_prune_metadata_only",
            ),
            t(
                GIT_STATE,
                "fx_git_10_missing_folder_is_reported_as_missing_and_unknown",
            ),
            t(GIT_DISC, "fx_git_10_prunable_registration"),
        ]),
    ),
    fx(
        "FX-GIT-11",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_11_no_upstream_local_only_commits_is_s3"),
            t(GIT_CLS, "fx_git_11_no_upstream_local_only_commits_is_s3"),
            t(GIT_STATE, "fx_git_11_no_upstream_and_local_only_commits"),
        ]),
    ),
    fx(
        "FX-GIT-12",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_GIT,
                "fx_git_12_stale_remote_refs_are_protective_without_fetch",
            ),
            t(
                GIT_CLS,
                "fx_git_12_stale_remote_refs_classify_protectively_without_fetch",
            ),
            t(
                GIT_STATE,
                "fx_git_12_stale_remote_refs_are_used_as_is_and_nothing_is_fetched",
            ),
        ]),
    ),
    fx(
        "FX-GIT-13",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_13_dirty_submodule_is_s3"),
            t(GIT_CLS, "fx_git_13_dirty_submodule_is_s3"),
            t(GIT_STATE, "fx_git_13_dirty_submodule_is_reported"),
        ]),
    ),
    fx(
        "FX-GIT-14",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_GIT, "fx_git_14_process_cwd_in_worktree_is_s3_in_use"),
            t(ACTIVITY, "fx_git_14_process_cwd_inside_worktree_is_in_use"),
            t(
                GIT_CLS,
                "fx_git_14_process_cwd_in_worktree_is_s3_in_use_for_this_run",
            ),
            t(
                GIT_STATE,
                "fx_git_14_analysis_works_while_a_process_has_its_cwd_in_the_worktree",
            ),
        ]),
    ),
    fx(
        "FX-GIT-15",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_GIT,
                "fx_git_15_malicious_fsmonitor_never_runs_and_scan_completes",
            ),
            t(GIT_HARD, "fx_git_15_fsmonitor_command_never_runs"),
            t(GIT_HARD, "fx_git_15_hooks_never_run"),
            t(GIT_HARD, "fx_git_15_filter_drivers_never_run"),
            t(GIT_HARD, "fx_git_15_diff_drivers_never_run"),
            t(GIT_HARD, "fx_git_15_included_config_never_runs"),
            t(GIT_HARD, "fx_git_15_ssh_command_and_lazy_fetch_never_run"),
            t(GIT_HARD, "fx_git_15_submodule_config_never_runs"),
            t(GIT_HARD, "fx_git_15_untrusted_git_binaries_are_never_used"),
            t(GIT_HARD, "fx_git_15_scan_leaves_repository_unchanged"),
        ]),
    ),
    fx(
        "FX-GIT-16",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_GIT,
                "fx_git_16_bare_repo_worktree_pointing_outside_scan_root",
            ),
            t(GIT_CLS, "fx_git_16_bare_repo_worktree_outside_scan_root"),
            t(GIT_DISC, "fx_git_16_bare_repo_worktree_outside_scan_root"),
            t(GIT_STATE, "fx_git_16_bare_repository_with_worktrees"),
        ]),
    ),
    fx(
        "FX-GIT-17",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            E2E_GIT,
            "fx_git_17_sixty_agent_worktrees_in_mixed_states",
        )]),
    ),
    // §3.3 Filesystem edge cases (FX-FS)
    fx(
        "FX-FS-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 68,
            reason: "scan and classify are end-to-end (link counted, never followed, canary bytes not attributed); on Unix the links are removed through the real Executor walker (#67); the Windows walker (#68) is pending",
            partial: &[
                t(
                    EXEC_UNIX,
                    "fx_fs_01_symlink_to_canary_inside_candidate_link_removed_canary_intact",
                ),
                t(
                    E2E_FS,
                    "fx_fs_01_symlink_to_canary_inside_candidate_link_removed_canary_intact",
                ),
            ],
        },
    ),
    fx(
        "FX-FS-02",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_FS,
                "fx_fs_02_symlink_loops_terminate_and_are_recorded_once",
            ),
            t(
                SCAN_SPECIAL,
                "fx_fs_02_symlink_loops_chains_and_dangling_links",
            ),
            t(
                SCAN_SPECIAL,
                "fx_fs_02_directory_hardlink_loops_are_reported_once_and_not_descended",
            ),
            t(
                SCAN_SPECIAL,
                "fx_fs_02_links_are_never_followed_even_where_the_os_would",
            ),
            t(SCAN_UNIX, "fx_fs_02_symlink_loops_are_not_followed"),
            t(SCAN_WIN, "fx_fs_02_junction_loops_are_never_descended"),
        ]),
    ),
    fx(
        "FX-FS-03",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 56,
            reason: "the scanner never follows a link, so such a candidate is never found by a scan (tested); a path given explicitly through a link (e.g. a clean target) gets the link_escape signal from the shared engine, P1-CLI-02 / scull-engine (D-29), not merged yet",
            partial: &[t(
                E2E_FS,
                "fx_fs_03_candidate_behind_an_escaping_link_is_never_reached",
            )],
        },
    ),
    fx(
        "FX-FS-04",
        Gate::Mvp,
        "L (M via disk image)",
        Mode::Nightly,
        Status::Tracked {
            issue: 189,
            reason: "needs a privileged nightly job (bind/loop mounts); no nightly workflow exists yet. The Executor walker's abort at a mount point runs where an unprivileged mount namespace exists (skipped with the reason elsewhere) and is simulated in scull-platform's walker unit tests",
            partial: &[t(
                EXEC_UNIX,
                "fx_fs_04_mount_inside_candidate_is_never_crossed",
            )],
        },
    ),
    fx(
        "FX-FS-05",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 68,
            reason: "bytes attributed once (shared, not freed) is end-to-end; on Unix the candidate's link is removed through the real Executor walker with the canary file intact (#67); the Windows walker (#68) is pending",
            partial: &[
                t(
                    EXEC_UNIX,
                    "fx_fs_05_hardlink_to_canary_link_removed_canary_intact",
                ),
                t(E2E_FS, "fx_fs_05_hardlink_to_canary_bytes_attributed_once"),
                t(
                    HARDLINKS,
                    "fx_fs_05_link_to_canary_is_shared_and_counted_once",
                ),
            ],
        },
    ),
    fx(
        "FX-FS-06",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_FS, "fx_fs_06_sparse_file_estimate_uses_allocated_size"),
            t(LARGE, "fx_fs_06_sparse_file_reported_by_allocated_size"),
        ]),
    ),
    fx(
        "FX-FS-07",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 68,
            reason: "scan and classify with read-only entries are end-to-end and I-3 proves no permission changed; on Unix the Executor walker deletes what is permitted and reports the rest without changing permissions (#67); the Windows walker (#68) is pending",
            partial: &[
                t(
                    EXEC_UNIX,
                    "fx_fs_07_read_only_entries_deleted_if_permitted_or_reported",
                ),
                t(
                    E2E_FS,
                    "fx_fs_07_read_only_entries_inside_candidate_permissions_untouched",
                ),
            ],
        },
    ),
    fx(
        "FX-FS-08",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 68,
            reason: "the scan reports the denied folder with its reason, marks the item incomplete and never lowers protection (end-to-end, Unix); on Unix the Executor reports the item incomplete with the reason, no escalation (#67); the Windows walker (#68) is pending",
            partial: &[
                t(
                    EXEC_UNIX,
                    "fx_fs_08_permission_denied_subfolder_reported_not_escalated",
                ),
                t(
                    E2E_FS,
                    "fx_fs_08_permission_denied_subfolder_is_reported_not_escalated",
                ),
                t(
                    SCAN_UNIX,
                    "fx_fs_08_unreadable_directory_is_reported_and_scan_continues",
                ),
            ],
        },
    ),
    fx(
        "FX-FS-09",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_FS,
                "fx_fs_09_unicode_names_identity_and_rtl_override_flagged",
            ),
            t(
                PROTECT,
                "fx_fs_09_unicode_normalization_alias_cannot_bypass_protection",
            ),
        ]),
    ),
    fx(
        "FX-FS-10",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            E2E_FS,
            "fx_fs_10_control_characters_in_names_are_escaped_and_json_stays_valid",
        )]),
    ),
    fx(
        "FX-FS-11",
        Gate::Mvp,
        "MW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_FS,
                "fx_fs_11_protected_folder_by_case_alias_is_still_protected",
            ),
            t(PROTECT, "fx_fs_11_case_alias_cannot_bypass_protection"),
        ]),
    ),
    fx(
        "FX-FS-12",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 68,
            reason: "a >260-character candidate path is scanned and classified end-to-end; on Unix a tree far beyond PATH_MAX is deleted by the handle-relative Executor walker (#67); the Windows walker (#68, `\\\\?\\` paths) is pending",
            partial: &[
                t(
                    EXEC_UNIX,
                    "fx_fs_12_very_deep_tree_beyond_path_max_is_removed",
                ),
                t(
                    E2E_FS,
                    "fx_fs_12_long_path_candidate_is_scanned_and_classified",
                ),
            ],
        },
    ),
    fx(
        "FX-FS-13",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(E2E_FS, "fx_fs_13_option_like_folder_names_are_paths")]),
    ),
    fx(
        "FX-FS-14",
        Gate::Mvp,
        "ML",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_FS,
                "fx_fs_14_fifo_and_socket_inside_candidate_never_opened",
            ),
            t(
                EXEC_UNIX,
                "fx_fs_14_fifo_and_socket_inside_candidate_unlinked_never_opened",
            ),
            t(
                SCAN_SPECIAL,
                "fx_fs_14_devices_sockets_and_fifos_inside_a_candidate_are_never_opened",
            ),
            t(
                SCAN_UNIX,
                "fx_fs_14_fifo_without_writer_is_never_opened_and_scan_is_fast",
            ),
            t(SCAN_UNIX, "fx_fs_14_special_files_are_never_opened"),
            t(
                SCAN_UNIX,
                "fx_fs_14_device_nodes_are_classified_from_metadata_only",
            ),
            t(
                SCAN_UNIX,
                "fx_fs_14_special_file_roots_are_refused_without_opening",
            ),
            t(
                SCAN_WIN,
                "fx_fs_14_af_unix_socket_file_is_a_special_never_opened",
            ),
        ]),
    ),
    fx(
        "FX-FS-15",
        Gate::Mvp,
        "MLW",
        Mode::Nightly,
        Status::Tracked {
            issue: 189,
            reason: "a 1M-entry tree takes minutes; it belongs in the nightly job, which doesn't exist yet",
            partial: &[],
        },
    ),
    // §3.4 Races and crash recovery (FX-RACE)
    fx(
        "FX-RACE-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_RACE,
                "fx_race_01_candidate_replaced_by_link_to_canary_skipped_audited",
            ),
            t(
                REVAL,
                "fx_race_01_candidate_replaced_by_symlink_skipped_canary_intact",
            ),
            t(EXEC_UNIX, "fx_race_01_candidate_replaced_by_symlink_is_refused"),
            t(EXEC_WIN, "fx_race_01_candidate_replaced_by_junction_is_refused"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-RACE-02",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                EXEC_UNIX,
                "fx_race_02_subfolder_swapped_for_symlink_before_listing_only_link_removed",
            ),
            t(
                EXEC_UNIX,
                "fx_race_02_subfolder_swapped_for_symlink_between_stat_and_open_aborts",
            ),
            t(
                EXEC_UNIX,
                "fx_race_02_subfolder_replaced_by_other_directory_aborts_on_identity",
            ),
            t(EXEC_UNIX, "fx_race_02_nested_folder_swapped_mid_walk_aborts"),
            t(EXEC_UNIX, "fx_race_02_directory_moved_out_during_walk_aborts"),
            t(
                EXEC_UNIX,
                "fx_race_02_concurrent_symlink_swaps_never_reach_the_canary",
            ),
            t(
                EXEC_UNIX,
                "fx_race_02_directory_moved_after_listing_nothing_outside_deleted",
            ),
            t(
                EXEC_WIN,
                "fx_race_02_subfolder_swapped_for_junction_before_listing_only_link_removed",
            ),
            t(
                EXEC_WIN,
                "fx_race_02_subfolder_swapped_for_junction_after_listing_aborts",
            ),
            t(
                EXEC_WIN,
                "fx_race_02_subfolder_replaced_by_other_directory_aborts_on_identity",
            ),
            t(EXEC_WIN, "fx_race_02_nested_folder_swapped_mid_walk_aborts"),
            t(EXEC_WIN, "fx_race_02_entry_kind_changed_since_listing_aborts"),
            t(
                EXEC_WIN,
                "fx_race_02_held_directory_cannot_be_moved_out_during_walk",
            ),
            t(
                EXEC_WIN,
                "fx_race_02_concurrent_symlink_swaps_never_reach_the_canary",
            ),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-RACE-03",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            REVAL,
            "fx_race_03_new_untracked_file_in_worktree_raises_tier_item_skipped",
        )]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-RACE-04",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            REVAL,
            "fx_race_04_build_started_after_scan_item_skipped_in_use",
        )]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-RACE-05",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                EXEC_LOCK,
                "fx_race_05_two_executors_start_at_once_second_exits_6",
            ),
            t(
                EXEC_LOCK,
                "fx_race_05_other_process_holds_lock_exit_6_then_released_on_death",
            ),
            t(
                QUAR,
                "fx_race_05_plan_invalidated_when_another_operation_touched_its_targets",
            ),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-RACE-06",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 74,
            reason: "cancel mid-execution, partial audit and a completing rerun are covered through the journal on Unix; the journal fixtures need quarantine moves, which Windows does not have yet",
            partial: &[t(
                JOURNAL,
                "fx_race_06_cancel_mid_execution_consistent_and_rerun_completes",
            )],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    fx(
        "FX-RACE-07",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 74,
            reason: "a process killed during a quarantine move is reconciled by the journal on Unix; Windows has no quarantine moves yet, so there is nothing to kill there",
            partial: &[t(
                JOURNAL,
                "fx_race_07_killed_during_quarantine_move_journal_reconciles",
            )],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    // §3.5 Executor, plans, quarantine, restore (FX-EXEC)
    fx(
        "FX-EXEC-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 76,
            reason: "the S0 delete after a consumed approval and the reported measured delta are end-to-end with I-4 on the Executor's audit, but I-5 can't hold: Operation::finish always records measured_delta_bytes as None",
            partial: &[t(
                E2E_EXEC,
                "fx_exec_01_s0_deleted_after_confirmation_measured_delta_reported",
            )],
        },
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-EXEC-02",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 71,
            reason: "same-volume quarantine and the identical restored tree are covered on Unix; Windows quarantine moves (handle-based rename) are not implemented yet",
            partial: &[
                t(
                    QUAR,
                    "fx_exec_02_s1_item_quarantined_by_same_volume_rename_tree_identical",
                ),
                t(
                    RESTORE,
                    "fx_exec_02_quarantined_item_restored_tree_identical_and_idempotent",
                ),
            ],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    fx(
        "FX-EXEC-03",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 71,
            reason: "blocked-never-deleted is covered on Unix and the read-only parent Trash refusal on Linux; Windows quarantine is not implemented yet, so the fallback chain can't be staged there",
            partial: &[
                t(QUAR, "fx_exec_03_quarantine_impossible_blocked_never_deleted"),
                t(
                    TRASH,
                    "fx_exec_03_read_only_parent_trash_refused_never_deleted",
                ),
            ],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    fx(
        "FX-EXEC-04",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 72,
            reason: "restore to a sibling path without overwriting is covered on Unix; Windows restore needs the Windows quarantine moves first",
            partial: &[t(
                RESTORE,
                "fx_exec_04_original_path_exists_restored_to_sibling_nothing_overwritten",
            )],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    fx(
        "FX-EXEC-05",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 72,
            reason: "a tampered folder manifest is refused and the database manifest wins on Unix; Windows restore needs the Windows quarantine moves first",
            partial: &[t(
                RESTORE,
                "fx_exec_05_tampered_manifest_pointing_at_ssh_is_refused_db_is_authoritative",
            )],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    fx(
        "FX-EXEC-06",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 71,
            reason: "expired quarantine entries are purged with freed bytes recorded on Unix; Windows quarantine is not implemented yet",
            partial: &[t(
                QUAR,
                "fx_exec_06_retention_expires_purged_freed_bytes_recorded",
            )],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    fx(
        "FX-EXEC-07",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            E2E_EXEC,
            "fx_exec_07_plan_file_edited_to_add_a_path_rejected_exit_4",
        )]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-EXEC-08",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(E2E_EXEC, "fx_exec_08_expired_plan_rejected_exit_4")]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-EXEC-09",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            E2E_EXEC,
            "fx_exec_09_plan_from_another_machine_or_user_rejected_exit_4",
        )]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-EXEC-10",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            REVAL,
            "fx_exec_10_item_became_s3_since_planning_is_skipped_with_reason",
        )]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-EXEC-11",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 77,
            reason: "type-to-confirm and the --allow-permanent refusal for permanent S1 deletion are not implemented yet (P2-EXEC-11)",
            partial: &[],
        },
    ),
    fx(
        "FX-EXEC-12",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 77,
            reason: "the Executor's refusal to run elevated without --allow-root is not implemented yet (P2-EXEC-11; Quick Clean already refuses)",
            partial: &[],
        },
    ),
    fx(
        "FX-EXEC-13",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_EXEC,
                "fx_exec_13_normal_run_audit_append_only_one_outcome_per_item",
            ),
            t(
                AUDIT,
                "fx_exec_13_normal_run_records_one_outcome_per_item_and_verifies",
            ),
            t(AUDIT, "fx_exec_13_audit_is_append_only_at_the_sql_level"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-EXEC-14",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 68,
            reason: "one of five items failing on permission denied (exit 3, four complete, failure explained in the report and the audit) is end-to-end on Unix; the Windows variant needs a deny-delete ACL injection",
            partial: &[t(
                E2E_EXEC,
                "fx_exec_14_one_of_five_fails_exit_3_others_complete",
            )],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    fx("FX-EXEC-15", Gate::V1x, "MLW", Mode::Ci, Status::Deferred),
    fx(
        "FX-EXEC-16",
        Gate::Mvp,
        "MLW",
        Mode::Nightly,
        Status::Tracked {
            issue: 71,
            reason: "the owner-only quarantine folder is checked on Unix in CI; reading it as another test user needs the nightly job, and Windows quarantine is not implemented yet",
            partial: &[t(QUAR, "fx_exec_16_quarantine_folder_is_owner_only")],
        },
    )
    .pending_ci("M")
    .missing_os("W"),
    // §3.6 Physical reclaim accuracy (FX-PHY)
    fx(
        "FX-PHY-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_PHY,
                "fx_phy_01_pnpm_hardlinked_node_modules_predicts_zero_measured_recorded",
            ),
            t(
                MEASURE,
                "fx_phy_01_hardlinked_node_modules_predicts_and_measures_zero",
            ),
            t(MEASURE, "fx_phy_01_prediction_explains_shared_bytes"),
            t(
                HARDLINKS,
                "fx_phy_01_pnpm_node_modules_linked_to_store_frees_about_nothing",
            ),
            t(
                MEASURE_UNIT,
                "fx_phy_01_hardlinks_predict_zero_and_measured_zero_agrees",
            ),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-PHY-02",
        Gate::Mvp,
        "M",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_PHY,
                "fx_phy_02_apfs_clone_low_confidence_measured_and_explained",
            ),
            t(MEASURE_UNIT, "fx_phy_02_apfs_clone_low_confidence_explained"),
        ]),
    )
    .pending_ci("M"),
    fx("FX-PHY-03", Gate::V1_0, "M", Mode::Manual, Status::Deferred),
    fx("FX-PHY-04", Gate::V1_0, "L", Mode::Nightly, Status::Deferred),
    fx(
        "FX-PHY-05",
        Gate::Mvp,
        "MW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_PHY, "fx_phy_05_ntfs_compressed_file_uses_allocated_size"),
            t(E2E_PHY, "fx_phy_05_apfs_compressed_file_uses_allocated_size"),
            t(MEASURE_UNIT, "fx_phy_05_compression_uses_allocated_bytes"),
        ]),
    )
    .pending_ci("MW"),
    fx(
        "FX-PHY-06",
        Gate::Mvp,
        "MLW",
        Mode::Nightly,
        Status::Tracked {
            issue: 189,
            reason: "a reference corpus of realistic projects measured against predictions (G3) is slow and belongs in the nightly job, which doesn't exist yet",
            partial: &[],
        },
    ),
    fx(
        "FX-PHY-07",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(TRASH, "fx_phy_07_trashed_item_reported_moved_not_freed"),
            t(
                MEASURE_UNIT,
                "fx_phy_07_trash_and_quarantine_are_moved_not_freed",
            ),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    // §3.7 Quick Clean: package caches (FX-QC)
    fx(
        "FX-QC-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                E2E_QC,
                "fx_qc_01_npm_cache_native_prune_measured_all_invariants",
            ),
            t(QC, "fx_qc_01_npm_prune_exact_argv_env_cwd_and_measured"),
            t(QC, "fx_qc_01_npm_wipe_only_when_requested"),
            t(CACHES, "fx_qc_01_npm_cache_commands_and_wipe_alternative"),
            t(CACHES, "fx_qc_01_npm_env_beats_npmrc_beats_default"),
            t(CACHES, "fx_qc_01_default_layout_linux"),
            t(CACHES, "fx_qc_01_default_layout_macos"),
            t(CACHES, "fx_qc_01_default_layout_windows"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-QC-02",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(
                QC,
                "fx_qc_02_pnpm_store_prune_only_with_partial_reclaim_explanation",
            ),
            t(CACHES, "fx_qc_02_pnpm_store_prune_only_with_shared_bytes"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-QC-03",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(QC, "fx_qc_03_tool_in_use_is_skipped_and_reported"),
            t(QC, "fx_qc_03_in_use_at_run_time_is_skipped"),
            t(QC, "fx_qc_03_real_process_inside_cache_is_detected"),
            t(CACHES, "fx_qc_03_tool_running_is_skipped"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-QC-04",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(QC, "fx_qc_04_symlinked_cache_is_skipped"),
            t(
                QC,
                "fx_qc_04_cache_swapped_for_symlink_after_planning_is_skipped",
            ),
            t(CACHES, "fx_qc_04_cache_symlinked_to_unexpected_location"),
            t(CACHES, "fx_qc_04_folder_above_cache_symlinked_elsewhere"),
            t(CACHES, "fx_qc_04_cache_junction_to_unexpected_location"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-QC-05",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 217,
            reason: "QC-03 says remove the orphaned cache through the safe executor, but Delete is S0-only (D-28) and caches are S1; until decision D-36 (#217) is confirmed and implemented the tool is skipped as not installed and the folder reported for manual removal, never removed",
            partial: &[t(
                QC,
                "fx_qc_05_uninstalled_tool_cache_is_reported_not_removed",
            )],
        },
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-QC-06",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(E2E_QC, "fx_qc_06_planted_npm_never_runs_all_invariants"),
            t(QC, "fx_qc_06_untrusted_npm_binaries_never_run"),
            t(QC, "fx_qc_06_only_untrusted_binaries_means_not_installed"),
            t(QC, "fx_qc_06_trusted_symlink_into_node_modules_is_rejected"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-QC-07",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(QC, "fx_qc_07_project_npmrc_never_applies"),
            t(QC, "fx_qc_07_user_npmrc_cache_at_home_is_refused"),
            t(CACHES, "fx_qc_07_npmrc_cache_at_home_is_refused"),
            t(CACHES, "fx_qc_07_env_redirects_to_home_or_root_are_refused"),
            t(CACHES, "fx_qc_07_project_npmrc_is_never_read"),
        ]),
    )
    .pending_ci(PHASE2_PENDING),
    fx(
        "FX-QC-08",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(QC, "fx_qc_08_unticked_tools_are_not_cleaned")]),
    )
    .pending_ci(PHASE2_PENDING),
    // §3.8 Configuration and rules (FX-CFG)
    fx(
        "FX-CFG-01",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(ENGINE, "fx_cfg_01_03_repo_policy_only_adds_protection"),
            t(
                CORE_CFG,
                "fx_cfg_01_repo_toml_lowering_tiers_to_s0_is_ignored_with_warning",
            ),
        ]),
    ),
    fx(
        "FX-CFG-02",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            CORE_CFG,
            "fx_cfg_02_repo_adding_roots_or_enabling_rules_is_ignored_with_warning",
        )]),
    ),
    fx(
        "FX-CFG-03",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            CORE_CFG,
            "fx_cfg_03_repo_protect_marker_and_config_add_protection_honored",
        )]),
    ),
    fx(
        "FX-CFG-04",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(ENGINE, "fx_cfg_04_user_protect_and_min_tier_are_honored"),
            t(
                CORE_CFG,
                "fx_cfg_04_user_config_protected_path_survives_every_layer",
            ),
        ]),
    ),
    fx(
        "FX-CFG-05",
        Gate::V1x,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[t(
            RULES,
            "fx_cfg_05_user_rule_assigning_s0_is_rejected_at_load",
        )]),
    ),
    fx(
        "FX-CFG-06",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Implemented(&[
            t(RULES, "fx_cfg_06_command_fields_are_rejected_at_load"),
            t(RULES, "fx_cfg_06_command_fields_rejected_anywhere_in_the_file"),
        ]),
    ),
];

/// The registered fixture with this ID.
pub fn lookup(id: &str) -> Option<&'static RegisteredFixture> {
    REGISTRY.iter().find(|f| f.id == id)
}

/// One row of a FIXTURES.md §3 table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocFixture {
    /// Fixture ID, e.g. `FX-GIT-15`.
    pub id: String,
    /// The `### 3.x` heading the row is under.
    pub section: String,
    /// Scenario text.
    pub scenario: String,
    /// OS cell; for the OS-specific sections without an OS column, the implied OS (`W`, `M`,
    /// `L`).
    pub os: String,
    /// Gate cell, parsed.
    pub gate: Option<Gate>,
    /// Mode cell, parsed.
    pub mode: Option<Mode>,
}

impl DocFixture {
    /// Whether the MVP CI gate requires this fixture.
    pub fn is_mvp_ci(&self) -> bool {
        self.gate == Some(Gate::Mvp) && self.mode == Some(Mode::Ci)
    }

    /// The ID prefix (`FX-CLS`).
    pub fn prefix(&self) -> &str {
        self.id
            .rsplit_once('-')
            .map_or(self.id.as_str(), |(p, _)| p)
    }
}

/// Parse every fixture row of the `docs/FIXTURES.md` §3 tables.
///
/// A row is any Markdown table row whose first cell is an `FX-...` ID; columns are found by
/// the table's header (`ID`, `Scenario`, `OS`, `Gate`, `Mode`). Tables without an `OS` column
/// (the OS-specific sections) get the OS implied by the ID prefix.
pub fn parse_fixture_matrix(markdown: &str) -> Vec<DocFixture> {
    let mut out = Vec::new();
    let mut section = String::new();
    let mut header: Vec<String> = Vec::new();
    for line in markdown.lines() {
        let line = line.trim();
        if let Some(h) = line.strip_prefix("### ") {
            section = h.trim().to_owned();
            header.clear();
            continue;
        }
        if !line.starts_with('|') {
            header.clear();
            continue;
        }
        let cells: Vec<String> = line
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().to_owned())
            .collect();
        if cells.first().is_some_and(|c| c == "ID") {
            header = cells;
            continue;
        }
        let Some(id) = cells.first().filter(|c| c.starts_with("FX-")) else {
            continue;
        };
        let col = |name: &str| {
            header
                .iter()
                .position(|h| h == name)
                .and_then(|i| cells.get(i))
                .map(String::as_str)
        };
        let os = match col("OS") {
            Some(os) => os.to_owned(),
            None => implied_os(id).to_owned(),
        };
        out.push(DocFixture {
            id: id.clone(),
            section: section.clone(),
            scenario: col("Scenario").unwrap_or_default().to_owned(),
            os,
            gate: col("Gate").and_then(Gate::parse),
            mode: col("Mode").and_then(Mode::parse),
        });
    }
    out
}

fn implied_os(id: &str) -> &'static str {
    if id.starts_with("FX-WIN-") {
        "W"
    } else if id.starts_with("FX-MAC-") {
        "M"
    } else if id.starts_with("FX-LNX-") {
        "L"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
### 3.1 Classification (FX-CLS)

| ID | Scenario | Expected | OS | Gate | Mode |
|---|---|---|---|---|---|
| FX-CLS-01 | `target/` idle | S0 | MLW | MVP | CI |
| FX-CLS-07 | `.venv` | S1 | MLW | v1.0 | CI |

### 3.10 Windows-specific (FX-WIN)

| ID | Scenario | Expected | Gate | Mode |
|---|---|---|---|---|
| FX-WIN-07 | OneDrive | S2 | v1.0 | Manual |
";

    #[test]
    fn parses_rows_with_and_without_an_os_column() {
        let rows = parse_fixture_matrix(SAMPLE);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "FX-CLS-01");
        assert_eq!(rows[0].section, "3.1 Classification (FX-CLS)");
        assert_eq!(rows[0].os, "MLW");
        assert!(rows[0].is_mvp_ci());
        assert_eq!(rows[0].prefix(), "FX-CLS");
        assert_eq!(rows[1].gate, Some(Gate::V1_0));
        assert!(!rows[1].is_mvp_ci());
        assert_eq!(rows[2].os, "W");
        assert_eq!(rows[2].mode, Some(Mode::Manual));
    }

    #[test]
    fn registry_ids_are_unique() {
        for (i, a) in REGISTRY.iter().enumerate() {
            assert!(
                REGISTRY[i + 1..].iter().all(|b| b.id != a.id),
                "duplicate {}",
                a.id
            );
        }
    }
}
