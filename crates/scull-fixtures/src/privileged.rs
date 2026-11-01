//! Privileged fixtures (docs/FIXTURES.md §2, mode **Nightly**): tests that mount filesystems
//! (FX-FS-04, FX-LNX-02/03/06, FX-MAC-05) acquire the privileges they need here, or are
//! skipped with a reason. They are `#[ignore]`d, so `cargo test --workspace` never runs them;
//! the nightly workflow (`.github/workflows/nightly-fixtures.yml`) runs them with
//! `-- --ignored` and [`REQUIRE_ENV`]`=1`, which turns a skip into a failure.
//!
//! A test opts in with [`require_privileges!`](crate::require_privileges) as its first
//! statement. Like [`isolate_network!`](crate::isolate_network), the test binary re-executes
//! itself, running only that test, in a child that has the privileges, and the parent returns
//! once the child passed:
//!
//! | OS | [`Need::Mounts`] (tmpfs, bind mounts, disk images) | [`Need::Root`] (loop devices, Btrfs, NFS) |
//! |---|---|---|
//! | Linux | in-process if real root; else `unshare --user --map-root-user --mount` (unprivileged user + mount namespace); else, with [`ALLOW_SUDO_ENV`]`=1`, `sudo -n unshare --mount` | in-process if real root; else, with [`ALLOW_SUDO_ENV`]`=1`, `sudo -n unshare --mount` |
//! | macOS | in-process: `hdiutil` attaches disk images without root | in-process if root; else, with [`ALLOW_SUDO_ENV`]`=1`, `sudo -n` |
//! | Windows | not available (no mount fixture targets Windows) | not available |
//!
//! Every Linux child runs in a **private mount namespace** (`--propagation private`), so a
//! mount the test forgets (or a test that crashes) disappears with the child and never shows
//! up on the host. `sudo` is only ever used non-interactively (`-n`) and only when
//! [`ALLOW_SUDO_ENV`]`=1` is set (the nightly workflow on hosted runners; never by default on a
//! developer machine).
//!
//! Ubuntu ≥ 23.10 (`kernel.apparmor_restrict_unprivileged_userns=1`) forbids an unprivileged
//! process to map uids in a new user namespace, so `unshare --map-root-user` fails there and,
//! without root or sudo, the tests are skipped with that reason. To run them on such a machine,
//! run the test binary as root in a disposable environment, e.g. a privileged container (see
//! docs/FIXTURES.md §2.1).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Set in the environment of the privileged child; the value names how it got its privileges.
pub const PRIVILEGED_ENV: &str = "SCULL_FIXTURES_PRIVILEGED";

/// When set to `1`, a privileged test panics instead of being skipped when this host can't
/// give it the privileges it needs (the nightly workflow sets it).
pub const REQUIRE_ENV: &str = "SCULL_FIXTURES_REQUIRE_PRIVILEGED";

/// When set to `1`, a privileged test may re-execute itself with `sudo -n` (non-interactive;
/// fails rather than prompting). Only for disposable machines such as CI runners.
pub const ALLOW_SUDO_ENV: &str = "SCULL_FIXTURES_ALLOW_SUDO";

/// What a privileged fixture needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Mount a tmpfs or bind mount (Linux: an unprivileged user + mount namespace suffices),
    /// or attach a disk image (macOS: no root needed).
    Mounts,
    /// Real root: loop devices, Btrfs, NFS (Linux; in a private mount namespace).
    Root,
}

/// The outcome of [`acquire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Privileges {
    /// Run the test body here; the string says why this process has the privileges.
    Here(String),
    /// A privileged child ran the test and it passed: return without running the body.
    RanInChild,
    /// This host can't provide the privileges; the string says why.
    Unavailable(String),
}

/// How a privileged child is started.
#[derive(Debug, Clone)]
struct Launcher {
    program: PathBuf,
    prefix: Vec<OsString>,
    label: &'static str,
}

