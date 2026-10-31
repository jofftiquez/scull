//! JavaScript / TypeScript detectors (CLS-02): what the `node.*` rules can't express as data.
//!
//! * [`NodeModulesDetector`] (`node.node-modules`): records the symlinks/junctions directly in
//!   `node_modules` and in its `@scope/` directories (workspace links, `npm link`). They are
//!   never followed: removing `node_modules` removes the links themselves, never their targets
//!   (FX-CLS-06). Nested `node_modules` are part of the outer one (the rule's `exclude.within`).
//! * [`FrameworkDetector`] (`.next`, `.nuxt`, `.output`, `.turbo`, `.parcel-cache`): the rule's
//!   config-file markers (`next.config.*`, `nuxt.config.*`, `turbo.json`, ...) are required, or,
//!   failing those, the project's `package.json` declaring the framework as a dependency (read
//!   as data through a [`ConfigReader`]). A generic name with only a `package.json` beside it is
//!   never enough (CLS-03).
//! * Both resolve **workspace lockfiles**: a package in a monorepo has no lockfile beside it,
//!   so when the rule finds none, the nearest ancestor (up to [`MAX_WORKSPACE_UP`] levels) that
//!   holds a lockfile is checked, and its lockfile counts only if that ancestor's workspace
//!   declaration (`pnpm-workspace.yaml` `packages:`, or `workspaces` in its `package.json`)
//!   includes the package. Otherwise the artifact stays "no lockfile" (S2).

use crate::detector::{
    ConfigReader, DetectContext, DetectedArtifact, Detector, Listing, MAX_CONFIG_BYTES,
    artifact_from_match, detect_with_rule, gather, list,
};
use crate::rules::{MarkerOutcome, NamePattern, Rule};
use scull_core::{Evidence, FsPath};
use scull_platform::{EntryKind, Platform};
use std::path::{Component, Path};

/// Lockfile names of npm, pnpm, Yarn and Bun.
pub const NODE_LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

/// How many directories above a project the workspace root may be.
pub const MAX_WORKSPACE_UP: usize = 6;

/// At most this many link names are listed in the evidence.
const MAX_LINKS_SHOWN: usize = 5;

/// Most segments a workspace glob may have.
const MAX_GLOB_SEGMENTS: usize = 32;

/// Framework rules and the package names that identify them in a `package.json`.
const FRAMEWORK_DEPS: &[(&str, &[&str])] = &[
    ("node.next", &["next"]),
    ("node.nuxt", &["nuxt", "nuxt3", "nuxt-nightly"]),
    (
        "node.output",
        &["nuxt", "nuxt3", "nuxt-nightly", "nitropack"],
    ),
    ("node.turbo", &["turbo"]),
    (
        "node.parcel-cache",
        &["parcel", "@parcel/core", "parcel-bundler"],
    ),
];

/// The ecosystem detector for `rule`, if it needs more than [`crate::RuleDetector`].
pub(crate) fn detector_for(rule: &Rule) -> Option<Box<dyn Detector>> {
    if rule.id() == "node.node-modules" {
        return Some(Box::new(NodeModulesDetector::new(rule.clone())));
    }
    FRAMEWORK_DEPS
        .iter()
        .find(|(id, _)| *id == rule.id())
        .map(|(_, deps)| Box::new(FrameworkDetector::new(rule.clone(), deps)) as Box<dyn Detector>)
}

/// `node_modules` with link awareness and workspace lockfiles.
#[derive(Debug, Clone)]
pub struct NodeModulesDetector {
    rule: Rule,
}

impl NodeModulesDetector {
    /// A detector for the `node.node-modules` rule (or a rule shaped like it).
    pub fn new(rule: Rule) -> Self {
        Self { rule }
    }
}

impl Detector for NodeModulesDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        let (mut a, inside) = detect_with_rule(&self.rule, dir, platform)?;
        resolve_workspace_lockfile(&mut a, dir.parent()?, platform, ctx);
        record_links(&mut a, dir, &inside, platform);
        Some(a)
    }
}

/// A framework output directory: config-file markers, or the framework as a dependency.
#[derive(Debug, Clone)]
pub struct FrameworkDetector {
    rule: Rule,
    /// The rule with only `../package.json` as marker, used with the dependency check.
    relaxed: Option<Rule>,
    deps: &'static [&'static str],
}

impl FrameworkDetector {
    /// A detector for `rule`, also accepting a `package.json` that depends on one of `deps`.
    pub fn new(rule: Rule, deps: &'static [&'static str]) -> Self {
        let mut relaxed = rule.clone();
        relaxed
            .markers_all
            .retain(|m| m.up() == 1 && m.name().as_str() == "package.json");
        relaxed.markers_any.clear();
        let relaxed = (!relaxed.markers_all.is_empty()).then_some(relaxed);
        Self {
            rule,
            relaxed,
            deps,
        }
    }
}

impl Detector for FrameworkDetector {
    fn id(&self) -> &str {
        self.rule.id()
    }

