//! Cloud-sync placeholders and synced folders (P1-PLAT-05; PLT-04, T-52, PRD §8.3).
//!
//! Reading a cloud placeholder downloads it: a scan that opened OneDrive, iCloud or Dropbox
//! files could pull gigabytes and fill the disk it was meant to clean (T-52). Everything
//! here is decided from **metadata only** (attributes, reparse tags, file flags, paths and the
//! mount table); no file content is ever read. The only files read are sync clients' own
//! small settings files (Dropbox `info.json`, the Linux `onedrive` client's `config`), to
//! find where their synced folders are.
//!
//! The rubric (§8.3) uses the result like this:
//!
//! * any entry with a [`CloudState`] (it is a placeholder or lives in a synced folder) is at
//!   least **S2**: deleting it locally deletes it in the cloud and on the user's other
//!   devices;
//! * [`CloudState::placeholder`] means the content is not on this disk: **never read it**
//!   (no hashing, no manifest parsing, no "is this a lockfile" peeking), and for a directory,
//!   don't list it either, since listing asks the provider to fetch its entries.
//!
//! # Signals per OS
//!
//! | OS | Placeholder (per entry) | Synced folders ([`SyncedRoots`]) |
//! |---|---|---|
//! | Windows | `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, `RECALL_ON_OPEN`, `OFFLINE`; pin state from `PINNED` / `UNPINNED`; `IO_REPARSE_TAG_CLOUD*` (Cloud Files API: OneDrive, iCloud, Dropbox...) and the legacy `IO_REPARSE_TAG_ONEDRIVE` mark cloud-managed entries | `%OneDrive%`, `%OneDriveCommercial%`, `%OneDriveConsumer%`; Dropbox `info.json` under `%LOCALAPPDATA%` / `%APPDATA%`; `%USERPROFILE%\iCloudDrive`, `%USERPROFILE%\Dropbox` |
//! | macOS | `SF_DATALESS` in `st_flags` (iCloud "Optimize Mac Storage" and every File Provider client evicting a file) | `~/Library/Mobile Documents` (iCloud Drive); each folder in `~/Library/CloudStorage` (OneDrive, Dropbox, Google Drive, Box... via File Provider); legacy Dropbox from `~/.dropbox/info.json` |
//! | Linux | none at the file level: sync clients keep full copies; files on cloud FUSE mounts are remote, so every file there counts as a placeholder | Dropbox (`~/.dropbox/info.json`, or `~/Dropbox` when `~/.dropbox-dist` / `~/.dropbox` exists); the `onedrive` client's `sync_dir` (default `~/OneDrive`); FUSE mounts from [`crate::volumes()`]: `fuse.rclone`, `fuse.onedriver`, `fuse.google-drive-ocamlfuse`, ... |
//!
//! Known gaps: new sync clients with unknown attributes (T-52 residual risk); macOS
//! "Desktop & Documents in iCloud" is caught per file by `SF_DATALESS`, not as a synced
//! root; Google Drive for desktop on Windows (a virtual drive) is not recognized as synced.

