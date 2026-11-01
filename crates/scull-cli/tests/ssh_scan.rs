//! `scull scan --ssh HOST:PATH` (P6-RES-01 preview) against a fake `ssh` shim: the shim records
//! its arguments and runs the remote command with `/bin/sh -c`, as a remote POSIX shell would,
//! using this build's `scull` as the "remote" binary over a generated fixture. No real host is
//! ever contacted. Unix only (the shim is a shell script).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_fixtures::{Fixture, InvariantInputs};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The normal shim: log the arguments, then run the last one (the remote command) like sshd's
/// shell would, in the fake remote home. Test-only tweak: the remote scan also gets
/// `--allow-network-roots`, because temp dirs are tmpfs on some machines.
const RUN_REMOTE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$SHIM_LOG"
for last; do :; done
last=$(printf '%s' "$last" | sed 's/ -- / --allow-network-roots -- /')
cd "$FAKE_REMOTE_HOME" || exit 1
HOME="$FAKE_REMOTE_HOME" PATH="$FAKE_REMOTE_PATH" exec /bin/sh -c "$last"
"#;

struct Env {
    fx: Fixture,
    tmp: tempfile::TempDir,
}

impl Env {
    fn new(project_dir: &str) -> Self {
        let mut fx = Fixture::new().unwrap();
        fx.cargo_project(project_dir, "app")
            .unwrap()
            .cargo_target(project_dir, "app", 8192)
            .unwrap();
        fx.checkpoint().unwrap();
        Self {
            fx,
            tmp: tempfile::tempdir().unwrap(),
        }
    }

