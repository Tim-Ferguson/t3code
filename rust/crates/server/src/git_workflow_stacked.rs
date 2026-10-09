//! `git.runStackedAction` (port of `runStackedAction` in `GitManager.ts`): commit, push and open a
//! pull request in one streamed workflow, reporting each step as it happens.
//!
//! The steps run in a fixed order and stop at the first failure, leaving what already happened in
//! place: a branch that was created stays, a commit that was made stays, a branch that was pushed
//! stays. Whatever the outcome the checkout's cached status is dropped, and a failure is reported
//! as an `action_failed` event naming the phase it happened in before the call itself fails.
//!
//! Text the original has a model write is behind [`TextGeneration`]: the commit message when the
//! caller sent none (and the feature-branch name that goes with it), and the title and description
//! of a pull request that is created (the input has no field for either). A model failing to write
//! them fails the action at that step with the text generation error, as in the original; what
//! came before it stays done.
use crate::{
    git_workflow::{WorkflowResult, ensure_git, manager},
    git_workflow_status::{HeadContext, PrInfo, branch_of_remote_ref, find_open_pr, resolve_head_context},
    pull_request_service::Env,
    source_control_bitbucket::sanitize_branch_fragment,
    source_control_provider::{CreateChangeRequestInput, DefaultBranchInput},
    source_control_registry::SourceControlRegistry,
    vcs_service::VcsService,
    vcs_status::StatusSource,
};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::SourceControlProviderKind;
use t3_vcs::{
    core::GitCore,
    error::OutputStream,
    progress::HookFinished,
    remote::{CommitStage, GitCommitOptions, GitCommitProgress},
};

const COMMIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_PROGRESS_TEXT_LENGTH: usize = 500;
const SHORT_SHA_LENGTH: usize = 7;
const TOAST_DESCRIPTION_MAX: usize = 72;

// ---------------------------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackedAction {
    Commit,
    Push,
    CreatePr,
    CommitPush,
    CommitPushPr,
}

impl StackedAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Push => "push",
            Self::CreatePr => "create_pr",
            Self::CommitPush => "commit_push",
            Self::CommitPushPr => "commit_push_pr",
        }
    }

    fn commits(self) -> bool {
        matches!(self, Self::Commit | Self::CommitPush | Self::CommitPushPr)
    }
}

/// `GitRunStackedActionInput`, already checked against the contract's bounds.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackedInput {
    pub action_id: String,
    pub cwd: String,
    pub action: StackedAction,
    #[serde(default)]
    pub commit_message: Option<String>,
    #[serde(default)]
    pub feature_branch: Option<bool>,
    #[serde(default)]
    pub file_paths: Option<Vec<String>>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Text the original has a model write
// ---------------------------------------------------------------------------------------------

pub struct CommitContext<'a> {
    pub cwd: &'a str,
    /// The thread the action was started from, whose project's settings steer the writing.
    pub thread_id: Option<&'a str>,
    pub branch: Option<&'a str>,
    /// `git diff --name-status` of what would be committed, cut at 8,000 characters.
    pub staged_summary: &'a str,
    /// The patch, cut at 50,000 characters.
    pub staged_patch: &'a str,
    /// Also name a feature branch for the change.
    pub include_branch: bool,
}

pub struct GeneratedCommit {
    pub subject: String,
    pub body: String,
    pub branch: Option<String>,
}

pub struct PrContext<'a> {
    pub cwd: &'a str,
    pub thread_id: Option<&'a str>,
    /// What the base is called locally, as git can read the repository's template at it.
    pub base_range_ref: &'a str,
    pub provider_kind: SourceControlProviderKind,
    pub base_branch: &'a str,
    pub head_branch: &'a str,
    pub commit_summary: &'a str,
    pub diff_summary: &'a str,
    pub diff_patch: &'a str,
}

pub struct GeneratedPr {
    pub title: String,
    pub body: String,
}

