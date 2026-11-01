//! Test support (feature `test-support`, Windows): deliver a real console Ctrl-C to another
//! process, so a fixture can check what `scull` does when the user presses Ctrl-C (FX-OUT-02:
//! exit 130, nothing half-done).
//!
//! Windows has no per-process Ctrl-C: `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)` reaches
//! every process attached to the caller's console. So the target is started with a console of
//! its own (`CREATE_NEW_CONSOLE`), and a **helper process** detaches from its console, attaches
//! to the target's, ignores the event itself and generates it. Never call this from a test
//! runner that shares its console with other tests: it gives up the caller's console.
#![allow(unsafe_code)]

use std::io;
use std::time::Duration;
use windows_sys::Win32::System::Console::{
    AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};

/// `CREATE_NEW_CONSOLE`: the process creation flag that gives the target its own console.
pub const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// Deliver `CTRL_C_EVENT` to every process attached to the console of process `pid` (started
/// with [`CREATE_NEW_CONSOLE`]). The calling process leaves its own console for good and
/// ignores the event; run it in a helper process that does nothing else afterwards.
pub fn send_ctrl_c_to_console_of(pid: u32) -> io::Result<()> {
    // SAFETY: FreeConsole takes no arguments; failing (no console) is fine.
    unsafe { FreeConsole() };
    // SAFETY: plain call with a process ID; on failure nothing was changed.
    if unsafe { AttachConsole(pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a null handler with TRUE makes this process ignore Ctrl-C (documented).
    if unsafe { SetConsoleCtrlHandler(None, 1) } == 0 {
        let e = io::Error::last_os_error();
        // SAFETY: detaches from the console attached above.
        unsafe { FreeConsole() };
        return Err(e);
    }
    // SAFETY: process group 0 = every process on the (target's) console we are attached to.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) };
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
