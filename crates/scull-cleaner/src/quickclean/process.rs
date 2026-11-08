//! Running one tool: a private sandbox for the sanitized environment, no shell, a timeout,
//! cancellation and capped output (SAFE-12). Modeled on `scull-git`'s `GitRunner::exec`.

use super::{OutputStream, output::OutputLines};
use scull_scanner::CancelToken;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Private, empty folders backing the sanitized environment and the neutral working
/// directory. Removed when dropped.
#[derive(Debug)]
pub(crate) struct Sandbox {
    _dir: tempfile::TempDir,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub tmp: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
}

impl Sandbox {
    /// A fresh sandbox inside `base` (the system temp folder when `None`).
    pub fn new(base: Option<&Path>) -> io::Result<Self> {
        let builder = {
            let mut b = tempfile::Builder::new();
            b.prefix("scull-qc-");
            b
        };
        let dir = match base {
            Some(b) => builder.tempdir_in(b)?,
            None => builder.tempdir()?,
        };
        let root = super::resolve::canonical(dir.path())
            .ok_or_else(|| io::Error::other("sandbox folder vanished"))?;
        let sub = |name: &str| -> io::Result<PathBuf> {
            let p = root.join(name);
            fs::create_dir(&p)?;
            Ok(p)
        };
        Ok(Self {
            cwd: sub("cwd")?,
            home: sub("home")?,
            tmp: sub("tmp")?,
            config: sub("config")?,
            cache: sub("cache")?,
            data: sub("data")?,
            state: sub("state")?,
            _dir: dir,
        })
    }

    /// Bun's package-manager commands require a project context even for global cache cleanup.
    pub fn bun_context(&self) -> io::Result<tempfile::TempDir> {
        let context = tempfile::Builder::new()
            .prefix("bun-")
            .tempdir_in(&self.cwd)?;
        let mut manifest = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(context.path().join("package.json"))?;
        manifest.write_all(b"{\"name\":\"scull-cache-cleanup\",\"private\":true}\n")?;
        Ok(context)
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The process exited; its exit code (`None` when killed by a signal).
    Exited(Option<i32>),
    /// It ran past the timeout and was killed.
    TimedOut(Duration),
    /// The operation was cancelled while it ran, and it was killed.
    Cancelled,
    /// It could not be started.
    SpawnFailed(String),
}

/// What one run produced.
#[derive(Debug, Clone)]
pub(crate) struct Ran {
    pub ended: Ended,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
    pub elapsed: Duration,
}

/// One process to start.
pub(crate) struct Spec<'a> {
    pub binary: &'a Path,
    pub args: &'a [OsString],
    /// The complete environment (nothing else is inherited).
    pub env: &'a [(String, OsString)],
    pub cwd: &'a Path,
    pub timeout: Duration,
    pub max_output: usize,
    pub cancel: &'a CancelToken,
}

