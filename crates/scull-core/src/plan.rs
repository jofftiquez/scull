//! Plans: the immutable, reviewable list of what an execution may touch (PRD §10.4, SAFE-11).
//!
//! A [`Plan`] is versioned JSON with a content hash, creation and expiry times, the scan it came
//! from, and the machine and user it was made for. Execution follows two rules:
//!
//! 1. **Verify** ([`Plan::verify`]): an edited, expired, foreign or unknown-version plan is
//!    rejected (exit code 4).
//! 2. **Narrow** ([`narrow`]): the engine re-derives candidates from the disk and executes only
//!    plan items that still pass classification at the same or a lower tier, with the same file
//!    identity and an action that is unchanged or safer. Items found on disk but absent from the
//!    plan are never added. A plan only ever *narrows* what can be removed.
//!
//! # The content hash is not a secret
//!
//! [`Plan::content_hash`] is SHA-256 over a canonical JSON encoding of every field except the
//! hash itself (see [`Plan::canonical_json`]). It detects accidental or naive edits to a plan
//! file, and it is the value the audit log and the approval record bind to (SAFE-08, MCP-04).
//! It is **not** a MAC: anyone who can edit the file can recompute it. Authority comes from:
//!
//! * the plan record kept in scull's local database (user-only permissions), which the
//!   executor compares against by plan ID and hash, and
//! * re-derivation plus [`narrow`] at execution, which bounds any plan, forged or not, to items
//!   that are still classified as removable right now.
//!
//! This module has no clock and no I/O. Every time-dependent function takes `now_unix`
//! (seconds since the Unix epoch, UTC); machine ID and user name are passed in by the caller.

use crate::action::Action;
use crate::candidate::{Candidate, Category};
use crate::error::{ErrorKind, ScullError};
use crate::id::{FileIdentity, ItemId, PlanId, RuleRef, ScanId, hex};
use crate::path::FsPath;
use crate::tier::Tier;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The plan schema version this build writes and accepts.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

/// Default (and maximum) lifetime of a CLI plan: 24 hours (PRD §10.4).
pub const CLI_PLAN_TTL_SECS: i64 = 24 * 60 * 60;

/// Default (and maximum) lifetime of an MCP plan: 15 minutes (PRD §10.4, §13.3).
pub const MCP_PLAN_TTL_SECS: i64 = 15 * 60;

/// Default (and maximum) lifetime of a desktop plan: 1 hour. The PRD does not fix this value;
/// it sits between the MCP and CLI defaults because the desktop review is interactive.
pub const DESKTOP_PLAN_TTL_SECS: i64 = 60 * 60;

/// How far in the future a plan's creation time may be before it is rejected, to tolerate small
/// clock adjustments between planning and execution.
pub const MAX_CLOCK_SKEW_SECS: i64 = 5 * 60;

/// Which surface created a plan. Determines the default and maximum lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanOrigin {
    /// `scull clean --plan-out` and friends.
    Cli,
    /// `prepare_cleanup` over MCP (approved out of band, §13.3).
    Mcp,
    /// The desktop Cleanup Review screen.
    Desktop,
}

impl PlanOrigin {
    /// Default lifetime in seconds. This is also the maximum: a plan's lifetime can only be
    /// shortened, never extended, and [`Plan::verify`] rejects a plan whose recorded lifetime
    /// exceeds it.
    pub const fn max_ttl_secs(self) -> i64 {
        match self {
            PlanOrigin::Cli => CLI_PLAN_TTL_SECS,
            PlanOrigin::Mcp => MCP_PLAN_TTL_SECS,
            PlanOrigin::Desktop => DESKTOP_PLAN_TTL_SECS,
        }
    }
}

/// SHA-256 of a plan's canonical JSON, as 64 lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentHash(String);

impl JsonSchema for ContentHash {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ContentHash".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "SHA-256 of the plan's canonical JSON, lowercase hex.",
            "pattern": "^[0-9a-f]{64}$"
        })
    }
}

impl ContentHash {
    /// Length of the hex string.
    pub const HEX_LEN: usize = 64;

    /// The hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn of(bytes: &[u8]) -> Self {
        Self(hex(&Sha256::digest(bytes)))
    }
}

impl TryFrom<String> for ContentHash {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == Self::HEX_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s))
        } else {
            Err(format!(
                "content hash must be {} lowercase hex characters",
                Self::HEX_LEN
            ))
        }
    }
}

impl From<ContentHash> for String {
    fn from(v: ContentHash) -> String {
        v.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One item of a plan: what was reviewed and what will be done to it.
///
/// The item is identified by [`ItemId`] *and* [`FileIdentity`]; `path` is for display and
/// audit only. Execution never acts on the path from a plan (see [`narrow`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    /// Stable item ID (identity + path).
    pub item_id: ItemId,
    /// File identity at planning time.
    pub identity: FileIdentity,
    /// Exact path at planning time (display and audit only).
    pub path: FsPath,
    /// Category, for grouping in the preview.
    pub category: Category,
    /// Rule that produced the candidate.
    pub rule: RuleRef,
    /// Tier at planning time. Never S3.
    pub tier: Tier,
    /// The action that was reviewed. Always mutating and allowed for `tier` (D-3).
    pub action: Action,
    /// Estimated reclaimable bytes at planning time.
    pub reclaimable_bytes: u64,
}

impl PlanItem {
    /// Build a plan item from a candidate. Refuses inconsistent candidates, S3 candidates
    /// (invariant I-1) and actions not allowed for the tier (D-3), with
    /// [`ErrorKind::SafetyBlocked`].
    pub fn from_candidate(c: &Candidate) -> Result<Self, ScullError> {
        if !c.is_consistent() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "candidate is inconsistent (tier, reasons and action disagree)",
            )
            .with_item(c.id.clone()));
        }
        let item = Self {
            item_id: c.id.clone(),
            identity: c.identity,
            path: c.path.clone(),
            category: c.category,
            rule: c.rule.clone(),
            tier: c.tier,
            action: c.action.clone(),
            reclaimable_bytes: c.sizes.reclaimable.bytes,
        };
        item.check().map_err(|m| {
            ScullError::new(ErrorKind::SafetyBlocked, m).with_item(item.item_id.clone())
        })?;
        Ok(item)
    }

    /// Structural check shared by [`Plan::new`], [`Plan::verify`] and [`narrow`].
    fn check(&self) -> Result<(), String> {
        if !self.tier.is_actionable() {
            return Err("S3 (protected) items can never be part of a plan".into());
        }
        if !action_allowed_for(self.tier, &self.action) {
            return Err(format!(
                "action {:?} is not allowed for tier {} (D-3)",
                self.action, self.tier
            ));
        }
        Ok(())
    }
}

