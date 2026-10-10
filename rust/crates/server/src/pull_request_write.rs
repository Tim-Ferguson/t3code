//! What can be written to a GitHub pull request: remarks, replies, resolutions, reactions, the
//! description, a review and the reader's viewed marks (the write half of
//! `GitHubPullRequestCli.ts` and `PullRequestService.ts`). Every write goes through the same
//! gates the original has: the host must offer it, the connection must hold the source-control
//! write scope (the transport checks that before any handler runs), and the signed-in account's
//! own standing on the pull request is read fresh and must allow it. Nothing here widens what a
//! call can do.
//!
//! Actions, reviewer requests and labels are in `pull_request_actions.rs`.
use crate::{
    pull_request_decode as decode,
    pull_request_github::{self as github, GitHub, PrFailure, Read, Target},
    pull_request_queries as queries,
    pull_request_service::{Env, PrError, PrResult, PullRequestService, Resolved},
    source_control_github_api::{self as api, GraphQlInput, Method, RestInput},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::{Mutex, OnceLock}};
use t3_contracts::*;

const ADD_COMMENT_GRAPHQL_MUTATION: &str = "mutation($subjectId: ID!, $body: String!) {\n  addComment(input: { subjectId: $subjectId, body: $body }) { clientMutationId }\n}";
const NODE_ID_CACHE_CAPACITY: usize = 128;

/// A pull request keeps its node id for life, so it is remembered rather than re-read: a reader
/// ticking files viewed would otherwise pay a GraphQL round trip per press.
fn node_ids() -> &'static Mutex<(HashMap<String, String>, Vec<String>)> {
    static HELD: OnceLock<Mutex<(HashMap<String, String>, Vec<String>)>> = OnceLock::new();
    HELD.get_or_init(Mutex::default)
}

/// Forgets every remembered node id. The original keeps them per client instance, so a test that
/// replays one pull request through several scenarios needs each to start from nothing.
pub fn clear_node_ids() {
    *node_ids().lock().unwrap() = Default::default();
}

fn verdict_label(verdict: PullRequestReviewVerdict) -> &'static str {
    match verdict {
        PullRequestReviewVerdict::Comment => "review",
        PullRequestReviewVerdict::Approve => "approve",
        PullRequestReviewVerdict::RequestChanges => "request changes on",
    }
}

fn refuse<T>(operation: &str, detail: &str) -> PrResult<T> {
    Err(PrError::operation(operation, detail))
}

/// `buildReviewSubmission`: the whole review as one request body, which is how GitHub keeps it
/// invisible until sent.
pub fn build_review_submission(verdict: PullRequestReviewVerdict, body: &str, comments: &[PullRequestReviewCommentDraft]) -> Value {
    let event = match verdict {
        PullRequestReviewVerdict::Comment => "COMMENT",
        PullRequestReviewVerdict::Approve => "APPROVE",
        PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
    };
    let comments: Vec<Value> = comments
        .iter()
        .map(|comment| {
            let (line, side) = match &comment.position {
                PullRequestReviewPosition::Added { new_line } => (*new_line, "RIGHT"),
                PullRequestReviewPosition::Deleted { old_line } => (*old_line, "LEFT"),
                PullRequestReviewPosition::Context { old_line, new_line, side } => match side {
                    PullRequestDiffSide::Left => (*old_line, "LEFT"),
                    PullRequestDiffSide::Right => (*new_line, "RIGHT"),
                },
            };
            json!({"path":comment.path,"line":line,"side":side,"body":comment.body})
        })
        .collect();
    json!({"event":event,"body":body,"comments":comments})
}