    fn detect(
        &self,
        dir: &Path,
        platform: &dyn Platform,
        ctx: &DetectContext<'_>,
    ) -> Option<DetectedArtifact> {
        let g = gather(&self.rule, dir, platform)?;
        let project = dir.parent()?;
        let mut a = match self.rule.check_markers(&g.surroundings) {
            MarkerOutcome::Matched(m) => {
                artifact_from_match(&self.rule, dir, g.identity, &m, &g.inside)
            }
            MarkerOutcome::MissingMarker { .. } => {
                let relaxed = self.relaxed.as_ref()?;
                let outcome = relaxed.check_markers(&g.surroundings);
                let m = outcome.matched()?;
                let reader = ctx.config_reader?;
                let manifest = project.join("package.json");
                let dep = declared_dependency(platform, reader, &manifest, self.deps)?;
                let mut a = artifact_from_match(&self.rule, dir, g.identity, m, &g.inside);
                a.evidence.push(Evidence::Note {
                    text: format!("{} declares a dependency on `{dep}`", manifest.display()),
                });
                a
            }
            _ => return None,
        };
        resolve_workspace_lockfile(&mut a, project, platform, ctx);
        Some(a)
    }
}

/// Record links directly inside `node_modules` and its `@scope/` directories.
fn record_links(a: &mut DetectedArtifact, dir: &Path, inside: &Listing, platform: &dyn Platform) {
    let mut links: Vec<String> = Vec::new();
    for (name, kind) in inside {
        if kind.is_link() {
            links.push(name.clone());
        } else if *kind == EntryKind::Dir
            && name.starts_with('@')
            && let Some(scoped) = list(platform, &dir.join(name))
        {
            links.extend(
                scoped
                    .iter()
                    .filter(|(_, k)| k.is_link())
                    .map(|(n, _)| format!("{name}/{n}")),
            );
        }
    }
    if links.is_empty() {
        return; // deeper levels weren't checked: stays "unknown", never "no links"
    }
    a.contains_links = Some(true);
    let shown: Vec<&str> = links
        .iter()
        .take(MAX_LINKS_SHOWN)
        .map(String::as_str)
        .collect();
    let more = links.len().saturating_sub(MAX_LINKS_SHOWN);
    let suffix = if more > 0 {
        format!(", and {more} more")
    } else {
        String::new()
    };
    a.evidence.push(Evidence::Note {
        text: format!(
            "contains {} symlink(s)/junction(s) ({}{suffix}); removal deletes the links \
             themselves, their targets are never followed or removed",
            links.len(),
            shown.join(", ")
        ),
    });
}

/// If the artifact has no lockfile beside its project, accept the nearest ancestor's lockfile
/// when that ancestor's workspace declaration includes the project.
fn resolve_workspace_lockfile(
    a: &mut DetectedArtifact,
    project: &Path,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) {
    if a.signals.lockfile_present {
        return;
    }
    let Some(reader) = ctx.config_reader else {
        return;
    };
    for root in project.ancestors().skip(1).take(MAX_WORKSPACE_UP) {
        let Some(listing) = list(platform, root) else {
            return; // unreadable: unknown, stay protective
        };
        let Some(lock) = NODE_LOCKFILES
            .iter()
            .find(|l| listing.get(**l) == Some(&EntryKind::File))
        else {
            continue;
        };
        // The nearest lockfile governs; whether or not the project is a member, stop here.
        let Some(rel) = relative_segments(project, root) else {
            return;
        };
        let Some((patterns, declared_in)) =
            workspace_patterns(root, &listing, lock, platform, reader)
        else {
            return;
        };
        if is_member(&patterns, &rel) {
            a.signals.lockfile_present = true;
            a.evidence.push(Evidence::Lockfile {
                path: FsPath::from_path(&root.join(lock)),
            });
            a.evidence.push(Evidence::Note {
                text: format!(
                    "workspace member of {} (declared in {})",
                    root.display(),
                    declared_in
                ),
            });
        }
        return;
    }
}

/// The workspace globs declared at `root`, and the file name they came from.
fn workspace_patterns(
    root: &Path,
    listing: &Listing,
    lock: &str,
    platform: &dyn Platform,
    reader: &dyn ConfigReader,
) -> Option<(Vec<String>, &'static str)> {
    if lock == "pnpm-lock.yaml" {
        if listing.get("pnpm-workspace.yaml") != Some(&EntryKind::File) {
            return None;
        }
        let text = read_small(platform, reader, &root.join("pnpm-workspace.yaml"))?;
        return Some((parse_pnpm_workspace(&text), "pnpm-workspace.yaml"));
    }
    if listing.get("package.json") != Some(&EntryKind::File) {
        return None;
    }
    let text = read_small(platform, reader, &root.join("package.json"))?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let ws = v.get("workspaces")?;
    let list = ws.as_array().or_else(|| ws.get("packages")?.as_array())?;
    Some((
        list.iter()
            .filter_map(|p| p.as_str().map(str::to_owned))
            .collect(),
        "package.json",
    ))
}

