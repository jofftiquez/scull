//! Scan IDs: time plus randomness (generated here, since `scull-core` performs no I/O).

use scull_core::ScanId;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A new scan ID: `scan_<unix seconds, hex><64 random bits, hex>`. IDs sort roughly by time
/// and never repeat within a process.
pub fn new_scan_id(now_unix: i64) -> ScanId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().hash_one((nanos, n, std::process::id()));
    let secs = u64::try_from(now_unix).unwrap_or(0);
    let suffix = format!("{secs:x}{random:016x}");
    // The suffix is always 17–32 hex digits, which `from_suffix` accepts.
    ScanId::from_suffix(&suffix).unwrap_or_else(|_| fallback())
}

fn fallback() -> ScanId {
    match ScanId::try_from("scan_0".to_owned()) {
        Ok(id) => id,
        Err(_) => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_valid_and_unique() {
        let a = new_scan_id(1_700_000_000);
        let b = new_scan_id(1_700_000_000);
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("scan_"));
        assert!(ScanId::try_from(a.as_str().to_owned()).is_ok());
    }
}
