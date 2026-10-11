//! The pull request a branch's status shows (port of `lookupStatusPr` and the head-context
//! logic around it in `GitManager.ts`). The host is asked about the branch's head under every
//! selector it could be filed under, the answers are narrowed to the ones whose head is really
//! this branch's, and the result is shared for a while: a pull request is re-read at most once a
//! minute while one is open, and every five minutes otherwise, so an idle branch costs the host
//! next to nothing. A failure backs off per branch and the last known answer is kept meanwhile.
use crate::{
    source_control_host::Host,
    source_control_provider::{ListChangeRequestsInput, ListState},
    source_control_registry::SourceControlRegistry,
    vcs_cache::TtlCache,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use t3_contracts::{ChangeRequest, ChangeRequestState, SourceControlProviderKind};
use t3_vcs::{
    Clock,
    core::{ExecuteGitInput, GitCore, GitTimeout},
    remote_refs::normalize_git_remote_url,
};

const PR_LOOKUP_CACHE_TTL: Duration = Duration::from_secs(60);
const PR_LOOKUP_NO_OPEN_PR_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const PR_LOOKUP_FAILURE_BASE_TTL: Duration = Duration::from_secs(20);
const PR_LOOKUP_FAILURE_MAX_TTL: Duration = Duration::from_secs(15 * 60);
const PR_LOOKUP_CACHE_CAPACITY: usize = 2_048;
const GITHUB_HEAD_BRANCH_PROBE_LIMIT: u32 = 100;

/// How long a failed lookup is cached after `consecutive_failures` in a row: a throttled host
/// rejects instantly, so a flat short TTL would make a rate-limited poller ask faster than a
/// healthy one does.
pub fn pr_lookup_failure_ttl(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(32);
    PR_LOOKUP_FAILURE_BASE_TTL.saturating_mul(1u32 << exponent.min(20)).min(PR_LOOKUP_FAILURE_MAX_TTL)
}

/// What a status asks about: the checked-out branch.
#[derive(Debug, Clone)]
pub struct BranchDetails {
    pub branch: String,
    pub upstream_ref: Option<String>,
    pub default_branch: Option<String>,
    pub is_default_branch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub base: String,
    pub head: String,
    pub state: ChangeRequestState,
    pub is_draft: bool,
    pub updated_at: Option<String>,
    pub is_cross_repository: Option<bool>,
    pub head_repository: Option<String>,
    pub head_owner: Option<String>,
    /// The commit the host says the head was at (`headSha`); `None` when the host does not report it.
    pub head_sha: Option<String>,
}

impl PrInfo {
    pub fn of(request: &ChangeRequest) -> Self {
        Self {
            number: request.number,
            title: request.title.clone(),
            url: request.url.clone(),
            base: request.base_ref_name.clone(),
            head: request.head_ref_name.clone(),
            state: request.state,
            is_draft: request.is_draft == Some(true),
            updated_at: request.updated_at.clone(),
            is_cross_repository: request.is_cross_repository,
            head_repository: request.head_repository_name_with_owner.clone().flatten(),
            head_owner: request.head_repository_owner_login.clone().flatten(),
            head_sha: request.head_sha.clone(),
        }
    }

    /// `toStatusPr`.
    pub fn status(&self) -> Value {
        let mut value = json!({
            "number":self.number,"title":self.title,"url":self.url,"baseRef":self.base,"headRef":self.head,
            "state":match self.state { ChangeRequestState::Open => "open", ChangeRequestState::Closed => "closed", ChangeRequestState::Merged => "merged" },
            "updatedAt":self.updated_at,
        });
        if self.is_draft {
            value["isDraft"] = json!(true);
        }
        value
    }
}

/// Where the branch's head lives, and under which names a host may know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadContext {
    pub local_branch: String,
    pub head_branch: String,
    pub head_selectors: Vec<String>,
    pub remote_name: Option<String>,
    pub head_remote_url_key: Option<String>,
    pub head_repository: Option<String>,
    pub head_owner: Option<String>,
    pub is_cross_repository: bool,
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
}

