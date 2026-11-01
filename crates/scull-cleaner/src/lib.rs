//! The Executor: the only code path in scull that deletes anything (SAFE-10).
//!
//! See `docs/ARCHITECTURE.md` for responsibilities and boundaries.

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-cleaner");
    }
}
