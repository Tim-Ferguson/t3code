//! The quiet window before an adapter settles a turn it held open for background work
//! (`ACP_DEFERRED_FINALIZE_DEBOUNCE`). The adapter arms it on the provider registry's idle clock, so a
//! test moves time instead of waiting; each arming is counted so the test knows when there is a window to
//! move past (`testHooks.onDeferredFinalizeScheduled`).
use std::sync::Arc;
use tokio::sync::watch;

/// How long a held turn stays quiet before it settles.
pub const DEBOUNCE_MS: i64 = 3_000;

#[derive(Clone)]
pub struct FinishDebounce {
    armed: Arc<watch::Sender<u64>>,
}
impl Default for FinishDebounce {
    fn default() -> Self {
        Self { armed: Arc::new(watch::channel(0).0) }
    }
}
impl FinishDebounce {
    /// A held turn armed its window.
    pub(crate) fn arm(&self) {
        self.armed.send_modify(|count| *count += 1);
    }
    /// How many windows have been armed so far; changes each time one is.
    pub fn armed(&self) -> watch::Receiver<u64> {
        self.armed.subscribe()
    }
}

/// Test control over when a provider's continuation run attaches to the traffic buffered for it. Open unless a
/// test closes it: a test that replays a recorded flood closes it so the run attaches only once the whole flood
/// is in, the order the original's in-memory replay produced.
#[derive(Clone)]
pub struct AttachGate {
    open: Arc<watch::Sender<bool>>,
}
impl Default for AttachGate {
    fn default() -> Self {
        Self { open: Arc::new(watch::channel(true).0) }
    }
}
impl AttachGate {
    pub fn close(&self) {
        self.open.send_replace(false);
    }
    pub fn open(&self) {
        self.open.send_replace(true);
    }
    pub(crate) fn is_open(&self) -> bool {
        *self.open.borrow()
    }
    pub(crate) async fn opened(&self) {
        let _ = self.open.subscribe().wait_for(|open| *open).await;
    }
}
