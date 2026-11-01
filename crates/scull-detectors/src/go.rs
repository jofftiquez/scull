//! Go (P5-DET-01, PRD §7.1, §10.5, §10.8): the build cache and the module cache.
//!
//! Go builds into the build cache and writes binaries to `GOBIN` or the current folder, so a Go
//! project has no build-output folder of its own to detect: both Go artifacts are global package
//! caches ([`crate::caches`], Quick Clean), cleaned only with Go's own commands:
//!
//! | Rule | Cache | Command | Location the command honors |
//! |---|---|---|---|
//! | `go.build-cache` | compiled packages and test results | `go clean -cache` | `GOCACHE` |
//! | `go.modcache` | downloaded module sources (`cache/download`, `<module>@<version>`) | `go clean -modcache` | `GOMODCACHE` (else `$GOPATH/pkg/mod`) |
//!
//! Go writes module files **read-only**, so the module cache is never removed directly (a plain
//! delete fails part way through); `go clean -modcache` makes them writable first.
//!
//! The Go env file (`go env -w`) is read as data, like the other tools' user configuration:
//! `GOENV` names it (`off` disables it), else `go/env` in the user config folder.
//! The environment beats the file, as in Go.

use crate::caches::{
    CacheEnv, CacheSource, CacheTool, RefusedCache, Spot, Spots, UserRc, configured, parse_npmrc,
    refused,
};
use crate::cargo::normalize;
use crate::detector::{DetectContext, Listing, list};
use crate::rules::Platform as Os;
use scull_platform::{EntryKind, Platform};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Evidence note for the module cache.
pub(crate) const MODCACHE_READ_ONLY_NOTE: &str = "Go writes module files read-only, so only \
     `go clean -modcache` removes them; a plain delete would fail part way through";

/// A parsed Go env file: its path and its `KEY=VALUE` settings.
type EnvFile = (PathBuf, BTreeMap<String, String>);

/// The Go env file in effect, or `None` when disabled (`GOENV=off`).
fn env_file(env: &CacheEnv) -> Option<PathBuf> {
    match env.var("GOENV") {
        Some(v) if v == "off" => None,
        Some(v) => Some(PathBuf::from(v)),
        None => Some(match env.os {
            Os::Linux => env.xdg_config().join("go").join("env"),
            Os::Macos => env
                .home
                .join("Library")
                .join("Application Support")
                .join("go")
                .join("env"),
            Os::Windows => env.roaming_app_data().join("go").join("env"),
        }),
    }
}