use crate::volumes::{VolumeInfo, path_depth, path_starts_with};
use crate::{DirLayout, DirSource, EntryKind, Metadata};
use serde::Serialize;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Windows `FILE_ATTRIBUTE_*` bits and reparse tags, and the macOS `SF_DATALESS` flag (values
/// from the Windows SDK and `<sys/stat.h>`), so the mapping is tested on every OS.
pub(crate) mod bits {
    /// `FILE_ATTRIBUTE_OFFLINE`: data not immediately available (moved to offline storage).
    pub(crate) const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    /// `FILE_ATTRIBUTE_RECALL_ON_OPEN`: opening the entry asks the provider for it (cloud
    /// directories whose children are not populated yet).
    pub(crate) const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    /// `FILE_ATTRIBUTE_PINNED`: "Always keep on this device".
    pub(crate) const FILE_ATTRIBUTE_PINNED: u32 = 0x0008_0000;
    /// `FILE_ATTRIBUTE_UNPINNED`: "Free up space" (online-only requested).
    pub(crate) const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
    /// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`: reading the data downloads it (a dehydrated
    /// placeholder).
    pub(crate) const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    /// `IO_REPARSE_TAG_CLOUD`: the Cloud Files API tag; `IO_REPARSE_TAG_CLOUD_1` ... `_F`
    /// differ only in the bits of [`IO_REPARSE_TAG_CLOUD_MASK`].
    pub(crate) const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;
    /// `IO_REPARSE_TAG_CLOUD_MASK`.
    pub(crate) const IO_REPARSE_TAG_CLOUD_MASK: u32 = 0x0000_F000;
    /// `IO_REPARSE_TAG_ONEDRIVE`: the pre-Cloud-Files OneDrive tag.
    pub(crate) const IO_REPARSE_TAG_ONEDRIVE: u32 = 0x8000_0021;
    /// macOS `SF_DATALESS`: the file's (or directory's) content is not on disk; reading it
    /// asks the File Provider to download it.
    pub(crate) const SF_DATALESS: u32 = 0x4000_0000;
}

/// Who syncs an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CloudProvider {
    /// Apple iCloud Drive.
    ICloud,
    /// Microsoft OneDrive (personal or business).
    OneDrive,
    /// Dropbox.
    Dropbox,
    /// Google Drive.
    GoogleDrive,
    /// Box.
    Box,
    /// A provider scull can't name (another Cloud Files / File Provider client, rclone, ...).
    Other,
}

impl CloudProvider {
    /// Stable lowercase name, as used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            CloudProvider::ICloud => "icloud",
            CloudProvider::OneDrive => "onedrive",
            CloudProvider::Dropbox => "dropbox",
            CloudProvider::GoogleDrive => "google_drive",
            CloudProvider::Box => "box",
            CloudProvider::Other => "other",
        }
    }
}

impl fmt::Display for CloudProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a synced folder keeps its files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SyncMechanism {
    /// Files on demand (Windows Cloud Files API, macOS File Provider / iCloud Drive): any file
    /// may be a placeholder; each one is checked by its metadata.
    OnDemand,
    /// A plain folder the client mirrors with full local copies (Dropbox on Linux, the Linux
    /// `onedrive` client, legacy clients). Deleting a file deletes it in the cloud.
    Mirror,
    /// A FUSE mount whose every read goes to the network (rclone, onedriver): every file in it
    /// counts as a placeholder.
    RemoteMount,
}

/// One synced folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SyncedRoot {
    /// The folder's path, as found (not canonicalized).
    pub path: PathBuf,
    /// Who syncs it.
    pub provider: CloudProvider,
    /// How it keeps its files.
    pub mechanism: SyncMechanism,
}

impl SyncedRoot {
    /// A synced root, e.g. for an in-memory fake in another crate's tests.
    pub fn new(
        path: impl Into<PathBuf>,
        provider: CloudProvider,
        mechanism: SyncMechanism,
    ) -> Self {
        Self {
            path: path.into(),
            provider,
            mechanism,
        }
    }
}

/// The cloud facts of one entry. Its presence alone makes the entry at least S2 (§8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CloudState {
    /// Who syncs it ([`CloudProvider::Other`] when only the entry's attributes say "cloud").
    pub provider: CloudProvider,
    /// The content is not (fully) on this disk: reading it would download it. Never read it;
    /// for a directory, don't list it (PLT-04).
    pub placeholder: bool,
    /// Windows pin state: `Some(true)` "Always keep on this device", `Some(false)` "Free up
    /// space" requested, `None` when not reported (and on macOS and Linux).
    pub pinned: Option<bool>,
    /// The synced folder the entry is in, when known.
    pub synced_root: Option<PathBuf>,
}

impl CloudState {
    /// The cloud state from an entry's metadata alone, without knowing the synced folders:
    /// `Some` for placeholders and cloud-managed entries (Windows attributes or reparse tag,
    /// macOS `SF_DATALESS`). Prefer [`SyncedRoots::cloud_state`], which also catches fully
    /// downloaded files in synced folders.
    pub fn from_metadata(md: &Metadata) -> Option<CloudState> {
        let flags = metadata_flags(md)?;
        Some(CloudState {
            provider: flags.provider(),
            placeholder: flags.placeholder,
            pinned: flags.pinned,
            synced_root: None,
        })
    }
}

