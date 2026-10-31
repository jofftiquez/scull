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
//! * a registered test function does not exist in the file it names.
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
}

impl RegisteredFixture {
    /// Whether the MVP CI gate requires this fixture.
    pub fn is_mvp_ci(&self) -> bool {
        self.gate == Gate::Mvp && self.mode == Mode::Ci
    }
}

/// Sections of FIXTURES.md §3 whose every fixture must be registered (ID prefix).
pub const COVERED_SECTIONS: &[&str] = &["FX-CLS", "FX-GIT", "FX-FS"];

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
            issue: 83,
            reason: "scan and classify are end-to-end (link counted, never followed, canary bytes not attributed) and removal runs on a std::fs stand-in; the removal half needs the Executor (#67 Unix, #68 Windows); the FX-EXEC fixtures (#83) re-run it through the real executor",
            partial: &[t(
                E2E_FS,
                "fx_fs_01_symlink_to_canary_inside_candidate_link_removed_canary_intact",
            )],
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
            reason: "needs a privileged nightly job (bind/loop mounts); no nightly workflow exists yet",
            partial: &[],
        },
    ),
    fx(
        "FX-FS-05",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 83,
            reason: "bytes attributed once (shared, not freed) is end-to-end; the canary keeping its content and link after the candidate is removed needs the Executor; the removal half needs the Executor (#67 Unix, #68 Windows); the FX-EXEC fixtures (#83) re-run it through the real executor",
            partial: &[
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
            issue: 83,
            reason: "scan and classify with read-only entries are end-to-end and I-3 proves no permission changed; deleting them within the candidate needs the Executor; the removal half needs the Executor (#67 Unix, #68 Windows); the FX-EXEC fixtures (#83) re-run it through the real executor",
            partial: &[t(
                E2E_FS,
                "fx_fs_07_read_only_entries_inside_candidate_permissions_untouched",
            )],
        },
    ),
    fx(
        "FX-FS-08",
        Gate::Mvp,
        "MLW",
        Mode::Ci,
        Status::Tracked {
            issue: 83,
            reason: "the scan reports the denied folder with its reason, marks the item incomplete and never lowers protection (end-to-end, Unix); the item's partial/failed outcome is an Executor result; the removal half needs the Executor (#67 Unix, #68 Windows); the FX-EXEC fixtures (#83) re-run it through the real executor",
            partial: &[
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
            issue: 83,
            reason: "a >260-character candidate path is scanned and classified end-to-end; paths beyond the OS limit (4096 bytes on Linux) need handle-relative traversal and deletion, i.e. the Executor; the removal half needs the Executor (#67 Unix, #68 Windows); the FX-EXEC fixtures (#83) re-run it through the real executor",
            partial: &[t(
                E2E_FS,
                "fx_fs_12_long_path_candidate_is_scanned_and_classified",
            )],
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