/// `buildSetFilesViewedGraphQlMutation`: one aliased mutation per file, in one document.
pub fn build_set_files_viewed_mutation(files: &[PullRequestSetFileViewed]) -> Option<(String, Value)> {
    if files.is_empty() {
        return None;
    }
    let parameters: Vec<String> = (0..files.len()).map(|index| format!("$path{index}: String!")).collect();
    let fields: Vec<String> = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            format!(
                "  f{index}: {}(input: {{ pullRequestId: $pullRequestId, path: $path{index} }}) {{ clientMutationId }}",
                if file.viewed { "markFileAsViewed" } else { "unmarkFileAsViewed" }
            )
        })
        .collect();
    let mut variables = serde_json::Map::new();
    for (index, file) in files.iter().enumerate() {
        variables.insert(format!("path{index}"), json!(file.path));
    }
    Some((format!("mutation($pullRequestId: ID!, {}) {{\n{}\n}}", parameters.join(", "), fields.join("\n")), Value::Object(variables)))
}

impl GitHub {
    pub(crate) async fn mutate(&self, gh_host: &str, operation: &str, query: &str, variables: Value) -> Read<()> {
        Box::pin(api::graphql(
            self.host(),
            GraphQlInput { host: gh_host.into(), operation: operation.into(), query: query.into(), variables: Some(variables), ..Default::default() },
        ))
        .await?;
        Ok(())
    }

