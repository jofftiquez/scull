//! Invariant I-6, "no network access unless the fixture is tagged `net`" (docs/FIXTURES.md §1;
//! scull has no network code outside opt-in forge lookups, PRD D-9 and principle 4).
//!
//! Two lines of defence, never confused with each other in the report:
//!
//! 1. **A real block** (`I-6 passed (blocked: ...)`). A test opts in with
//!    [`isolate_network!`](crate::isolate_network) (or [`run_isolated`]): the test binary
//!    re-executes itself, running only that test, inside an OS network sandbox, with
//!    [`SANDBOX_ENV`] naming the sandbox. Inside, [`crate::invariants::network_blocked`] does not
//!    trust the variable: it verifies the block with a zero-traffic route probe (a UDP `connect`
//!    to TEST-NET-1 `192.0.2.1` and `2001:db8::1` only asks the kernel for a route and records
//!    the peer; no packet is sent) and fails I-6 if a route exists.
//! 2. **A best-effort detector** (`I-6 passed (best-effort: ...)`), everywhere else: the
//!    internet sockets the process holds when the invariants are checked. Any non-loopback
//!    socket fails I-6, inside a sandbox too (an attempt was made even if it could not get
//!    out).
//!
//! | OS | Detector source |
//! |---|---|
//! | Linux | socket inodes of `/proc/self/fd` matched against `/proc/self/net/{tcp,tcp6,udp,udp6,raw,raw6,packet}` |
//! | macOS | `/usr/sbin/lsof -nP -a -p <pid> -i -F Pn` |
//! | Windows | `%SystemRoot%\System32\netstat.exe -ano`, filtered by PID |
//!
//! The detector is only best-effort: it sees sockets still open when it runs (a connection
//! opened and closed during the fixture goes unseen), not those of child processes (e.g.
//! `git`), and it sees the whole process, so a socket held by another test thread is attributed
//! to the fixture being checked. A fixture that deliberately opens sockets therefore belongs in
//! its own test binary.
//!
//! | OS | Sandbox used by [`run_isolated`] | Notes |
//! |---|---|---|
//! | Linux | `unshare --user --map-current-user --net`, falling back to `unshare --user --net` | A new network namespace has only a downed `lo`. Ubuntu ≥ 23.10 (`kernel.apparmor_restrict_unprivileged_userns=1`, incl. GitHub `ubuntu-latest`) forbids writing `uid_map`, so the fallback runs unmapped: the process sees every uid as the overflow uid (65534). File access is unchanged (the kernel checks the real uid), but a fixture that compares uids should not opt in. Unavailable if user namespaces are disabled (`kernel.unprivileged_userns_clone=0`) or `unshare` is missing. |
//! | macOS | `sandbox-exec -p '(version 1)(allow default)(deny network*)'` | `sandbox-exec` is deprecated but shipped; probed once with `/usr/bin/true`. |
//! | Windows | none | There is no unprivileged per-process network block. A Windows Firewall rule for the test binary needs admin rights and changes machine-wide state, which the harness never does (FIXTURES §2 safety). Fixtures run in-process with the best-effort detector only. |
//!
//! Known gaps of the sandbox, documented rather than hidden: on Linux a network namespace does
//! not stop a Unix-socket request to a host daemon (e.g. `systemd-resolved`'s varlink socket for
//! DNS); `sandbox-exec`'s `network*` covers Unix sockets too.
//!
//! Set [`REQUIRE_SANDBOX_ENV`]`=1` (e.g. in CI on Linux and macOS) to make [`run_isolated`]
//! panic instead of falling back when no sandbox is available.

pub(crate) mod sockets;

use crate::invariants::Outcome;
use std::borrow::Cow;
use std::ffi::OsString;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Set in the environment of a process running inside a network sandbox; the value names the
/// sandbox. [`run_isolated`] sets it for its child. Whoever starts a whole test run inside a
/// sandbox may set it too (any non-empty value): the block is verified either way.
pub const SANDBOX_ENV: &str = "SCULL_FIXTURES_NET_SANDBOX";

/// When set to `1`, [`run_isolated`] panics if this host offers no network sandbox instead of
/// running the test in-process with the best-effort detector.
pub const REQUIRE_SANDBOX_ENV: &str = "SCULL_FIXTURES_REQUIRE_NET_SANDBOX";

/// TEST-NET-1 (RFC 5737) and the IPv6 documentation prefix (RFC 3849): never routed on the
/// internet. Used by the route probe and by the tests' deliberate network attempts.
pub const UNROUTABLE_V4: &str = "192.0.2.1:9";
/// See [`UNROUTABLE_V4`].
pub const UNROUTABLE_V6: &str = "[2001:db8::1]:9";

