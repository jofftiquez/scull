//! Test support (feature `test-support`, Windows): deliver a real console control event to
//! another process, so a fixture can check what `scull` does when the user interrupts it
//! (FX-OUT-02: exit 130, nothing half-done).
//!
//! The event is `CTRL_BREAK_EVENT`, sent to the target's own process group. `CTRL_C_EVENT`
//! can only be sent to *every* process on a console (with a group ID Windows drops it), so a
//! test can't aim it at one child. Below a CI job, whose process group ignores Ctrl-C, a child
//! also keeps ignoring it even when its parent re-enables Ctrl-C (the attribute is not
//! re-inherited; CPython skips its own `CTRL_C_EVENT` test for this reason). scull re-enables
//! Ctrl-C for itself when it installs its handler ([`crate::interrupt::accept_ctrl_c`]), and
//! that handler (the `ctrlc` crate) treats Ctrl-C and Ctrl-Break alike, so the cancel path
//! the test drives is the one a user's Ctrl-C takes.
//!
//! The target is started with [`TARGET_CREATION_FLAGS`] (a console and a process group of its
//! own); a **helper process** attaches to the target's console and generates the event for the
//! target's group. Never call [`send_ctrl_break`] from a test runner that shares its console
//! with other tests: it gives up the caller's console.
#![allow(unsafe_code)]

use std::io;
use std::time::Duration;
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
};

/// `CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP`: the process creation flags for the target,
/// a console of its own and a process group whose ID is its process ID.
pub const TARGET_CREATION_FLAGS: u32 = 0x0000_0010 | 0x0000_0200;

/// Deliver `CTRL_BREAK_EVENT` to the process group of `pid` (a process started with
/// [`TARGET_CREATION_FLAGS`]). The calling process leaves its own console for good; run it in
/// a helper process that does nothing else afterwards.
pub fn send_ctrl_break(pid: u32) -> io::Result<()> {
    // SAFETY: FreeConsole takes no arguments; failing (no console) is fine.
    unsafe { FreeConsole() };
    // SAFETY: plain call with a process ID; on failure nothing was changed.
    if unsafe { AttachConsole(pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: plain call; the group is the target's own (its process ID), so no other process,
    // this one included, receives the event.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    let error = io::Error::last_os_error();
    // The event is delivered asynchronously, by the console host: stay attached meanwhile.
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: detaches from the target's console.
    unsafe { FreeConsole() };
    if sent == 0 {
        return Err(error);
    }
    Ok(())
}