    /// The pull request's own node id, which a mutation against the pull request itself is
    /// addressed by.
    pub(crate) async fn node_id(&self, target: &Target, operation: &str) -> Read<String> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        let key = format!("{} {owner}/{name} {}", target.host, target.number);
        {
            let mut held = node_ids().lock().unwrap();
            if let Some(id) = held.0.get(&key).cloned() {
                // Put back at the end on every hit, so what falls out is the pull request nobody
                // has looked at rather than the one being ticked through.
                held.1.retain(|existing| *existing != key);
                held.1.push(key);
                return Ok(id);
            }
        }
        let raw = Box::pin(api::graphql(
            self.host(),
            GraphQlInput {
                host: target.host.clone(),
                operation: operation.into(),
                query: queries::PULL_REQUEST_NODE_ID_GRAPHQL_QUERY.into(),
                variables: Some(json!({"owner":owner,"name":name,"number":target.number})),
                allow_reserve: Some(true),
                ..Default::default()
            },
        ))
        .await?;
        let id = serde_json::from_str::<Value>(raw.trim())
            .ok()
            .and_then(|value| value.pointer("/data/repository/pullRequest/id").and_then(Value::as_str).map(str::to_string))
            .ok_or_else(|| PrFailure::unreadable(operation))?;
        let mut held = node_ids().lock().unwrap();
        if held.0.len() >= NODE_ID_CACHE_CAPACITY && !held.1.is_empty() {
            let oldest = held.1.remove(0);
            held.0.remove(&oldest);
        }
        held.1.push(key.clone());
        held.0.insert(key, id.clone());
        Ok(id)
    }

    /// Whether a client-given subject actually hangs off the pull request the request names. A
    /// subject id is trusted to be whatever node it names, and that node can belong to any pull
    /// request on the host, so the mutation would write wherever the id really belongs.
    async fn subject_belongs(&self, target: &Target, subject_id: &str, operation: &str) -> Read<bool> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        let raw = Box::pin(api::graphql(
            self.host(),
            GraphQlInput {
                host: target.host.clone(),
                operation: operation.into(),
                query: queries::REACTION_SUBJECT_PULL_REQUEST_GRAPHQL_QUERY.into(),
                variables: Some(json!({"owner":owner,"name":name,"number":target.number,"subjectId":subject_id})),
                allow_reserve: Some(true),
                ..Default::default()
            },
        ))
        .await?;
        let value: Value = serde_json::from_str(raw.trim()).map_err(|_| PrFailure::unreadable(operation))?;
        let data = value.get("data").filter(|data| data.is_object()).ok_or_else(|| PrFailure::unreadable(operation))?;
        let (Some(repository), Some(node)) = (data.get("repository"), data.get("node")) else { return Err(PrFailure::unreadable(operation)) };
        let expected = repository.pointer("/pullRequest/id").and_then(Value::as_str);
        let actual = if node.is_null() { None } else { node.pointer("/pullRequest/id").and_then(Value::as_str).or_else(|| node.get("id").and_then(Value::as_str)) };
        Ok(expected.is_some() && actual.is_some() && expected == actual)
    }

    pub async fn comment(&self, target: &Target, body: &str) -> Read<()> {
        let subject = self.node_id(target, "commentOnPullRequest").await?;
        self.mutate(&target.host, "commentOnPullRequest", ADD_COMMENT_GRAPHQL_MUTATION, json!({"subjectId":subject,"body":body})).await
    }

    pub async fn update_comment(&self, target: &Target, comment_id: &str, kind: PullRequestCommentUpdateKind, body: &str) -> Read<()> {
        if !self.subject_belongs(target, comment_id, "updateComment").await? {
            return Err(PrFailure::failed("The named subject did not belong to the named pull request."));
        }
        let query = match kind {
            PullRequestCommentUpdateKind::IssueComment => queries::UPDATE_ISSUE_COMMENT_GRAPHQL_MUTATION,
            PullRequestCommentUpdateKind::ReviewComment => queries::UPDATE_REVIEW_COMMENT_GRAPHQL_MUTATION,
        };
        self.mutate(&target.host, "updateComment", query, json!({"commentId":comment_id,"body":body})).await
    }

    pub async fn reply_to_thread(&self, target: &Target, thread_id: &str, body: &str) -> Read<()> {
        self.mutate(&target.host, "replyToReviewThread", queries::REVIEW_THREAD_REPLY_GRAPHQL_MUTATION, json!({"threadId":thread_id,"body":body})).await
    }

    pub async fn set_thread_resolution(&self, target: &Target, thread_id: &str, resolved: bool) -> Read<()> {
        let query = if resolved { queries::RESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION } else { queries::UNRESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION };
        self.mutate(&target.host, "setReviewThreadResolution", query, json!({"threadId":thread_id})).await
    }

    pub async fn set_reaction(&self, target: &Target, subject_id: Option<&str>, content: PullRequestReactionContent, reacted: bool) -> Read<()> {
        let subject = match subject_id {
            None => self.node_id(target, "setReaction").await?,
            Some(given) => {
                if !self.subject_belongs(target, given, "setReaction").await? {
                    return Err(PrFailure::failed("The named subject did not belong to the named pull request."));
                }
                given.to_string()
            }
        };
        let query = if reacted { queries::ADD_REACTION_GRAPHQL_MUTATION } else { queries::REMOVE_REACTION_GRAPHQL_MUTATION };
        self.mutate(&target.host, "setReaction", query, json!({"subjectId":subject,"content":decode::github_reaction_content(content)})).await
    }

    pub async fn update_pull_request(&self, target: &Target, title: Option<&str>, body: Option<&str>) -> Read<()> {
        let id = self.node_id(target, "updatePullRequest").await?;
        // A field the caller did not name is left out of the request entirely, so GitHub keeps
        // the words that are there rather than being asked for an empty one.
        let mut variables = json!({"pullRequestId":id});
        if let Some(title) = title {
            variables["title"] = json!(title);
        }
        if let Some(body) = body {
            variables["body"] = json!(body);
        }
        self.mutate(&target.host, "updatePullRequest", queries::UPDATE_PULL_REQUEST_GRAPHQL_MUTATION, variables).await
    }

    pub async fn submit_review(&self, target: &Target, verdict: PullRequestReviewVerdict, body: &str, comments: &[PullRequestReviewCommentDraft]) -> Read<()> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        // The whole review is one request, so nothing is visible to anyone else until the
        // verdict is sent.
        Box::pin(api::rest(
            self.host(),
            RestInput {
                host: target.host.clone(),
                operation: "submitReview".into(),
                method: Method::Post,
                path: format!("repos/{owner}/{name}/pulls/{}/reviews", target.number),
                body: Some(build_review_submission(verdict, body, comments)),
                ..Default::default()
            },
        ))
        .await?;
        Ok(())
    }

    pub async fn set_files_viewed(&self, target: &Target, files: &[PullRequestSetFileViewed]) -> Read<()> {
        let Some((query, mut variables)) = build_set_files_viewed_mutation(files) else { return Ok(()) };
        let id = self.node_id(target, "setPullRequestFilesViewed").await?;
        variables["pullRequestId"] = json!(id);
        self.mutate(&target.host, "setPullRequestFilesViewed", &query, variables).await
    }
}

