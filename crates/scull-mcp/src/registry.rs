//! Worktree TTL registrations (PRO-08, AI-03): "this agent worktree should be gone in 7 days".
//!
//! An agent (MCP `register_worktree`) or a script (`scull worktrees register <path> --ttl 7d`)
//! records how long a worktree is expected to live. **Expiry never deletes anything**: an
//! expired registration only makes the worktree a review suggestion (FX-MCP-10).
//!
//! # Storage
//!
//! Registrations live in their own small file, `<data dir>/worktree-ttl.jsonl`, not in the
//! SQLite database. That keeps this metadata out of the schema migrations (main and the phase2
//! branch number migrations independently) and lets the MCP server keep the history database
//! strictly read-only. The file is:
//!
//! * **append-only**: one JSON object per line ([`Registration`]); the last line for a path
//!   wins. When it grows past [`COMPACT_AT_BYTES`] it is rewritten once with only the latest
//!   line per path (owner-only temp file, then rename).
//! * **owner-only**, created with [`scull_platform::owner_only::create_file`]; a symlink at its
//!   path is refused, never followed.
//! * **bounded**: at most [`MAX_READ_BYTES`] are read; unparsable lines are skipped.
//!
//! Paths are stored canonical and absolute. Nothing here touches the worktree itself.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// File name inside scull's data directory.
pub const REGISTRY_FILE: &str = "worktree-ttl.jsonl";
/// Shortest TTL accepted (one minute).
pub const MIN_TTL_SECS: u64 = 60;
/// Longest TTL accepted (366 days).
pub const MAX_TTL_SECS: u64 = 366 * 86_400;
/// At most this much of the file is read.
pub const MAX_READ_BYTES: u64 = 1024 * 1024;
/// Above this size the file is compacted on the next write.
pub const COMPACT_AT_BYTES: u64 = 256 * 1024;
/// Format version written in every line.
pub const FORMAT_VERSION: u32 = 1;

/// Who registered the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `scull worktrees register`.
    Cli,
    /// The MCP tool `register_worktree`.
    Mcp,
}

/// One registration (one line of the file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    /// Format version ([`FORMAT_VERSION`]).
    pub v: u32,
    /// The worktree folder, canonical and absolute.
    pub path: String,
    /// Time to live, seconds.
    pub ttl_secs: u64,
    /// When it was registered (Unix seconds).
    pub registered_at: i64,
    /// `registered_at + ttl_secs`.
    pub expires_at: i64,
    /// Who registered it.
    pub source: Source,
}

impl Registration {
    /// Whether the TTL has run out at `now`.
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// Why the registry could not be read or written.
#[derive(Debug)]
pub enum RegistryError {
    /// An I/O error on the registry file or its folder.
    Io(PathBuf, io::Error),
    /// The registry path is a symlink or not a regular file.
    NotAFile(PathBuf),
    /// The worktree path cannot be stored (not valid UTF-8).
    BadPath(PathBuf),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Io(p, e) => write!(f, "{}: {e}", p.display()),
            RegistryError::NotAFile(p) => write!(
                f,
                "{} is not a regular file (links are never followed)",
                p.display()
            ),
            RegistryError::BadPath(p) => {
                write!(
                    f,
                    "{} is not valid UTF-8 and cannot be registered",
                    p.display()
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Parse a TTL such as `30m`, `12h`, `7d` or `2w` (a positive whole number and one unit).
pub fn parse_ttl(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("`{s}`: add a unit: m (minutes), h, d or w, e.g. 7d"))?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("`{s}`: expected a whole number then m, h, d or w, e.g. 7d"))?;
    let mult = match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(format!("`{s}`: unknown unit `{unit}`; use m, h, d or w")),
    };
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| format!("`{s}` is too long"))?;
    if secs < MIN_TTL_SECS {
        return Err(format!("`{s}` is shorter than the minimum of 1m"));
    }
    if secs > MAX_TTL_SECS {
        return Err(format!("`{s}` is longer than the maximum of 366d"));
    }
    Ok(secs)
}

