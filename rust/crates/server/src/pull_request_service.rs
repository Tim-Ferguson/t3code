//! The pull request service (port of `PullRequestService.ts`): which project and host a
//! reference belongs to, the shared caches in front of the host, and the wire shapes assembled
//! from what GitHub answered. Handlers decode, call one method here, and map the error.
//!
//! Every read is shared for a short while and concurrent identical reads share one request.
//! Invalidation never enumerates a cache: a key carries the epoch of its project and of its
//! pull request, so bumping an epoch strands every entry made under the old one.
use crate::{
    persistence::Store,
    pull_request_github::{self as github, GitHub, PrFailure, Reason, Target},
    pull_request_provider::{PullRequestProvider, Subject},
    source_control_detect::{RepositoryIdentityParts, detect_provider_from_remote_url, repository_selector},
    source_control_host::Host,
    source_control_provider::ProviderContext,
    source_control_registry::SourceControlRegistry,
    vcs_cache::TtlCache,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use t3_contracts::*;
use t3_vcs::{Clock, remote_refs::normalize_git_remote_url};

const DETAIL_CACHE_TTL: Duration = Duration::from_secs(60);
const MERGED_DETAIL_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const CHECKS_CACHE_TTL: Duration = Duration::from_secs(15);
const DIFF_CACHE_TTL: Duration = Duration::from_secs(60);
const COMMIT_DIFF_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const FILES_VIEWED_CACHE_TTL: Duration = Duration::from_secs(15);
const VIEWER_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const STALE_DETAIL_WINDOW: Duration = Duration::from_secs(10 * 60);
const IDENTITY_POSITIVE_TTL: Duration = Duration::from_secs(15 * 60);
const IDENTITY_NEGATIVE_TTL: Duration = Duration::from_secs(60);
const DETAIL_CACHE_CAPACITY: usize = 128;
const REF_EPOCH_CAPACITY: usize = 2_048;
const MAX_CACHED_DIFF_PATCH_BYTES: usize = 512 * 1024;

fn detail_ttl(state: Option<PullRequestState>) -> Duration {
    if state == Some(PullRequestState::Merged) { MERGED_DETAIL_CACHE_TTL } else { DETAIL_CACHE_TTL }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// `PullRequestError`: the host cannot be read at all, or one operation failed.
#[derive(Debug, Clone)]
pub enum PrError {
    Unavailable(PullRequestUnavailableError),
    Operation { error: PullRequestOperationError, reason: Option<Reason> },
}

impl PrError {
    pub fn operation(operation: &str, detail: impl Into<String>) -> Self {
        Self::Operation { error: PullRequestOperationError::new(operation, detail), reason: None }
    }
    pub fn unsupported() -> Self {
        Self::Unavailable(PullRequestUnavailableError { reason: PullRequestUnavailableReason::ProviderUnsupported, provider: None })
    }
    pub fn wire(&self) -> Value {
        match self {
            Self::Unavailable(error) => error.wire(),
            Self::Operation { error, .. } => error.wire(),
        }
    }
    /// A host that cannot be read at all, as opposed to one request that failed.
    pub fn from_failure(operation: &str, failure: PrFailure, kind: SourceControlProviderKind) -> Self {
        match failure.reason {
            Reason::MissingTool => Self::Unavailable(PullRequestUnavailableError {
                reason: PullRequestUnavailableReason::CliMissing,
                provider: Some(kind),
            }),
            Reason::Unauthenticated => Self::Unavailable(PullRequestUnavailableError {
                reason: PullRequestUnavailableReason::CliUnauthenticated,
                provider: Some(kind),
            }),
            reason => Self::Operation {
                error: PullRequestOperationError {
                    operation: operation.into(),
                    detail: failure.detail,
                    not_found: reason == Reason::NotFound,
                },
                reason: Some(reason),
            },
        }
    }
}

pub type PrResult<T> = Result<T, PrError>;

/// Why a host is not readable, told as the thing to do about it.
pub fn provider_detail(failure: &PrFailure, kind: SourceControlProviderKind) -> String {
    let reason = match failure.reason {
        Reason::MissingTool => PullRequestUnavailableReason::CliMissing,
        Reason::Unauthenticated => PullRequestUnavailableReason::CliUnauthenticated,
        _ => return failure.detail.clone(),
    };
    pull_request_provider_requirement(kind, reason).map_or_else(|| failure.detail.clone(), str::to_string)
}

// ---------------------------------------------------------------------------------------------
// Projects and repository identity
// ---------------------------------------------------------------------------------------------

/// One thing a call acts through: the process host (settings of the request) and the store.
#[derive(Clone)]
pub struct Env {
    pub host: Host,
    pub store: Store,
}

#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub title: String,
    pub workspace_root: String,
}

/// What a checkout's primary remote says about its repository (`RepositoryIdentity`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub kind: SourceControlProviderKind,
    pub canonical_key: String,
    pub remote_url: String,
    pub display_name: Option<String>,
    pub owner: Option<String>,
    pub name: Option<String>,
}

impl Identity {
    pub fn repository_selector(&self) -> Option<String> {
        repository_selector(Some(&RepositoryIdentityParts {
            provider: Some(self.kind),
            display_name: self.display_name.clone(),
            owner: self.owner.clone(),
            name: self.name.clone(),
        }))
    }

    /// `pullRequestHostOf`: the host below which the repository is addressed.
    pub fn host(&self) -> String {
        if self.kind == SourceControlProviderKind::Forgejo {
            if let Ok(url) = url::Url::parse(&self.remote_url) {
                if matches!(url.scheme(), "http" | "https") {
                    return crate::source_control_detect::js_host(&url).to_lowercase();
                }
            }
        }
        match self.canonical_key.split('/').next().map(str::trim) {
            Some(host) if !host.is_empty() => host.to_lowercase(),
            _ => self.kind.as_str().to_string(),
        }
    }
}

/// `pickPrimaryRemote`: upstream, then origin, then the first by name.
fn pick_primary_remote(remotes: &[t3_vcs::git::VcsRemote]) -> Option<&t3_vcs::git::VcsRemote> {
    for preferred in ["upstream", "origin"] {
        if let Some(remote) = remotes.iter().find(|remote| remote.name == preferred) {
            return Some(remote);
        }
    }
    remotes.iter().min_by(|left, right| left.name.cmp(&right.name))
}

