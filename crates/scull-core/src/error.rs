//! The error model shared by the CLI, MCP server and desktop app, and the stable process exit
//! codes (PRD §12, CLI-05).
//!
//! Every error carries an [`ErrorKind`] with a stable snake_case machine code (used in JSON
//! output and MCP results), a human message, and optional context (a path, an item ID). Each
//! kind maps to one [`ExitCode`]; the table is documented on [`ExitCode`].
//!
//! `scull-core` performs no I/O; OS errors are converted to a kind plus a message (see
//! [`ScullError::from_io`]) so no `std::io::Error` value ever appears in serialized output.

use crate::id::ItemId;
use crate::path::FsPath;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable process exit codes (CLI-05), serialized as the integer.
///
/// Every [`ErrorKind`] maps to exactly one exit code via [`ErrorKind::exit_code`]:
///
/// | Exit code | [`ExitCode`] | Meaning | [`ErrorKind`] codes |
/// |---:|---|---|---|
/// | 0 | [`Success`](ExitCode::Success) | Success | (none) |
/// | 1 | [`General`](ExitCode::General) | General error | `io`, `permission_denied`, `not_found`, `unsupported_platform`, `internal` |
/// | 2 | [`Usage`](ExitCode::Usage) | Usage/config error | `usage`, `config` |
/// | 3 | [`PartialFailure`](ExitCode::PartialFailure) | Completed with partial failures (see report) | `partial_failure` |
/// | 4 | [`PlanRejected`](ExitCode::PlanRejected) | Plan rejected (expired, tampered, foreign, or invalidated) | `plan_expired`, `plan_tampered`, `plan_foreign`, `plan_invalidated` |
/// | 5 | [`SafetyBlocked`](ExitCode::SafetyBlocked) | Blocked by safety policy | `safety_blocked` |
/// | 6 | [`LockHeld`](ExitCode::LockHeld) | Execution lock held by another operation | `lock_held` |
/// | 130 | [`Cancelled`](ExitCode::Cancelled) | Cancelled by user | `cancelled` |
///
/// Codes and exit values are part of the public contract: new kinds may be added, existing
/// ones never change their code string or exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "i32", try_from = "i32")]
pub enum ExitCode {
    /// 0: success.
    Success,
    /// 1: general error.
    General,
    /// 2: usage or configuration error.
    Usage,
    /// 3: completed, but some items failed (see the report).
    PartialFailure,
    /// 4: plan rejected (expired, tampered, foreign or invalidated).
    PlanRejected,
    /// 5: blocked by safety policy.
    SafetyBlocked,
    /// 6: the execution lock is held by another operation (SAFE-09).
    LockHeld,
    /// 130: cancelled by the user.
    Cancelled,
}

impl ExitCode {
    /// Every exit code, in ascending numeric order.
    pub const ALL: [ExitCode; 8] = [
        ExitCode::Success,
        ExitCode::General,
        ExitCode::Usage,
        ExitCode::PartialFailure,
        ExitCode::PlanRejected,
        ExitCode::SafetyBlocked,
        ExitCode::LockHeld,
        ExitCode::Cancelled,
    ];

    /// The numeric process exit code.
    pub const fn as_i32(self) -> i32 {
        match self {
            ExitCode::Success => 0,
            ExitCode::General => 1,
            ExitCode::Usage => 2,
            ExitCode::PartialFailure => 3,
            ExitCode::PlanRejected => 4,
            ExitCode::SafetyBlocked => 5,
            ExitCode::LockHeld => 6,
            ExitCode::Cancelled => 130,
        }
    }

    /// The exit code for a numeric value, if it is one of the documented codes.
    pub fn from_i32(code: i32) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_i32() == code)
    }
}

impl From<ExitCode> for i32 {
    fn from(c: ExitCode) -> i32 {
        c.as_i32()
    }
}

impl TryFrom<i32> for ExitCode {
    type Error = String;

    fn try_from(code: i32) -> Result<Self, Self::Error> {
        Self::from_i32(code).ok_or_else(|| format!("unknown exit code {code}"))
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(c: ExitCode) -> Self {
        // Every documented code fits in a u8 (max 130).
        std::process::ExitCode::from(c.as_i32() as u8)
    }
}

impl fmt::Display for ExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_i32())
    }
}

