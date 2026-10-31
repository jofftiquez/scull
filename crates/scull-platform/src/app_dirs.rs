//! Per-user application directories (P1-PLAT-06; CFG-01, DATA-01, DATA-04).
//!
//! [`app_dirs()`] resolves where scull keeps its config, data (the SQLite database and the
//! quarantine manifest), cache and state (logs) on this OS. It only computes paths: nothing is
//! created. Create them with [`create_dir_owner_only`](crate::create_dir_owner_only).
//!
//! | | Linux and other Unix (XDG) | macOS | Windows |
//! |---|---|---|---|
//! | `config` | `$XDG_CONFIG_HOME/scull`, default `~/.config/scull` | `~/Library/Application Support/scull` | `{RoamingAppData}\scull` |
//! | `data` | `$XDG_DATA_HOME/scull`, default `~/.local/share/scull` | `~/Library/Application Support/scull` | `{LocalAppData}\scull\data` |
//! | `cache` | `$XDG_CACHE_HOME/scull`, default `~/.cache/scull` | `~/Library/Caches/scull` | `{LocalAppData}\scull\cache` |
//! | `state` | `$XDG_STATE_HOME/scull`, default `~/.local/state/scull` | `~/Library/Logs/scull` | `{LocalAppData}\scull\state` |
//!
//! * **XDG:** a `XDG_*` variable that is unset, empty or relative is ignored and the default is
//!   used, as the XDG Base Directory spec requires.
//! * **Windows:** the folders come from `SHGetKnownFolderPath` (`FOLDERID_RoamingAppData`,
//!   `FOLDERID_LocalAppData`); the `APPDATA` / `LOCALAPPDATA` environment variables are only a
//!   fallback when the shell API fails (e.g. a service account without a loaded profile).
//!   Config may roam with the profile; data, cache and state are in the **non-roaming** local
//!   folder so the database and quarantine metadata never leave the machine (DATA-04, T-50,
//!   FX-WIN-12).
//! * **`SCULL_HOME`:** when set (non-empty), everything goes under that one directory:
//!   `$SCULL_HOME/config`, `/data`, `/cache`, `/state`, on every OS. Meant for tests and
//!   portable setups. A relative value is an error rather than being ignored, so a mistyped
//!   override never silently falls back to the real directories.
//!
//! [`AppDirs::resolve`] is the pure resolver behind [`app_dirs()`]: it takes the layout and a
//! lookup closure instead of reading the real environment, so every OS's rules are testable on
//! any OS.

use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

/// Directory name scull uses inside each platform folder.
pub const APP_NAME: &str = "scull";

/// Environment variable that puts all of scull's directories under one root (tests,
/// portable setups).
pub const HOME_OVERRIDE_ENV: &str = "SCULL_HOME";

/// scull's per-user directories. See the [module docs](self) for the per-OS locations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[non_exhaustive]
pub struct AppDirs {
    /// User configuration (`config.toml`, CFG-01). May roam on Windows.
    pub config: PathBuf,
    /// The database and quarantine metadata (DATA-01, SAFE-16). Never roams.
    pub data: PathBuf,
    /// Disposable cache; may be deleted at any time.
    pub cache: PathBuf,
    /// Logs and other state worth keeping across runs but not part of the data store
    /// (`XDG_STATE_HOME`). Never roams.
    pub state: PathBuf,
}

/// Which OS's directory conventions to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DirLayout {
    /// XDG Base Directory spec: Linux and other Unix except macOS.
    Xdg,
    /// macOS `~/Library` folders.
    MacOs,
    /// Windows known folders.
    Windows,
}

impl DirLayout {
    /// The layout of the OS this binary was built for.
    pub const fn native() -> Self {
        if cfg!(windows) {
            DirLayout::Windows
        } else if cfg!(target_os = "macos") {
            DirLayout::MacOs
        } else {
            DirLayout::Xdg
        }
    }
}