/// A project this page can read: its remote is on a host with an implementation.
#[derive(Debug, Clone)]
pub struct Supported {
    pub cursor_key: String,
    pub project: ProjectInfo,
    pub kind: SourceControlProviderKind,
    pub repository: String,
    pub host: String,
    pub remote: String,
}

pub fn list_cursor_key(host: &str, repository: &str) -> String {
    format!("{host} {}", repository.to_lowercase())
}

fn is_supported(kind: SourceControlProviderKind) -> bool {
    crate::pull_request_registry::supported(kind)
}

// ---------------------------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------------------------

/// A reference resolved to the project that serves it, and the key its reads are cached under.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub project: Supported,
    pub number: u64,
    pub input: PullRequestRef,
    pub namespace: Option<String>,
}

impl Resolved {
    pub fn target(&self) -> Target {
        Target { host: self.project.host.clone(), repository: self.project.repository.clone(), number: self.number }
    }

    /// The change request as a provider is asked about it.
    pub fn subject(&self) -> Subject {
        Subject {
            cwd: self.project.project.workspace_root.clone(),
            host: self.project.host.clone(),
            repository: self.project.repository.clone(),
            number: self.number,
        }
    }
}

#[derive(Default)]
pub(crate) struct Epochs {
    pub(crate) counter: u64,
    pub(crate) listings: u64,
    refs: HashMap<String, u64>,
    files_viewed: HashMap<String, u64>,
    projects: HashMap<String, u64>,
    project_floor: u64,
    refs_order: Vec<String>,
}

impl Epochs {
    fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }
    fn ref_epoch(&self, project_id: &str, scope: &str) -> u64 {
        self.projects.get(project_id).copied().unwrap_or(self.project_floor).max(self.refs.get(scope).copied().unwrap_or(0))
    }
    fn bump(&mut self, scope: String) {
        let epoch = self.next();
        if !self.refs.contains_key(&scope) && self.refs.len() >= REF_EPOCH_CAPACITY {
            if !self.refs_order.is_empty() {
                let oldest = self.refs_order.remove(0);
                self.refs.remove(&oldest);
            }
        }
        if !self.refs.contains_key(&scope) {
            self.refs_order.push(scope.clone());
        }
        self.refs.insert(scope, epoch);
    }
}

fn ref_scope(project_id: &str, host: &str, repository: &str, number: u64) -> String {
    json!([project_id, host.to_lowercase(), repository.to_lowercase(), number]).to_string()
}

/// The newest reading of a pull request wins; merged is final (`supersedesReading`).
fn supersedes(current: Option<(&str, &str, Option<i64>)>, next: (PullRequestState, &str, Option<i64>)) -> bool {
    let Some((current_state, current_updated, current_observed)) = current else { return true };
    if current_state == "merged" && next.0 != PullRequestState::Merged {
        return false;
    }
    if next.1 != current_updated {
        return next.1 > current_updated;
    }
    next.2.unwrap_or(i64::MIN) >= current_observed.unwrap_or(i64::MIN)
}

fn state_name(state: PullRequestState) -> &'static str {
    match state {
        PullRequestState::Open => "open",
        PullRequestState::Closed => "closed",
        PullRequestState::Merged => "merged",
    }
}

/// A held reading with the time it was taken.
struct Held<T> {
    at: Duration,
    value: T,
}

struct LastGood<T> {
    capacity: usize,
    entries: Mutex<(HashMap<String, Held<T>>, Vec<String>)>,
}

