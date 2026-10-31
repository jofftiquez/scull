//! Reading small configuration files as data (Cargo config, npmrc, `.scull.toml`).

use scull_detectors::ConfigReader;
use scull_platform::{CloudState, Native, Platform};
use std::io::Read;
use std::path::Path;

/// Reads small regular files, never following a link at the final component and never
/// opening anything but a regular file (no FIFOs or devices; SAFE-18). Contents are data
/// only: nothing read here is executed.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsConfigReader;

impl ConfigReader for FsConfigReader {
    fn read_text(&self, path: &Path, max_bytes: u64) -> Option<String> {
        let md = std::fs::symlink_metadata(path).ok()?;
        if !md.file_type().is_file() || md.len() > max_bytes {
            return None;
        }
        // Never read a cloud placeholder: reading would download it (PLT-04, T-52).
        let pmd = Native.metadata(path).ok()?;
        if CloudState::from_metadata(&pmd).is_some_and(|c| c.placeholder) {
            return None;
        }
        let mut s = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(max_bytes)
            .read_to_string(&mut s)
            .ok()?;
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_small_regular_files_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("a.toml");
        std::fs::write(&f, "x = 1\n").expect("write");
        assert_eq!(
            FsConfigReader.read_text(&f, 100).as_deref(),
            Some("x = 1\n")
        );
        assert_eq!(FsConfigReader.read_text(&f, 2), None, "too large");
        assert_eq!(
            FsConfigReader.read_text(dir.path(), 100),
            None,
            "a directory"
        );
        assert_eq!(FsConfigReader.read_text(&dir.path().join("no"), 100), None);
    }
}
