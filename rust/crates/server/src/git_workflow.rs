//! Pull requests as git sees them (port of `resolvePullRequest` and `preparePullRequestThread`
//! in `GitManager.ts`): naming the pull request a reference means, and checking it out into the
//! project's checkout or a worktree of its own so a thread can work on it.
//!
//! Project setup scripts are not run for a prepared worktree; the original starts one in the
//! background when the request names a thread.
use crate::{
    pull_request_service::Env,
    source_control_bitbucket::sanitize_branch_fragment,
    source_control_detect::is_ssh_remote_url,
    source_control_provider::{CheckoutChangeRequestInput, GetChangeRequestInput, RepositoryUrlsInput},
    source_control_registry::SourceControlRegistry,
    vcs_error::VcsRpcError,
    vcs_input::CreateWorktreeInput,
    vcs_service::{VcsContext, VcsService},
    vcs_status::StatusSource,
};
use serde_json::{Value, json};
use std::path::Path;
use t3_contracts::{ChangeRequest, ChangeRequestState, SourceControlProviderError};
use t3_vcs::VcsDriverKind;

/// `GitManagerServiceError`.
#[derive(Debug, Clone)]
pub enum WorkflowError {
    Vcs(VcsRpcError),
    Provider(SourceControlProviderError),
    Materialization(Value),
    /// Something this server does not do yet, said as the method that needed it.
    Unsupported { method: &'static str, message: String },
    /// A model could not write the text the workflow needed (`TextGenerationError`).
    TextGeneration(crate::text_generation::TextGenerationError),
}

impl WorkflowError {
    pub fn wire(&self) -> Value {
        match self {
            Self::Vcs(error) => error.wire(),
            Self::Provider(error) => error.wire(),
            Self::Materialization(value) => value.clone(),
            Self::Unsupported { method, message } => json!({"_tag":"NativeMethodUnsupportedError","method":method,"message":message}),
            Self::TextGeneration(error) => error.wire(),
        }
    }

    /// `error.message`, which a progress event carries when an action fails.
    pub fn message(&self) -> String {
        match self {
            Self::Vcs(error) => error.to_string(),
            Self::Provider(error) => error.message(),
            Self::Materialization(value) => value["cause"]["message"].as_str().unwrap_or("Pull request materialization failed.").to_string(),
            Self::Unsupported { message, .. } => message.clone(),
            Self::TextGeneration(error) => error.message(),
        }
    }
}

impl From<VcsRpcError> for WorkflowError {
    fn from(error: VcsRpcError) -> Self {
        Self::Vcs(error)
    }
}
impl From<SourceControlProviderError> for WorkflowError {
    fn from(error: SourceControlProviderError) -> Self {
        Self::Provider(error)
    }
}
impl From<t3_vcs::GitCommandError> for WorkflowError {
    fn from(error: t3_vcs::GitCommandError) -> Self {
        Self::Vcs(error.into())
    }
}

pub type WorkflowResult<T> = Result<T, WorkflowError>;

/// `ensureGit`: the repository at `cwd` must exist and be a git one; the failures are
/// `GitManagerError`s.
pub(crate) async fn ensure_git(operation: &str, cwd: &str) -> WorkflowResult<()> {
    let registry = VcsService::shared().registry();
    let handle = registry.resolve(cwd, None).await.map_err(|cause| {
        VcsRpcError::manager(operation, cwd, "Failed to resolve the VCS driver for this Git workflow.", Some(&cause))
    })?;
    if handle.kind != VcsDriverKind::Git {
        return Err(VcsRpcError::manager(
            operation,
            cwd,
            format!("The {operation} workflow currently supports Git repositories only; detected {}. ({cwd})", handle.kind),
            None,
        )
        .into());
    }
    Ok(())
}

pub(crate) fn manager(operation: &str, cwd: &str, detail: &str) -> WorkflowError {
    VcsRpcError::manager(operation, cwd, detail, None).into()
}

/// `#7` means `7`.
fn normalize_reference(reference: &str) -> String {
    let trimmed = t3_contracts::trim_wire_string(reference);
    match trimmed.strip_prefix('#') {
        Some(number) if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) => number.to_string(),
        _ => trimmed.to_string(),
    }
}

/// The pull request as the thread-preparing workflow reads it.
#[derive(Debug, Clone)]
struct Resolved {
    number: u64,
    title: String,
    url: String,
    base: String,
    head: String,
    state: ChangeRequestState,
    is_cross_repository: Option<bool>,
    head_repository: Option<String>,
    head_owner: Option<String>,
}

