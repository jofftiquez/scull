//! Running one tool: a private sandbox for the sanitized environment, no shell, a timeout,
//! cancellation and capped output (SAFE-12). Modeled on `scull-git`'s `GitRunner::exec`.

use scull_scanner::CancelToken;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
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
    let (tx, rx) = mpsc::channel::<(bool, io::Result<(Vec<u8>, bool)>)>();
    for (is_out, stream) in [
        (
            true,
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
        (
            false,
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        ),
    ] {
        if let Some(s) = stream {
            let (tx, cap) = (tx.clone(), spec.max_output);
            thread::spawn(move || {
                let _ = tx.send((is_out, read_capped(s, cap)));
            });
        }
    }
    drop(tx);

    let deadline = started + spec.timeout;
    let mut nap = Duration::from_millis(1);
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ended::Exited(status.code()),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Ended::SpawnFailed(format!("waiting for {}: {e}", spec.binary.display()));
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
        thread::sleep(nap);
        nap = (nap * 2).min(Duration::from_millis(25));
    };

    // Collect both streams. A grandchild still holding a pipe can't stall us for more than a
    // short grace period.
    let mut ran = Ran {
        ended,
        stdout: Vec::new(),
        stdout_truncated: false,
        stderr: Vec::new(),
        stderr_truncated: false,
        elapsed: Duration::ZERO,
    };
    for _ in 0..2 {
        let Ok((is_out, res)) = rx.recv_timeout(Duration::from_secs(2)) else {
            break;
        };
        let (bytes, truncated) = res.unwrap_or_default();
        if is_out {
            ran.stdout = bytes;
            ran.stdout_truncated = truncated;
        } else {
            ran.stderr = bytes;
            ran.stderr_truncated = truncated;
        }
    }
    ran.elapsed = started.elapsed();
    ran
}

/// Reads up to `cap` bytes, then drains the rest so the child never blocks on a full pipe.
/// Returns the bytes and whether the cap was exceeded.
fn read_capped(mut r: impl Read, cap: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    (&mut r).take(cap as u64).read_to_end(&mut buf)?;
    let rest = io::copy(&mut r, &mut io::sink())?;
    Ok((buf, rest > 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_capped_reports_overflow_and_drains() {
        let data = [b'x'; 100];
        let (b, over) = read_capped(&data[..], 10).expect("read");
        assert_eq!(b.len(), 10);
        assert!(over);
        let (b, over) = read_capped(&data[..], 100).expect("read");
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
