//! `[auto]`: opt-in unattended auto-policies (PRO-10, decision D-11; threat T-05).
//!
//! An auto-policy lets the scheduled `scull check` clean **S0 items only**, without a click, by
//! moving them into quarantine (never deleting them). Everything here is off by default:
//!
//! * a policy runs only when the user wrote `enabled = true` for it **in the user config file**.
//!   [`resolve`](super::resolve) takes `[auto]` from the user file layer only: no environment
//!   variable, CLI flag, MCP tool or repository file (CFG-03) can set, enable or widen it;
//! * `paused = true` (or any of the other kill switches, see `scull check`) stops every policy;
//! * each policy names one **kind** of item ([`AutoKind`]); there is no kind for S1–S3 items,
//!   package caches cleaned by their tools, worktrees or anything else;
//! * a run is capped ([`AutoConfig::max_items_per_run`], [`AutoConfig::max_gib_per_run`]) and so
//!   is a day ([`AutoConfig::max_gib_per_day`]); values above the hard ceilings
//!   ([`MAX_ITEMS_PER_RUN_CEILING`] …) make every policy refuse to run instead of being
//!   clamped silently.
//!
//! This module only parses and stores the settings; checking them, choosing items and running
//! them is `scull_proactive::auto` and the CLI's `scull check`.
//!
//! ```toml
//! [auto]
//! paused = false            # kill switch: true stops every policy
//! max_items_per_run = 50    # caps, shared by all policies
//! max_gib_per_run = 20
//! max_gib_per_day = 50
//!
//! [[auto.policy]]
//! name = "old-build-outputs"
//! enabled = true            # off unless written here
//! kind = "build_outputs"    # S0 build outputs / dependency folders of idle projects
//! idle = "60d"              # project idle for longer than this (at least 7d)
//! every = "7d"              # at most once per this interval (at least 24h)
//! under = ["~/Projects"]    # optional: only items inside these folders
//! ```

use super::DurationSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default cap on the items one run may clean, across all policies.
pub const DEFAULT_MAX_ITEMS_PER_RUN: u32 = 50;
/// Default cap on the bytes (GiB) one run may clean, across all policies.
pub const DEFAULT_MAX_GIB_PER_RUN: u32 = 20;
/// Default cap on the bytes (GiB) auto-policies may clean within 24 hours.
pub const DEFAULT_MAX_GIB_PER_DAY: u32 = 50;
/// Highest accepted `max_items_per_run`.
pub const MAX_ITEMS_PER_RUN_CEILING: u32 = 1_000;
/// Highest accepted `max_gib_per_run`.
pub const MAX_GIB_PER_RUN_CEILING: u32 = 1_024;
/// Highest accepted `max_gib_per_day`.
pub const MAX_GIB_PER_DAY_CEILING: u32 = 4_096;
/// Shortest accepted idle window: 7 days.
pub const MIN_IDLE: DurationSpec = DurationSpec::from_hours(7 * 24);
/// Shortest accepted interval between two runs of a policy: 24 hours.
pub const MIN_EVERY: DurationSpec = DurationSpec::from_hours(24);

/// `[auto]` in the user config file: opt-in unattended auto-policies (PRO-10, D-11), read from
/// that file only and off unless a policy says `enabled = true`. Only S0 build outputs, only
/// into quarantine, within the caps below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct AutoConfig {
    /// Kill switch: `true` stops every policy.
    pub paused: bool,
    /// At most this many items per run, across all policies.
    pub max_items_per_run: u32,
    /// At most this many GiB per run, across all policies.
    pub max_gib_per_run: u32,
    /// At most this many GiB within any 24 hours, across all policies and runs.
    pub max_gib_per_day: u32,
    /// The policies, each off unless it says `enabled = true`.
    pub policy: Vec<AutoPolicy>,
}

impl Default for AutoConfig {
    fn default() -> Self {
        Self {
            paused: false,
            max_items_per_run: DEFAULT_MAX_ITEMS_PER_RUN,
            max_gib_per_run: DEFAULT_MAX_GIB_PER_RUN,
            max_gib_per_day: DEFAULT_MAX_GIB_PER_DAY,
            policy: Vec::new(),
        }
    }
}

impl AutoConfig {
    /// Whether any policy is switched on (`enabled = true`). Says nothing about whether it may
    /// run: see `scull_proactive::auto`.
    pub fn any_enabled(&self) -> bool {
        self.policy.iter().any(|p| p.enabled)
    }
}

/// One auto-policy (`[[auto.policy]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoPolicy {
    /// A unique name (letters, digits, `.`, `_`, `-`; at most 64 characters).
    pub name: String,
    /// Off unless `true`.
    #[serde(default)]
    pub enabled: bool,
    /// What it cleans.
    pub kind: AutoKind,
    /// Only items whose project has been idle for longer than this (at least [`MIN_IDLE`]).
    pub idle: DurationSpec,
    /// Runs at most once per this interval (at least [`MIN_EVERY`]).
    pub every: DurationSpec,
    /// Only items inside one of these folders (`~` allowed). Empty: anywhere in `scan.roots`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub under: Vec<PathBuf>,
}

