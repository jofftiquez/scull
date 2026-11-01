//! Where each AI client keeps its MCP configuration and user-level instructions, per OS.
//!
//! Everything here is computed from an [`Env`] (home folder, OS layout, environment lookup), so
//! every OS's locations are testable on any OS and tests never see the real home folder.
//! Probing is read-only (`symlink_metadata`), and only happens in [`Target::detected`].
//!
//! | Client | MCP config | Key | Guidance |
//! |---|---|---|---|
//! | Claude Code | `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json`) | `mcpServers` | `~/.claude/CLAUDE.md` |
//! | Codex | `~/.codex/config.toml` (`$CODEX_HOME`) | `[mcp_servers.scull]` | `~/.codex/AGENTS.md` |
//! | Cursor | `~/.cursor/mcp.json` | `mcpServers` | printed only |
//! | VS Code | `<user dir>/Code/User/mcp.json` | `servers` | printed only |
//! | Gemini CLI | `~/.gemini/settings.json` | `mcpServers` | `~/.gemini/GEMINI.md` |
//!
//! VS Code's user folder is `$XDG_CONFIG_HOME` (default `~/.config`) on Linux,
//! `~/Library/Application Support` on macOS and `%APPDATA%` (default `~\AppData\Roaming`) on
//! Windows. Cursor and VS Code have no user-level instruction file, so their guidance is
//! printed for the user to paste into project rules.

use crate::cli::AgentClient;
use scull_platform::DirLayout;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The inputs that decide client locations. Tests build one around a temp home.
pub struct Env<'a> {
    /// The user's home folder.
    pub home: PathBuf,
    /// Which OS's conventions apply.
    pub layout: DirLayout,
    /// Environment lookup (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `XDG_CONFIG_HOME`, `APPDATA`).
    pub var: &'a dyn Fn(&str) -> Option<OsString>,
}

impl Env<'_> {
    /// An absolute, non-empty environment value (by `layout`'s rules), else `None`.
    fn abs_var(&self, name: &str) -> Option<PathBuf> {
        let v = (self.var)(name).filter(|v| !v.is_empty())?;
        let p = PathBuf::from(&v);
        let abs = p.is_absolute() || (self.layout == DirLayout::Windows && windows_absolute(&v));
        abs.then_some(p)
    }
}

/// `X:\…`, `X:/…` or `\\…`, independent of the host OS.
fn windows_absolute(v: &OsString) -> bool {
    let b = v.as_encoded_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
        || b.starts_with(br"\\")
}

/// How the client's MCP config file is structured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    /// A JSON object whose `key` maps server names to entries; `typed` adds `"type": "stdio"`.
    Json {
        /// The servers object, e.g. `mcpServers`.
        key: &'static str,
        /// Whether entries carry `"type": "stdio"`.
        typed: bool,
    },
    /// Codex's TOML `[mcp_servers.<name>]` tables.
    Toml,
}

/// One client's files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The MCP config file.
    pub config: PathBuf,
    /// Its format.
    pub format: ConfigFormat,
    /// The user-level instruction file, if the client has one.
    pub guidance: Option<PathBuf>,
    /// A folder whose presence suggests the client is installed.
    pub probe: PathBuf,
}

impl Target {
    /// Whether the client looks installed (its folder exists). Read-only.
    pub fn detected(&self) -> bool {
        std::fs::symlink_metadata(&self.probe).is_ok()
    }
}

fn join(base: &Path, parts: &[&str]) -> PathBuf {
    parts.iter().fold(base.to_path_buf(), |p, c| p.join(c))
}