/// What `textGeneration/` is to the original. The model, the writing-style policy and the
/// repository's pull request template are the implementation's to bring.
pub trait TextGeneration: Send + Sync {
    fn commit_message<'a>(&'a self, context: CommitContext<'a>) -> BoxFuture<'a, WorkflowResult<GeneratedCommit>>;
    fn pr_content<'a>(&'a self, context: PrContext<'a>) -> BoxFuture<'a, WorkflowResult<GeneratedPr>>;
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// What a change request is called on a host (`getChangeRequestTerminologyForKind`).
pub struct Terms {
    pub short_label: &'static str,
    pub singular: &'static str,
}

pub fn terms_for(kind: SourceControlProviderKind) -> Terms {
    match kind {
        SourceControlProviderKind::Gitlab => Terms { short_label: "MR", singular: "merge request" },
        SourceControlProviderKind::Unknown => Terms { short_label: "change request", singular: "change request" },
        _ => Terms { short_label: "PR", singular: "pull request" },
    }
}

/// `sanitizeFeatureBranchName`: a `feature/…` ref name for any text.
pub fn sanitize_feature_branch_name(raw: &str) -> String {
    let sanitized = sanitize_branch_fragment(raw);
    if sanitized.contains('/') {
        return if sanitized.starts_with("feature/") { sanitized } else { format!("feature/{sanitized}") };
    }
    format!("feature/{sanitized}")
}

/// `resolveAutoFeatureBranchName`: a `feature/…` name no existing branch has, by numeric suffix.
pub fn resolve_auto_feature_branch_name(existing: &[String], preferred: Option<&str>) -> String {
    let preferred = preferred.map(str::trim).filter(|preferred| !preferred.is_empty()).unwrap_or("feature/update");
    let base = sanitize_feature_branch_name(preferred);
    let taken: std::collections::HashSet<String> = existing.iter().map(|name| name.to_lowercase()).collect();
    if !taken.contains(&base) {
        return base;
    }
    let mut suffix = 2;
    while taken.contains(&format!("{base}-{suffix}")) {
        suffix += 1;
    }
    format!("{base}-{suffix}")
}

/// `parseCustomCommitMessage`: the first line is the subject, the rest the body.
pub fn parse_custom_commit_message(raw: &str) -> Option<(String, String)> {
    let normalized = raw.replace("\r\n", "\n");
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return None;
    }
    let mut lines = normalized.split('\n');
    let subject = lines.next().unwrap_or("").trim().to_string();
    if subject.is_empty() {
        return None;
    }
    Some((subject, lines.collect::<Vec<_>>().join("\n").trim().to_string()))
}

/// `sanitizeCommitMessage`: what a model wrote, made safe to commit.
pub fn sanitize_generated_commit(subject: &str, body: &str) -> (String, String) {
    let first = subject.trim().split('\n').next().unwrap_or("").trim_end_matches('\r').trim();
    let trimmed = first.trim_end_matches('.').trim();
    let safe = if trimmed.is_empty() { "Update project files".to_string() } else { trimmed.chars().take(72).collect::<String>().trim_end().to_string() };
    (safe, body.trim().to_string())
}

fn limit_context(value: &str, max_chars: usize) -> String {
    // The original counts UTF-16 units; for the text git prints that is the same number but for
    // astral characters, which this cut keeps whole instead of halving.
    if value.encode_utf16().count() <= max_chars {
        return value.to_string();
    }
    let cut: String = value.chars().take(max_chars).collect();
    format!("{cut}\n\n[truncated]")
}

/// `sanitizeProgressText`.
pub fn sanitize_progress_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_PROGRESS_TEXT_LENGTH {
        return Some(trimmed.to_string());
    }
    Some(trimmed.chars().take(MAX_PROGRESS_TEXT_LENGTH).collect::<String>().trim_end().to_string())
}

fn truncate_text(value: Option<&str>) -> Option<String> {
    let value = value.filter(|value| !value.is_empty())?;
    if value.chars().count() <= TOAST_DESCRIPTION_MAX {
        return Some(value.to_string());
    }
    Some(format!("{}...", value.chars().take(TOAST_DESCRIPTION_MAX - 3).collect::<String>().trim_end()))
}

fn short_sha(sha: Option<&str>) -> Option<String> {
    sha.filter(|sha| !sha.is_empty()).map(|sha| sha.chars().take(SHORT_SHA_LENGTH).collect())
}

fn titled(title: String, description: Option<String>) -> (String, Option<String>) {
    (title, description)
}