/// One input [`AppDirs::resolve`] asks its lookup closure for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DirSource {
    /// An environment variable, e.g. `XDG_DATA_HOME` or `SCULL_HOME`.
    Env(&'static str),
    /// The user's home directory (Unix: `$HOME`).
    Home,
    /// Windows `FOLDERID_RoamingAppData` (`%APPDATA%`).
    RoamingAppData,
    /// Windows `FOLDERID_LocalAppData` (`%LOCALAPPDATA%`), never roams.
    LocalAppData,
}

impl fmt::Display for DirSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirSource::Env(name) => write!(f, "environment variable {name}"),
            DirSource::Home => f.write_str("home directory"),
            DirSource::RoamingAppData => f.write_str("known folder RoamingAppData"),
            DirSource::LocalAppData => f.write_str("known folder LocalAppData"),
        }
    }
}

/// Why the application directories could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppDirsError {
    /// A required base directory is unknown (e.g. `$HOME` unset).
    Missing(DirSource),
    /// A required base directory, or the `SCULL_HOME` override, is not an absolute path.
    NotAbsolute {
        /// Where the value came from.
        source: DirSource,
        /// The value.
        path: PathBuf,
    },
}

impl fmt::Display for AppDirsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppDirsError::Missing(source) => {
                write!(f, "cannot locate scull's directories: {source} is not set")
            }
            AppDirsError::NotAbsolute { source, path } => write!(
                f,
                "cannot locate scull's directories: {source} is not an absolute path ({})",
                path.display()
            ),
        }
    }
}

impl std::error::Error for AppDirsError {}

/// scull's directories on this machine, honoring `SCULL_HOME` and the OS conventions in the
/// [module docs](self). Computes paths only; creates nothing.
pub fn app_dirs() -> Result<AppDirs, AppDirsError> {
    AppDirs::resolve(DirLayout::native(), native_lookup)
}

fn native_lookup(source: DirSource) -> Option<OsString> {
    match source {
        DirSource::Env(name) => std::env::var_os(name),
        DirSource::Home => std::env::var_os("HOME"),
        DirSource::RoamingAppData | DirSource::LocalAppData => {
            crate::native::known_folder(source).map(PathBuf::into_os_string)
        }
    }
}

impl AppDirs {
    /// The pure resolver behind [`app_dirs()`]: applies `layout`'s conventions to the values
    /// `lookup` returns, without reading the real environment or touching the filesystem.
    ///
    /// `lookup` returns `None` for an unset value. Empty values count as unset.
    pub fn resolve(
        layout: DirLayout,
        lookup: impl Fn(DirSource) -> Option<OsString>,
    ) -> Result<AppDirs, AppDirsError> {
        let get = |source: DirSource| lookup(source).filter(|v| !v.is_empty());

        let override_source = DirSource::Env(HOME_OVERRIDE_ENV);
        if let Some(root) = get(override_source) {
            let root = absolute(layout, override_source, root)?;
            return Ok(AppDirs {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
                state: root.join("state"),
            });
        }

        match layout {
            DirLayout::Xdg => {
                let home = || -> Result<PathBuf, AppDirsError> {
                    let home =
                        get(DirSource::Home).ok_or(AppDirsError::Missing(DirSource::Home))?;
                    absolute(layout, DirSource::Home, home)
                };
                // XDG spec: unset, empty or relative values are ignored.
                let xdg = |var: &'static str, default: &[&str]| -> Result<PathBuf, AppDirsError> {
                    let base = match get(DirSource::Env(var)) {
                        Some(v) if is_absolute(layout, &v) => PathBuf::from(v),
                        _ => default.iter().fold(home()?, |p, c| p.join(c)),
                    };
                    Ok(base.join(APP_NAME))
                };
                Ok(AppDirs {
                    config: xdg("XDG_CONFIG_HOME", &[".config"])?,
                    data: xdg("XDG_DATA_HOME", &[".local", "share"])?,
                    cache: xdg("XDG_CACHE_HOME", &[".cache"])?,
                    state: xdg("XDG_STATE_HOME", &[".local", "state"])?,
                })
            }
            DirLayout::MacOs => {
                let home = get(DirSource::Home).ok_or(AppDirsError::Missing(DirSource::Home))?;
                let library = absolute(layout, DirSource::Home, home)?.join("Library");
                let support = library.join("Application Support").join(APP_NAME);
                Ok(AppDirs {
                    config: support.clone(),
                    data: support,
                    cache: library.join("Caches").join(APP_NAME),
                    state: library.join("Logs").join(APP_NAME),
                })
            }
            DirLayout::Windows => {
                let folder = |known: DirSource, env: &'static str| {
                    match get(known) {
                        Some(v) => absolute(layout, known, v),
                        None => {
                            let source = DirSource::Env(env);
                            let v = get(source).ok_or(AppDirsError::Missing(known))?;
                            absolute(layout, source, v)
                        }
                    }
                    .map(|p| p.join(APP_NAME))
                };
                let roaming = folder(DirSource::RoamingAppData, "APPDATA")?;
                let local = folder(DirSource::LocalAppData, "LOCALAPPDATA")?;
                Ok(AppDirs {
                    config: roaming,
                    data: local.join("data"),
                    cache: local.join("cache"),
                    state: local.join("state"),
                })
            }
        }
    }

    /// All four directories, in the order config, data, cache, state.
    pub fn all(&self) -> [&Path; 4] {
        [&self.config, &self.data, &self.cache, &self.state]
    }
}

