use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::sleep;

/// Cooperative cancellation shared between the signal handler and every worker.
///
/// Cloning shares the same underlying state, so a single `cancel()` is observed
/// by all holders. Cancellation is one-way: once requested it stays requested.
#[derive(Debug, Clone)]
pub struct Cancellation {
    sender: Arc<watch::Sender<bool>>,
    receiver: watch::Receiver<bool>,
}

impl Cancellation {
    /// Create a new, not-yet-cancelled token.
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self {
            sender: Arc::new(sender),
            receiver,
        }
    }

    /// Request cancellation. Repeated calls are harmless.
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Resolve as soon as cancellation is requested.
    pub async fn cancelled(&self) {
        let mut receiver = self.receiver.clone();
        if *receiver.borrow() {
            return;
        }
        // The sender is held by this token, so `changed()` only fails if the
        // channel closed, which cannot happen while `self` is alive.
        while receiver.changed().await.is_ok() {
            if *receiver.borrow() {
                return;
            }
        }
    }

    /// Sleep for `duration` unless cancellation happens first.
    ///
    /// Returns `false` when the sleep was cut short by cancellation.
    pub async fn sleep(&self, duration: Duration) -> bool {
        if duration.is_zero() {
            return !self.is_cancelled();
        }

        tokio::select! {
            biased;
            _ = self.cancelled() => false,
            _ = sleep(duration) => true,
        }
    }
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn test_cancel_is_observed_by_clones() {
        let token = Cancellation::new();
        let clone = token.clone();

        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[tokio::test]
    async fn test_sleep_returns_early_on_cancel() {
        let token = Cancellation::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            canceller.cancel();
        });

        let started = Instant::now();
        let completed = token.sleep(Duration::from_secs(30)).await;

        assert!(!completed);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_sleep_completes_without_cancel() {
        let token = Cancellation::new();
        assert!(token.sleep(Duration::from_millis(5)).await);
        assert!(token.sleep(Duration::ZERO).await);
    }

    #[tokio::test]
    async fn test_cancelled_resolves_when_already_cancelled() {
        let token = Cancellation::new();
        token.cancel();
        token.cancelled().await;
    }
}