/// `summarizeGitActionResult`: the toast's title and description.
pub fn summarize(result: &Value, terms: &Terms) -> (String, Option<String>) {
    let pr = &result["pr"];
    let status = pr["status"].as_str().unwrap_or("");
    if status == "created" || status == "opened_existing" {
        let number = pr["number"].as_u64().map(|number| format!(" #{number}")).unwrap_or_default();
        let verb = if status == "created" { "Created" } else { "Opened" };
        return titled(format!("{verb} {}{number}", terms.short_label), truncate_text(pr["title"].as_str()));
    }
    let push = &result["push"];
    let commit = &result["commit"];
    if push["status"] == "pushed" {
        let sha = short_sha(commit["commitSha"].as_str());
        let branch = push["upstreamBranch"].as_str().or_else(|| push["branch"].as_str());
        let sha_part = sha.map(|sha| format!(" {sha}")).unwrap_or_default();
        let branch_part = branch.map(|branch| format!(" to {branch}")).unwrap_or_default();
        return titled(format!("Pushed{sha_part}{branch_part}"), truncate_text(commit["subject"].as_str()));
    }
    if commit["status"] == "created" {
        let title = short_sha(commit["commitSha"].as_str()).map_or_else(|| "Committed changes".to_string(), |sha| format!("Committed {sha}"));
        return titled(title, truncate_text(commit["subject"].as_str()));
    }
    ("Done".into(), None)
}

/// The call to action on the toast: push what was only committed, view the pull request that
/// exists, or offer to open one for a branch that was just pushed (`buildCompletionToast`).
pub fn call_to_action(action: StackedAction, result: &Value, open_pr_url: Option<&str>, current_branch_is_default: bool, terms: &Terms) -> Value {
    let pr_status = result["pr"]["status"].as_str().unwrap_or("");
    if action == StackedAction::Commit && result["commit"]["status"] == "created" {
        return json!({"kind":"run_action","label":"Push","action":{"kind":"push"}});
    }
    if action != StackedAction::Commit {
        if let Some(url) = open_pr_url.filter(|url| !url.is_empty()) {
            if !current_branch_is_default || pr_status == "created" || pr_status == "opened_existing" {
                return json!({"kind":"open_pr","label":format!("View {}", terms.short_label),"url":url});
            }
        }
    }
    if matches!(action, StackedAction::Push | StackedAction::CommitPush) && result["push"]["status"] == "pushed" && !current_branch_is_default {
        return json!({"kind":"run_action","label":format!("Create {}", terms.short_label),"action":{"kind":"create_pr"}});
    }
    json!({"kind":"none"})
}

// ---------------------------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------------------------

/// Where events go: the stream the client reads. Sending never waits, and a client that has gone
/// away simply stops receiving.
pub type Events = tokio::sync::mpsc::UnboundedSender<Value>;

#[derive(Clone)]
struct Emitter {
    events: Events,
    action_id: String,
    cwd: String,
    action: StackedAction,
}

impl Emitter {
    fn emit(&self, mut event: Value) {
        event["actionId"] = json!(self.action_id);
        event["cwd"] = json!(self.cwd);
        event["action"] = json!(self.action.as_str());
        let _ = self.events.send(event);
    }

    fn phase(&self, phase: &str, label: impl Into<String>) {
        self.emit(json!({"kind":"phase_started","phase":phase,"label":label.into()}));
    }
}

// ---------------------------------------------------------------------------------------------
// The workflow
// ---------------------------------------------------------------------------------------------

struct Suggestion {
    subject: String,
    body: String,
    branch: Option<String>,
}

impl Suggestion {
    fn from_custom(subject: String, body: String, include_branch: bool) -> Self {
        let branch = include_branch.then(|| sanitize_feature_branch_name(&subject));
        Self { subject, body, branch }
    }
}

