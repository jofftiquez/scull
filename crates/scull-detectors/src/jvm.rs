//! Java / Kotlin (P5-DET-01, PRD §7.1, §10.8): Gradle and Maven caches.
//!
//! The project build folders (`build/` and `.gradle/` beside a Gradle build script, `target/`
//! beside a `pom.xml`) are plain rules (`rules/ecosystems/jvm.toml`): each needs the build
//! file beside it **and** entries only the build tool writes inside it (CLS-03).
//!
//! # Caches: located, measured, inspect-only
//!
//! | Rule | Cache | Overrides | Default |
//! |---|---|---|---|
//! | `gradle.caches` | `$GRADLE_USER_HOME/caches` (dependencies, transforms, the local build cache) | `GRADLE_USER_HOME` | `~/.gradle/caches` |
//! | `maven.repository` | the local repository | `-Dmaven.repo.local=` in `MAVEN_OPTS` / `MAVEN_ARGS`; `<localRepository>` in `~/.m2/settings.xml` | `~/.m2/repository` |
//!
//! Neither tool has a command that cleans its cache (QC-03 asks for the tool's own command):
//! Gradle has no prune or wipe command (it removes unused entries by itself), and Maven's
//! `dependency:purge-local-repository` is a per-project plugin goal that re-resolves (downloads)
//! what it removes. Removing the folders directly is not a Quick Clean action either: D-36
//! allows scull's own removal only for a cache whose tool is **not** installed, and only when
//! the classified action is the tool's command. So both caches are shown with their size and
//! location, and classified **S3, inspect-only** (no action). Maven's repository is also not
//! purely a cache: `mvn install` puts locally built artifacts there that may exist nowhere else.

use crate::caches::{CacheEnv, CacheSource, CacheTool, Spot, Spots, UserRc, configured, refused};
use crate::detector::{DetectContext, Listing};
use scull_platform::{EntryKind, Platform};
use std::path::PathBuf;

/// Evidence note for the Gradle caches.
pub(crate) const GRADLE_NOTE: &str = "Gradle has no command that cleans its caches; it removes \
     entries unused for a while by itself (30 days by default). With every Gradle daemon stopped \
     (`./gradlew --stop`) the folder can be removed by hand; scull doesn't offer that";

/// Evidence note for the Maven local repository.
pub(crate) const MAVEN_NOTE: &str = "Maven has no command that cleans its local repository, and \
     it may also hold artifacts installed from local projects (`mvn install`) that exist nowhere \
     else; scull only shows it";

/// Where the Gradle caches are: `$GRADLE_USER_HOME/caches`, else `~/.gradle/caches`.
pub(crate) fn gradle_spots(env: &CacheEnv) -> Vec<Spot> {
    let tool = CacheTool::Gradle;
    let (root, source) = match env.first_var(&["GRADLE_USER_HOME"]) {
        // Gradle takes the value literally; relative values are refused by validation.
        Some((var, v)) => (PathBuf::from(v), CacheSource::Env { var }),
        None => (env.home.join(".gradle"), CacheSource::Default),
    };
    vec![Spot::new(tool, root, source).sub("caches")]
}

