//! Windows: process working directories from each process's PEB, open files through Restart
//! Manager.
//!
//! # Working directories
//!
//! Windows has no documented API for another process's current directory. Like Process
//! Explorer and psutil, we read it from the process's `RTL_USER_PROCESS_PARAMETERS`
//! (`CurrentDirectory.DosPath`), found through the PEB address `NtQueryInformationProcess`
//! returns. This needs `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`, which a non-elevated
//! user has for their own non-elevated processes only. The structure offsets have been stable
//! since Windows XP; 32-bit (WOW64) processes are read through their 32-bit PEB. Only 64-bit
//! builds of scull read PEBs; a 32-bit build reports working directories as unsupported.
//!
//! Processes that cannot be opened are counted by owner (token user SID compared with ours).
//! When even the owner cannot be read (protected and system processes), the process is
//! counted as another user's.
//!
//! # Open files
//!
//! Restart Manager reports which processes hold any of a set of registered files open. There
//! is no system-wide list of open files to match against, so each query walks the candidate
//! (never through reparse points) and registers at most [`Limits::max_sampled_files`] files;
//! a walk that does not reach every file is reported as truncated. Restart Manager does not
//! say which of the files a process holds, so a [`Hit`] carries the queried path.
//!
//! Restart Manager is only queried here (`RmGetList`); `RmShutdown` / `RmRestart` are never
//! called, so no process is affected.

#![allow(unsafe_code)]

use super::{Collected, Hit, Limits, Observation, Proc};
use std::ffi::{OsString, c_void};
use std::mem::{MaybeUninit, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr;
use std::time::Instant;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_SUCCESS, GetLastError, HANDLE,
    STILL_ACTIVE,
};
use windows_sys::Win32::Security::{EqualSid, GetTokenInformation, TOKEN_QUERY, TokenUser};
use windows_sys::Win32::System::ProcessStatus::K32EnumProcesses;
use windows_sys::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// A process or token handle, closed on drop.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle we own (constructed only from successful opens)
        // and is closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn open_process(pid: u32, access: u32) -> Result<Owned, u32> {
    // SAFETY: plain call; the returned handle is checked and owned by `Owned`.
    let h = unsafe { OpenProcess(access, 0, pid) };
    if h.is_null() {
        // SAFETY: no preconditions.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Owned(h))
    }
}

/// The token user of a process, as an owned buffer holding a `TOKEN_USER` (and its SID).
struct TokenUserBuf(Vec<u64>);

impl TokenUserBuf {
    fn of(process: HANDLE) -> Option<Self> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `process` is a valid process handle; `token` receives a new handle.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return None;
        }
        let token = Owned(token);
        let mut need = 0u32;
        // SAFETY: size query with a null buffer; fails with ERROR_INSUFFICIENT_BUFFER and
        // writes the needed size.
        unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut need) };
        if need == 0 {
            return None;
        }
        // u64 elements keep the buffer 8-byte aligned for `TOKEN_USER`.
        let mut buf = vec![0u64; (need as usize).div_ceil(8)];
        let len = u32::try_from(buf.len() * 8).ok()?;
        // SAFETY: `buf` is writable, aligned and `len >= need` bytes long.
        let ok = unsafe {
            GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut need)
        };
        (ok != 0).then_some(Self(buf))
    }

    fn sid(&self) -> *mut c_void {
        // SAFETY: the buffer holds a `TOKEN_USER` written by `GetTokenInformation`; its first
        // field is `SID_AND_ATTRIBUTES { Sid, .. }`, so the first pointer-sized word is the SID
        // pointer (pointing into this same buffer, which outlives the returned pointer's use).
        unsafe { *self.0.as_ptr().cast::<*mut c_void>() }
    }

    fn same_user(&self, other: &Self) -> bool {
        // SAFETY: both SIDs are valid for the lifetime of their buffers.
        unsafe { EqualSid(self.sid(), other.sid()) != 0 }
    }
}