/// `resolveCommitAndBranchSuggestion`: what would be committed, and the words for it. `None`
/// when there is nothing to commit. A message the caller sent is used as written; otherwise it is
/// generated.
async fn resolve_suggestion(
    core: &GitCore,
    text: &dyn TextGeneration,
    cwd: &str,
    thread_id: Option<&str>,
    branch: Option<&str>,
    commit_message: Option<&str>,
    include_branch: bool,
    file_paths: Option<&[String]>,
) -> WorkflowResult<Option<Suggestion>> {
    let Some(context) = core.prepare_commit_context(Path::new(cwd), file_paths).await? else { return Ok(None) };
    if let Some((subject, body)) = commit_message.and_then(parse_custom_commit_message) {
        return Ok(Some(Suggestion::from_custom(subject, body, include_branch)));
    }
    let staged_summary = limit_context(&context.staged_summary, 8_000);
    let staged_patch = limit_context(&context.staged_patch, 50_000);
    let generated = text.commit_message(CommitContext { cwd, thread_id, branch, staged_summary: &staged_summary, staged_patch: &staged_patch, include_branch }).await?;
    let (subject, body) = sanitize_generated_commit(&generated.subject, &generated.body);
    Ok(Some(Suggestion { subject, body, branch: generated.branch }))
}

/// `runFeatureBranchStep`: a new branch for the change, off the current one, checked out.
async fn feature_branch_step(core: &GitCore, text: &dyn TextGeneration, input: &StackedInput, branch: Option<&str>) -> WorkflowResult<(Value, Option<Suggestion>)> {
    let path = Path::new(&input.cwd);
    let suggestion = resolve_suggestion(core, text, &input.cwd, input.thread_id.as_deref(), branch, input.commit_message.as_deref(), true, input.file_paths.as_deref()).await?;
    let Some(suggestion) = suggestion else {
        return Err(manager("runFeatureBranchStep", &input.cwd, "Cannot create a feature branch because there are no changes to commit."));
    };
    let preferred = suggestion.branch.clone().unwrap_or_else(|| sanitize_feature_branch_name(&suggestion.subject));
    let existing = core.list_local_branch_names(path).await?;
    let resolved = resolve_auto_feature_branch_name(&existing, Some(&preferred));
    core.create_ref(path, &resolved, false).await?;
    core.switch_ref(path, &resolved).await?;
    Ok((json!({"status":"created","name":resolved}), Some(suggestion)))
}

/// `runCommitStep`: stage, commit, and report the hooks as they run.
async fn commit_step(
    core: &GitCore,
    text: &dyn TextGeneration,
    emitter: &Emitter,
    input: &StackedInput,
    branch: Option<&str>,
    resolved: Option<Suggestion>,
) -> WorkflowResult<Value> {
    let suggestion = match resolved {
        Some(suggestion) => Some(suggestion),
        None => {
            let needs_generation = input.commit_message.as_deref().is_none_or(|message| message.trim().is_empty());
            if needs_generation {
                emitter.phase("commit", "Generating commit message...");
            }
            resolve_suggestion(core, text, &input.cwd, input.thread_id.as_deref(), branch, input.commit_message.as_deref(), false, input.file_paths.as_deref()).await?
        }
    };
    let Some(suggestion) = suggestion else { return Ok(json!({"status":"skipped_no_changes"})) };
    emitter.phase("commit", "Committing...");

    let current_hook: Arc<Mutex<Option<String>>> = Arc::default();
    let progress = GitCommitProgress {
        on_output_line: Some({
            let (emitter, current) = (emitter.clone(), current_hook.clone());
            Arc::new(move |stream: OutputStream, text: &str| {
                if let Some(text) = sanitize_progress_text(text) {
                    emitter.emit(json!({"kind":"hook_output","hookName":current.lock().unwrap().clone(),"stream":stream.as_str(),"text":text}));
                }
            })
        }),
        on_hook_started: Some({
            let (emitter, current) = (emitter.clone(), current_hook.clone());
            Arc::new(move |name: &str| {
                *current.lock().unwrap() = Some(name.to_string());
                emitter.emit(json!({"kind":"hook_started","hookName":name}));
            })
        }),
        on_hook_finished: Some({
            let (emitter, current) = (emitter.clone(), current_hook.clone());
            Arc::new(move |finished: &HookFinished| {
                let mut current = current.lock().unwrap();
                if current.as_deref() == Some(finished.hook_name.as_str()) {
                    *current = None;
                }
                emitter.emit(json!({"kind":"hook_finished","hookName":finished.hook_name,"exitCode":finished.exit_code,"durationMs":finished.duration_ms}));
            })
        }),
    };
    let sha = core
        .commit(
            Path::new(&input.cwd),
            &suggestion.subject,
            &suggestion.body,
            GitCommitOptions { timeout: Some(COMMIT_TIMEOUT), progress: Some(progress), stage: Some(CommitStage { file_paths: input.file_paths.clone() }) },
        )
        .await?;
    // A hook git never reported finishing is taken to have passed.
    if let Some(hook) = current_hook.lock().unwrap().take() {
        emitter.emit(json!({"kind":"hook_finished","hookName":hook,"exitCode":0,"durationMs":null}));
    }
    Ok(json!({"status":"created","commitSha":sha,"subject":suggestion.subject}))
}