/// `parseRepositoryNameWithOwnerFromRemoteUrl`, for the shapes git remotes take.
pub fn repository_of_remote_url(url: Option<&str>) -> Option<String> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)^(?:[^@/\s]+@[^:/\s]+:|(?:ssh|https?|git)://[^/]+/)((?:[^/\s]+/)+[^/\s]+?)(?:\.git)?/?$").unwrap()
    });
    let url = url?.trim();
    non_empty(pattern.captures(url).and_then(|captures| captures.get(1)).map(|found| found.as_str()))
}

fn owner_of(repository: Option<&str>) -> Option<String> {
    non_empty(repository.and_then(|repository| repository.trim().split('/').next()))
}

/// `extractBranchNameFromRemoteRef`: the branch after the remote's name.
pub(crate) fn branch_of_remote_ref(reference: &str, remote_name: Option<&str>) -> String {
    let normalized = reference.trim();
    if normalized.is_empty() {
        return String::new();
    }
    if let Some(rest) = normalized.strip_prefix("refs/remotes/") {
        return branch_of_remote_ref(rest, remote_name);
    }
    if let Some(remote) = remote_name.filter(|remote| !remote.is_empty()) {
        if let Some(branch) = normalized.strip_prefix(&format!("{remote}/")) {
            let branch = branch.trim();
            return if branch.is_empty() { String::new() } else { branch.to_string() };
        }
    }
    match normalized.find('/') {
        None => normalized.to_string(),
        Some(slash) => normalized[slash + 1..].trim().to_string(),
    }
}

fn push_unique(values: &mut Vec<String>, next: Option<&str>) {
    if let Some(next) = non_empty(next) {
        if !values.contains(&next) {
            values.push(next);
        }
    }
}

/// `matchesBranchHeadContext`: a pull request whose head is not this branch's (another fork's
/// branch of the same name) is not this branch's pull request.
pub fn matches_head_context(pr: &PrInfo, head: &HeadContext) -> bool {
    if pr.head != head.head_branch {
        return false;
    }
    let lower = |value: Option<String>| value.map(|value| value.to_lowercase());
    let expected_repository = lower(non_empty(head.head_repository.as_deref()));
    let expected_owner = lower(non_empty(head.head_owner.as_deref())).or_else(|| owner_of(expected_repository.as_deref()));
    // The pull request's own head repository, worked out for a fork from its URL.
    let pr_repository = lower(non_empty(pr.head_repository.as_deref()).or_else(|| {
        if pr.is_cross_repository != Some(true) {
            return None;
        }
        let owner = non_empty(pr.head_owner.as_deref())?;
        let rest = pr.url.trim().strip_prefix("https://").or_else(|| pr.url.trim().strip_prefix("http://"))?;
        let parts: Vec<&str> = rest.split('/').collect();
        (parts.len() >= 5 && parts[3] == "pull").then(|| format!("{owner}/{}", parts[2]))
    }));
    let pr_owner = lower(non_empty(pr.head_owner.as_deref())).or_else(|| owner_of(pr_repository.as_deref()));
    if let Some(expected) = &expected_repository {
        if let Some(actual) = &pr_repository {
            if expected != actual {
                return false;
            }
        }
        if let (Some(expected), Some(actual)) = (&expected_owner, &pr_owner) {
            if expected != actual {
                return false;
            }
        }
    }
    if let (Some(expected), Some(actual)) = (&expected_owner, &pr_owner) {
        if expected != actual {
            return false;
        }
    }
    let expected_known = expected_repository.is_some() || expected_owner.is_some();
    let actual_known = pr_repository.is_some() || pr_owner.is_some();
    if head.is_cross_repository {
        if pr.is_cross_repository == Some(false) {
            return false;
        }
        return !(expected_known && !actual_known);
    }
    if pr.is_cross_repository == Some(true) && (!expected_known || !actual_known) {
        return false;
    }
    true
}

