//! Runtime skips that fail in CI.
//!
//! Some fixtures can only run their assertions when a prerequisite holds (symlinks are
//! permitted, permissions are enforced because the test is not running as root, Git is
//! installed, ...). Locally a missing prerequisite skips the test with a message. In CI
//! (the `CI` environment variable is set, as on GitHub Actions) it is a **failure**: a green
//! CI run then proves the assertions ran. `SCULL_TEST_ALLOW_SKIP=1` allows the skip anyway,
//! for running CI-like environments locally.
//!
//! ```no_run
//! if !scull_fixtures::skip::require(git_available(), "fx_race_03", "git is not installed") {
//!     return;
//! }
//! # fn git_available() -> bool { true }
//! ```

/// Set to `1` to allow a skip even when `CI` is set.
pub const ALLOW_SKIP_ENV: &str = "SCULL_TEST_ALLOW_SKIP";

/// Whether a missing prerequisite may skip a test here: always outside CI, in CI only with
/// [`ALLOW_SKIP_ENV`]` = 1`.
pub fn allowed() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    !set("CI") || std::env::var(ALLOW_SKIP_ENV).as_deref() == Ok("1")
}

/// Skip `test` because `why`: prints the reason, or panics when skips are not
/// [allowed] (CI). Return from the test right after calling it.
#[track_caller]
pub fn skip(test: &str, why: &str) {
    assert!(
        allowed(),
        "{test} would be skipped in CI ({why}): provide the prerequisite in the CI job, or set \
         {ALLOW_SKIP_ENV}=1 to allow the skip"
    );
    eprintln!("SKIP {test}: {why}");
}

/// `true` when `prerequisite` holds; otherwise [`skip`]s `test` (a failure in CI) and returns
/// `false`, so the caller returns.
#[track_caller]
pub fn require(prerequisite: bool, test: &str, why: &str) -> bool {
    if !prerequisite {
        skip(test, why);
    }
    prerequisite
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_met_prerequisite_never_skips() {
        assert!(require(true, "t", "never shown"));
    }
}