/// `resolveBaseBranch`: the branch the pull request is opened against.
async fn base_branch(env: &Env, core: &GitCore, cwd: &str, branch: &str, upstream_ref: Option<&str>, head: &HeadContext) -> String {
    let path = Path::new(cwd);
    if let Ok(Some(configured)) = core.read_config_value(path, &format!("branch.{branch}.gh-merge-base")).await {
        if !configured.is_empty() {
            return configured;
        }
    }
    if let Some(upstream) = upstream_ref.filter(|_| !head.is_cross_repository) {
        let upstream_branch = branch_of_remote_ref(upstream, head.remote_name.as_deref());
        if !upstream_branch.is_empty() && upstream_branch != branch {
            return upstream_branch;
        }
    }
    if let Ok(provider) = SourceControlRegistry::new(env.host.clone()).resolve(cwd).await {
        if let Ok(Some(default)) = provider.get_default_branch(DefaultBranchInput { cwd: cwd.into(), context: None }).await {
            if !default.is_empty() {
                return default;
            }
        }
    }
    // The provider lookup can fail for reasons unrelated to the branch, so what the remote itself
    // records is next: a repository whose default branch is master would otherwise get a base
    // branch that does not exist.
    if let Ok(remote) = core.resolve_primary_remote_name(path).await {
        if let Ok(Some(default)) = core.resolve_default_branch_name(path, &remote).await {
            if !default.is_empty() {
                return default;
            }
        }
    }
    "main".into()
}

/// `resolveBaseRangeRef`: the base as git has it locally, so a stale local branch does not widen
/// the diff.
async fn base_range_ref(core: &GitCore, cwd: &str, base: &str) -> String {
    let path = Path::new(cwd);
    let Ok(remote) = core.resolve_primary_remote_name(path).await else { return base.into() };
    core.resolve_remote_tracking_commit(path, base, &remote).await.map(|(sha, _)| sha).unwrap_or_else(|_| base.into())
}

fn pr_value(pr: &PrInfo, status: &str) -> Value {
    json!({"status":status,"url":pr.url,"number":pr.number,"baseBranch":pr.base,"headBranch":pr.head,"title":pr.title})
}

/// `runPrStep`: the pull request for the branch, an existing one if there is one.
async fn pr_step(env: &Env, core: &GitCore, text: &dyn TextGeneration, emitter: &Emitter, cwd: &str, thread_id: Option<&str>, fallback_branch: Option<&str>) -> WorkflowResult<Value> {
    let path = Path::new(cwd);
    let provider = SourceControlRegistry::new(env.host.clone()).resolve(cwd).await?;
    let terms = terms_for(provider.kind());
    let details = core.status_details(path).await?;
    let Some(branch) = details.branch.clone().or_else(|| fallback_branch.map(str::to_string)) else {
        return Err(manager("runPrStep", cwd, "Cannot create a pull request from detached HEAD."));
    };
    if !details.has_upstream {
        return Err(manager("runPrStep", cwd, "Current branch has not been pushed. Push before creating a PR."));
    }
    let head = resolve_head_context(core, cwd, &branch, details.upstream_ref.as_deref(), None).await;
    if let Some(existing) = find_open_pr(&env.host, cwd, &head).await? {
        return Ok(pr_value(&existing, "opened_existing"));
    }
    let base = base_branch(env, core, cwd, &branch, details.upstream_ref.as_deref(), &head).await;
    emitter.phase("pr", format!("Generating {} content...", terms.short_label));
    let range_ref = base_range_ref(core, cwd, &base).await;
    let range = core.read_range_context(path, &range_ref).await?;
    let (commit_summary, diff_summary, diff_patch) = (limit_context(&range.commit_summary, 20_000), limit_context(&range.diff_summary, 20_000), limit_context(&range.diff_patch, 60_000));
    let generated = text
        .pr_content(PrContext { cwd, thread_id, base_range_ref: &range_ref, provider_kind: provider.kind(), base_branch: &base, head_branch: &head.head_branch, commit_summary: &commit_summary, diff_summary: &diff_summary, diff_patch: &diff_patch })
        .await?;

    // The body travels in a file of its own, which is removed whatever the host answers.
    let body_file = std::env::temp_dir().join(format!("t3code-pr-body-{}-{}.md", std::process::id(), uuid::Uuid::new_v4()));
    tokio::fs::write(&body_file, &generated.body).await.map_err(|_| manager("runPrStep", cwd, "Failed to write pull request body temp file."))?;
    emitter.phase("pr", format!("Creating {}...", terms.singular));
    let created = provider
        .create_change_request(CreateChangeRequestInput {
            cwd: cwd.into(),
            context: None,
            source: None,
            target: None,
            base_ref_name: base.clone(),
            head_selector: head.preferred_head_selector(),
            title: generated.title.clone(),
            body_file: body_file.to_string_lossy().into_owned(),
        })
        .await;
    let _ = tokio::fs::remove_file(&body_file).await;
    created?;

    match find_open_pr(&env.host, cwd, &head).await? {
        Some(found) => Ok(pr_value(&found, "created")),
        None => Ok(json!({"status":"created","baseBranch":base,"headBranch":head.head_branch,"title":generated.title})),
    }
}