/// Whether `action` may be planned or executed for an item at `tier`: the action must be
/// mutating, the tier must not be S3, and `Delete` is only for S0 (D-3).
pub fn action_allowed_for(tier: Tier, action: &Action) -> bool {
    tier.is_actionable() && action.is_mutating() && action.allowed_for(tier)
}

/// Inputs to [`Plan::new`] other than the items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanParams {
    /// Plan ID (generated outside `scull-core`).
    pub id: PlanId,
    /// The scan the items came from.
    pub scan_id: ScanId,
    /// Creation time, Unix seconds (UTC).
    pub created_unix: i64,
    /// Lifetime in seconds; `None` for the origin's default. Must be in
    /// `1..=origin.max_ttl_secs()`.
    pub ttl_secs: Option<i64>,
    /// ID of the machine the plan is bound to.
    pub machine_id: String,
    /// User the plan is bound to.
    pub user: String,
    /// Which surface created the plan.
    pub origin: PlanOrigin,
    /// SAFE-09: `scull_storage::Store::operations_watermark`, read **before** the scan the
    /// items come from started: every operation recorded after it invalidates the plan.
    /// Required for a plan built from a scan; `None` only for a plan that comes from no scan,
    /// which then falls back to its creation time (operations started at or after it).
    pub operations_watermark: Option<u64>,
}

/// An immutable cleanup plan (PRD §10.4, SAFE-11).
///
/// Construct with [`Plan::new`] (which computes the hash) or by deserializing a plan file
/// (which rejects unknown fields). Either way, call [`Plan::verify`] before trusting it, and
/// [`narrow`] it against freshly derived candidates before executing anything.
///
/// ```
/// use scull_core::*;
///
/// let identity = FileIdentity::new(1, 42);
/// let path = "/home/me/proj/target";
/// let reasons = vec![TierReason::new(Tier::S0, ReasonCode::Regenerable, "Cargo.lock present")];
/// let candidate = Candidate {
///     id: ItemId::derive(identity, path.as_bytes()),
///     path: FsPath::from(path),
///     identity,
///     category: Category::ProjectArtifact,
///     artifact: "Cargo build output".into(),
///     rule: RuleRef { id: "rust.target".into(), version: 1 },
///     project: None,
///     tier: Tier::S0,
///     reasons,
///     evidence: vec![],
///     sizes: Sizes::default(),
///     flags: Flags::default(),
///     regeneration: Regeneration::default(),
///     action: Action::Delete,
/// };
/// let params = PlanParams {
///     id: PlanId::from_suffix("01J").unwrap(),
///     scan_id: ScanId::from_suffix("01J").unwrap(),
///     created_unix: 1_800_000_000,
///     ttl_secs: None,
///     machine_id: "machine-a".into(),
///     user: "me".into(),
///     origin: PlanOrigin::Cli,
///     operations_watermark: Some(0),
/// };
/// let plan = Plan::new(params, vec![PlanItem::from_candidate(&candidate).unwrap()]).unwrap();
/// assert!(plan.verify(1_800_000_060, "machine-a", "me").is_ok());
/// assert_eq!(
///     plan.verify(1_800_000_060, "machine-b", "me").unwrap_err().kind,
///     ErrorKind::PlanForeign
/// );
///
/// let narrowed = narrow(&plan, &[candidate]);
/// assert_eq!(narrowed.executable.len(), 1);
/// assert!(narrow(&plan, &[]).executable.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    schema_version: u32,
    id: PlanId,
    created_unix: i64,
    expires_unix: i64,
    scan_id: ScanId,
    machine_id: String,
    user: String,
    origin: PlanOrigin,
    /// SAFE-09: the position of the last recorded operation in scull's database when the scan
    /// the items came from started (operations are append-only, so positions only grow). An
    /// operation recorded after this position touched the targets after the scan saw them and
    /// invalidates the plan. Absent in plans that predate it; those use the creation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operations_watermark: Option<u64>,
    items: Vec<PlanItem>,
    content_hash: ContentHash,
}

/// JSON key of the hash field, excluded from the canonical encoding.
const HASH_KEY: &str = "content_hash";

