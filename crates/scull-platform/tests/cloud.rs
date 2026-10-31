//! Cloud placeholders and synced folders (P1-PLAT-05; PLT-04, T-52, FX-WIN-07, FX-MAC-02
//! platform part).
//!
//! Synced-folder detection runs against fake home trees built in a fresh temp directory, for
//! every OS layout on every OS; the real `$HOME` and environment are never consulted. No test
//! reads a file's contents through scull: placeholders are recognized from metadata only.
//!
//! What can't be tested here, and why:
//! * Windows `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` / `RECALL_ON_OPEN` and the
//!   `IO_REPARSE_TAG_CLOUD*` tags are set only by a Cloud Files sync provider, not by user
//!   code; their mapping is unit-tested in `src/cloud.rs`. `OFFLINE`, `PINNED` and `UNPINNED`
//!   are settable (`attrib +O/+P/+U`) and tested on real files below.
//! * macOS `SF_DATALESS` is set only by the File Provider framework when it evicts a file;
//!   its mapping is unit-tested, and its absence on ordinary files is tested below.
//! * Real OneDrive/iCloud placeholders (FX-WIN-07, FX-MAC-02) need a signed-in account: manual
//!   release checklist.

use scull_platform::{
    CloudProvider, CloudState, DirLayout, DirSource, Native, Platform, SyncMechanism, SyncedRoots,
};
use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

type TestResult = Result<(), Box<dyn Error>>;

/// A lookup over a fixed table, standing in for the environment.
fn lookup(table: HashMap<DirSource, PathBuf>) -> impl Fn(DirSource) -> Option<OsString> {
    move |source| table.get(&source).map(|p| p.clone().into_os_string())
}

fn mkdirs(paths: &[&Path]) -> TestResult {
    for p in paths {
        fs::create_dir_all(p)?;
    }
    Ok(())
}

fn roots_of(found: &SyncedRoots) -> Vec<(PathBuf, CloudProvider, SyncMechanism)> {
    let mut v: Vec<_> = found
        .roots()
        .iter()
        .map(|r| (r.path.clone(), r.provider, r.mechanism))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// JSON string escaping for a path (Windows backslashes).
fn json_path(p: &Path) -> String {
    p.display().to_string().replace('\\', "\\\\")
}

#[test]
fn macos_layout_icloud_and_file_provider_folders() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("Users").join("u");
    let library = home.join("Library");
    let icloud = library.join("Mobile Documents");
    let storage = library.join("CloudStorage");
    let legacy_dropbox = home.join("Dropbox (Legacy)");
    mkdirs(&[
        &icloud.join("com~apple~CloudDocs"),
        &storage.join("OneDrive-Personal"),
        &storage.join("GoogleDrive-u@example.com"),
        &storage.join("Dropbox"),
        &home.join(".dropbox"),
        &legacy_dropbox,
        &home.join("projects"),
    ])?;
    fs::write(storage.join("stray.txt"), b"not a folder")?;
    fs::write(
        home.join(".dropbox").join("info.json"),
        format!(
            r#"{{"personal": {{"path": "{}"}}}}"#,
            json_path(&legacy_dropbox)
        ),
    )?;

    let found = SyncedRoots::resolve(
        DirLayout::MacOs,
        lookup(HashMap::from([(DirSource::Home, home.clone())])),
        &[],
    );
    let on_demand = SyncMechanism::OnDemand;
    assert_eq!(
        roots_of(&found),
        [
            (
                legacy_dropbox.clone(),
                CloudProvider::Dropbox,
                SyncMechanism::Mirror
            ),
            (storage.join("Dropbox"), CloudProvider::Dropbox, on_demand),
            (
                storage.join("GoogleDrive-u@example.com"),
                CloudProvider::GoogleDrive,
                on_demand
            ),
            (
                storage.join("OneDrive-Personal"),
                CloudProvider::OneDrive,
                on_demand
            ),
            (icloud.clone(), CloudProvider::ICloud, on_demand),
        ]
    );

    // A downloaded file in iCloud Drive: synced (≥ S2), not a placeholder.
    let doc = icloud.join("com~apple~CloudDocs").join("notes.txt");
    fs::write(&doc, b"hello")?;
    let state = found
        .cloud_state(&doc, &Native.metadata(&doc)?)
        .ok_or("iCloud file not recognized as synced")?;
    assert_eq!(state.provider, CloudProvider::ICloud);
    assert!(!state.placeholder);
    assert_eq!(state.synced_root.as_deref(), Some(icloud.as_path()));

    // Outside every synced folder: nothing.
    let src = home.join("projects").join("main.rs");
    fs::write(&src, b"fn main() {}")?;
    assert_eq!(found.cloud_state(&src, &Native.metadata(&src)?), None);
    Ok(())
}

