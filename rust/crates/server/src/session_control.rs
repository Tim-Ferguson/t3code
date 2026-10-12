//! Who may drive a server browser tab, and one at a time (apps/server/src/preview/SessionControl.ts).
//!
//! A tab belongs to one agent session and may be taken over by one viewer. Control is reserved the moment it is asked for, but running work
//! drains first: actions run in the order they were asked for, a takeover or release invalidates every action queued before it, and an
//! action that returned before its navigation committed (`track`) still holds back the next one until it has. Every method reads and reserves
//! its place when it is called, not when its future is first polled, as the original's promises do.
use futures_util::{FutureExt, future::Shared};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use t3_contracts::PreviewAutomationControlReason;
use tokio::{sync::watch, task::JoinHandle};

/// Control moved, or the caller never had it.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserControlInterrupted {
    pub message: String,
    pub reason: PreviewAutomationControlReason,
}

impl BrowserControlInterrupted {
    pub fn new(message: impl Into<String>, reason: PreviewAutomationControlReason) -> Self {
        Self { message: message.into(), reason }
    }
    /// "Browser control changed", the original's default.
    pub fn changed() -> Self {
        Self::new("Browser control changed. Refresh the snapshot before trying again.", PreviewAutomationControlReason::Interrupted)
    }
}

impl std::fmt::Display for BrowserControlInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for BrowserControlInterrupted {}

type Pending = Shared<Pin<Box<dyn Future<Output = ()> + Send>>>;

/// Tells the entry queued behind this one that it may run, however this one ended.
struct Done(watch::Sender<bool>);
impl Drop for Done {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

struct State {
    owner: Option<String>,
    epoch: u64,
    closed: bool,
    /// Resolves when the entry queued last has finished.
    tail: watch::Receiver<bool>,
    /// Work that outlives the action which started it, such as a navigation that has not committed.
    pending: Pending,
}

struct Inner {
    agent_id: Option<String>,
    on_generation_change: Box<dyn Fn() + Send + Sync>,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct SessionControl(Arc<Inner>);

fn finished() -> Pending {
    (Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>).shared()
}

async fn finished_entry(mut entry: watch::Receiver<bool>) {
    // A sender dropped without a word is an entry that was abandoned; it is finished as well.
    let _ = entry.wait_for(|done| *done).await;
}

impl SessionControl {
    /// `agent_id` is the provider session the tab belongs to. `on_generation_change` runs whenever the controller changes, so refs issued
    /// to the previous one can be revoked.
    pub fn new(agent_id: Option<String>, on_generation_change: impl Fn() + Send + Sync + 'static) -> Self {
        let (_, tail) = watch::channel(true);
        Self(Arc::new(Inner { agent_id, on_generation_change: Box::new(on_generation_change), state: Mutex::new(State { owner: None, epoch: 0, closed: false, tail, pending: finished() }) }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.0.agent_id.as_deref()
    }

    /// The viewer that controls the tab.
    pub fn controller(&self) -> Option<String> {
        self.lock().owner.clone()
    }

    pub fn generation(&self) -> u64 {
        self.lock().epoch
    }

    /// Takes a place in the queue: the returned future waits its turn, runs `run`, and lets the next entry go once the work `track` was given
    /// has finished too. The result is returned as soon as `run` is.
    fn enqueue<A, E, F, Fut>(&self, state: &mut State, run: F) -> impl Future<Output = Result<A, E>> + use<A, E, F, Fut>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<A, E>>,
    {
        let (done, entry) = watch::channel(false);
        let previous = std::mem::replace(&mut state.tail, entry);
        let control = self.clone();
        async move {
            let done = Done(done);
            finished_entry(previous).await;
            let result = run().await;
            let pending = control.lock().pending.clone();
            // The caller does not wait for the navigation, the next entry does.
            tokio::spawn(async move {
                pending.await;
                drop(done);
            });
            result
        }
    }

    fn assert_open(state: &State) -> Result<(), BrowserControlInterrupted> {
        if state.closed {
            return Err(BrowserControlInterrupted::new("This browser tab is closed.", PreviewAutomationControlReason::Closed));
        }
        Ok(())
    }

    fn change_owner(&self, state: &mut State, owner: Option<String>) {
        state.owner = owner;
        state.epoch += 1;
        (self.0.on_generation_change)();
    }

    /// Work that finishes after the action that started it returned, such as a navigation that has not committed. The next action waits for
    /// it; its failure is its own.
    pub fn track(&self, work: impl Future<Output = ()> + Send + 'static) {
        let handle: JoinHandle<()> = tokio::spawn(work);
        let settled: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
            let _ = handle.await;
        });
        let mut state = self.lock();
        let previous = state.pending.clone();
        state.pending = (Box::pin(async move {
            previous.await;
            settled.await;
        }) as Pin<Box<dyn Future<Output = ()> + Send>>)
            .shared();
    }

