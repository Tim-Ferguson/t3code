//! One in-flight decoded update crosses the WebView ABI. Closing the renderer
//! releases its waiter so the Effect stream guard can interrupt the server.
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
#[derive(Clone)]
pub struct FocusOwner {
    pub environment: String,
    pub thread: String,
    pub terminal: String,
}
impl FocusOwner {
    pub fn activate(
        &self,
        panes: &mut t3_client::terminal_ui::ScopedPanes,
        environment: Option<&str>,
        thread: Option<&str>,
    ) -> bool {
        if environment != Some(self.environment.as_str()) || thread != Some(self.thread.as_str()) {
            return false;
        }
        let key = t3_client::terminal_ui::ScopedPanes::key(&self.environment, &self.thread);
        let current = panes.get(&key);
        if current.active_terminal_id == self.terminal
            || !current.terminal_ids.contains(&self.terminal)
        {
            return false;
        }
        panes.set(&key, current.activate(&self.terminal));
        true
    }
}
#[derive(Default)]
pub struct SizeEpoch {
    last: Option<(i64, u64)>,
}
impl SizeEpoch {
    pub fn observe(
        &mut self,
        buffer: &t3_client::terminal_session::BufferState,
        operate: bool,
    ) -> bool {
        if !operate
            || buffer.version == 0
            || buffer.status != t3_client::terminal_session::SessionStatus::Running
        {
            return false;
        }
        let epoch = (buffer.output.generation, buffer.lifecycle_version);
        if self.last == Some(epoch) {
            return false;
        }
        self.last = Some(epoch);
        true
    }
}
#[derive(Clone, Default)]
pub struct Receipts {
    pending: Rc<RefCell<Option<(u64, futures_channel::oneshot::Sender<()>)>>>,
    next: Rc<Cell<u64>>,
    closed: Rc<Cell<bool>>,
}
impl Receipts {
    pub fn begin(&self) -> Option<(u64, futures_channel::oneshot::Receiver<()>)> {
        if self.closed.get() {
            return None;
        }
        let id = self.next.get() + 1;
        self.next.set(id);
        let (tx, rx) = futures_channel::oneshot::channel();
        *self.pending.borrow_mut() = Some((id, tx));
        Some((id, rx))
    }
    pub fn apply(&self, id: u64) {
        let mut pending = self.pending.borrow_mut();
        if pending.as_ref().is_some_and(|(key, _)| *key == id) {
            if let Some((_, sender)) = pending.take() {
                let _ = sender.send(());
            }
        }
    }
    pub fn close(&self) {
        self.closed.set(true);
        self.pending.borrow_mut().take();
    }
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn focus_selects_the_pane_to_close_and_rejects_old_destination_or_removed_pane() {
        use t3_client::terminal_ui::{Direction, ScopedPanes};
        let mut panes = ScopedPanes::default();
        for environment in ["a", "b"] {
            let key = ScopedPanes::key(environment, "thread");
            panes.upsert(&key, "first", false, Direction::Horizontal);
            panes.upsert(&key, "second", true, Direction::Horizontal);
        }
        let focus = FocusOwner {
            environment: "a".into(),
            thread: "thread".into(),
            terminal: "first".into(),
        };
        assert!(!focus.activate(&mut panes, Some("b"), Some("thread")));
        assert!(!focus.activate(&mut panes, Some("a"), Some("other")));
        assert_eq!(panes.get("b:thread").active_terminal_id, "second");
        assert!(focus.activate(&mut panes, Some("a"), Some("thread")));
        let close = panes.get("a:thread").active_terminal_id;
        panes.close("a:thread", &close);
        assert_eq!(panes.get("a:thread").terminal_ids, vec!["second"]);
        assert!(!focus.activate(&mut panes, Some("a"), Some("thread")));
        assert_eq!(panes.get("b:thread").terminal_ids, vec!["first", "second"]);
    }
    #[test]
    fn only_running_operable_snapshots_request_size_and_restarts_recovery_resend() {
        use t3_client::terminal_session::{BufferState, SessionStatus};
        let mut epoch = SizeEpoch::default();
        let mut buffer = BufferState::seed(1);
        assert!(!epoch.observe(&buffer, true));
        buffer.version = 1;
        buffer.status = SessionStatus::Running;
        assert!(!epoch.observe(&buffer, false));
        assert!(epoch.observe(&buffer, true));
        assert!(!epoch.observe(&buffer, true));
        buffer.version += 1;
        assert!(!epoch.observe(&buffer, true));
        buffer.lifecycle_version += 1;
        assert!(epoch.observe(&buffer, true));
        buffer.status = SessionStatus::Exited;
        buffer.lifecycle_version += 1;
        assert!(!epoch.observe(&buffer, true));
        buffer.status = SessionStatus::Running;
        buffer.output.generation = 2;
        assert!(epoch.observe(&buffer, true));
    }
    #[tokio::test]
    async fn renderer_failure_releases_actual_pending_consumer_receipt() {
        let receipts = Receipts::default();
        let (id, receipt) = receipts.begin().unwrap();
        receipts.apply(id + 1);
        let close = receipts.clone();
        let consumer = async move {
            assert!(receipt.await.is_err());
        };
        let failure = async move {
            tokio::task::yield_now().await;
            close.close();
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(consumer, failure);
        })
        .await
        .unwrap();
        assert!(receipts.is_closed());
        assert!(receipts.begin().is_none());
    }
    #[tokio::test]
    async fn unrelated_or_late_receipt_cannot_release_new_update() {
        let receipts = Receipts::default();
        let (old_id, old) = receipts.begin().unwrap();
        let (id, mut current) = receipts.begin().unwrap();
        assert!(old.await.is_err());
        receipts.apply(old_id);
        assert_eq!(current.try_recv().unwrap(), None);
        receipts.apply(id);
        current.await.unwrap();
    }
}