/// What an entry's own metadata says about cloud sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CloudFlags {
    /// Content not local (recall / offline / dataless).
    pub(crate) placeholder: bool,
    /// Windows pin state.
    pub(crate) pinned: Option<bool>,
    /// Tagged with the legacy OneDrive reparse tag.
    pub(crate) onedrive_tag: bool,
}

impl CloudFlags {
    fn provider(self) -> CloudProvider {
        if self.onedrive_tag {
            CloudProvider::OneDrive
        } else {
            CloudProvider::Other
        }
    }
}

/// Windows: cloud flags from `FILE_ATTRIBUTE_*` bits and the reparse tag; `None` when nothing
/// cloud-related is set.
pub(crate) fn windows_cloud_flags(attributes: u32, reparse_tag: Option<u32>) -> Option<CloudFlags> {
    use bits::*;
    let recall = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN;
    let placeholder = attributes & (recall | FILE_ATTRIBUTE_OFFLINE) != 0;
    let pinned = match (
        attributes & FILE_ATTRIBUTE_PINNED != 0,
        attributes & FILE_ATTRIBUTE_UNPINNED != 0,
    ) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        // Neither, or both (contradictory): not reported.
        _ => None,
    };
    let cloud_tag =
        reparse_tag.is_some_and(|t| t & !IO_REPARSE_TAG_CLOUD_MASK == IO_REPARSE_TAG_CLOUD);
    let onedrive_tag = reparse_tag == Some(IO_REPARSE_TAG_ONEDRIVE);
    let pin_bits = attributes & (FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_UNPINNED) != 0;
    (placeholder || pin_bits || cloud_tag || onedrive_tag).then_some(CloudFlags {
        placeholder,
        pinned,
        onedrive_tag,
    })
}

/// macOS: cloud flags from `st_flags`; `None` unless `SF_DATALESS` is set.
pub(crate) fn macos_cloud_flags(bsd_flags: u32) -> Option<CloudFlags> {
    (bsd_flags & bits::SF_DATALESS != 0).then_some(CloudFlags {
        placeholder: true,
        pinned: None,
        onedrive_tag: false,
    })
}

fn metadata_flags(md: &Metadata) -> Option<CloudFlags> {
    md.file_attributes
        .and_then(|a| windows_cloud_flags(a, md.reparse_tag))
        .or_else(|| md.bsd_flags.and_then(macos_cloud_flags))
}

/// The synced folders on this machine, found once and then matched against paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncedRoots {
    roots: Vec<SyncedRoot>,
}

impl SyncedRoots {
    /// The synced folders of the current user on this OS: reads environment variables, checks
    /// the known locations with metadata calls, reads the sync clients' settings files and
    /// (Linux) the mount table. Never reads synced content.
    pub fn detect() -> SyncedRoots {
        let volumes = if cfg!(target_os = "linux") {
            crate::volumes().unwrap_or_default()
        } else {
            Vec::new()
        };
        Self::resolve(DirLayout::native(), native_lookup, &volumes)
    }

