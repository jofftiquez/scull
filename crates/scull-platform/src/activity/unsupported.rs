//! Other Unix systems: no process inspection yet; every signal is reported as unsupported.

use super::{Collected, Limits, is_under_exact};
use std::path::Path;
use std::time::Instant;

pub(super) fn collect(_limits: &Limits, _deadline: Instant) -> Collected {
    let reason = format!(
        "process inspection is not implemented on {}",
        std::env::consts::OS
    );
    Collected {
        cwd_unsupported: Some(reason.clone()),
        files_unsupported: Some(reason),
        ..Collected::default()
    }
}

pub(super) fn is_under(path: &Path, root: &Path) -> bool {
    is_under_exact(path, root)
}

pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    a == b
}