/// What an auto-policy may clean. Deliberately small: there is no kind for S1–S3 items,
/// package caches, worktrees or Docker data (D-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutoKind {
    /// S0 build outputs and dependency folders of projects (category `project_artifact`,
    /// e.g. `target/`, `node_modules/`), moved into quarantine.
    BuildOutputs,
}

impl AutoKind {
    /// The serde name.
    pub fn as_str(self) -> &'static str {
        match self {
            AutoKind::BuildOutputs => "build_outputs",
        }
    }
}

/// `[auto]` in a [`ConfigLayer`](super::ConfigLayer). Honored from the user config file only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutoLayer {
    /// See [`AutoConfig::paused`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
    /// See [`AutoConfig::max_items_per_run`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items_per_run: Option<u32>,
    /// See [`AutoConfig::max_gib_per_run`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_gib_per_run: Option<u32>,
    /// See [`AutoConfig::max_gib_per_day`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_gib_per_day: Option<u32>,
    /// See [`AutoConfig::policy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Vec<AutoPolicy>>,
}

impl AutoLayer {
    pub(super) fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The effective settings of this (user file) layer over the defaults.
    pub fn resolve(&self) -> AutoConfig {
        let d = AutoConfig::default();
        AutoConfig {
            paused: self.paused.unwrap_or(d.paused),
            max_items_per_run: self.max_items_per_run.unwrap_or(d.max_items_per_run),
            max_gib_per_run: self.max_gib_per_run.unwrap_or(d.max_gib_per_run),
            max_gib_per_day: self.max_gib_per_day.unwrap_or(d.max_gib_per_day),
            policy: self.policy.clone().unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigLayer, EnvLayer, Origin, resolve};
    use std::path::Path;

    const FILE: &str = "[auto]\nmax_gib_per_day = 10\n\n\
        [[auto.policy]]\nname = \"old\"\nenabled = true\nkind = \"build_outputs\"\n\
        idle = \"60d\"\nevery = \"7d\"\nunder = [\"~/Projects\"]\n\n\
        [[auto.policy]]\nname = \"off\"\nkind = \"build_outputs\"\nidle = \"30d\"\nevery = \"1d\"\n";

    #[test]
    fn off_by_default() {
        let c = AutoConfig::default();
        assert!(!c.paused);
        assert!(c.policy.is_empty());
        assert!(!c.any_enabled());
        assert_eq!(
            (c.max_items_per_run, c.max_gib_per_run, c.max_gib_per_day),
            (50, 20, 50)
        );
    }

    #[test]
    fn parses_from_the_user_file_only() {
        let user = ConfigLayer::from_toml(FILE).expect("parses");
        let r = resolve(&ConfigLayer::default(), &EnvLayer::default(), &user);
        let a = &r.config.auto;
        assert_eq!(a.max_gib_per_day, 10);
        assert_eq!(a.max_gib_per_run, 20, "unset keeps the default");
        assert_eq!(a.policy.len(), 2);
        assert!(a.policy[0].enabled);
        assert!(
            !a.policy[1].enabled,
            "a policy is off unless it says enabled = true"
        );
        assert_eq!(a.policy[0].kind, AutoKind::BuildOutputs);
        assert_eq!(a.policy[0].idle.as_secs(), 60 * 86_400);
        assert_eq!(r.origin("auto.policy"), &[Origin::UserConfig]);
        assert_eq!(r.origin("auto.max_gib_per_run"), &[Origin::Default]);
        let home = Path::new("/home/u");
        let e = r.config.with_home_expanded(home);
        assert_eq!(e.auto.policy[0].under, vec![home.join("Projects")]);

        // The same section in the CLI or environment layer is ignored: only the file enables.
        let r = resolve(&user, &EnvLayer::default(), &ConfigLayer::default());
        assert!(r.config.auto.policy.is_empty());
        assert!(!r.config.auto.any_enabled());
        let env = EnvLayer {
            layer: user.clone(),
            ..EnvLayer::default()
        };
        let r = resolve(&ConfigLayer::default(), &env, &ConfigLayer::default());
        assert!(r.config.auto.policy.is_empty());
        assert_eq!(r.origin("auto.policy"), &[Origin::Default]);
    }

    #[test]
    fn unknown_kinds_and_keys_are_rejected() {
        for bad in [
            "[[auto.policy]]\nname = \"x\"\nkind = \"package_caches\"\nidle = \"60d\"\nevery = \"7d\"\n",
            "[[auto.policy]]\nname = \"x\"\nkind = \"worktrees\"\nidle = \"60d\"\nevery = \"7d\"\n",
            "[[auto.policy]]\nname = \"x\"\nkind = \"build_outputs\"\nidle = \"60d\"\nevery = \"7d\"\ntier = \"S1\"\n",
            "[auto]\nenabled = true\n",
            "[[auto.policy]]\nname = \"x\"\nkind = \"build_outputs\"\nevery = \"7d\"\n",
        ] {
            assert!(ConfigLayer::from_toml(bad).is_err(), "accepted: {bad}");
        }
    }
}
