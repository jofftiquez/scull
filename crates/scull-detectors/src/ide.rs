//! IDE and editor caches (P5-DET-05, PRD §7.1): JetBrains caches and indexes, VS Code (and
//! fork) workspace storage of workspaces that no longer exist, and extension versions the
//! editor has marked obsolete. **Never settings or extension state** of anything in use.
//!
//! | Rule | Folder | Tier |
//! |---|---|---|
//! | `jetbrains.caches` | `~/.cache/JetBrains/<Product><year.minor>` (Linux), `~/Library/Caches/JetBrains/…` (macOS), `%LocalAppData%\JetBrains\…` (Windows) | **S1** (re-indexing is slow) |
//! | `vscode.workspace-storage` | `<data>/User/workspaceStorage/<md5>` whose workspace is gone | **S2** |
//! | `vscode.obsolete-extension` | `~/.vscode/extensions/<id>-<version>` listed in `.obsolete`, referenced by no installed extension | **S1** |
//!
//! [`IdeDetector`] adds what data can't express:
//!
//! * JetBrains: the product folder must sit in a `JetBrains` folder whose parent is an OS cache
//!   folder (`.cache`, `Caches`, `Local`). Settings folders (`~/.config/JetBrains`,
//!   `~/Library/Application Support/JetBrains`, `%AppData%\JetBrains`) never match, and the rule
//!   excludes any folder that holds settings. The name must end in `<year>.<minor>`; a note says
//!   when the same product has a newer version folder beside it (a leftover).
//! * Workspace storage: the folder must sit in `User/workspaceStorage` of a known editor data
//!   folder. `workspace.json` is read as data; only a local `file://` workspace that **no
//!   longer exists** makes it a candidate. A workspace that exists, a remote one, or a file
//!   that can't be read leaves it alone: it may hold extension data (chat or task history).
//! * Obsolete extensions: the folder must sit in `extensions/` of a known editor home
//!   (`.vscode`, `.cursor`, …), be listed with `true` in `.obsolete` (the editor's own list of
//!   versions to delete), and appear in no entry of `extensions.json`. Either file missing or
//!   unreadable leaves it alone. Extension versions are shared by every profile, so "not the
//!   newest version" alone is never enough.

use crate::detector::{DetectContext, DetectedArtifact, Detector, detect_with_rule, list};
use crate::long_tail::is_lower_hex;
use crate::node::read_small;
use crate::rules::Rule;
use scull_core::Evidence;
use scull_platform::Platform;
use std::path::{Path, PathBuf};

/// Rule ID of JetBrains caches.
pub const JETBRAINS_CACHES_RULE_ID: &str = "jetbrains.caches";

/// Rule ID of editor workspace storage.
pub const WORKSPACE_STORAGE_RULE_ID: &str = "vscode.workspace-storage";

/// Rule ID of obsolete editor extension versions.
pub const OBSOLETE_EXTENSION_RULE_ID: &str = "vscode.obsolete-extension";

/// Parent folders of a JetBrains cache folder's `JetBrains` folder (OS cache folders).
const JETBRAINS_CACHE_HOMES: &[&str] = &[".cache", "Caches", "Local"];

/// Editor data folders that hold `User/workspaceStorage`.
pub const EDITOR_DATA_DIRS: &[&str] = &[
    "Code",
    "Code - Insiders",
    "Code - OSS",
    "VSCodium",
    "Cursor",
    "Windsurf",
];

/// Editor home folders that hold `extensions/`.
pub const EDITOR_EXTENSION_HOMES: &[&str] = &[
    ".vscode",
    ".vscode-insiders",
    ".vscode-oss",
    ".cursor",
    ".windsurf",
    ".vscode-server",
    ".vscode-server-insiders",
    ".cursor-server",
];

/// The ecosystem detector for `rule`, if it is an IDE rule.
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    matches!(
        rule.id(),
        JETBRAINS_CACHES_RULE_ID | WORKSPACE_STORAGE_RULE_ID | OBSOLETE_EXTENSION_RULE_ID
    )
    .then(|| Box::new(IdeDetector::new(rule.clone())) as Box<dyn Detector>)
}

/// An IDE rule plus the checks data can't express (module docs).
#[derive(Debug, Clone)]
pub struct IdeDetector {
    rule: Rule,
}

impl IdeDetector {
    /// A detector for one of the IDE rules.
    pub fn new(rule: Rule) -> Self {
        Self { rule }
    }
}

impl Detector for IdeDetector {
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
        let mut up = dir.ancestors().skip(1).map(|a| a.file_name()?.to_str());
        let parent = up.next()??;
        let grandparent = up.next()??;
        // Cheap name checks before any I/O.
        let placed = match self.rule.id() {
            JETBRAINS_CACHES_RULE_ID => {
                parent == "JetBrains"
                    && JETBRAINS_CACHE_HOMES.contains(&grandparent)
                    && product_version(name).is_some()
            }
            WORKSPACE_STORAGE_RULE_ID => {
                parent == "workspaceStorage"
                    && grandparent == "User"
                    && up
                        .next()
                        .flatten()
                        .is_some_and(|d| EDITOR_DATA_DIRS.contains(&d))
                    && is_lower_hex(name, 32)
            }
            OBSOLETE_EXTENSION_RULE_ID => {
                parent == "extensions" && EDITOR_EXTENSION_HOMES.contains(&grandparent)
            }
            _ => false,
        };
        if !placed {
            return None;
        }
        let (mut a, _) = detect_with_rule(&self.rule, dir, platform)?;
        a.projects.clear(); // editor-wide folders belong to no project
        match self.rule.id() {
            JETBRAINS_CACHES_RULE_ID => jetbrains(&mut a, dir, name, platform),
            WORKSPACE_STORAGE_RULE_ID => workspace_storage(&mut a, dir, platform, ctx)?,
            _ => obsolete_extension(&mut a, dir, name, platform, ctx)?,
        }
        Some(a)
    }
}