impl Resolved {
    fn of(request: &ChangeRequest) -> Self {
        Self {
            number: request.number,
            title: request.title.clone(),
            url: request.url.clone(),
            base: request.base_ref_name.clone(),
            head: request.head_ref_name.clone(),
            state: request.state,
            is_cross_repository: request.is_cross_repository,
            head_repository: request.head_repository_name_with_owner.clone().flatten(),
            head_owner: request.head_repository_owner_login.clone().flatten(),
        }
    }

    fn wire(&self) -> Value {
        json!({"number":self.number,"title":self.title,"url":self.url,"baseBranch":self.base,"headBranch":self.head,"state":match self.state {
            ChangeRequestState::Open => "open", ChangeRequestState::Closed => "closed", ChangeRequestState::Merged => "merged"}})
    }

    /// The repository the head lives in: named by the host, or worked out for a fork from the
    /// owner and the name in the pull request's URL.
    fn head_repository_name(&self) -> Option<String> {
        if let Some(explicit) = self.head_repository.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
            return Some(explicit.to_string());
        }
        if self.is_cross_repository != Some(true) {
            return None;
        }
        let owner = self.head_owner.as_deref().map(str::trim).filter(|owner| !owner.is_empty())?;
        let url = self.url.trim();
        let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
        let parts: Vec<&str> = rest.split('/').collect();
        let valid = parts.len() >= 5 && parts[3..].first().is_some() && parts[3] == "pull" && parts[4].bytes().all(|byte| byte.is_ascii_digit()) && !parts[4].is_empty();
        let name = parts.get(2).map(|name| name.trim()).filter(|name| valid && !name.is_empty())?;
        Some(format!("{owner}/{name}"))
    }

    /// A fork's head gets a branch of its own name-spaced under `t3code/`, so it can never shadow
    /// a local branch of the same name.
    fn local_branch(&self) -> String {
        if self.is_cross_repository != Some(true) {
            return self.head.clone();
        }
        format!("t3code/pr-{}/{}", self.number, sanitize_branch_fragment(&self.head).trim())
    }
}

/// `resolvePullRequest`: what a reference means.
pub async fn resolve_pull_request(env: &Env, cwd: &str, reference: &str) -> WorkflowResult<Value> {
    ensure_git("GitWorkflowService.resolvePullRequest", cwd).await?;
    let provider = SourceControlRegistry::new(env.host.clone()).resolve(cwd).await?;
    let request = provider
        .get_change_request(GetChangeRequestInput { cwd: cwd.into(), context: None, reference: normalize_reference(reference) })
        .await?;
    Ok(json!({"pullRequest":Resolved::of(&request).wire()}))
}

// ---------------------------------------------------------------------------------------------
// Preparing a thread
// ---------------------------------------------------------------------------------------------

async fn remote_for_head(env: &Env, cwd: &str, pull_request: &Resolved, repository: &str) -> WorkflowResult<String> {
    let core = VcsService::shared().core();
    let path = Path::new(cwd);
    let provider = SourceControlRegistry::new(env.host.clone()).resolve(cwd).await?;
    let urls = provider
        .get_repository_clone_urls(RepositoryUrlsInput { cwd: cwd.into(), context: None, repository: repository.into() })
        .await?;
    let origin = core.read_config_value(path, "remote.origin.url").await?;
    let remote_url = if origin.as_deref().is_some_and(is_ssh_remote_url) { urls.ssh_url } else { urls.url };
    let preferred = pull_request
        .head_owner
        .as_deref()
        .map(str::trim)
        .filter(|owner| !owner.is_empty())
        .map(str::to_string)
        .or_else(|| repository.split('/').next().map(str::trim).filter(|owner| !owner.is_empty()).map(str::to_string))
        .unwrap_or_else(|| "fork".into());
    Ok(core.ensure_remote(path, &preferred, &remote_url).await?)
}

/// Tracking config for the checked-out head branch. Best effort: a pull request that cannot
/// be tracked is still a pull request that can be worked on.
async fn configure_upstream(env: &Env, cwd: &str, pull_request: &Resolved, local_branch: &str) {
    let attempt = async {
        let core = VcsService::shared().core();
        let path = Path::new(cwd);
        let repository = pull_request.head_repository_name().unwrap_or_default();
        if repository.is_empty() && pull_request.is_cross_repository != Some(true) {
            let remote = core.resolve_primary_remote_name(path).await?;
            core.fetch_remote_tracking_branch(path, &remote, &pull_request.head).await?;
            core.set_branch_upstream(path, local_branch, &remote, &pull_request.head).await?;
            return Ok::<(), WorkflowError>(());
        }
        if repository.is_empty() {
            return Ok(());
        }
        let remote = remote_for_head(env, cwd, pull_request, &repository).await?;
        core.fetch_remote_tracking_branch(path, &remote, &pull_request.head).await?;
        core.set_branch_upstream(path, local_branch, &remote, &pull_request.head).await?;
        Ok(())
    };
    if let Err(error) = Box::pin(attempt).await {
        tracing::warn!(cwd=%cwd,local_branch=%local_branch,head_branch=%pull_request.head,error=?error,"configurePullRequestHeadUpstream failed");
    }
}