/// Where `client` keeps its files under `env`.
pub fn target(client: AgentClient, env: &Env<'_>) -> Target {
    let home = &env.home;
    match client {
        AgentClient::ClaudeCode => {
            let dir = env
                .abs_var("CLAUDE_CONFIG_DIR")
                .unwrap_or_else(|| home.join(".claude"));
            let config = match env.abs_var("CLAUDE_CONFIG_DIR") {
                Some(d) => d.join(".claude.json"),
                None => home.join(".claude.json"),
            };
            Target {
                config,
                format: ConfigFormat::Json {
                    key: "mcpServers",
                    typed: true,
                },
                guidance: Some(dir.join("CLAUDE.md")),
                probe: dir,
            }
        }
        AgentClient::Codex => {
            let dir = env
                .abs_var("CODEX_HOME")
                .unwrap_or_else(|| home.join(".codex"));
            Target {
                config: dir.join("config.toml"),
                format: ConfigFormat::Toml,
                guidance: Some(dir.join("AGENTS.md")),
                probe: dir,
            }
        }
        AgentClient::Cursor => {
            let dir = home.join(".cursor");
            Target {
                config: dir.join("mcp.json"),
                format: ConfigFormat::Json {
                    key: "mcpServers",
                    typed: false,
                },
                guidance: None,
                probe: dir,
            }
        }
        AgentClient::Vscode => {
            let base = match env.layout {
                DirLayout::MacOs => join(home, &["Library", "Application Support"]),
                DirLayout::Windows => env
                    .abs_var("APPDATA")
                    .unwrap_or_else(|| join(home, &["AppData", "Roaming"])),
                _ => env
                    .abs_var("XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home.join(".config")),
            };
            let user = join(&base, &["Code", "User"]);
            Target {
                config: user.join("mcp.json"),
                format: ConfigFormat::Json {
                    key: "servers",
                    typed: true,
                },
                guidance: None,
                probe: base.join("Code"),
            }
        }
        AgentClient::GeminiCli => {
            let dir = home.join(".gemini");
            Target {
                config: dir.join("settings.json"),
                format: ConfigFormat::Json {
                    key: "mcpServers",
                    typed: false,
                },
                guidance: Some(dir.join("GEMINI.md")),
                probe: dir,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with<'a>(
        home: &str,
        layout: DirLayout,
        var: &'a dyn Fn(&str) -> Option<OsString>,
    ) -> Env<'a> {
        Env {
            home: PathBuf::from(home),
            layout,
            var,
        }
    }

    #[test]
    fn unix_locations() {
        let none = |_: &str| None;
        let env = env_with("/h", DirLayout::Xdg, &none);
        let t = target(AgentClient::ClaudeCode, &env);
        assert_eq!(t.config, Path::new("/h/.claude.json"));
        assert_eq!(
            t.guidance.as_deref(),
            Some(Path::new("/h/.claude/CLAUDE.md"))
        );
        let t = target(AgentClient::Codex, &env);
        assert_eq!(t.config, Path::new("/h/.codex/config.toml"));
        assert_eq!(t.format, ConfigFormat::Toml);
        assert_eq!(
            target(AgentClient::Cursor, &env).config,
            Path::new("/h/.cursor/mcp.json")
        );
        let t = target(AgentClient::Vscode, &env);
        assert_eq!(t.config, Path::new("/h/.config/Code/User/mcp.json"));
        assert_eq!(
            t.format,
            ConfigFormat::Json {
                key: "servers",
                typed: true
            }
        );
        assert_eq!(
            target(AgentClient::GeminiCli, &env).config,
            Path::new("/h/.gemini/settings.json")
        );
        let env = env_with("/h", DirLayout::MacOs, &none);
        assert_eq!(
            target(AgentClient::Vscode, &env).config,
            Path::new("/h/Library/Application Support/Code/User/mcp.json")
        );
    }

    #[test]
    fn environment_overrides_must_be_absolute() {
        let var = |k: &str| match k {
            "CODEX_HOME" => Some(OsString::from("/c")),
            "XDG_CONFIG_HOME" => Some(OsString::from("relative")),
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/cc")),
            _ => None,
        };
        let env = env_with("/h", DirLayout::Xdg, &var);
        assert_eq!(
            target(AgentClient::Codex, &env).config,
            Path::new("/c/config.toml")
        );
        // A relative XDG value is ignored (XDG spec).
        assert_eq!(
            target(AgentClient::Vscode, &env).config,
            Path::new("/h/.config/Code/User/mcp.json")
        );
        let t = target(AgentClient::ClaudeCode, &env);
        assert_eq!(t.config, Path::new("/cc/.claude.json"));
        assert_eq!(t.guidance.as_deref(), Some(Path::new("/cc/CLAUDE.md")));
    }

    #[test]
    fn windows_vscode_uses_appdata() {
        let var = |k: &str| (k == "APPDATA").then(|| OsString::from(r"C:\Users\u\AppData\Roaming"));
        let env = env_with(r"C:\Users\u", DirLayout::Windows, &var);
        let t = target(AgentClient::Vscode, &env);
        assert!(
            t.config
                .to_string_lossy()
                .starts_with(r"C:\Users\u\AppData\Roaming"),
            "{}",
            t.config.display()
        );
        assert!(t.config.ends_with("mcp.json"));
        let none = |_: &str| None;
        let env = env_with(r"C:\Users\u", DirLayout::Windows, &none);
        assert!(
            target(AgentClient::Vscode, &env)
                .config
                .to_string_lossy()
                .contains("AppData")
        );
    }
}