impl<T: Clone> LastGood<T> {
    fn new(capacity: usize) -> Self {
        Self { capacity, entries: Mutex::new((HashMap::new(), vec![])) }
    }
    fn peek(&self, key: &str) -> Option<(Duration, T)> {
        self.entries.lock().unwrap().0.get(key).map(|held| (held.at, held.value.clone()))
    }
    fn record(&self, key: &str, at: Duration, value: T) {
        let mut guard = self.entries.lock().unwrap();
        let (map, order) = &mut *guard;
        if map.remove(key).is_some() {
            order.retain(|existing| existing != key);
        }
        if map.len() >= self.capacity && !order.is_empty() {
            let oldest = order.remove(0);
            map.remove(&oldest);
        }
        map.insert(key.to_string(), Held { at, value });
        order.push(key.to_string());
    }
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

pub struct PullRequestService {
    pub(crate) clock: Arc<dyn Clock>,
    /// The pause every host but GitHub is held to after a rate-limited answer.
    limits: Arc<crate::source_control_rate_limit::RateLimit>,
    /// What an environment-backed viewed-files record holds between reads and presses.
    pub(crate) viewed: crate::pull_request_viewed::ViewedState,
    pub(crate) lists: TtlCache<String, PullRequestListResult, PrError>,
    pub(crate) recent_stats: Mutex<HashMap<String, (Duration, PullRequestDiffStat)>>,
    identities: TtlCache<String, Option<Identity>, ()>,
    viewers: Mutex<HashMap<String, (Duration, String)>>,
    routing: Mutex<HashMap<String, (Duration, (String, String))>>,
    /// When each repository last appeared in a host search, which is how a listing knows an empty
    /// answer for it is real and not the host failing to index it.
    pub(crate) search_visible_at: Mutex<HashMap<String, Duration>>,
    pub(crate) epochs: Mutex<Epochs>,
    refreshes: tokio::sync::watch::Sender<u64>,
    merges: tokio::sync::broadcast::Sender<crate::pull_request_actions::PullRequestMergeEvent>,
    details: TtlCache<String, PullRequestDetail, PrError>,
    summaries: TtlCache<String, PullRequestSummary, PrError>,
    previews: TtlCache<String, PullRequestPreview, PrError>,
    activities: TtlCache<String, PullRequestActivity, PrError>,
    checks: TtlCache<String, PullRequestChecks, PrError>,
    diffs: TtlCache<String, PullRequestDiffResult, PrError>,
    files_viewed: TtlCache<String, PullRequestFilesViewedResult, PrError>,
    stacks: TtlCache<String, Option<PullRequestStack>, PrError>,
    last_summary: LastGood<PullRequestSummary>,
    last_detail: LastGood<PullRequestDetail>,
}

impl PullRequestService {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            limits: Arc::default(),
            viewed: Default::default(),
            identities: TtlCache::new(512, clock.clone()),
            lists: TtlCache::new(64, clock.clone()),
            recent_stats: Mutex::default(),
            viewers: Mutex::default(),
            routing: Mutex::default(),
            search_visible_at: Mutex::default(),
            epochs: Mutex::default(),
            refreshes: tokio::sync::watch::channel(0).0,
            merges: tokio::sync::broadcast::channel(64).0,
            details: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            summaries: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            previews: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            activities: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            checks: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            diffs: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            files_viewed: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            stacks: TtlCache::new(DETAIL_CACHE_CAPACITY, clock.clone()),
            last_summary: LastGood::new(DETAIL_CACHE_CAPACITY),
            last_detail: LastGood::new(DETAIL_CACHE_CAPACITY),
            clock,
        }
    }

    /// The process's service, on the process host's clock.
    pub fn shared() -> &'static Arc<Self> {
        static SHARED: OnceLock<Arc<PullRequestService>> = OnceLock::new();
        SHARED.get_or_init(|| Arc::new(Self::new(Host::process_default().clock().clone())))
    }

    pub(crate) fn now(&self) -> Duration {
        self.clock.monotonic()
    }

    pub(crate) fn now_ms(&self) -> i64 {
        self.clock.utc_now().timestamp_millis()
    }

    // --- projects ------------------------------------------------------------------------

    async fn projects(&self, env: &Env) -> Vec<ProjectInfo> {
        let store = env.store.clone();
        tokio::task::spawn_blocking(move || {
            store
                .projections("project")
                .unwrap_or_default()
                .into_iter()
                .filter(|row| row["deletedAt"].is_null())
                .filter_map(|row| {
                    Some(ProjectInfo {
                        id: row["projectId"].as_str()?.parse().ok()?,
                        title: row["title"].as_str().unwrap_or("").to_string(),
                        workspace_root: row["workspaceRoot"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// What the primary remote of a checkout says it is, or `None` without a git remote.
    pub async fn identity(&self, env: &Env, workspace_root: &str) -> Option<Identity> {
        let ttl = |result: &Result<Option<Identity>, ()>| match result {
            Ok(Some(_)) => IDENTITY_POSITIVE_TTL,
            _ => IDENTITY_NEGATIVE_TTL,
        };
        self.identities
            .get(workspace_root.to_string(), Box::pin(read_identity(env.host.clone(), workspace_root.to_string())), ttl)
            .await
            .ok()
            .flatten()
    }

    pub(crate) async fn supported_projects(&self, env: &Env, only: impl Fn(&ProjectInfo) -> bool) -> Vec<Supported> {
        self.workspace_projects(env, only, None).await.0
    }

    /// What the workspace has, split by whether this server can read it. Hosts it cannot read are
    /// counted by host rather than dropped, so their projects are explained in the provider list
    /// instead of quietly missing from the page.
    pub(crate) async fn workspace_projects(
        &self,
        env: &Env,
        only: impl Fn(&ProjectInfo) -> bool,
        host_filter: Option<&str>,
    ) -> (Vec<Supported>, BTreeMap<String, (SourceControlProviderKind, u64)>) {
        let mut supported = vec![];
        let mut unimplemented: BTreeMap<String, (SourceControlProviderKind, u64)> = BTreeMap::new();
        let mut seen = std::collections::HashSet::new();
        for project in self.projects(env).await.into_iter().filter(|project| only(project)) {
            let Some(identity) = self.identity(env, &project.workspace_root).await else { continue };
            let Some(repository) = identity.repository_selector() else { continue };
            // Worktrees of one repository are separate projects; reading the remote once keeps
            // the page from repeating every change request per local checkout.
            let host = identity.host();
            if host_filter.is_some_and(|wanted| !wanted.eq_ignore_ascii_case(&host)) {
                continue;
            }
            let key = list_cursor_key(&host, &repository);
            if !seen.insert(key.clone()) {
                continue;
            }
            if !is_supported(identity.kind) {
                unimplemented.entry(host).or_insert((identity.kind, 0)).1 += 1;
                continue;
            }
            supported.push(Supported {
                cursor_key: key,
                project,
                kind: identity.kind,
                repository: repository.clone(),
                host: host.clone(),
                remote: normalize_git_remote_url(&format!("https://{host}/{repository}")),
            });
        }
        (supported, unimplemented)
    }

    /// The project whose checkout and credentials serve a reference. The project's own
    /// repository is the default; a reference that names a `host` may instead point at any
    /// repository on that host.
    pub async fn require_project(&self, env: &Env, reference: &PullRequestRef) -> PrResult<Supported> {
        let project_id = reference.project_id.as_str().to_string();
        let own = self.supported_projects(env, |project| project.id.as_str() == project_id).await.into_iter().next();
        let repository = reference.repository.as_str().trim().to_string();
        let host = reference.host.as_ref().map(|host| host.as_str().trim().to_lowercase());
        if let Some(own) = &own {
            if own.repository.eq_ignore_ascii_case(&repository) && host.as_deref().is_none_or(|host| host == own.host) {
                return Ok(own.clone());
            }
        }
        let Some(host) = host else {
            return Err(match own {
                None => PrError::unsupported(),
                // The repository travels through the client, so it is checked against the
                // project's own remote rather than being handed to a provider verbatim.
                Some(_) => PrError::operation("resolveRepository", "The change request does not belong to the selected project."),
            });
        };
        let on_host: Vec<Supported> = self.supported_projects(env, |_| true).await.into_iter().filter(|candidate| candidate.host == host).collect();
        let route = on_host
            .iter()
            .find(|candidate| candidate.repository.eq_ignore_ascii_case(&repository))
            .or_else(|| on_host.first())
            .cloned();
        let Some(route) = route else { return Err(PrError::unsupported()) };
        Ok(if route.repository.eq_ignore_ascii_case(&repository) {
            route
        } else {
            let remote = normalize_git_remote_url(&format!("https://{host}/{repository}"));
            Supported { repository, remote, ..route }
        })
    }

    pub(crate) async fn resolve(&self, env: &Env, reference: &PullRequestRef, namespace: Option<String>) -> PrResult<Resolved> {
        let project = self.require_project(env, reference).await?;
        Ok(Resolved { project, number: reference.number, input: reference.clone(), namespace })
    }

    pub(crate) fn key(&self, resolved: &Resolved) -> String {
        let scope = ref_scope(resolved.project.project.id.as_str(), &resolved.project.host, &resolved.project.repository, resolved.number);
        let epoch = self.epochs.lock().unwrap().ref_epoch(resolved.project.project.id.as_str(), &scope);
        json!([
            epoch,
            resolved.project.project.id,
            resolved.input.host.as_ref().map(|host| host.as_str().to_lowercase()),
            resolved.project.repository.to_lowercase(),
            resolved.number,
            resolved.input.expected_account_id.as_ref().map(|id| id.as_str()),
            resolved.namespace,
        ])
        .to_string()
    }

    fn scope_of(&self, resolved: &Resolved) -> String {
        ref_scope(resolved.project.project.id.as_str(), &resolved.project.host, &resolved.project.repository, resolved.number)
    }

    // --- viewer and routing --------------------------------------------------------------

    /// Who is signed in on a host; a success is believed for ten minutes, a failure never.
    pub(crate) async fn viewer_of(&self, env: &Env, kind: SourceControlProviderKind, cwd: &str, host: &str) -> Result<String, PrFailure> {
        let held = format!("{kind}\0{host}");
        {
            let viewers = self.viewers.lock().unwrap();
            if let Some((at, viewer)) = viewers.get(&held) {
                if self.now().saturating_sub(*at) <= VIEWER_CACHE_TTL {
                    return Ok(viewer.clone());
                }
            }
        }
        let provider = self.provider(env, kind).ok_or_else(|| PrFailure::failed("This host cannot be read."))?;
        let viewer = provider.viewer(cwd, host).await?;
        self.viewers.lock().unwrap().insert(held, (self.now(), viewer.clone()));
        Ok(viewer)
    }

    /// The provider that serves a kind of host, or none for a host with no implementation.
    pub(crate) fn provider(&self, env: &Env, kind: SourceControlProviderKind) -> Option<Arc<dyn PullRequestProvider>> {
        crate::pull_request_registry::provider(&env.host, kind, &self.limits, &self.clock)
    }

    pub(crate) fn provider_of(&self, env: &Env, resolved: &Resolved) -> PrResult<Arc<dyn PullRequestProvider>> {
        self.provider(env, resolved.project.kind).ok_or_else(PrError::unsupported)
    }

    /// The signed-in account and the credential namespace its reads are cached under.
    async fn account(&self, env: &Env, gh_host: &str) -> Result<(String, String, String), PrFailure> {
        let (_, fingerprint) = crate::source_control_github_api::credential(&env.host, gh_host).await?;
        {
            let routing = self.routing.lock().unwrap();
            if let Some((at, (account, viewer))) = routing.get(&fingerprint) {
                if self.now().saturating_sub(*at) < VIEWER_CACHE_TTL {
                    return Ok((account.clone(), viewer.clone(), fingerprint));
                }
            }
        }
        let (token, _) = crate::source_control_github_api::credential(&env.host, gh_host).await?;
        // Pinned so an auth switch cannot poison this cache entry.
        let pinned = crate::source_control_github_api::PinnedGitHubCredential {
            host: gh_host.to_string(),
            token,
            credential_fingerprint: fingerprint.clone(),
        };
        let identity = crate::source_control_github_api::with_pinned_credential(
            pinned,
            crate::source_control_github_api::with_credential_scope(fingerprint.clone(), github_for(env).routing_identity(gh_host)),
        )
        .await?;
        let mut routing = self.routing.lock().unwrap();
        if routing.len() >= 128 {
            if let Some(oldest) = routing.keys().next().cloned() {
                routing.remove(&oldest);
            }
        }
        routing.insert(fingerprint.clone(), (self.now(), identity.clone()));
        Ok((identity.0, identity.1, fingerprint))
    }

    /// `withRoutingCredential`: an operation asked for a particular account runs only if that
    /// account still owns the active credential, and under that credential alone.
    pub async fn routed<'a, T: Send + 'a>(
        &self,
        env: &Env,
        reference: &PullRequestRef,
        operation: impl FnOnce(Option<String>) -> BoxFuture<'a, PrResult<T>> + Send + 'a,
    ) -> PrResult<T> {
        let Some(expected) = &reference.expected_account_id else { return operation(None).await };
        let rejected = || PrError::operation("routeIdentity", "The GitHub account could not be verified before starting the operation.");
        let project = self.require_project(env, reference).await.map_err(|_| rejected())?;
        if project.kind != SourceControlProviderKind::Github
            || reference.host.as_ref().map(|host| host.as_str().to_lowercase()) != Some(project.host.to_lowercase())
        {
            return Err(rejected());
        }
        let (account, _viewer, fingerprint) = self.account(env, &project.host).await.map_err(|_| rejected())?;
        if account != expected.as_str() {
            return Err(rejected());
        }
        let (token, _) = crate::source_control_github_api::credential(&env.host, &project.host).await.map_err(|_| rejected())?;
        let pinned = crate::source_control_github_api::PinnedGitHubCredential {
            host: project.host.clone(),
            token,
            credential_fingerprint: fingerprint.clone(),
        };
        crate::source_control_github_api::with_pinned_credential(
            pinned,
            crate::source_control_github_api::with_credential_scope(fingerprint.clone(), operation(Some(fingerprint))),
        )
        .await
    }

    pub async fn routing_identity(&self, env: &Env, input: &PullRequestRoutingIdentityInput) -> PrResult<PullRequestRoutingIdentityResult> {
        let host = input.host.as_str().to_lowercase();
        let project = self.supported_projects(env, |_| true).await.into_iter().find(|candidate| candidate.host == host && candidate.kind == SourceControlProviderKind::Github);
        let Some(_) = project else { return Err(PrError::unsupported()) };
        let (account_id, viewer, _) = self
            .account(env, &host)
            .await
            .map_err(|failure| PrError::from_failure("routeIdentity", failure, SourceControlProviderKind::Github))?;
        Ok(PullRequestRoutingIdentityResult { account_id, host, provider: SourceControlProviderKind::Github, viewer })
    }

    pub async fn routing(&self, env: &Env, reference: &PullRequestRef) -> PrResult<PullRequestRoutingResult> {
        let project = self.require_project(env, reference).await?;
        if project.kind != SourceControlProviderKind::Github {
            return Err(PrError::unsupported());
        }
        let (account_id, viewer, _) = self
            .account(env, &project.host)
            .await
            .map_err(|failure| PrError::from_failure("routeIdentity", failure, project.kind))?;
        if viewer.trim().is_empty() || account_id.trim().is_empty() {
            return Err(PrError::operation("routeIdentity", "The signed-in account could not be verified."));
        }
        Ok(PullRequestRoutingResult {
            account_id,
            host: project.host,
            provider: project.kind,
            viewer,
            project_title: project.project.title,
            workspace_root: project.project.workspace_root,
        })
    }

    // --- reads ---------------------------------------------------------------------------

    pub async fn summary(&self, env: &Env, reference: &PullRequestRef, namespace: Option<String>, recover_transient_failure: bool) -> PrResult<PullRequestSummary> {
        let resolved = self.resolve(env, reference, namespace).await?;
        let key = self.key(&resolved);
        // A change request already read does not wait on the host again.
        if let Some((_, held)) = self.last_summary.peek(&key) {
            if reference.allow_stale != Some(false) && (recover_transient_failure || held.state == PullRequestState::Merged) {
                return Ok(held);
            }
        }
        let summary = self
            .summaries
            .get(key.clone(), Box::pin(self.summary_uncached(env.clone(), resolved.clone())), |result| {
                if result.is_ok() { DETAIL_CACHE_TTL } else { Duration::ZERO }
            })
            .await?;
        self.record_summary(&key, &summary);
        Ok(summary)
    }

    fn record_summary(&self, key: &str, summary: &PullRequestSummary) {
        let held = self.last_summary.peek(key);
        let current = held.as_ref().map(|(_, held)| (state_name(held.state), held.updated_at.as_str(), held.observed_at));
        if supersedes(current, (summary.state, &summary.updated_at, summary.observed_at)) {
            self.last_summary.record(key, self.now(), summary.clone());
        }
    }

    pub(crate) async fn summary_uncached(&self, env: Env, resolved: Resolved) -> PrResult<PullRequestSummary> {
        let observed_at = self.now_ms();
        let provider = self.provider_of(&env, &resolved)?;
        let read = provider.summary(&resolved.subject()).await.map_err(|failure| PrError::from_failure("summary", failure, resolved.project.kind))?;
        Ok(PullRequestSummary {
            provider: resolved.project.kind,
            project_id: resolved.project.project.id.clone(),
            repository: resolved.project.repository.clone(),
            number: read.number,
            title: read.title,
            url: read.url,
            state: read.state,
            is_draft: read.is_draft,
            head_branch: read.head_branch,
            base_branch: read.base_branch,
            closed_at: Some(read.closed_at),
            merged_at: Some(read.merged_at),
            updated_at: read.updated_at,
            observed_at: Some(observed_at),
            author: read.author,
            additions: read.additions,
            deletions: read.deletions,
            changed_files: read.changed_files,
            review_decision: read.review_decision,
            checks_state: read.checks_state,
            mergeability: read.mergeability,
            stack: read.stack,
        })
    }

    pub async fn detail(self: &Arc<Self>, env: &Env, reference: &PullRequestRef, namespace: Option<String>) -> PrResult<PullRequestDetail> {
        let resolved = self.resolve(env, reference, namespace).await?;
        let key = self.key(&resolved);
        // Held readings answer at once and refresh behind the reader, unless a fresh read is asked for.
        if reference.allow_stale != Some(false) {
            if let Some((_, held)) = self.last_detail.peek(&key) {
                let (service, env, key) = (self.clone(), env.clone(), key);
                tokio::spawn(async move {
                    let _ = Box::pin(service.detail_fresh(&env, resolved, key)).await;
                });
                return Ok(held);
            }
        }
        self.detail_fresh(env, resolved, key).await
    }

    async fn detail_fresh(&self, env: &Env, resolved: Resolved, key: String) -> PrResult<PullRequestDetail> {
        let read = self
            .details
            .get(key.clone(), Box::pin(self.detail_uncached(env.clone(), resolved.clone())), |result| match result {
                Ok(detail) => detail_ttl(Some(detail.state)),
                Err(_) => Duration::ZERO,
            })
            .await;
        match read {
            Ok(detail) => {
                let summary = summary_from_detail(&detail, self.last_summary.peek(&key).map(|(_, held)| held));
                self.record_summary(&key, &summary);
                self.last_detail.record(&key, self.now(), detail.clone());
                Ok(detail)
            }
            // A transient failure with a recent reading in hand keeps the page up.
            Err(PrError::Operation { reason: Some(Reason::Failed | Reason::RateLimited), .. }) if self.fresh_enough(&key) => {
                self.last_detail.peek(&key).map(|(_, held)| held).ok_or_else(|| PrError::operation("detail", "unavailable"))
            }
            Err(error) => Err(error),
        }
    }

    fn fresh_enough(&self, key: &str) -> bool {
        self.last_detail.peek(key).is_some_and(|(at, _)| self.now().saturating_sub(at) <= STALE_DETAIL_WINDOW)
    }

    async fn detail_uncached(&self, env: Env, resolved: Resolved) -> PrResult<PullRequestDetail> {
        let observed_at = self.now_ms();
        let kind = resolved.project.kind;
        let provider = self.provider_of(&env, &resolved)?;
        let subject = resolved.subject();
        let (read, viewer) = tokio::join!(provider.detail(&subject), self.viewer_of(&env, kind, &subject.cwd, &resolved.project.host));
        let read = read.map_err(|failure| PrError::from_failure("detail", failure, kind))?;
        let viewer = viewer.ok().filter(|viewer| !viewer.trim().is_empty());
        let item = read.row;
        Ok(PullRequestDetail {
            provider: kind,
            capabilities: provider.capabilities(),
            viewer_permissions: read.viewer_permissions,
            project_id: resolved.project.project.id.clone(),
            project_title: resolved.project.project.title.clone(),
            workspace_root: resolved.project.project.workspace_root.clone(),
            repository: resolved.project.repository.clone(),
            number: item.number,
            title: item.title,
            body: read.body,
            url: item.url,
            author: item.author,
            state: item.state,
            is_draft: item.is_draft,
            mergeability: item.mergeability,
            additions: item.additions,
            deletions: item.deletions,
            changed_files: read.changed_files,
            head_branch: item.head_branch,
            head_repository_name_with_owner: item.head_repository_name_with_owner,
            head_sha: read.head_sha.filter(|sha| !sha.is_empty()),
            base_branch: item.base_branch,
            created_at: item.created_at,
            updated_at: item.updated_at,
            observed_at: Some(observed_at),
            merged_at: read.merged_at,
            closed_at: read.closed_at,
            reviewers: read.reviewers,
            labels: item.labels,
            checks: read.checks,
            merge_capabilities: read.merge_capabilities,
            viewer,
            base_comparison: read.base_comparison,
            behind_by: read.behind_by,
            auto_merge_enabled: read.auto_merge_enabled,
            auto_merge_method: read.auto_merge_method,
            workflow_approvals_required: read.workflow_approvals_required,
        })
    }

    pub async fn preview(&self, env: &Env, reference: &PullRequestRef, namespace: Option<String>) -> PrResult<PullRequestPreview> {
        let resolved = self.resolve(env, reference, namespace).await?;
        let key = self.key(&resolved);
        let preview_of = |detail: &PullRequestDetail| PullRequestPreview {
            project_id: detail.project_id.clone(),
            repository: detail.repository.clone(),
            number: detail.number,
            title: detail.title.clone(),
            url: detail.url.clone(),
            author: detail.author.clone(),
            state: detail.state,
            is_draft: detail.is_draft,
            created_at: detail.created_at.clone(),
        };
        // A detail read in the last minute already holds everything the card shows.
        if let Some((at, held)) = self.last_detail.peek(&key) {
            if self.now().saturating_sub(at) < detail_ttl(Some(held.state)) {
                return Ok(preview_of(&held));
            }
        }
        self.previews
            .get(key, Box::pin(self.preview_uncached(env.clone(), resolved)), |result| match result {
                Ok(preview) => detail_ttl(Some(preview.state)),
                Err(_) => Duration::ZERO,
            })
            .await
    }

    async fn preview_uncached(&self, env: Env, resolved: Resolved) -> PrResult<PullRequestPreview> {
        let provider = self.provider_of(&env, &resolved)?;
        let read = provider.preview(&resolved.subject()).await.map_err(|failure| PrError::from_failure("preview", failure, resolved.project.kind))?;
        Ok(PullRequestPreview {
            project_id: resolved.project.project.id.clone(),
            repository: resolved.project.repository.clone(),
            number: read.number,
            title: read.title,
            url: read.url,
            author: read.author,
            state: read.state,
            is_draft: read.is_draft,
            created_at: read.created_at,
        })
    }

    pub async fn checks(&self, env: &Env, reference: &PullRequestRef, namespace: Option<String>) -> PrResult<PullRequestChecks> {
        let resolved = self.resolve(env, reference, namespace).await?;
        let key = self.key(&resolved);
        self.checks
            .get(key, Box::pin(self.checks_uncached(env.clone(), resolved)), |result| if result.is_ok() { CHECKS_CACHE_TTL } else { Duration::ZERO })
            .await
    }

    async fn checks_uncached(&self, env: Env, resolved: Resolved) -> PrResult<PullRequestChecks> {
        let provider = self.provider_of(&env, &resolved)?;
        provider.checks(&resolved.subject()).await.map_err(|failure| PrError::from_failure("checks", failure, resolved.project.kind))
    }

    pub async fn activity(&self, env: &Env, reference: &PullRequestRef, namespace: Option<String>) -> PrResult<PullRequestActivity> {
        let resolved = self.resolve(env, reference, namespace).await?;
        let key = self.key(&resolved);
        let state = self.last_summary.peek(&key).map(|(_, held)| held.state);
        let ttl = detail_ttl(state);
        self.activities
            .get(key, Box::pin(self.activity_uncached(env.clone(), resolved)), move |result| if result.is_ok() { ttl } else { Duration::ZERO })
            .await
    }

    async fn activity_uncached(&self, env: Env, resolved: Resolved) -> PrResult<PullRequestActivity> {
        let provider = self.provider_of(&env, &resolved)?;
        provider.activity(&resolved.subject()).await.map_err(|failure| PrError::from_failure("activity", failure, resolved.project.kind))
    }

    pub async fn thread_comments(&self, env: &Env, input: &PullRequestThreadCommentsInput) -> PrResult<PullRequestThreadCommentsResult> {
        let resolved = self.resolve(env, &input.reference, None).await?;
        let provider = self.provider_of(env, &resolved)?;
        provider
            .thread_comments(&resolved.subject(), input.thread_id.as_str(), input.cursor.as_str())
            .await
            .map_err(|failure| PrError::from_failure("threadComments", failure, resolved.project.kind))
    }

    pub async fn diff(&self, env: &Env, input: &PullRequestDiffInput, namespace: Option<String>) -> PrResult<PullRequestDiffResult> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let ref_key = self.key(&resolved);
        let updated = if input.commit.is_none() { self.last_summary.peek(&ref_key).map(|(_, held)| held.updated_at) } else { None };
        let key = json!([ref_key, input.cursor.as_ref().map(|c| c.as_str()), input.commit.as_ref().map(|c| c.as_str()), updated]).to_string();
        let commit_scoped = input.commit.is_some();
        self.diffs
            .get(key, Box::pin(self.diff_uncached(env.clone(), resolved, input.cursor.clone(), input.commit.clone())), move |result| match result {
                Ok(diff) if diff.patch.len() * 2 <= MAX_CACHED_DIFF_PATCH_BYTES => {
                    if commit_scoped { COMMIT_DIFF_CACHE_TTL } else { DIFF_CACHE_TTL }
                }
                _ => Duration::ZERO,
            })
            .await
    }

    async fn diff_uncached(&self, env: Env, resolved: Resolved, cursor: Option<TrimmedNonEmptyString>, commit: Option<TrimmedNonEmptyString>) -> PrResult<PullRequestDiffResult> {
        let provider = self.provider_of(&env, &resolved)?;
        if !provider.capabilities().diff {
            return Err(PrError::operation("diff", "This host cannot provide a diff for a change request."));
        }
        provider
            .diff(&resolved.subject(), cursor.as_ref().map(|c| c.as_str()), commit.as_ref().map(|c| c.as_str()))
            .await
            .map_err(|failure| PrError::from_failure("diff", failure, resolved.project.kind))
    }

    pub async fn diff_file_contents(&self, env: &Env, input: &PullRequestDiffFileContentsInput) -> PrResult<PullRequestDiffFileContentsResult> {
        let resolved = self.resolve(env, &input.reference, None).await?;
        let provider = self.provider_of(env, &resolved)?;
        if !provider.capabilities().diff {
            return Err(PrError::operation("diffFileContents", "This host cannot expand unchanged pull request lines."));
        }
        provider
            .diff_file_contents(&resolved.subject(), input.commit.as_ref().map(|c| c.as_str()), input.change_type, input.old_path.as_str(), input.new_path.as_str())
            .await
            .map_err(|failure| PrError::from_failure("diffFileContents", failure, resolved.project.kind))
    }

    pub async fn files_viewed(&self, env: &Env, reference: &PullRequestRef, namespace: Option<String>) -> PrResult<PullRequestFilesViewedResult> {
        let resolved = self.resolve(env, reference, namespace).await?;
        let epoch = {
            let scope = self.scope_of(&resolved);
            self.epochs.lock().unwrap().files_viewed.get(&scope).copied().unwrap_or(0)
        };
        let key = json!([self.key(&resolved), epoch]).to_string();
        self.files_viewed
            .get(key, Box::pin(self.files_viewed_uncached(env.clone(), resolved)), |result| if result.is_ok() { FILES_VIEWED_CACHE_TTL } else { Duration::ZERO })
            .await
    }

    async fn files_viewed_uncached(&self, env: Env, resolved: Resolved) -> PrResult<PullRequestFilesViewedResult> {
        let provider = self.provider_of(&env, &resolved)?;
        match provider.capabilities().viewed_files {
            Some(PullRequestViewedFilesStore::Host) => provider.files_viewed(&resolved.subject()).await.map_err(|failure| PrError::from_failure("filesViewed", failure, resolved.project.kind)),
            Some(PullRequestViewedFilesStore::Environment) => Box::pin(self.environment_files_viewed(&env, &resolved)).await,
            None => Err(PrError::operation("filesViewed", "This host does not track which files a reader has seen.")),
        }
    }

    pub async fn stack(&self, env: &Env, reference: &PullRequestRef, namespace: Option<String>, include_details: bool) -> PrResult<Option<PullRequestStack>> {
        let resolved = self.resolve(env, reference, namespace).await?;
        let key = format!("{}:{include_details}", self.key(&resolved));
        self.stacks
            .get(key, Box::pin(self.stack_uncached(env.clone(), resolved, include_details)), |result| if result.is_ok() { DETAIL_CACHE_TTL } else { Duration::ZERO })
            .await
    }

    async fn stack_uncached(&self, env: Env, resolved: Resolved, include_details: bool) -> PrResult<Option<PullRequestStack>> {
        let provider = self.provider_of(&env, &resolved)?;
        provider.stack(&resolved.subject(), include_details).await.map_err(|failure| PrError::from_failure("stack", failure, resolved.project.kind))
    }

    /// Threads that link the pull request, newest first, archived ones included.
    pub async fn linked_threads(&self, env: &Env, reference: &PullRequestRef) -> PrResult<PullRequestLinkedThreadsResult> {
        let Some(key) = self.sync_key(env, reference).await else {
            return Ok(PullRequestLinkedThreadsResult { threads: vec![] });
        };
        let store = env.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut rows = vec![];
            for projection in store.projections("thread").map_err(|_| ())? {
                let thread = &projection["thread"];
                if !thread["deletedAt"].is_null() {
                    continue;
                }
                let linked = crate::thread_pull_requests::links(thread).iter().any(|link| {
                    link["source"] != "stack-dismissed" && crate::thread_pull_requests::normalize(link) == key
                });
                if linked {
                    rows.push((thread["updatedAt"].as_str().unwrap_or("").to_string(), thread.clone()));
                }
            }
            rows.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1["id"].as_str().cmp(&right.1["id"].as_str())));
            Ok::<_, ()>(rows)
        })
        .await
        .map_err(|_| PrError::operation("linkedThreads", "Could not load linked threads."))?
        .map_err(|()| PrError::operation("linkedThreads", "Could not load linked threads."))
        .map(|rows| PullRequestLinkedThreadsResult {
            threads: rows
                .into_iter()
                .filter_map(|(_, thread)| {
                    Some(PullRequestLinkedThread {
                        id: thread["id"].as_str()?.parse().ok()?,
                        project_id: thread["projectId"].as_str()?.parse().ok()?,
                        title: thread["title"].as_str().unwrap_or("").to_string(),
                        archived_at: thread["archivedAt"].as_str().map(str::to_string),
                    })
                })
                .collect(),
        })
    }

    /// `pullRequestSyncKey`: the host-level identity a thread link is filed under.
    pub async fn sync_key(&self, env: &Env, reference: &PullRequestRef) -> Option<Value> {
        let host = match &reference.host {
            Some(host) => host.as_str().to_string(),
            None => {
                let project = self.projects(env).await.into_iter().find(|project| project.id == reference.project_id)?;
                self.identity(env, &project.workspace_root).await?.host()
            }
        };
        Some(crate::thread_pull_requests::normalize(&json!({
            "host": host,
            "repository": reference.repository.as_str(),
            "number": reference.number,
        })))
    }

    // --- invalidation --------------------------------------------------------------------

    pub async fn invalidate(&self, env: &Env, input: &PullRequestInvalidateInput, notify_readers: bool) {
        if input.files_viewed_only == Some(true) {
            if let Some(reference) = &input.reference {
                if let Ok(resolved) = self.resolve(env, reference, None).await {
                    let scope = self.scope_of(&resolved);
                    let mut epochs = self.epochs.lock().unwrap();
                    let epoch = epochs.next();
                    epochs.files_viewed.insert(scope, epoch);
                }
            }
            return;
        }
        match &input.reference {
            Some(reference) => {
                if let Ok(resolved) = self.resolve(env, reference, None).await {
                    let scope = self.scope_of(&resolved);
                    self.epochs.lock().unwrap().bump(scope);
                }
            }
            None => {
                let mut epochs = self.epochs.lock().unwrap();
                epochs.listings = epochs.next();
                self.viewers.lock().unwrap().clear();
            }
        }
        if notify_readers {
            let epoch = self.epochs.lock().unwrap().next();
            self.refreshes.send_replace(epoch);
        }
    }

    /// A write changed the pull request: strand what is held for it and for every listing, and
    /// tell the readers to fetch again (`invalidatedByMutation`).
    pub async fn after_mutation(&self, resolved: &Resolved) {
        let scope = self.scope_of(resolved);
        let epoch = {
            let mut epochs = self.epochs.lock().unwrap();
            epochs.bump(scope);
            epochs.listings = epochs.next();
            epochs.listings
        };
        self.refreshes.send_replace(epoch);
    }

    /// The project's pull requests may have changed after a turn: forget what is held for the
    /// whole project and tell readers to fetch again (`refreshAfterTurn`).
    pub fn refresh_after_turn(&self, project_id: &str) {
        let epoch = {
            let mut epochs = self.epochs.lock().unwrap();
            let listings = epochs.next();
            epochs.listings = listings;
            epochs.projects.remove(project_id);
            if epochs.projects.len() >= REF_EPOCH_CAPACITY {
                if let Some(oldest) = epochs.projects.keys().next().cloned() {
                    epochs.project_floor = epochs.projects.remove(&oldest).unwrap_or(0);
                }
            }
            epochs.projects.insert(project_id.to_string(), listings);
            listings
        };
        self.refreshes.send_replace(epoch);
    }

    /// Drops what is held for one pull request without telling the readers (a write that failed
    /// part-way may still have changed it).
    pub(crate) fn invalidate_reference(&self, resolved: &Resolved) {
        let scope = self.scope_of(resolved);
        self.epochs.lock().unwrap().bump(scope);
    }

    /// Tells whoever listens that a merge was confirmed; with nobody listening this is nothing.
    pub(crate) fn publish_merge(&self, event: crate::pull_request_actions::PullRequestMergeEvent) {
        let _ = self.merges.send(event);
    }

    /// Merges confirmed from now on, for whatever settles the threads that were working on them.
    pub fn subscribe_merges(&self) -> tokio::sync::broadcast::Receiver<crate::pull_request_actions::PullRequestMergeEvent> {
        self.merges.subscribe()
    }

    /// Readers are told whenever something they hold may have changed; the value is a revision.
    pub fn subscribe_refreshes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.refreshes.subscribe()
    }

    pub fn listings_epoch(&self) -> u64 {
        self.epochs.lock().unwrap().listings
    }

    pub fn ref_epoch_of(&self, resolved: &Resolved) -> u64 {
        let scope = self.scope_of(resolved);
        self.epochs.lock().unwrap().ref_epoch(resolved.project.project.id.as_str(), &scope)
    }

    /// Records a pull request's files-viewed marks as changed by this reader's own press.
    pub fn bump_files_viewed(&self, resolved: &Resolved) {
        let scope = self.scope_of(resolved);
        let mut epochs = self.epochs.lock().unwrap();
        let epoch = epochs.next();
        epochs.files_viewed.insert(scope, epoch);
    }
}

pub fn github_for(env: &Env) -> GitHub {
    GitHub::new(env.host.clone())
}

/// The picture every GitHub install serves at `/<login>.png`, for an actor with none.
pub fn with_avatar(actor: Option<PullRequestActor>, host: &str) -> Option<PullRequestActor> {
    let mut actor = actor?;
    if actor.avatar_url.is_none() {
        actor.avatar_url = github::login_avatar_url(actor.login.as_str(), host);
    }
    Some(actor)
}

fn summary_from_detail(detail: &PullRequestDetail, previous: Option<PullRequestSummary>) -> PullRequestSummary {
    // Detail does not carry review/check summaries. Keep the last summary observation.
    let previous = previous.unwrap_or_else(|| PullRequestSummary {
        provider: detail.provider,
        project_id: detail.project_id.clone(),
        repository: detail.repository.clone(),
        number: detail.number,
        title: detail.title.clone(),
        url: detail.url.clone(),
        state: detail.state,
        is_draft: None,
        head_branch: detail.head_branch.clone(),
        base_branch: detail.base_branch.clone(),
        closed_at: None,
        merged_at: None,
        updated_at: detail.updated_at.clone(),
        observed_at: None,
        author: None,
        additions: None,
        deletions: None,
        changed_files: None,
        review_decision: None,
        checks_state: None,
        mergeability: None,
        stack: None,
    });
    PullRequestSummary {
        provider: detail.provider,
        project_id: detail.project_id.clone(),
        repository: detail.repository.clone(),
        number: detail.number,
        title: detail.title.clone(),
        url: detail.url.clone(),
        state: detail.state,
        is_draft: Some(detail.is_draft),
        author: Some(detail.author.clone()),
        additions: Some(detail.additions),
        deletions: Some(detail.deletions),
        changed_files: Some(detail.changed_files),
        mergeability: Some(detail.mergeability),
        head_branch: detail.head_branch.clone(),
        base_branch: detail.base_branch.clone(),
        closed_at: Some(detail.closed_at.clone()),
        merged_at: Some(detail.merged_at.clone()),
        updated_at: detail.updated_at.clone(),
        observed_at: detail.observed_at,
        ..previous
    }
}

// ---------------------------------------------------------------------------------------------
// Repository identity
// ---------------------------------------------------------------------------------------------

async fn read_identity(host: Host, workspace_root: String) -> Result<Option<Identity>, ()> {
    let path = Path::new(&workspace_root);
    let remotes = match host.shared.git.list_remotes(path).await {
        Ok(listed) => listed.remotes,
        Err(_) => return Ok(None),
    };
    let Some(remote) = pick_primary_remote(&remotes) else { return Ok(None) };
    let canonical_key = normalize_git_remote_url(&remote.url);
    let detected = detect_provider_from_remote_url(&remote.url);
    let mut kind = detected.as_ref().map_or(SourceControlProviderKind::Unknown, |provider| provider.kind);
    if kind == SourceControlProviderKind::Unknown {
        // A self-hosted install with an ordinary host name is told apart by who is signed in.
        if let Some(provider) = detected {
            let context = ProviderContext { provider, remote_name: remote.name.clone(), remote_url: remote.url.clone(), requested_host: None };
            let registry = SourceControlRegistry::new(host.clone());
            if let Ok(handle) = registry.resolve_handle(&workspace_root, Some(context)).await {
                if let Some(context) = handle.context {
                    kind = context.provider.kind;
                }
            }
        }
    }
    let path_below_host = canonical_key.split('/').skip(1).collect::<Vec<_>>().join("/");
    let segments: Vec<&str> = path_below_host.split('/').filter(|segment| !segment.is_empty()).collect();
    Ok(Some(Identity {
        kind,
        canonical_key,
        remote_url: remote.url.clone(),
        display_name: (!path_below_host.is_empty()).then(|| path_below_host.clone()),
        owner: segments.first().map(|owner| owner.to_string()),
        name: segments.last().map(|name| name.to_string()),
    }))
}