/// `buildCompletionToast`.
async fn completion_toast(env: &Env, core: &GitCore, cwd: &str, result: &Value, action: StackedAction) -> WorkflowResult<Value> {
    let kind = SourceControlRegistry::new(env.host.clone()).resolve(cwd).await.map_or(SourceControlProviderKind::Unknown, |provider| provider.kind());
    let terms = terms_for(kind);
    let (title, description) = summarize(result, &terms);
    let mut branch_context: Option<(String, Option<String>, bool)> = None;
    let mut default_branch = false;
    if action != StackedAction::Commit {
        let fin = core.status_details(Path::new(cwd)).await?;
        if let Some(branch) = fin.branch.clone() {
            branch_context = Some((branch, fin.upstream_ref.clone(), fin.has_upstream));
            default_branch = fin.is_default_branch;
        }
    }
    let pr = &result["pr"];
    let explicit = (matches!(pr["status"].as_str(), Some("created" | "opened_existing"))).then(|| pr["url"].as_str().filter(|url| !url.is_empty()).map(str::to_string)).flatten();
    let lookup = matches!(action, StackedAction::CommitPush | StackedAction::Push)
        && result["push"]["status"] == "pushed"
        && result["branch"]["status"] != "created"
        && !default_branch
        && explicit.is_none()
        && branch_context.as_ref().is_some_and(|(_, _, has_upstream)| *has_upstream);
    let mut latest: Option<String> = None;
    if lookup {
        if let Some((branch, upstream, _)) = &branch_context {
            let head = resolve_head_context(core, cwd, branch, upstream.as_deref(), None).await;
            latest = find_open_pr(&env.host, cwd, &head).await.ok().flatten().map(|found| found.url);
        }
    }
    let open_url = latest.or(explicit);
    let mut toast = json!({"title":title,"cta":call_to_action(action, result, open_url.as_deref(), default_branch, &terms)});
    if let Some(description) = description {
        toast["description"] = json!(description);
    }
    Ok(toast)
}

