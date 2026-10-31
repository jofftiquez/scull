//! macOS: `libproc` (`proc_listallpids`, `proc_pidinfo`, `proc_pidfdinfo`).
//!
//! * Owner: `PROC_PIDT_SHORTBSDINFO`, which the kernel answers for any process without a
//!   same-user check. If even that fails, the owner is assumed to be the current user, so the
//!   gap stays protective.
//! * Working directory: `PROC_PIDVNODEPATHINFO` (`pvi_cdir`). Needs the same user or root.
//! * Open files: `PROC_PIDLISTFDS`, then `PROC_PIDFDVNODEPATHINFO` for each vnode descriptor.
//!   Needs the same user or root.
//!
//! These calls copy kernel bookkeeping into our buffers; no file is opened and the inspected
//! process is not affected.

#![allow(unsafe_code)]

use super::{Collected, Limits, Proc, is_under_exact};
use std::ffi::{CStr, c_char, c_int, c_void};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// `PROC_PIDFDVNODEPATHINFO` from `<sys/proc_info.h>` (not exported by `libc`).
const PROC_PIDFDVNODEPATHINFO: c_int = 2;

/// `struct proc_fileinfo` from `<sys/proc_info.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcFileInfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: libc::off_t,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` from `<sys/proc_info.h>` (not exported by `libc`).
#[repr(C)]
#[derive(Clone, Copy)]
struct VnodeFdInfoWithPath {
    pfi: ProcFileInfo,
    pvip: libc::vnode_info_path,
}

// Layout checks against the C headers (sizes from <sys/proc_info.h> on 64-bit macOS).
const _: () = assert!(size_of::<ProcFileInfo>() == 24);
const _: () = assert!(size_of::<VnodeFdInfoWithPath>() == 1200);

/// Size of `T` as the `c_int` buffer length libproc expects.
fn c_size<T>() -> c_int {
    // All structs used here are far below `c_int::MAX` bytes.
    c_int::try_from(size_of::<T>()).unwrap_or(c_int::MAX)
}

/// `proc_pidinfo` for one fixed-size flavor. `Ok(None)` if the process is gone.
///
/// `T` must be a plain C struct for which all-zero bytes are a valid value.
fn pidinfo<T: Copy>(pid: c_int, flavor: c_int) -> io::Result<Option<T>> {
    let mut buf = MaybeUninit::<T>::zeroed();
    // SAFETY: `buf` is a writable buffer of exactly `size_of::<T>()` bytes, and we pass that
    // size; the kernel writes at most that many bytes.
    let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, buf.as_mut_ptr().cast(), c_size::<T>()) };
    if n == c_size::<T>() {
        // SAFETY: the kernel filled the whole buffer, and it started zeroed; `T` is a plain C
        // struct for which any byte pattern is valid.
        return Ok(Some(unsafe { buf.assume_init() }));
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(None)
    } else {
        Err(err)
    }
}

/// A NUL-terminated `vip_path` as a path.
fn vip_path(p: &libc::vnode_info_path) -> Option<PathBuf> {
    // `vip_path` is `[[c_char; 32]; 32]`, i.e. one contiguous `char[MAXPATHLEN]`.
    let bytes: &[c_char] = p.vip_path.as_flattened();
    // SAFETY: `c_char` and `u8` have the same size and alignment; the slice stays in bounds.
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast(), bytes.len()) };
    let s = CStr::from_bytes_until_nul(bytes).ok()?;
    if s.is_empty() {
        return None;
    }
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(s.to_bytes())))
}

fn list_pids() -> io::Result<Vec<c_int>> {
    for _ in 0..4 {
        // SAFETY: a null buffer with size 0 asks only for the number of pids.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        let cap = usize::try_from(n).unwrap_or(0) + 64;
        let mut pids: Vec<c_int> = vec![0; cap];
        let bytes = c_int::try_from(cap * size_of::<c_int>()).unwrap_or(c_int::MAX);
        // SAFETY: `pids` has room for `cap` pids, i.e. `bytes` bytes.
        let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        let got = usize::try_from(got).unwrap_or(0);
        if got < cap {
            pids.truncate(got);
            return Ok(pids);
        }
        // The buffer filled up: more processes appeared; try again.
    }
    Err(io::Error::other("process list kept growing"))
}

