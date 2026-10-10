//! Forgejo and Gitea pull requests behind the pull request provider interface (port of
//! `ForgejoPullRequestProvider.ts` and `forgejoPullRequestJson.ts`). Every request is one call of
//! the host's API through `ForgejoApi` (`fj` over HTTP or `tea api`, as `source_control_forgejo`
//! decides), addressed by repository path. Forgejo's public API has no thread replies, thread
//! resolution or draft conversion, so the page is told so through the capabilities.
use crate::{
    pull_request_actions::RunAction,
    pull_request_bitbucket_revisions::parse_diff_file_revisions,
    pull_request_decode::{CheckEntry, dedupe_checks},
    pull_request_github::{PrFailure, Reason, Read},
    pull_request_gitlab_json::{Fields, R, fields, int_of, opt_bool, opt_nullable_int, opt_nullable_object, opt_nullable_str, parse, req_int, req_str},
    pull_request_provider::*,
    source_control_decode::{format_iso, parse_date},
    source_control_forgejo::{ApiInput, ApiOutput, ForgejoCli, ForgejoCliError, Reason as CliReason, RepositoryInput},
    source_control_host::Host,
    source_control_uri::encode_uri_component,
    workspace_entries::collate,
};
use futures_util::{StreamExt, TryStreamExt, future::BoxFuture, stream};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use t3_contracts::*;

/// The page size every list is asked for, which also works with Forgejo's default maximum.
const ITEMS_PER_PAGE: usize = 50;
/// How many rows one conversation read gathers before it is reported as truncated.
const ROW_LIMIT: usize = 500;

/// The one thing this provider needs of the tool: a request, answered. `ForgejoCli` is the real
/// one; a test hands in a scripted one.
pub trait ForgejoApi: Send + Sync {
    fn api<'a>(&'a self, input: ApiInput) -> BoxFuture<'a, Result<ApiOutput, ForgejoCliError>>;
}

impl ForgejoApi for ForgejoCli {
    fn api<'a>(&'a self, input: ApiInput) -> BoxFuture<'a, Result<ApiOutput, ForgejoCliError>> {
        Box::pin(ForgejoCli::api(self, input))
    }
}

// ---- capabilities ---------------------------------------------------------------------------------

pub fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        comment: true,
        actions: vec![PullRequestAction::Merge, PullRequestAction::Close, PullRequestAction::Reopen, PullRequestAction::UpdateBranch],
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        update_methods: Some(vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase]),
        search: false,
        reactions: Some(true),
        labels: Some(true),
        // Forgejo's public API has no thread replies/resolution or draft conversion endpoint.
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: false,
            resolve: false,
            verdicts: vec![PullRequestReviewVerdict::Comment, PullRequestReviewVerdict::Approve, PullRequestReviewVerdict::RequestChanges],
        },
        reviewers: PullRequestReviewerCapabilities { request: true, list_candidates: true },
        edit: Some(PullRequestEditCapabilities { change_request: true, comment: true }),
        stacks: None,
        stack_actions: None,
    }
}

// ---- the API's shapes ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub login: String,
    pub full_name: Option<String>,
    pub avatar_url: Option<String>,
}

fn user(value: &Value) -> R<User> {
    let o = fields(value)?;
    Ok(User { login: req_str(o, "login")?.to_string(), full_name: opt_nullable_str(o, "full_name")?.map(str::to_string), avatar_url: opt_nullable_str(o, "avatar_url")?.map(str::to_string) })
}

/// A key that must be said, as a user or as null.
fn req_user(o: &Fields, key: &str) -> R<Option<User>> {
    match o.get(key) {
        Some(Value::Null) => Ok(None),
        Some(value) => user(value).map(Some),
        None => Err(()),
    }
}

fn req_nullable_str(o: &Fields, key: &str) -> R<Option<String>> {
    match o.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(()),
    }
}

/// `Schema.optional(Schema.String)`: absent, or a string.
fn opt_str(o: &Fields, key: &str) -> R<Option<String>> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub id: i64,
    pub name: String,
    pub color: Option<String>,
    pub description: Option<String>,
}

