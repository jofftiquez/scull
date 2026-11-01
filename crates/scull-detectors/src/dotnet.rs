//! .NET (P5-DET-01, PRD §7.1, §10.8): `bin/` and `obj/` build folders and the NuGet global
//! packages folder.
//!
//! # Build folders (`rules/ecosystems/dotnet.toml`)
//!
//! `bin/` and `obj/` are only candidates beside a project file (`*.csproj`, `*.fsproj`,
//! `*.vbproj`): a `.sln` alone, or the folder name alone, is never enough (CLS-03). `obj/` must
//! also hold restore or build output (`project.assets.json`, `*.nuget.g.props`, ...) and `bin/`
//! a configuration folder (`Debug/`, `Release/`).
//!
//! [`DotnetBinDetector`] adds what data can't express: `bin/` is a common name for a folder of
//! scripts, and MSBuild only ever writes configuration folders into it. A `bin/` with anything
//! else at its top level (a loose file, a link) may hold more than build output, so it is
//! raised to **S2** with a note instead of S0.
//!
//! # NuGet global packages folder ([`crate::caches`], `nuget.packages`)
//!
//! Located from `NUGET_PACKAGES`, then `globalPackagesFolder` in the user `NuGet.Config` (read
//! as data; relative values are relative to that file's folder, `%VAR%` is expanded), else
//! `~/.nuget/packages`. Cleaned with `dotnet nuget locals global-packages --clear`, which honors
//! `NUGET_PACKAGES`. Project `NuGet.Config` files are never read (T-23).

use crate::caches::{CacheEnv, CacheSource, CacheTool, Spot, Spots, UserRc, configured, refused};
use crate::detector::{DetectContext, DetectedArtifact, Detector, Listing, detect_with_rule, list};
use crate::jvm::strip_xml_comments;
use crate::rules::{Platform as Os, Rule};
use scull_core::{Evidence, Tier};
use scull_platform::{EntryKind, Platform};
use std::path::{Path, PathBuf};

/// Rule ID of .NET `bin/` folders.
pub const BIN_RULE_ID: &str = "dotnet.bin";

/// Package folders looked into when recognizing the NuGet layout.
const MAX_PACKAGES_PROBED: usize = 8;

/// The ecosystem detector for `rule`, if it needs more than [`crate::RuleDetector`].
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    (rule.id() == BIN_RULE_ID).then(|| Box::new(DotnetBinDetector::new(rule.clone())) as _)
}

/// .NET `bin/` whose top level holds only configuration folders.
#[derive(Debug, Clone)]
pub struct DotnetBinDetector {
    rule: Rule,
}

impl DotnetBinDetector {
    /// A detector for the `dotnet.bin` rule (or a rule shaped like it).
    pub fn new(rule: Rule) -> Self {
        Self { rule }
    }
}

impl Detector for DotnetBinDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        _ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        let (mut a, inside) = detect_with_rule(&self.rule, dir, platform)?;
        if let Some(why) = bin_problem(&inside) {
            a.base_tier = a.base_tier.max(Tier::S2);
            a.evidence.push(Evidence::Note { text: why });
        }
        Some(a)
    }
}

/// Why a `bin/` listing may hold more than MSBuild output, or `None`.
fn bin_problem(inside: &Listing) -> Option<String> {
    let odd: Vec<&str> = inside
        .iter()
        .filter(|(_, k)| **k != EntryKind::Dir)
        .map(|(n, _)| n.as_str())
        .take(3)
        .collect();
    (!odd.is_empty()).then(|| {
        format!(
            "`{}` sits directly in bin/, where .NET only writes configuration folders \
             (Debug/, Release/): the folder may hold more than build output",
            odd.join("`, `")
        )
    })
}

/// The user `NuGet.Config`.
fn user_config(env: &CacheEnv) -> PathBuf {
    match env.os {
        Os::Windows => env.roaming_app_data().join("NuGet").join("NuGet.Config"),
        Os::Linux | Os::Macos => env.home.join(".nuget").join("NuGet").join("NuGet.Config"),
    }
}

/// Where `dotnet nuget locals global-packages --clear` acts.
pub(crate) fn nuget_spots(
    env: &CacheEnv,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Spots {
    let tool = CacheTool::NuGet;
    if let Some((var, v)) = env.first_var(&["NUGET_PACKAGES"]) {
        let (root, source) = configured(env, tool, &v, CacheSource::Env { var })?;
        return Ok(vec![Spot::new(tool, root, source)]);
    }
    let key = "globalPackagesFolder";
    let source = |file: &Path| CacheSource::Config {
        file: file.to_path_buf(),
        key: key.into(),
    };
    match UserRc::read(user_config(env), platform, ctx) {
        UserRc::Absent => {}
        UserRc::Unreadable { file } => {
            return Err(refused(
                tool,
                None,
                source(&file),
                format!(
                    "{} exists but can't be read as a small regular file, so the effective \
                     packages folder is unknown",
                    file.display()
                ),
            ));
        }
        UserRc::Text { file, text } => {
            if let Some(v) = parse_nuget_global_packages(&text) {
                let raw = expand_percent(&v, env)
                    .map_err(|why| refused(tool, None, source(&file), format!("`{v}`: {why}")))?;
                let raw = relative_to(&raw, &file);
                let (root, source) = configured(env, tool, &raw, source(&file))?;
                return Ok(vec![Spot::new(tool, root, source)]);
            }
        }
    }
    Ok(vec![Spot::new(
        tool,
        env.home.join(".nuget").join("packages"),
        CacheSource::Default,
    )])
}

/// `%NAME%` environment references, as NuGet expands them in config values. An unset variable
/// is an error (the location would be unknown).
fn expand_percent(raw: &str, env: &CacheEnv) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[i..]);
            return Ok(out);
        };
        let name = &after[..end];
        let v = env
            .var(name)
            .ok_or_else(|| format!("references the unset variable `{name}`"))?;
        out.push_str(&v);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A relative config value is relative to the config file's folder (NuGet's rule); `~` and