/// Runs `spec`: absolute binary, cleared environment, null stdin, piped and capped output.
/// Kills the process on timeout or cancellation.
pub(crate) fn run(spec: &Spec<'_>) -> Ran {
    run_with_output(spec, &mut |_| {})
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OutputEvent {
    Line(OutputStream, String),
    Truncated(OutputStream),
}

// Live delivery has a separate line budget; final captured output keeps its byte limit.
const MAX_LIVE_LINES: usize = 256;

enum PipeEvent {
    Bytes(OutputStream, Vec<u8>),
    Truncated(OutputStream),
    Done(OutputStream),
}

/// Forwards escaped output on the calling thread while retaining the same capped capture.
pub(crate) fn run_with_output(spec: &Spec<'_>, output: &mut dyn FnMut(OutputEvent)) -> Ran {
    let started = Instant::now();
    let mut cmd = Command::new(spec.binary);
    cmd.env_clear()
        .current_dir(spec.cwd)
        .args(spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in spec.env {
        cmd.env(k, v);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ran {
                ended: Ended::SpawnFailed(format!("{}: {e}", spec.binary.display())),
                stdout: Vec::new(),
                stdout_truncated: false,
                stderr: Vec::new(),
                stderr_truncated: false,
                elapsed: started.elapsed(),
            };
        }
    };
    // Fixed-size chunks and a bounded queue keep fast writers from growing event memory.
    let (tx, rx) = mpsc::sync_channel::<PipeEvent>(8);
    for (stream, pipe) in [
        (
            OutputStream::Stdout,
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
        (
            OutputStream::Stderr,
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
    ] {
        if let Some(pipe) = pipe {
            let (tx, cap) = (tx.clone(), spec.max_output);
            thread::spawn(move || {
                let _ = read_capped(pipe, cap, |bytes, truncated| {
                    let event = if truncated {
                        PipeEvent::Truncated(stream)
                    } else {
                        PipeEvent::Bytes(stream, bytes.to_vec())
                    };
                    let _ = tx.send(event);
                });
                let _ = tx.send(PipeEvent::Done(stream));
            });
        }
    }
    drop(tx);
    let mut ran = Ran {
        ended: Ended::Exited(None),
        stdout: Vec::new(),
        stdout_truncated: false,
        stderr: Vec::new(),
        stderr_truncated: false,
        elapsed: Duration::ZERO,
    };
    let mut stdout_lines = OutputLines::default();
    let mut stderr_lines = OutputLines::default();
    let mut live_lines = [0; 2];
    let mut live_truncated = [false; 2];
    let mut emit = |event| {
        let stream = match &event {
            OutputEvent::Line(stream, _) | OutputEvent::Truncated(stream) => *stream,
        };
        let index = match stream {
            OutputStream::Stdout => 0,
            OutputStream::Stderr => 1,
        };
        if live_truncated[index] {
            return;
        }
        if matches!(event, OutputEvent::Line(_, _)) && live_lines[index] < MAX_LIVE_LINES {
            live_lines[index] += 1;
            output(event);
        } else {
            live_truncated[index] = true;
            output(OutputEvent::Truncated(stream));
        }
    };
    let ended = {
        let mut forward = |event| {
            let (stream, bytes, truncated) = match event {
                PipeEvent::Bytes(stream, bytes) => (stream, bytes, false),
                PipeEvent::Truncated(stream) => (stream, Vec::new(), true),
                PipeEvent::Done(stream) => {
                    let lines = match stream {
                        OutputStream::Stdout => &mut stdout_lines,
                        OutputStream::Stderr => &mut stderr_lines,
                    };
                    if let Some(line) = lines.finish() {
                        emit(OutputEvent::Line(stream, line));
                    }
                    return;
                }
            };
            let (capture, lines, over) = match stream {
                OutputStream::Stdout => (
                    &mut ran.stdout,
                    &mut stdout_lines,
                    &mut ran.stdout_truncated,
                ),
                OutputStream::Stderr => (
                    &mut ran.stderr,
                    &mut stderr_lines,
                    &mut ran.stderr_truncated,
                ),
            };
            capture.extend_from_slice(&bytes);
            for line in lines.push(&bytes) {
                emit(OutputEvent::Line(stream, line));
            }
            if truncated {
                *over = true;
                if let Some(line) = lines.finish() {
                    emit(OutputEvent::Line(stream, line));
                }
                emit(OutputEvent::Truncated(stream));
            }
        };

        let deadline = started + spec.timeout;
        let mut nap = Duration::from_millis(1);
        let ended = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ended::Exited(status.code()),
                Ok(None) => {}
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Ended::SpawnFailed(format!(
                        "waiting for {}: {e}",
                        spec.binary.display()
                    ));
                }
            }
            if spec.cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                break Ended::Cancelled;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break Ended::TimedOut(spec.timeout);
            }
            match rx.recv_timeout(nap) {
                Ok(event) => forward(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(nap),
            }
            nap = (nap * 2).min(Duration::from_millis(25));
        };

        // Descendants can retain a pipe after the child exits; never wait indefinitely for EOF.
        let grace = Instant::now() + Duration::from_secs(2);
        while let Some(left) = grace.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(event) => forward(event),
                Err(_) => break,
            }
        }
        ended
    };
    for (stream, lines) in [
        (OutputStream::Stdout, &mut stdout_lines),
        (OutputStream::Stderr, &mut stderr_lines),
    ] {
        if let Some(line) = lines.finish() {
            emit(OutputEvent::Line(stream, line));
        }
    }
    ran.ended = ended;
    ran.elapsed = started.elapsed();
    ran
}

