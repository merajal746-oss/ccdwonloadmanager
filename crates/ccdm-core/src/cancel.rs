//! Cooperative cancellation (cf. XDM `CancelFlag`).
//!
//! Download loops check the flag after every chunk and abort with
//! [`CcdmError::Cancelled`](crate::CcdmError::Cancelled). Because partial
//! `.part` files stay on disk, cancelling is exactly how pause works:
//! resume just starts the missing ranges again.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cheaply cloneable cancellation flag shared between the UI thread
/// (pause button) and the download task.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag {
    cancelled: Arc<AtomicBool>,
}

impl CancelFlag {
    /// Unsignalled flag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the download loop to stop at the next chunk boundary.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Re-arm after a cancel (used when resuming).
    pub fn clear(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// `Err(Cancelled)` when signalled, `Ok(())` otherwise — for byte loops.
    pub fn check(&self) -> crate::Result<()> {
        if self.is_cancelled() {
            Err(crate::CcdmError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_and_clear() {
        let flag = CancelFlag::new();
        assert!(!flag.is_cancelled());
        assert!(flag.check().is_ok());
        flag.cancel();
        assert!(flag.is_cancelled());
        assert!(flag.check().is_err());
        flag.clear();
        assert!(flag.check().is_ok());
    }

    #[test]
    fn clones_share_state() {
        let flag = CancelFlag::new();
        let other = flag.clone();
        other.cancel();
        assert!(flag.is_cancelled());
    }
}