fn label(value: &Value) -> R<Label> {
    let o = fields(value)?;
    Ok(Label { id: req_int(o, "id")?, name: req_str(o, "name")?.to_string(), color: opt_nullable_str(o, "color")?.map(str::to_string), description: opt_nullable_str(o, "description")?.map(str::to_string) })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Repository {
    pub full_name: String,
    pub push: Option<bool>,
    pub archived: Option<bool>,
    pub allow_merge_commits: Option<bool>,
    pub allow_squash_merge: Option<bool>,
    pub allow_rebase: Option<bool>,
    pub allow_rebase_update: Option<bool>,
}

fn repository(value: &Value) -> R<Repository> {
    let o = fields(value)?;
    let push = match o.get("permissions") {
        None => None,
        Some(permissions) => {
            let permissions = fields(permissions)?;
            match (permissions.get("push"), permissions.get("admin")) {
                (Some(Value::Bool(push)), Some(Value::Bool(_))) => Some(*push),
                _ => return Err(()),
            }
        }
    };
    Ok(Repository {
        full_name: req_str(o, "full_name")?.to_string(),
        push,
        archived: opt_bool(o, "archived")?,
        allow_merge_commits: opt_bool(o, "allow_merge_commits")?,
        allow_squash_merge: opt_bool(o, "allow_squash_merge")?,
        allow_rebase: opt_bool(o, "allow_rebase")?,
        allow_rebase_update: opt_bool(o, "allow_rebase_update")?,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Branch {
    pub reference: String,
    pub sha: String,
    pub repo: Option<Repository>,
}

fn branch(o: &Fields, key: &str) -> R<Branch> {
    let b = fields(o.get(key).ok_or(())?)?;
    let repo = match b.get("repo") {
        Some(Value::Null) => None,
        Some(value) => Some(repository(value)?),
        None => return Err(()),
    };
    Ok(Branch { reference: req_str(b, "ref")?.to_string(), sha: req_str(b, "sha")?.to_string(), repo })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pull {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub html_url: String,
    pub user: Option<User>,
    pub state: String,
    pub draft: Option<bool>,
    pub merged: bool,
    pub mergeable: Option<bool>,
    pub is_locked: Option<bool>,
    pub head: Branch,
    pub base: Branch,
    pub merge_base: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
    pub labels: Option<Vec<Label>>,
    pub requested_reviewers: Option<Vec<User>>,
}

pub fn pull(value: &Value) -> R<Pull> {
    let o = fields(value)?;
    let required_bool = |key: &str| match o.get(key) {
        Some(Value::Bool(flag)) => Ok(*flag),
        _ => Err(()),
    };
    if let Some(comments) = o.get("comments") {
        int_of(comments)?;
    }
    let labels = match o.get("labels") {
        Some(Value::Null) => None,
        Some(Value::Array(entries)) => Some(entries.iter().map(label).collect::<R<Vec<_>>>()?),
        _ => return Err(()),
    };
    let requested_reviewers = match o.get("requested_reviewers") {
        None | Some(Value::Null) => None,
        Some(Value::Array(entries)) => Some(entries.iter().map(user).collect::<R<Vec<_>>>()?),
        _ => return Err(()),
    };
    Ok(Pull {
        number: req_int(o, "number")?,
        title: req_str(o, "title")?.to_string(),
        body: req_nullable_str(o, "body")?,
        html_url: req_str(o, "html_url")?.to_string(),
        user: req_user(o, "user")?,
        state: req_str(o, "state")?.to_string(),
        draft: opt_bool(o, "draft")?,
        merged: required_bool("merged")?,
        mergeable: opt_bool(o, "mergeable")?,
        is_locked: opt_bool(o, "is_locked")?,
        head: branch(o, "head")?,
        base: branch(o, "base")?,
        merge_base: opt_str(o, "merge_base")?,
        created_at: req_str(o, "created_at")?.to_string(),
        updated_at: req_str(o, "updated_at")?.to_string(),
        closed_at: req_nullable_str(o, "closed_at")?,
        merged_at: req_nullable_str(o, "merged_at")?,
        additions: opt_nullable_int(o, "additions")?,
        deletions: opt_nullable_int(o, "deletions")?,
        changed_files: opt_nullable_int(o, "changed_files")?,
        labels,
        requested_reviewers,
    })
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub id: i64,
    pub body: String,
    pub user: Option<User>,
    pub created_at: String,
    pub html_url: Option<String>,
}

fn comment_fields(o: &Fields) -> R<Comment> {
    Ok(Comment { id: req_int(o, "id")?, body: req_str(o, "body")?.to_string(), user: req_user(o, "user")?, created_at: req_str(o, "created_at")?.to_string(), html_url: opt_str(o, "html_url")? })
}

fn comment(value: &Value) -> R<Comment> {
    comment_fields(fields(value)?)
}

#[derive(Debug, Clone)]
pub struct Review {
    pub id: i64,
    pub body: String,
    pub user: Option<User>,
    pub state: String,
    pub submitted_at: String,
    pub html_url: Option<String>,
    pub comments_count: i64,
}

fn review(value: &Value) -> R<Review> {
    let o = fields(value)?;
    Ok(Review {
        id: req_int(o, "id")?,
        body: req_str(o, "body")?.to_string(),
        user: req_user(o, "user")?,
        state: req_str(o, "state")?.to_string(),
        submitted_at: req_str(o, "submitted_at")?.to_string(),
        html_url: opt_str(o, "html_url")?,
        comments_count: req_int(o, "comments_count")?,
    })
}

#[derive(Debug, Clone)]
pub struct ReviewComment {
    pub comment: Comment,
    pub path: String,
    pub position: i64,
    pub original_position: i64,
    pub resolved: bool,
}

fn review_comment(value: &Value) -> R<ReviewComment> {
    let o = fields(value)?;
    let comment = comment_fields(o)?;
    req_str(o, "commit_id")?;
    req_str(o, "original_commit_id")?;
    Ok(ReviewComment { comment, path: req_str(o, "path")?.to_string(), position: req_int(o, "position")?, original_position: req_int(o, "original_position")?, resolved: req_user(o, "resolver")?.is_some() })
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub author: Option<User>,
    pub message: String,
    pub date: String,
    pub parents: Vec<String>,
    pub stats: Option<(i64, i64)>,
}

fn commit(value: &Value) -> R<Commit> {
    let o = fields(value)?;
    let inner = fields(o.get("commit").ok_or(())?)?;
    let committer = fields(inner.get("committer").ok_or(())?)?;
    let parents = match o.get("parents") {
        Some(Value::Array(parents)) => parents.iter().map(|parent| Ok(req_str(fields(parent)?, "sha")?.to_string())).collect::<R<Vec<_>>>()?,
        _ => return Err(()),
    };
    let stats = match opt_nullable_object(o, "stats")? {
        None => None,
        Some(stats) => Some((req_int(stats, "additions")?, req_int(stats, "deletions")?)),
    };
    Ok(Commit { sha: req_str(o, "sha")?.to_string(), author: req_user(o, "author")?, message: req_str(inner, "message")?.to_string(), date: req_str(committer, "date")?.to_string(), parents, stats })
}

#[derive(Debug, Clone)]
pub struct Status {
    pub context: String,
    pub status: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub updated_at: String,
}

fn status(value: &Value) -> R<Status> {
    let o = fields(value)?;
    Ok(Status {
        context: req_str(o, "context")?.to_string(),
        status: req_str(o, "status")?.to_string(),
        description: req_nullable_str(o, "description")?,
        target_url: req_nullable_str(o, "target_url")?,
        updated_at: req_str(o, "updated_at")?.to_string(),
    })
}

#[derive(Debug, Clone)]
pub struct Reaction {
    pub content: String,
    pub user: Option<User>,
}

fn reaction(value: &Value) -> R<Reaction> {
    let o = fields(value)?;
    Ok(Reaction { content: req_str(o, "content")?.to_string(), user: req_user(o, "user")? })
}

// ---- what they become ---------------------------------------------------------------------------------

pub fn actor(user: Option<&User>) -> Option<PullRequestActor> {
    let user = user.filter(|user| !user.login.is_empty())?;
    Some(PullRequestActor {
        is_bot: None,
        login: TrimmedNonEmptyString::new(&user.login).ok()?,
        name: user.full_name.clone().filter(|name| !name.is_empty()),
        avatar_url: user.avatar_url.clone().filter(|url| !url.is_empty()),
    })
}

fn to_iso_utc(value: &str) -> String {
    parse_date(value).map(|date| format_iso(&date)).unwrap_or_else(|| value.to_string())
}

fn state_of(pull: &Pull) -> PullRequestState {
    if pull.merged {
        PullRequestState::Merged
    } else if pull.state == "closed" {
        PullRequestState::Closed
    } else {
        PullRequestState::Open
    }
}

fn is_draft(pull: &Pull) -> bool {
    pull.draft.unwrap_or_else(|| {
        let title = pull.title.to_lowercase();
        title.starts_with("[wip]") || title.starts_with("wip:")
    })
}

pub fn change_request(pull: &Pull) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number: pull.number.max(0) as u64,
        title: pull.title.clone(),
        url: pull.html_url.clone(),
        author: actor(pull.user.as_ref()),
        head_branch: pull.head.reference.clone(),
        head_repository_name_with_owner: Some(pull.head.repo.as_ref().map(|repo| repo.full_name.clone())),
        base_branch: pull.base.reference.clone(),
        state: state_of(pull),
        is_draft: is_draft(pull),
        // A false value also covers drafts and unfinished or failed conflict checks.
        mergeability: if pull.mergeable == Some(true) { PullRequestMergeability::Mergeable } else { PullRequestMergeability::Unknown },
        additions: pull.additions.unwrap_or(0).max(0) as u64,
        deletions: pull.deletions.unwrap_or(0).max(0) as u64,
        created_at: to_iso_utc(&pull.created_at),
        updated_at: to_iso_utc(&pull.updated_at),
        review_request_logins: pull.requested_reviewers.iter().flatten().map(|user| user.login.clone()).collect(),
        labels: pull.labels.iter().flatten().map(|label| PullRequestLabel { name: label.name.clone(), color: label.color.clone() }).collect(),
        review_decision: None,
        checks_state: None,
    }
}

fn plain_comment(comment: &Comment) -> PullRequestComment {
    PullRequestComment {
        id: comment.id.to_string(),
        kind: PullRequestCommentKind::IssueComment,
        author: actor(comment.user.as_ref()),
        body: comment.body.clone(),
        created_at: to_iso_utc(&comment.created_at),
        edited_at: None,
        url: comment.html_url.clone().filter(|url| !url.is_empty()),
        path: None,
        review_state: None,
        reactions: None,
    }
}

fn review_entry(review: &Review) -> PullRequestComment {
    PullRequestComment {
        id: format!("review:{}", review.id),
        kind: PullRequestCommentKind::Review,
        author: actor(review.user.as_ref()),
        body: review.body.clone(),
        created_at: to_iso_utc(&review.submitted_at),
        edited_at: None,
        url: review.html_url.clone().filter(|url| !url.is_empty()),
        path: None,
        review_state: Some(
            match review.state.as_str() {
                "REQUEST_CHANGES" => "CHANGES_REQUESTED",
                "COMMENT" => "COMMENTED",
                other => other,
            }
            .to_string(),
        ),
        reactions: None,
    }
}

fn review_thread(comment: &ReviewComment) -> PullRequestReviewThread {
    let old_side = comment.position == 0 && comment.original_position > 0;
    let line = if old_side { comment.original_position } else { comment.position };
    PullRequestReviewThread {
        id: comment.comment.id.to_string(),
        path: comment.path.clone(),
        line: (line > 0).then_some(line as u64),
        side: if old_side { PullRequestDiffSide::Left } else { PullRequestDiffSide::Right },
        is_resolved: comment.resolved,
        is_outdated: false,
        comments: vec![],
        comment_count: None,
        next_comments_cursor: None,
    }
}

fn thread_comment(comment: &PullRequestComment) -> PullRequestThreadComment {
    PullRequestThreadComment { id: comment.id.clone(), author: comment.author.clone(), body: comment.body.clone(), created_at: comment.created_at.clone(), edited_at: None, url: comment.url.clone(), reactions: comment.reactions.clone() }
}

fn to_commit(commit: &Commit) -> PullRequestCommit {
    PullRequestCommit {
        oid: commit.sha.clone(),
        message_headline: commit.message.split('\n').next().unwrap_or("").to_string(),
        committed_date: to_iso_utc(&commit.date),
        additions: commit.stats.map(|(additions, _)| additions.max(0) as u64),
        deletions: commit.stats.map(|(_, deletions)| deletions.max(0) as u64),
        authors: Some(actor(commit.author.as_ref()).into_iter().collect()),
    }
}

fn checks(statuses: &[Status]) -> Vec<PullRequestCheck> {
    dedupe_checks(
        statuses
            .iter()
            .map(|status| CheckEntry {
                workflow_name: None,
                at: Some(status.updated_at.clone()),
                check: PullRequestCheck {
                    name: if status.context.is_empty() { "check".into() } else { status.context.clone() },
                    description: status.description.clone().filter(|text| !text.is_empty()),
                    url: status.target_url.clone().filter(|text| !text.is_empty()),
                    status: match status.status.as_str() {
                        "success" => PullRequestCheckStatus::Success,
                        "failure" | "error" => PullRequestCheckStatus::Failure,
                        _ => PullRequestCheckStatus::Pending,
                    },
                    required: None,
                },
            })
            .collect(),
    )
}

/// The emoji name Forgejo's API uses for each reaction.
pub fn reaction_name(content: PullRequestReactionContent) -> &'static str {
    use PullRequestReactionContent::*;
    match content {
        ThumbsUp => "+1",
        ThumbsDown => "-1",
        Laugh => "laugh",
        Hooray => "hooray",
        Confused => "confused",
        Heart => "heart",
        Rocket => "rocket",
        Eyes => "eyes",
    }
}

const REACTION_ORDER: [PullRequestReactionContent; 8] = {
    use PullRequestReactionContent::*;
    [ThumbsUp, ThumbsDown, Laugh, Hooray, Confused, Heart, Rocket, Eyes]
};

fn reactions(rows: &[Reaction], viewer: &str) -> Vec<PullRequestReaction> {
    REACTION_ORDER
        .iter()
        .filter_map(|content| {
            let matching: Vec<&Reaction> = rows.iter().filter(|reaction| reaction.content == reaction_name(*content)).collect();
            (!matching.is_empty()).then(|| PullRequestReaction {
                content: *content,
                count: matching.len() as u64,
                actors: matching.iter().filter_map(|reaction| reaction.user.as_ref().map(|user| user.login.clone()).filter(|login| !login.is_empty() && login != viewer)).collect(),
                viewer_has_reacted: matching.iter().any(|reaction| reaction.user.as_ref().is_some_and(|user| user.login == viewer)),
            })
        })
        .collect()
}

/// Review ids differ from the issue-comment ids Forgejo's reactions API uses.
fn review_comment_id(review: &Review) -> Option<String> {
    let url = review.html_url.as_deref().unwrap_or("");
    let at = url.rfind("#issuecomment-")?;
    let id = &url[at + "#issuecomment-".len()..];
    (!id.is_empty() && !id.starts_with('0') && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.to_string())
}

fn permissions(repo: &Repository, pull: &Pull, viewer: &str) -> PullRequestViewerPermissions {
    let capabilities = capabilities();
    let can_write = repo.push.unwrap_or(false);
    let is_author = pull.user.as_ref().is_some_and(|user| user.login == viewer);
    let can_edit = can_write || is_author;
    let active = repo.archived != Some(true);
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: if active { capabilities.actions.into_iter().filter(|action| if matches!(action, PullRequestAction::Merge | PullRequestAction::UpdateBranch) { can_write } else { can_edit }).collect() } else { vec![] },
        comment: active && (pull.is_locked != Some(true) || can_write),
        resolve: false,
        verdicts: if !active { vec![] } else if is_author { vec![PullRequestReviewVerdict::Comment] } else { capabilities.review.verdicts },
        request_reviewers: active && can_edit,
        update_methods: Some(if active && can_write { if repo.allow_rebase_update == Some(true) { vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase] } else { vec![PullRequestUpdateMethod::Merge] } } else { vec![] }),
        labels: Some(active && can_write),
    }
}