impl PullRequestService {
    /// What the signed-in account may do with the pull request, read fresh: the page hides what
    /// a viewer may not do, but a request that arrived without passing through the page, or
    /// after the access behind it was withdrawn, must not be handed to the host on the client's
    /// word.
    async fn viewer_permissions_of(&self, env: &Env, resolved: &Resolved, operation: &str) -> PrResult<PullRequestViewerPermissions> {
        let provider = self.provider_of(env, resolved)?;
        provider
            .viewer_permissions(&resolved.subject(), false)
            .await
            .map_err(|failure| PrError::from_failure(operation, failure, resolved.project.kind))
    }

    /// Runs a write as an interactive caller, then forgets what is held for the pull request and
    /// tells the readers to fetch again.
    pub(crate) async fn written<T>(&self, resolved: &Resolved, operation: &str, write: impl std::future::Future<Output = Read<T>>) -> PrResult<T> {
        let kind = resolved.project.kind;
        let result = api::with_allow_reserve(write).await.map_err(|failure| PrError::from_failure(operation, failure, kind))?;
        self.after_mutation(resolved).await;
        Ok(result)
    }

    pub async fn comment(&self, env: &Env, input: &PullRequestCommentInput, namespace: Option<String>) -> PrResult<()> {
        // The contract keeps the body verbatim because it is markdown, so the "did the user
        // actually write something" check lives here.
        if input.body.trim().is_empty() {
            return refuse("comment", "A comment cannot be empty.");
        }
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        if !provider.capabilities().comment {
            return refuse("comment", "This host cannot post a comment on a change request.");
        }
        let viewer = api::with_allow_reserve(self.viewer_permissions_of(env, &resolved, "comment")).await?;
        if !viewer.comment {
            return refuse("comment", "You need write access on this repository to comment on a change request.");
        }
        self.written(&resolved, "comment", provider.comment(&resolved.subject(), &input.body)).await
    }

    /// Rewriting the change request's own words, and rewriting a remark, are left to the host to
    /// allow or refuse: every host lets the person who wrote something rewrite it whatever
    /// access they have otherwise, and none of them reports that as a permission.
    pub async fn update(&self, env: &Env, input: &PullRequestUpdateInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        if !provider.capabilities().edit.is_some_and(|edit| edit.change_request) {
            return refuse("update", "This host cannot rewrite a change request.");
        }
        if input.title.is_none() && input.body.is_none() {
            return refuse("update", "Nothing was changed.");
        }
        self.written(&resolved, "update", provider.update(&resolved.subject(), input.title.as_ref().map(|title| title.as_str()), input.body.as_deref())).await
    }

    pub async fn update_comment(&self, env: &Env, input: &PullRequestCommentUpdateInput, namespace: Option<String>) -> PrResult<()> {
        if input.body.trim().is_empty() {
            return refuse("updateComment", "A comment cannot be empty.");
        }
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        if !provider.capabilities().edit.is_some_and(|edit| edit.comment) {
            return refuse("updateComment", "This host cannot rewrite a comment.");
        }
        self.written(&resolved, "updateComment", provider.update_comment(&resolved.subject(), input.comment_id.as_str(), input.kind, &input.body)).await
    }

