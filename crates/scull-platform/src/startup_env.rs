//! Removing environment variables at process start (P4-MCP-02 review).
//!
//! The desktop app clears variables that would let another process turn on its web view's
//! remote debugging or inject browser arguments (`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`,
//! `WEBKIT_INSPECTOR_SERVER`, ...) before any web view exists. Since Rust 2024, removing a
//! variable is `unsafe` (another thread could be reading the environment), so it lives here,
//! the only crate allowed `unsafe`, behind a function whose contract is "call it first".

/// Remove each variable in `names` from this process's environment. Returns the names that
/// were set (and are now gone), for logging without values.
///
/// Call it at the very start of `main`, before any other thread exists (no runtime, logger or
/// GUI toolkit started yet): that is what makes the removal sound.
pub fn remove_vars_before_threads(names: &[&str]) -> Vec<String> {
    let mut removed = Vec::new();
    for name in names {
        if std::env::var_os(name).is_some() {
            #[allow(unsafe_code)]
            // SAFETY: the caller guarantees no other thread exists yet (this runs first in
            // `main`), so nothing can read or write the environment concurrently.
            unsafe {
                std::env::remove_var(name);
            }
            removed.push((*name).to_owned());
        }
    }
    removed
}