    fn shim(&self, body: &str) -> PathBuf {
        let dir = self.tmp.path().join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ssh");
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.tmp.path().join("ssh.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn scull_home(&self) -> PathBuf {
        self.tmp.path().join("scull-home")
    }

    fn run(&self, shim_dir: Option<&Path>, args: &[&str]) -> Output {
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_scull"));
        // Without a shim, PATH is one empty folder: the real ssh must never be found.
        let path = match shim_dir {
            Some(d) => vec![
                d.to_path_buf(),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
            ],
            None => {
                let empty = self.tmp.path().join("empty-bin");
                std::fs::create_dir_all(&empty).unwrap();
                vec![empty]
            }
        };
        let remote_path = format!("{}:/usr/bin:/bin", bin.parent().unwrap().display());
        let mut c = Command::new(&bin);
        c.args(args)
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("SHIM_LOG", self.tmp.path().join("ssh.log"))
            .env("FAKE_REMOTE_HOME", self.fx.base())
            .env("FAKE_REMOTE_PATH", remote_path)
            .env("SCULL_HOME", self.scull_home())
            .env("HOME", self.tmp.path())
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        for (k, _) in std::env::vars() {
            if k.starts_with("SCULL_") && k != "SCULL_HOME" {
                c.env_remove(k);
            }
        }
        c.output().unwrap()
    }

    /// `~/<root>/<rel>` as the remote shell sees it.
    fn remote(&self, rel: &str) -> String {
        let root = self.fx.root().strip_prefix(self.fx.base()).unwrap();
        format!("~/{}/{rel}", root.display())
            .trim_end_matches('/')
            .to_owned()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn scans_over_the_shim_with_a_locked_down_client() {
    let env = Env::new("app");
    let shim = env.shim(RUN_REMOTE);
    let target = format!("build-box:{}", env.remote(""));
    let out = env.run(Some(&shim), &["scan", "--ssh", &target]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("Remote scan of build-box:/"), "{stdout}");
    assert!(stdout.contains("target"), "{stdout}");
    assert!(stdout.contains("Totals by tier:"));
    assert!(stdout.contains("nothing was changed or recorded"));

    let log = env.log();
    for o in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "UpdateHostKeys=no",
        "ForwardAgent=no",
        "ForwardX11=no",
        "ClearAllForwardings=yes",
        "PermitLocalCommand=no",
        "RequestTTY=no",
    ] {
        assert!(
            log.windows(2).any(|w| w[0] == "-o" && w[1] == o),
            "{o}: {log:?}"
        );
    }
    assert_eq!(&log[..3], ["-T", "-e", "none"]);
    let n = log.len();
    assert_eq!(log[n - 3], "--");
    assert_eq!(log[n - 2], "build-box");
    assert!(
        log[n - 1].starts_with("'scull' 'scan' '--json' '--no-history' -- ~/"),
        "{}",
        log[n - 1]
    );
    // Nothing recorded locally, nothing changed in the scanned tree.
    assert!(!env.scull_home().join("data").exists());
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn json_mode_returns_the_validated_remote_document_and_quoting_survives_the_remote_shell() {
    let env = Env::new("it's a dir; echo pwned");
    let shim = env.shim(RUN_REMOTE);
    let target = format!("me@host:{}", env.remote("it's a dir; echo pwned"));
    let out = env.run(Some(&shim), &["scan", "--json", "--ssh", &target]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["schema"], "scull/scan@1");
    let roots = doc["data"]["roots"].as_array().unwrap();
    assert!(
        roots[0]
            .as_str()
            .unwrap()
            .ends_with("it's a dir; echo pwned"),
        "{roots:?}"
    );
    assert!(!text(&out.stdout).contains("pwned\n"));
    assert!(
        doc["data"]["candidates"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
    );
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn ssh_failures_are_reported_escaped() {
    let env = Env::new("app");
    let shim =
        env.shim("#!/bin/sh\nprintf 'Host key verification failed.\\033[2J\\n' >&2\nexit 255\n");
    let out = env.run(Some(&shim), &["scan", "--ssh", "newhost:/srv"]);
    assert_eq!(out.status.code(), Some(1));
    let err = text(&out.stderr);
    assert!(err.contains("unknown host key"), "{err}");
    assert!(err.contains("Host key verification failed."), "{err}");
    assert!(!err.contains('\u{1b}'), "escaped: {err:?}");

    let shim = env.shim("#!/bin/sh\necho 'sh: scull: not found' >&2\nexit 127\n");
    let out = env.run(Some(&shim), &["scan", "--ssh", "h:/srv"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--remote-bin"),
        "{}",
        text(&out.stderr)
    );
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn untrusted_remote_output_is_refused() {
    let env = Env::new("app");
    for (body, why) in [
        ("echo 'not json'", "not JSON"),
        (
            r#"echo '{"schema":"scull/scan@1","version":"0","data":{"evil":"\u001b[2J"}}'"#,
            "valid scull/scan@1",
        ),
        (
            r#"echo '{"schema":"scull/scan@9","version":"0","data":{}}'"#,
            "valid scull/scan@1",
        ),
    ] {
        let shim = env.shim(&format!("#!/bin/sh\n{body}\n"));
        let out = env.run(Some(&shim), &["scan", "--ssh", "h:/srv"]);
        assert_eq!(out.status.code(), Some(1), "{body}");
        let err = text(&out.stderr);
        assert!(err.contains(why), "{body}: {err}");
        assert!(!err.contains('\u{1b}'));
        assert!(out.stdout.is_empty());
    }
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn usage_errors_and_missing_ssh() {
    let env = Env::new("app");
    let shim = env.shim(RUN_REMOTE);
    for args in [
        &["scan", "--ssh", "-oProxyCommand=x:/srv"][..],
        &["scan", "--ssh", "h:relative"],
        &["scan", "--ndjson", "--ssh", "h:/srv"],
        &["scan", "--ssh", "h:/srv", "/local/path"],
        &["scan", "--remote-bin", "/x"],
    ] {
        let out = env.run(Some(&shim), args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
    assert!(env.log().is_empty(), "ssh never ran");
    // No ssh on PATH at all.
    let out = env.run(None, &["scan", "--ssh", "h:/srv"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("no trusted ssh client"),
        "{}",
        text(&out.stderr)
    );
    env.fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