    /// Runs `run` in turn if `allowed` still holds when it is its turn and control has not changed since it was asked for.
    fn action<A, E, P, F, Fut>(&self, allowed: P, run: F) -> impl Future<Output = Result<A, E>> + use<A, E, P, F, Fut>
    where
        E: From<BrowserControlInterrupted>,
        P: Fn(&State) -> bool + Send + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<A, E>>,
    {
        let mut state = self.lock();
        let refusal = match Self::assert_open(&state) {
            Err(error) => Some(error),
            Ok(()) if !allowed(&state) => Some(BrowserControlInterrupted::new("You do not control this browser tab.", PreviewAutomationControlReason::Interrupted)),
            Ok(()) => None,
        };
        let epoch = state.epoch;
        let control = self.clone();
        let queued = refusal.is_none().then(|| {
            self.enqueue(&mut state, move || async move {
                let still = {
                    let state = control.lock();
                    Self::assert_open(&state).and_then(|()| if epoch != state.epoch || !allowed(&state) { Err(BrowserControlInterrupted::changed()) } else { Ok(()) })
                };
                match still {
                    Ok(()) => run().await,
                    Err(error) => Err(E::from(error)),
                }
            })
        });
        async move {
            match (refusal, queued) {
                (Some(error), _) => Err(E::from(error)),
                (None, Some(queued)) => queued.await,
                (None, None) => unreachable!("an action that was not refused is queued"),
            }
        }
    }

    /// The assigned agent's action. A different agent, or a tab a viewer controls, is refused without queueing.
    pub fn agent<A, E, F, Fut>(&self, agent_id: &str, run: F) -> impl Future<Output = Result<A, E>> + use<A, E, F, Fut>
    where
        E: From<BrowserControlInterrupted>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<A, E>>,
    {
        let refusal = if self.0.agent_id.as_deref() != Some(agent_id) {
            Some(BrowserControlInterrupted::new("This tab belongs to another agent.", PreviewAutomationControlReason::AgentMismatch))
        } else if self.lock().owner.is_some() {
            Some(BrowserControlInterrupted::new("A human controls this tab.", PreviewAutomationControlReason::HumanControl))
        } else {
            None
        };
        let agent = agent_id.to_owned();
        let inner = self.0.agent_id.clone();
        let queued = refusal.is_none().then(|| self.action(move |state| inner.as_deref() == Some(agent.as_str()) && state.owner.is_none(), run));
        async move {
            match (refusal, queued) {
                (Some(error), _) => Err(E::from(error)),
                (None, Some(queued)) => queued.await,
                (None, None) => unreachable!("an action that was not refused is queued"),
            }
        }
    }

    /// Work any client may ask for, such as a viewport or appearance change. It waits its turn behind running actions but needs no control.
    pub fn system<A, E, F, Fut>(&self, run: F) -> impl Future<Output = Result<A, E>> + use<A, E, F, Fut>
    where
        E: From<BrowserControlInterrupted>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<A, E>>,
    {
        let control = self.clone();
        let mut state = self.lock();
        let refusal = Self::assert_open(&state).err();
        let queued = refusal.is_none().then(|| {
            self.enqueue(&mut state, move || async move {
                let open = Self::assert_open(&control.lock());
                match open {
                    Ok(()) => run().await,
                    Err(error) => Err(E::from(error)),
                }
            })
        });
        async move {
            match (refusal, queued) {
                (Some(error), _) => Err(E::from(error)),
                (None, Some(queued)) => queued.await,
                (None, None) => unreachable!("an action that was not refused is queued"),
            }
        }
    }