fn list_fds(pid: c_int) -> io::Result<Option<Vec<libc::proc_fdinfo>>> {
    // SAFETY: a null buffer with size 0 asks only for the needed size.
    let need =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if need <= 0 {
        let err = io::Error::last_os_error();
        return if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(err)
        };
    }
    let count = usize::try_from(need).unwrap_or(0) / size_of::<libc::proc_fdinfo>() + 16;
    let mut fds: Vec<libc::proc_fdinfo> = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        count
    ];
    let bytes = c_int::try_from(count * size_of::<libc::proc_fdinfo>()).unwrap_or(c_int::MAX);
    // SAFETY: `fds` has room for `count` entries, i.e. `bytes` bytes.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    if got <= 0 {
        let err = io::Error::last_os_error();
        return if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(err)
        };
    }
    fds.truncate(usize::try_from(got).unwrap_or(0) / size_of::<libc::proc_fdinfo>());
    Ok(Some(fds))
}

fn fd_path(pid: c_int, fd: i32) -> Option<PathBuf> {
    let mut info = MaybeUninit::<VnodeFdInfoWithPath>::zeroed();
    let size = c_size::<VnodeFdInfoWithPath>();
    // SAFETY: `info` is a writable buffer of `size` bytes and we pass that size.
    let n = unsafe {
        libc::proc_pidfdinfo(
            pid,
            fd,
            PROC_PIDFDVNODEPATHINFO,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n != size {
        return None; // closed meanwhile, or not a vnode
    }
    // SAFETY: fully written by the kernel, zero-initialized before; plain C struct.
    let info = unsafe { info.assume_init() };
    vip_path(&info.pvip)
}

pub(super) fn collect(limits: &Limits, deadline: Instant) -> Collected {
    let mut out = Collected::default();
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let my_uid = unsafe { libc::geteuid() };
    let pids = match list_pids() {
        Ok(p) => p,
        Err(e) => {
            out.error = Some(format!("cannot list processes: {e}"));
            return out;
        }
    };
    let mut open_files = 0usize;
    for pid in pids {
        if pid <= 0 {
            continue; // kernel_task (0) holds no user cwd or files
        }
        if out.procs.len() >= limits.max_processes || Instant::now() >= deadline {
            out.truncated = true;
            break;
        }
        let (own, name) =
            match pidinfo::<libc::proc_bsdshortinfo>(pid, libc::PROC_PIDT_SHORTBSDINFO) {
                Ok(Some(info)) => {
                    let comm: &[c_char] = &info.pbsi_comm;
                    // SAFETY: same size/alignment reinterpretation of an in-bounds slice.
                    let comm: &[u8] =
                        unsafe { std::slice::from_raw_parts(comm.as_ptr().cast(), comm.len()) };
                    let name = CStr::from_bytes_until_nul(comm)
                        .ok()
                        .map(|c| c.to_string_lossy().into_owned());
                    (info.pbsi_uid == my_uid, name)
                }
                Ok(None) => continue,   // exited
                Err(_) => (true, None), // owner unknown: protective
            };
        let mut proc = Proc {
            pid: pid.unsigned_abs(),
            name,
            ..Proc::default()
        };

        match pidinfo::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO) {
            Ok(Some(info)) => proc.cwd = vip_path(&info.pvi_cdir),
            Ok(None) => continue,
            Err(_) => out.cwd_gaps.deny(own),
        }

        match list_fds(pid) {
            Ok(Some(fds)) => {
                for fd in fds {
                    if u32::try_from(libc::PROX_FDTYPE_VNODE).ok() != Some(fd.proc_fdtype) {
                        continue;
                    }
                    if open_files >= limits.max_open_files {
                        out.truncated = true;
                        break;
                    }
                    if let Some(path) = fd_path(pid, fd.proc_fd) {
                        open_files += 1;
                        proc.files.push(path);
                    }
                }
            }
            Ok(None) => {}
            Err(_) => out.file_gaps.deny(own),
        }
        out.procs.push(proc);
    }
    out
}

/// Prefix test that ignores ASCII case: the default APFS volume is case-insensitive, and the
/// kernel may report a path with the case it was looked up with. Matching more loosely only
/// ever reports more processes as holding a path, which is the protective direction.
pub(super) fn is_under(path: &Path, root: &Path) -> bool {
    is_under_exact(path, root) || {
        let mut p = path.components();
        root.components().all(|r| {
            p.next()
                .is_some_and(|c| eq_ignore_case(c.as_os_str(), r.as_os_str()))
        })
    }
}

pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    a == b || (a.components().count() == b.components().count() && is_under(a, b))
}

fn eq_ignore_case(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    a.as_bytes().eq_ignore_ascii_case(b.as_bytes())
}