/// The head as a local branch: from the fork it lives in, else from the pull request's own ref;
/// when the first fails, the second is the fallback.
async fn materialize_head(env: &Env, cwd: &str, pull_request: &Resolved, local_branch: &str) -> WorkflowResult<()> {
    let core = VcsService::shared().core();
    let path = Path::new(cwd);
    let primary = Box::pin(async {
        match pull_request.head_repository_name() {
            None => core.fetch_pull_request_branch(path, pull_request.number, local_branch).await.map_err(WorkflowError::from),
            Some(repository) => {
                let remote = remote_for_head(env, cwd, pull_request, &repository).await?;
                core.fetch_remote_branch(path, &remote, &pull_request.head, local_branch).await?;
                core.set_branch_upstream(path, local_branch, &remote, &pull_request.head).await?;
                Ok(())
            }
        }
    })
    .await;
    let Err(primary) = primary else { return Ok(()) };
    match core.fetch_pull_request_branch(path, pull_request.number, local_branch).await {
        Ok(()) => Ok(()),
        Err(_) => {
            tracing::warn!(cwd=%cwd,cause=?primary,"pull request head fetch failed before the fallback also failed");
            Err(WorkflowError::Materialization(json!({
            "_tag":"GitPullRequestMaterializationError",
            "cwd":cwd,
            "pullRequestNumber":pull_request.number,
            "headRepository":pull_request.head_repository_name(),
            "headBranch":pull_request.head,
            "localBranch":local_branch,
            "cause":{"name":"AggregateError","message":format!("Repository-head and pull-request-ref fetches both failed for pull request #{}.", pull_request.number)},
        })))
        }
    }
}