pub struct Action<O> {
    pub owner: O,
    pub method: &'static str,
    pub payload: serde_json::Value,
}
pub async fn pump<O, F, G, U>(
    mut receiver: futures_channel::mpsc::UnboundedReceiver<Action<O>>,
    mut current: F,
    mut request: G,
) where
    F: FnMut(&Action<O>) -> bool,
    G: FnMut(Action<O>) -> U,
    U: std::future::Future<Output = ()>,
{
    use futures_util::StreamExt;
    while let Some(action) = receiver.next().await {
        if current(&action) {
            request(action).await
        }
    }
}
#[cfg(test)]
mod ordering_tests {
    use super::*;
    #[tokio::test]
    async fn ordered_input_waits_for_receipt_and_drops_abandoned_destination_queue() {
        let (tx, rx) = futures_channel::mpsc::unbounded();
        for method in ["first", "second", "third"] {
            tx.unbounded_send(Action {
                owner: 1,
                method,
                payload: serde_json::Value::Null,
            })
            .unwrap();
        }
        drop(tx);
        let owner = Rc::new(Cell::new(1));
        let guard = owner.clone();
        let log = Rc::new(RefCell::new(vec![]));
        let output = log.clone();
        let (start_tx, start_rx) = futures_channel::oneshot::channel();
        let started = Rc::new(RefCell::new(Some(start_tx)));
        let (receipt_tx, receipt_rx) = futures_channel::oneshot::channel();
        let receipt = Rc::new(RefCell::new(Some(receipt_rx)));
        let consumer = pump(
            rx,
            move |action| action.owner == guard.get(),
            move |action| {
                let log = output.clone();
                let signal = started.borrow_mut().take();
                let receipt = receipt.borrow_mut().take();
                async move {
                    log.borrow_mut().push(action.method);
                    if let Some(signal) = signal {
                        let _ = signal.send(());
                    }
                    if let Some(receipt) = receipt {
                        let _ = receipt.await;
                    }
                }
            },
        );
        let switch = async {
            start_rx.await.unwrap();
            assert_eq!(*log.borrow(), vec!["first"]);
            owner.set(2);
            receipt_tx.send(()).unwrap();
        };
        tokio::join!(consumer, switch);
        assert_eq!(*log.borrow(), vec!["first"]);
    }
    #[tokio::test]
    async fn serial_input_preserves_enqueue_order_across_delayed_completion() {
        let (tx, rx) = futures_channel::mpsc::unbounded();
        let log = Rc::new(RefCell::new(vec![]));
        for method in ["a", "b", "c"] {
            tx.unbounded_send(Action {
                owner: (),
                method,
                payload: serde_json::Value::Null,
            })
            .unwrap();
        }
        drop(tx);
        let output = log.clone();
        pump(
            rx,
            |_| true,
            move |action| {
                let output = output.clone();
                async move {
                    tokio::task::yield_now().await;
                    output.borrow_mut().push(action.method)
                }
            },
        )
        .await;
        assert_eq!(*log.borrow(), vec!["a", "b", "c"]);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAttempt {
    Close,
    Exit,
}
/// Close and its compatibility fallback belong to the captured destination,
/// including error settlement after the fallback finishes.
pub async fn close_with_fallback<F, Fut>(
    mut current: impl FnMut() -> bool,
    mut request: F,
) -> Option<String>
where
    F: FnMut(CloseAttempt) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if !current() {
        return None;
    }
    match request(CloseAttempt::Close).await {
        Ok(()) => None,
        Err(cause) if cause.to_ascii_lowercase().contains("interrupt") || !current() => None,
        Err(_) => match request(CloseAttempt::Exit).await {
            Err(cause) if current() => Some(cause),
            _ => None,
        },
    }
}
#[cfg(test)]
mod close_tests {
    use super::*;
    use futures_util::{FutureExt, pin_mut};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    #[test]
    fn canceled_before_poll_never_closes_the_new_destination() {
        let called = Cell::new(false);
        let future = close_with_fallback(
            || false,
            |_| {
                called.set(true);
                std::future::ready(Ok(()))
            },
        );
        assert_eq!(future.now_or_never(), Some(None));
        assert!(!called.get());
    }
    #[test]
    fn pending_fallback_error_cannot_escape_to_the_new_destination() {
        let owned = Rc::new(Cell::new(true));
        let check = owned.clone();
        let (tx, rx) = futures_channel::oneshot::channel();
        let receiver = RefCell::new(Some(rx));
        let attempts = RefCell::new(Vec::new());
        let future = close_with_fallback(
            move || check.get(),
            |attempt| {
                attempts.borrow_mut().push(attempt);
                let rx = if attempt == CloseAttempt::Exit {
                    receiver.borrow_mut().take()
                } else {
                    None
                };
                async move {
                    if let Some(rx) = rx {
                        rx.await.unwrap()
                    } else {
                        Err("old-server close failed".into())
                    }
                }
            },
        );
        pin_mut!(future);
        assert!(future.as_mut().now_or_never().is_none());
        assert_eq!(
            *attempts.borrow(),
            [CloseAttempt::Close, CloseAttempt::Exit]
        );
        owned.set(false);
        tx.send(Err("old-server fallback failed".into())).unwrap();
        assert_eq!(future.now_or_never(), Some(None));
    }
}