fn list_pids() -> Option<Vec<u32>> {
    let mut cap = 1024usize;
    for _ in 0..6 {
        let mut pids = vec![0u32; cap];
        let bytes = u32::try_from(cap * 4).ok()?;
        let mut used = 0u32;
        // SAFETY: `pids` has room for `bytes` bytes.
        if unsafe { K32EnumProcesses(pids.as_mut_ptr(), bytes, &mut used) } == 0 {
            return None;
        }
        if used < bytes {
            pids.truncate(used as usize / 4);
            return Some(pids);
        }
        cap *= 4;
    }
    None
}

/// Read a plain integer from another process's memory.
fn read<T: Copy + Default>(process: HANDLE, addr: usize) -> Option<T> {
    let mut v = T::default();
    let mut n = 0usize;
    // SAFETY: `v` is a writable `T`; we ask for exactly `size_of::<T>()` bytes. `addr` is a
    // foreign address that `ReadProcessMemory` validates (it fails rather than faulting).
    let ok = unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
            process,
            addr as *const c_void,
            (&raw mut v).cast(),
            size_of::<T>(),
            &mut n,
        )
    };
    (ok != 0 && n == size_of::<T>()).then_some(v)
}

fn read_utf16(process: HANDLE, addr: usize, len_bytes: u16) -> Option<OsString> {
    let units = usize::from(len_bytes) / 2;
    let mut buf = vec![0u16; units];
    let mut n = 0usize;
    // SAFETY: `buf` has room for `units * 2` bytes, the size we ask for.
    let ok = unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
            process,
            addr as *const c_void,
            buf.as_mut_ptr().cast(),
            units * 2,
            &mut n,
        )
    };
    (ok != 0 && n == units * 2).then(|| OsString::from_wide(&buf))
}

/// `PROCESS_BASIC_INFORMATION` (declared locally: the `windows-sys` one needs extra features).
#[repr(C)]
struct ProcessBasicInformation {
    exit_status: i32,
    peb_base_address: usize,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

/// The current directory of a process opened with query + VM read access.
#[cfg(target_pointer_width = "64")]
fn read_cwd(process: HANDLE) -> Option<PathBuf> {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessBasicInformation as PBI_CLASS, ProcessWow64Information,
    };
    // WOW64 (32-bit) process: its 32-bit PEB is the one it keeps up to date.
    let mut peb32: usize = 0;
    let mut ret = 0u32;
    // SAFETY: `peb32` is a writable pointer-sized buffer, the size this class returns.
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            ProcessWow64Information,
            (&raw mut peb32).cast(),
            size_of::<usize>() as u32,
            &mut ret,
        )
    };
    if status >= 0 && peb32 != 0 {
        // PEB32.ProcessParameters at 0x10; RTL_USER_PROCESS_PARAMETERS32.CurrentDirectory.
        // DosPath (UNICODE_STRING32) at 0x24: Length (u16) then Buffer (u32) at 0x28.
        let params = read::<u32>(process, peb32 + 0x10)? as usize;
        let len = read::<u16>(process, params + 0x24)?;
        let buf = read::<u32>(process, params + 0x28)? as usize;
        return read_utf16(process, buf, len).map(PathBuf::from);
    }
    let mut pbi = MaybeUninit::<ProcessBasicInformation>::zeroed();
    // SAFETY: `pbi` is a writable buffer of the size passed.
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            PBI_CLASS,
            pbi.as_mut_ptr().cast(),
            size_of::<ProcessBasicInformation>() as u32,
            &mut ret,
        )
    };
    if status < 0 {
        return None;
    }
    // SAFETY: zero-initialized and (partly) filled by the call; all fields are integers.
    let pbi = unsafe { pbi.assume_init() };
    let peb = pbi.peb_base_address;
    if peb == 0 {
        return None;
    }
    // PEB.ProcessParameters at 0x20; RTL_USER_PROCESS_PARAMETERS.CurrentDirectory.DosPath
    // (UNICODE_STRING) at 0x38: Length (u16) then Buffer (pointer) at 0x40.
    let params = usize::try_from(read::<u64>(process, peb + 0x20)?).ok()?;
    let len = read::<u16>(process, params + 0x38)?;
    let buf = usize::try_from(read::<u64>(process, params + 0x40)?).ok()?;
    read_utf16(process, buf, len).map(PathBuf::from)
}