impl Plan {
    /// Build a plan and compute its content hash. Items are sorted by item ID.
    ///
    /// Refuses ([`ErrorKind::SafetyBlocked`]) any S3 item or action not allowed for its tier;
    /// refuses ([`ErrorKind::Usage`]) duplicate item IDs, an empty machine ID or user, and a
    /// lifetime outside `1..=origin.max_ttl_secs()`.
    pub fn new(params: PlanParams, mut items: Vec<PlanItem>) -> Result<Self, ScullError> {
        let max = params.origin.max_ttl_secs();
        let ttl = params.ttl_secs.unwrap_or(max);
        if ttl <= 0 || ttl > max {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!("plan lifetime must be between 1 and {max} seconds, got {ttl}"),
            ));
        }
        let expires_unix = params
            .created_unix
            .checked_add(ttl)
            .ok_or_else(|| ScullError::new(ErrorKind::Usage, "plan expiry time is out of range"))?;
        if params.machine_id.is_empty() || params.user.is_empty() {
            return Err(ScullError::new(
                ErrorKind::Usage,
                "a plan must be bound to a machine ID and a user",
            ));
        }
        for item in &items {
            item.check().map_err(|m| {
                ScullError::new(ErrorKind::SafetyBlocked, m).with_item(item.item_id.clone())
            })?;
        }
        items.sort_by(|a, b| a.item_id.cmp(&b.item_id));
        if let Some(w) = items.windows(2).find(|w| w[0].item_id == w[1].item_id) {
            return Err(ScullError::new(ErrorKind::Usage, "duplicate item in plan")
                .with_item(w[0].item_id.clone()));
        }

        let mut plan = Self {
            schema_version: PLAN_SCHEMA_VERSION,
            id: params.id,
            created_unix: params.created_unix,
            expires_unix,
            scan_id: params.scan_id,
            machine_id: params.machine_id,
            user: params.user,
            origin: params.origin,
            operations_watermark: params.operations_watermark,
            items,
            content_hash: ContentHash("0".repeat(ContentHash::HEX_LEN)),
        };
        plan.content_hash = plan.compute_hash()?;
        Ok(plan)
    }

    /// SAFE-09: the last recorded operation's position in scull's database when the scan the
    /// items came from started. Every operation recorded after it invalidates the plan. `None`
    /// for plans without one: then operations started at or after [`Plan::created_unix`] do.
    pub fn operations_watermark(&self) -> Option<u64> {
        self.operations_watermark
    }

    /// Schema version the plan was written with.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Plan ID.
    pub fn id(&self) -> &PlanId {
        &self.id
    }

    /// Creation time, Unix seconds (UTC).
    pub fn created_unix(&self) -> i64 {
        self.created_unix
    }

    /// Expiry time, Unix seconds (UTC). The plan is expired when `now_unix >= expires_unix`.
    pub fn expires_unix(&self) -> i64 {
        self.expires_unix
    }

    /// The scan the items came from.
    pub fn scan_id(&self) -> &ScanId {
        &self.scan_id
    }

    /// Machine the plan is bound to.
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// User the plan is bound to.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Which surface created the plan.
    pub fn origin(&self) -> PlanOrigin {
        self.origin
    }

    /// The planned items (sorted by item ID when built with [`Plan::new`]).
    pub fn items(&self) -> &[PlanItem] {
        &self.items
    }

    /// The stored content hash. Detects edits; it is not a secret (see the module docs).
    pub fn content_hash(&self) -> &ContentHash {
        &self.content_hash
    }

    /// The canonical JSON encoding the content hash is computed over:
    ///
    /// * every field of the plan except `content_hash`;
    /// * object keys sorted by their UTF-8 bytes, at every level;
    /// * `items` sorted by `item_id` (ties broken by the item's canonical encoding);
    /// * no whitespace; strings escape only `"`, `\` and control characters
    ///   (as `\b \t \n \f \r` or `\u00XX`); integers in plain decimal.
    ///
    /// The encoding is built from the serialized value, so it does not depend on struct field
    /// order or on how the plan file was formatted.
    pub fn canonical_json(&self) -> Result<String, ScullError> {
        let mut value = serde_json::to_value(self).map_err(|e| {
            ScullError::new(ErrorKind::Internal, format!("cannot serialize plan: {e}"))
        })?;
        let Value::Object(map) = &mut value else {
            return Err(ScullError::new(
                ErrorKind::Internal,
                "plan did not serialize to an object",
            ));
        };
        map.remove(HASH_KEY);
        if let Some(Value::Array(items)) = map.get_mut("items") {
            let mut keyed: Vec<(String, String, Value)> = items
                .drain(..)
                .map(|item| {
                    let id = item
                        .get("item_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let mut enc = String::new();
                    write_canonical(&item, &mut enc);
                    (id, enc, item)
                })
                .collect();
            keyed.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
            items.extend(keyed.into_iter().map(|(_, _, v)| v));
        }
        let mut out = String::new();
        write_canonical(&value, &mut out);
        Ok(out)
    }

    /// SHA-256 of [`Plan::canonical_json`]. Equal for any two plans with the same content,
    /// regardless of item order.
    pub fn compute_hash(&self) -> Result<ContentHash, ScullError> {
        Ok(ContentHash::of(self.canonical_json()?.as_bytes()))
    }

    /// Check that the plan may be used on this machine, by this user, at `now_unix`.
    ///
    /// Checks run in this order, and the first failure is returned:
    ///
    /// 1. [`ErrorKind::PlanTampered`]: unknown schema version; empty machine ID or user;
    ///    expiry not after creation, or a lifetime longer than the origin allows; an S3 item,
    ///    an action not allowed for its tier (D-3), or a duplicate item; content hash mismatch.
    /// 2. [`ErrorKind::PlanForeign`]: `machine_id` or `user` differs from the plan's.
    /// 3. [`ErrorKind::PlanExpired`]: `now_unix >= expires_unix`, or the plan was created more
    ///    than [`MAX_CLOCK_SKEW_SECS`] in the future.
    ///
    /// Passing `verify` does not authorize execution by itself: the executor must also match
    /// the plan against its database record and [`narrow`] it against fresh candidates.
    pub fn verify(&self, now_unix: i64, machine_id: &str, user: &str) -> Result<(), ScullError> {
        self.verify_integrity()?;
        if self.machine_id != machine_id || self.user != user {
            return Err(ScullError::new(
                ErrorKind::PlanForeign,
                "plan was created on another machine or by another user",
            ));
        }
        if now_unix >= self.expires_unix {
            return Err(ScullError::new(
                ErrorKind::PlanExpired,
                format!(
                    "plan expired {} seconds ago; create a new plan",
                    i128::from(now_unix) - i128::from(self.expires_unix)
                ),
            ));
        }
        if i128::from(self.created_unix) > i128::from(now_unix) + i128::from(MAX_CLOCK_SKEW_SECS) {
            return Err(ScullError::new(
                ErrorKind::PlanExpired,
                "plan creation time is in the future; check the system clock",
            ));
        }
        Ok(())
    }

    /// The structural and hash checks of [`Plan::verify`] (step 1 only).
    fn verify_integrity(&self) -> Result<(), ScullError> {
        let tampered = |m: String| ScullError::new(ErrorKind::PlanTampered, m);
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(tampered(format!(
                "unsupported plan schema version {} (this scull reads version {})",
                self.schema_version, PLAN_SCHEMA_VERSION
            )));
        }
        if self.machine_id.is_empty() || self.user.is_empty() {
            return Err(tampered("plan has no machine or user binding".into()));
        }
        let lifetime = i128::from(self.expires_unix) - i128::from(self.created_unix);
        if lifetime <= 0 || lifetime > i128::from(self.origin.max_ttl_secs()) {
            return Err(tampered(format!(
                "plan lifetime of {lifetime} seconds is not allowed for {:?} plans",
                self.origin
            )));
        }
        let mut seen = BTreeSet::new();
        for item in &self.items {
            item.check()
                .map_err(|m| tampered(m).with_item(item.item_id.clone()))?;
            if !seen.insert(&item.item_id) {
                return Err(
                    tampered("duplicate item in plan".into()).with_item(item.item_id.clone())
                );
            }
        }
        let actual = self.compute_hash()?;
        if actual != self.content_hash {
            return Err(tampered(
                "plan content does not match its hash; the plan file was edited".into(),
            ));
        }
        Ok(())
    }
}

