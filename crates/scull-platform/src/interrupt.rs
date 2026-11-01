//! Ctrl-C delivery to this process (CLI-02, CLI-05).

/// Make sure this process receives Ctrl-C, before a Ctrl-C handler is installed.
///
/// * **Windows:** a process started in a new process group (`CREATE_NEW_PROCESS_GROUP`, as
///   some launchers, services and CI runners do) ignores Ctrl-C, and every child inherits
///   that: a user's Ctrl-C would then never reach scull's handler. This restores normal Ctrl-C
///   processing for this process (`SetConsoleCtrlHandler(NULL, FALSE)`). Ctrl-Break is never
///   ignored and needs nothing.
/// * **Unix:** nothing to do (`SIGINT` reaches the handler `ctrlc` installs).
pub fn accept_ctrl_c() -> std::io::Result<()> {
    imp()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn imp() -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    // SAFETY: a null handler with FALSE restores normal Ctrl-C processing (documented); no
    // pointer is passed.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn imp() -> std::io::Result<()> {
    Ok(())
}