    /// An action of the viewer that controls the tab.
    pub fn human<A, E, F, Fut>(&self, viewer_id: &str, run: F) -> impl Future<Output = Result<A, E>> + use<A, E, F, Fut>
    where
        E: From<BrowserControlInterrupted>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<A, E>>,
    {
        let viewer = viewer_id.to_owned();
        self.action(move |state| state.owner.as_deref() == Some(viewer.as_str()), run)
    }

    /// Reserves the tab for a viewer at once; resolves when the work running now has drained.
    pub fn take(&self, viewer_id: &str) -> impl Future<Output = Result<(), BrowserControlInterrupted>> + use<> {
        let refusal = {
            let mut state = self.lock();
            match Self::assert_open(&state) {
                Err(error) => Some(error),
                Ok(()) if state.owner.as_deref().is_some_and(|owner| owner != viewer_id) => Some(BrowserControlInterrupted::new("Another viewer controls this browser tab.", PreviewAutomationControlReason::Interrupted)),
                Ok(()) => {
                    if state.owner.as_deref() != Some(viewer_id) {
                        self.change_owner(&mut state, Some(viewer_id.to_owned()));
                    }
                    None
                }
            }
        };
        let drained = refusal.is_none().then(|| self.human::<(), BrowserControlInterrupted, _, _>(viewer_id, || async { Ok(()) }));
        async move {
            match (refusal, drained) {
                (Some(error), _) => Err(error),
                (None, Some(drained)) => drained.await,
                (None, None) => unreachable!("a take that was not refused drains"),
            }
        }
    }

    /// Hands the tab back to its agent at once; `after_drain` runs once the viewer's running work has drained.
    pub fn release<E, F, Fut>(&self, viewer_id: &str, after_drain: F) -> impl Future<Output = Result<(), E>> + use<E, F, Fut>
    where
        E: From<BrowserControlInterrupted>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let mut state = self.lock();
        let refusal = match Self::assert_open(&state) {
            Err(error) => Some(error),
            Ok(()) if state.owner.as_deref() != Some(viewer_id) => Some(BrowserControlInterrupted::new("Only the controlling viewer can release this browser tab.", PreviewAutomationControlReason::Interrupted)),
            Ok(()) => {
                self.change_owner(&mut state, None);
                None
            }
        };
        let control = self.clone();
        let queued = refusal.is_none().then(|| self.enqueue(&mut state, after_drain));
        drop(state);
        let _ = control;
        async move {
            match (refusal, queued) {
                (Some(error), _) => Err(E::from(error)),
                (None, Some(queued)) => queued.await,
                (None, None) => unreachable!("a release that was not refused drains"),
            }
        }
    }

    /// A viewer went away: its control ends unless it never had it or the tab is gone.
    pub fn disconnect<E, F, Fut>(&self, viewer_id: &str, after_drain: F) -> impl Future<Output = Result<(), E>> + use<E, F, Fut>
    where
        E: From<BrowserControlInterrupted>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let owns = {
            let state = self.lock();
            !state.closed && state.owner.as_deref() == Some(viewer_id)
        };
        let released = owns.then(|| self.release(viewer_id, after_drain));
        async move {
            match released {
                Some(released) => released.await,
                None => Ok(()),
            }
        }
    }

    /// Ends the tab's control for good: nothing queued or new runs, and this resolves once the running work has finished.
    pub fn close(&self) -> impl Future<Output = ()> + use<> {
        let tail = {
            let mut state = self.lock();
            if !state.closed {
                state.closed = true;
                self.change_owner(&mut state, None);
            }
            state.tail.clone()
        };
        finished_entry(tail)
    }
}

#[cfg(test)]
#[path = "session_control_tests.rs"]
mod tests;