/// Write `v` in canonical form (sorted keys, no whitespace). See [`Plan::canonical_json`].
fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_canonical_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_str(k, out);
                out.push(':');
                write_canonical(&m[k], out);
            }
            out.push('}');
        }
    }
}

fn write_canonical_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Why a plan item was left out of the executable set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum DropReason {
    /// The plan item itself is invalid (S3, or an action not allowed for its tier).
    InvalidPlanItem,
    /// The item appears more than once in the plan; only the first occurrence is kept.
    DuplicateInPlan,
    /// No fresh candidate has this item ID (gone, moved, or no longer detected).
    Missing,
    /// More than one fresh candidate has this item ID.
    Ambiguous,
    /// The object at the path was replaced (different device/inode or volume/file ID).
    IdentityChanged,
    /// The fresh candidate's path differs from the planned path.
    PathChanged,
    /// The fresh candidate is internally inconsistent.
    InconsistentCandidate,
    /// The item is now S3 (protected) (FX-EXEC-10).
    NowProtected,
    /// The item's tier is now more protective than when it was planned.
    TierRaised {
        /// Tier at planning time.
        planned: Tier,
        /// Tier now.
        fresh: Tier,
    },
    /// The proposed action changed to something that is not the same or safer.
    ActionChanged,
}

impl DropReason {
    /// Stable machine code, identical to the serialized `reason` tag.
    pub fn code(&self) -> &'static str {
        match self {
            DropReason::InvalidPlanItem => "invalid_plan_item",
            DropReason::DuplicateInPlan => "duplicate_in_plan",
            DropReason::Missing => "missing",
            DropReason::Ambiguous => "ambiguous",
            DropReason::IdentityChanged => "identity_changed",
            DropReason::PathChanged => "path_changed",
            DropReason::InconsistentCandidate => "inconsistent_candidate",
            DropReason::NowProtected => "now_protected",
            DropReason::TierRaised { .. } => "tier_raised",
            DropReason::ActionChanged => "action_changed",
        }
    }
}

/// The result of [`narrow`]: what may be executed now, and what was dropped and why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NarrowResult {
    /// Plan items that still qualify, in plan order. Always a subset of the plan's items.
    pub executable: Vec<PlanItem>,
    /// Plan items that were dropped, in plan order, with the reason.
    pub dropped: Vec<(ItemId, DropReason)>,
}

/// Rank of the filesystem actions by how recoverable they are: quarantine (restorable by scull)
/// is safest, then Trash, then delete. Other actions are not comparable.
fn fs_action_rank(a: &Action) -> Option<u8> {
    match a {
        Action::Quarantine => Some(0),
        Action::Trash => Some(1),
        Action::Delete => Some(2),
        _ => None,
    }
}

/// The action to execute, given the reviewed action and the freshly proposed one: the same
/// action, or the safer of two filesystem actions. Never an action less safe than `planned`.
fn executed_action(planned: &Action, fresh: &Action) -> Option<Action> {
    if !planned.is_mutating() || !fresh.is_mutating() {
        return None;
    }
    if planned == fresh {
        return Some(planned.clone());
    }
    match (fs_action_rank(planned), fs_action_rank(fresh)) {
        (Some(p), Some(f)) if f <= p => Some(fresh.clone()),
        (Some(_), Some(_)) => Some(planned.clone()),
        _ => None,
    }
}