// ---- paths and failures ----------------------------------------------------------------------------------

fn repo_path(repository: &str) -> String {
    format!("repos/{}", repository.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/"))
}

fn pull_path(subject: &Subject) -> String {
    format!("{}/pulls/{}", repo_path(&subject.repository), subject.number)
}

fn issue_path(subject: &Subject) -> String {
    format!("{}/issues/{}", repo_path(&subject.repository), subject.number)
}

fn failed(detail: &str) -> PrFailure {
    PrFailure::failed(detail)
}

fn tool_failure(error: ForgejoCliError) -> PrFailure {
    let reason = match error.reason {
        Some(CliReason::MissingCli) => Reason::MissingTool,
        Some(CliReason::Authentication) => Reason::Unauthenticated,
        Some(CliReason::RateLimit) => Reason::RateLimited,
        Some(CliReason::NotFound) => Reason::NotFound,
        _ => Reason::Failed,
    };
    PrFailure { reason, detail: error.detail, retry_at: None }
}

fn unsupported_operation(operation: &str) -> PrFailure {
    failed(&format!("Forgejo does not expose {operation} through its API."))
}

fn target(subject: &Subject) -> RepositoryInput {
    RepositoryInput { cwd: subject.cwd.clone(), context: None, repository: Some(subject.repository.clone()), reference: None, host: Some(subject.host.clone()) }
}

pub struct ForgejoPullRequests {
    api: Arc<dyn ForgejoApi>,
}

impl ForgejoPullRequests {
    pub fn new(host: Host) -> Self {
        Self::with_api(Arc::new(ForgejoCli::new(host)))
    }

    pub fn with_api(api: Arc<dyn ForgejoApi>) -> Self {
        Self { api }
    }

    async fn request(&self, target: RepositoryInput, path: String, method: Option<&'static str>, body: Option<Value>) -> Read<ApiOutput> {
        self.api.api(ApiInput { target, path, method, body }).await.map_err(tool_failure)
    }

    async fn write(&self, subject: &Subject, path: String, method: &'static str, body: Option<Value>) -> Read<()> {
        self.request(target(subject), path, Some(method), body).await.map(|_| ())
    }

    /// One read whose answer must decode as `decode` says.
    async fn read<T>(&self, target: RepositoryInput, path: String, decode: impl FnOnce(&Value) -> R<T>) -> Read<T> {
        let output = self.request(target, path.clone(), None, None).await?;
        parse(&output.stdout).and_then(|value| decode(&value)).map_err(|()| failed("Forgejo returned an invalid response."))
    }

    async fn read_array<T>(&self, target: RepositoryInput, path: String, decode: impl Fn(&Value) -> R<T>) -> Read<Vec<T>> {
        let output = self.request(target, path, None, None).await?;
        match parse(&output.stdout) {
            Ok(Value::Null) => Ok(vec![]),
            Ok(Value::Array(rows)) => rows.iter().map(decode).collect::<R<Vec<_>>>().map_err(|()| failed("Forgejo returned an invalid response.")),
            _ => Err(failed("Forgejo returned an invalid response.")),
        }
    }

    /// One page of a list, and whether there is another: the `link` header says so, or its absence
    /// says nothing and a full page is taken to mean more.
    async fn read_page<T>(&self, target: RepositoryInput, path: String, decode: impl Fn(&Value) -> R<T>, index: usize) -> Read<(Vec<T>, bool)> {
        let path = format!("{path}{}limit={ITEMS_PER_PAGE}&page={index}", if path.contains('?') { "&" } else { "?" });
        let output = self.request(target, path, None, None).await?;
        let rows = match parse(&output.stdout) {
            Ok(Value::Null) => vec![],
            Ok(Value::Array(rows)) => rows.iter().map(decode).collect::<R<Vec<_>>>().map_err(|()| failed("Forgejo returned an invalid response."))?,
            _ => return Err(failed("Forgejo returned an invalid response.")),
        };
        let link = output.stderr.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim().eq_ignore_ascii_case("link").then(|| value.trim().to_string())
        });
        let more = !rows.is_empty() && link.as_ref().is_none_or(|link| has_next(link));
        Ok((rows, more))
    }

    /// Every row of a list up to `limit`, and whether that cut it short.
    async fn page<T>(&self, target: RepositoryInput, path: String, decode: impl Fn(&Value) -> R<T> + Copy, limit: usize) -> Read<(Vec<T>, bool)> {
        let mut items = vec![];
        let mut index = 1;
        while items.len() < limit {
            let (rows, more) = self.read_page(target.clone(), path.clone(), decode, index).await?;
            items.extend(rows);
            if !more {
                return Ok((items, false));
            }
            index += 1;
        }
        Ok((items, true))
    }

    async fn get_pull(&self, subject: &Subject) -> Read<Pull> {
        self.read(target(subject), pull_path(subject), pull).await
    }

    async fn get_repo(&self, subject: &Subject) -> Read<Repository> {
        self.read(target(subject), repo_path(&subject.repository), repository).await
    }

    async fn get_viewer(&self, cwd: &str, repository: Option<&str>, host: &str) -> Read<String> {
        let target = RepositoryInput { cwd: cwd.into(), context: None, repository: repository.map(str::to_string), reference: None, host: Some(host.into()) };
        self.read(target, "user".into(), |value| Ok(req_str(fields(value)?, "login")?.to_string())).await
    }

    async fn statuses(&self, subject: &Subject, sha: &str) -> Read<Vec<Status>> {
        Ok(self.page(target(subject), format!("{}/statuses/{}?sort=recentupdate", repo_path(&subject.repository), encode_uri_component(sha)), status, ROW_LIMIT).await?.0)
    }

    async fn permissions_of(&self, subject: &Subject) -> Read<PullRequestViewerPermissions> {
        let (repo, pull, viewer) = tokio::try_join!(Box::pin(self.get_repo(subject)), Box::pin(self.get_pull(subject)), Box::pin(self.get_viewer(&subject.cwd, Some(&subject.repository), &subject.host)))?;
        Ok(permissions(&repo, &pull, &viewer))
    }

    async fn do_activity(&self, subject: &Subject) -> Read<PullRequestActivity> {
        let (cwd, repo) = (subject.cwd.as_str(), &subject.repository);
        // Issue comments ignore page/limit; fetch this unpaginated endpoint once.
        let comments = async {
            let items = self.read_array(target(subject), format!("{}/comments", issue_path(subject)), comment).await?;
            let truncated = items.len() > ROW_LIMIT;
            Ok::<_, PrFailure>((items.into_iter().take(ROW_LIMIT).collect::<Vec<_>>(), truncated))
        };
        let (comments, reviews, commits, issue_reactions, viewer) = tokio::try_join!(
            Box::pin(comments),
            Box::pin(self.page(target(subject), format!("{}/reviews", pull_path(subject)), review, ROW_LIMIT)),
            Box::pin(self.page(target(subject), format!("{}/commits", pull_path(subject)), commit, ROW_LIMIT)),
            Box::pin(self.page(target(subject), format!("{}/reactions", issue_path(subject)), reaction, ROW_LIMIT)),
            Box::pin(self.get_viewer(cwd, Some(repo), &subject.host))
        )?;
        let comment_paths: Vec<String> = reviews.0.iter().filter(|review| review.comments_count > 0 && review.state != "PENDING").map(|review| format!("{}/reviews/{}/comments", pull_path(subject), review.id)).collect();
        let inline_reads = stream::iter(comment_paths.into_iter().map(|path| Box::pin(self.read_array(target(subject), path, review_comment))))
            .buffered(4)
            .try_collect::<Vec<_>>()
            .await?;
        let all_inline: Vec<ReviewComment> = inline_reads.into_iter().flatten().collect();
        let inline: Vec<&ReviewComment> = all_inline.iter().take(ROW_LIMIT).collect();
        let mut entries: Vec<(PullRequestComment, Option<String>)> = vec![];
        entries.extend(comments.0.iter().map(|comment| (plain_comment(comment), Some(comment.id.to_string()))));
        entries.extend(reviews.0.iter().filter(|review| review.state != "PENDING" && review.state != "REQUEST_REVIEW").map(|review| (review_entry(review), review_comment_id(review))));
        entries.extend(inline.iter().map(|comment| (PullRequestComment { kind: PullRequestCommentKind::ReviewComment, path: Some(comment.path.clone()), ..plain_comment(&comment.comment) }, Some(comment.comment.id.to_string()))));
        let viewer_name = viewer.as_str();
        let mut enriched: Vec<PullRequestComment> = stream::iter(entries.into_iter().map(|(entry, reaction_id)| {
            Box::pin(async move {
                let rows = match reaction_id {
                    None => vec![],
                    Some(id) => self.read_array(target(subject), format!("{}/issues/comments/{id}/reactions", repo_path(repo)), reaction).await?,
                };
                Ok::<_, PrFailure>(PullRequestComment { reactions: Some(reactions(&rows, viewer_name)), ..entry })
            })
        }))
        .buffered(4)
        .try_collect()
        .await?;
        let mut by_id: HashMap<String, PullRequestComment> = HashMap::new();
        for entry in &enriched {
            by_id.insert(entry.id.clone(), entry.clone());
        }
        enriched.sort_by(|left, right| collate(&left.created_at, &right.created_at));
        Ok(PullRequestActivity {
            author: None,
            reviewers: None,
            comment_count: enriched.len() as u64,
            comments_truncated: comments.1 || reviews.1 || all_inline.len() > inline.len(),
            comments: enriched,
            review_threads_truncated: None,
            review_threads: inline
                .iter()
                .map(|comment| PullRequestReviewThread { comments: vec![thread_comment(&by_id.get(&comment.comment.id.to_string()).cloned().unwrap_or_else(|| plain_comment(&comment.comment)))], ..review_thread(comment) })
                .collect(),
            commits: commits.0.iter().map(to_commit).collect(),
            reactions: Some(reactions(&issue_reactions.0, &viewer)),
        })
    }

    async fn content(&self, subject: &Subject, repository: &str, reference: &str, path: &str) -> Read<String> {
        let target = RepositoryInput { repository: Some(repository.into()), ..target(subject) };
        let route = format!("{}/contents/{}?ref={}", repo_path(repository), path.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/"), encode_uri_component(reference));
        let encoded = self
            .read(target, route, |value| {
                let o = fields(value)?;
                if req_str(o, "encoding")? != "base64" {
                    return Err(());
                }
                Ok(req_str(o, "content")?.to_string())
            })
            .await?;
        Ok(decode_base64(&encoded))
    }
}

