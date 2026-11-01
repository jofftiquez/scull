//! Xcode (P5-DET-02, PRD §7.1, §10.5): DerivedData, device support files, archives and
//! unavailable simulators. macOS only (the rules in `rules/ecosystems/xcode.toml` say so).
//!
//! | Rule | Folder (default location) | Tier | Cleanup |
//! |---|---|---|---|
//! | `xcode.derived-data` | `~/Library/Developer/Xcode/DerivedData/<Project>-<hash>` | **S0**; **S1** with Swift package checkouts inside | safe executor |
//! | `xcode.device-support` | `~/Library/Developer/Xcode/<OS> DeviceSupport/<version (build)>` | **S1** (copied again from a device, slowly) | safe executor |
//! | `xcode.archive` | `~/Library/Developer/Xcode/Archives/<date>/<name>.xcarchive` | **S2** (signed build + dSYMs) | quarantine |
//! | `xcode.simulators` | `~/Library/Developer/CoreSimulator/Devices` | **S1** | `xcrun simctl delete unavailable` (tool-native) |
//!
//! [`XcodeDetector`] adds what data can't express:
//!
//! * DerivedData and device support folders must sit in their usual parent (`DerivedData`,
//!   `<OS> DeviceSupport`), and the simulator device set in `CoreSimulator`. Look-alikes
//!   elsewhere are not candidates. Archives are recognized anywhere: wherever one is, it is a
//!   signed build (S2).
//! * A DerivedData folder records its project in `info.plist` (`WorkspacePath`). It is read as
//!   data through the [`ConfigReader`](crate::ConfigReader) and becomes the candidate's
//!   project; when that project no longer exists a note says the folder is a leftover.
//!   DerivedData belongs to no project otherwise.
//! * DerivedData with `SourcePackages/` holds Swift package checkouts that are downloaded again:
//!   costly to recreate, so S1.
//! * The simulator device set is cleaned only with `xcrun simctl delete unavailable` (chosen
//!   here, CLS-04, never run by the detector). The command removes only devices whose runtime
//!   is no longer installed, so the folder's size overstates what it frees; the evidence says
//!   so and counts the devices.

use crate::detector::{DetectContext, DetectedArtifact, Detector, Listing, detect_with_rule};
use crate::node::read_small;
use crate::rules::Rule;
use scull_core::{Action, Evidence, FsPath, ProjectRef};
use scull_platform::{EntryKind, Platform};
use scull_safety::Regen;
use std::path::{Path, PathBuf};

/// The simulator cleanup command (PRD §10.5).
pub const SIMCTL_DELETE_UNAVAILABLE: [&str; 3] = ["simctl", "delete", "unavailable"];

/// What an Xcode rule detects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum XcodeKind {
    /// One project's DerivedData folder.
    DerivedData,
    /// One OS version's device support files.
    DeviceSupport,
    /// An `.xcarchive`.
    Archive,
    /// The simulator device set.
    Simulators,
}

impl XcodeKind {
    /// Every kind.
    pub const ALL: [XcodeKind; 4] = [
        XcodeKind::DerivedData,
        XcodeKind::DeviceSupport,
        XcodeKind::Archive,
        XcodeKind::Simulators,
    ];

    /// The rule ID.
    pub fn rule_id(self) -> &'static str {
        match self {
            XcodeKind::DerivedData => "xcode.derived-data",
            XcodeKind::DeviceSupport => "xcode.device-support",
            XcodeKind::Archive => "xcode.archive",
            XcodeKind::Simulators => "xcode.simulators",
        }
    }

    /// The kind for a rule ID.
    pub fn from_rule_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.rule_id() == id)
    }
}

/// The ecosystem detector for `rule`, if it is an Xcode rule.
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    XcodeKind::from_rule_id(rule.id())
        .map(|k| Box::new(XcodeDetector::new(rule.clone(), k)) as Box<dyn Detector>)
}

/// An Xcode rule plus the checks data can't express (module docs).
#[derive(Debug, Clone)]
pub struct XcodeDetector {
    rule: Rule,
    kind: XcodeKind,
}

impl XcodeDetector {
    /// A detector for `rule`, treated as `kind`.
    pub fn new(rule: Rule, kind: XcodeKind) -> Self {
        Self { rule, kind }
    }

    /// What it detects.
    pub fn kind(&self) -> XcodeKind {
        self.kind
    }
}

impl Detector for XcodeDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        let name = dir.file_name()?.to_str()?;
        let parent = dir.parent()?.file_name()?.to_str()?;
        // Cheap name checks before any I/O.
        let placed = match self.kind {
            XcodeKind::DerivedData => parent == "DerivedData" && has_xcode_hash(name),
            XcodeKind::DeviceSupport => parent.ends_with(" DeviceSupport"),
            XcodeKind::Archive => true,
            XcodeKind::Simulators => parent == "CoreSimulator",
        };
        if !placed {
            return None;
        }
        let (mut a, inside) = detect_with_rule(&self.rule, dir, platform)?;
        a.projects.clear(); // Xcode's own folders belong to no project by location
        match self.kind {
            XcodeKind::DerivedData => derived_data(&mut a, dir, &inside, platform, ctx),
            XcodeKind::Simulators => simulators(&mut a, &inside),
            XcodeKind::DeviceSupport | XcodeKind::Archive => {}
        }
        Some(a)
    }
}