    /// The resolver behind [`SyncedRoots::detect`]: applies `layout`'s conventions to the
    /// values `lookup` returns (home directory, environment variables, Windows known folders)
    /// and to `volumes`. Reads the filesystem only under those paths, so tests can point it at
    /// a fake home tree.
    ///
    /// Unset, empty and relative values are ignored. A candidate folder counts only if it is
    /// an existing directory, except FUSE mounts from `volumes`, which are never touched
    /// (they may stall, T-53).
    pub fn resolve(
        layout: DirLayout,
        lookup: impl Fn(DirSource) -> Option<OsString>,
        volumes: &[VolumeInfo],
    ) -> SyncedRoots {
        let get = |source: DirSource| {
            lookup(source)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let mut found = SyncedRoots::default();
        let home = get(DirSource::Home);

        match layout {
            DirLayout::MacOs => {
                if let Some(home) = &home {
                    let library = home.join("Library");
                    found.add_dir(
                        library.join("Mobile Documents"),
                        CloudProvider::ICloud,
                        SyncMechanism::OnDemand,
                    );
                    for dir in subdirectories(&library.join("CloudStorage")) {
                        let provider = file_provider_name(&dir);
                        found.add_dir(dir, provider, SyncMechanism::OnDemand);
                    }
                    found.add_dropbox(&home.join(".dropbox"), SyncMechanism::Mirror);
                }
            }
            DirLayout::Windows => {
                for var in ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"] {
                    if let Some(dir) = get(DirSource::Env(var)) {
                        found.add_dir(dir, CloudProvider::OneDrive, SyncMechanism::OnDemand);
                    }
                }
                let app_data = [
                    get(DirSource::LocalAppData),
                    get(DirSource::Env("LOCALAPPDATA")),
                    get(DirSource::RoamingAppData),
                    get(DirSource::Env("APPDATA")),
                ];
                for base in app_data.into_iter().flatten() {
                    found.add_dropbox(&base.join("Dropbox"), SyncMechanism::OnDemand);
                }
                if let Some(home) = &home {
                    found.add_dir(
                        home.join("iCloudDrive"),
                        CloudProvider::ICloud,
                        SyncMechanism::OnDemand,
                    );
                    found.add_dir(
                        home.join("Dropbox"),
                        CloudProvider::Dropbox,
                        SyncMechanism::OnDemand,
                    );
                }
            }
            DirLayout::Xdg => {
                if let Some(home) = &home {
                    found.add_dropbox(&home.join(".dropbox"), SyncMechanism::Mirror);
                    if is_dir(&home.join(".dropbox-dist")) || is_dir(&home.join(".dropbox")) {
                        found.add_dir(
                            home.join("Dropbox"),
                            CloudProvider::Dropbox,
                            SyncMechanism::Mirror,
                        );
                    }
                    let config_home = get(DirSource::Env("XDG_CONFIG_HOME"))
                        .unwrap_or_else(|| home.join(".config"));
                    let onedrive_config = config_home.join("onedrive");
                    if is_dir(&onedrive_config) {
                        let config = read_small(&onedrive_config.join("config"));
                        let sync_dir = onedrive_sync_dir(config.as_deref().unwrap_or(""), home);
                        found.add_dir(sync_dir, CloudProvider::OneDrive, SyncMechanism::Mirror);
                    }
                }
            }
        }

        for volume in volumes {
            if let Some(provider) = cloud_fuse_provider(&volume.fs_type) {
                found.add(
                    volume.mount_point.clone(),
                    provider,
                    SyncMechanism::RemoteMount,
                );
            }
        }
        found
    }

    /// A fixed set of roots, e.g. for an in-memory fake in another crate's tests.
    pub fn from_roots(roots: Vec<SyncedRoot>) -> SyncedRoots {
        SyncedRoots { roots }
    }

    /// Every synced folder found.
    pub fn roots(&self) -> &[SyncedRoot] {
        &self.roots
    }

    /// The innermost synced folder containing `path` (or equal to it). Paths compare by
    /// component, ignoring ASCII case on Windows; pass paths in the same form the roots were
    /// found in (absolute, not resolved through symlinks unless the roots were).
    pub fn synced_root_for(&self, path: &Path) -> Option<&SyncedRoot> {
        self.roots
            .iter()
            .filter(|root| path_starts_with(path, &root.path))
            .max_by_key(|root| path_depth(&root.path))
    }

    /// The cloud state of the entry at `path` with metadata `md`: `Some` when the entry is a
    /// placeholder or cloud-managed by its own metadata, or lives in a synced folder. Uses
    /// metadata and paths only; never reads the entry.
    pub fn cloud_state(&self, path: &Path, md: &Metadata) -> Option<CloudState> {
        let root = self.synced_root_for(path);
        let flags = metadata_flags(md);
        if root.is_none() && flags.is_none() {
            return None;
        }
        let flags = flags.unwrap_or_default();
        // On a remote mount every read goes to the network. Directories are still listed
        // (the volume is off by default anyway, D-22); their files are never read.
        let remote = root.is_some_and(|r| r.mechanism == SyncMechanism::RemoteMount)
            && md.kind != EntryKind::Dir;
        Some(CloudState {
            provider: root.map_or_else(|| flags.provider(), |r| r.provider),
            placeholder: flags.placeholder || remote,
            pinned: flags.pinned,
            synced_root: root.map(|r| r.path.clone()),
        })
    }

    fn add(&mut self, path: PathBuf, provider: CloudProvider, mechanism: SyncMechanism) {
        if !self.roots.iter().any(|r| r.path == path) {
            self.roots.push(SyncedRoot::new(path, provider, mechanism));
        }
    }