/// The Go env file's `KEY=VALUE` lines, or why the tool must be refused (it exists but can't be
/// read, so the effective location is unknown). Pure apart from the read: the file is data.
fn read_env_file(
    env: &CacheEnv,
    tool: CacheTool,
    key: &str,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Result<Option<EnvFile>, Box<RefusedCache>> {
    let Some(file) = env_file(env) else {
        return Ok(None);
    };
    let source = CacheSource::Config {
        file: file.clone(),
        key: key.to_owned(),
    };
    if !file.is_absolute() {
        return Err(refused(
            tool,
            None,
            source,
            format!(
                "GOENV `{}` is not an absolute path, so the Go env file in effect is unknown",
                file.display()
            ),
        ));
    }
    match UserRc::read(file, platform, ctx) {
        UserRc::Absent => Ok(None),
        UserRc::Text { file, text } => Ok(Some((file, parse_npmrc(&text)))),
        UserRc::Unreadable { file } => Err(refused(
            tool,
            None,
            source,
            format!(
                "{} exists but can't be read as a small regular file, so the effective cache \
                 location is unknown",
                file.display()
            ),
        )),
    }
}

/// `key` from the environment, else from the Go env file, with where it came from.
fn go_setting(env: &CacheEnv, file: Option<&EnvFile>, key: &str) -> Option<(String, CacheSource)> {
    if let Some(v) = env.var(key) {
        return Some((v, CacheSource::Env { var: key.into() }));
    }
    let (path, values) = file?;
    values.get(key).filter(|v| !v.is_empty()).map(|v| {
        (
            v.clone(),
            CacheSource::Config {
                file: path.clone(),
                key: key.into(),
            },
        )
    })
}

/// Where `go clean -cache` acts.
pub(crate) fn build_cache_spots(
    env: &CacheEnv,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Spots {
    let tool = CacheTool::GoBuild;
    let file = read_env_file(env, tool, "GOCACHE", platform, ctx)?;
    if let Some((v, source)) = go_setting(env, file.as_ref(), "GOCACHE") {
        if v == "off" {
            return Ok(Vec::new()); // the build cache is disabled: nothing to find
        }
        let (root, source) = configured(env, tool, &v, source)?;
        return Ok(vec![Spot::new(tool, root, source)]);
    }
    let root = match env.os {
        Os::Linux => env.xdg_cache().join("go-build"),
        Os::Macos => env.home.join("Library").join("Caches").join("go-build"),
        Os::Windows => env.local_app_data().join("go-build"),
    };
    Ok(vec![Spot::new(tool, root, CacheSource::Default)])
}

/// The `GOPATH` entries (`:`-separated, `;` on Windows); `~/go` when unset.
fn gopath_entries(raw: Option<&str>, env: &CacheEnv) -> Vec<String> {
    match raw {
        Some(v) => {
            let sep = if env.os == Os::Windows { ';' } else { ':' };
            v.split(sep)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        }
        None => vec![env.home.join("go").to_string_lossy().into_owned()],
    }
}

/// Where `go clean -modcache` acts: `GOMODCACHE`, else `pkg/mod` in the first `GOPATH` entry.
///
/// A module cache that **is** a `GOPATH` entry is refused: the GOPATH root also holds `bin/`
/// and, in GOPATH mode, the user's sources (`src/`), and `go clean -modcache` removes the whole
/// module cache folder.
pub(crate) fn modcache_spots(
    env: &CacheEnv,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Spots {
    let tool = CacheTool::GoMod;
    let file = read_env_file(env, tool, "GOMODCACHE", platform, ctx)?;
    let gopath = go_setting(env, file.as_ref(), "GOPATH");
    let entries = gopath_entries(gopath.as_ref().map(|(v, _)| v.as_str()), env);
    let (root, source) = match go_setting(env, file.as_ref(), "GOMODCACHE") {
        Some((v, source)) => configured(env, tool, &v, source)?,
        None => {
            let first = entries.first().cloned().unwrap_or_default();
            let source = gopath
                .as_ref()
                .map_or(CacheSource::Default, |(_, s)| s.clone());
            let (base, source) = configured(env, tool, &first, source)?;
            (base.join("pkg").join("mod"), source)
        }
    };
    let normalized = normalize(&root);
    let is_gopath_root = entries
        .iter()
        .map(PathBuf::from)
        .chain(std::iter::once(env.home.join("go")))
        .any(|g| normalize(&g) == normalized);
    if is_gopath_root {
        return Err(refused(
            tool,
            Some(root.clone()),
            source,
            format!(
                "{} is a GOPATH root (it also holds `bin/` and possibly sources), not a module \
                 cache of its own",
                root.display()
            ),
        ));
    }
    Ok(vec![Spot::new(tool, root, source)])
}

/// Whether `name` is a two-digit lowercase hex bucket (`00` … `ff`).
fn is_hex_bucket(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The build cache layout: `trim.txt` (Go's trim timestamp), or the `README` Go writes together
/// with at least one `00`…`ff` bucket.
pub(crate) fn build_cache_markers(inside: &Listing) -> Vec<&str> {
    let file = |n: &str| inside.get(n) == Some(&EntryKind::File);
    let bucket = inside
        .iter()
        .find(|(n, k)| **k == EntryKind::Dir && is_hex_bucket(n))
        .map(|(n, _)| n.as_str());
    match (file("trim.txt"), file("README"), bucket) {
        (true, _, b) => std::iter::once("trim.txt").chain(b).collect(),
        (false, true, Some(b)) => vec!["README", b],
        _ => Vec::new(),
    }
}

/// The module cache layout: `cache/download` (where Go keeps downloaded zips and `.info`/`.mod`
/// files), checked with no-follow listings.
pub(crate) fn modcache_markers(
    path: &Path,
    inside: &Listing,
    platform: &dyn Platform,
) -> Vec<PathBuf> {
    if inside.get("cache") != Some(&EntryKind::Dir) {
        return Vec::new();
    }
    let cache = path.join("cache");
    match list(platform, &cache) {
        Some(l) if l.get("download") == Some(&EntryKind::Dir) => vec![cache.join("download")],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(entries: &[(&str, EntryKind)]) -> Listing {
        entries.iter().map(|(n, k)| ((*n).to_owned(), *k)).collect()
    }

    #[test]
    fn build_cache_layout() {
        use EntryKind::{Dir, File};
        assert_eq!(
            build_cache_markers(&listing(&[("trim.txt", File), ("0a", Dir)])),
            vec!["trim.txt", "0a"]
        );
        assert_eq!(
            build_cache_markers(&listing(&[("README", File), ("ff", Dir)])),
            vec!["README", "ff"]
        );
        assert!(build_cache_markers(&listing(&[("README", File)])).is_empty());
        assert!(build_cache_markers(&listing(&[("README", File), ("FF", Dir)])).is_empty());
        assert!(build_cache_markers(&listing(&[("trim.txt", Dir), ("0a", File)])).is_empty());
    }

    #[test]
    fn gopath_is_split_per_os() {
        let unix = CacheEnv::with_vars("/home/u", Os::Linux, Vec::<(String, String)>::new());
        assert_eq!(
            gopath_entries(Some("/a:/b"), &unix),
            vec!["/a".to_owned(), "/b".to_owned()]
        );
        assert_eq!(gopath_entries(None, &unix), vec!["/home/u/go".to_owned()]);
        let win = CacheEnv::with_vars("C:\\Users\\u", Os::Windows, Vec::<(String, String)>::new());
        assert_eq!(
            gopath_entries(Some("C:\\go;D:\\work"), &win),
            vec!["C:\\go".to_owned(), "D:\\work".to_owned()]
        );
    }
}