/// A short human duration: `3d 4h`, `5h 10m`, `12m`.
pub fn human_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// The registry file in a data directory.
#[derive(Debug, Clone)]
pub struct Registry {
    file: PathBuf,
}

impl Registry {
    /// The registry inside scull's data directory (nothing is created until a write).
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self {
            file: data_dir.join(REGISTRY_FILE),
        }
    }

    /// The file path.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The latest registration per path, sorted by path. A missing file is empty.
    pub fn load(&self) -> Result<Vec<Registration>, RegistryError> {
        let text = match read_bounded(&self.file)? {
            Some(t) => t,
            None => return Ok(Vec::new()),
        };
        Ok(latest(&text).into_values().collect())
    }

    /// Append a registration of `worktree` (canonical, absolute) with `ttl_secs` at `now`.
    pub fn record(
        &self,
        worktree: &Path,
        ttl_secs: u64,
        now: i64,
        source: Source,
    ) -> Result<Registration, RegistryError> {
        let path = worktree
            .to_str()
            .ok_or_else(|| RegistryError::BadPath(worktree.to_path_buf()))?
            .to_owned();
        let reg = Registration {
            v: FORMAT_VERSION,
            path,
            ttl_secs,
            registered_at: now,
            expires_at: now.saturating_add(i64::try_from(ttl_secs).unwrap_or(i64::MAX)),
            source,
        };
        let io_err = |p: &Path, e| RegistryError::Io(p.to_path_buf(), e);
        if let Some(dir) = self.file.parent() {
            scull_platform::create_dir_owner_only(dir)
                .map_err(|e| io_err(dir, io::Error::other(e.to_string())))?;
        }
        let mut line = serde_json::to_string(&reg)
            .map_err(|e| io_err(&self.file, io::Error::other(e.to_string())))?;
        line.push('\n');
        match std::fs::symlink_metadata(&self.file) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut f = scull_platform::owner_only::create_file(&self.file)
                    .map_err(|e| io_err(&self.file, e))?;
                f.write_all(line.as_bytes())
                    .and_then(|()| f.sync_all())
                    .map_err(|e| io_err(&self.file, e))?;
            }
            Err(e) => return Err(io_err(&self.file, e)),
            Ok(md) if !md.file_type().is_file() => {
                return Err(RegistryError::NotAFile(self.file.clone()));
            }
            Ok(md) if md.len() >= COMPACT_AT_BYTES => {
                let text = read_bounded(&self.file)?.unwrap_or_default();
                let mut map = latest(&text);
                map.insert(reg.path.clone(), reg.clone());
                self.rewrite(map.values())?;
            }
            Ok(_) => {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&self.file)
                    .map_err(|e| io_err(&self.file, e))?;
                f.write_all(line.as_bytes())
                    .and_then(|()| f.sync_all())
                    .map_err(|e| io_err(&self.file, e))?;
            }
        }
        Ok(reg)
    }

    fn rewrite<'a>(
        &self,
        regs: impl Iterator<Item = &'a Registration>,
    ) -> Result<(), RegistryError> {
        let mut text = String::new();
        for r in regs {
            if let Ok(l) = serde_json::to_string(r) {
                text.push_str(&l);
                text.push('\n');
            }
        }
        let tmp = self
            .file
            .with_file_name(format!(".{REGISTRY_FILE}.tmp-{}", std::process::id()));
        let res = (|| {
            let mut f = scull_platform::owner_only::create_file(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &self.file)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res.map_err(|e| RegistryError::Io(self.file.clone(), e))
    }
}

/// The file's text (at most [`MAX_READ_BYTES`]), `None` if missing. Never follows a link.
fn read_bounded(file: &Path) -> Result<Option<String>, RegistryError> {
    match std::fs::symlink_metadata(file) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(RegistryError::Io(file.to_path_buf(), e)),
        Ok(md) if !md.file_type().is_file() => {
            return Err(RegistryError::NotAFile(file.to_path_buf()));
        }
        Ok(_) => {}
    }
    let mut buf = Vec::new();
    std::fs::File::open(file)
        .and_then(|f| f.take(MAX_READ_BYTES).read_to_end(&mut buf))
        .map_err(|e| RegistryError::Io(file.to_path_buf(), e))?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Last valid line per path.
fn latest(text: &str) -> BTreeMap<String, Registration> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        if let Ok(r) = serde_json::from_str::<Registration>(line)
            && r.v == FORMAT_VERSION
            && Path::new(&r.path).is_absolute()
        {
            map.insert(r.path.clone(), r);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttls_parse() {
        assert_eq!(parse_ttl("7d"), Ok(7 * 86_400));
        assert_eq!(parse_ttl("12h"), Ok(12 * 3600));
        assert_eq!(parse_ttl("30m"), Ok(1800));
        assert_eq!(parse_ttl("2w"), Ok(14 * 86_400));
        assert_eq!(parse_ttl(" 1d "), Ok(86_400));
        for bad in [
            "",
            "7",
            "d",
            "7x",
            "-1d",
            "0m",
            "30s",
            "400d",
            "1.5d",
            "99999999999999999999w",
        ] {
            assert!(parse_ttl(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(human_duration(90), "1m");
        assert_eq!(human_duration(7200), "2h");
        assert_eq!(human_duration(3 * 86_400 + 4 * 3600), "3d 4h");
        assert_eq!(human_duration(5 * 3600 + 600), "5h 10m");
    }

    #[test]
    fn record_load_latest_wins_and_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let reg = Registry::in_data_dir(&data);
        assert!(reg.load().unwrap().is_empty());
        let wt = dir.path().join("wt");
        reg.record(&wt, 3600, 1000, Source::Cli).unwrap();
        reg.record(&dir.path().join("other"), 60, 1000, Source::Mcp)
            .unwrap();
        let r = reg.record(&wt, 86_400, 2000, Source::Mcp).unwrap();
        assert_eq!(r.expires_at, 2000 + 86_400);
        let all = reg.load().unwrap();
        assert_eq!(all.len(), 2);
        let w = all.iter().find(|x| x.path == wt.to_str().unwrap()).unwrap();
        assert_eq!(w, &r);
        assert!(!w.is_expired(2000));
        assert!(w.is_expired(2000 + 86_400));
        // Append-only: three lines on disk.
        let text = std::fs::read_to_string(reg.file()).unwrap();
        assert_eq!(text.lines().count(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(reg.file()).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[test]
    fn garbage_lines_are_skipped_and_big_files_compact() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registry::in_data_dir(dir.path());
        let wt = dir.path().join("wt");
        reg.record(&wt, 3600, 1, Source::Cli).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(reg.file())
            .unwrap();
        writeln!(f, "not json").unwrap();
        writeln!(f, "{{\"v\":1,\"path\":\"relative\",\"ttl_secs\":60,\"registered_at\":0,\"expires_at\":60,\"source\":\"cli\"}}").unwrap();
        // Grow past the compaction threshold with repeated registrations.
        let line = format!(
            "{}\n",
            serde_json::to_string(&reg.load().unwrap()[0]).unwrap()
        );
        let reps = (COMPACT_AT_BYTES as usize / line.len()) + 1;
        f.write_all(line.repeat(reps).as_bytes()).unwrap();
        drop(f);
        assert_eq!(reg.load().unwrap().len(), 1);
        reg.record(&dir.path().join("b"), 60, 5, Source::Cli)
            .unwrap();
        let text = std::fs::read_to_string(reg.file()).unwrap();
        assert_eq!(text.lines().count(), 2, "compacted to one line per path");
        assert_eq!(reg.load().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_registry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(REGISTRY_FILE)).unwrap();
        let reg = Registry::in_data_dir(dir.path());
        assert!(matches!(reg.load(), Err(RegistryError::NotAFile(_))));
        assert!(matches!(
            reg.record(&dir.path().join("wt"), 60, 0, Source::Cli),
            Err(RegistryError::NotAFile(_))
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }
}