async fn run(env: &Env, text: &dyn TextGeneration, emitter: &Emitter, input: &StackedInput, phase: &mut Option<&'static str>) -> WorkflowResult<Value> {
    let core = VcsService::shared().core();
    let path = Path::new(&input.cwd);
    let action = input.action;
    let initial = core.status_details(path).await?;
    let wants_commit = action.commits();
    let feature_branch = input.feature_branch == Some(true);
    let wants_push = matches!(action, StackedAction::Push | StackedAction::CommitPush | StackedAction::CommitPushPr)
        || (action == StackedAction::CreatePr && (!initial.has_upstream || initial.ahead_count > 0));
    let wants_pr = matches!(action, StackedAction::CreatePr | StackedAction::CommitPushPr);

    if feature_branch && !wants_commit {
        return Err(manager("runStackedAction", &input.cwd, "Feature-branch checkout is only supported for commit actions."));
    }
    if action == StackedAction::CreatePr && initial.has_working_tree_changes {
        return Err(manager("runStackedAction", &input.cwd, "Commit local changes before creating a PR."));
    }
    let mut phases = vec![];
    if feature_branch {
        phases.push("branch");
    }
    if wants_commit {
        phases.push("commit");
    }
    if wants_push {
        phases.push("push");
    }
    if wants_pr {
        phases.push("pr");
    }
    emitter.emit(json!({"kind":"action_started","phases":phases}));

    if !feature_branch && wants_push && initial.branch.is_none() {
        return Err(manager("runStackedAction", &input.cwd, "Cannot push from detached HEAD."));
    }
    if !feature_branch && wants_pr && initial.branch.is_none() {
        return Err(manager("runStackedAction", &input.cwd, "Cannot create a pull request from detached HEAD."));
    }

    let mut resolved: Option<Suggestion> = None;
    let branch_step = if feature_branch {
        *phase = Some("branch");
        emitter.phase("branch", "Preparing feature branch...");
        let (step, suggestion) = feature_branch_step(&core, text, input, initial.branch.as_deref()).await?;
        resolved = suggestion;
        step
    } else {
        json!({"status":"skipped_not_requested"})
    };
    let current_branch = branch_step["name"].as_str().map(str::to_string).or(initial.branch.clone());

    let commit = if wants_commit {
        *phase = Some("commit");
        Box::pin(commit_step(&core, text, emitter, input, current_branch.as_deref(), resolved)).await?
    } else {
        json!({"status":"skipped_not_requested"})
    };

    let push = if wants_push {
        emitter.phase("push", "Pushing...");
        *phase = Some("push");
        let pushed = core.push_current_branch(path, current_branch.as_deref(), None).await?;
        let mut value = json!({"status":match pushed.status { t3_vcs::remote::PushStatus::Pushed => "pushed", t3_vcs::remote::PushStatus::SkippedUpToDate => "skipped_up_to_date" },"branch":pushed.branch});
        if let Some(upstream) = &pushed.upstream_branch {
            value["upstreamBranch"] = json!(upstream);
        }
        if let Some(set) = pushed.set_upstream {
            value["setUpstream"] = json!(set);
        }
        value
    } else {
        json!({"status":"skipped_not_requested"})
    };

    let pr = if wants_pr {
        let kind = SourceControlRegistry::new(env.host.clone()).resolve(&input.cwd).await.map_or(SourceControlProviderKind::Unknown, |provider| provider.kind());
        emitter.phase("pr", format!("Preparing {}...", terms_for(kind).short_label));
        *phase = Some("pr");
        Box::pin(pr_step(env, &core, text, emitter, &input.cwd, input.thread_id.as_deref(), current_branch.as_deref())).await?
    } else {
        json!({"status":"skipped_not_requested"})
    };

    let mut result = json!({"action":action.as_str(),"branch":branch_step,"commit":commit,"push":push,"pr":pr});
    result["toast"] = completion_toast(env, &core, &input.cwd, &result, action).await?;
    emitter.emit(json!({"kind":"action_finished","result":result}));
    Ok(result)
}

/// Runs the action, sending each event as it happens, and returns the result the final
/// `action_finished` event carries. The checkout's cached status is dropped whatever the outcome.
pub async fn run_stacked_action(env: &Env, text: &dyn TextGeneration, input: &StackedInput, events: Events) -> WorkflowResult<Value> {
    ensure_git("GitWorkflowService.runStackedAction", &input.cwd).await?;
    let emitter = Emitter { events, action_id: input.action_id.clone(), cwd: input.cwd.clone(), action: input.action };
    let mut phase: Option<&'static str> = None;
    let result = Box::pin(run(env, text, &emitter, input, &mut phase)).await;
    VcsService::shared().status().source().invalidate_status(&input.cwd).await;
    if let Err(error) = &result {
        let message = error.message();
        emitter.emit(json!({"kind":"action_failed","phase":phase,"message":if message.trim().is_empty() { "The action failed.".to_string() } else { message }}));
    }
    result
}