/// Intersect a plan with freshly derived candidates (SAFE-11). The result can only shrink the
/// plan: candidates in `fresh` that are not in the plan are never added.
///
/// A plan item is kept only if **all** hold:
///
/// * the item itself is valid (not S3; action allowed for its tier) and not a duplicate;
/// * exactly one fresh candidate has the same [`ItemId`], with the same [`FileIdentity`] and
///   path;
/// * that candidate is consistent ([`Candidate::is_consistent`]), not S3, and its tier is the
///   same as or less protective than the planned tier;
/// * its action is the same as the planned one, or both are filesystem actions (in which case
///   the safer of the two runs: quarantine, then Trash, then delete).
///
/// Each kept item is the planned item with `tier`, `action` and `reclaimable_bytes` updated to
/// the fresh tier, the executed action and the fresh estimate. Call [`Plan::verify`] first;
/// `narrow` re-checks item validity anyway so it is safe on an unverified plan.
pub fn narrow(plan: &Plan, fresh: &[Candidate]) -> NarrowResult {
    let mut by_id: BTreeMap<&ItemId, Vec<&Candidate>> = BTreeMap::new();
    for c in fresh {
        by_id.entry(&c.id).or_default().push(c);
    }

    let mut result = NarrowResult::default();
    let mut seen = BTreeSet::new();
    for item in &plan.items {
        match narrow_item(item, &by_id, &mut seen) {
            Ok(kept) => result.executable.push(kept),
            Err(reason) => result.dropped.push((item.item_id.clone(), reason)),
        }
    }
    result
}

