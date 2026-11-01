//! The OS schedulers that run `scull check` periodically (PRO-07), and the one place their
//! tools are launched: `launchctl` (macOS LaunchAgents), `systemctl --user` (systemd user
//! timers) and `schtasks` (Windows Task Scheduler).
//!
//! * [`Scheduler::native`]: which scheduler this OS has.
//! * [`Scheduler::program`]: the tool's absolute path (never resolved through `PATH`, so a
//!   folder early in `PATH` can't substitute its own `launchctl`).
//! * [`CommandRunner`]: runs one tool invocation. [`SystemRunner`] is the real one; callers
//!   inject a fake in tests, so **no test ever registers a real schedule**.
//!
//! Nothing here decides *what* to register or asks for consent; `scull schedule` does that and
//! shows every file and command first.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// An OS scheduler scull can register `scull check` with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheduler {
    /// macOS `launchd`: a per-user LaunchAgent (`~/Library/LaunchAgents`).
    Launchd,
    /// systemd user manager: a `.timer` and `.service` in `~/.config/systemd/user`.
    SystemdUser,
    /// Windows Task Scheduler: a per-user task defined by an XML file (`schtasks /XML`).
    TaskScheduler,
}

impl Scheduler {
    /// The scheduler of the OS named `os` (as in [`std::env::consts::OS`]): `macos` →
    /// [`Scheduler::Launchd`], `linux` → [`Scheduler::SystemdUser`], `windows` →
    /// [`Scheduler::TaskScheduler`]; `None` elsewhere.
    pub fn for_os(os: &str) -> Option<Self> {
        match os {
            "macos" => Some(Scheduler::Launchd),
            "linux" => Some(Scheduler::SystemdUser),
            "windows" => Some(Scheduler::TaskScheduler),
            _ => None,
        }
    }

    /// The scheduler of this OS.
    pub fn native() -> Option<Self> {
        Self::for_os(std::env::consts::OS)
    }

    /// `launchd`, `systemd`, `task_scheduler`.
    pub fn as_str(self) -> &'static str {
        match self {
            Scheduler::Launchd => "launchd",
            Scheduler::SystemdUser => "systemd",
            Scheduler::TaskScheduler => "task_scheduler",
        }
    }

    /// The tool's absolute path: `/bin/launchctl`; `/usr/bin/systemctl` (or `/bin/systemctl`
    /// when only that exists); `%SystemRoot%\System32\schtasks.exe` (`C:\Windows` when
    /// `SystemRoot` is unset).
    pub fn program(self) -> PathBuf {
        match self {
            Scheduler::Launchd => PathBuf::from("/bin/launchctl"),
            Scheduler::SystemdUser => ["/usr/bin/systemctl", "/bin/systemctl"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
                .unwrap_or_else(|| PathBuf::from("/usr/bin/systemctl")),
            Scheduler::TaskScheduler => {
                let root = std::env::var_os("SystemRoot")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
                root.join("System32").join("schtasks.exe")
            }
        }
    }
}

/// What one tool invocation produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    /// Exit code (`None` when killed by a signal).
    pub code: Option<i32>,
    /// Standard output, lossily decoded.
    pub stdout: String,
    /// Standard error, lossily decoded.
    pub stderr: String,
}

impl CommandOutput {
    /// Whether the tool exited with code 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs scheduler tools. Implemented by [`SystemRunner`]; tests inject a fake.
pub trait CommandRunner {
    /// Run `program` with `args` (no shell, stdin closed) and wait for it.
    fn run(&self, program: &Path, args: &[String]) -> io::Result<CommandOutput>;
}

/// The real runner: spawns the tool directly (no shell), stdin closed, output captured.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &Path, args: &[String]) -> io::Result<CommandOutput> {
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        Ok(CommandOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_os_has_its_scheduler_and_absolute_tool() {
        assert_eq!(Scheduler::for_os("macos"), Some(Scheduler::Launchd));
        assert_eq!(Scheduler::for_os("linux"), Some(Scheduler::SystemdUser));
        assert_eq!(Scheduler::for_os("windows"), Some(Scheduler::TaskScheduler));
        assert_eq!(Scheduler::for_os("freebsd"), None);
        for s in [
            Scheduler::Launchd,
            Scheduler::SystemdUser,
            Scheduler::TaskScheduler,
        ] {
            let p = s.program();
            // An absolute path on its own OS; on others at least not a bare name.
            assert!(p.components().count() > 1, "{p:?}");
        }
        assert_eq!(Scheduler::TaskScheduler.as_str(), "task_scheduler");
    }
}
