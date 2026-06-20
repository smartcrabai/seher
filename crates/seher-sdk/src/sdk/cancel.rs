use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Shared cancellation signal. Cheap to clone; all clones share the same state.
///
/// Designed for cooperative cancellation: callers `cancel()` once, and each
/// participant checks `is_cancelled()` in its polling loop or awaits
/// `cancelled()` concurrently.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.0.notify.notify_waiters();
    }

    /// Returns `true` if [`cancel`] has been called on any clone of this token.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Resolves immediately if already cancelled; otherwise suspends until
    /// [`cancel`] is called.
    pub async fn cancelled(&self) {
        loop {
            // Register the Notified future BEFORE checking is_cancelled() to
            // avoid a lost-wakeup: if cancel() fires between the check and
            // notified().await, the notification is already recorded and the
            // future resolves immediately on the next poll.
            let notified = self.0.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests may panic on unexpected fixtures")]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Synchronous tests
    // -----------------------------------------------------------------------

    #[test]
    fn initially_not_cancelled() {
        // Given: a freshly created token
        // When: is_cancelled is checked before any cancel() call
        // Then: returns false
        let cancel = CancelToken::new();
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn cancel_sets_is_cancelled_true() {
        // Given: a fresh token
        // When: cancel() is called
        // Then: is_cancelled() returns true
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        // Given: a token
        // When: cancel() is called multiple times
        // Then: no panic and is_cancelled() stays true
        let cancel = CancelToken::new();
        cancel.cancel();
        cancel.cancel();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn clone_sees_cancel_from_original() {
        // Given: a token and a clone
        // When: cancel() is called on the original
        // Then: the clone reflects the cancellation
        let cancel = CancelToken::new();
        let clone = cancel.clone();
        cancel.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn original_sees_cancel_from_clone() {
        // Given: a token and a clone
        // When: cancel() is called on the clone
        // Then: the original reflects the cancellation
        let cancel = CancelToken::new();
        let clone = cancel.clone();
        clone.cancel();
        assert!(cancel.is_cancelled());
    }

    // -----------------------------------------------------------------------
    // Async tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cancelled_future_resolves_after_cancel() {
        // Given: a token whose cancel() will be called from a concurrent task
        let cancel = CancelToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel2.cancel();
        });
        // When: we await cancelled()
        // Then: it must resolve within a generous timeout
        tokio::time::timeout(std::time::Duration::from_millis(500), cancel.cancelled())
            .await
            .expect("cancelled() should resolve within 500 ms after cancel() is called");
    }

    #[tokio::test]
    async fn multiple_awaiters_all_notified() {
        // Given: three concurrent tasks awaiting cancelled()
        let cancel = CancelToken::new();
        let c1 = cancel.clone();
        let c2 = cancel.clone();
        let c3 = cancel.clone();

        let h1 = tokio::spawn(async move { c1.cancelled().await });
        let h2 = tokio::spawn(async move { c2.cancelled().await });
        let h3 = tokio::spawn(async move { c3.cancelled().await });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // When: cancel() is called once
        cancel.cancel();
        // Then: all three awaiters must resolve
        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            h1.await.expect("h1 should complete");
            h2.await.expect("h2 should complete");
            h3.await.expect("h3 should complete");
        })
        .await
        .expect("all awaiters should resolve within 500 ms");
    }
}