#[test]
fn windows_layout_onedrive_env_dropbox_info_and_icloud() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("Users").join("u");
    let personal = home.join("OneDrive");
    let business = home.join("OneDrive - Contoso");
    let local_app_data = home.join("AppData").join("Local");
    let work_dropbox = home.join("Dropbox (Work)");
    let icloud = home.join("iCloudDrive");
    mkdirs(&[
        &personal,
        &business,
        &local_app_data.join("Dropbox"),
        &work_dropbox,
        &icloud,
        &home.join("Documents"),
    ])?;
    fs::write(
        local_app_data.join("Dropbox").join("info.json"),
        format!(
            r#"{{"business": {{"path": "{}"}}}}"#,
            json_path(&work_dropbox)
        ),
    )?;

    let found = SyncedRoots::resolve(
        DirLayout::Windows,
        lookup(HashMap::from([
            (DirSource::Home, home.clone()),
            (DirSource::Env("OneDrive"), personal.clone()),
            (DirSource::Env("OneDriveCommercial"), business.clone()),
            // Points nowhere: ignored.
            (DirSource::Env("OneDriveConsumer"), home.join("missing")),
            (DirSource::LocalAppData, local_app_data.clone()),
            // Relative: ignored.
            (DirSource::Env("APPDATA"), PathBuf::from("relative")),
        ])),
        &[],
    );
    let on_demand = SyncMechanism::OnDemand;
    assert_eq!(
        roots_of(&found),
        [
            (work_dropbox.clone(), CloudProvider::Dropbox, on_demand),
            (personal.clone(), CloudProvider::OneDrive, on_demand),
            (business.clone(), CloudProvider::OneDrive, on_demand),
            (icloud.clone(), CloudProvider::ICloud, on_demand),
        ]
    );
    let file = business.join("Q3.xlsx");
    fs::write(&file, b"x")?;
    let state = found.cloud_state(&file, &Native.metadata(&file)?);
    assert_eq!(state.map(|s| s.provider), Some(CloudProvider::OneDrive));
    assert_eq!(found.synced_root_for(&home.join("Documents")), None);
    Ok(())
}

#[test]
fn linux_layout_dropbox_and_onedrive_client() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("home").join("u");
    let dropbox = home.join("Dropbox");
    let onedrive = home.join("Cloud").join("OneDrive");
    mkdirs(&[
        &home.join(".dropbox-dist"),
        &dropbox,
        &home.join(".config").join("onedrive"),
        &onedrive,
    ])?;
    fs::write(
        home.join(".config").join("onedrive").join("config"),
        "# onedrive client config\nsync_dir = \"~/Cloud/OneDrive\"\n",
    )?;

    let found = SyncedRoots::resolve(
        DirLayout::Xdg,
        lookup(HashMap::from([(DirSource::Home, home.clone())])),
        &[],
    );
    assert_eq!(
        roots_of(&found),
        [
            (
                onedrive.clone(),
                CloudProvider::OneDrive,
                SyncMechanism::Mirror
            ),
            (
                dropbox.clone(),
                CloudProvider::Dropbox,
                SyncMechanism::Mirror
            ),
        ]
    );
    let nested = dropbox.join("photos").join("2026");
    fs::create_dir_all(&nested)?;
    let state = found
        .cloud_state(&nested, &Native.metadata(&nested)?)
        .ok_or("Dropbox folder not recognized")?;
    assert_eq!(state.provider, CloudProvider::Dropbox);
    assert!(!state.placeholder);
    Ok(())
}

