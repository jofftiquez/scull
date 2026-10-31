//! Core types for scull. This crate performs **no I/O**: no filesystem, network, process or
//! clock access. Everything here is plain data with serde and JSON Schema support, shared by the
//! CLI, desktop app and MCP server so the same input always gives the same classification
//! (PRD principle 6).
//!
//! * [`Tier`]: risk tiers S0–S3, ordered by protectiveness (PRD §8.2).
//! * [`Candidate`]: a detected item with evidence, sizes, flags and a proposed [`Action`].
//! * [`Evidence`] / [`TierReason`]: why a candidate got its tier (PRD §8.3).
//! * [`Sizes`]: logical, allocated and reclaimable bytes with [`Confidence`] (PRD §9).
//! * [`ItemId`], [`FileIdentity`], [`ScanId`], [`PlanId`], [`OperationId`]: identifiers.
//! * [`Plan`] / [`PlanItem`] / [`narrow`]: content-hashed, expiring, machine/user-bound plans
//!   that execution can only narrow (PRD §10.4, SAFE-11).
//! * [`ScullError`] / [`ErrorKind`] / [`ExitCode`]: the shared error model and stable exit
//!   codes (CLI-05).
//! * [`config`]: config layers and precedence, untrusted repository policy files and scan-root
//!   checks (PRD §11, CFG-01…04).
//!
//! See `docs/ARCHITECTURE.md` for how these flow through the engine.

mod action;
mod candidate;
pub mod config;
mod error;
mod evidence;
mod id;
mod path;
mod plan;
mod sizes;
mod tier;

pub use action::{Action, Restorability};
pub use candidate::{Candidate, Category, Flags, ProjectRef, Regeneration, RegenerationCost};
pub use error::{ErrorKind, ExitCode, ScullError};
pub use evidence::{Evidence, ReasonCode, TierReason};
pub use id::{FileIdentity, ItemId, OperationId, PlanId, RuleRef, ScanId};
pub use path::FsPath;
pub use plan::{
    CLI_PLAN_TTL_SECS, ContentHash, DESKTOP_PLAN_TTL_SECS, DropReason, MAX_CLOCK_SKEW_SECS,
    MCP_PLAN_TTL_SECS, NarrowResult, PLAN_SCHEMA_VERSION, Plan, PlanItem, PlanOrigin, PlanParams,
    action_allowed_for, narrow,
};
pub use sizes::{Confidence, Reclaimable, SizeKind, Sizes};
pub use tier::{Tier, TierBreakdown};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