    fn add_dir(&mut self, path: PathBuf, provider: CloudProvider, mechanism: SyncMechanism) {
        if is_dir(&path) {
            self.add(path, provider, mechanism);
        }
    }

    /// Dropbox folders listed in `<settings>/info.json`.
    fn add_dropbox(&mut self, settings: &Path, mechanism: SyncMechanism) {
        if let Some(text) = read_small(&settings.join("info.json")) {
            for path in dropbox_info_paths(&text) {
                self.add_dir(path, CloudProvider::Dropbox, mechanism);
            }
        }
    }
}

/// The process's own values: `$HOME` (Windows: `%USERPROFILE%`), environment variables and
/// Windows known folders.
fn native_lookup(source: DirSource) -> Option<OsString> {
    match source {
        DirSource::Env(name) => std::env::var_os(name),
        DirSource::Home => {
            if cfg!(windows) {
                std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
            } else {
                std::env::var_os("HOME")
            }
        }
        DirSource::RoamingAppData | DirSource::LocalAppData => {
            crate::native::known_folder(source).map(PathBuf::into_os_string)
        }
    }
}

/// An existing directory (following a symlink: a synced root may be one).
fn is_dir(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|md| md.is_dir())
}

/// The real directories (not links) directly inside `dir`; empty if it can't be listed.
fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Sync clients' settings files are tiny; refuse anything large or unreadable.
const MAX_SETTINGS_FILE: u64 = 1024 * 1024;

fn read_small(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_SETTINGS_FILE)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// The provider of a `~/Library/CloudStorage` folder by its name (`OneDrive-Personal`,
/// `Dropbox`, `GoogleDrive-user@example.com`, `Box-Box`).
pub(crate) fn file_provider_name(dir: &Path) -> CloudProvider {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is = |prefix: &str| {
        name == prefix
            || name
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(['-', ' ']))
    };
    if is("OneDrive") {
        CloudProvider::OneDrive
    } else if is("Dropbox") {
        CloudProvider::Dropbox
    } else if is("GoogleDrive") {
        CloudProvider::GoogleDrive
    } else if is("Box") {
        CloudProvider::Box
    } else if is("iCloudDrive") || is("iCloud") {
        CloudProvider::ICloud
    } else {
        CloudProvider::Other
    }
}

/// The provider behind a cloud FUSE filesystem type, or `None` for other filesystems.
pub(crate) fn cloud_fuse_provider(fs_type: &str) -> Option<CloudProvider> {
    match fs_type {
        "fuse.onedriver" => Some(CloudProvider::OneDrive),
        "fuse.google-drive-ocamlfuse" | "fuse.gdfs" => Some(CloudProvider::GoogleDrive),
        "fuse.dbxfs" => Some(CloudProvider::Dropbox),
        "fuse.rclone" | "fuse.s3fs" | "fuse.gcsfuse" | "fuse.goofys" | "fuse.blobfuse"
        | "fuse.blobfuse2" | "fuse.mountpoint-s3" => Some(CloudProvider::Other),
        _ => None,
    }
}

