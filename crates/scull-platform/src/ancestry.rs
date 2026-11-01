//! The chain of processes above this one (#35: hardening `scull approve` against agents that
//! drive a pseudo-terminal).
//!
//! [`ancestry`] returns this process's parent, its parent, and so on, up to
//! [`MAX_ANCESTRY`] levels or the first process that can't be read. Each entry has the
//! process ID, its short name (the executable's file name where the OS reports it) and, when
//! readable, the full executable path.
//!
//! This is a **signal, not a guarantee**: names are chosen by whoever starts a process, a
//! daemonizing process (a `tmux` server) cuts the chain, and a process can re-parent itself.
//! Callers use it to refuse the obvious cases and say so honestly.
//!
//! * Linux: `/proc/<pid>/stat` (parent and `comm`) and `/proc/<pid>/exe`.
//! * macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` (parent and name) and `proc_pidpath`.
//! * Windows: `NtQueryInformationProcess(ProcessBasicInformation)` (parent) and
//!   `QueryFullProcessImageNameW`; a parent created after its child is a reused process ID
//!   and ends the chain.
//! * Elsewhere: unsupported (an error; callers fail closed).

use crate::Result;
#[allow(unused_imports)] // Not every OS module uses them.
use crate::{Operation, PlatformError};
use std::path::PathBuf;

/// Most levels [`ancestry`] walks.
pub const MAX_ANCESTRY: usize = 64;

/// One process above this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Process ID.
    pub pid: u32,
    /// The name the OS reports for the process: `comm` on Linux (at most 15 bytes), the
    /// process name on macOS, the executable's file name on Windows. It can differ from the
    /// executable's file name (a launcher that renames itself, a versioned binary), so callers
    /// compare both ([`ProcessInfo::names`]).
    pub name: String,
    /// Full path of the executable, when the OS lets us read it.
    pub exe: Option<PathBuf>,
}

impl ProcessInfo {
    /// The process name and, when different, the executable's file name.
    pub fn names(&self) -> Vec<String> {
        let mut out = vec![self.name.clone()];
        if let Some(f) = self.exe.as_deref().and_then(std::path::Path::file_name) {
            let f = f
                .to_string_lossy()
                .trim_end_matches(" (deleted)")
                .to_owned();
            if f != self.name {
                out.push(f);
            }
        }
        out
    }
}

/// This process's ancestors, parent first (this process itself is not included). Ends at the
/// root of the tree, at [`MAX_ANCESTRY`] levels, or at the first process that can't be read
/// (another user's, protected, or gone). Fails only when not even the parent can be read, or
/// on an OS without support.
pub fn ancestry() -> Result<Vec<ProcessInfo>> {
    imp::ancestry()
}