impl HeadContext {
    /// `preferredHeadSelector`: how a host is told which branch a new pull request comes from. A
    /// fork's is `owner:branch`, because the bare name would name the base repository's branch.
    pub fn preferred_head_selector(&self) -> String {
        match (&self.head_owner, self.is_cross_repository, self.head_branch.is_empty()) {
            (Some(owner), true, false) => format!("{owner}:{}", self.head_branch),
            _ => self.head_branch.clone(),
        }
    }
}

#[derive(Clone)]
struct Lookup {
    latest: Option<PrInfo>,
    head: HeadContext,
}

#[derive(Clone)]
struct LastKnown {
    pr: Option<Value>,
    upstream_ref: Option<String>,
    head_branch: String,
    remote_name: Option<String>,
    head_remote_url_key: Option<String>,
}

pub struct StatusPrLookup {
    cache: TtlCache<String, Lookup, ()>,
    epochs: Mutex<HashMap<String, u64>>,
    streaks: Arc<Mutex<HashMap<String, u32>>>,
    last_known: Mutex<HashMap<String, LastKnown>>,
}

impl StatusPrLookup {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            cache: TtlCache::new(PR_LOOKUP_CACHE_CAPACITY, clock),
            epochs: Mutex::default(),
            streaks: Arc::default(),
            last_known: Mutex::default(),
        }
    }

    /// The process's lookup, on the process host's clock.
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<StatusPrLookup> = OnceLock::new();
        SHARED.get_or_init(|| Self::new(Host::process_default().clock().clone()))
    }

    /// Git actions and user-driven refreshes bump the epoch to bypass what is cached.
    pub fn bump_epoch(&self, cwd: &str) {
        *self.epochs.lock().unwrap().entry(cwd.to_string()).or_default() += 1;
    }

    /// The pull request to show for `details` in `cwd`, or `None`. Never fails: a lookup that
    /// cannot be made keeps the last answer known for the branch.
    pub async fn lookup(&self, host: &Host, core: &GitCore, cwd: &str, details: &BranchDetails, refresh_missing: bool) -> Option<Value> {
        let epoch = self.epochs.lock().unwrap().get(cwd).copied().unwrap_or(0);
        let key = [cwd, &details.branch, details.upstream_ref.as_deref().unwrap_or(""), details.default_branch.as_deref().unwrap_or(""), &epoch.to_string()].join("\u{0}");
        let branch_key = format!("{cwd}\u{0}{}", details.branch);
        // A "no pull request" answer is retried on request, since someone may just have opened one.
        let streaks = self.streaks.clone();
        let ttl_key = key.clone();
        let ttl = move |result: &Result<Lookup, ()>| match result {
            Ok(lookup) => {
                streaks.lock().unwrap().remove(&ttl_key);
                if lookup.latest.as_ref().is_some_and(|pr| pr.state == ChangeRequestState::Open) { PR_LOOKUP_CACHE_TTL } else { PR_LOOKUP_NO_OPEN_PR_CACHE_TTL }
            }
            Err(()) => {
                let mut streaks = streaks.lock().unwrap();
                let streak = streaks.get(&ttl_key).copied().unwrap_or(0) + 1;
                if streaks.len() >= PR_LOOKUP_CACHE_CAPACITY && !streaks.contains_key(&ttl_key) {
                    if let Some(oldest) = streaks.keys().next().cloned() {
                        streaks.remove(&oldest);
                    }
                }
                streaks.insert(ttl_key.clone(), streak);
                pr_lookup_failure_ttl(streak)
            }
        };
        if refresh_missing {
            // Peeking would need the cache to expose it: a forced re-ask is the same as a new epoch.
            self.bump_epoch(cwd);
            return Box::pin(self.lookup_inner(host, core, cwd, details, &branch_key, false)).await;
        }
        let load = Box::pin(self.read(host.clone(), core.clone(), cwd.to_string(), details.clone()));
        match self.cache.get(key, load, ttl).await {
            Ok(lookup) => self.answer(&branch_key, details, lookup),
            Err(()) => {
                let (head, _) = self.head_for_lookup(core, cwd, details).await;
                self.last_known_for(&branch_key, details, &head)
            }
        }
    }

    async fn lookup_inner(&self, host: &Host, core: &GitCore, cwd: &str, details: &BranchDetails, branch_key: &str, _cached: bool) -> Option<Value> {
        match self.read(host.clone(), core.clone(), cwd.to_string(), details.clone()).await {
            Ok(lookup) => self.answer(branch_key, details, lookup),
            Err(()) => {
                let (head, _) = self.head_for_lookup(core, cwd, details).await;
                self.last_known_for(branch_key, details, &head)
            }
        }
    }

    fn answer(&self, branch_key: &str, details: &BranchDetails, lookup: Lookup) -> Option<Value> {
        // On the default branch only open pull requests are shown: a merged or closed match is
        // usually reverse-merge history, not the thread's pull request.
        let pr = lookup
            .latest
            .filter(|latest| !(details.is_default_branch && latest.state != ChangeRequestState::Open))
            .map(|latest| latest.status());
        let mut known = self.last_known.lock().unwrap();
        if known.len() >= PR_LOOKUP_CACHE_CAPACITY && !known.contains_key(branch_key) {
            if let Some(oldest) = known.keys().next().cloned() {
                known.remove(&oldest);
            }
        }
        known.insert(
            branch_key.to_string(),
            LastKnown {
                pr: pr.clone(),
                upstream_ref: details.upstream_ref.clone(),
                head_branch: lookup.head.head_branch.clone(),
                remote_name: lookup.head.remote_name.clone(),
                head_remote_url_key: lookup.head.head_remote_url_key.clone(),
            },
        );
        pr
    }

    /// The last answer for the branch, while it is still about the same head.
    fn last_known_for(&self, branch_key: &str, details: &BranchDetails, head: &HeadContext) -> Option<Value> {
        let known = self.last_known.lock().unwrap().get(branch_key).cloned()?;
        if known.head_branch != head.head_branch {
            return None;
        }
        // Both sides must be resolved before a mismatch is real: a failure to read the current
        // remote reads as unknown, not as no remote.
        if let (Some(before), Some(now)) = (&known.head_remote_url_key, &head.head_remote_url_key) {
            return if before == now { known.pr } else { None };
        }
        if let (Some(_), Some(_), Some(before), Some(now)) = (&known.upstream_ref, &details.upstream_ref, &known.remote_name, &head.remote_name) {
            return if before == now { known.pr } else { None };
        }
        known.pr
    }

    async fn read(&self, host: Host, core: GitCore, cwd: String, details: BranchDetails) -> Result<Lookup, ()> {
        let (head, lookup) = self.head_for_lookup(&core, &cwd, &details).await;
        if !lookup {
            return Ok(Lookup { latest: None, head });
        }
        // Only skipped when the branch is untracked as well: anything carrying an upstream keeps
        // asking the host.
        if details.upstream_ref.is_none() && is_unpublished(&core, &cwd, &head).await {
            return Ok(Lookup { latest: None, head });
        }
        let latest = find_latest(&host, &cwd, &head).await?;
        Ok(Lookup { latest, head })
    }

    /// `resolveLookupHeadContext`: a branch cut from the default branch tracks it without being
    /// published there, so it is looked up by its own name on whichever remote holds it.
    async fn head_for_lookup(&self, core: &GitCore, cwd: &str, details: &BranchDetails) -> (HeadContext, bool) {
        let head = resolve_head_context(core, cwd, &details.branch, details.upstream_ref.as_deref(), None).await;
        let upstream_head_is_default = details.default_branch.as_deref() == Some(head.head_branch.as_str())
            || (details.default_branch.is_none() && matches!(head.head_branch.as_str(), "main" | "master"));
        if head.head_branch == details.branch || !upstream_head_is_default || head.is_cross_repository {
            return (head, true);
        }
        let Some(remote) = find_remote_tracking_remote(core, cwd, &details.branch, head.remote_name.as_deref()).await else {
            return (head, false);
        };
        (resolve_head_context(core, cwd, &details.branch, None, Some(&remote)).await, true)
    }
}

