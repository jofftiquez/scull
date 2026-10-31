//! Ctrl-C handling (CLI-02).
//!
//! The first Ctrl-C sets a process-wide flag. Long-running commands poll [`check`] between
//! units of work, stop cleanly (leaving no partial state) and exit with
//! [`ExitCode::Cancelled`] (130). A second Ctrl-C exits
//! immediately with 130, for when a command is stuck in a blocking call.
//!
//! The handler is installed lazily by commands that do real work, so `--help`, `--version`
//! and `completions` stay fast.

use scull_core::{ErrorKind, ExitCode, ScullError};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static INSTALL: Once = Once::new();

/// Install the Ctrl-C handler (idempotent). Failure to install is reported at `-v` only: the
/// default OS behavior (terminate) still applies, which is safe for read-only commands.
pub fn install() -> Result<(), String> {
    let mut result = Ok(());
    INSTALL.call_once(|| {
        result = ctrlc::set_handler(on_interrupt).map_err(|e| e.to_string());
    });
    result
}

fn on_interrupt() {
    if CANCELLED.swap(true, Ordering::SeqCst) {
        std::process::exit(ExitCode::Cancelled.as_i32());
    }
}

/// Whether Ctrl-C has been pressed.
pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

/// `Err(cancelled)` once Ctrl-C has been pressed; commands propagate it with `?`.
pub fn check() -> Result<(), ScullError> {
    if is_cancelled() {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}

/// The error returned when the user cancels.
pub fn cancelled_error() -> ScullError {
    ScullError::new(ErrorKind::Cancelled, "cancelled by user")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_maps_to_exit_130() {
        let e = cancelled_error();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert_eq!(e.exit_code(), ExitCode::Cancelled);
        assert_eq!(e.exit_code().as_i32(), 130);
    }

    #[test]
    fn flag_drives_check() {
        // The flag is process-global; this is the only test that sets it.
        assert!(check().is_ok());
        assert!(install().is_ok());
        assert!(install().is_ok(), "second install is a no-op");
        on_interrupt();
        assert!(is_cancelled());
        assert_eq!(check().map_err(|e| e.kind), Err(ErrorKind::Cancelled));
        CANCELLED.store(false, Ordering::SeqCst);
    }
}
