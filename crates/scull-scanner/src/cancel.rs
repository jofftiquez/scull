//! Cooperative cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cheap, cloneable cancellation flag shared between the caller and a running scan.
///
/// Call [`CancelToken::cancel`] from any thread (a Ctrl-C handler, a UI button, a timeout).
/// Workers check the flag between every directory entry, so [`crate::scan`] returns within
/// a few tens of milliseconds with [`crate::ScanStatus::Cancelled`] (PRD §21.2: < 500 ms).
/// A token can't be reset; use a new one for the next scan.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::CancelToken;

    #[test]
    fn clones_share_the_flag() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!b.is_cancelled());
        a.cancel();
        assert!(b.is_cancelled());
    }
}