/// Split a JetBrains product folder name into product and `(year, minor)`:
/// `IntelliJIdea2024.2` → `("IntelliJIdea", (2024, 2))`.
pub fn product_version(name: &str) -> Option<(&str, (u32, u32))> {
    let (head, minor) = name.rsplit_once('.')?;
    if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) || head.len() < 5 {
        return None;
    }
    let (product, year) = head.split_at(head.len() - 4);
    if product.is_empty() || !year.starts_with("20") || !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((product, (year.parse().ok()?, minor.parse().ok()?)))
}

/// Note a newer version folder of the same product beside it.
fn jetbrains(a: &mut DetectedArtifact, dir: &Path, name: &str, platform: &dyn Platform) {
    let Some((product, version)) = product_version(name) else {
        return;
    };
    let Some(siblings) = dir.parent().and_then(|p| list(platform, p)) else {
        return;
    };
    let newest = siblings
        .keys()
        .filter_map(|s| product_version(s).map(|(p, v)| (p, v, s)))
        .filter(|(p, v, _)| *p == product && *v > version)
        .max_by_key(|(_, v, _)| *v);
    if let Some((_, _, newer)) = newest {
        a.evidence.push(Evidence::Note {
            text: format!(
                "a newer version of the same IDE has its own caches ({newer}): this folder is a \
                 leftover from an older version"
            ),
        });
    }
}

/// `None` unless the recorded workspace is a local path that no longer exists.
fn workspace_storage(
    a: &mut DetectedArtifact,
    dir: &Path,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Option<()> {
    let reader = ctx.config_reader?;
    let text = read_small(platform, reader, &dir.join("workspace.json"))?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let uri = ["folder", "workspace", "configuration"]
        .iter()
        .find_map(|k| json.get(k)?.as_str())?;
    let path = file_uri_to_path(uri)?;
    match platform.metadata(&path) {
        Err(e) if e.is_not_found() => {}
        _ => return None, // exists (in use) or unknown
    }
    a.evidence.push(Evidence::Note {
        text: format!(
            "saved editor state for the workspace {}, which no longer exists",
            path.display()
        ),
    });
    Some(())
}

/// A local `file://` URI as a path. `None` for other schemes, a host (UNC), malformed
/// percent-encoding or a path that isn't absolute on this platform.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    if !rest.starts_with('/') {
        return None; // `file://host/…`
    }
    let decoded = percent_decode(rest)?;
    // Windows drive paths are written `/c:/Users/…`.
    let b = decoded.as_bytes();
    let path = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        decoded[1..].to_owned()
    } else {
        decoded
    };
    let p = PathBuf::from(path);
    p.is_absolute().then_some(p)
}

fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
        } else {
            out.push(b);
        }
    }
    let s = String::from_utf8(out).ok()?;
    (!s.contains('\0')).then_some(s)
}

/// `None` unless the editor listed the folder as obsolete and no installed extension refers
/// to it.
fn obsolete_extension(
    a: &mut DetectedArtifact,
    dir: &Path,
    name: &str,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Option<()> {
    let reader = ctx.config_reader?;
    let parent = dir.parent()?;
    let obsolete: serde_json::Value =
        serde_json::from_str(&read_small(platform, reader, &parent.join(".obsolete"))?).ok()?;
    if obsolete.get(name).and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    let installed: serde_json::Value = serde_json::from_str(&read_small(
        platform,
        reader,
        &parent.join("extensions.json"),
    )?)
    .ok()?;
    let installed = installed.as_array()?;
    let referenced = installed.iter().any(|e| {
        e.get("relativeLocation")
            .and_then(serde_json::Value::as_str)
            == Some(name)
            || ["path", "fsPath"].iter().any(|k| {
                e.get("location")
                    .and_then(|l| l.get(k))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|p| {
                        p.trim_end_matches(['/', '\\'])
                            .rsplit(['/', '\\'])
                            .next()
                            .is_some_and(|last| last.eq_ignore_ascii_case(name))
                    })
            })
    });
    if referenced {
        return None;
    }
    a.evidence.push(Evidence::Note {
        text: "the editor marked this extension version obsolete (extensions/.obsolete) and no \
               installed extension refers to it; the editor deletes it on its next start"
            .into(),
    });
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jetbrains_product_versions() {
        assert_eq!(
            product_version("IntelliJIdea2024.2"),
            Some(("IntelliJIdea", (2024, 2)))
        );
        assert_eq!(
            product_version("PyCharmCE2023.3"),
            Some(("PyCharmCE", (2023, 3)))
        );
        assert_eq!(product_version("Toolbox"), None);
        assert_eq!(product_version("2024.2"), None);
        assert_eq!(product_version("Rider2024."), None);
        assert_eq!(product_version("Rider1999.1"), None);
        assert_eq!(product_version("Rider2024.x"), None);
    }

    #[test]
    fn file_uris() {
        if cfg!(windows) {
            assert_eq!(
                file_uri_to_path("file:///c%3A/Users/me/My%20App"),
                Some(PathBuf::from("c:/Users/me/My App"))
            );
        } else {
            assert_eq!(
                file_uri_to_path("file:///home/me/My%20App"),
                Some(PathBuf::from("/home/me/My App"))
            );
        }
        assert_eq!(
            file_uri_to_path("vscode-remote://ssh-remote+box/home/me"),
            None
        );
        assert_eq!(file_uri_to_path("file://server/share/x"), None);
        assert_eq!(file_uri_to_path("file:///bad%zz"), None);
        assert_eq!(file_uri_to_path("file:///nul%00"), None);
    }
}