async fn canonical(path: &str) -> String {
    tokio::fs::canonicalize(path).await.map(|path| path.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
}

#[derive(Debug, Clone)]
struct LocalHead {
    name: String,
    worktree_path: Option<String>,
}

/// The local branch a pull request's head lives on, in this checkout or another worktree.
async fn find_local_head_branch(cwd: &str, pull_request: &Resolved, root: &str) -> WorkflowResult<Option<LocalHead>> {
    let local = pull_request.local_branch();
    let input = serde_json::from_value(json!({"cwd":cwd,"refresh":true})).expect("list refs input");
    let refs = VcsService::shared().list_refs(&input).await?.refs;
    if let Some(found) = refs.iter().find(|branch| !branch.is_remote && branch.name == local) {
        return Ok(Some(LocalHead { name: found.name.clone(), worktree_path: found.worktree_path.clone() }));
    }
    if local == pull_request.head {
        return Ok(None);
    }
    for branch in &refs {
        if branch.is_remote || branch.name != pull_request.head {
            continue;
        }
        let Some(worktree) = &branch.worktree_path else { continue };
        if canonical(worktree).await != root {
            return Ok(Some(LocalHead { name: branch.name.clone(), worktree_path: Some(worktree.clone()) }));
        }
    }
    Ok(None)
}

/// Git refuses to move a branch that is checked out in a worktree, so a reused worktree is
/// advanced from inside it. One that cannot be moved (no reachable head, local commits, dirty
/// tree) is still handed back, because stranding the thread is worse than reporting staleness.
async fn reuse_worktree(env: &Env, pull_request: &Resolved, worktree: &str, checked_out: &str) -> WorkflowResult<Value> {
    let core = VcsService::shared().core();
    let path = Path::new(worktree);
    let local = pull_request.local_branch();
    let wire = |on_head: bool| json!({"pullRequest":pull_request.wire(),"branch":local,"worktreePath":worktree,"isOnPullRequestHead":on_head});
    let ensure_upstream = || async {
        let branch = core.status_details(path).await.ok().and_then(|details| details.branch).unwrap_or_else(|| pull_request.head.clone());
        configure_upstream(env, worktree, pull_request, &branch).await;
    };
    if checked_out != local {
        // A branch that merely shares the head's bare name is somebody else's work: it keeps its
        // tracking config and nothing else.
        ensure_upstream().await;
        return Ok(wire(false));
    }
    // Read before the upstream is touched: it force-updates the remote-tracking ref, and once
    // that has jumped to a rewritten head a checkout holding nothing of its own cannot be told
    // from one carrying local commits.
    let before_fetch = core.resolve_commit(path, "@{upstream}").await.ok();
    ensure_upstream().await;
    let target = match core.fetch_pull_request_head_commit(path, pull_request.number).await {
        Ok(commit) => Some(commit),
        Err(_) => {
            let details = core.status_details(path).await.ok();
            match details.and_then(|details| details.upstream_ref) {
                Some(upstream) if upstream.ends_with(&format!("/{}", pull_request.head)) => core.resolve_commit(path, &upstream).await.ok(),
                _ => {
                    return Err(manager("preparePullRequestThread", worktree, "The pull request head could not be resolved for this checkout."));
                }
            }
        }
    };
    let on_target = match target {
        Some(commit) => match core.refresh_checked_out_branch(path, &commit, before_fetch.as_deref()).await {
            Ok(result) => result.on_target,
            Err(error) => {
                tracing::warn!(worktree=%worktree,local_branch=%local,error=?error,"reused worktree refresh failed");
                false
            }
        },
        None => false,
    };
    Ok(wire(on_target))
}

const IN_MAIN_REPO: &str = "This PR branch is already checked out in the main repo. Use Local, or switch the main repo off that branch before creating a worktree thread.";

/// `preparePullRequestThread`: the pull request checked out where a thread can work on it,
/// either in the project's checkout (`local`) or a worktree of its own.
pub async fn prepare_pull_request_thread(env: &Env, context: &VcsContext, cwd: &str, reference: &str, mode: &str) -> WorkflowResult<Value> {
    ensure_git("GitWorkflowService.preparePullRequestThread", cwd).await?;
    let result = Box::pin(prepare_inner(env, context, cwd, reference, mode)).await;
    // The status shown for this checkout is stale whatever happened.
    VcsService::shared().status().source().invalidate_status(cwd).await;
    result
}

async fn prepare_inner(env: &Env, context: &VcsContext, cwd: &str, reference: &str, mode: &str) -> WorkflowResult<Value> {
    let core = VcsService::shared().core();
    let normalized = normalize_reference(reference);
    let root = canonical(cwd).await;
    let provider = SourceControlRegistry::new(env.host.clone()).resolve(cwd).await?;
    let request = provider
        .get_change_request(GetChangeRequestInput { cwd: cwd.into(), context: None, reference: normalized.clone() })
        .await?;
    let pull_request = Resolved::of(&request);

    if mode == "local" {
        provider
            .checkout_change_request(CheckoutChangeRequestInput { cwd: cwd.into(), context: None, reference: normalized, force: true })
            .await?;
        let details = core.status_details(Path::new(cwd)).await?;
        let branch = details.branch.unwrap_or_else(|| pull_request.head.clone());
        configure_upstream(env, cwd, &pull_request, &branch).await;
        return Ok(json!({"pullRequest":pull_request.wire(),"branch":branch,"worktreePath":null,"isOnPullRequestHead":true}));
    }

    let local = pull_request.local_branch();
    for after_fetch in [false, true] {
        if after_fetch {
            materialize_head(env, cwd, &pull_request, &local).await?;
        }
        let existing = find_local_head_branch(cwd, &pull_request, &root).await?;
        let existing_path = match existing.as_ref().and_then(|branch| branch.worktree_path.as_deref()) {
            Some(path) => Some(canonical(path).await),
            None => None,
        };
        if let (Some(branch), Some(path)) = (&existing, &existing_path) {
            if *path != root {
                return reuse_worktree(env, &pull_request, branch.worktree_path.as_deref().unwrap_or(path), &branch.name).await;
            }
        }
        if existing_path.as_deref() == Some(root.as_str()) {
            return Err(manager("preparePullRequestThread", cwd, IN_MAIN_REPO));
        }
    }

    let created = VcsService::shared()
        .create_worktree(&CreateWorktreeInput { cwd: cwd.into(), ref_name: local, new_ref_name: None, base_ref_name: None, path: None }, context)
        .await?;
    let worktree_path = created["worktree"]["path"].as_str().unwrap_or_default().to_string();
    let branch = created["worktree"]["refName"].as_str().unwrap_or_default().to_string();
    configure_upstream(env, &worktree_path, &pull_request, &branch).await;
    Ok(json!({"pullRequest":pull_request.wire(),"branch":branch,"worktreePath":worktree_path,"isOnPullRequestHead":true}))
}
