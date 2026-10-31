//! Identifiers: file identity (device + inode / volume + file ID), item IDs and opaque IDs for
//! scans, plans and operations.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;

/// The identity of a filesystem object, independent of its path (SAFE-05, SAFE-13).
///
/// * Unix: `volume` = `st_dev`, `file` = `st_ino`.
/// * Windows: `volume` = volume serial number, `file` = the 128-bit file ID
///   (`FILE_ID_INFO`; NTFS IDs fit in 64 bits, ReFS needs 128).
///
/// Two paths with the same identity are the same object (hardlinks, case aliases, 8.3 names).
/// Values come only from `scull-platform`. In JSON the file ID is a hex string so 128-bit
/// values survive JavaScript clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileIdentity {
    /// Device / volume identifier.
    pub volume: u64,
    /// Inode number / file ID.
    #[serde(with = "hex_u128")]
    pub file: u128,
}

impl JsonSchema for FileIdentity {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "FileIdentity".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "description": "Path-independent identity of a filesystem object (device/volume + inode/file ID).",
            "required": ["volume", "file"],
            "properties": {
                "volume": { "type": "integer", "minimum": 0 },
                "file": { "type": "string", "pattern": "^[0-9a-f]{1,32}$", "description": "Inode / file ID as lowercase hex." }
            },
            "additionalProperties": false
        })
    }
}

impl FileIdentity {
    /// Construct from raw parts.
    pub const fn new(volume: u64, file: u128) -> Self {
        Self { volume, file }
    }
}

impl fmt::Display for FileIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}:{:x}", self.volume, self.file)
    }
}

mod hex_u128 {
    use super::*;

    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{v:x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let s = String::deserialize(d)?;
        if s.is_empty() || s.len() > 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(serde::de::Error::custom("file id must be 1-32 hex digits"));
        }
        u128::from_str_radix(&s, 16).map_err(serde::de::Error::custom)
    }
}

/// Stable ID of a candidate: a hash of its identity and its path bytes.
///
/// Binding both means an item ID stops matching if the object at the path is replaced
/// (new identity) *or* the same object is moved (new path). Plans refer to items by `ItemId`,
/// never by path alone (SAFE-11): a lossy or edited path in a plan file can't redirect deletion.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ItemId(String);

impl JsonSchema for ItemId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ItemId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Stable item ID: 128-bit truncated SHA-256 of identity + path, lowercase hex.",
            "pattern": "^[0-9a-f]{32}$"
        })
    }
}

impl ItemId {
    /// Length of the hex string (128-bit truncated SHA-256).
    pub const HEX_LEN: usize = 32;

    /// Derive an item ID. `path_bytes` is the platform-native path encoding (bytes on Unix,
    /// UTF-16LE on Windows) as produced by `scull-platform`.
    pub fn derive(identity: FileIdentity, path_bytes: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(b"scull-item-v1\0");
        h.update(identity.volume.to_le_bytes());
        h.update(identity.file.to_le_bytes());
        h.update((path_bytes.len() as u64).to_le_bytes());
        h.update(path_bytes);
        let digest = h.finalize();
        Self(hex(&digest[..Self::HEX_LEN / 2]))
    }

    /// The hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ItemId {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() == Self::HEX_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s))
        } else {
            Err(format!(
                "item id must be {} lowercase hex characters",
                Self::HEX_LEN
            ))
        }
    }
}

impl From<ItemId> for String {
    fn from(v: ItemId) -> String {
        v.0
    }
}

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lowercase hex encoding.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

macro_rules! opaque_id {
    ($(#[$m:meta])* $name:ident, $prefix:literal) => {
        $(#[$m])*
        ///
        #[doc = concat!(
            "Opaque string of the form `", $prefix, "_<suffix>` where the suffix is 1–64 ",
            "characters of `[0-9A-Za-z]`. Generation (time + randomness) lives outside ",
            "`scull-core`, which performs no I/O."
        )]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "pattern": concat!("^", $prefix, "_[0-9A-Za-z]{1,64}$")
                })
            }
        }

        impl $name {
            /// Required prefix, including the trailing underscore.
            pub const PREFIX: &'static str = concat!($prefix, "_");

            /// Build from a suffix (validated).
            pub fn from_suffix(suffix: &str) -> Result<Self, String> {
                Self::try_from(format!("{}{}", Self::PREFIX, suffix))
            }

            /// The full ID string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = String;

            fn try_from(s: String) -> Result<Self, Self::Error> {
                match s.strip_prefix(Self::PREFIX) {
                    Some(rest)
                        if !rest.is_empty()
                            && rest.len() <= 64
                            && rest.bytes().all(|b| b.is_ascii_alphanumeric()) =>
                    {
                        Ok(Self(s))
                    }
                    _ => Err(format!(
                        "{} must look like {}<1-64 alphanumerics>",
                        stringify!($name),
                        Self::PREFIX
                    )),
                }
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id!(
    /// ID of one scan.
    ScanId,
    "scan"
);
opaque_id!(
    /// ID of a plan (PRD §10.4).
    PlanId,
    "plan"
);
opaque_id!(
    /// ID of one execution of a plan (an *operation*, PRD §6).
    OperationId,
    "op"
);

/// Reference to the detector rule that produced a candidate (CLS-01).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleRef {
    /// Rule ID, e.g. `rust.target`.
    pub id: String,
    /// Rule version; bumps whenever the rule's matching or tiering changes.
    pub version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_binds_identity_and_path() {
        let a = ItemId::derive(FileIdentity::new(1, 2), b"/p/target");
        assert_eq!(a.as_str().len(), ItemId::HEX_LEN);
        assert_eq!(a, ItemId::derive(FileIdentity::new(1, 2), b"/p/target"));
        assert_ne!(a, ItemId::derive(FileIdentity::new(1, 3), b"/p/target"));
        assert_ne!(a, ItemId::derive(FileIdentity::new(1, 2), b"/p/target2"));
    }

    #[test]
    fn item_id_rejects_malformed() {
        assert!(ItemId::try_from("xyz".to_string()).is_err());
        assert!(ItemId::try_from("A".repeat(32)).is_err());
        assert!(ItemId::try_from("a".repeat(32)).is_ok());
    }

    #[test]
    fn opaque_ids_validate_prefix() {
        assert!(PlanId::from_suffix("01HZX").is_ok());
        assert!(PlanId::try_from("scan_01HZX".to_string()).is_err());
        assert!(PlanId::try_from("plan_".to_string()).is_err());
        assert!(PlanId::try_from("plan_../../x".to_string()).is_err());
        assert_eq!(OperationId::PREFIX, "op_");
    }

    #[test]
    fn file_identity_hex_rejects_garbage() {
        let bad = r#"{"volume":1,"file":"zz"}"#;
        assert!(serde_json::from_str::<FileIdentity>(bad).is_err());
        let ok = r#"{"volume":1,"file":"ffffffffffffffffffffffffffffffff"}"#;
        assert_eq!(
            serde_json::from_str::<FileIdentity>(ok).unwrap().file,
            u128::MAX
        );
    }
}