/// Serializes the detector and the probe across test threads, so the probe's own socket is
/// never seen by a concurrent check.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The sandbox this process runs in, as named by [`SANDBOX_ENV`], if any.
pub fn active_sandbox() -> Option<String> {
    std::env::var(SANDBOX_ENV).ok().filter(|v| !v.is_empty())
}

/// How [`run_isolated`] starts a sandboxed child on this host.
#[derive(Debug, Clone)]
struct Launcher {
    program: PathBuf,
    prefix: Vec<OsString>,
    label: &'static str,
}

impl Launcher {
    fn command(&self, exe: &Path) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.prefix).arg(exe).env(SANDBOX_ENV, self.label);
        c
    }
}

fn first_existing(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Whether `program prefix... true` exits successfully (stdio discarded).
fn probe(program: &Path, prefix: &[OsString], true_bin: &Path) -> Result<(), String> {
    let out = Command::new(program)
        .args(prefix)
        .arg(true_bin)
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{} {}` failed ({}): {}",
            program.display(),
            prefix
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn find_launcher() -> Result<Launcher, String> {
    if cfg!(target_os = "linux") {
        let unshare = first_existing(&["/usr/bin/unshare", "/bin/unshare"])
            .ok_or("`unshare` not found in /usr/bin or /bin")?;
        let true_bin = first_existing(&["/usr/bin/true", "/bin/true"]).ok_or("`true` not found")?;
        let variants: [(&[&str], &'static str); 2] = [
            (
                &["--user", "--map-current-user", "--net", "--"],
                "linux-netns (user namespace, uid mapped)",
            ),
            (
                &["--user", "--net", "--"],
                "linux-netns (user namespace, uid unmapped)",
            ),
        ];
        let mut errors = Vec::new();
        for (args, label) in variants {
            let prefix: Vec<OsString> = args.iter().map(OsString::from).collect();
            match probe(&unshare, &prefix, &true_bin) {
                Ok(()) => {
                    return Ok(Launcher {
                        program: unshare,
                        prefix,
                        label,
                    });
                }
                Err(e) => errors.push(e),
            }
        }
        Err(format!(
            "no unprivileged network namespace: {}",
            errors.join("; ")
        ))
    } else if cfg!(target_os = "macos") {
        let sandbox_exec =
            first_existing(&["/usr/bin/sandbox-exec"]).ok_or("/usr/bin/sandbox-exec not found")?;
        let prefix: Vec<OsString> = vec![
            "-p".into(),
            "(version 1)(allow default)(deny network*)".into(),
        ];
        probe(&sandbox_exec, &prefix, Path::new("/usr/bin/true"))?;
        Ok(Launcher {
            program: sandbox_exec,
            prefix,
            label: "macos-sandbox-exec (deny network*)",
        })
    } else if cfg!(windows) {
        Err(
            "Windows has no unprivileged per-process network block; a Windows Firewall rule \
             needs admin rights and changes machine-wide state, which the harness never does"
                .into(),
        )
    } else {
        Err("no network sandbox implemented for this OS".into())
    }
}

fn launcher() -> Result<&'static Launcher, &'static str> {
    static LAUNCHER: OnceLock<Result<Launcher, String>> = OnceLock::new();
    match LAUNCHER.get_or_init(find_launcher) {
        Ok(l) => Ok(l),
        Err(e) => Err(e.as_str()),
    }
}

/// The sandbox [`run_isolated`] would use on this host (its label), or why there is none.
/// Probed once per process.
pub fn sandbox_support() -> Result<&'static str, &'static str> {
    launcher().map(|l| l.label)
}

/// Run the current test (`test_name`, as libtest names it: `module::fn`, without the crate)
/// inside a network sandbox, by re-executing this test binary with `--exact test_name`.
///
/// Returns `true` in the parent once the sandboxed child ran exactly that test and it passed:
/// the caller must then return without running the body again. Returns `false` when the body
/// should run in this process: either this process already is the sandboxed child, or the host
/// has no sandbox (a note goes to stderr; with [`REQUIRE_SANDBOX_ENV`]`=1` it panics instead).
/// Panics with the child's output if the child fails or ran no test.
///
/// Prefer [`isolate_network!`](crate::isolate_network), which derives `test_name`.
pub fn run_isolated(test_name: &str) -> bool {
    if active_sandbox().is_some() {
        return false;
    }
    let launcher = match launcher() {
        Ok(l) => l,
        Err(why) => {
            if std::env::var(REQUIRE_SANDBOX_ENV).is_ok_and(|v| v == "1") {
                panic!("{REQUIRE_SANDBOX_ENV}=1 but no network sandbox is available: {why}");
            }
            eprintln!(
                "I-6: no network sandbox on this host ({why}); `{test_name}` runs in-process \
                 with the best-effort socket detector only"
            );
            return false;
        }
    };
    let exe = std::env::current_exe().expect("current test executable");
    let out = launcher
        .command(&exe)
        .args([
            "--exact",
            test_name,
            "--nocapture",
            "--test-threads=1",
            "--include-ignored",
        ])
        .output()
        .expect("spawn the sandboxed test child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Re-emit through the test harness's capture, so the child's output shows like ours.
    print!("{stdout}");
    eprint!("{stderr}");
    assert!(
        out.status.success(),
        "`{test_name}` failed inside {} ({}); its output is above",
        launcher.label,
        out.status
    );
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the sandboxed child ran no test named `{test_name}` (check the name passed to \
         run_isolated); its output is above"
    );
    true
}

/// The libtest name of a test function from the `type_name` of a nested item:
/// `fx_net::module::my_test::__here` → `module::my_test`.
#[doc(hidden)]
pub fn test_name_from_type_name(type_name: &str) -> String {
    let parts: Vec<&str> = type_name.split("::").collect();
    match parts.as_slice() {
        [_krate, path @ .., _item] if !path.is_empty() => path.join("::"),
        _ => type_name.to_owned(),
    }
}

/// Opt the current `#[test]` into a network sandbox (see [`crate::net`]): use it as the first
/// statement of a test function returning `()`. In the parent it re-runs the test in a
/// sandboxed child and returns once that passed; in the child (or with no sandbox on this host)
/// it does nothing and the body runs here.
///
/// ```no_run
/// #[test]
/// fn my_fixture() {
///     scull_fixtures::isolate_network!();
///     // ... build, run, check_invariants: I-6 reports `blocked: ...` on Linux and macOS
/// }
/// ```
#[macro_export]
macro_rules! isolate_network {
    () => {{
        fn __scull_fixtures_here() {}
        let name = $crate::net::test_name_from_type_name(::std::any::type_name_of_val(
            &__scull_fixtures_here,
        ));
        if $crate::net::run_isolated(&name) {
            return;
        }
    }};
}

/// Result of the zero-traffic route probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// No route to either unroutable address (or no socket allowed): the block holds.
    Blocked(String),
    /// The kernel has a route to this address: the network is reachable.
    Reachable(&'static str),
}

/// Ask the kernel for a route to [`UNROUTABLE_V4`] and [`UNROUTABLE_V6`] with UDP `connect`,
/// which sends nothing.
fn probe_route() -> Route {
    let mut why = Vec::new();
    for (bind, target) in [("0.0.0.0:0", UNROUTABLE_V4), ("[::]:0", UNROUTABLE_V6)] {
        match UdpSocket::bind(bind).and_then(|s| s.connect(target)) {
            Ok(()) => return Route::Reachable(target),
            Err(e) => why.push(format!("{target}: {e}")),
        }
    }
    Route::Blocked(why.join("; "))
}

/// The detector's view: external sockets found and the method, or why it could not run.
pub(crate) type Detected = Result<(Vec<sockets::SocketEntry>, &'static str), String>;

/// The I-6 decision, separated from the OS so every branch is unit-tested.
pub(crate) fn evaluate(
    sandbox: Option<&str>,
    detected: Detected,
    route: impl FnOnce() -> Route,
) -> Outcome {
    if let Ok((found, how)) = &detected {
        let external: Vec<String> = found
            .iter()
            .filter(|s| s.is_external())
            .map(|s| format!("  {s}"))
            .collect();
        if !external.is_empty() {
            let place = sandbox.map_or_else(
                || "no network sandbox".to_owned(),
                |s| format!("inside {s}"),
            );
            return Outcome::Failed(format!(
                "the process holds non-loopback socket(s) ({place}; seen by {how}):\n{}",
                external.join("\n")
            ));
        }
    }
    match sandbox {
        Some(s) => match route() {
            Route::Reachable(target) => Outcome::Failed(format!(
                "{SANDBOX_ENV}={s} claims a network sandbox, but the kernel has a route to \
                 {target}: the network is not blocked"
            )),
            Route::Blocked(why) => {
                let detector = match &detected {
                    Ok((_, how)) => format!("{how} saw no non-loopback socket"),
                    Err(e) => format!("socket detector unavailable: {e}"),
                };
                Outcome::PassedWith(format!(
                    "blocked: {s}; verified, no route ({why}); {detector}"
                ))
            }
        },
        None => match detected {
            Ok((_, how)) => Outcome::PassedWith(format!(
                "best-effort: no network sandbox; {how} saw no non-loopback socket held by this \
                 process (sockets already closed or held by child processes are not seen)"
            )),
            Err(e) => Outcome::Unimplemented {
                tracking: Cow::Owned(format!(
                    "#151: no network sandbox and the socket detector failed: {e}"
                )),
            },
        },
    }
}

/// I-6 for the current process; see [`crate::invariants::network_blocked`].
pub(crate) fn check_current_process() -> Outcome {
    let _g = lock();
    let sandbox = active_sandbox();
    // Detector first: the probe opens (and closes) its own sockets.
    let detected = sockets::current_process();
    evaluate(sandbox.as_deref(), detected, probe_route)
}

#[cfg(test)]
mod tests {
    use super::sockets::SocketEntry;
    use super::*;

    fn sock(local: &str, remote: Option<&str>) -> SocketEntry {
        SocketEntry {
            proto: "udp".into(),
            local: local.into(),
            remote: remote.map(Into::into),
        }
    }

    fn unreachable() -> Route {
        panic!("the route probe must not run here")
    }

    #[test]
    fn verified_sandbox_passes_as_blocked() {
        let o = evaluate(Some("linux-netns"), Ok((vec![], "proc")), || {
            Route::Blocked("192.0.2.1:9: Network is unreachable".into())
        });
        match o {
            Outcome::PassedWith(n) => {
                assert!(n.starts_with("blocked: linux-netns"), "{n}");
                assert!(n.contains("Network is unreachable"), "{n}");
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn claimed_sandbox_with_a_route_fails() {
        let o = evaluate(Some("external"), Ok((vec![], "proc")), || {
            Route::Reachable(UNROUTABLE_V4)
        });
        assert!(
            matches!(&o, Outcome::Failed(m) if m.contains("not blocked") && m.contains("192.0.2.1")),
            "{o:?}"
        );
    }

    #[test]
    fn external_socket_fails_inside_and_outside_a_sandbox() {
        for sandbox in [None, Some("linux-netns")] {
            let found = vec![
                sock("127.0.0.1:5000", None),
                sock("10.0.0.2:40000", Some("192.0.2.1:9")),
            ];
            let o = evaluate(sandbox, Ok((found, "proc")), unreachable);
            match o {
                Outcome::Failed(m) => {
                    assert!(m.contains("192.0.2.1:9"), "{m}");
                    assert!(!m.contains("127.0.0.1:5000"), "loopback is not listed: {m}");
                }
                o => panic!("{o:?}"),
            }
        }
    }

    #[test]
    fn no_sandbox_and_no_sockets_is_only_best_effort() {
        let found = vec![sock("127.0.0.1:5000", Some("127.0.0.1:6000"))];
        let o = evaluate(None, Ok((found, "lsof -i")), unreachable);
        assert!(
            matches!(&o, Outcome::PassedWith(n) if n.starts_with("best-effort:") && n.contains("lsof -i")),
            "{o:?}"
        );
    }

    #[test]
    fn no_sandbox_and_no_detector_is_unimplemented_never_a_pass() {
        let o = evaluate(None, Err("lsof missing".into()), unreachable);
        assert!(!o.ran(), "{o:?}");
        assert!(
            matches!(&o, Outcome::Unimplemented { tracking } if tracking.contains("lsof missing")),
            "{o:?}"
        );
    }

    #[test]
    fn sandbox_without_detector_still_passes_on_the_verified_block() {
        let o = evaluate(
            Some("macos-sandbox-exec"),
            Err("lsof denied".into()),
            || Route::Blocked("Operation not permitted".into()),
        );
        assert!(
            matches!(&o, Outcome::PassedWith(n) if n.starts_with("blocked:") && n.contains("lsof denied")),
            "{o:?}"
        );
    }

    #[test]
    fn test_names_are_derived_from_type_names() {
        assert_eq!(
            test_name_from_type_name("network::normal_fixture::__scull_fixtures_here"),
            "normal_fixture"
        );
        assert_eq!(
            test_name_from_type_name("scull_fixtures::net::tests::a::__here"),
            "net::tests::a"
        );
        assert_eq!(test_name_from_type_name("odd"), "odd");
    }
}