#[test]
fn linux_layout_without_clients_finds_nothing() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().join("home").join("u");
    // A folder merely named Dropbox, with no Dropbox client installed.
    mkdirs(&[&home.join("Dropbox"), &home.join("OneDrive")])?;
    let found = SyncedRoots::resolve(
        DirLayout::Xdg,
        lookup(HashMap::from([(DirSource::Home, home.clone())])),
        &[],
    );
    assert!(found.roots().is_empty(), "{found:?}");

    // No home at all.
    let none = SyncedRoots::resolve(DirLayout::MacOs, |_| None, &[]);
    assert!(none.roots().is_empty());
    Ok(())
}

/// Ordinary files and directories are never reported as placeholders (no false positives
/// that would wrongly hide content), and on macOS the flags are read.
#[test]
fn ordinary_entries_are_not_cloud() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("plain.bin");
    fs::write(&file, [0u8; 4096])?;
    for path in [tmp.path(), file.as_path()] {
        let md = Native.metadata(path)?;
        assert_eq!(CloudState::from_metadata(&md), None, "{}", path.display());
        if cfg!(target_os = "macos") {
            assert!(md.bsd_flags.is_some(), "st_flags not read on macOS");
        } else {
            assert_eq!(md.bsd_flags, None);
        }
    }
    Ok(())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::process::{Command, Stdio};

    /// `FILE_ATTRIBUTE_OFFLINE`.
    const OFFLINE: u32 = 0x1000;
    /// `FILE_ATTRIBUTE_PINNED`.
    const PINNED: u32 = 0x8_0000;
    /// `FILE_ATTRIBUTE_UNPINNED`.
    const UNPINNED: u32 = 0x10_0000;

    /// `attrib <flag> <path>`: sets an attribute without opening the file for data.
    fn attrib(flag: &str, path: &Path) -> Result<bool, Box<dyn Error>> {
        let out = Command::new("attrib")
            .arg(flag)
            .arg(path)
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            eprintln!(
                "attrib {flag} failed: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(out.status.success())
    }

    /// Fails on CI (where the attribute is expected to be settable), skips elsewhere.
    fn skip(reason: &str) -> TestResult {
        if std::env::var_os("CI").is_some() && std::env::var_os("SCULL_TEST_ALLOW_SKIP").is_none() {
            return Err(format!("{reason} (required on CI)").into());
        }
        eprintln!("skipping: {reason}");
        Ok(())
    }

    fn state_after(flag: &str, bit: u32) -> Result<Option<CloudState>, Box<dyn Error>> {
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join("doc.txt");
        fs::write(&file, b"content")?;
        if !attrib(flag, &file)? {
            return Err(format!("attrib {flag} failed").into());
        }
        let md = Native.metadata(&file)?;
        let attrs = md.file_attributes.ok_or("no attributes")?;
        if attrs & bit == 0 {
            return Err(
                format!("attrib {flag} did not set {bit:#x} (attributes {attrs:#x})").into(),
            );
        }
        let state = CloudState::from_metadata(&md);
        // Clear it again so the temp dir is removed normally.
        attrib(&flag.replace('+', "-"), &file)?;
        Ok(state)
    }

    /// FX-WIN-07 (platform part): an offline file is a placeholder; it is never read.
    #[test]
    fn offline_attribute_is_a_placeholder() -> TestResult {
        match state_after("+O", OFFLINE) {
            Ok(state) => {
                let state = state.ok_or("offline file not detected")?;
                assert!(state.placeholder);
                assert_eq!(state.pinned, None);
                assert_eq!(state.provider, CloudProvider::Other);
                Ok(())
            }
            Err(e) => skip(&e.to_string()),
        }
    }

    #[test]
    fn pinned_attribute_is_reported() -> TestResult {
        match state_after("+P", PINNED) {
            Ok(state) => {
                let state = state.ok_or("pinned file not detected")?;
                assert!(!state.placeholder);
                assert_eq!(state.pinned, Some(true));
                Ok(())
            }
            Err(e) => skip(&e.to_string()),
        }
    }

    #[test]
    fn unpinned_attribute_is_reported() -> TestResult {
        match state_after("+U", UNPINNED) {
            Ok(state) => {
                let state = state.ok_or("unpinned file not detected")?;
                assert!(!state.placeholder);
                assert_eq!(state.pinned, Some(false));
                Ok(())
            }
            Err(e) => skip(&e.to_string()),
        }
    }
}