fn narrow_item<'a>(
    item: &'a PlanItem,
    by_id: &BTreeMap<&ItemId, Vec<&Candidate>>,
    seen: &mut BTreeSet<&'a ItemId>,
) -> Result<PlanItem, DropReason> {
    if item.check().is_err() {
        return Err(DropReason::InvalidPlanItem);
    }
    if !seen.insert(&item.item_id) {
        return Err(DropReason::DuplicateInPlan);
    }
    let fresh = match by_id.get(&item.item_id).map(Vec::as_slice) {
        None | Some([]) => return Err(DropReason::Missing),
        Some([one]) => *one,
        Some(_) => return Err(DropReason::Ambiguous),
    };
    if fresh.identity != item.identity {
        return Err(DropReason::IdentityChanged);
    }
    if fresh.path != item.path {
        return Err(DropReason::PathChanged);
    }
    if !fresh.is_consistent() {
        return Err(DropReason::InconsistentCandidate);
    }
    if !fresh.tier.is_actionable() {
        return Err(DropReason::NowProtected);
    }
    if fresh.tier > item.tier {
        return Err(DropReason::TierRaised {
            planned: item.tier,
            fresh: fresh.tier,
        });
    }
    let action = executed_action(&item.action, &fresh.action)
        .filter(|a| action_allowed_for(fresh.tier, a))
        .ok_or(DropReason::ActionChanged)?;
    Ok(PlanItem {
        tier: fresh.tier,
        action,
        reclaimable_bytes: fresh.sizes.reclaimable.bytes,
        ..item.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Flags, Regeneration};
    use crate::evidence::{ReasonCode, TierReason};
    use crate::sizes::{Confidence, Reclaimable, Sizes};

    const T0: i64 = 1_800_000_000;
    const MACHINE: &str = "machine-a";
    const USER: &str = "dev";

    fn candidate(n: u128, tier: Tier, action: Action) -> Candidate {
        let identity = FileIdentity::new(7, n);
        let path = format!("/work/p{n}/target");
        let reasons = vec![TierReason::new(tier, ReasonCode::Regenerable, "test")];
        Candidate {
            id: ItemId::derive(identity, path.as_bytes()),
            path: FsPath::from(path.as_str()),
            identity,
            category: Category::ProjectArtifact,
            artifact: "Cargo build output".into(),
            rule: RuleRef {
                id: "rust.target".into(),
                version: 1,
            },
            project: None,
            tier,
            reasons,
            evidence: vec![],
            sizes: Sizes {
                logical: 100,
                allocated: 100,
                reclaimable: Reclaimable {
                    bytes: 100 + n as u64,
                    confidence: Confidence::High,
                    shared_bytes: 0,
                },
            },
            flags: Flags::default(),
            regeneration: Regeneration::default(),
            action,
        }
    }

    fn params(origin: PlanOrigin) -> PlanParams {
        PlanParams {
            id: PlanId::from_suffix("01TEST").unwrap(),
            scan_id: ScanId::from_suffix("01SCAN").unwrap(),
            created_unix: T0,
            ttl_secs: None,
            machine_id: MACHINE.into(),
            user: USER.into(),
            origin,
            operations_watermark: None,
        }
    }

    fn sample() -> (Plan, Vec<Candidate>) {
        let cands = vec![
            candidate(1, Tier::S0, Action::Delete),
            candidate(2, Tier::S1, Action::Quarantine),
            candidate(3, Tier::S2, Action::Trash),
        ];
        let items = cands
            .iter()
            .map(|c| PlanItem::from_candidate(c).unwrap())
            .collect();
        (Plan::new(params(PlanOrigin::Cli), items).unwrap(), cands)
    }

    fn kind(r: Result<(), ScullError>) -> ErrorKind {
        r.unwrap_err().kind
    }

    #[test]
    fn fresh_plan_verifies() {
        let (plan, _) = sample();
        assert_eq!(plan.schema_version(), PLAN_SCHEMA_VERSION);
        assert_eq!(plan.expires_unix(), T0 + CLI_PLAN_TTL_SECS);
        assert!(plan.verify(T0, MACHINE, USER).is_ok());
        assert!(
            plan.verify(T0 + CLI_PLAN_TTL_SECS - 1, MACHINE, USER)
                .is_ok()
        );
        let back: Plan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
        assert_eq!(back, plan);
        assert!(back.verify(T0, MACHINE, USER).is_ok());
    }

    #[test]
    fn default_ttls_follow_prd() {
        assert_eq!(CLI_PLAN_TTL_SECS, 86_400);
        assert_eq!(MCP_PLAN_TTL_SECS, 900);
        let (_, cands) = sample();
        let items = vec![PlanItem::from_candidate(&cands[0]).unwrap()];
        let mcp = Plan::new(params(PlanOrigin::Mcp), items).unwrap();
        assert_eq!(mcp.expires_unix() - mcp.created_unix(), 900);
        assert!(mcp.verify(T0 + 899, MACHINE, USER).is_ok());
        assert_eq!(
            kind(mcp.verify(T0 + 900, MACHINE, USER)),
            ErrorKind::PlanExpired
        );
    }

    #[test]
    fn expired_plan_is_rejected_fx_exec_08() {
        let (plan, _) = sample();
        let e = plan
            .verify(T0 + CLI_PLAN_TTL_SECS, MACHINE, USER)
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::PlanExpired);
        assert_eq!(e.exit_code().as_i32(), 4);
        assert_eq!(
            kind(plan.verify(i64::MAX, MACHINE, USER)),
            ErrorKind::PlanExpired
        );
    }

    #[test]
    fn plan_from_the_future_is_rejected() {
        let (plan, _) = sample();
        assert!(plan.verify(T0 - MAX_CLOCK_SKEW_SECS, MACHINE, USER).is_ok());
        assert_eq!(
            kind(plan.verify(T0 - MAX_CLOCK_SKEW_SECS - 1, MACHINE, USER)),
            ErrorKind::PlanExpired
        );
        assert_eq!(
            kind(plan.verify(i64::MIN, MACHINE, USER)),
            ErrorKind::PlanExpired
        );
    }

    #[test]
    fn foreign_plan_is_rejected_fx_exec_09() {
        let (plan, _) = sample();
        for (m, u) in [("machine-b", USER), (MACHINE, "root"), ("", ""), ("x", "y")] {
            let e = plan.verify(T0, m, u).unwrap_err();
            assert_eq!(e.kind, ErrorKind::PlanForeign, "{m}/{u}");
            assert_eq!(e.exit_code().as_i32(), 4);
        }
    }

    fn edited(plan: &Plan, f: impl FnOnce(&mut Value)) -> Result<Plan, serde_json::Error> {
        let mut v = serde_json::to_value(plan).unwrap();
        f(&mut v);
        serde_json::from_value(v)
    }

    #[test]
    fn plan_edited_to_add_a_path_is_rejected_fx_exec_07() {
        let (plan, _) = sample();
        let extra = candidate(99, Tier::S0, Action::Delete);
        let mut extra_item =
            serde_json::to_value(PlanItem::from_candidate(&extra).unwrap()).unwrap();
        extra_item["path"] = "/home/dev/Documents".into();
        let p = edited(&plan, |v| {
            v["items"].as_array_mut().unwrap().push(extra_item)
        })
        .unwrap();
        let e = p.verify(T0, MACHINE, USER).unwrap_err();
        assert_eq!(e.kind, ErrorKind::PlanTampered);
        assert_eq!(e.exit_code().as_i32(), 4);
    }

    #[test]
    fn edited_fields_are_rejected() {
        let (plan, _) = sample();
        type Edit = Box<dyn Fn(&mut Value)>;
        let edits: Vec<Edit> = vec![
            Box::new(|v| v["items"][0]["path"] = "/home/dev/.ssh".into()),
            Box::new(|v| {
                let t = if v["items"][1]["tier"] == "S0" {
                    "S1"
                } else {
                    "S0"
                };
                v["items"][1]["tier"] = t.into();
            }),
            Box::new(|v| {
                let a = if v["items"][1]["action"]["type"] == "trash" {
                    "quarantine"
                } else {
                    "trash"
                };
                v["items"][1]["action"] = serde_json::json!({ "type": a });
            }),
            Box::new(|v| v["items"][0]["reclaimable_bytes"] = 1.into()),
            Box::new(|v| v["expires_unix"] = (T0 + 60).into()),
            Box::new(|v| v["created_unix"] = (T0 + 60).into()),
            Box::new(|v| v["machine_id"] = "machine-b".into()),
            Box::new(|v| v["user"] = "root".into()),
            Box::new(|v| v["scan_id"] = "scan_OTHER".into()),
            Box::new(|v| v["id"] = "plan_OTHER".into()),
            Box::new(|v| v["origin"] = "mcp".into()),
            Box::new(|v| {
                v["items"].as_array_mut().unwrap().remove(0);
            }),
            Box::new(|v| v["content_hash"] = "0".repeat(64).into()),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let p = edited(&plan, edit).unwrap();
            assert_ne!(p, plan, "edit {i} was a no-op");
            assert_eq!(
                kind(p.verify(T0, MACHINE, USER)),
                ErrorKind::PlanTampered,
                "edit {i}"
            );
        }
    }

    #[test]
    fn plan_with_recomputed_hash_but_s3_item_is_rejected() {
        let (plan, _) = sample();
        let mut p = edited(&plan, |v| {
            v["items"][1]["tier"] = "S3".into();
            v["items"][1]["action"] = serde_json::json!({"type": "none"});
        })
        .unwrap();
        p.content_hash = p.compute_hash().unwrap();
        assert_eq!(kind(p.verify(T0, MACHINE, USER)), ErrorKind::PlanTampered);
    }

    #[test]
    fn plan_with_recomputed_hash_but_delete_at_s1_is_rejected() {
        let (plan, _) = sample();
        let id = PlanItem::from_candidate(&candidate(2, Tier::S1, Action::Quarantine))
            .unwrap()
            .item_id;
        let mut p = plan.clone();
        let item = p.items.iter_mut().find(|i| i.item_id == id).unwrap();
        item.action = Action::Delete;
        p.content_hash = p.compute_hash().unwrap();
        let e = p.verify(T0, MACHINE, USER).unwrap_err();
        assert_eq!(e.kind, ErrorKind::PlanTampered);
        assert_eq!(e.item_id, Some(id));
    }

    #[test]
    fn overlong_lifetime_is_rejected_even_with_valid_hash() {
        let (plan, _) = sample();
        let mut p = plan.clone();
        p.expires_unix = T0 + CLI_PLAN_TTL_SECS + 1;
        p.content_hash = p.compute_hash().unwrap();
        assert_eq!(kind(p.verify(T0, MACHINE, USER)), ErrorKind::PlanTampered);
        let mut p = plan;
        p.expires_unix = p.created_unix;
        p.content_hash = p.compute_hash().unwrap();
        assert_eq!(kind(p.verify(T0, MACHINE, USER)), ErrorKind::PlanTampered);
    }

    #[test]
    fn duplicate_items_are_rejected() {
        let (plan, cands) = sample();
        let mut p = plan.clone();
        p.items.push(PlanItem::from_candidate(&cands[0]).unwrap());
        p.content_hash = p.compute_hash().unwrap();
        assert_eq!(kind(p.verify(T0, MACHINE, USER)), ErrorKind::PlanTampered);

        let item = PlanItem::from_candidate(&cands[0]).unwrap();
        let e = Plan::new(params(PlanOrigin::Cli), vec![item.clone(), item]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let (plan, _) = sample();
        for version in [0u32, 2, u32::MAX] {
            let mut p = plan.clone();
            p.schema_version = version;
            p.content_hash = p.compute_hash().unwrap();
            let e = p.verify(T0, MACHINE, USER).unwrap_err();
            assert_eq!(e.kind, ErrorKind::PlanTampered);
            assert!(e.message.contains("schema version"), "{}", e.message);
        }
    }

    #[test]
    fn unknown_fields_are_rejected_when_deserializing() {
        let (plan, _) = sample();
        assert!(edited(&plan, |v| v["approved"] = true.into()).is_err());
        assert!(edited(&plan, |v| v["items"][0]["force"] = true.into()).is_err());
        assert!(edited(&plan, |v| v["content_hash"] = "ABC".into()).is_err());
        assert!(
            edited(&plan, |v| {
                v.as_object_mut().unwrap().remove("machine_id");
            })
            .is_err()
        );
    }

    #[test]
    fn builder_refuses_s3_and_d3_violations() {
        let mut s3 = candidate(5, Tier::S0, Action::Delete);
        s3.raise(TierReason::new(Tier::S3, ReasonCode::GitTracked, "tracked"));
        assert_eq!(
            PlanItem::from_candidate(&s3).unwrap_err().kind,
            ErrorKind::SafetyBlocked
        );

        let mut item = PlanItem::from_candidate(&candidate(5, Tier::S0, Action::Delete)).unwrap();
        item.tier = Tier::S3;
        let e = Plan::new(params(PlanOrigin::Cli), vec![item.clone()]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked);
        assert_eq!(e.item_id.as_ref(), Some(&item.item_id));

        item.tier = Tier::S2;
        assert_eq!(
            Plan::new(params(PlanOrigin::Cli), vec![item.clone()])
                .unwrap_err()
                .kind,
            ErrorKind::SafetyBlocked
        );
        item.action = Action::None;
        assert_eq!(
            Plan::new(params(PlanOrigin::Cli), vec![item])
                .unwrap_err()
                .kind,
            ErrorKind::SafetyBlocked
        );

        let inconsistent = Candidate {
            tier: Tier::S0,
            ..candidate(6, Tier::S2, Action::Quarantine)
        };
        assert_eq!(
            PlanItem::from_candidate(&inconsistent).unwrap_err().kind,
            ErrorKind::SafetyBlocked
        );
    }

    #[test]
    fn builder_validates_binding_and_lifetime() {
        let mut p = params(PlanOrigin::Mcp);
        p.ttl_secs = Some(MCP_PLAN_TTL_SECS + 1);
        assert_eq!(Plan::new(p, vec![]).unwrap_err().kind, ErrorKind::Usage);
        let mut p = params(PlanOrigin::Cli);
        p.ttl_secs = Some(0);
        assert_eq!(Plan::new(p, vec![]).unwrap_err().kind, ErrorKind::Usage);
        let mut p = params(PlanOrigin::Cli);
        p.created_unix = i64::MAX;
        assert_eq!(Plan::new(p, vec![]).unwrap_err().kind, ErrorKind::Usage);
        let mut p = params(PlanOrigin::Cli);
        p.machine_id.clear();
        assert_eq!(Plan::new(p, vec![]).unwrap_err().kind, ErrorKind::Usage);
        let mut p = params(PlanOrigin::Desktop);
        p.ttl_secs = Some(60);
        let plan = Plan::new(p, vec![]).unwrap();
        assert_eq!(plan.expires_unix(), T0 + 60);
        assert!(plan.verify(T0 + 59, MACHINE, USER).is_ok());
    }

    #[test]
    fn canonical_json_is_sorted_and_compact() {
        let (plan, _) = sample();
        let c = plan.canonical_json().unwrap();
        assert!(!c.contains(HASH_KEY));
        assert!(!c.contains(' '), "{c}");
        assert!(!c.contains('\n'));
        assert!(
            c.starts_with(r#"{"created_unix":1800000000,"expires_unix":"#),
            "{c}"
        );
        let v: Value = serde_json::from_str(&c).unwrap();
        let mut again = String::new();
        write_canonical(&v, &mut again);
        assert_eq!(again, c);
        assert_eq!(plan.compute_hash().unwrap(), *plan.content_hash());
    }

    #[test]
    fn canonical_strings_escape_controls() {
        let mut out = String::new();
        write_canonical_str("a\"b\\c\n\u{1}\u{7f}é", &mut out);
        assert_eq!(out, "\"a\\\"b\\\\c\\n\\u0001\u{7f}é\"");
    }

    #[test]
    fn hash_ignores_input_order() {
        let (plan, cands) = sample();
        let reversed = cands
            .iter()
            .rev()
            .map(|c| PlanItem::from_candidate(c).unwrap())
            .collect();
        let other = Plan::new(params(PlanOrigin::Cli), reversed).unwrap();
        assert_eq!(other.content_hash(), plan.content_hash());
        assert_eq!(other, plan);
    }

    #[test]
    fn narrow_keeps_unchanged_items() {
        let (plan, cands) = sample();
        let r = narrow(&plan, &cands);
        assert!(r.dropped.is_empty());
        assert_eq!(r.executable, plan.items);
    }

    #[test]
    fn narrow_never_adds_fresh_items() {
        let (plan, mut cands) = sample();
        cands.push(candidate(50, Tier::S0, Action::Delete));
        let r = narrow(&plan, &cands);
        assert_eq!(r.executable.len(), 3);
        assert!(r.executable.iter().all(|i| plan.items.contains(i)));
    }

    fn reason_for(r: &NarrowResult, c: &Candidate) -> DropReason {
        r.dropped
            .iter()
            .find(|(id, _)| *id == c.id)
            .map(|(_, d)| d.clone())
            .unwrap()
    }

    #[test]
    fn narrow_drops_items_that_became_s3_fx_exec_10() {
        let (plan, mut cands) = sample();
        cands[1].raise(TierReason::new(
            Tier::S3,
            ReasonCode::InUse,
            "cargo lock held",
        ));
        let r = narrow(&plan, &cands);
        assert_eq!(r.executable.len(), 2);
        assert_eq!(reason_for(&r, &cands[1]), DropReason::NowProtected);
    }

    #[test]
    fn narrow_drops_raised_tiers_and_replaced_objects() {
        let (plan, mut cands) = sample();
        cands[0].raise(TierReason::new(
            Tier::S1,
            ReasonCode::RecentActivity,
            "recent",
        ));
        cands[1].identity = FileIdentity::new(7, 999);
        cands[2].path = FsPath::from("/elsewhere");
        let r = narrow(&plan, &cands);
        assert!(r.executable.is_empty());
        assert_eq!(
            reason_for(&r, &cands[0]),
            DropReason::TierRaised {
                planned: Tier::S0,
                fresh: Tier::S1
            }
        );
        assert_eq!(reason_for(&r, &cands[1]), DropReason::IdentityChanged);
        assert_eq!(reason_for(&r, &cands[2]), DropReason::PathChanged);
    }

    #[test]
    fn narrow_drops_missing_ambiguous_and_inconsistent() {
        let (plan, cands) = sample();
        let mut inconsistent = cands[2].clone();
        inconsistent.tier = Tier::S0;
        let fresh = vec![cands[1].clone(), cands[1].clone(), inconsistent];
        let r = narrow(&plan, &fresh);
        assert!(r.executable.is_empty());
        assert_eq!(reason_for(&r, &cands[0]), DropReason::Missing);
        assert_eq!(reason_for(&r, &cands[1]), DropReason::Ambiguous);
        assert_eq!(reason_for(&r, &cands[2]), DropReason::InconsistentCandidate);
    }

    #[test]
    fn narrow_only_ever_makes_actions_safer() {
        let (plan, mut cands) = sample();
        // S0 planned Delete, now proposed Quarantine: run the safer Quarantine.
        cands[0].action = Action::Quarantine;
        // S2 planned Trash, tier dropped to S0 and proposes Delete: keep the reviewed Trash.
        cands[2] = Candidate {
            reasons: vec![TierReason::new(Tier::S0, ReasonCode::Regenerable, "x")],
            tier: Tier::S0,
            action: Action::Delete,
            ..cands[2].clone()
        };
        // S1 planned Quarantine, now a tool command: not comparable, dropped.
        cands[1].action = Action::ToolCommand {
            tool: "npm".into(),
            args: vec!["cache".into(), "clean".into()],
        };
        let r = narrow(&plan, &cands);
        let get = |c: &Candidate| r.executable.iter().find(|i| i.item_id == c.id).cloned();
        assert_eq!(get(&cands[0]).unwrap().action, Action::Quarantine);
        let kept = get(&cands[2]).unwrap();
        assert_eq!((kept.tier, kept.action), (Tier::S0, Action::Trash));
        assert!(get(&cands[1]).is_none());
        assert_eq!(reason_for(&r, &cands[1]), DropReason::ActionChanged);
    }

    #[test]
    fn narrow_rechecks_unverified_plans() {
        let (plan, cands) = sample();
        let mut p = plan.clone();
        p.items[0].tier = Tier::S3;
        p.items.push(p.items[1].clone());
        let r = narrow(&p, &cands);
        assert_eq!(r.executable.len(), 2);
        assert_eq!(r.dropped.len(), 2);
        assert!(
            r.dropped
                .iter()
                .any(|(_, d)| *d == DropReason::InvalidPlanItem)
        );
        assert!(
            r.dropped
                .iter()
                .any(|(_, d)| *d == DropReason::DuplicateInPlan)
        );
    }

    #[test]
    fn drop_reason_codes_match_serialization() {
        let all = [
            DropReason::InvalidPlanItem,
            DropReason::DuplicateInPlan,
            DropReason::Missing,
            DropReason::Ambiguous,
            DropReason::IdentityChanged,
            DropReason::PathChanged,
            DropReason::InconsistentCandidate,
            DropReason::NowProtected,
            DropReason::TierRaised {
                planned: Tier::S0,
                fresh: Tier::S2,
            },
            DropReason::ActionChanged,
        ];
        for d in all {
            let v = serde_json::to_value(&d).unwrap();
            assert_eq!(v["reason"], Value::from(d.code()));
            assert_eq!(serde_json::from_value::<DropReason>(v).unwrap(), d);
        }
    }

    #[test]
    fn plan_schema_forbids_additional_properties() {
        let schema = serde_json::to_value(schemars::schema_for!(Plan)).unwrap();
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        let required = schema["required"].as_array().unwrap();
        for k in [
            "schema_version",
            "content_hash",
            "machine_id",
            "user",
            "items",
        ] {
            assert!(required.contains(&k.into()), "{k}");
        }
    }
}