    pub async fn submit_review(&self, env: &Env, input: &PullRequestSubmitReviewInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        let capabilities = provider.capabilities();
        // The surface hides what a host cannot do, and this refuses it as well.
        if !capabilities.review.verdicts.contains(&input.verdict) {
            return refuse("submitReview", &format!("This host cannot {} a change request.", verdict_label(input.verdict)));
        }
        if !input.comments.is_empty() && !capabilities.review.inline_comment {
            return refuse("submitReview", "This host cannot comment on a line of a change request.");
        }
        // A verdict with nothing attached to it is a request every host rejects, and doing so
        // here says which of the two is missing rather than reporting the host's refusal.
        if input.verdict != PullRequestReviewVerdict::Approve && input.body.trim().is_empty() && input.comments.is_empty() {
            return refuse("submitReview", "A review needs a summary or at least one comment.");
        }
        let viewer = api::with_allow_reserve(self.viewer_permissions_of(env, &resolved, "submitReview")).await?;
        if !viewer.verdicts.contains(&input.verdict) {
            return refuse("submitReview", &format!("You need write access on this repository to {} a change request.", verdict_label(input.verdict)));
        }
        if !input.comments.is_empty() && !viewer.comment {
            return refuse("submitReview", "You need write access on this repository to comment on a line of a change request.");
        }
        self.written(&resolved, "submitReview", provider.submit_review(&resolved.subject(), input.verdict, &input.body, &input.comments)).await
    }

    pub async fn reply_to_thread(&self, env: &Env, input: &PullRequestThreadReplyInput, namespace: Option<String>) -> PrResult<()> {
        if input.body.trim().is_empty() {
            return refuse("replyToThread", "A reply cannot be empty.");
        }
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        if !provider.capabilities().review.reply {
            return refuse("replyToThread", "This host cannot reply to a review conversation.");
        }
        let viewer = api::with_allow_reserve(self.viewer_permissions_of(env, &resolved, "replyToThread")).await?;
        if !viewer.comment {
            return refuse("replyToThread", "You need write access on this repository to reply to a review conversation.");
        }
        self.written(&resolved, "replyToThread", provider.reply_to_thread(&resolved.subject(), input.thread_id.as_str(), &input.body)).await
    }

    pub async fn set_thread_resolution(&self, env: &Env, input: &PullRequestThreadResolutionInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        if !provider.capabilities().review.resolve {
            return refuse("setThreadResolution", "This host cannot resolve a review conversation.");
        }
        let viewer = api::with_allow_reserve(self.viewer_permissions_of(env, &resolved, "setThreadResolution")).await?;
        if !viewer.resolve {
            return refuse(
                "setThreadResolution",
                "You need write access on this repository, or to have opened this change request, to resolve a review conversation.",
            );
        }
        self.written(&resolved, "setThreadResolution", provider.set_thread_resolution(&resolved.subject(), input.thread_id.as_str(), input.resolved)).await
    }

    /// Reacting is gated on the host alone: every host with reactions takes one from whoever can
    /// read the change request.
    pub async fn set_reaction(&self, env: &Env, input: &PullRequestReactionInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        if provider.capabilities().reactions != Some(true) {
            return refuse("setReaction", "This host has no reactions.");
        }
        self.written(&resolved, "setReaction", provider.set_reaction(&resolved.subject(), input.subject_id.as_ref().map(|subject| subject.as_str()), input.content, input.reacted)).await
    }

    /// The reader's own bookkeeping: ticking a file off says nothing about the change request,
    /// and dropping a 300-file diff on every checkbox is the whole cost of the feature, so only
    /// the viewed marks are forgotten.
    pub async fn set_files_viewed(&self, env: &Env, input: &PullRequestSetFilesViewedInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        match provider.capabilities().viewed_files {
            Some(PullRequestViewedFilesStore::Host) => {
                let result = api::with_allow_reserve(provider.set_files_viewed(&resolved.subject(), &input.files)).await;
                result.map_err(|failure| PrError::from_failure("setFilesViewed", failure, resolved.project.kind))?;
            }
            Some(PullRequestViewedFilesStore::Environment) => Box::pin(self.environment_set_files_viewed(env, &resolved, &input.files)).await?,
            None => return refuse("setFilesViewed", "This host does not track which files a reader has seen."),
        }
        self.bump_files_viewed(&resolved);
        Ok(())
    }
}
