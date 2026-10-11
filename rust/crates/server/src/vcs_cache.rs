//! A small TTL cache for the VCS services: one load per key at a time, concurrent callers
//! share its result, a lifetime chosen from the loaded result (zero means "do not retain",
//! used for failures), and a capacity that evicts the oldest entry. The same behaviour as
//! `effect/Cache.makeWith` as the original uses it; `t3_vcs` keeps its own copy private.
use std::{
    collections::HashMap,
    future::Future,
    hash::Hash,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_vcs::Clock;
use tokio::sync::OnceCell;

struct Entry<V, E> {
    cell: Arc<OnceCell<Result<V, E>>>,
    /// `None` while the value is still loading.
    expires_at: Option<Duration>,
    sequence: u64,
}

pub(crate) struct TtlCache<K, V, E> {
    capacity: usize,
    clock: Arc<dyn Clock>,
    state: Mutex<(HashMap<K, Entry<V, E>>, u64)>,
}

impl<K: Hash + Eq + Clone, V: Clone, E: Clone> TtlCache<K, V, E> {
    pub(crate) fn new(capacity: usize, clock: Arc<dyn Clock>) -> Self {
        Self {
            capacity,
            clock,
            state: Mutex::new((HashMap::new(), 0)),
        }
    }

    /// The cached result for `key`, loading it with `load` when absent or expired. `ttl` is
    /// applied to the loaded result.
    ///
    /// `load` is boxed here, before the future is built. A generic `load` would be embedded in
    /// this future several times over (as the argument, the closure that captures it, and the
    /// `OnceCell` init future), and every caller's future would grow by that multiple.
    pub(crate) fn get<'a>(
        &'a self,
        key: K,
        load: impl Future<Output = Result<V, E>> + Send + 'a,
        ttl: impl Fn(&Result<V, E>) -> Duration + 'a,
    ) -> impl Future<Output = Result<V, E>> + 'a {
        self.get_boxed(key, Box::pin(load), ttl)
    }

    async fn get_boxed(
        &self,
        key: K,
        load: Pin<Box<dyn Future<Output = Result<V, E>> + Send + '_>>,
        ttl: impl Fn(&Result<V, E>) -> Duration,
    ) -> Result<V, E> {
        let cell = {
            let now = self.clock.monotonic();
            let mut state = self.state.lock().unwrap();
            let (entries, sequence) = &mut *state;
            let live = entries
                .get(&key)
                .filter(|entry| entry.expires_at.is_none_or(|expires_at| now < expires_at))
                .map(|entry| entry.cell.clone());
            match live {
                Some(cell) => cell,
                None => {
                    *sequence += 1;
                    let cell = Arc::new(OnceCell::new());
                    entries.insert(
                        key.clone(),
                        Entry {
                            cell: cell.clone(),
                            expires_at: None,
                            sequence: *sequence,
                        },
                    );
                    if entries.len() > self.capacity
                        && let Some(oldest) = entries
                            .iter()
                            .filter(|(candidate, _)| **candidate != key)
                            .min_by_key(|(_, entry)| entry.sequence)
                            .map(|(candidate, _)| candidate.clone())
                    {
                        entries.remove(&oldest);
                    }
                    cell
                }
            }
        };
        let result = cell.get_or_init(|| load).await.clone();
        let retain_for = ttl(&result);
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.0.get_mut(&key)
            && Arc::ptr_eq(&entry.cell, &cell)
        {
            if retain_for.is_zero() {
                state.0.remove(&key);
            } else if entry.expires_at.is_none() {
                entry.expires_at = Some(self.clock.monotonic() + retain_for);
            }
        }
        result
    }

    /// The result for `key` if one is held and still live, without loading anything.
    pub(crate) fn peek(&self, key: &K) -> Option<Result<V, E>> {
        let now = self.clock.monotonic();
        let state = self.state.lock().unwrap();
        state
            .0
            .get(key)
            .filter(|entry| entry.expires_at.is_none_or(|expires_at| now < expires_at))
            .and_then(|entry| entry.cell.get().cloned())
    }

    pub(crate) fn invalidate(&self, key: &K) {
        self.state.lock().unwrap().0.remove(key);
    }
}