/// `Buffer.from(text, "base64").toString("utf8")`: characters outside the alphabet are skipped and
/// padding is optional.
fn decode_base64(text: &str) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
    // Decoding ends at the first padding character, and anything outside the alphabet is skipped.
    let cleaned: String = text.chars().take_while(|c| *c != '=').filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/')).collect();
    let usable = &cleaned[..cleaned.len() - (cleaned.len() % 4 == 1) as usize];
    String::from_utf8_lossy(&STANDARD_NO_PAD.decode(usable).unwrap_or_default()).into_owned()
}

/// `/rel="?next"?/i` in a `link` header.
fn has_next(link: &str) -> bool {
    static NEXT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    NEXT.get_or_init(|| regex::Regex::new(r#"(?i)rel="?next"?"#).unwrap()).is_match(link)
}

impl PullRequestProvider for ForgejoPullRequests {
    fn kind(&self) -> SourceControlProviderKind {
        SourceControlProviderKind::Forgejo
    }

    fn capabilities(&self) -> PullRequestCapabilities {
        capabilities()
    }

    fn viewer<'a>(&'a self, cwd: &'a str, host: &'a str) -> BoxFuture<'a, Read<String>> {
        Box::pin(self.get_viewer(cwd, None, host))
    }

    fn list<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<ChangeRequestPage>> {
        Box::pin(async move {
            let repository = request.repositories.first().cloned().unwrap_or_default();
            let offset = request.cursor.as_ref().map_or(0, |cursor| cursor.delivered as usize);
            let state = match request.state {
                PullRequestListState::Open => "open",
                PullRequestListState::Closed | PullRequestListState::Merged => "closed",
                PullRequestListState::All => "all",
            };
            let query = format!("{}/pulls?state={state}&sort=recentupdate", repo_path(&repository));
            let place = RepositoryInput { cwd: request.cwd.clone(), context: None, repository: Some(repository.clone()), reference: None, host: Some(request.host.clone()) };
            let decode = |value: &Value| -> R<Option<Pull>> { if value.is_null() { Ok(None) } else { pull(value).map(Some) } };
            // Self-hosted servers may cap pages below 50. Establish their actual page size before
            // translating the service's row offset into an API page number.
            let first = self.read_page(place.clone(), query.clone(), decode, 1).await?;
            let page_size = if first.0.is_empty() { ITEMS_PER_PAGE } else { first.0.len() };
            let first_index = offset / page_size + 1;
            let (mut consumed, mut more, mut items) = (0usize, true, vec![]);
            let limit = request.limit as usize;
            let mut index = first_index;
            while more && consumed < limit {
                let (rows, has_more) = if index == 1 { first.clone() } else { self.read_page(place.clone(), query.clone(), decode, index).await? };
                let start = if index == first_index { offset % page_size } else { 0 };
                let before = consumed;
                for row in rows.iter().skip(start) {
                    if consumed >= limit {
                        break;
                    }
                    consumed += 1;
                    if let Some(row) = row {
                        items.push(change_request(row));
                    }
                }
                more = has_more || rows.len().saturating_sub(start) > consumed - before;
                index += 1;
            }
            Ok(ChangeRequestPage { items, truncated: more, cursor_advance: Some(consumed), continues: true })
        })
    }

    fn summary<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestSummary>> {
        Box::pin(async move {
            let pull = self.get_pull(subject).await?;
            let row = change_request(&pull);
            Ok(ChangeRequestSummary {
                number: row.number,
                title: row.title,
                url: row.url,
                head_branch: row.head_branch,
                base_branch: row.base_branch,
                state: row.state,
                is_draft: Some(row.is_draft),
                closed_at: Some(pull.closed_at.as_deref().map(to_iso_utc)).flatten(),
                merged_at: Some(pull.merged_at.as_deref().map(to_iso_utc)).flatten(),
                updated_at: row.updated_at,
                author: Some(row.author),
                additions: Some(row.additions),
                deletions: Some(row.deletions),
                changed_files: None,
                review_decision: None,
                checks_state: None,
                mergeability: Some(row.mergeability),
                stack: None,
            })
        })
    }

    fn checks<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestChecks>> {
        Box::pin(async move {
            let pull = self.get_pull(subject).await?;
            let statuses = self.statuses(subject, &pull.head.sha).await?;
            Ok(PullRequestChecks { state: state_of(&pull), checks: checks(&statuses) })
        })
    }

    fn detail<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestDetail>> {
        Box::pin(async move {
            let (pull, repo, viewer) = tokio::try_join!(Box::pin(self.get_pull(subject)), Box::pin(self.get_repo(subject)), Box::pin(self.get_viewer(&subject.cwd, Some(&subject.repository), &subject.host)))?;
            let statuses = self.statuses(subject, &pull.head.sha).await?;
            let base_comparison = match pull.merge_base.as_deref() {
                None | Some("") => PullRequestBaseComparison::Unknown,
                Some(merge_base) if merge_base == pull.base.sha => PullRequestBaseComparison::UpToDate,
                Some(_) => PullRequestBaseComparison::Behind,
            };
            Ok(ChangeRequestDetail {
                row: change_request(&pull),
                head_sha: None,
                body: pull.body.clone().unwrap_or_default(),
                changed_files: pull.changed_files.unwrap_or(0).max(0) as u64,
                merged_at: pull.merged_at.as_deref().map(to_iso_utc),
                closed_at: pull.closed_at.as_deref().map(to_iso_utc),
                reviewers: pull.requested_reviewers.iter().flatten().filter_map(|user| actor(Some(user))).collect(),
                checks: checks(&statuses),
                merge_capabilities: PullRequestMergeCapabilities { merge: repo.allow_merge_commits.unwrap_or(true), squash: repo.allow_squash_merge.unwrap_or(true), rebase: repo.allow_rebase.unwrap_or(true) },
                viewer_permissions: permissions(&repo, &pull, &viewer),
                base_comparison: Some(base_comparison),
                behind_by: None,
                auto_merge_enabled: None,
                auto_merge_method: None,
                workflow_approvals_required: None,
            })
        })
    }

    fn activity<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestActivity>> {
        Box::pin(self.do_activity(subject))
    }

    fn viewer_permissions<'a>(&'a self, subject: &'a Subject, _include_update_branch: bool) -> BoxFuture<'a, Read<PullRequestViewerPermissions>> {
        Box::pin(self.permissions_of(subject))
    }

    fn diff<'a>(&'a self, subject: &'a Subject, _cursor: Option<&'a str>, commit: Option<&'a str>) -> BoxFuture<'a, Read<PullRequestDiffResult>> {
        Box::pin(async move {
            let path = match commit {
                Some(commit) => format!("{}/git/commits/{}.diff", repo_path(&subject.repository), encode_uri_component(commit)),
                None => format!("{}.diff", pull_path(subject)),
            };
            let output = self.request(target(subject), path, None, None).await?;
            Ok(PullRequestDiffResult { patch: output.stdout, truncated: false, next_cursor: None, omitted_file_stats: None })
        })
    }

    fn diff_file_contents<'a>(
        &'a self,
        subject: &'a Subject,
        commit: Option<&'a str>,
        change_type: PullRequestDiffChangeType,
        old_path: &'a str,
        new_path: &'a str,
    ) -> BoxFuture<'a, Read<PullRequestDiffFileContentsResult>> {
        Box::pin(async move {
            let pull = self.get_pull(subject).await?;
            let found = match commit {
                Some(commit) => Some(self.read(target(subject), format!("{}/git/commits/{}", repo_path(&subject.repository), encode_uri_component(commit)), crate::pull_request_forgejo::commit).await?),
                None => None,
            };
            let old_ref = match &found {
                Some(found) => found.parents.first().cloned(),
                None => Some(pull.merge_base.clone().filter(|base| !base.is_empty()).unwrap_or_else(|| pull.base.sha.clone())),
            };
            let new_ref = commit.map_or(pull.head.sha.as_str(), |commit| commit);
            let old = async {
                match (&old_ref, change_type) {
                    (Some(old_ref), change) if change != PullRequestDiffChangeType::New && !old_ref.is_empty() => self.content(subject, &subject.repository, old_ref, old_path).await,
                    _ => Ok(String::new()),
                }
            };
            let new = async {
                if change_type == PullRequestDiffChangeType::Deleted {
                    return Ok(String::new());
                }
                let repository = pull.head.repo.as_ref().map_or(subject.repository.as_str(), |repo| repo.full_name.as_str());
                self.content(subject, repository, new_ref, new_path).await
            };
            let (old_contents, new_contents) = tokio::try_join!(Box::pin(old), Box::pin(new))?;
            Ok(PullRequestDiffFileContentsResult { old_contents, new_contents })
        })
    }

    fn file_revisions<'a>(&'a self, subject: &'a Subject, paths: &'a [String]) -> BoxFuture<'a, Read<FileRevisions>> {
        Box::pin(async move {
            let output = self.request(target(subject), format!("{}.diff", pull_path(subject)), None, None).await?;
            let mut revisions = parse_diff_file_revisions(&output.stdout);
            for path in paths {
                revisions.entry(path.clone()).or_insert_with(String::new);
            }
            Ok(FileRevisions { revisions, complete: true })
        })
    }

    fn run_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            match run.action {
                PullRequestAction::Merge => {
                    let method = match run.merge_method {
                        Some(PullRequestMergeMethod::Squash) => "squash",
                        Some(PullRequestMergeMethod::Rebase) => "rebase",
                        _ => "merge",
                    };
                    self.write(subject, format!("{}/merge", pull_path(subject)), "POST", Some(json!({"Do": method}))).await
                }
                PullRequestAction::Close | PullRequestAction::Reopen => {
                    self.write(subject, pull_path(subject), "PATCH", Some(json!({"state": if run.action == PullRequestAction::Close { "closed" } else { "open" }}))).await
                }
                PullRequestAction::UpdateBranch => {
                    let style = if run.update_method == Some(PullRequestUpdateMethod::Rebase) { "rebase" } else { "merge" };
                    self.write(subject, format!("{}/update?style={style}", pull_path(subject)), "POST", None).await
                }
                other => Err(unsupported_operation(&serde_json::to_value(other).ok().and_then(|name| name.as_str().map(str::to_string)).unwrap_or_default())),
            }
        })
    }

    fn update<'a>(&'a self, subject: &'a Subject, title: Option<&'a str>, body: Option<&'a str>) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let mut fields = serde_json::Map::new();
            if let Some(title) = title {
                fields.insert("title".into(), json!(title));
            }
            if let Some(body) = body {
                fields.insert("body".into(), json!(body));
            }
            self.write(subject, pull_path(subject), "PATCH", Some(Value::Object(fields))).await
        })
    }

    fn comment<'a>(&'a self, subject: &'a Subject, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.write(subject, format!("{}/comments", issue_path(subject)), "POST", Some(json!({"body": body}))))
    }

    fn update_comment<'a>(&'a self, subject: &'a Subject, comment_id: &'a str, _kind: PullRequestCommentUpdateKind, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.write(subject, format!("{}/issues/comments/{}", repo_path(&subject.repository), encode_uri_component(comment_id)), "PATCH", Some(json!({"body": body}))))
    }

    fn submit_review<'a>(&'a self, subject: &'a Subject, verdict: PullRequestReviewVerdict, body: &'a str, comments: &'a [PullRequestReviewCommentDraft]) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let pull = self.get_pull(subject).await?;
            let drafts: Vec<Value> = comments
                .iter()
                .map(|draft| {
                    let (old, old_line, new_line) = match &draft.position {
                        PullRequestReviewPosition::Added { new_line } => (false, 0, *new_line),
                        PullRequestReviewPosition::Deleted { old_line } => (true, *old_line, 0),
                        PullRequestReviewPosition::Context { old_line, new_line, side } => (*side == PullRequestDiffSide::Left, *old_line, *new_line),
                    };
                    json!({
                        "path": if old { draft.old_path.as_ref().unwrap_or(&draft.path).as_str() } else { draft.path.as_str() },
                        "body": draft.body,
                        "old_position": if old { old_line } else { 0 },
                        "new_position": if old { 0 } else { new_line },
                    })
                })
                .collect();
            let event = match verdict {
                PullRequestReviewVerdict::Approve => "APPROVED",
                PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
                PullRequestReviewVerdict::Comment => "COMMENT",
            };
            self.write(subject, format!("{}/reviews", pull_path(subject)), "POST", Some(json!({"event": event, "body": body, "commit_id": pull.head.sha, "comments": drafts}))).await
        })
    }

    fn reviewer_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestReviewerCandidateList>> {
        Box::pin(async move {
            let (pull, users) = tokio::try_join!(Box::pin(self.get_pull(subject)), Box::pin(self.page(target(subject), format!("{}/assignees", repo_path(&subject.repository)), user, ROW_LIMIT)))?;
            let author = pull.user.as_ref().map(|user| user.login.as_str());
            Ok(PullRequestReviewerCandidateList {
                candidates: users
                    .0
                    .iter()
                    .filter(|user| Some(user.login.as_str()) != author)
                    .filter_map(|user| {
                        let actor = actor(Some(user))?;
                        Some(PullRequestReviewerCandidate {
                            is_bot: None,
                            login: actor.login.as_str().to_string(),
                            name: actor.name,
                            avatar_url: actor.avatar_url,
                            id: user.login.clone(),
                            kind: PullRequestReviewerKind::User,
                            is_requested: pull.requested_reviewers.iter().flatten().any(|reviewer| reviewer.login == user.login),
                        })
                    })
                    .collect(),
                truncated: users.1,
            })
        })
    }

    fn set_reviewer_request<'a>(&'a self, subject: &'a Subject, reviewers: &'a [PullRequestReviewerRequestTarget], requested: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let ids: Vec<&str> = reviewers.iter().map(|reviewer| reviewer.id.as_str()).collect();
            self.write(subject, format!("{}/requested_reviewers", pull_path(subject)), if requested { "POST" } else { "DELETE" }, Some(json!({"reviewers": ids}))).await
        })
    }

    fn label_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestLabelCandidateList>> {
        Box::pin(async move {
            let (pull, labels) = tokio::try_join!(Box::pin(self.get_pull(subject)), Box::pin(self.page(target(subject), format!("{}/labels", repo_path(&subject.repository)), label, ROW_LIMIT)))?;
            Ok(PullRequestLabelCandidateList {
                candidates: labels
                    .0
                    .iter()
                    .map(|label| PullRequestLabelCandidate {
                        name: label.name.clone(),
                        color: label.color.clone(),
                        description: label.description.clone(),
                        is_applied: pull.labels.iter().flatten().any(|applied| applied.id == label.id),
                    })
                    .collect(),
                truncated: labels.1,
            })
        })
    }

    fn set_labels<'a>(&'a self, subject: &'a Subject, wanted: &'a [&'a str], applied: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let (labels, _) = self.page(target(subject), format!("{}/labels", repo_path(&subject.repository)), label, ROW_LIMIT).await?;
            let selected: Vec<&Label> = labels.iter().filter(|label| wanted.contains(&label.name.as_str())).collect();
            if selected.len() != wanted.len() {
                return Err(failed("One or more requested labels could not be found."));
            }
            if applied {
                return self.write(subject, format!("{}/labels", issue_path(subject)), "POST", Some(json!({"labels": selected.iter().map(|label| label.id).collect::<Vec<_>>()}))).await;
            }
            for label in selected {
                self.write(subject, format!("{}/labels/{}", issue_path(subject), label.id), "DELETE", None).await?;
            }
            Ok(())
        })
    }

    fn reply_to_thread<'a>(&'a self, _subject: &'a Subject, _thread_id: &'a str, _body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(unsupported_operation("thread replies")) })
    }

    fn set_reaction<'a>(&'a self, subject: &'a Subject, subject_id: Option<&'a str>, content: PullRequestReactionContent, reacted: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let mut comment_id = subject_id.map(str::to_string);
            if let Some(id) = comment_id.as_deref().and_then(|id| id.strip_prefix("review:")) {
                if id.is_empty() || id.starts_with('0') || !id.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(failed("Invalid Forgejo review ID."));
                }
                let found = self.read(target(subject), format!("{}/reviews/{id}", pull_path(subject)), review).await?;
                comment_id = review_comment_id(&found);
                if comment_id.is_none() {
                    return Err(failed("Forgejo did not return a comment ID for this review."));
                }
            }
            if let Some(id) = comment_id.as_deref().filter(|id| !id.is_empty())
                && (id.starts_with('0') || !id.bytes().all(|byte| byte.is_ascii_digit()))
            {
                return Err(failed("Invalid Forgejo comment ID."));
            }
            let path = match comment_id.as_deref().filter(|id| !id.is_empty()) {
                Some(id) => format!("{}/issues/comments/{id}/reactions", repo_path(&subject.repository)),
                None => format!("{}/reactions", issue_path(subject)),
            };
            self.write(subject, path, if reacted { "POST" } else { "DELETE" }, Some(json!({"content": reaction_name(content)}))).await
        })
    }

    fn set_thread_resolution<'a>(&'a self, _subject: &'a Subject, _thread_id: &'a str, _resolved: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(unsupported_operation("thread resolution")) })
    }
}
