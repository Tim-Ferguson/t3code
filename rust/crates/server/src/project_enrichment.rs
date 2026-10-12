//! The repository identity and the favicon of each project's workspace, resolved off the request that wants them
//! (`ProjectEnrichmentService.ts`). A shell build asks for what is already known and gets that at once; a root not yet known is queued
//! for a few background workers, and the identity reaches the clients watching the shell as an enrichment frame. No shell build runs git
//! and none scans the disk for an icon.
//!
//! The service belongs to a store (`Store::extension`), so a transport handler reaches it from the server's state and a test's server gets
//! it without a field of its own. It starts its workers on first use and keeps them for the life of the runtime.
//!
//! The favicon is the file a project icon falls back on when the project names none (`ProjectFaviconResolver`); it travels as the
//! `faviconPath` of a `Project`, never of a shell (`ProjectService.toProject` against `enrichShell`).
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
/// A favicon scan checks about thirty files, and icons rarely change, so a resolved favicon is kept longer than the identity.
/// Moving a project's workspace root, and deleting it, invalidate both at once.
const FAVICON_TTL: Duration = Duration::from_secs(15 * 60);
const FAILURE_TTL: Duration = Duration::from_secs(5);

/// What finds the icon file of a workspace root: the file's absolute path, none for no icon, an error for a root it could not look in.
pub type FaviconResolver = Arc<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// `ProjectFaviconResolver.resolvePath` without a saved path.
pub fn workspace_favicon_resolver() -> FaviconResolver {
    Arc::new(|root| crate::asset_workspace::resolve_favicon_path(root, None).map_err(|cause| cause["message"].as_str().unwrap_or("The workspace root could not be read.").to_owned()))
}

/// What is known of a root.
#[derive(Debug, Clone, PartialEq)]
pub struct Enrichment {
    pub repository_identity: Option<RepositoryIdentity>,
    /// True once resolution has finished, including to no identity at all.
    pub resolved: bool,
    /// The icon file found in the workspace, once it is known and there is one.
    pub favicon_path: Option<String>,
}

/// A root whose resolution has finished.
#[derive(Debug, Clone)]
pub struct Change {
    pub workspace_root: String,
    pub repository_identity: Option<RepositoryIdentity>,
}

/// The roots one kind of work has queued and is working on, and the queue the workers take from.
struct Lane {
    pending: Mutex<HashSet<String>>,
    queue: mpsc::Sender<String>,
}

impl Lane {
    fn new() -> (Self, mpsc::Receiver<String>) {
        let (queue, receiver) = mpsc::channel(MAX_PENDING);
        (Self { pending: Mutex::new(HashSet::new()), queue }, receiver)
    }

    /// Queues a root for the workers. A root already queued, or a queue that is full, is left alone: the next shell build asks again.
    fn request(&self, root: &str, what: &str) {
        let mut pending = self.pending.lock().unwrap();
        if pending.len() >= MAX_PENDING || !pending.insert(root.to_owned()) {
            return;
        }
        if self.queue.try_send(root.to_owned()).is_err() {
            pending.remove(root);
            tracing::warn!(workspace_root = root, field = what, "Project metadata enrichment queue is full");
        }
    }
}

struct Inner {
    resolver: RepositoryIdentityResolver,
    cache: TtlCache<String, Option<RepositoryIdentity>, ()>,
    identities: Lane,
    favicon_resolver: FaviconResolver,
    favicons: TtlCache<String, Option<String>, String>,
    favicon_lane: Lane,
    favicon_changes: broadcast::Sender<String>,
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
        Self::start_with(resolver, workspace_favicon_resolver(), clock)
    }

    /// A service over any resolvers and clock.
    pub fn start_with(resolver: RepositoryIdentityResolver, favicon_resolver: FaviconResolver, clock: Arc<dyn Clock>) -> Self {
        let (identities, identity_queue) = Lane::new();
        let (favicon_lane, favicon_queue) = Lane::new();
        let inner = Arc::new(Inner {
            resolver,
            cache: TtlCache::new(512, clock.clone()),
            identities,
            favicon_resolver,
            favicons: TtlCache::new(512, clock),
            favicon_lane,
            favicon_changes: broadcast::channel(256).0,
            changes: broadcast::channel(256).0,
        });
        Self::spawn_workers(&inner, identity_queue, |inner, root| Box::pin(async move { inner.resolve(&root).await }), |inner| &inner.identities);
        Self::spawn_workers(&inner, favicon_queue, |inner, root| Box::pin(async move { inner.resolve_favicon(&root).await }), |inner| &inner.favicon_lane);
        Self { inner }
    }

    fn spawn_workers(
        inner: &Arc<Inner>,
        receiver: mpsc::Receiver<String>,
        resolve: fn(Arc<Inner>, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        lane: fn(&Inner) -> &Lane,
    ) {
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
        for _ in 0..WORKERS {
            let (inner, receiver) = (Arc::downgrade(inner), receiver.clone());
            tokio::spawn(async move {
                loop {
                    let Some(root) = receiver.lock().await.recv().await else { return };
                    let Some(inner) = inner.upgrade() else { return };
                    resolve(inner.clone(), root.clone()).await;
                    lane(&inner).pending.lock().unwrap().remove(&root);
                }
            });
        }
    }

    /// What is resolved already, without starting or awaiting anything.
    pub fn peek(&self, root: &str) -> Enrichment {
        let favicon_path = match self.inner.favicons.peek(&root.to_owned()) {
            Some(Ok(path)) => path,
            _ => None,
        };
        match self.inner.cache.peek(&root.to_owned()) {
            Some(Ok(identity)) => Enrichment { repository_identity: identity, resolved: true, favicon_path },
            _ => Enrichment { repository_identity: None, resolved: false, favicon_path },
        }
    }

    /// Queues what is not resolved, for the workers.
    pub fn request(&self, root: &str) {
        if self.inner.cache.peek(&root.to_owned()).is_none() {
            self.inner.identities.request(root, "repositoryIdentity");
        }
        if self.inner.favicons.peek(&root.to_owned()).is_none() {
            self.inner.favicon_lane.request(root, "faviconPath");
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
            self.inner.favicons.invalidate(&root.to_owned());
        }
    }

    /// Roots whose icon lookup has finished, whatever it found (the lookup is not part of `Change`, which the shell stream reads).
    pub fn subscribe_favicons(&self) -> broadcast::Receiver<String> {
        self.inner.favicon_changes.subscribe()
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

    /// Looks for the icon on a blocking thread; a root that cannot be looked in is held as a failure for a few seconds. A root that is
    /// gone (a moved or deleted checkout) fails quietly, as the original does not log it.
    async fn resolve_favicon(&self, root: &str) {
        let (resolver, owned) = (self.favicon_resolver.clone(), root.to_owned());
        let found = self
            .favicons
            .get(
                root.to_owned(),
                async move { tokio::task::spawn_blocking(move || resolver(&owned)).await.unwrap_or_else(|error| Err(error.to_string())) },
                |result| if result.is_ok() { FAVICON_TTL } else { FAILURE_TTL },
            )
            .await;
        let _ = self.favicon_changes.send(root.to_owned());
        if let Err(message) = found {
            if !message.starts_with("Workspace root does not exist") {
                tracing::warn!(workspace_root = root, field = "faviconPath", %message, "Failed to enrich optional project metadata");
            }
        }
    }
}
