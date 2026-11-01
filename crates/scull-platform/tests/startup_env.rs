//! `remove_vars_before_threads` removes exactly the named variables. It must run before any
//! other thread exists, so the check runs in a child process of this test binary, on its main
//! thread (`--test-threads=1`), with the variables set only there.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

const CHILD: &str = "SCULL_STARTUP_ENV_CHILD";

#[test]
fn removes_the_named_variables_and_nothing_else() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--test-threads=1", "--nocapture"])
        .env(CHILD, "1")
        .env("SCULL_TEST_INJECT_A", "--remote-debugging-port=9222")
        .env("SCULL_TEST_INJECT_B", "127.0.0.1:9222")
        .env("SCULL_TEST_KEEP", "kept")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("removed=[\"SCULL_TEST_INJECT_A\", \"SCULL_TEST_INJECT_B\"]"),
        "{stdout}"
    );
    assert!(
        stdout.contains("after: A=None B=None keep=Some(\"kept\")"),
        "{stdout}"
    );
}

/// Runs only in the child process started above.
#[test]
fn child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let removed = scull_platform::startup_env::remove_vars_before_threads(&[
        "SCULL_TEST_INJECT_A",
        "SCULL_TEST_INJECT_B",
        "SCULL_TEST_NOT_SET",
    ]);
    println!("removed={removed:?}");
    println!(
        "after: A={:?} B={:?} keep={:?}",
        std::env::var("SCULL_TEST_INJECT_A").ok(),
        std::env::var("SCULL_TEST_INJECT_B").ok(),
        std::env::var("SCULL_TEST_KEEP").ok()
    );
}