/// The `packages:` list of a `pnpm-workspace.yaml`. A deliberately small reader for the two
/// shapes pnpm documents (block list and inline `[...]`); anything else yields no patterns,
/// so the package is not treated as a member (the protective answer).
pub(crate) fn parse_pnpm_workspace(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let top_level = !line.starts_with([' ', '\t', '-']);
        if top_level {
            in_packages = false;
            if let Some(rest) = line.strip_prefix("packages:") {
                let rest = strip_comment(rest).trim();
                if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    out.extend(inner.split(',').map(unquote).filter(|s| !s.is_empty()));
                } else {
                    in_packages = rest.is_empty();
                }
            }
            continue;
        }
        if in_packages && let Some(item) = trimmed.strip_prefix('-') {
            let item = unquote(item);
            if !item.is_empty() {
                out.push(item);
            }
        }
    }
    out
}

fn strip_comment(s: &str) -> &str {
    match s.find(" #") {
        Some(i) => &s[..i],
        None => s,
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    match s.chars().next() {
        // Quoted: the value ends at the closing quote (anything after it is a comment).
        Some(q @ ('\'' | '"')) => s[1..]
            .find(q)
            .map(|end| s[1..1 + end].trim().to_owned())
            .unwrap_or_default(),
        _ => strip_comment(s).trim().to_owned(),
    }
}

/// Whether the project at `rel` (segments below the workspace root) is included by the
/// patterns: at least one positive pattern matches and no `!` pattern does.
pub(crate) fn is_member(patterns: &[String], rel: &[String]) -> bool {
    let rel: Vec<&str> = rel.iter().map(String::as_str).collect();
    let mut included = false;
    for p in patterns {
        let (negated, p) = match p.trim().strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, p.trim()),
        };
        let p = p.trim_start_matches("./").trim_end_matches('/');
        if p.is_empty() {
            continue;
        }
        let segs: Vec<&str> = p
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        if segs.len() > MAX_GLOB_SEGMENTS || segs.contains(&"..") {
            continue;
        }
        if glob_match(&segs, &rel) {
            if negated {
                return false;
            }
            included = true;
        }
    }
    included
}

fn glob_match(pat: &[&str], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            glob_match(rest, path) || (!path.is_empty() && glob_match(pat, &path[1..]))
        }
        Some((seg, rest)) => match path.split_first() {
            Some((name, tail)) => segment_match(seg, name) && glob_match(rest, tail),
            None => false,
        },
    }
}

fn segment_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains(['*', '?', '{']) {
        return pattern == name;
    }
    NamePattern::parse(pattern).is_ok_and(|p| p.matches(name))
}

/// `path` below `root` as UTF-8 segments, or `None` if it isn't below it.
fn relative_segments(path: &Path, root: &Path) -> Option<Vec<String>> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// The first of `deps` that `manifest` (a `package.json`) declares in `dependencies`,
/// `devDependencies` or `optionalDependencies`.
fn declared_dependency(
    platform: &dyn Platform,
    reader: &dyn ConfigReader,
    manifest: &Path,
    deps: &[&'static str],
) -> Option<&'static str> {
    let text = read_small(platform, reader, manifest)?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let sections = ["dependencies", "devDependencies", "optionalDependencies"];
    deps.iter().copied().find(|d| {
        sections
            .iter()
            .any(|s| v.get(s).and_then(|o| o.get(d)).is_some())
    })
}

pub(crate) fn read_small(
    platform: &dyn Platform,
    reader: &dyn ConfigReader,
    path: &Path,
) -> Option<String> {
    let md = platform.metadata(path).ok()?;
    if md.kind != EntryKind::File || md.logical_size > MAX_CONFIG_BYTES {
        return None;
    }
    reader.read_text(path, MAX_CONFIG_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(s: &str) -> Vec<String> {
        s.split('/').map(str::to_owned).collect()
    }

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn pnpm_workspace_shapes() {
        let block = "packages:\n  - 'packages/*'\n  - \"apps/**\" # apps\n  - '!**/test/**'\n\ncatalog:\n  - nope\n";
        assert_eq!(
            parse_pnpm_workspace(block),
            ["packages/*", "apps/**", "!**/test/**"]
        );
        assert_eq!(
            parse_pnpm_workspace("packages: ['a/*', \"b\"]\n"),
            ["a/*", "b"]
        );
        assert_eq!(parse_pnpm_workspace("packages:\n- tools/*\n"), ["tools/*"]);
        assert!(parse_pnpm_workspace("onlyBuiltDependencies:\n  - esbuild\n").is_empty());
    }

    #[test]
    fn workspace_membership() {
        let p = pats(&["packages/*", "apps/**", "!apps/legacy"]);
        assert!(is_member(&p, &segs("packages/ui")));
        assert!(!is_member(&p, &segs("packages/ui/nested")));
        assert!(is_member(&p, &segs("apps/web")));
        assert!(is_member(&p, &segs("apps/a/b")));
        assert!(!is_member(&p, &segs("apps/legacy")));
        assert!(!is_member(&p, &segs("tools/x")));
        assert!(is_member(&pats(&["./libs/{a,b}/"]), &segs("libs/b")));
        assert!(!is_member(&pats(&["../x"]), &segs("x")));
        assert!(!is_member(&[], &segs("packages/ui")));
    }
}