struct RemoteRepository {
    url_key: Option<String>,
    repository: Option<String>,
    owner: Option<String>,
}

async fn remote_repository(core: &GitCore, cwd: &str, remote_name: Option<&str>) -> RemoteRepository {
    let Some(remote_name) = remote_name else { return RemoteRepository { url_key: None, repository: None, owner: None } };
    let url = core.read_config_value(Path::new(cwd), &format!("remote.{remote_name}.url")).await.ok().flatten();
    let repository = repository_of_remote_url(url.as_deref());
    RemoteRepository {
        url_key: url.as_deref().filter(|url| !url.is_empty()).map(normalize_git_remote_url),
        owner: owner_of(repository.as_deref()),
        repository,
    }
}

/// `resolveBranchHeadContext`.
pub async fn resolve_head_context(core: &GitCore, cwd: &str, branch: &str, upstream_ref: Option<&str>, remote_override: Option<&str>) -> HeadContext {
    let path = Path::new(cwd);
    let remote_name = match remote_override {
        Some(remote) => Some(remote.to_string()),
        None => core.read_config_value(path, &format!("branch.{branch}.remote")).await.ok().flatten(),
    };
    let from_upstream = upstream_ref.map(|upstream| branch_of_remote_ref(upstream, remote_name.as_deref())).unwrap_or_default();
    let head_branch = if from_upstream.is_empty() { branch.to_string() } else { from_upstream.clone() };
    let probe_local_selector = from_upstream.is_empty() || head_branch == branch;
    let remote = remote_repository(core, cwd, remote_name.as_deref()).await;
    let origin = if remote_name.as_deref() == Some("origin") {
        RemoteRepository { url_key: remote.url_key.clone(), repository: remote.repository.clone(), owner: remote.owner.clone() }
    } else {
        remote_repository(core, cwd, Some("origin")).await
    };
    let is_cross_repository = match (&remote.repository, &origin.repository) {
        (Some(remote), Some(origin)) => remote.to_lowercase() != origin.to_lowercase(),
        _ => remote_name.as_deref().is_some_and(|name| name != "origin") && remote.repository.is_some(),
    };
    let owner_selector = remote.owner.as_ref().filter(|_| !head_branch.is_empty()).map(|owner| format!("{owner}:{head_branch}"));
    let alias_selector = remote_name.as_ref().filter(|_| !head_branch.is_empty()).map(|name| format!("{name}:{head_branch}"));
    let probe_remote_owned = is_cross_repository || remote_name.as_deref().is_some_and(|name| name != "origin");
    let alias_if_distinct = || if alias_selector != owner_selector { alias_selector.clone() } else { None };
    let mut selectors = vec![];
    if is_cross_repository && probe_remote_owned {
        push_unique(&mut selectors, owner_selector.as_deref());
        push_unique(&mut selectors, alias_if_distinct().as_deref());
    }
    if probe_local_selector {
        push_unique(&mut selectors, Some(branch));
    }
    push_unique(&mut selectors, (head_branch != branch).then_some(head_branch.as_str()));
    if !is_cross_repository && probe_remote_owned {
        push_unique(&mut selectors, owner_selector.as_deref());
        push_unique(&mut selectors, alias_if_distinct().as_deref());
    }
    HeadContext {
        local_branch: branch.to_string(),
        head_branch,
        head_selectors: selectors,
        head_remote_url_key: remote.url_key.clone().or_else(|| if remote_name.is_none() { origin.url_key.clone() } else { None }),
        remote_name,
        head_repository: remote.repository,
        head_owner: remote.owner,
        is_cross_repository,
    }
}

