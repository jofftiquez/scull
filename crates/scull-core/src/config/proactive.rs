//! `[proactive]`: alert policies for proactive protection (PRD §16: PRO-01 forecast reserve
//! and alert window, PRO-02 staleness windows, PRO-03 budgets, PRO-04 spikes).
//!
//! These settings only decide when scull **alerts**; nothing here deletes anything or enables
//! unattended cleanup (auto-policies are a separate, later opt-in, D-11). They are read from
//! the user config file (no environment variables or flags), and repository files can't set
//! them (CFG-03).
//!
//! The file format is documented on [`ProactiveConfig`].

use super::DurationSpec;
use crate::candidate::Category;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default reserve: "full" means less than 10 GiB free (PRO-01).
pub const DEFAULT_RESERVE_GIB: u32 = 10;
/// Default forecast alert window: 14 days (PRO-01).
pub const DEFAULT_ALERT_DAYS: u32 = 14;
/// Default staleness window for worktrees: 14 days (PRO-02).
pub const DEFAULT_STALE_WORKTREE: DurationSpec = DurationSpec::from_hours(14 * 24);
/// Default staleness window for build outputs: 30 days (PRO-02).
pub const DEFAULT_STALE_BUILD: DurationSpec = DurationSpec::from_hours(30 * 24);
/// Default staleness window for caches: 90 days (PRO-02).
pub const DEFAULT_STALE_CACHE: DurationSpec = DurationSpec::from_hours(90 * 24);
/// Default spike threshold: 20 GiB (PRO-04 example "20 GB in 24 h").
pub const DEFAULT_SPIKE_GIB: u32 = 20;
/// Default spike window: 24 hours (PRO-04).
pub const DEFAULT_SPIKE_WINDOW: DurationSpec = DurationSpec::from_hours(24);
/// Default relative spike threshold: 50% growth within the window.
pub const DEFAULT_SPIKE_PERCENT: u32 = 50;
/// A relative spike also needs at least this much growth: 5 GiB.
pub const DEFAULT_SPIKE_MIN_GIB: u32 = 5;

/// Alert policies (PRD §16): `[proactive]` in the user config file.
///
/// ```toml
/// [proactive]
/// reserve_gib = 10          # "full" means less free space than this
/// alert_days = 14           # alert when a drive is forecast to be full within this
/// stale_worktree = "14d"    # worktrees idle for longer than this
/// stale_build = "30d"       # build outputs of projects idle for longer than this
/// stale_cache = "90d"       # caches untouched for longer than this
/// spike_gib = 20            # alert when something grows by this much...
/// spike_window = "24h"      # ...within this window
/// spike_percent = 50        # or by this percentage (and at least spike_min_gib); 0 = off
/// spike_min_gib = 5
///
/// [[proactive.budgets]]     # "all Rust target/ <= 50 GB"
/// category = "project_artifact"
/// ecosystem = "rust"
/// max_gib = 50
///
/// [[proactive.budgets]]     # "this project <= 30 GB"
/// path = "~/Projects/monorepo"
/// max_gib = 30
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ProactiveConfig {
    /// "Full" means less free space than this many GiB (PRO-01).
    pub reserve_gib: u32,
    /// Alert when a drive is forecast to drop below the reserve within this many days.
    pub alert_days: u32,
    /// Worktrees idle for longer than this are stale (PRO-02).
    pub stale_worktree: DurationSpec,
    /// Build outputs of projects idle for longer than this are stale (PRO-02).
    pub stale_build: DurationSpec,
    /// Caches untouched for longer than this are stale (PRO-02).
    pub stale_cache: DurationSpec,
    /// Alert when a category or drive grows by this many GiB within `spike_window` (PRO-04).
    pub spike_gib: u32,
    /// The window spikes are measured over.
    pub spike_window: DurationSpec,
    /// Also alert on growth by this percentage within the window (0 turns it off)...
    pub spike_percent: u32,
    /// ...when the growth is at least this many GiB.
    pub spike_min_gib: u32,
    /// Size caps per category, ecosystem or path (PRO-03). None by default.
    pub budgets: Vec<Budget>,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            reserve_gib: DEFAULT_RESERVE_GIB,
            alert_days: DEFAULT_ALERT_DAYS,
            stale_worktree: DEFAULT_STALE_WORKTREE,
            stale_build: DEFAULT_STALE_BUILD,
            stale_cache: DEFAULT_STALE_CACHE,
            spike_gib: DEFAULT_SPIKE_GIB,
            spike_window: DEFAULT_SPIKE_WINDOW,
            spike_percent: DEFAULT_SPIKE_PERCENT,
            spike_min_gib: DEFAULT_SPIKE_MIN_GIB,
            budgets: Vec::new(),
        }
    }
}

