//! The single execution lock per user (P2-EXEC-04, SAFE-09): a second executor fails with exit
//! code 6 (or waits when asked), across threads and across processes, and the OS releases the
//! lock when its holder dies. Plans touch nothing while another executor holds the lock.
//!
//! Fixture IDs (docs/FIXTURES.md): FX-RACE-05.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/planning.rs"]
mod planning;

use planning::*;
use scull_cleaner::{ExecutionLock, Executor, LockOptions};
use scull_core::ErrorKind;
use scull_engine::CancelToken;
use scull_fixtures::{Fixture, InvariantInputs};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

/// Environment variable telling the helper test which lock to hold.
const HOLD_ENV: &str = "SCULL_TEST_HOLD_EXEC_LOCK";

fn lock_path(fx: &Fixture) -> PathBuf {
    fx.base().join("state").join("execution.lock")
}

/// Not a test on its own: the child process of the cross-process fixtures. Takes the lock,
/// says so on stdout, and holds it until killed (or stdin closes).
#[test]
#[ignore = "helper process for fx_race_05_* (run by them, not directly)"]
fn lock_holder_child() {
    let Some(path) = std::env::var_os(HOLD_ENV) else {
        return;
    };
    let exec = Executor::acquire(&LockOptions::at(PathBuf::from(path))).unwrap();
    assert!(exec.lock().is_some());
    println!("LOCKED");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    drop(exec);
}

/// Another process holding the lock at `path`; returns once it has it.
fn spawn_holder(path: &Path) -> Child {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lock_holder_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLD_ENV, path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let mut lines = BufReader::new(out).lines();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "the holder never took the lock");
        match lines.next() {
            Some(Ok(l)) if l.contains("LOCKED") => break,
            Some(_) => {}
            None => panic!("the holder exited before taking the lock"),
        }
    }
    child
}

/// FX-RACE-05: two executors start at once → exactly one gets the lock; the other fails with
/// `LockHeld`, exit code 6. Nothing overlaps: the winner runs its plan, the loser touches
/// nothing.
#[test]
fn fx_race_05_two_executors_start_at_once_second_exits_6() {
    let fx = two_cargo_projects();
    let path = lock_path(&fx);
    let barrier = Arc::new(Barrier::new(2));
    let results: Vec<_> = (0..2)
        .map(|_| {
            let (b, p) = (Arc::clone(&barrier), path.clone());
            std::thread::spawn(move || {
                b.wait();
                Executor::acquire(&LockOptions::at(p))
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let (winners, losers): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
    assert_eq!(winners.len(), 1, "exactly one executor holds the lock");
    let loser = losers.into_iter().next().unwrap().unwrap_err();
    println!("second executor: [{}] {}", loser.kind, loser.message);
    assert_eq!(loser.kind, ErrorKind::LockHeld);
    assert_eq!(loser.exit_code().as_i32(), 6);

    // The winner executes; a third executor arriving meanwhile is refused too.
    let winner = winners.into_iter().next().unwrap().unwrap();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);
    assert_eq!(
        Executor::acquire(&LockOptions::at(&path)).unwrap_err().kind,
        ErrorKind::LockHeld
    );
    let r = winner
        .execute_plan(&plan, &ctx(), &h.revalidation(&fx), &CancelToken::new())
        .unwrap();
    print_report(&fx, &r);
    assert_executed(&fx, &r, "app/target");
    assert_executed(&fx, &r, "lib/target");
    drop(winner);
    assert!(Executor::acquire(&LockOptions::at(&path)).is_ok());
    check(&fx, &plan, &r, &["app/target", "lib/target"]);
}

/// FX-RACE-05 across processes: while another scull process executes, this one exits 6 and
/// touches nothing; when that process dies (killed, no cleanup), the OS releases the lock and
/// this one proceeds. No PID file is involved.
#[test]
fn fx_race_05_other_process_holds_lock_exit_6_then_released_on_death() {
    let fx = two_cargo_projects();
    let path = lock_path(&fx);
    let mut holder = spawn_holder(&path);

    let e = Executor::acquire(&LockOptions::at(&path)).unwrap_err();
    println!("while another process holds it: [{}] {}", e.kind, e.message);
    assert_eq!(e.kind, ErrorKind::LockHeld);
    assert_eq!(e.exit_code().as_i32(), 6);
    let waited = Instant::now();
    let e =
        Executor::acquire(&LockOptions::at(&path).wait(Duration::from_millis(200))).unwrap_err();
    assert_eq!(e.kind, ErrorKind::LockHeld);
    assert!(waited.elapsed() >= Duration::from_millis(200));
    fx.check_invariants(&InvariantInputs::new()).assert_ok();

    // The holder is killed mid-operation: no unlock, no cleanup.
    holder.kill().unwrap();
    holder.wait().unwrap();
    let exec = Executor::acquire(&LockOptions::at(&path).wait(Duration::from_secs(10))).unwrap();
    println!(
        "after the holder died: acquired {}",
        exec.lock().unwrap().path().display()
    );
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);
    let r = exec
        .execute_plan(&plan, &ctx(), &h.revalidation(&fx), &CancelToken::new())
        .unwrap();
    assert_executed(&fx, &r, "app/target");
    check(&fx, &plan, &r, &["app/target"]);
}

/// A waiting executor gets the lock as soon as the holder finishes.
#[test]
fn waiting_executor_proceeds_when_the_holder_finishes() {
    let fx = two_cargo_projects();
    let path = lock_path(&fx);
    let mut holder = spawn_holder(&path);
    let p = path.clone();
    let waiter = std::thread::spawn(move || {
        ExecutionLock::acquire(&LockOptions::at(p).wait(Duration::from_secs(30)))
    });
    std::thread::sleep(Duration::from_millis(200));
    // Closing stdin lets the holder finish normally.
    drop(holder.stdin.take());
    holder.wait().unwrap();
    assert!(waiter.join().unwrap().is_ok());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Plans never run without the lock.
#[test]
fn execute_plan_refuses_without_the_lock() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);
    let e = Executor::new()
        .execute_plan(&plan, &ctx(), &h.revalidation(&fx), &CancelToken::new())
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