async fn run(core: &GitCore, cwd: &str, operation: &str, args: &[&str]) -> Option<String> {
    core.execute(ExecuteGitInput {
        operation: operation.into(),
        cwd: cwd.into(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        timeout: GitTimeout::After(Duration::from_secs(5)),
        ..Default::default()
    })
    .await
    .ok()
    .map(|result| result.stdout)
}

/// The remote that holds a ref named after the local branch: the preferred one, then origin,
/// then the first configured.
async fn find_remote_tracking_remote(core: &GitCore, cwd: &str, branch: &str, preferred: Option<&str>) -> Option<String> {
    if branch.is_empty() {
        return None;
    }
    let remotes = run(core, cwd, "GitManager.findRemoteTrackingRemote.remotes", &["remote"]).await?;
    let names: Vec<String> = remotes.lines().map(str::trim).filter(|name| !name.is_empty()).map(str::to_string).collect();
    if names.is_empty() {
        return None;
    }
    let patterns: Vec<String> = names.iter().map(|name| format!("refs/remotes/{name}/{branch}")).collect();
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend(patterns.iter().map(String::as_str));
    let refs = run(core, cwd, "GitManager.findRemoteTrackingRemote.refs", &args).await?;
    let present: Vec<&str> = refs.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let matching: Vec<&String> = names.iter().filter(|name| present.contains(&format!("refs/remotes/{name}/{branch}").as_str())).collect();
    if let Some(preferred) = preferred {
        if matching.iter().any(|name| name.as_str() == preferred) {
            return Some(preferred.to_string());
        }
    }
    if matching.iter().any(|name| name.as_str() == "origin") {
        return Some("origin".into());
    }
    matching.first().map(|name| name.to_string())
}

/// Whether git has no record of this branch on any remote, so a pull request cannot exist for
/// it and asking the host is a guaranteed-empty call. A probe that fails keeps the lookup.
async fn is_unpublished(core: &GitCore, cwd: &str, head: &HeadContext) -> bool {
    if head.head_branch.is_empty() {
        return false;
    }
    let path = Path::new(cwd);
    let (remote_key, merge_key) = (format!("branch.{}.remote", head.local_branch), format!("branch.{}.merge", head.local_branch));
    let (remote, merge) = tokio::join!(core.read_config_value(path, &remote_key), core.read_config_value(path, &merge_key));
    let (Ok(remote), Ok(merge)) = (remote, merge) else { return false };
    if remote.is_some() && merge.is_some() {
        return false;
    }
    let any = run(core, cwd, "GitManager.isUnpublishedBranch", &["for-each-ref", "--count=1", "--format=%(refname)", "refs/remotes"]).await;
    let this = run(core, cwd, "GitManager.isUnpublishedBranch", &["for-each-ref", "--count=1", "--format=%(refname)", &format!("refs/remotes/*/{}", head.head_branch)]).await;
    match (any, this) {
        (Some(any), Some(this)) => !any.trim().is_empty() && this.trim().is_empty(),
        _ => false,
    }
}

/// `findOpenPr`: the first open pull request, under any selector the host can be asked about,
/// whose head really is this branch's.
pub async fn find_open_pr(host: &Host, cwd: &str, head: &HeadContext) -> Result<Option<PrInfo>, t3_contracts::SourceControlProviderError> {
    let provider = SourceControlRegistry::new(host.clone()).resolve(cwd).await?;
    let github = provider.kind() == SourceControlProviderKind::Github;
    for selector in head.head_selectors.iter().filter(|selector| !github || !selector.contains(':')) {
        let found = provider
            .list_change_requests(ListChangeRequestsInput {
                cwd: cwd.into(),
                context: None,
                source: None,
                head_selector: selector.clone(),
                state: ListState::Open,
                limit: Some(if github { GITHUB_HEAD_BRANCH_PROBE_LIMIT } else { 1 }),
            })
            .await?;
        if let Some(pr) = found.iter().map(PrInfo::of).find(|pr| matches_head_context(pr, head)) {
            return Ok(Some(PrInfo { state: ChangeRequestState::Open, updated_at: None, ..pr }));
        }
    }
    Ok(None)
}

/// `findLatestPrForHeadContext`: every selector is asked about, the matches are merged by
/// number, and the newest open one wins, else the newest of any state.
async fn find_latest(host: &Host, cwd: &str, head: &HeadContext) -> Result<Option<PrInfo>, ()> {
    let registry = SourceControlRegistry::new(host.clone());
    let handle = registry.resolve_handle(cwd, None).await.map_err(|_| ())?;
    let kind = handle.context.as_ref().map_or(SourceControlProviderKind::Unknown, |context| context.provider.kind);
    let selectors: Vec<&String> = head
        .head_selectors
        .iter()
        .filter(|selector| kind != SourceControlProviderKind::Github || !selector.contains(':'))
        .collect();
    let mut by_number: HashMap<u64, PrInfo> = HashMap::new();
    for selector in selectors {
        let found = handle
            .provider
            .list_change_requests(ListChangeRequestsInput {
                cwd: cwd.into(),
                context: None,
                source: None,
                head_selector: selector.clone(),
                state: ListState::All,
                limit: Some(if kind == SourceControlProviderKind::Github { GITHUB_HEAD_BRANCH_PROBE_LIMIT } else { 20 }),
            })
            .await
            .map_err(|_| ())?;
        for request in &found {
            let pr = PrInfo::of(request);
            if matches_head_context(&pr, head) {
                by_number.insert(pr.number, pr);
            }
        }
    }
    let mut parsed: Vec<PrInfo> = by_number.into_values().collect();
    // Newest update first; a pull request with no update time sorts last.
    parsed.sort_by(|left, right| right.updated_at.cmp(&left.updated_at).then(left.number.cmp(&right.number)));
    let open = parsed.iter().position(|pr| pr.state == ChangeRequestState::Open);
    Ok(open.map(|index| parsed.remove(index)).or_else(|| parsed.into_iter().next()))
}

/// What storage cleanup needs of `GitManager.branchPullRequest`: the latest pull request of the branch in any state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPullRequest {
    pub state: ChangeRequestState,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: Option<String>,
}