/// Captures only `cap` bytes, then keeps draining so the child never blocks on its pipes.
fn read_capped(
    mut r: impl Read,
    cap: usize,
    mut output: impl FnMut(&[u8], bool),
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    let mut kept = 0;
    let mut truncated = false;
    loop {
        let n = match r.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let take = n.min(cap.saturating_sub(kept));
        if take > 0 {
            output(&buffer[..take], false);
            kept += take;
        }
        if take < n && !truncated {
            truncated = true;
            output(&[], true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn newline_flood_bounds_live_events_without_shortening_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("flood"), vec![b'\n'; 70_000]).expect("fixture output");
        let args = [
            OsString::from("-c"),
            OsString::from("/bin/cat flood; /bin/cat flood >&2"),
        ];
        for cap in [65_536, 100_000] {
            let mut events = Vec::new();
            let ran = run_with_output(
                &Spec {
                    binary: Path::new("/bin/sh"),
                    args: &args,
                    env: &[],
                    cwd: dir.path(),
                    timeout: Duration::from_secs(5),
                    max_output: cap,
                    cancel: &CancelToken::new(),
                },
                &mut |event| events.push(event),
            );
            assert_eq!(ran.ended, Ended::Exited(Some(0)));
            assert_eq!(ran.stdout.len(), cap.min(70_000));
            assert_eq!(ran.stderr.len(), cap.min(70_000));
            assert_eq!(ran.stdout_truncated, cap < 70_000);
            assert_eq!(ran.stderr_truncated, cap < 70_000);
            for stream in [OutputStream::Stdout, OutputStream::Stderr] {
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, OutputEvent::Line(s, _) if *s == stream))
                        .count(),
                    256
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| **event == OutputEvent::Truncated(stream))
                        .count(),
                    1
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn newline_flood_does_not_starve_cancellation_with_slow_callbacks() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("flood"), vec![b'\n'; 70_000]).expect("fixture output");
        let args = [
            OsString::from("-c"),
            OsString::from("/bin/cat flood; exec /bin/sleep 10"),
        ];
        let cancel = CancelToken::new();
        let mut callbacks = 0;
        let ran = run_with_output(
            &Spec {
                binary: Path::new("/bin/sh"),
                args: &args,
                env: &[],
                cwd: dir.path(),
                timeout: Duration::from_secs(5),
                max_output: 65_536,
                cancel: &cancel,
            },
            &mut |_| {
                callbacks += 1;
                cancel.cancel();
                thread::sleep(Duration::from_millis(1));
            },
        );
        assert_eq!(ran.ended, Ended::Cancelled);
        assert!(callbacks <= 257, "{callbacks} callbacks");
        assert!(ran.elapsed < Duration::from_secs(2), "{:?}", ran.elapsed);
    }

    #[cfg(unix)]
    #[test]
    fn output_caps_each_stream_and_keeps_draining() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = [
            OsString::from("-c"),
            OsString::from(
                "i=0; while [ $i -lt 20000 ]; do printf 'abcdefghij'; printf 'klmnopqrst' >&2; i=$((i+1)); done",
            ),
        ];
        let mut events = Vec::new();
        let ran = run_with_output(
            &Spec {
                binary: Path::new("/bin/sh"),
                args: &args,
                env: &[],
                cwd: dir.path(),
                timeout: Duration::from_secs(10),
                max_output: 64,
                cancel: &CancelToken::new(),
            },
            &mut |event| events.push(event),
        );
        assert_eq!(ran.ended, Ended::Exited(Some(0)));
        assert_eq!(ran.stdout.len(), 64);
        assert_eq!(ran.stderr.len(), 64);
        assert!(ran.stdout_truncated && ran.stderr_truncated);
        for stream in [OutputStream::Stdout, OutputStream::Stderr] {
            assert_eq!(
                events
                    .iter()
                    .filter(|e| **e == OutputEvent::Truncated(stream))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter_map(|e| match e {
                        OutputEvent::Line(s, line) if *s == stream => Some(line.len()),
                        _ => None,
                    })
                    .sum::<usize>(),
                64
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn pipe_eof_flushes_a_line_before_the_process_exits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = [
            OsString::from("-c"),
            OsString::from(
                "printf 'tail'; exec 1>&-; while [ ! -f release ]; do /bin/sleep 0.01; done",
            ),
        ];
        let ran = run_with_output(
            &Spec {
                binary: Path::new("/bin/sh"),
                args: &args,
                env: &[],
                cwd: dir.path(),
                timeout: Duration::from_secs(3),
                max_output: 64,
                cancel: &CancelToken::new(),
            },
            &mut |event| {
                if matches!(event, OutputEvent::Line(OutputStream::Stdout, ref line) if line == "tail")
                {
                    fs::write(dir.path().join("release"), b"go").expect("release fixture");
                }
            },
        );
        assert_eq!(ran.ended, Ended::Exited(Some(0)));
    }

    #[cfg(unix)]
    #[test]
    fn inherited_pipe_does_not_stall_return_or_lose_the_partial_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = [
            OsString::from("-c"),
            OsString::from("printf 'tail'; /bin/sleep 4 &"),
        ];
        let mut events = Vec::new();
        let ran = run_with_output(
            &Spec {
                binary: Path::new("/bin/sh"),
                args: &args,
                env: &[],
                cwd: dir.path(),
                timeout: Duration::from_secs(1),
                max_output: 64,
                cancel: &CancelToken::new(),
            },
            &mut |event| events.push(event),
        );
        assert_eq!(ran.ended, Ended::Exited(Some(0)));
        assert!(ran.elapsed < Duration::from_secs(3), "{:?}", ran.elapsed);
        assert_eq!(ran.stdout, b"tail");
        assert_eq!(
            events,
            [OutputEvent::Line(OutputStream::Stdout, "tail".into())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_from_live_output_kills_the_process() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = [
            OsString::from("-c"),
            OsString::from("printf 'ready\\n'; exec /bin/sleep 10"),
        ];
        let cancel = CancelToken::new();
        let ran = run_with_output(
            &Spec {
                binary: Path::new("/bin/sh"),
                args: &args,
                env: &[],
                cwd: dir.path(),
                timeout: Duration::from_secs(5),
                max_output: 64,
                cancel: &cancel,
            },
            &mut |_| cancel.cancel(),
        );
        assert_eq!(ran.ended, Ended::Cancelled);
        assert!(ran.elapsed < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn output_arrives_before_exit_and_flushes_the_final_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let release = dir.path().join("release");
        let args = [
            OsString::from("-c"),
            OsString::from(
                "printf 'ready\\n'; while [ ! -f release ]; do /bin/sleep 0.01; done; printf 'tail'; printf 'error\\n' >&2",
            ),
        ];
        let mut events = Vec::new();
        let ran = run_with_output(
            &Spec {
                binary: Path::new("/bin/sh"),
                args: &args,
                env: &[],
                cwd: dir.path(),
                timeout: Duration::from_secs(3),
                max_output: 1024,
                cancel: &CancelToken::new(),
            },
            &mut |event| {
                if matches!(&event, OutputEvent::Line(super::super::OutputStream::Stdout, line) if line == "ready")
                {
                    fs::write(&release, b"go").expect("release fixture");
                }
                events.push(event);
            },
        );
        assert_eq!(ran.ended, Ended::Exited(Some(0)));
        assert_eq!(ran.stdout, b"ready\ntail");
        assert!(events.contains(&OutputEvent::Line(
            super::super::OutputStream::Stdout,
            "tail".into()
        )));
        assert!(events.contains(&OutputEvent::Line(
            super::super::OutputStream::Stderr,
            "error".into()
        )));
    }

    #[test]
    fn read_capped_reports_overflow_and_drains() {
        let data = [b'x'; 100];
        let capture = |cap| {
            let mut bytes = Vec::new();
            let mut truncated = false;
            read_capped(&data[..], cap, |b, over| {
                bytes.extend_from_slice(b);
                truncated |= over;
            })
            .expect("read");
            (bytes, truncated)
        };
        let (b, over) = capture(10);
        assert_eq!(b.len(), 10);
        assert!(over);
        let (b, over) = capture(100);
        assert_eq!(b.len(), 100);
        assert!(!over);
    }

    #[test]
    fn sandbox_folders_exist_and_are_removed() {
        let base = tempfile::tempdir().expect("tempdir");
        let root;
        {
            let sb = Sandbox::new(Some(base.path())).expect("sandbox");
            for d in [
                &sb.cwd, &sb.home, &sb.tmp, &sb.config, &sb.cache, &sb.data, &sb.state,
            ] {
                assert!(d.is_dir(), "{}", d.display());
                assert_eq!(fs::read_dir(d).expect("read").count(), 0);
            }
            root = sb.cwd.parent().expect("parent").to_path_buf();
        }
        assert!(!root.exists());
    }

    #[test]
    fn bun_context_is_private_minimal_and_removed_after_use() {
        let base = tempfile::tempdir().expect("tempdir");
        let sandbox = Sandbox::new(Some(base.path())).expect("sandbox");
        let path;
        {
            let context = sandbox.bun_context().expect("Bun context");
            path = context.path().to_path_buf();
            assert!(path.starts_with(&sandbox.cwd));
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(path.join("package.json")).expect("manifest"))
                    .expect("JSON");
            assert_eq!(
                manifest,
                serde_json::json!({"name": "scull-cache-cleanup", "private": true})
            );
            assert_eq!(fs::read_dir(&path).expect("read").count(), 1);
            assert!(!sandbox.cwd.join("package.json").exists());
        }
        assert!(!path.exists());
    }

    #[test]
    #[ignore = "requires a real Bun binary via SCULL_TEST_BUN_BINARY; uses only a temporary cache"]
    fn real_bun_cleans_only_the_temporary_cache_from_its_private_context() {
        let binary = fs::canonicalize(
            std::env::var_os("SCULL_TEST_BUN_BINARY").expect("SCULL_TEST_BUN_BINARY"),
        )
        .expect("Bun binary");
        let base = tempfile::tempdir().expect("tempdir");
        let sandbox = Sandbox::new(Some(base.path())).expect("sandbox");
        let context = sandbox.bun_context().expect("Bun context");
        let cache = sandbox.cache.join("bun");
        fs::create_dir(&cache).expect("cache");
        let canary = base.path().join("keep");
        fs::write(&canary, b"outside the selected cache").expect("canary");
        let env = vec![
            ("HOME".into(), sandbox.home.clone().into_os_string()),
            ("USERPROFILE".into(), sandbox.home.clone().into_os_string()),
            (
                "BUN_INSTALL_CACHE_DIR".into(),
                cache.clone().into_os_string(),
            ),
            ("TMPDIR".into(), sandbox.tmp.clone().into_os_string()),
            ("NO_COLOR".into(), OsString::from("1")),
        ];
        let cancel = CancelToken::new();
        let query = [OsString::from("pm"), OsString::from("cache")];
        let mut spec = Spec {
            binary: &binary,
            args: &query,
            env: &env,
            cwd: context.path(),
            timeout: Duration::from_secs(10),
            max_output: 4096,
            cancel: &cancel,
        };
        let located = run(&spec);
        assert_eq!(located.ended, Ended::Exited(Some(0)), "{located:?}");
        let path = String::from_utf8(located.stdout).expect("cache path");
        assert_eq!(
            fs::canonicalize(path.trim()).expect("resolved cache"),
            fs::canonicalize(&cache).expect("private cache")
        );
        let fixture = cache.join("fixture");
        fs::write(&fixture, b"temporary package").expect("fixture");
        let remove = [
            OsString::from("pm"),
            OsString::from("cache"),
            OsString::from("rm"),
        ];
        spec.args = &remove;
        let cleaned = run(&spec);
        assert_eq!(cleaned.ended, Ended::Exited(Some(0)), "{cleaned:?}");
        assert!(!fixture.exists());
        assert_eq!(
            fs::read(&canary).expect("canary"),
            b"outside the selected cache"
        );
    }

    #[test]
    fn a_missing_binary_is_a_spawn_failure() {
        let base = tempfile::tempdir().expect("tempdir");
        let missing = base.path().join("no-such-tool");
        let ran = run(&Spec {
            binary: &missing,
            args: &[],
            env: &[],
            cwd: base.path(),
            timeout: Duration::from_secs(5),
            max_output: 1024,
            cancel: &CancelToken::new(),
        });
        assert!(
            matches!(ran.ended, Ended::SpawnFailed(_)),
            "{:?}",
            ran.ended
        );
    }
}