#[allow(dead_code)] // Not every OS module uses it.
fn unsupported(what: &str) -> PlatformError {
    PlatformError::from_io(
        Operation::Metadata,
        std::path::Path::new("process ancestry"),
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("{what} is not supported on this OS"),
        ),
    )
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{MAX_ANCESTRY, ProcessInfo};
    use crate::{Operation, PlatformError, Result};
    use std::path::{Path, PathBuf};

    /// `(ppid, comm)` from `/proc/<pid>/stat`. `comm` is in parentheses and may itself contain
    /// `)`, so the fields after it are found from the last `)`.
    fn stat(pid: u32) -> std::io::Result<(u32, String)> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "unexpected stat");
        let open = text.find('(').ok_or_else(bad)?;
        let close = text.rfind(')').ok_or_else(bad)?;
        let comm = text.get(open + 1..close).ok_or_else(bad)?.to_owned();
        let rest: Vec<&str> = text
            .get(close + 1..)
            .ok_or_else(bad)?
            .split_whitespace()
            .collect();
        // rest[0] = state, rest[1] = ppid.
        let ppid = rest.get(1).and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        Ok((ppid, comm))
    }

    fn info(pid: u32, comm: String) -> ProcessInfo {
        let exe: Option<PathBuf> = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
        ProcessInfo {
            pid,
            name: comm,
            exe,
        }
    }

    pub(super) fn ancestry() -> Result<Vec<ProcessInfo>> {
        let me = std::process::id();
        let (mut ppid, _) = stat(me).map_err(|e| {
            PlatformError::from_io(Operation::Metadata, Path::new("/proc/self/stat"), e)
        })?;
        let mut out = Vec::new();
        while ppid != 0 && out.len() < MAX_ANCESTRY {
            let Ok((next, comm)) = stat(ppid) else {
                break;
            };
            out.push(info(ppid, comm));
            if next == ppid {
                break;
            }
            ppid = next;
        }
        if out.is_empty() && std::os::unix::process::parent_id() != 0 {
            return Err(PlatformError::from_io(
                Operation::Metadata,
                Path::new("/proc"),
                std::io::Error::other("the parent process can't be read"),
            ));
        }
        Ok(out)
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod imp {
    use super::{MAX_ANCESTRY, ProcessInfo};
    use crate::{Operation, PlatformError, Result};
    use std::ffi::{CStr, c_int};
    use std::mem::MaybeUninit;
    use std::path::{Path, PathBuf};

    fn bsdinfo(pid: u32) -> Option<libc::proc_bsdinfo> {
        let mut buf = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: `buf` is a writable buffer of exactly `size` bytes, the size this flavor
        // fills; the result is used only when the call reports filling all of it.
        let n = unsafe {
            libc::proc_pidinfo(
                pid as c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                buf.as_mut_ptr().cast(),
                size,
            )
        };
        if n == size {
            // SAFETY: the kernel filled the whole structure (checked above).
            Some(unsafe { buf.assume_init() })
        } else {
            None
        }
    }

    fn path(pid: u32) -> Option<PathBuf> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: `buf` is writable for the length passed; the kernel writes a NUL-terminated
        // path of at most that many bytes and returns its length.
        let n =
            unsafe { libc::proc_pidpath(pid as c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
    }

    fn c_name(raw: &[libc::c_char]) -> String {
        // SAFETY: reinterpreting `c_char` as `u8` of the same length; both are one byte.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(raw.as_ptr().cast(), raw.len()) };
        CStr::from_bytes_until_nul(bytes)
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
    }

    pub(super) fn ancestry() -> Result<Vec<ProcessInfo>> {
        let me = std::process::id();
        let first = bsdinfo(me).ok_or_else(|| {
            PlatformError::from_io(
                Operation::Metadata,
                Path::new("proc_pidinfo"),
                std::io::Error::last_os_error(),
            )
        })?;
        let mut ppid = first.pbi_ppid;
        let mut out = Vec::new();
        while ppid != 0 && out.len() < MAX_ANCESTRY {
            let Some(b) = bsdinfo(ppid) else {
                break;
            };
            let exe = path(ppid);
            let name = {
                let n = c_name(&b.pbi_name);
                if n.is_empty() { c_name(&b.pbi_comm) } else { n }
            };
            out.push(ProcessInfo {
                pid: ppid,
                name,
                exe,
            });
            if b.pbi_ppid == ppid {
                break;
            }
            ppid = b.pbi_ppid;
        }
        if out.is_empty() && std::os::unix::process::parent_id() > 1 {
            return Err(PlatformError::from_io(
                Operation::Metadata,
                Path::new("proc_pidinfo"),
                std::io::Error::other("the parent process can't be read"),
            ));
        }
        Ok(out)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use super::{MAX_ANCESTRY, ProcessInfo};
    use crate::{Operation, PlatformError, Result};
    use std::ffi::{OsString, c_void};
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    /// `PROCESS_BASIC_INFORMATION` (winternl.h).
    #[repr(C)]
    struct BasicInfo {
        exit_status: i32,
        peb: *mut c_void,
        affinity: usize,
        base_priority: i32,
        unique_pid: usize,
        parent_pid: usize,
    }

    struct Owned(HANDLE);

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: a valid process handle we own, closed exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn open(pid: u32) -> Option<Owned> {
        // SAFETY: plain call; a null handle means failure.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        (!h.is_null()).then_some(Owned(h))
    }

    fn parent(h: HANDLE) -> Option<u32> {
        use windows_sys::Wdk::System::Threading::{
            NtQueryInformationProcess, ProcessBasicInformation,
        };
        let mut info = MaybeUninit::<BasicInfo>::zeroed();
        let mut ret = 0u32;
        // SAFETY: `info` is a writable buffer of the size passed, the size this class fills.
        let status = unsafe {
            NtQueryInformationProcess(
                h,
                ProcessBasicInformation,
                info.as_mut_ptr().cast(),
                size_of::<BasicInfo>() as u32,
                &mut ret,
            )
        };
        if status < 0 {
            return None;
        }
        // SAFETY: the call succeeded and filled the structure.
        let info = unsafe { info.assume_init() };
        u32::try_from(info.parent_pid).ok()
    }

    fn created(h: HANDLE) -> Option<u64> {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
        // SAFETY: four writable FILETIME out-parameters.
        let ok = unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) };
        (ok != 0).then(|| (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
    }

    fn image(h: HANDLE) -> Option<PathBuf> {
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` is writable for `len` UTF-16 units; `len` is updated to the length
        // written (without the NUL).
        let ok = unsafe {
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        };
        if ok == 0 {
            return None;
        }
        buf.truncate(len as usize);
        Some(PathBuf::from(OsString::from_wide(&buf)))
    }

    pub(super) fn ancestry() -> Result<Vec<ProcessInfo>> {
        // SAFETY: the pseudo-handle of this process; never closed.
        let me = unsafe { GetCurrentProcess() };
        let mut ppid = parent(me).ok_or_else(|| {
            PlatformError::from_io(
                Operation::Metadata,
                Path::new("NtQueryInformationProcess"),
                std::io::Error::last_os_error(),
            )
        })?;
        let mut child_created = created(me);
        let mut out = Vec::new();
        while ppid != 0 && out.len() < MAX_ANCESTRY {
            let Some(h) = open(ppid) else {
                break;
            };
            let born = created(h.0);
            // A "parent" created after its child is an unrelated process that reused the ID.
            if let (Some(p), Some(c)) = (born, child_created)
                && p > c
            {
                break;
            }
            let exe = image(h.0);
            let name = exe
                .as_deref()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("pid {ppid}"));
            let next = parent(h.0);
            out.push(ProcessInfo {
                pid: ppid,
                name,
                exe,
            });
            match next {
                Some(n) if n != ppid => {
                    ppid = n;
                    child_created = born;
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use super::ProcessInfo;
    use crate::Result;

    pub(super) fn ancestry() -> Result<Vec<ProcessInfo>> {
        Err(super::unsupported("reading the process ancestry"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestry_starts_at_the_parent_and_names_it() {
        let a = ancestry().expect("ancestry");
        assert!(!a.is_empty(), "a test always has a parent");
        #[cfg(unix)]
        assert_eq!(a[0].pid, std::os::unix::process::parent_id());
        assert!(a.iter().all(|p| !p.name.is_empty()), "{a:?}");
        assert!(a.iter().all(|p| p.names()[0] == p.name));
        assert!(a.len() <= MAX_ANCESTRY);
        let mut pids: Vec<u32> = a.iter().map(|p| p.pid).collect();
        pids.dedup();
        assert_eq!(pids.len(), a.len(), "no loops: {a:?}");
    }

    #[test]
    fn this_process_is_not_included() {
        let me = std::process::id();
        let a = ancestry().expect("ancestry");
        assert!(
            a.iter().all(|p| p.pid != me),
            "this process itself is not included"
        );
    }
}