/// A size cap (PRO-03): the allocated bytes of every candidate matching all the given filters
/// must stay at or below `max_gib`. A budget with no filter caps everything scull found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Only candidates of this category (e.g. `project_artifact`, `package_cache`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<Category>,
    /// Only candidates whose project ecosystem is this (e.g. `rust`, `node`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecosystem: Option<String>,
    /// Only candidates inside this directory (`~` allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The cap, in GiB.
    pub max_gib: u32,
}

/// `[proactive]` in a [`ConfigLayer`](super::ConfigLayer). See [`ProactiveConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProactiveLayer {
    /// See [`ProactiveConfig::reserve_gib`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_gib: Option<u32>,
    /// See [`ProactiveConfig::alert_days`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_days: Option<u32>,
    /// See [`ProactiveConfig::stale_worktree`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_worktree: Option<DurationSpec>,
    /// See [`ProactiveConfig::stale_build`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_build: Option<DurationSpec>,
    /// See [`ProactiveConfig::stale_cache`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_cache: Option<DurationSpec>,
    /// See [`ProactiveConfig::spike_gib`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spike_gib: Option<u32>,
    /// See [`ProactiveConfig::spike_window`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spike_window: Option<DurationSpec>,
    /// See [`ProactiveConfig::spike_percent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spike_percent: Option<u32>,
    /// See [`ProactiveConfig::spike_min_gib`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spike_min_gib: Option<u32>,
    /// See [`ProactiveConfig::budgets`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budgets: Option<Vec<Budget>>,
}

impl ProactiveLayer {
    pub(super) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigLayer, EnvLayer, Origin, resolve};

    #[test]
    fn defaults_follow_the_prd() {
        let p = ProactiveConfig::default();
        assert_eq!(p.reserve_gib, 10);
        assert_eq!(p.alert_days, 14);
        assert_eq!(p.stale_worktree.as_secs(), 14 * 86_400);
        assert_eq!(p.stale_build.as_secs(), 30 * 86_400);
        assert_eq!(p.stale_cache.as_secs(), 90 * 86_400);
        assert_eq!((p.spike_gib, p.spike_window.as_secs()), (20, 86_400));
        assert!(p.budgets.is_empty());
    }

    #[test]
    fn parses_from_the_user_file() {
        let layer = ConfigLayer::from_toml(
            "[proactive]\nreserve_gib = 20\nstale_cache = \"30d\"\n\n\
             [[proactive.budgets]]\ncategory = \"project_artifact\"\necosystem = \"rust\"\nmax_gib = 50\n\n\
             [[proactive.budgets]]\npath = \"~/Projects/big\"\nmax_gib = 30\n",
        )
        .expect("parses");
        let r = resolve(&ConfigLayer::default(), &EnvLayer::default(), &layer);
        let p = &r.config.proactive;
        assert_eq!(p.reserve_gib, 20);
        assert_eq!(p.stale_cache.as_secs(), 30 * 86_400);
        assert_eq!(p.alert_days, 14, "unset fields keep their defaults");
        assert_eq!(p.budgets.len(), 2);
        assert_eq!(p.budgets[0].category, Some(Category::ProjectArtifact));
        assert_eq!(p.budgets[0].ecosystem.as_deref(), Some("rust"));
        assert_eq!(p.budgets[1].path, Some(PathBuf::from("~/Projects/big")));
        assert_eq!(r.origin("proactive.reserve_gib"), &[Origin::UserConfig]);
        assert_eq!(r.origin("proactive.spike_gib"), &[Origin::Default]);
        let home = PathBuf::from("/home/u");
        let expanded = r.config.with_home_expanded(&home);
        assert_eq!(
            expanded.proactive.budgets[1].path,
            Some(home.join("Projects").join("big"))
        );

        let err = ConfigLayer::from_toml("[proactive]\nreserve = 1\n").expect_err("unknown key");
        assert!(
            err.message.contains("unknown field `reserve`"),
            "{}",
            err.message
        );
        let err =
            ConfigLayer::from_toml("[[proactive.budgets]]\ncategory = \"rust\"\nmax_gib = 1\n")
                .expect_err("bad category");
        assert!(err.message.contains("category"), "{}", err.message);
    }
}