fn first_existing(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Whether `program prefix... /usr/bin/true` succeeds.
fn probe(l: &Launcher) -> Result<(), String> {
    let true_bin = first_existing(&["/usr/bin/true", "/bin/true"]).ok_or("`true` not found")?;
    let out = Command::new(&l.program)
        .args(&l.prefix)
        .arg(true_bin)
        .output()
        .map_err(|e| format!("{}: {e}", l.program.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{} failed ({}): {}",
            l.label,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn sudo_allowed() -> bool {
    std::env::var(ALLOW_SUDO_ENV).is_ok_and(|v| v == "1")
}

/// Whether this process is real root (uid 0 in the initial user namespace).
pub fn is_real_root() -> bool {
    let uid0 = Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "0");
    if !uid0 {
        return false;
    }
    if cfg!(target_os = "linux") {
        // Inside a user namespace uid 0 maps to someone else: `0 1000 1`, not `0 0 4294967295`.
        return std::fs::read_to_string("/proc/self/uid_map").is_ok_and(|m| {
            m.lines().any(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() == 3 && f[0] == "0" && f[1] == "0"
            })
        });
    }
    true
}

/// The environment forwarded through `sudo` (which resets it): the harness's own variables
/// plus `PATH` and `RUST_BACKTRACE`. Names compare case-insensitively on Windows, where the
/// search path is usually spelled `Path`.
fn forwarded_env(label: &str) -> Vec<OsString> {
    let is = |k: &str, name: &str| {
        if cfg!(windows) {
            k.eq_ignore_ascii_case(name)
        } else {
            k == name
        }
    };
    let mut env: Vec<OsString> = vec!["env".into(), format!("{PRIVILEGED_ENV}={label}").into()];
    for (k, v) in std::env::vars_os() {
        let ks = k.to_string_lossy();
        if (ks.starts_with("SCULL_") && !is(&ks, PRIVILEGED_ENV))
            || is(&ks, "PATH")
            || is(&ks, "RUST_BACKTRACE")
        {
            let mut kv = k.clone();
            kv.push("=");
            kv.push(&v);
            env.push(kv);
        }
    }
    env
}

fn launchers(need: Need) -> Vec<Launcher> {
    let mut out = Vec::new();
    if cfg!(target_os = "linux") {
        let Some(unshare) = first_existing(&["/usr/bin/unshare", "/bin/unshare"]) else {
            return out;
        };
        if need == Need::Mounts {
            out.push(Launcher {
                program: unshare.clone(),
                prefix: [
                    "--user",
                    "--map-root-user",
                    "--mount",
                    "--propagation",
                    "private",
                    "--",
                ]
                .iter()
                .map(OsString::from)
                .collect(),
                label: "linux user+mount namespace (unshare --user --map-root-user --mount)",
            });
        }
        if sudo_allowed()
            && let Some(sudo) = first_existing(&["/usr/bin/sudo", "/bin/sudo"])
        {
            let label = "root in a private mount namespace (sudo -n unshare --mount)";
            let mut prefix: Vec<OsString> = vec!["-n".into()];
            prefix.extend(forwarded_env(label));
            prefix.push(unshare.into_os_string());
            prefix.extend(
                ["--mount", "--propagation", "private", "--"]
                    .iter()
                    .map(OsString::from),
            );
            out.push(Launcher {
                program: sudo,
                prefix,
                label,
            });
        }
    } else if cfg!(target_os = "macos")
        && need == Need::Root
        && sudo_allowed()
        && let Some(sudo) = first_existing(&["/usr/bin/sudo"])
    {
        let label = "root (sudo -n)";
        let mut prefix: Vec<OsString> = vec!["-n".into()];
        prefix.extend(forwarded_env(label));
        out.push(Launcher {
            program: sudo,
            prefix,
            label,
        });
    }
    out
}

/// Give the current test (`test_name`, as libtest names it) the privileges `need` asks for:
/// run it here, in a privileged child, or report why that is impossible. Prefer
/// [`require_privileges!`](crate::require_privileges).
///
/// Panics with the child's output when the child fails or ran no test.
pub fn acquire(test_name: &str, need: Need) -> Privileges {
    if let Ok(label) = std::env::var(PRIVILEGED_ENV)
        && !label.is_empty()
    {
        return Privileges::Here(label);
    }
    if cfg!(windows) {
        return Privileges::Unavailable("no privileged mount fixtures on Windows".into());
    }
    if is_real_root() {
        return Privileges::Here("real root".into());
    }
    if cfg!(target_os = "macos") && need == Need::Mounts {
        return Privileges::Here("macOS: hdiutil attaches disk images without root".into());
    }
    let mut errors = Vec::new();
    for l in launchers(need) {
        match probe(&l) {
            Ok(()) => {
                run_child(&l, test_name);
                return Privileges::RanInChild;
            }
            Err(e) => errors.push(e),
        }
    }
    if !sudo_allowed() {
        errors.push(format!("{ALLOW_SUDO_ENV} is not 1, so sudo is not tried"));
    }
    if need == Need::Root {
        errors.push("needs real root".into());
    }
    Privileges::Unavailable(errors.join("; "))
}

fn run_child(l: &Launcher, test_name: &str) {
    let exe = std::env::current_exe().expect("current test executable");
    let out = Command::new(&l.program)
        .args(&l.prefix)
        .arg(&exe)
        .args([
            "--exact",
            test_name,
            "--nocapture",
            "--test-threads=1",
            "--include-ignored",
        ])
        .env(PRIVILEGED_ENV, l.label)
        .output()
        .expect("spawn the privileged test child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    print!("{stdout}");
    eprint!("{stderr}");
    assert!(
        out.status.success(),
        "`{test_name}` failed in its privileged child ({}; {}); its output is above",
        l.label,
        out.status
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the privileged child ran no test named `{test_name}`; its output is above"
    );
}

/// Report a privileged test that can't run here: a panic with [`REQUIRE_ENV`]`=1`, else a
/// `SKIPPED` line on stdout (shown with `--nocapture`).
pub fn skip(test_name: &str, why: &str) {
    if std::env::var(REQUIRE_ENV).is_ok_and(|v| v == "1") {
        panic!("{REQUIRE_ENV}=1 but `{test_name}` can't get its privileges: {why}");
    }
    println!("SKIPPED {test_name}: {why}");
}

/// Whether a tool is on `PATH` or in the usual system directories.
pub fn has_tool(name: &str) -> bool {
    tool_path(name).is_some()
}

/// The path of a tool on `PATH` or in `/usr/sbin`, `/sbin`, `/usr/bin`, `/bin`.
pub fn tool_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/sbin", "/usr/bin", "/bin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// `path` exists and is a directory.
pub(crate) fn is_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Opt the current `#[test]` into privileges (see [`crate::privileged`]); use it as the first
/// statement of a test returning `()`. Evaluates to the label of how this process got them
/// (`String`) when the body should run here; returns from the test when a privileged child
/// already ran it, or when the host can't provide them (a `SKIPPED` line, or a panic with
/// [`REQUIRE_ENV`](crate::privileged::REQUIRE_ENV)`=1`).
///
/// ```no_run
/// #[test]
/// #[ignore = "nightly: needs mount privileges"]
/// fn fx_fs_04_example() {
///     let how = scull_fixtures::require_privileges!(scull_fixtures::privileged::Need::Mounts);
///     println!("privileged: {how}");
///     // ... mount with scull_fixtures::Mounts, scan, check_invariants ...
/// }
/// ```
#[macro_export]
macro_rules! require_privileges {
    ($need:expr) => {{
        fn __scull_fixtures_here() {}
        let name = $crate::net::test_name_from_type_name(::std::any::type_name_of_val(
            &__scull_fixtures_here,
        ));
        match $crate::privileged::acquire(&name, $need) {
            $crate::privileged::Privileges::Here(label) => label,
            $crate::privileged::Privileges::RanInChild => return,
            $crate::privileged::Privileges::Unavailable(why) => {
                $crate::privileged::skip(&name, &why);
                return;
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwarded_env_names_the_label_first_and_keeps_path() {
        let env = forwarded_env("x");
        assert_eq!(env[0], "env");
        assert_eq!(env[1], OsString::from(format!("{PRIVILEGED_ENV}=x")));
        if std::env::var_os("PATH").is_some() {
            // `Path=` on Windows, where names are case-insensitive.
            assert!(env.iter().any(|e| {
                e.to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with("PATH=")
            }));
        }
    }

    #[test]
    fn no_sudo_launcher_unless_allowed() {
        if !sudo_allowed() {
            assert!(
                launchers(Need::Root)
                    .iter()
                    .all(|l| !l.program.ends_with("sudo"))
            );
        }
    }
}