fn exited(process: HANDLE) -> bool {
    let mut code = 0u32;
    // SAFETY: `process` is a valid handle; `code` is writable.
    let ok = unsafe { GetExitCodeProcess(process, &mut code) };
    ok != 0 && code != STILL_ACTIVE as u32
}

pub(super) fn collect(limits: &Limits, deadline: Instant) -> Collected {
    // Open files are not part of the snapshot: they are queried per path (Restart Manager).
    let mut out = Collected::default();
    if cfg!(not(target_pointer_width = "64")) {
        out.cwd_unsupported =
            Some("process working directories are only read by 64-bit builds".into());
        return out;
    }
    // SAFETY: the pseudo-handle of the current process needs no closing.
    let me = TokenUserBuf::of(unsafe { GetCurrentProcess() });
    let Some(pids) = list_pids() else {
        out.error = Some("cannot list processes".into());
        return out;
    };
    let my_pid = std::process::id();
    for pid in pids {
        if pid == 0 || pid == 4 {
            continue; // System Idle Process and System: no user working directory
        }
        if out.procs.len() >= limits.max_processes || Instant::now() >= deadline {
            out.truncated = true;
            break;
        }
        let mut proc = Proc {
            pid,
            ..Proc::default()
        };
        let process = match open_process(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ) {
            Ok(h) => h,
            Err(ERROR_INVALID_PARAMETER) => continue, // exited
            Err(_) => {
                let own = owner_is(&me, pid);
                out.cwd_gaps.deny(own);
                continue;
            }
        };
        #[cfg(target_pointer_width = "64")]
        let cwd = read_cwd(process.0);
        #[cfg(not(target_pointer_width = "64"))]
        let cwd: Option<PathBuf> = None;
        match cwd {
            Some(cwd) => {
                // Resolve 8.3 names and case to the final path, like the queried root.
                proc.cwd = Some(std::fs::canonicalize(&cwd).unwrap_or(cwd));
            }
            None if exited(process.0) => continue,
            None if pid == my_pid => {}
            None => {
                let own = me
                    .as_ref()
                    .zip(TokenUserBuf::of(process.0))
                    .is_some_and(|(me, them)| me.same_user(&them));
                out.cwd_gaps.deny(own);
                continue;
            }
        }
        out.procs.push(proc);
    }
    out
}

/// Whether `pid` runs as the current user. Unknown owner (the process can't even be opened
/// for limited queries, or its token can't be read) counts as another user.
fn owner_is(me: &Option<TokenUserBuf>, pid: u32) -> bool {
    let Some(me) = me else { return false };
    let Ok(h) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return false;
    };
    TokenUserBuf::of(h.0).is_some_and(|them| me.same_user(&them))
}

/// Lowercased path without the verbatim prefix and trailing separators, for comparison.
fn normalized(path: &Path) -> String {
    let mut s = String::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::VerbatimDisk(d) | Prefix::Disk(d) => {
                    s.push(char::from(d).to_ascii_lowercase());
                    s.push(':');
                }
                Prefix::VerbatimUNC(server, share) | Prefix::UNC(server, share) => {
                    s.push_str("\\\\");
                    s.push_str(&server.to_string_lossy().to_lowercase());
                    s.push('\\');
                    s.push_str(&share.to_string_lossy().to_lowercase());
                }
                _ => s.push_str(&p.as_os_str().to_string_lossy().to_lowercase()),
            },
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => s.push_str("\\.."),
            Component::Normal(n) => {
                s.push('\\');
                s.push_str(&n.to_string_lossy().to_lowercase());
            }
        }
    }
    s
}

pub(super) fn is_under(path: &Path, root: &Path) -> bool {
    let (p, r) = (normalized(path), normalized(root));
    p == r || (p.starts_with(&r) && p[r.len()..].starts_with('\\'))
}

