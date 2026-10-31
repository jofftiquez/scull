//! Read-only MCP server over stdio (MCP-01…05).
//!
//! See `docs/PRD.md` and `HANDOVER.md` for responsibilities and boundaries.

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-mcp");
    }
}