/// `GitManager.branchPullRequest(..., { refresh: true })` for a branch that exists locally: the same
/// remote, upstream and default-branch reading, the same head context and the same latest-pull-request
/// choice as the status lookup, asked of the host afresh (a refresh drops the cached answer). Differences:
/// a branch with no local ref answers `None` (the original follows a remote-tracking ref; cleanup only asks
/// about checked-out branches), and there is no repository-identity re-check, which only guards the cache.
pub async fn branch_pull_request(host: &Host, core: &GitCore, cwd: &str, branch: &str) -> Result<Option<BranchPullRequest>, String> {
    let remotes = core
        .execute(ExecuteGitInput { operation: "GitManager.branchPullRequest.remotes".into(), cwd: cwd.into(), args: vec!["remote".into()], ..Default::default() })
        .await
        .map_err(|error| error.to_string())?;
    let remote_names: Vec<&str> = remotes.stdout.lines().map(str::trim).filter(|name| !name.is_empty()).collect();
    let Some(first) = remote_names.first().copied() else { return Ok(None) };
    let expected = format!("refs/heads/{branch}");
    let refs = core
        .execute(ExecuteGitInput {
            operation: "GitManager.branchPullRequest.branchRef".into(),
            cwd: cwd.into(),
            args: vec!["for-each-ref".into(), "--format=%(refname)%00%(upstream:short)%00%(upstream:remotename)%00%(upstream:remoteref)".into(), expected.clone()],
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    let Some(exact) = refs.stdout.lines().find(|line| line.split('\u{0}').next() == Some(expected.as_str())) else { return Ok(None) };
    let mut fields = exact.split('\u{0}').skip(1);
    let (saved_upstream, saved_remote, saved_ref) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""), fields.next().unwrap_or(""));
    let upstream_ref = if saved_upstream.is_empty() {
        None
    } else if saved_remote.is_empty() || saved_ref.is_empty() {
        return Err(format!("Saved upstream for {branch} is incomplete."));
    } else {
        Some(format!("{saved_remote}/{}", saved_ref.strip_prefix("refs/heads/").unwrap_or(saved_ref)))
    };
    let default_remote = if remote_names.contains(&"origin") { "origin" } else { first };
    let default_branch = core.resolve_default_branch_name(Path::new(cwd), default_remote).await.ok().flatten();
    let details = BranchDetails { branch: branch.into(), upstream_ref, default_branch: default_branch.clone(), is_default_branch: false };
    let lookup = StatusPrLookup::shared().read(host.clone(), core.clone(), cwd.into(), details).await.map_err(|()| format!("The pull request of {branch} could not be read."))?;
    let Some(latest) = lookup.latest else { return Ok(None) };
    let on_default = default_branch.as_deref() == Some(branch) || (default_branch.is_none() && matches!(branch, "main" | "master"));
    if on_default && latest.state != ChangeRequestState::Open {
        return Ok(None);
    }
    Ok(Some(BranchPullRequest { state: latest.state, head_ref: latest.head, base_ref: latest.base, head_sha: latest.head_sha }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_matching_ttls_and_remote_parsing_agree_with_the_original() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/git-workflow-status.jsonl").lines().filter(|line| !line.is_empty()) {
            let row: Value = serde_json::from_str(line).unwrap();
            match row["type"].as_str().unwrap() {
                "failureTtl" => {
                    let ttl = pr_lookup_failure_ttl(row["failures"].as_u64().unwrap() as u32);
                    assert_eq!(ttl.as_millis() as u64, row["millis"].as_u64().unwrap(), "{row}");
                }
                "repository" => {
                    assert_eq!(repository_of_remote_url(row["url"].as_str()).map(Value::from).unwrap_or(Value::Null), row["result"], "{row}");
                }
                "matches" => {
                    let (head, pr) = (&row["head"], &row["pr"]);
                    let text = |value: &Value| value.as_str().map(str::to_string);
                    let head = HeadContext {
                        local_branch: String::new(),
                        head_branch: head["headBranch"].as_str().unwrap().into(),
                        head_selectors: vec![],
                        remote_name: None,
                        head_remote_url_key: None,
                        head_repository: text(&head["headRepositoryNameWithOwner"]),
                        head_owner: text(&head["headRepositoryOwnerLogin"]),
                        is_cross_repository: head["isCrossRepository"].as_bool().unwrap(),
                    };
                    let pr = PrInfo {
                        number: pr["number"].as_u64().unwrap(),
                        title: String::new(),
                        url: pr["url"].as_str().unwrap().into(),
                        base: "main".into(),
                        head: pr["head"].as_str().unwrap().into(),
                        state: ChangeRequestState::Open,
                        is_draft: false,
                        updated_at: None,
                        is_cross_repository: pr["isCrossRepository"].as_bool(),
                        head_repository: text(&pr["headRepository"]),
                        head_owner: text(&pr["headOwner"]),
                        head_sha: None,
                    };
                    assert_eq!(matches_head_context(&pr, &head), row["result"].as_bool().unwrap(), "{row}");
                }
                other => panic!("unknown case {other}"),
            }
            count += 1;
        }
        assert_eq!(count, 80);
    }
}