/// Up to `limits.max_sampled_files` regular files under `dir`, breadth first, never through
/// reparse points. The flag is `true` when every file was collected.
fn sample_files(dir: &Path, limits: &Limits, deadline: Instant) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut queue = std::collections::VecDeque::from([dir.to_path_buf()]);
    let mut complete = true;
    while let Some(d) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            complete = false;
            continue;
        };
        for entry in rd {
            if Instant::now() >= deadline {
                return (files, false);
            }
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            // From the directory listing: symlinks and junctions report `is_symlink`, and are
            // neither followed nor registered.
            let Ok(ft) = entry.file_type() else {
                complete = false;
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                queue.push_back(entry.path());
            } else if ft.is_file() {
                if files.len() >= limits.max_sampled_files {
                    return (files, false);
                }
                files.push(entry.path());
            }
        }
    }
    (files, complete)
}

/// Ask Restart Manager which processes hold any of `files` open.
pub(super) fn restart_manager(files: &[PathBuf], hit_path: &Path) -> Result<Vec<Hit>, String> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    // SAFETY: `session` and `key` (CCH_RM_SESSION_KEY + 1 chars) are writable.
    let rc = unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) };
    if rc != ERROR_SUCCESS {
        return Err(format!("RmStartSession failed ({rc})"));
    }
    struct Session(u32);
    impl Drop for Session {
        fn drop(&mut self) {
            // SAFETY: ends the session we started, once.
            unsafe { RmEndSession(self.0) };
        }
    }
    let session = Session(session);

    let wide: Vec<Vec<u16>> = files
        .iter()
        .map(|f| f.as_os_str().encode_wide().chain(Some(0)).collect())
        .collect();
    let ptrs: Vec<*const u16> = wide.iter().map(|w| w.as_ptr()).collect();
    for chunk in ptrs.chunks(256) {
        // SAFETY: `chunk` holds `chunk.len()` pointers to NUL-terminated UTF-16 strings that
        // live in `wide` for the duration of the call.
        let rc = unsafe {
            RmRegisterResources(
                session.0,
                chunk.len() as u32,
                chunk.as_ptr(),
                0,
                ptr::null(),
                0,
                ptr::null(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("RmRegisterResources failed ({rc})"));
        }
    }

    let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
    for _ in 0..5 {
        let mut needed = 0u32;
        let mut count = infos.len() as u32;
        let mut reasons = 0u32;
        let buf = if infos.is_empty() {
            ptr::null_mut()
        } else {
            infos.as_mut_ptr()
        };
        // SAFETY: `buf` is null with `count == 0`, or points to `count` writable entries.
        let rc = unsafe { RmGetList(session.0, &mut needed, &mut count, buf, &mut reasons) };
        if rc == ERROR_SUCCESS {
            infos.truncate(count as usize);
            return Ok(infos
                .iter()
                .map(|i| Hit {
                    pid: i.Process.dwProcessId,
                    name: Some(wide_to_string(&i.strAppName)).filter(|s| !s.is_empty()),
                    path: hit_path.to_path_buf(),
                })
                .collect());
        }
        if rc != ERROR_MORE_DATA {
            return Err(format!("RmGetList failed ({rc})"));
        }
        // SAFETY: `RM_PROCESS_INFO` is a plain C struct; all-zero is a valid value.
        let zero: RM_PROCESS_INFO = unsafe { MaybeUninit::zeroed().assume_init() };
        infos = vec![zero; needed as usize + 4];
    }
    Err("RmGetList: the list of processes kept changing".into())
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

pub(super) fn open_files_under(dir: &Path, limits: &Limits) -> Observation {
    let deadline = Instant::now() + limits.time_budget;
    let (files, complete) = sample_files(dir, limits, deadline);
    match restart_manager(&files, dir) {
        Ok(hits) => Observation {
            hits,
            truncated: !complete,
            ..Observation::default()
        },
        Err(e) => Observation::failed(e),
    }
}

pub(super) fn files_open(files: &[PathBuf], _limits: &Limits) -> Observation {
    let hit_path = files.first().cloned().unwrap_or_default();
    match restart_manager(files, &hit_path) {
        Ok(hits) => Observation {
            hits,
            ..Observation::default()
        },
        Err(e) => Observation::failed(e),
    }
}