/// The folders in a Dropbox `info.json`: `{"personal": {"path": "..."}, "business": {...}}`.
pub(crate) fn dropbox_info_paths(json: &str) -> Vec<PathBuf> {
    let Ok(serde_json::Value::Object(accounts)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    accounts
        .values()
        .filter_map(|account| account.get("path")?.as_str())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .collect()
}

/// The Linux `onedrive` client's `sync_dir` setting (default `~/OneDrive`), with `~`
/// expanded to `home`.
pub(crate) fn onedrive_sync_dir(config: &str, home: &Path) -> PathBuf {
    let configured = config.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key.trim() == "sync_dir").then(|| value.trim().trim_matches('"').to_owned())
    });
    match configured.as_deref() {
        None | Some("") => home.join("OneDrive"),
        Some("~") => home.to_path_buf(),
        Some(value) => match value.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(value),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::bits::*;
    use super::*;

    fn md(kind: EntryKind) -> Metadata {
        Metadata {
            kind,
            identity: None,
            logical_size: 0,
            allocated_size: None,
            nlink: None,
            mtime_unix: None,
            readonly: false,
            file_attributes: None,
            reparse_tag: None,
            bsd_flags: None,
        }
    }

    fn win_md(attributes: u32, tag: Option<u32>) -> Metadata {
        Metadata {
            file_attributes: Some(attributes),
            reparse_tag: tag,
            ..md(EntryKind::File)
        }
    }

    const ARCHIVE: u32 = 0x20;
    const REPARSE: u32 = 0x400;

    #[test]
    fn windows_attribute_mapping() {
        // Ordinary file: nothing cloud-related.
        assert_eq!(windows_cloud_flags(ARCHIVE, None), None);
        // Dehydrated OneDrive placeholder: recall-on-data-access + cloud tag + unpinned.
        let f = windows_cloud_flags(
            ARCHIVE | REPARSE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_UNPINNED,
            Some(IO_REPARSE_TAG_CLOUD | 0x3000),
        );
        assert_eq!(
            f,
            Some(CloudFlags {
                placeholder: true,
                pinned: Some(false),
                onedrive_tag: false
            })
        );
        // Directory not yet populated.
        assert!(
            windows_cloud_flags(FILE_ATTRIBUTE_RECALL_ON_OPEN, Some(IO_REPARSE_TAG_CLOUD))
                .is_some_and(|f| f.placeholder)
        );
        // Offline (HSM or legacy placeholders).
        assert!(windows_cloud_flags(FILE_ATTRIBUTE_OFFLINE, None).is_some_and(|f| f.placeholder));
        // Hydrated, "always keep on this device": cloud-managed, not a placeholder.
        assert_eq!(
            windows_cloud_flags(
                ARCHIVE | REPARSE | FILE_ATTRIBUTE_PINNED,
                Some(IO_REPARSE_TAG_CLOUD)
            ),
            Some(CloudFlags {
                placeholder: false,
                pinned: Some(true),
                onedrive_tag: false
            })
        );
        // Hydrated, no pin state: still cloud-managed by its tag.
        assert_eq!(
            windows_cloud_flags(REPARSE, Some(IO_REPARSE_TAG_CLOUD | 0xF000)),
            Some(CloudFlags::default())
        );
        // Contradictory pin bits are not reported.
        assert!(
            windows_cloud_flags(FILE_ATTRIBUTE_PINNED | FILE_ATTRIBUTE_UNPINNED, None)
                .is_some_and(|f| f.pinned.is_none())
        );
        // Legacy OneDrive tag names the provider.
        let f = windows_cloud_flags(REPARSE, Some(IO_REPARSE_TAG_ONEDRIVE));
        assert!(f.is_some_and(|f| f.onedrive_tag && !f.placeholder));
        // Other reparse tags (symlink, junction, app exec alias, dedup) are not cloud.
        for tag in [0xA000_000C, 0xA000_0003, 0x8000_001B, 0x8000_0013] {
            assert_eq!(windows_cloud_flags(REPARSE, Some(tag)), None, "{tag:#x}");
        }
        // A tag that only looks like CLOUD in other bits is not cloud.
        assert_eq!(
            windows_cloud_flags(REPARSE, Some(IO_REPARSE_TAG_CLOUD | 0x1_0000)),
            None
        );
    }

    /// `SF_DATALESS` can only be set by the File Provider framework, so tests can't create a
    /// dataless file; the mapping is tested here and the flag's absence on ordinary files is
    /// tested on macOS in `tests/cloud.rs`.
    #[test]
    fn macos_flag_mapping() {
        assert_eq!(macos_cloud_flags(0), None);
        const UF_HIDDEN: u32 = 0x8000;
        const UF_COMPRESSED: u32 = 0x20;
        assert_eq!(macos_cloud_flags(UF_HIDDEN | UF_COMPRESSED), None);
        assert_eq!(
            macos_cloud_flags(SF_DATALESS | UF_HIDDEN),
            Some(CloudFlags {
                placeholder: true,
                pinned: None,
                onedrive_tag: false
            })
        );
        let dataless = Metadata {
            bsd_flags: Some(SF_DATALESS),
            ..md(EntryKind::File)
        };
        let state = CloudState::from_metadata(&dataless);
        assert!(state.is_some_and(|s| s.placeholder && s.provider == CloudProvider::Other));
        assert_eq!(CloudState::from_metadata(&md(EntryKind::File)), None);
    }

    #[test]
    fn cloud_state_combines_roots_and_flags() {
        let roots = SyncedRoots::from_roots(vec![
            SyncedRoot::new(
                "/h/OneDrive",
                CloudProvider::OneDrive,
                SyncMechanism::OnDemand,
            ),
            SyncedRoot::new(
                "/h/OneDrive/Shared/Dropbox",
                CloudProvider::Dropbox,
                SyncMechanism::Mirror,
            ),
            SyncedRoot::new(
                "/mnt/gdrive",
                CloudProvider::Other,
                SyncMechanism::RemoteMount,
            ),
        ]);
        let p = Path::new;
        let file = md(EntryKind::File);

        // Outside any root, plain metadata: no cloud state.
        assert_eq!(roots.cloud_state(p("/h/src/a.rs"), &file), None);
        assert_eq!(roots.synced_root_for(p("/h/OneDriveX/a")), None);

        // Fully downloaded file in a synced folder: S2 by location, not a placeholder.
        let s = roots.cloud_state(p("/h/OneDrive/doc.txt"), &file);
        let s = s.expect("in a synced folder");
        assert_eq!(s.provider, CloudProvider::OneDrive);
        assert!(!s.placeholder);
        assert_eq!(s.synced_root.as_deref(), Some(p("/h/OneDrive")));

        // The root itself counts as inside it.
        assert!(
            roots
                .cloud_state(p("/h/OneDrive"), &md(EntryKind::Dir))
                .is_some()
        );

        // Innermost root wins.
        let s = roots.cloud_state(p("/h/OneDrive/Shared/Dropbox/x"), &file);
        assert_eq!(s.map(|s| s.provider), Some(CloudProvider::Dropbox));

        // Placeholder by attributes in a synced folder.
        let ph = win_md(
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_UNPINNED,
            Some(IO_REPARSE_TAG_CLOUD),
        );
        let s = roots
            .cloud_state(p("/h/OneDrive/big.iso"), &ph)
            .expect("placeholder");
        assert!(s.placeholder);
        assert_eq!(s.pinned, Some(false));
        assert_eq!(s.provider, CloudProvider::OneDrive);

        // Placeholder outside any known root (unknown provider) is still caught.
        let s = roots
            .cloud_state(p("/elsewhere/x"), &ph)
            .expect("placeholder");
        assert!(s.placeholder);
        assert_eq!(s.provider, CloudProvider::Other);
        assert_eq!(s.synced_root, None);

        // Remote mount: files are placeholders, directories are not.
        assert!(
            roots
                .cloud_state(p("/mnt/gdrive/a.bin"), &file)
                .is_some_and(|s| s.placeholder)
        );
        assert!(
            roots
                .cloud_state(p("/mnt/gdrive/dir"), &md(EntryKind::Dir))
                .is_some_and(|s| !s.placeholder)
        );
    }

    #[test]
    fn file_provider_folder_names() {
        let p =
            |n: &str| file_provider_name(Path::new("/h/Library/CloudStorage").join(n).as_path());
        assert_eq!(p("OneDrive-Personal"), CloudProvider::OneDrive);
        assert_eq!(p("OneDrive-ContosoLtd"), CloudProvider::OneDrive);
        assert_eq!(p("Dropbox"), CloudProvider::Dropbox);
        assert_eq!(p("Dropbox-Personal"), CloudProvider::Dropbox);
        assert_eq!(p("GoogleDrive-u@example.com"), CloudProvider::GoogleDrive);
        assert_eq!(p("Box-Box"), CloudProvider::Box);
        assert_eq!(p("Boxcryptor"), CloudProvider::Other);
        assert_eq!(p("pCloud Drive"), CloudProvider::Other);
    }

    #[test]
    fn fuse_providers() {
        assert_eq!(
            cloud_fuse_provider("fuse.rclone"),
            Some(CloudProvider::Other)
        );
        assert_eq!(
            cloud_fuse_provider("fuse.onedriver"),
            Some(CloudProvider::OneDrive)
        );
        assert_eq!(
            cloud_fuse_provider("fuse.google-drive-ocamlfuse"),
            Some(CloudProvider::GoogleDrive)
        );
        assert_eq!(cloud_fuse_provider("fuse.sshfs"), None);
        assert_eq!(cloud_fuse_provider("ext4"), None);
    }

    #[test]
    fn dropbox_info_json() {
        // Absolute on the host running the test.
        let base = if cfg!(windows) {
            r"C:\\Users\\u"
        } else {
            "/home/u"
        };
        let sep = if cfg!(windows) { r"\\" } else { "/" };
        let json = format!(
            r#"{{"personal": {{"path": "{base}{sep}Dropbox", "host": 1, "is_team": false}},
                "business": {{"path": "{base}{sep}Dropbox (Work)"}},
                "weird": {{"path": "relative/path"}}, "other": 5}}"#
        );
        let home = PathBuf::from(base.replace(r"\\", r"\"));
        let mut paths = dropbox_info_paths(&json);
        paths.sort();
        assert_eq!(paths, [home.join("Dropbox"), home.join("Dropbox (Work)")]);
        assert!(dropbox_info_paths("not json").is_empty());
        assert!(dropbox_info_paths("[1,2]").is_empty());
    }

    #[test]
    fn onedrive_client_sync_dir() {
        let home = Path::new("/home/u");
        assert_eq!(onedrive_sync_dir("", home), home.join("OneDrive"));
        assert_eq!(
            onedrive_sync_dir("# comment\nsync_dir = \"~/Cloud/OD\"\n", home),
            home.join("Cloud/OD")
        );
        assert_eq!(
            onedrive_sync_dir("sync_dir=\"/data/onedrive\"", home),
            PathBuf::from("/data/onedrive")
        );
        assert_eq!(
            onedrive_sync_dir("# sync_dir = \"~/Ignored\"\n", home),
            home.join("OneDrive")
        );
        assert_eq!(
            onedrive_sync_dir("sync_dir = \"\"", home),
            home.join("OneDrive")
        );
    }

    /// Cloud FUSE mounts from the volume list (P1-PLAT-03) become remote-mount roots without
    /// touching the mount (it may stall); other network mounts don't.
    #[test]
    fn resolve_adds_cloud_fuse_mounts_from_volumes() {
        use crate::volumes::{FsFeatures, VolumeKind};
        let volume = |mount: &str, fs_type: &str| VolumeInfo {
            mount_point: PathBuf::from(mount),
            source: String::from("remote:"),
            volume_id: Some(77),
            fs_type: fs_type.to_owned(),
            kind: VolumeKind::Network,
            read_only: false,
            total_bytes: None,
            free_bytes: None,
            supports: FsFeatures::default(),
        };
        let volumes = [
            volume("/never/touched/gdrive", "fuse.rclone"),
            volume("/never/touched/od", "fuse.onedriver"),
            volume("/never/touched/ssh", "fuse.sshfs"),
            volume("/never/touched/nfs", "nfs4"),
        ];
        let found = SyncedRoots::resolve(DirLayout::Xdg, |_| None, &volumes);
        assert_eq!(
            found.roots(),
            [
                SyncedRoot::new(
                    "/never/touched/gdrive",
                    CloudProvider::Other,
                    SyncMechanism::RemoteMount
                ),
                SyncedRoot::new(
                    "/never/touched/od",
                    CloudProvider::OneDrive,
                    SyncMechanism::RemoteMount
                ),
            ]
        );
        let state = found.cloud_state(Path::new("/never/touched/od/a.docx"), &md(EntryKind::File));
        assert!(state.is_some_and(|s| s.placeholder && s.provider == CloudProvider::OneDrive));
    }

    #[test]
    fn json_names() -> Result<(), serde_json::Error> {
        let s = CloudState {
            provider: CloudProvider::GoogleDrive,
            placeholder: true,
            pinned: None,
            synced_root: None,
        };
        let v = serde_json::to_value(&s)?;
        assert_eq!(v["provider"], "google_drive");
        assert_eq!(CloudProvider::ICloud.as_str(), "icloud");
        let r = serde_json::to_value(SyncedRoot::new(
            "/x",
            CloudProvider::Box,
            SyncMechanism::RemoteMount,
        ))?;
        assert_eq!(r["mechanism"], "remote_mount");
        Ok(())
    }
}