/// Where the Maven local repository is.
pub(crate) fn maven_spots(
    env: &CacheEnv,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Spots {
    let tool = CacheTool::Maven;
    for var in ["MAVEN_ARGS", "MAVEN_OPTS"] {
        if let Some(v) = env.var(var).as_deref().and_then(repo_local_property) {
            let raw = maven_properties(&v, env);
            let (root, source) = configured(env, tool, &raw, CacheSource::Env { var: var.into() })?;
            return Ok(vec![Spot::new(tool, root, source)]);
        }
    }
    let settings = env.home.join(".m2").join("settings.xml");
    let key = "localRepository";
    match UserRc::read(settings, platform, ctx) {
        UserRc::Absent => {}
        UserRc::Text { file, text } => {
            if let Some(v) = parse_maven_local_repository(&text) {
                let raw = maven_properties(&v, env);
                let source = CacheSource::Config {
                    file,
                    key: key.into(),
                };
                let (root, source) = configured(env, tool, &raw, source)?;
                return Ok(vec![Spot::new(tool, root, source)]);
            }
        }
        UserRc::Unreadable { file } => {
            return Err(refused(
                tool,
                None,
                CacheSource::Config {
                    file: file.clone(),
                    key: key.into(),
                },
                format!(
                    "{} exists but can't be read as a small regular file, so the effective \
                     repository location is unknown",
                    file.display()
                ),
            ));
        }
    }
    let root = env.home.join(".m2");
    Ok(vec![
        Spot::new(tool, root, CacheSource::Default).sub("repository"),
    ])
}

/// The value of `-Dmaven.repo.local=…` in a `MAVEN_OPTS` / `MAVEN_ARGS` string (last one wins,
/// as with JVM system properties). Quotes around the value are removed.
pub(crate) fn repo_local_property(opts: &str) -> Option<String> {
    opts.split_whitespace()
        .filter_map(|t| t.strip_prefix("-Dmaven.repo.local="))
        .map(|v| v.trim_matches(['"', '\'']).to_owned())
        .rfind(|v| !v.is_empty())
}

/// Maven's property syntax in a configured path: `${user.home}` is the home folder and
/// `${env.NAME}` an environment variable (left as `${NAME}` for [`crate::caches::expand`], which
/// refuses unset ones). Other properties stay as written and are refused there.
fn maven_properties(raw: &str, env: &CacheEnv) -> String {
    raw.replace("${user.home}", &env.home.to_string_lossy())
        .replace("${env.", "${")
}

/// Remove XML comments (`<!-- … -->`); an unterminated comment drops the rest.
pub(crate) fn strip_xml_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i + 4..].find("-->") {
            Some(j) => rest = &rest[i + 4 + j + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The `<localRepository>` of a Maven `settings.xml` (outside comments; the shipped default
/// has it commented out). Pure: the file is data, never executed.
pub(crate) fn parse_maven_local_repository(text: &str) -> Option<String> {
    let text = strip_xml_comments(text);
    let start = text.find("<localRepository>")? + "<localRepository>".len();
    let end = text[start..].find("</localRepository>")?;
    let v = text[start..start + end].trim();
    (!v.is_empty()).then(|| v.to_owned())
}

/// The Gradle user home's cache layout: `modules-2` (dependencies), `files-2.1`,
/// `build-cache-1`, or versioned `transforms-N` / `jars-N` folders.
pub(crate) fn gradle_markers(inside: &Listing) -> Vec<&str> {
    let versioned = |n: &str, prefix: &str| {
        n.strip_prefix(prefix)
            .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
    };
    inside
        .iter()
        .filter(|(_, k)| **k == EntryKind::Dir)
        .map(|(n, _)| n.as_str())
        .filter(|n| {
            matches!(*n, "modules-2" | "files-2.1" | "build-cache-1")
                || versioned(n, "transforms-")
                || versioned(n, "jars-")
        })
        .collect()
}

/// The Maven repository layout: group folders such as `org/`, `com/`, `io/`.
pub(crate) fn maven_markers(inside: &Listing) -> Vec<&str> {
    inside
        .iter()
        .filter(|(n, k)| {
            **k == EntryKind::Dir
                && matches!(
                    n.as_str(),
                    "org" | "com" | "io" | "net" | "javax" | "jakarta"
                )
        })
        .map(|(n, _)| n.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Platform as Os;

    #[test]
    fn maven_settings_are_parsed_as_data() {
        let shipped = "<settings>\n  <!-- localRepository\n   | Default: ${user.home}/.m2/repository\n  <localRepository>/path/to/local/repo</localRepository>\n  -->\n</settings>";
        assert_eq!(parse_maven_local_repository(shipped), None);
        let set = "<settings><localRepository> /data/m2 </localRepository></settings>";
        assert_eq!(parse_maven_local_repository(set), Some("/data/m2".into()));
        assert_eq!(
            parse_maven_local_repository("<localRepository></localRepository>"),
            None
        );
        assert_eq!(parse_maven_local_repository("<!-- open"), None);
    }

    #[test]
    fn maven_opts_property() {
        assert_eq!(
            repo_local_property("-Xmx2g -Dmaven.repo.local=/a -Dmaven.repo.local=\"/b\""),
            Some("/b".into())
        );
        assert_eq!(repo_local_property("-Xmx2g"), None);
        let env = CacheEnv::with_vars("/home/u", Os::Linux, Vec::<(String, String)>::new());
        assert_eq!(
            maven_properties("${user.home}/m2/${env.X}", &env),
            "/home/u/m2/${X}"
        );
    }

    #[test]
    fn gradle_layout() {
        let l: Listing = [
            ("modules-2", EntryKind::Dir),
            ("transforms-4", EntryKind::Dir),
            ("transforms-x", EntryKind::Dir),
            ("jars-9", EntryKind::File),
            ("8.10", EntryKind::Dir),
        ]
        .iter()
        .map(|(n, k)| ((*n).to_owned(), *k))
        .collect();
        assert_eq!(gradle_markers(&l), vec!["modules-2", "transforms-4"]);
    }
}