impl JsonSchema for ExitCode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ExitCode".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "integer",
            "description": "Stable process exit code (CLI-05).",
            "enum": [0, 1, 2, 3, 4, 5, 6, 130]
        })
    }
}

/// What went wrong, as a stable machine code. Serialized as the snake_case string returned by
/// [`ErrorKind::as_str`]. See [`ExitCode`] for the exit code of each kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// Invalid command-line usage or arguments.
    Usage,
    /// Invalid or unreadable configuration.
    Config,
    /// An I/O error not covered by a more specific kind.
    Io,
    /// The OS denied access.
    PermissionDenied,
    /// A path, item, plan or operation does not exist.
    NotFound,
    /// The plan is past its expiry time.
    PlanExpired,
    /// The plan's content hash does not match (it was edited).
    PlanTampered,
    /// The plan was created on another machine or by another user.
    PlanForeign,
    /// Another operation touched the plan's targets after it was created (SAFE-09).
    PlanInvalidated,
    /// The safety policy refused the action.
    SafetyBlocked,
    /// The execution lock is held by another operation (SAFE-09).
    LockHeld,
    /// The user cancelled.
    Cancelled,
    /// The operation completed but some items failed.
    PartialFailure,
    /// The operation is not supported on this platform or filesystem.
    UnsupportedPlatform,
    /// A bug in scull.
    Internal,
}

impl ErrorKind {
    /// Every kind, in declaration order.
    pub const ALL: [ErrorKind; 15] = [
        ErrorKind::Usage,
        ErrorKind::Config,
        ErrorKind::Io,
        ErrorKind::PermissionDenied,
        ErrorKind::NotFound,
        ErrorKind::PlanExpired,
        ErrorKind::PlanTampered,
        ErrorKind::PlanForeign,
        ErrorKind::PlanInvalidated,
        ErrorKind::SafetyBlocked,
        ErrorKind::LockHeld,
        ErrorKind::Cancelled,
        ErrorKind::PartialFailure,
        ErrorKind::UnsupportedPlatform,
        ErrorKind::Internal,
    ];

    /// The stable machine code, identical to the serialized form.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Usage => "usage",
            ErrorKind::Config => "config",
            ErrorKind::Io => "io",
            ErrorKind::PermissionDenied => "permission_denied",
            ErrorKind::NotFound => "not_found",
            ErrorKind::PlanExpired => "plan_expired",
            ErrorKind::PlanTampered => "plan_tampered",
            ErrorKind::PlanForeign => "plan_foreign",
            ErrorKind::PlanInvalidated => "plan_invalidated",
            ErrorKind::SafetyBlocked => "safety_blocked",
            ErrorKind::LockHeld => "lock_held",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::PartialFailure => "partial_failure",
            ErrorKind::UnsupportedPlatform => "unsupported_platform",
            ErrorKind::Internal => "internal",
        }
    }

    /// The process exit code for this kind (CLI-05).
    pub const fn exit_code(self) -> ExitCode {
        match self {
            ErrorKind::Usage | ErrorKind::Config => ExitCode::Usage,
            ErrorKind::Io
            | ErrorKind::PermissionDenied
            | ErrorKind::NotFound
            | ErrorKind::UnsupportedPlatform
            | ErrorKind::Internal => ExitCode::General,
            ErrorKind::PartialFailure => ExitCode::PartialFailure,
            ErrorKind::PlanExpired
            | ErrorKind::PlanTampered
            | ErrorKind::PlanForeign
            | ErrorKind::PlanInvalidated => ExitCode::PlanRejected,
            ErrorKind::SafetyBlocked => ExitCode::SafetyBlocked,
            ErrorKind::LockHeld => ExitCode::LockHeld,
            ErrorKind::Cancelled => ExitCode::Cancelled,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<std::io::ErrorKind> for ErrorKind {
    /// Map an OS error category. Only the categories with a dedicated kind are distinguished;
    /// everything else is [`ErrorKind::Io`].
    fn from(k: std::io::ErrorKind) -> Self {
        match k {
            std::io::ErrorKind::NotFound => ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
            std::io::ErrorKind::Unsupported => ErrorKind::UnsupportedPlatform,
            _ => ErrorKind::Io,
        }
    }
}

/// A serializable error: kind, human message and optional context.
///
/// ```
/// use scull_core::{ErrorKind, ExitCode, FsPath, ScullError};
///
/// let e = ScullError::new(ErrorKind::PlanExpired, "plan expired 3 minutes ago")
///     .with_path(FsPath::from("/home/me/plan.json"));
/// assert_eq!(e.exit_code(), ExitCode::PlanRejected);
/// assert_eq!(e.exit_code().as_i32(), 4);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScullError {
    /// Stable machine code.
    pub kind: ErrorKind,
    /// Human-readable explanation.
    pub message: String,
    /// The path involved, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<FsPath>,
    /// The item involved, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<ItemId>,
}

impl ScullError {
    /// An error with no context.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            path: None,
            item_id: None,
        }
    }

    /// Convert an OS error into a kind plus message. The `std::io::Error` value itself is not
    /// kept, so the result serializes deterministically on every platform.
    pub fn from_io(err: &std::io::Error) -> Self {
        Self::new(err.kind().into(), err.to_string())
    }

    /// Attach the path involved.
    pub fn with_path(mut self, path: FsPath) -> Self {
        self.path = Some(path);
        self
    }

    /// Attach the item involved.
    pub fn with_item(mut self, item_id: ItemId) -> Self {
        self.item_id = Some(item_id);
        self
    }

    /// The process exit code for this error (CLI-05).
    pub const fn exit_code(&self) -> ExitCode {
        self.kind.exit_code()
    }
}