fn absolute(
    layout: DirLayout,
    source: DirSource,
    value: OsString,
) -> Result<PathBuf, AppDirsError> {
    if is_absolute(layout, &value) {
        Ok(PathBuf::from(value))
    } else {
        Err(AppDirsError::NotAbsolute {
            source,
            path: PathBuf::from(value),
        })
    }
}

/// Absolute by `layout`'s rules rather than the host's, so the resolver can be tested for
/// every OS on any OS: `/…` on Unix; `X:\…`, `X:/…` or `\\…` (UNC, verbatim) on Windows.
fn is_absolute(layout: DirLayout, value: &OsStr) -> bool {
    let b = value.as_encoded_bytes();
    match layout {
        DirLayout::Xdg | DirLayout::MacOs => b.first() == Some(&b'/'),
        DirLayout::Windows => {
            let sep = |c: Option<&u8>| matches!(c, Some(b'\\' | b'/'));
            let drive = b.first().is_some_and(u8::is_ascii_alphabetic)
                && b.get(1) == Some(&b':')
                && sep(b.get(2));
            let unc = sep(b.first()) && sep(b.get(1));
            drive || unc
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(DirSource, &str)]) -> impl Fn(DirSource) -> Option<OsString> + use<> {
        let map: HashMap<DirSource, OsString> =
            pairs.iter().map(|(k, v)| (*k, OsString::from(v))).collect();
        move |k| map.get(&k).cloned()
    }

    fn p(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn xdg_defaults_from_home() {
        let dirs = AppDirs::resolve(DirLayout::Xdg, env(&[(DirSource::Home, "/home/u")])).unwrap();
        assert_eq!(dirs.config, p(&["/home/u", ".config", "scull"]));
        assert_eq!(dirs.data, p(&["/home/u", ".local", "share", "scull"]));
        assert_eq!(dirs.cache, p(&["/home/u", ".cache", "scull"]));
        assert_eq!(dirs.state, p(&["/home/u", ".local", "state", "scull"]));
    }

    #[test]
    fn xdg_env_overrides_each_dir() {
        let dirs = AppDirs::resolve(
            DirLayout::Xdg,
            env(&[
                (DirSource::Home, "/home/u"),
                (DirSource::Env("XDG_CONFIG_HOME"), "/x/cfg"),
                (DirSource::Env("XDG_DATA_HOME"), "/x/data"),
                (DirSource::Env("XDG_CACHE_HOME"), "/x/cache"),
                (DirSource::Env("XDG_STATE_HOME"), "/x/state"),
            ]),
        )
        .unwrap();
        assert_eq!(dirs.config, p(&["/x/cfg", "scull"]));
        assert_eq!(dirs.data, p(&["/x/data", "scull"]));
        assert_eq!(dirs.cache, p(&["/x/cache", "scull"]));
        assert_eq!(dirs.state, p(&["/x/state", "scull"]));
    }

    #[test]
    fn xdg_relative_or_empty_values_are_ignored() {
        let dirs = AppDirs::resolve(
            DirLayout::Xdg,
            env(&[
                (DirSource::Home, "/home/u"),
                (DirSource::Env("XDG_CONFIG_HOME"), "relative/cfg"),
                (DirSource::Env("XDG_DATA_HOME"), ""),
                (DirSource::Env("XDG_CACHE_HOME"), "./cache"),
            ]),
        )
        .unwrap();
        assert_eq!(dirs.config, p(&["/home/u", ".config", "scull"]));
        assert_eq!(dirs.data, p(&["/home/u", ".local", "share", "scull"]));
        assert_eq!(dirs.cache, p(&["/home/u", ".cache", "scull"]));
    }

    #[test]
    fn xdg_without_home_needs_every_var() {
        assert_eq!(
            AppDirs::resolve(DirLayout::Xdg, env(&[])),
            Err(AppDirsError::Missing(DirSource::Home))
        );
        assert!(matches!(
            AppDirs::resolve(DirLayout::Xdg, env(&[(DirSource::Home, "home/u")])),
            Err(AppDirsError::NotAbsolute {
                source: DirSource::Home,
                ..
            })
        ));
        let all_set = env(&[
            (DirSource::Env("XDG_CONFIG_HOME"), "/c"),
            (DirSource::Env("XDG_DATA_HOME"), "/d"),
            (DirSource::Env("XDG_CACHE_HOME"), "/k"),
            (DirSource::Env("XDG_STATE_HOME"), "/s"),
        ]);
        assert_eq!(
            AppDirs::resolve(DirLayout::Xdg, all_set).unwrap().data,
            p(&["/d", "scull"])
        );
    }

    #[test]
    fn macos_library_folders() {
        let dirs = AppDirs::resolve(
            DirLayout::MacOs,
            env(&[
                (DirSource::Home, "/Users/u"),
                // XDG variables are not consulted on macOS.
                (DirSource::Env("XDG_DATA_HOME"), "/x/data"),
            ]),
        )
        .unwrap();
        let support = p(&["/Users/u", "Library", "Application Support", "scull"]);
        assert_eq!(dirs.config, support);
        assert_eq!(dirs.data, support);
        assert_eq!(dirs.cache, p(&["/Users/u", "Library", "Caches", "scull"]));
        assert_eq!(dirs.state, p(&["/Users/u", "Library", "Logs", "scull"]));
        assert_eq!(
            AppDirs::resolve(DirLayout::MacOs, env(&[])),
            Err(AppDirsError::Missing(DirSource::Home))
        );
    }

    #[test]
    fn windows_known_folders_data_never_roams() {
        let roaming = r"C:\Users\u\AppData\Roaming";
        let local = r"C:\Users\u\AppData\Local";
        let dirs = AppDirs::resolve(
            DirLayout::Windows,
            env(&[
                (DirSource::RoamingAppData, roaming),
                (DirSource::LocalAppData, local),
                // Known folders win over the environment.
                (DirSource::Env("APPDATA"), r"D:\elsewhere"),
                (DirSource::Env("LOCALAPPDATA"), r"D:\elsewhere"),
            ]),
        )
        .unwrap();
        assert_eq!(dirs.config, Path::new(roaming).join("scull"));
        assert_eq!(dirs.data, Path::new(local).join("scull").join("data"));
        assert_eq!(dirs.cache, Path::new(local).join("scull").join("cache"));
        assert_eq!(dirs.state, Path::new(local).join("scull").join("state"));
        for dir in [&dirs.data, &dirs.cache, &dirs.state] {
            assert!(
                dir.starts_with(local),
                "{} must be under LocalAppData",
                dir.display()
            );
        }
    }

    #[test]
    fn windows_env_fallback_and_errors() {
        let dirs = AppDirs::resolve(
            DirLayout::Windows,
            env(&[
                (DirSource::Env("APPDATA"), r"C:\R"),
                (DirSource::Env("LOCALAPPDATA"), r"\\server\share\L"),
            ]),
        )
        .unwrap();
        assert_eq!(dirs.config, Path::new(r"C:\R").join("scull"));
        assert_eq!(
            dirs.data,
            Path::new(r"\\server\share\L").join("scull").join("data")
        );

        assert_eq!(
            AppDirs::resolve(
                DirLayout::Windows,
                env(&[(DirSource::Env("APPDATA"), r"C:\R")])
            ),
            Err(AppDirsError::Missing(DirSource::LocalAppData))
        );
        assert!(matches!(
            AppDirs::resolve(
                DirLayout::Windows,
                env(&[
                    (DirSource::RoamingAppData, r"C:relative"),
                    (DirSource::LocalAppData, r"C:\L")
                ])
            ),
            Err(AppDirsError::NotAbsolute {
                source: DirSource::RoamingAppData,
                ..
            })
        ));
    }

    #[test]
    fn scull_home_override_puts_everything_under_one_root() {
        for (layout, root) in [
            (DirLayout::Xdg, "/tmp/sh"),
            (DirLayout::MacOs, "/tmp/sh"),
            (DirLayout::Windows, r"C:\tmp\sh"),
        ] {
            let dirs = AppDirs::resolve(
                layout,
                env(&[
                    (DirSource::Env(HOME_OVERRIDE_ENV), root),
                    (DirSource::Home, "/home/u"),
                    (DirSource::Env("XDG_DATA_HOME"), "/x/data"),
                    (DirSource::RoamingAppData, r"C:\R"),
                    (DirSource::LocalAppData, r"C:\L"),
                ]),
            )
            .unwrap();
            let root = Path::new(root);
            assert_eq!(dirs.config, root.join("config"), "{layout:?}");
            assert_eq!(dirs.data, root.join("data"), "{layout:?}");
            assert_eq!(dirs.cache, root.join("cache"), "{layout:?}");
            assert_eq!(dirs.state, root.join("state"), "{layout:?}");
        }
    }

    #[test]
    fn relative_scull_home_is_an_error_not_a_fallback() {
        let lookup = env(&[
            (DirSource::Env(HOME_OVERRIDE_ENV), "rel"),
            (DirSource::Home, "/home/u"),
        ]);
        assert_eq!(
            AppDirs::resolve(DirLayout::Xdg, lookup),
            Err(AppDirsError::NotAbsolute {
                source: DirSource::Env(HOME_OVERRIDE_ENV),
                path: PathBuf::from("rel"),
            })
        );
        // Empty means unset.
        let lookup = env(&[
            (DirSource::Env(HOME_OVERRIDE_ENV), ""),
            (DirSource::Home, "/home/u"),
        ]);
        assert_eq!(
            AppDirs::resolve(DirLayout::Xdg, lookup).unwrap().data,
            p(&["/home/u", ".local", "share", "scull"])
        );
    }

    #[test]
    fn absolute_rules_per_layout() {
        for (layout, v, want) in [
            (DirLayout::Xdg, "/a", true),
            (DirLayout::Xdg, "a/b", false),
            (DirLayout::Xdg, r"C:\a", false),
            (DirLayout::Windows, r"C:\a", true),
            (DirLayout::Windows, "c:/a", true),
            (DirLayout::Windows, r"\\?\C:\a", true),
            (DirLayout::Windows, r"\\srv\share", true),
            (DirLayout::Windows, r"C:a", false),
            (DirLayout::Windows, r"\a", false),
            (DirLayout::Windows, "/a", false),
        ] {
            assert_eq!(is_absolute(layout, OsStr::new(v)), want, "{layout:?} {v}");
        }
    }

    #[test]
    fn native_layout_matches_target() {
        let want = if cfg!(windows) {
            DirLayout::Windows
        } else if cfg!(target_os = "macos") {
            DirLayout::MacOs
        } else {
            DirLayout::Xdg
        };
        assert_eq!(DirLayout::native(), want);
    }
}
