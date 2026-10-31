//! Invariant I-6, "no network access unless the fixture is tagged `net`" (docs/FIXTURES.md §1,
//! P1-FX-05): a normal fixture passes it inside a real network sandbox, and deliberate network
//! attempts fail it or are refused by the sandbox.
//!
//! This is its own test binary because the I-6 socket detector sees the whole process: the
//! deliberate violations below would otherwise be attributed to fixtures checked concurrently
//! in other files. Inside this file the tests are serialized for the same reason.
//!
//! No test sends a packet outside a sandbox: a UDP `connect` only records the peer, and the TCP
//! attempt runs only where a verified sandbox is in effect.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Pipeline, run_checked};
use scull_fixtures::net::{self, UNROUTABLE_V4};
use scull_fixtures::{Fixture, InvariantId, InvariantInputs, NodeLock, Outcome, days_ago};
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 16 * 1024)
        .unwrap()
        .age("app/target", days_ago(3))
        .unwrap()
        .npm_project("web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("web", &["left-pad"])
        .unwrap()
        .checkpoint()
        .unwrap();
    fx
}

/// An error that proves the network was blocked, not merely slow.
fn is_block_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::NetworkUnreachable
            | ErrorKind::HostUnreachable
            | ErrorKind::PermissionDenied
            | ErrorKind::AddrNotAvailable
    )
}

/// Bind a UDP socket and point it at TEST-NET-1. Sends nothing. Inside a sandbox the bind or
/// the connect may be refused; that refusal is returned.
fn deliberate_udp_socket() -> Result<UdpSocket, std::io::Error> {
    let s = UdpSocket::bind("0.0.0.0:0")?;
    // Without a route (sandbox, offline host) the connect fails but the socket stays bound to
    // all interfaces, which the detector must still flag.
    let _ = s.connect(UNROUTABLE_V4);
    Ok(s)
}

/// A normal read-only fixture, run through the real pipeline, passes I-6. On Linux (user
/// namespaces available) and macOS it runs in a sandboxed child and I-6 says `blocked: ...`;
/// elsewhere (Windows) it is `best-effort: ...`.
#[test]
fn normal_fixture_passes_i6() {
    let _g = serial();
    scull_fixtures::isolate_network!();
    let fx = fixture();
    run_checked(&fx, &Pipeline::new(fx.root()));
    let report = fx.check_invariants(&InvariantInputs::new());
    println!("{report}");
    report.assert_ok();
    let Outcome::PassedWith(note) = report.outcome(InvariantId::I6) else {
        panic!("I-6 did not run:\n{report}");
    };
    match net::active_sandbox() {
        Some(s) => assert!(note.starts_with(&format!("blocked: {s}")), "{note}"),
        None => {
            assert!(note.starts_with("best-effort:"), "{note}");
            assert!(
                net::sandbox_support().is_err(),
                "a sandbox is available but the test ran outside it"
            );
        }
    }
}

/// A fixture whose process holds a socket aimed at the outside fails I-6, with or without a
/// sandbox, and passes again once the socket is closed.
#[test]
fn deliberate_socket_fails_i6() {
    let _g = serial();
    let fx = fixture();
    let sock = match deliberate_udp_socket() {
        Ok(s) => s,
        Err(e) => {
            // Only a sandbox may refuse the socket itself (macOS `deny network*`).
            let sandbox = net::active_sandbox().expect("bind refused outside a sandbox");
            assert!(is_block_error(&e), "{e}");
            println!("{sandbox} refused the socket: {e}");
            fx.check_invariants(&InvariantInputs::new()).assert_ok();
            return;
        }
    };
    let port = sock.local_addr().unwrap().port();
    let report = fx.check_invariants(&InvariantInputs::new());
    println!("{report}");
    let failures = report.failures();
    assert_eq!(
        failures.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        vec![InvariantId::I6],
        "{report}"
    );
    assert!(failures[0].1.contains(&format!(":{port}")), "{report}");

    drop(sock);
    let report = fx.check_invariants(&InvariantInputs::new());
    report.assert_ok();
    assert!(report.ran().contains(&InvariantId::I6), "{report}");
}

/// Inside the sandbox, a real TCP connection attempt to TEST-NET-1 is refused at once with a
/// "network unreachable" / "operation not permitted" error, and I-6 still fails for the socket
/// left behind. Without a sandbox (Windows) the attempt would put packets on the wire, so it is
/// not made.
#[test]
fn deliberate_connect_is_blocked_in_sandbox() {
    let _g = serial();
    scull_fixtures::isolate_network!();
    let Some(sandbox) = net::active_sandbox() else {
        println!(
            "skipped: no network sandbox on this host ({}); no packet sent",
            net::sandbox_support().err().unwrap_or("?")
        );
        return;
    };
    let fx = fixture();
    let addr: SocketAddr = UNROUTABLE_V4.parse().unwrap();
    let err = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .expect_err("a TCP connection got out of the sandbox");
    assert!(is_block_error(&err), "{sandbox}: unexpected error {err:?}");
    println!("{sandbox}: TCP connect to {UNROUTABLE_V4} refused: {err}");

    match deliberate_udp_socket() {
        Ok(sock) => {
            let report = fx.check_invariants(&InvariantInputs::new());
            println!("{report}");
            assert!(
                report.outcome(InvariantId::I6).is_failure(),
                "a socket opened inside {sandbox} must fail I-6:\n{report}"
            );
            drop(sock);
        }
        Err(e) => {
            assert!(is_block_error(&e), "{e}");
            println!("{sandbox}: UDP socket refused: {e}");
        }
    }
    let report = fx.check_invariants(&InvariantInputs::new());
    report.assert_ok();
}

/// A fixture tagged `net` is exempt from I-6, even while it holds a socket.
#[test]
fn net_tagged_fixture_is_exempt() {
    let _g = serial();
    let fx = fixture();
    let sock = deliberate_udp_socket().ok();
    let report = fx.check_invariants(&InvariantInputs::new().tagged_net());
    report.assert_ok();
    assert!(
        matches!(report.outcome(InvariantId::I6), Outcome::NotApplicable(r) if r.contains("net")),
        "{report}"
    );
    drop(sock);
}

/// Reports which network sandbox this host offers (shown in the test output).
#[test]
fn reports_sandbox_support() {
    match net::sandbox_support() {
        Ok(label) => println!("I-6 sandbox: {label}"),
        Err(why) => println!("I-6 sandbox unavailable: {why}"),
    }
    if cfg!(windows) {
        assert!(
            net::sandbox_support().is_err(),
            "no sandbox is claimed on Windows"
        );
    }
}