impl fmt::Display for ScullError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)?;
        if let Some(p) = &self.path {
            write!(f, " (path: {p})")?;
        }
        if let Some(i) = &self.item_id {
            write!(f, " (item: {i})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ScullError {}

impl From<std::io::Error> for ScullError {
    fn from(err: std::io::Error) -> Self {
        Self::from_io(&err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::FileIdentity;
    use std::collections::HashSet;

    /// Fails to compile when a kind is added without updating `ErrorKind::ALL` and the tests.
    fn exhaustive_index(k: ErrorKind) -> usize {
        match k {
            ErrorKind::Usage => 0,
            ErrorKind::Config => 1,
            ErrorKind::Io => 2,
            ErrorKind::PermissionDenied => 3,
            ErrorKind::NotFound => 4,
            ErrorKind::PlanExpired => 5,
            ErrorKind::PlanTampered => 6,
            ErrorKind::PlanForeign => 7,
            ErrorKind::PlanInvalidated => 8,
            ErrorKind::SafetyBlocked => 9,
            ErrorKind::LockHeld => 10,
            ErrorKind::Cancelled => 11,
            ErrorKind::PartialFailure => 12,
            ErrorKind::UnsupportedPlatform => 13,
            ErrorKind::Internal => 14,
        }
    }

    #[test]
    fn all_lists_every_kind_once() {
        for (i, k) in ErrorKind::ALL.into_iter().enumerate() {
            assert_eq!(
                exhaustive_index(k),
                i,
                "{k:?} out of place in ErrorKind::ALL"
            );
        }
    }

    #[test]
    fn exit_code_table_matches_cli_05() {
        let table: [(ExitCode, i32); 8] = [
            (ExitCode::Success, 0),
            (ExitCode::General, 1),
            (ExitCode::Usage, 2),
            (ExitCode::PartialFailure, 3),
            (ExitCode::PlanRejected, 4),
            (ExitCode::SafetyBlocked, 5),
            (ExitCode::LockHeld, 6),
            (ExitCode::Cancelled, 130),
        ];
        assert_eq!(table.map(|(c, _)| c), ExitCode::ALL);
        for (code, n) in table {
            assert_eq!(code.as_i32(), n);
            assert_eq!(ExitCode::from_i32(n), Some(code));
        }
        for n in [-1, 7, 8, 127, 129, 131, 255] {
            assert_eq!(ExitCode::from_i32(n), None);
        }
    }

    #[test]
    fn every_kind_maps_to_documented_exit_code() {
        let table: [(ErrorKind, i32); 15] = [
            (ErrorKind::Usage, 2),
            (ErrorKind::Config, 2),
            (ErrorKind::Io, 1),
            (ErrorKind::PermissionDenied, 1),
            (ErrorKind::NotFound, 1),
            (ErrorKind::PlanExpired, 4),
            (ErrorKind::PlanTampered, 4),
            (ErrorKind::PlanForeign, 4),
            (ErrorKind::PlanInvalidated, 4),
            (ErrorKind::SafetyBlocked, 5),
            (ErrorKind::LockHeld, 6),
            (ErrorKind::Cancelled, 130),
            (ErrorKind::PartialFailure, 3),
            (ErrorKind::UnsupportedPlatform, 1),
            (ErrorKind::Internal, 1),
        ];
        assert_eq!(table.map(|(k, _)| k), ErrorKind::ALL);
        for (kind, n) in table {
            assert_eq!(kind.exit_code().as_i32(), n, "{kind}");
            assert_ne!(kind.exit_code(), ExitCode::Success, "{kind}");
        }
    }

    #[test]
    fn kind_codes_are_stable_and_unique() {
        let codes = ErrorKind::ALL.map(ErrorKind::as_str);
        assert_eq!(
            codes,
            [
                "usage",
                "config",
                "io",
                "permission_denied",
                "not_found",
                "plan_expired",
                "plan_tampered",
                "plan_foreign",
                "plan_invalidated",
                "safety_blocked",
                "lock_held",
                "cancelled",
                "partial_failure",
                "unsupported_platform",
                "internal",
            ]
        );
        assert_eq!(codes.iter().collect::<HashSet<_>>().len(), codes.len());
        for k in ErrorKind::ALL {
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::Value::from(k.as_str())
            );
            assert_eq!(k.to_string(), k.as_str());
            let back: ErrorKind = serde_json::from_value(k.as_str().into()).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn exit_code_serializes_as_integer() {
        for c in ExitCode::ALL {
            let v = serde_json::to_value(c).unwrap();
            assert_eq!(v, serde_json::Value::from(c.as_i32()));
            assert_eq!(serde_json::from_value::<ExitCode>(v).unwrap(), c);
        }
        assert!(serde_json::from_str::<ExitCode>("7").is_err());
    }

    #[test]
    fn error_round_trips_with_and_without_context() {
        let item = ItemId::derive(FileIdentity::new(1, 2), b"/p/target");
        let full = ScullError::new(ErrorKind::SafetyBlocked, "inside a protected root")
            .with_path(FsPath::from("/home/me/.ssh"))
            .with_item(item.clone());
        let json = serde_json::to_value(&full).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "safety_blocked",
                "message": "inside a protected root",
                "path": "/home/me/.ssh",
                "item_id": item.as_str(),
            })
        );
        assert_eq!(serde_json::from_value::<ScullError>(json).unwrap(), full);

        let bare = ScullError::new(ErrorKind::LockHeld, "another operation is running");
        let json = serde_json::to_string(&bare).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"lock_held","message":"another operation is running"}"#
        );
        assert_eq!(serde_json::from_str::<ScullError>(&json).unwrap(), bare);
    }

    #[test]
    fn unknown_fields_and_kinds_are_rejected() {
        let extra = r#"{"kind":"io","message":"x","errno":5}"#;
        assert!(serde_json::from_str::<ScullError>(extra).is_err());
        let bad_kind = r#"{"kind":"oops","message":"x"}"#;
        assert!(serde_json::from_str::<ScullError>(bad_kind).is_err());
    }

    #[test]
    fn io_errors_become_kind_and_message() {
        use std::io;
        let cases = [
            (io::ErrorKind::NotFound, ErrorKind::NotFound),
            (io::ErrorKind::PermissionDenied, ErrorKind::PermissionDenied),
            (io::ErrorKind::Unsupported, ErrorKind::UnsupportedPlatform),
            (io::ErrorKind::Interrupted, ErrorKind::Io),
            (io::ErrorKind::Other, ErrorKind::Io),
        ];
        for (io_kind, kind) in cases {
            let e = ScullError::from(io::Error::new(io_kind, "boom"));
            assert_eq!(e.kind, kind);
            assert_eq!(e.message, "boom");
            assert_eq!(e.path, None);
        }
    }

    #[test]
    fn display_includes_code_message_and_context() {
        let e = ScullError::new(ErrorKind::NotFound, "no such plan")
            .with_path(FsPath::from("/tmp/plan.json"));
        assert_eq!(
            e.to_string(),
            "not_found: no such plan (path: /tmp/plan.json)"
        );
        let as_dyn: &dyn std::error::Error = &e;
        assert!(as_dyn.source().is_none());
    }

    #[test]
    fn schema_forbids_additional_properties() {
        let schema = serde_json::to_value(schemars::schema_for!(ScullError)).unwrap();
        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&"kind".into()));
        assert!(required.contains(&"message".into()));
        assert!(!required.contains(&"path".into()));
    }
}
