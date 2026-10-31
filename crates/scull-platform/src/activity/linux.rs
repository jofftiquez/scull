//! Linux: `/proc/<pid>/cwd` and `/proc/<pid>/fd/*` symlinks, read with `readlink` only.
//!
//! Reading those links needs ptrace access to the process (same user and dumpable, or
//! `CAP_SYS_PTRACE`); processes we may not inspect are counted by owner (the uid of
//! `/proc/<pid>`, readable for every visible process). With `hidepid=` mounts, other users'
//! processes are invisible and cannot be counted at all.

use super::{Collected, Limits, Proc, is_under_exact};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

pub(super) fn collect(limits: &Limits, deadline: Instant) -> Collected {
    let mut out = Collected::default();
    let my_uid = fs::metadata("/proc/self").map(|m| m.uid()).ok();
    let entries = match fs::read_dir("/proc") {
        Ok(rd) => rd,
        Err(e) => {
            let reason = format!("/proc is not readable: {e}");
            out.cwd_unsupported = Some(reason.clone());
            out.files_unsupported = Some(reason);
            return out;
        }
    };
    let mut open_files = 0usize;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if out.procs.len() >= limits.max_processes || Instant::now() >= deadline {
            out.truncated = true;
            break;
        }
        let base = entry.path();
        let owner = match fs::symlink_metadata(&base) {
            Ok(md) => md.uid(),
            Err(_) => continue, // exited
        };
        // Unknown own uid: count every denial as our own (protective).
        let own = my_uid.is_none_or(|me| me == owner);
        let mut proc = Proc {
            pid,
            ..Proc::default()
        };

        match fs::read_link(base.join("cwd")) {
            Ok(cwd) => proc.cwd = Some(cwd),
            Err(e) if gone(&e) => continue,
            Err(_) => out.cwd_gaps.deny(own),
        }

        match fs::read_dir(base.join("fd")) {
            Ok(fds) => {
                for fd in fds {
                    let Ok(fd) = fd else { continue };
                    if open_files >= limits.max_open_files {
                        out.truncated = true;
                        break;
                    }
                    // `readlink` only: the target is never opened. Sockets, pipes and anonymous
                    // inodes read as `socket:[…]` etc., which are not absolute paths.
                    if let Ok(target) = fs::read_link(fd.path())
                        && target.is_absolute()
                    {
                        open_files += 1;
                        proc.files.push(target);
                    }
                }
            }
            Err(e) if gone(&e) => {}
            Err(_) => out.file_gaps.deny(own),
        }

        proc.name = fs::read_to_string(base.join("comm"))
            .ok()
            .map(|s| s.trim_end().to_owned());
        out.procs.push(proc);
    }
    out
}

/// The process exited between listing and inspection.
fn gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3 /* ESRCH */)
}

pub(super) fn is_under(path: &Path, root: &Path) -> bool {
    is_under_exact(path, root)
}

pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    a == b
}