/// Whether `name` ends with `-` and Xcode's 28-letter lowercase project hash.
fn has_xcode_hash(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(project, hash)| {
        !project.is_empty() && hash.len() == 28 && hash.bytes().all(|b| b.is_ascii_lowercase())
    })
}

fn derived_data(
    a: &mut DetectedArtifact,
    dir: &Path,
    inside: &Listing,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) {
    if inside.get("SourcePackages") == Some(&EntryKind::Dir) {
        a.signals.regen = Regen::NetworkOrLong;
        a.evidence.push(Evidence::Note {
            text: "holds Swift package checkouts (SourcePackages/), which Xcode downloads \
                   again: costly to recreate"
                .into(),
        });
    }
    let Some(reader) = ctx.config_reader else {
        return;
    };
    let Some(text) = read_small(platform, reader, &dir.join("info.plist")) else {
        return;
    };
    let Some(workspace) = plist_string(&text, "WorkspacePath").map(PathBuf::from) else {
        return;
    };
    if !workspace.is_absolute() {
        return;
    }
    let exists = match platform.metadata(&workspace) {
        Ok(_) => true,
        Err(e) if e.is_not_found() => false,
        Err(_) => return, // unknown: say nothing
    };
    if let Some(root) = workspace.parent() {
        a.projects = vec![ProjectRef {
            root: FsPath::from_path(root),
            name: workspace
                .file_stem()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            ecosystem: "xcode".into(),
        }];
    }
    a.evidence.push(Evidence::Note {
        text: if exists {
            format!("built from {}", workspace.display())
        } else {
            format!(
                "built from {}, which no longer exists: this DerivedData is a leftover",
                workspace.display()
            )
        },
    });
}

fn simulators(a: &mut DetectedArtifact, inside: &Listing) {
    a.tool_action = Some(Action::ToolCommand {
        tool: "xcrun".into(),
        args: SIMCTL_DELETE_UNAVAILABLE
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    });
    let devices = inside
        .iter()
        .filter(|(n, k)| **k == EntryKind::Dir && is_uuid(n))
        .count();
    a.evidence.push(Evidence::Note {
        text: format!(
            "{devices} simulator device(s) here; `xcrun simctl delete unavailable` removes only \
             those whose runtime is no longer installed and keeps the rest, so this folder's \
             size overstates what it frees. Not undoable"
        ),
    });
}

/// Whether `name` looks like a simulator device UUID (`8-4-4-4-12` hex digits).
fn is_uuid(name: &str) -> bool {
    let parts: Vec<&str> = name.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The `<string>` value following `<key>name</key>` in an XML property list. Pure: the file is
/// data, never executed. Binary property lists give `None`.
pub(crate) fn plist_string(text: &str, name: &str) -> Option<String> {
    let key = format!("<key>{name}</key>");
    let after = &text[text.find(&key)? + key.len()..];
    let after = after.trim_start();
    let body = after.strip_prefix("<string>")?;
    let end = body.find("</string>")?;
    Some(
        body[..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xcode_project_hash() {
        assert!(has_xcode_hash("MyApp-abcdefghijklmnopqrstuvwxyzab"));
        assert!(has_xcode_hash("My-App-abcdefghijklmnopqrstuvwxyzab"));
        assert!(!has_xcode_hash("MyApp-ABCDEFGHIJKLMNOPQRSTUVWXYZAB"));
        assert!(!has_xcode_hash("MyApp-abcdefghijklmnopqrstuvwxyza"));
        assert!(!has_xcode_hash("-abcdefghijklmnopqrstuvwxyzab"));
    }

    #[test]
    fn plist_values() {
        let p = "<?xml version=\"1.0\"?><plist><dict>\n\t<key>LastAccessedDate</key>\n\t<date>2026-10-01T00:00:00Z</date>\n\t<key>WorkspacePath</key>\n\t<string>/Users/me/App &amp; Co/App.xcodeproj</string>\n</dict></plist>";
        assert_eq!(
            plist_string(p, "WorkspacePath").as_deref(),
            Some("/Users/me/App & Co/App.xcodeproj")
        );
        assert_eq!(plist_string(p, "LastAccessedDate"), None, "not a string");
        assert_eq!(plist_string("bplist00\u{1}", "WorkspacePath"), None);
    }

    #[test]
    fn device_uuids() {
        assert!(is_uuid("0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"));
        assert!(!is_uuid("device_set.plist"));
        assert!(!is_uuid("0A1B2C3D-4E5F-6071-8293"));
    }
}
