//! Android (P5-DET-02, PRD §7.1): Android Gradle Plugin build output and emulator images.
//!
//! | Rule | Folder | Tier |
//! |---|---|---|
//! | `android.build` | a module's `build/` with the plugin's `intermediates/` beside a Gradle build script | **S0**; **S1** with release outputs; **S2** with R8/ProGuard mapping files |
//! | `android.avd` | `<name>.avd` in an `avd` folder (`~/.android/avd`, `ANDROID_AVD_HOME`) | **S2**: the emulator's installed apps and data |
//!
//! [`AndroidDetector`] adds what data can't express:
//!
//! * `android.build`: a release APK or App Bundle (`outputs/apk/release`,
//!   `outputs/bundle/release`) is signed, and rebuilding it needs the signing key: S1. Mapping
//!   files (`outputs/mapping/`) are needed to deobfuscate crash reports from a shipped release
//!   and may be the only copy: S2.
//! * `android.avd`: the folder must sit in a folder named `avd`.
//!
//! The project `.gradle/` folder and the Gradle caches are the Java/Kotlin rules' and the cache
//! locator's.

use crate::detector::{DetectContext, DetectedArtifact, Detector, list};
use crate::rules::Rule;
use scull_core::{Evidence, Tier};
use scull_platform::{EntryKind, Platform};
use std::path::Path;

/// Rule ID of Android Gradle Plugin build output.
pub const BUILD_RULE_ID: &str = "android.build";

/// Rule ID of emulator images.
pub const AVD_RULE_ID: &str = "android.avd";

/// The ecosystem detector for `rule`, if it is an Android rule.
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    matches!(rule.id(), BUILD_RULE_ID | AVD_RULE_ID)
        .then(|| Box::new(AndroidDetector::new(rule.clone())) as Box<dyn Detector>)
}

/// An Android rule plus the checks data can't express (module docs).
#[derive(Debug, Clone)]
pub struct AndroidDetector {
    rule: Rule,
}

impl AndroidDetector {
    /// A detector for the `android.build` or `android.avd` rule.
    pub fn new(rule: Rule) -> Self {
        Self { rule }
    }
}

impl Detector for AndroidDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        _ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        if self.rule.id() == AVD_RULE_ID {
            let parent = dir.parent()?.file_name()?.to_str()?;
            if parent != "avd" {
                return None;
            }
            let (mut a, _) = crate::detector::detect_with_rule(&self.rule, dir, platform)?;
            a.projects.clear(); // an emulator belongs to no project
            return Some(a);
        }
        let (mut a, inside) = crate::detector::detect_with_rule(&self.rule, dir, platform)?;
        if inside.get("outputs") == Some(&EntryKind::Dir) {
            release_outputs(&mut a, &dir.join("outputs"), platform);
        }
        Some(a)
    }
}

/// Raise the floor for release outputs and mapping files under `outputs`. A level that can't be
/// listed is treated as holding them (missing evidence is never evidence of absence).
fn release_outputs(a: &mut DetectedArtifact, outputs: &Path, platform: &dyn Platform) {
    let Some(listing) = list(platform, outputs) else {
        a.base_tier = a.base_tier.max(Tier::S2);
        a.evidence.push(Evidence::Note {
            text: "outputs/ could not be listed, so it may hold release builds or mapping files"
                .into(),
        });
        return;
    };
    if listing.get("mapping") == Some(&EntryKind::Dir) {
        a.base_tier = a.base_tier.max(Tier::S2);
        a.evidence.push(Evidence::Note {
            text: "holds R8/ProGuard mapping files (outputs/mapping/), needed to deobfuscate \
                   crash reports from a shipped release; they may be the only copy"
                .into(),
        });
    }
    for kind in ["apk", "bundle"] {
        if listing.get(kind) != Some(&EntryKind::Dir) {
            continue;
        }
        let release = match list(platform, &outputs.join(kind)) {
            Some(l) => l.keys().any(|v| v.to_ascii_lowercase().contains("release")),
            None => true,
        };
        if release {
            a.base_tier = a.base_tier.max(Tier::S1);
            a.evidence.push(Evidence::Note {
                text: format!(
                    "holds a signed release build (outputs/{kind}/…release), which needs the \
                     signing key to rebuild"
                ),
            });
        }
    }
}
