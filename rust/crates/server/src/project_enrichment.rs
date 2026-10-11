//! The repository identity of each project's workspace, resolved off the request that wants it
//! (`ProjectEnrichmentService.ts`). A shell build asks for what is already known and gets that at
//! once; a root not yet known is queued for a few background workers, and the answer reaches the
//! clients watching the shell as an enrichment frame. No shell build runs git.
//!
//! The service belongs to a store (`Store::extension`), so a transport handler reaches it from the
//! server's state and a test's server gets it without a field of its own. It starts its workers on
//! first use and keeps them for the life of the runtime.
use crate::{
    persistence::Store,
    repository_identity::{RepositoryIdentityResolver, Refine, system_probe},
    vcs_cache::TtlCache,
};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::RepositoryIdentity;
use t3_vcs::{Clock, SystemClock};
use tokio::sync::{broadcast, mpsc};

const MAX_PENDING: usize = 512;
const WORKERS: usize = 4;
/// How long a resolved root is held before the next look; the resolver's own caches last longer, so
/// this is a lookup in memory.
const SUCCESS_TTL: Duration = Duration::from_secs(60);

/// What is known of a root.
#[derive(Debug, Clone, PartialEq)]
pub struct Enrichment {
    pub repository_identity: Option<RepositoryIdentity>,
    /// True once resolution has finished, including to no identity at all.
    pub resolved: bool,
}

/// A root whose resolution has finished.
#[derive(Debug, Clone)]
pub struct Change {
    pub workspace_root: String,
    pub repository_identity: Option<RepositoryIdentity>,
}

struct Inner {
    resolver: RepositoryIdentityResolver,
    cache: TtlCache<String, Option<RepositoryIdentity>, ()>,
    pending: Mutex<HashSet<String>>,
    queue: mpsc::Sender<String>,
    changes: broadcast::Sender<Change>,
}

pub struct ProjectEnrichment {
    inner: Arc<Inner>,
}

impl ProjectEnrichment {
    /// The service of this store, started on first use. `refine` is used only by the call that
    /// starts it.
    pub fn for_store(store: &Store, refine: impl FnOnce() -> Option<Refine>) -> Arc<Self> {
        store.extension(|| Self::start(RepositoryIdentityResolver::new(system_probe(), refine()), Arc::new(SystemClock)))
    }

    /// A service over any resolver and clock; tests use it to count spawns and move time.
    pub fn start(resolver: RepositoryIdentityResolver, clock: Arc<dyn Clock>) -> Self {
        let (queue, receiver) = mpsc::channel(MAX_PENDING);
        let inner = Arc::new(Inner {
            resolver,
            cache: TtlCache::new(512, clock),
            pending: Mutex::new(HashSet::new()),
            queue,
            changes: broadcast::channel(256).0,
        });
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
        for _ in 0..WORKERS {
            let (inner, receiver) = (Arc::downgrade(&inner), receiver.clone());
            tokio::spawn(async move {
                loop {
                    let Some(root) = receiver.lock().await.recv().await else { return };
                    let Some(inner) = inner.upgrade() else { return };
                    inner.resolve(&root).await;
                    inner.pending.lock().unwrap().remove(&root);
                }
            });
        }
        Self { inner }
    }

    /// What is resolved already, without starting or awaiting anything.
    pub fn peek(&self, root: &str) -> Enrichment {
        match self.inner.cache.peek(&root.to_owned()) {
            Some(Ok(identity)) => Enrichment { repository_identity: identity, resolved: true },
            _ => Enrichment { repository_identity: None, resolved: false },
        }
    }

    /// Queues a root that is not resolved, for the workers. A root already queued, or a queue that
    /// is full, is left alone: the next shell build asks again.
    pub fn request(&self, root: &str) {
        if self.inner.cache.peek(&root.to_owned()).is_some() {
            return;
        }
        let mut pending = self.inner.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING || !pending.insert(root.to_owned()) {
            return;
        }
        if self.inner.queue.try_send(root.to_owned()).is_err() {
            pending.remove(root);
            tracing::warn!(workspace_root = root, "Project metadata enrichment queue is full");
        }
    }

    /// `getAvailable`: what is resolved, and the rest queued.
    pub fn get_available(&self, root: &str) -> Enrichment {
        let available = self.peek(root);
        self.request(root);
        available
    }

    /// Forgets these roots: their project was made, moved or removed.
    pub fn invalidate<'a>(&self, roots: impl IntoIterator<Item = &'a str>) {
        for root in roots {
            self.inner.cache.invalidate(&root.to_owned());
        }
    }

    /// Roots as they finish resolving.
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.inner.changes.subscribe()
    }
}

impl Inner {
    async fn resolve(&self, root: &str) {
        let identity = self
            .cache
            .get(root.to_owned(), async { Ok(self.resolver.resolve(root, false).await) }, |_| SUCCESS_TTL)
            .await
            .ok()
            .flatten();
        let _ = self.changes.send(Change { workspace_root: root.to_owned(), repository_identity: identity });
    }
}