/// absolute values are left for [`configured`].
fn relative_to(raw: &str, file: &Path) -> String {
    let p = Path::new(raw);
    if p.is_absolute() || raw.starts_with('~') || raw.starts_with("${") {
        return raw.to_owned();
    }
    match file.parent() {
        Some(dir) => dir.join(p).to_string_lossy().into_owned(),
        None => raw.to_owned(),
    }
}

/// The value of `<add key="globalPackagesFolder" value="…" />` in a `NuGet.Config`, outside
/// comments; the key is matched case-insensitively and the last one wins. Pure: the file is
/// data, never executed.
pub(crate) fn parse_nuget_global_packages(text: &str) -> Option<String> {
    let text = strip_xml_comments(text);
    let mut found = None;
    for element in text.split('<').skip(1) {
        let Some(attrs) = element.strip_prefix("add") else {
            continue;
        };
        if !attrs.starts_with(char::is_whitespace) {
            continue;
        }
        let attrs = attrs.split('>').next().unwrap_or_default();
        let key = xml_attr(attrs, "key");
        if key.is_some_and(|k| k.eq_ignore_ascii_case("globalPackagesFolder"))
            && let Some(v) = xml_attr(attrs, "value").filter(|v| !v.trim().is_empty())
        {
            found = Some(v.trim().to_owned());
        }
    }
    found
}

/// The value of attribute `name` in an element's attribute text (`key="x" value='y'`), with
/// the five predefined XML entities decoded.
fn xml_attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    loop {
        let i = rest.find(name)?;
        let before_ok = rest[..i]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let after = rest[i + name.len()..].trim_start();
        if before_ok && let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            let q = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let body = &v[1..];
            let end = body.find(q)?;
            return Some(
                body[..end]
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&amp;", "&"),
            );
        }
        rest = &rest[i + name.len()..];
    }
}

/// The global packages layout: `<id>/<version>/` holding `.nupkg.metadata` or a `.nupkg`
/// (no-follow listings of a few package folders).
pub(crate) fn nuget_markers(
    path: &Path,
    inside: &Listing,
    platform: &dyn Platform,
) -> Vec<PathBuf> {
    let packages = inside
        .iter()
        .filter(|(_, k)| **k == EntryKind::Dir)
        .take(MAX_PACKAGES_PROBED);
    for (id, _) in packages {
        let pkg = path.join(id);
        let Some(versions) = list(platform, &pkg) else {
            continue;
        };
        for (v, _) in versions
            .iter()
            .filter(|(_, k)| **k == EntryKind::Dir)
            .take(2)
        {
            let dir = pkg.join(v);
            let Some(files) = list(platform, &dir) else {
                continue;
            };
            if let Some((f, _)) = files.iter().find(|(f, k)| {
                **k == EntryKind::File && (*f == ".nupkg.metadata" || f.ends_with(".nupkg"))
            }) {
                return vec![dir.join(f)];
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nuget_config_is_parsed_as_data() {
        let cfg = r#"<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <!-- <add key="globalPackagesFolder" value="/commented" /> -->
  <config>
    <add key="GlobalPackagesFolder" value="/data/nuget&amp;pkgs" />
  </config>
  <packageSources>
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" />
  </packageSources>
</configuration>"#;
        assert_eq!(
            parse_nuget_global_packages(cfg),
            Some("/data/nuget&pkgs".into())
        );
        assert_eq!(
            parse_nuget_global_packages("<config><add key='x' value='y'/></config>"),
            None
        );
        assert_eq!(
            parse_nuget_global_packages("<addition key=\"globalPackagesFolder\" value=\"/x\"/>"),
            None
        );
        assert_eq!(
            parse_nuget_global_packages("<add mykey=\"globalPackagesFolder\" value=\"/x\"/>"),
            None
        );
    }

    #[test]
    fn percent_expansion_and_relative_values() {
        let env = CacheEnv::with_vars("/home/u", Os::Linux, [("DATA", "/data")]);
        assert_eq!(expand_percent("%DATA%/nuget", &env).unwrap(), "/data/nuget");
        assert!(expand_percent("%NOPE%/x", &env).is_err());
        assert_eq!(expand_percent("50%", &env).unwrap(), "50%");
        let file = Path::new("/home/u/.nuget/NuGet/NuGet.Config");
        assert_eq!(
            Path::new(&relative_to("pkgs", file)),
            Path::new("/home/u/.nuget/NuGet").join("pkgs")
        );
        assert_eq!(relative_to("~/pkgs", file), "~/pkgs");
    }

    #[test]
    fn bin_with_loose_files_is_flagged() {
        let l = |e: &[(&str, EntryKind)]| -> Listing {
            e.iter().map(|(n, k)| ((*n).to_owned(), *k)).collect()
        };
        assert_eq!(
            bin_problem(&l(&[
                ("Debug", EntryKind::Dir),
                ("Release", EntryKind::Dir)
            ])),
            None
        );
        let why = bin_problem(&l(&[
            ("Debug", EntryKind::Dir),
            ("deploy.sh", EntryKind::File),
        ]))
        .unwrap();
        assert!(why.contains("deploy.sh"));
    }
}
