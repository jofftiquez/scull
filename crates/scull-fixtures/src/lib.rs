//! Generated fixture trees, canary directories and universal invariants I-1…I-8 (docs/FIXTURES.md).
//!
//! See `docs/PRD.md` and `HANDOVER.md` for responsibilities and boundaries.

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-fixtures");
    }
}
