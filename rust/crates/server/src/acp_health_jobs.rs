//! Owned per-instance enrichment. Generation admission also guards publication.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;
#[derive(Clone)]
struct Slot {
    generation: u64,
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}
#[derive(Default)]
struct State {
    next: u64,
    closed: bool,
    slots: HashMap<String, Slot>,
    #[cfg(test)]
    admitted: Option<tokio::sync::mpsc::UnboundedSender<String>>,
}
struct Owner(Arc<Mutex<State>>, Arc<tokio::sync::Mutex<()>>);
impl Drop for Owner {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap();
        state.closed = true;
        for slot in state.slots.values() {
            slot.stop.send_replace(true);
        }
    }
}
#[derive(Clone)]
pub(crate) struct Refreshes(Arc<Owner>);
impl Default for Refreshes {
    fn default() -> Self {
        Self(Arc::new(Owner(Arc::default(), Arc::default())))
    }
}
pub(crate) struct Lease {
    state: Arc<Mutex<State>>,
    instance: String,
    generation: u64,
    stop: watch::Receiver<bool>,
    done: watch::Sender<bool>,
    previous: Option<Slot>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if state
            .slots
            .get(&self.instance)
            .is_some_and(|slot| slot.generation == self.generation)
        {
            state.slots.remove(&self.instance);
        }
        self.done.send_replace(true);
    }
}
impl Refreshes {
    /// After the admission await, registration and transfer to an owned task
    /// happen without another await. Cancellation cannot orphan admission.
    pub(crate) async fn admit(&self, instance: &str) -> Option<Lease> {
        let _admission = self.0.1.lock().await;
        let mut state = self.0.0.lock().unwrap();
        if state.closed {
            return None;
        }
        let generation = state.next;
        state.next += 1;
        let (stop, cancellation) = watch::channel(false);
        let (done, completed) = watch::channel(false);
        let previous = state.slots.insert(
            instance.into(),
            Slot {
                generation,
                stop,
                done: completed,
            },
        );
        if let Some(previous) = &previous {
            previous.stop.send_replace(true);
        }
        #[cfg(test)]
        if let Some(admitted) = &state.admitted {
            let _ = admitted.send(instance.into());
        }
        Some(Lease {
            state: self.0.0.clone(),
            instance: instance.into(),
            generation,
            stop: cancellation,
            done,
            previous,
        })
    }
    #[cfg(test)]
    pub(crate) fn observe_admissions(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.0.0.lock().unwrap().admitted = Some(sender);
        receiver
    }
    #[cfg(test)]
    pub(crate) async fn wait(&self, instance: &str) {
        let slot = self.0.0.lock().unwrap().slots.get(instance).cloned();
        if let Some(mut slot) = slot {
            if !*slot.done.borrow() {
                let _ = slot.done.wait_for(|done| *done).await;
            }
        }
    }
    pub(crate) async fn stop_all(&self, close: bool) {
        self.stop_all_retaining(close, ()).await;
    }
    pub(crate) async fn stop_all_retaining<T: Send + 'static>(&self, close: bool, retained: T) {
        let admission = self.0.1.clone().lock_owned().await;
        let slots = {
            let mut state = self.0.0.lock().unwrap();
            state.closed |= close;
            let slots = std::mem::take(&mut state.slots);
            for slot in slots.values() {
                slot.stop.send_replace(true);
            }
            slots
        };
        // Hold both refresh admission and the caller's scope lease inside the
        // drain task, even if the caller stops waiting during native cleanup.
        let drain = tokio::spawn(async move {
            let _admission = admission;
            let _retained = retained;
            for mut slot in slots.into_values() {
                if !*slot.done.borrow() {
                    let _ = slot.done.wait_for(|done| *done).await;
                }
            }
        });
        let _ = drain.await;
    }
}
impl Lease {
    pub(crate) async fn await_previous(&mut self) {
        if let Some(mut slot) = self.previous.take() {
            if !*slot.done.borrow() {
                let _ = slot.done.wait_for(|done| *done).await;
            }
        }
    }
    pub(crate) fn stopped(&self) -> bool {
        *self.stop.borrow()
    }
    pub(crate) fn cancellation(&self) -> watch::Receiver<bool> {
        self.stop.clone()
    }
    /// Keep generation validation and publication in one critical section.
    pub(crate) fn publish(&self, publish: impl FnOnce()) -> bool {
        let state = self.state.lock().unwrap();
        if state.closed
            || self.stopped()
            || !state
                .slots
                .get(&self.instance)
                .is_some_and(|slot| slot.generation == self.generation)
        {
            return false;
        }
        publish();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_drain_retains_admission_and_scope_until_actual_job_cleanup() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let manager = Refreshes::default();
            let lease = manager.admit("agent").await.unwrap();
            let (cleanup, entered) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let job = tokio::spawn(async move {
                let mut stop = lease.cancellation();
                stop.wait_for(|stop| *stop).await.unwrap();
                cleanup.send(()).unwrap();
                released.await.unwrap();
                drop(lease);
            });
            let scope = Arc::new(());
            let retained = scope.clone();
            let stopping = manager.clone();
            let drain = tokio::spawn(async move {
                stopping.stop_all_retaining(false, retained).await;
            });
            entered.await.unwrap();
            drain.abort();
            let _ = drain.await;
            assert_eq!(
                Arc::strong_count(&scope),
                2,
                "owned native drain still retains caller scope"
            );
            let mut next = Box::pin(manager.admit("agent"));
            assert!(
                futures_util::poll!(next.as_mut()).is_pending(),
                "no new job crosses pending drain"
            );
            release.send(()).unwrap();
            job.await.unwrap();
            let lease = next.await.unwrap();
            assert_eq!(Arc::strong_count(&scope), 1);
            drop(lease);
            manager.stop_all(true).await;
            assert!(manager.admit("agent").await.is_none());
        })
        .await
        .unwrap();
    }
}
