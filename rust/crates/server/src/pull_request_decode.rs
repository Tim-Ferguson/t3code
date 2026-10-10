//! GitHub GraphQL answers decoded into the pull request wire shapes (port of the decoders in
//! `gitHubPullRequestJson.ts`). Every decoder takes the raw response text and either returns the
//! shape or says the answer is unusable; a field the original schema marks optional is read
//! leniently, and a row that does not decode is skipped where the original skips it.
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use t3_contracts::{
    PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestChecksState, PullRequestComment,
    PullRequestCommentKind, PullRequestCommit, PullRequestDiffSide, PullRequestFileViewed,
    PullRequestFileViewedState, PullRequestLabel, PullRequestMergeCapabilities, PullRequestMergeMethod,
    PullRequestMergeability, PullRequestOmittedFileStat, PullRequestReaction, PullRequestReactionContent,
    PullRequestReviewDecision, PullRequestReviewThread, PullRequestState, PullRequestThreadComment,
    TrimmedNonEmptyString, trim_wire_string,
};

/// An answer that does not have the shape the read needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeFailure(pub String);

pub type Decoded<T> = Result<T, DecodeFailure>;

fn fail<T>(what: &str) -> Decoded<T> {
    Err(DecodeFailure(what.to_string()))
}

fn parse(raw: &str) -> Decoded<Value> {
    serde_json::from_str(raw).map_err(|error| DecodeFailure(error.to_string()))
}

/// A present, non-null member.
fn member<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|member| !member.is_null())
}

fn required_object<'a>(value: &'a Value, key: &str) -> Decoded<&'a Value> {
    match member(value, key) {
        Some(found) if found.is_object() => Ok(found),
        _ => fail(&format!("{key} must be an object")),
    }
}

fn required_str<'a>(value: &'a Value, key: &str) -> Decoded<&'a str> {
    member(value, key).and_then(Value::as_str).map_or_else(|| fail(&format!("{key} must be a string")), Ok)
}

fn required_int(value: &Value, key: &str) -> Decoded<i64> {
    member(value, key).and_then(as_int).map_or_else(|| fail(&format!("{key} must be an integer")), Ok)
}

fn required_bool(value: &Value, key: &str) -> Decoded<bool> {
    member(value, key).and_then(Value::as_bool).map_or_else(|| fail(&format!("{key} must be a boolean")), Ok)
}

fn as_int(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().filter(|float| float.fract() == 0.0).map(|float| float as i64))
}

pub(crate) fn trimmed(value: Option<&Value>) -> Option<String> {
    let text = trim_wire_string(value?.as_str()?);
    (!text.is_empty()).then(|| text.to_string())
}

fn nodes_of(value: Option<&Value>) -> Vec<&Value> {
    value
        .and_then(|connection| connection.get("nodes"))
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().filter(|node| !node.is_null()).collect())
        .unwrap_or_default()
}

/// Null once a connection has nothing further; the flag decides, not the cursor's presence.
fn next_cursor_of(page_info: Option<&Value>) -> Option<String> {
    let page_info = page_info?;
    if page_info.get("hasNextPage").and_then(Value::as_bool) == Some(true) {
        trimmed(page_info.get("endCursor"))
    } else {
        None
    }
}

pub(crate) fn to_actor(raw: Option<&Value>) -> Option<PullRequestActor> {
    let raw = raw?;
    let login = trimmed(raw.get("login"))?;
    let is_bot = raw.get("__typename").and_then(Value::as_str) == Some("Bot")
        || raw.get("is_bot").and_then(Value::as_bool) == Some(true);
    Some(PullRequestActor {
        is_bot: is_bot.then_some(Some(true)),
        login: TrimmedNonEmptyString::new(&login).ok()?,
        name: trimmed(raw.get("name")),
        avatar_url: trimmed(raw.get("avatarUrl")),
    })
}

pub(crate) fn to_state(raw: &Value) -> PullRequestState {
    if trimmed(raw.get("mergedAt")).is_some() {
        return PullRequestState::Merged;
    }
    match raw.get("state").and_then(Value::as_str).map(|state| trim_wire_string(state).to_uppercase()).as_deref() {
        Some("MERGED") => PullRequestState::Merged,
        Some("CLOSED") => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

pub(crate) fn to_mergeability(value: Option<&Value>) -> PullRequestMergeability {
    match value.and_then(Value::as_str).map(|text| trim_wire_string(text).to_uppercase()).as_deref() {
        Some("MERGEABLE") => PullRequestMergeability::Mergeable,
        Some("CONFLICTING") => PullRequestMergeability::Conflicting,
        _ => PullRequestMergeability::Unknown,
    }
}

fn to_merge_method(value: Option<&Value>) -> Option<PullRequestMergeMethod> {
    match value.and_then(Value::as_str).map(|text| trim_wire_string(text).to_uppercase()).as_deref() {
        Some("MERGE") => Some(PullRequestMergeMethod::Merge),
        Some("SQUASH") => Some(PullRequestMergeMethod::Squash),
        Some("REBASE") => Some(PullRequestMergeMethod::Rebase),
        _ => None,
    }
}

fn to_review_decision(value: Option<&Value>) -> Option<PullRequestReviewDecision> {
    match value.and_then(Value::as_str).map(|text| trim_wire_string(text).to_uppercase()).as_deref() {
        Some("APPROVED") => Some(PullRequestReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(PullRequestReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(PullRequestReviewDecision::ReviewRequired),
        _ => None,
    }
}

/// GitHub's own decision counts only reviews that satisfy the branch rules; when it reports no
/// verdict the latest review per reviewer decides, changes requested outranking approval.
/// `latest_reviews` is either an array (`gh` style) or a connection with `nodes`.
pub(crate) fn to_review_decision_with_reviews(
    value: Option<&Value>,
    latest_reviews: Option<&Value>,
) -> Option<PullRequestReviewDecision> {
    let summarized = to_review_decision(value);
    if matches!(
        summarized,
        Some(PullRequestReviewDecision::Approved | PullRequestReviewDecision::ChangesRequested)
    ) {
        return summarized;
    }
    let reviews: Vec<&Value> = match latest_reviews {
        Some(Value::Array(reviews)) => reviews.iter().collect(),
        Some(connection @ Value::Object(_)) => nodes_of(Some(connection)),
        _ => vec![],
    };
    let states: Vec<String> = reviews
        .iter()
        .map(|review| {
            review.get("state").and_then(Value::as_str).map(|state| trim_wire_string(state).to_uppercase()).unwrap_or_default()
        })
        .collect();
    if states.iter().any(|state| state == "CHANGES_REQUESTED") {
        return Some(PullRequestReviewDecision::ChangesRequested);
    }
    if states.iter().any(|state| state == "APPROVED") {
        return Some(PullRequestReviewDecision::Approved);
    }
    summarized
}

pub(crate) fn to_labels(raw: Option<&Value>) -> Vec<PullRequestLabel> {
    let items: Vec<&Value> = match raw {
        Some(Value::Array(items)) => items.iter().filter(|item| !item.is_null()).collect(),
        Some(connection @ Value::Object(_)) => nodes_of(Some(connection)),
        _ => vec![],
    };
    items
        .into_iter()
        .filter_map(|label| {
            let name = trimmed(label.get("name"))?;
            Some(PullRequestLabel { name, color: trimmed(label.get("color")) })
        })
        .collect()
}

pub(crate) fn to_review_request_logins(requests: &[&Value]) -> Vec<String> {
    requests.iter().filter_map(|request| trimmed(request.get("login"))).collect()
}

pub(crate) fn has_team_review_request(requests: &[&Value]) -> bool {
    requests.iter().any(|request| {
        trimmed(request.get("login")).is_none()
            && (trimmed(request.get("slug")).is_some() || trimmed(request.get("name")).is_some())
    })
}

// ---------------------------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------------------------

fn upper(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(|text| trim_wire_string(text).to_uppercase())
}

pub(crate) fn to_check_status(raw: &Value) -> PullRequestCheckStatus {
    if let Some(status) = upper(raw.get("status")) {
        if status != "COMPLETED" && !status.is_empty() {
            return PullRequestCheckStatus::Pending;
        }
    }
    let verdict = upper(member(raw, "conclusion")).or_else(|| upper(raw.get("state")));
    match verdict.as_deref() {
        Some("SUCCESS") => PullRequestCheckStatus::Success,
        Some("ACTION_REQUIRED") => PullRequestCheckStatus::ActionRequired,
        Some("FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE") => PullRequestCheckStatus::Failure,
        Some("CANCELLED") => PullRequestCheckStatus::Cancelled,
        Some("SKIPPED") => PullRequestCheckStatus::Skipped,
        Some("PENDING" | "EXPECTED") => PullRequestCheckStatus::Pending,
        _ => PullRequestCheckStatus::Neutral,
    }
}

fn real_timestamp(value: Option<&Value>) -> Option<String> {
    trimmed(value).filter(|at| at != "0001-01-01T00:00:00Z")
}

fn is_nameless(raw: &Value) -> bool {
    trimmed(raw.get("name")).is_none() && trimmed(raw.get("context")).is_none()
}

pub(crate) struct CheckEntry {
    pub(crate) check: PullRequestCheck,
    pub(crate) workflow_name: Option<String>,
    pub(crate) at: Option<String>,
}

fn to_check_entries(raw: &[Value]) -> Vec<CheckEntry> {
    raw.iter()
        .filter_map(|check| {
            let name = trimmed(check.get("name")).or_else(|| trimmed(check.get("context")))?;
            Some(CheckEntry {
                check: PullRequestCheck {
                    name,
                    status: to_check_status(check),
                    description: trimmed(check.get("description")),
                    url: trimmed(check.get("detailsUrl")).or_else(|| trimmed(check.get("targetUrl"))),
                    required: check.get("isRequired").and_then(Value::as_bool),
                },
                workflow_name: trimmed(check.get("workflowName")),
                at: real_timestamp(check.get("completedAt")).or_else(|| real_timestamp(check.get("startedAt"))),
            })
        })
        .collect()
}

/// One row per check rather than one per run of it (`dedupeChecks`).
pub(crate) fn dedupe_checks(entries: Vec<CheckEntry>) -> Vec<PullRequestCheck> {
    let mut order: Vec<String> = vec![];
    let mut newest: HashMap<String, CheckEntry> = HashMap::new();
    for entry in entries {
        let key = format!("{} {}", entry.workflow_name.clone().unwrap_or_default(), entry.check.name);
        let replaces = match newest.get(&key) {
            None => {
                order.push(key.clone());
                true
            }
            Some(kept) => match &entry.at {
                None => kept.at.is_none(),
                Some(candidate) => kept.at.as_ref().is_none_or(|kept| candidate >= kept),
            },
        };
        if replaces {
            newest.insert(key, entry);
        }
    }
    let survivors: Vec<CheckEntry> = order.into_iter().filter_map(|key| newest.remove(&key)).collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for entry in &survivors {
        *counts.entry(entry.check.name.clone()).or_default() += 1;
    }
    survivors
        .into_iter()
        .map(|entry| {
            let workflow = entry.workflow_name.unwrap_or_default();
            if !workflow.is_empty() && counts.get(&entry.check.name).copied().unwrap_or(0) > 1 {
                PullRequestCheck { name: format!("{workflow} / {}", entry.check.name), ..entry.check }
            } else {
                entry.check
            }
        })
        .collect()
}

pub(crate) fn to_checks(raw: &[Value]) -> Vec<PullRequestCheck> {
    dedupe_checks(to_check_entries(raw))
}

/// The one word a listing row has space for. A failure outranks anything still running; null
/// for a head commit with no checks at all.
pub(crate) fn rollup_checks_state(raw: &[Value]) -> Option<PullRequestChecksState> {
    let mut statuses: Vec<PullRequestCheckStatus> = to_checks(raw).iter().map(|check| check.status).collect();
    statuses.extend(raw.iter().filter(|check| is_nameless(check)).map(to_check_status));
    if statuses.is_empty() {
        return None;
    }
    if statuses.iter().any(|status| matches!(status, PullRequestCheckStatus::Failure | PullRequestCheckStatus::Cancelled)) {
        return Some(PullRequestChecksState::Failing);
    }
    if statuses
        .iter()
        .any(|status| matches!(status, PullRequestCheckStatus::Pending | PullRequestCheckStatus::ActionRequired))
    {
        return Some(PullRequestChecksState::Pending);
    }
    statuses.contains(&PullRequestCheckStatus::Success).then_some(PullRequestChecksState::Passing)
}

/// A check context with its workflow's name folded in, as the rollup contexts carry it.
fn check_contexts(nodes: &[&Value]) -> Vec<Value> {
    nodes
        .iter()
        .map(|node| {
            let mut context = (*node).clone();
            let workflow = node
                .get("checkSuite")
                .and_then(|suite| suite.get("workflowRun"))
                .and_then(|run| run.get("workflow"))
                .and_then(|workflow| workflow.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            context["workflowName"] = workflow;
            context
        })
        .collect()
}

/// The checks and their one-word verdict from every context of the head commit.
pub fn checks_from_contexts(contexts: &[Value]) -> (Vec<PullRequestCheck>, Option<PullRequestChecksState>) {
    (to_checks(contexts), rollup_checks_state(contexts))
}

// ---------------------------------------------------------------------------------------------
// Reactions
// ---------------------------------------------------------------------------------------------

pub fn github_reaction_content(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "THUMBS_UP",
        PullRequestReactionContent::ThumbsDown => "THUMBS_DOWN",
        PullRequestReactionContent::Laugh => "LAUGH",
        PullRequestReactionContent::Hooray => "HOORAY",
        PullRequestReactionContent::Confused => "CONFUSED",
        PullRequestReactionContent::Heart => "HEART",
        PullRequestReactionContent::Rocket => "ROCKET",
        PullRequestReactionContent::Eyes => "EYES",
    }
}

fn reaction_content(name: &str) -> Option<PullRequestReactionContent> {
    Some(match name {
        "THUMBS_UP" => PullRequestReactionContent::ThumbsUp,
        "THUMBS_DOWN" => PullRequestReactionContent::ThumbsDown,
        "LAUGH" => PullRequestReactionContent::Laugh,
        "HOORAY" => PullRequestReactionContent::Hooray,
        "CONFUSED" => PullRequestReactionContent::Confused,
        "HEART" => PullRequestReactionContent::Heart,
        "ROCKET" => PullRequestReactionContent::Rocket,
        "EYES" => PullRequestReactionContent::Eyes,
        _ => return None,
    })
}

/// The groups as the contract carries them: a group with nobody behind it is dropped, and the
/// viewer's own login is left out of `actors` (the page says "You") while `count` keeps it.
fn to_reactions(groups: Option<&Value>, viewer: Option<&str>) -> Vec<PullRequestReaction> {
    let viewer = viewer.map(str::to_lowercase);
    let mut reactions = vec![];
    for group in groups.and_then(Value::as_array).into_iter().flatten() {
        let Some(content) = trimmed(group.get("content")).and_then(|name| reaction_content(&name.to_uppercase())) else {
            continue;
        };
        let reactors = group.get("reactors");
        let logins: Vec<String> = reactors
            .and_then(|reactors| reactors.get("nodes"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|node| trimmed(node.get("login")))
            .collect();
        let total = reactors.and_then(|reactors| reactors.get("totalCount")).and_then(as_int);
        let count = total.unwrap_or(logins.len() as i64).max(logins.len() as i64);
        if count <= 0 {
            continue;
        }
        let actors = match &viewer {
            None => logins,
            Some(viewer) => logins.into_iter().filter(|login| login.to_lowercase() != *viewer).collect(),
        };
        reactions.push(PullRequestReaction {
            content,
            count: count as u64,
            actors,
            viewer_has_reacted: group.get("viewerHasReacted").and_then(Value::as_bool) == Some(true),
        });
    }
    reactions
}

// ---------------------------------------------------------------------------------------------
// Core detail
// ---------------------------------------------------------------------------------------------

/// What a list row, a search row and the core read share.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubListItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<t3_contracts::PullRequestStackMembership>,
    pub author_id: Option<String>,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub review_decision: Option<PullRequestReviewDecision>,
    pub additions: u64,
    pub deletions: u64,
    pub created_at: String,
    pub updated_at: String,
    pub review_request_logins: Vec<String>,
    pub has_team_review_request: bool,
    pub labels: Vec<PullRequestLabel>,
    pub checks_state: Option<PullRequestChecksState>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubDetail {
    #[serde(flatten)]
    pub item: GitHubListItem,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_cross_repository: Option<bool>,
    pub head_repository_owner: Option<String>,
    pub head_sha: Option<String>,
    pub body: String,
    pub changed_files: u64,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub checks: Vec<PullRequestCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_method: Option<PullRequestMergeMethod>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubViewerAccess {
    pub can_write: bool,
    pub can_triage: bool,
    pub can_update: bool,
    pub did_author: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_update_branch: Option<bool>,
    pub merge_capabilities: PullRequestMergeCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubBaseComparison {
    pub behind_by: Option<u64>,
    pub viewer_can_update: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubCore {
    #[serde(flatten)]
    pub detail: GitHubDetail,
    pub viewer_access: GitHubViewerAccess,
    pub comparison: Option<GitHubBaseComparison>,
    pub checks_truncated: bool,
}

pub(crate) fn can_write(permission: Option<&Value>) -> bool {
    matches!(upper(permission).as_deref(), Some("ADMIN" | "MAINTAIN" | "WRITE"))
}

pub(crate) fn can_triage(permission: Option<&Value>) -> bool {
    upper(permission).as_deref() == Some("TRIAGE") || can_write(permission)
}

/// Updating is a permission, so a missing answer grants it; authorship is a fact, so a missing
/// answer is "not the author".
fn viewer_fields(raw: Option<&Value>) -> (bool, bool) {
    let can_update = raw.and_then(|raw| raw.get("viewerCanUpdate")).and_then(Value::as_bool) != Some(false);
    let did_author = raw.and_then(|raw| raw.get("viewerDidAuthor")).and_then(Value::as_bool) == Some(true);
    (can_update, did_author)
}

fn list_item(raw: &Value, review_requests: &[&Value], labels: Option<&Value>, rollup: &[Value]) -> Decoded<GitHubListItem> {
    Ok(GitHubListItem {
        stack: None,
        author_id: raw.get("author").and_then(|author| trimmed(author.get("id"))),
        number: required_int(raw, "number")?.max(0) as u64,
        title: required_str(raw, "title")?.to_string(),
        url: required_str(raw, "url")?.to_string(),
        author: to_actor(raw.get("author")),
        head_branch: required_str(raw, "headRefName")?.to_string(),
        base_branch: required_str(raw, "baseRefName")?.to_string(),
        state: to_state(raw),
        is_draft: raw.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        mergeability: to_mergeability(raw.get("mergeable")),
        review_decision: to_review_decision_with_reviews(raw.get("reviewDecision"), raw.get("latestReviews")),
        additions: raw.get("additions").and_then(as_int).unwrap_or(0).max(0) as u64,
        deletions: raw.get("deletions").and_then(as_int).unwrap_or(0).max(0) as u64,
        created_at: required_str(raw, "createdAt")?.to_string(),
        updated_at: required_str(raw, "updatedAt")?.to_string(),
        review_request_logins: to_review_request_logins(review_requests),
        has_team_review_request: has_team_review_request(review_requests),
        labels: to_labels(labels),
        checks_state: rollup_checks_state(rollup),
    })
}

/// The core read: the pull request, the viewer's standing, the base comparison and the first
/// hundred check contexts (`decodePullRequestCoreJson`).
pub fn decode_core(raw: &str) -> Decoded<GitHubCore> {
    let value = parse(raw)?;
    let repository = required_object(required_object(&value, "data")?, "repository")?;
    let merge_commit = required_bool(repository, "mergeCommitAllowed")?;
    let squash = required_bool(repository, "squashMergeAllowed")?;
    let rebase = required_bool(repository, "rebaseMergeAllowed")?;
    let pr = required_object(repository, "pullRequest")?;
    let can_update_branch = required_bool(pr, "viewerCanUpdateBranch")?;
    let base_ref = pr.get("baseRef").ok_or_else(|| DecodeFailure("missing baseRef".into()))?;
    let behind_by = match base_ref {
        Value::Null => None,
        base_ref => match base_ref.get("compare") {
            None => return fail("missing compare"),
            Some(Value::Null) => None,
            Some(compare) => Some(required_int(compare, "behindBy")?),
        },
    };
    let requests_connection = required_object(pr, "reviewRequests")?;
    let reviewers: Vec<&Value> = nodes_of(Some(requests_connection))
        .into_iter()
        .filter_map(|node| member(node, "requestedReviewer"))
        .collect();
    let labels_connection = required_object(pr, "labels")?;
    let commits = required_object(pr, "commits")?;
    let contexts_connection = nodes_of(Some(commits))
        .first()
        .and_then(|node| node.get("commit"))
        .and_then(|commit| member(commit, "statusCheckRollup"))
        .and_then(|rollup| member(rollup, "contexts"));
    let contexts = check_contexts(&nodes_of(contexts_connection));
    let item = list_item(pr, &reviewers, Some(labels_connection), &contexts)?;
    let (can_update, did_author) = viewer_fields(Some(pr));
    let open = pr.get("state").and_then(Value::as_str) == Some("OPEN");
    let auto_merge = pr.get("autoMergeRequest");
    Ok(GitHubCore {
        detail: GitHubDetail {
            item,
            is_cross_repository: pr.get("isCrossRepository").and_then(Value::as_bool),
            head_repository_owner: pr.get("headRepositoryOwner").and_then(|owner| trimmed(owner.get("login"))),
            head_sha: trimmed(pr.get("headRefOid")),
            body: pr.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
            changed_files: pr.get("changedFiles").and_then(as_int).unwrap_or(0).max(0) as u64,
            merged_at: trimmed(pr.get("mergedAt")),
            closed_at: trimmed(pr.get("closedAt")),
            checks: to_checks(&contexts),
            auto_merge_enabled: auto_merge.map(|request| !request.is_null()),
            auto_merge_method: auto_merge.and_then(|request| to_merge_method(request.get("mergeMethod"))),
        },
        viewer_access: GitHubViewerAccess {
            can_write: can_write(repository.get("viewerPermission")),
            can_triage: can_triage(repository.get("viewerPermission")),
            can_update,
            did_author,
            can_update_branch: None,
            merge_capabilities: PullRequestMergeCapabilities { merge: merge_commit, squash, rebase },
        },
        comparison: match behind_by {
            Some(behind) if open => Some(GitHubBaseComparison {
                behind_by: Some(behind.max(0) as u64),
                viewer_can_update: can_update_branch,
            }),
            _ => None,
        },
        checks_truncated: contexts_connection
            .and_then(|connection| connection.get("pageInfo"))
            .and_then(|info| info.get("hasNextPage"))
            .and_then(Value::as_bool)
            == Some(true),
    })
}

/// One page of the check contexts of the head commit (`decodePullRequestCheckContextsJson`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckContextsPage {
    pub head_sha: String,
    pub contexts: Vec<Value>,
    pub next_cursor: Option<String>,
}

pub fn decode_check_contexts(raw: &str) -> Decoded<CheckContextsPage> {
    let value = parse(raw)?;
    let pr = required_object(required_object(required_object(&value, "data")?, "repository")?, "pullRequest")?;
    let head_sha = required_str(pr, "headRefOid")?.to_string();
    let commits = required_object(pr, "commits")?;
    let contexts = nodes_of(Some(commits))
        .first()
        .and_then(|node| node.get("commit"))
        .and_then(|commit| member(commit, "statusCheckRollup"))
        .and_then(|rollup| member(rollup, "contexts"));
    Ok(CheckContextsPage {
        head_sha,
        contexts: check_contexts(&nodes_of(contexts)),
        next_cursor: next_cursor_of(contexts.and_then(|connection| connection.get("pageInfo"))),
    })
}

/// The card shown on hover (`decodePullRequestPreviewJson`), without the project and repository.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPreview {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub created_at: String,
}

pub fn decode_preview(raw: &str) -> Decoded<GitHubPreview> {
    let value = parse(raw)?;
    let pr = required_object(required_object(required_object(&value, "data")?, "repository")?, "pullRequest")?;
    if pr.get("author").is_none() {
        return fail("missing author");
    }
    Ok(GitHubPreview {
        number: required_int(pr, "number")?.max(0) as u64,
        title: required_str(pr, "title")?.to_string(),
        url: required_str(pr, "url")?.to_string(),
        author: to_actor(pr.get("author")),
        state: to_state(pr),
        is_draft: required_bool(pr, "isDraft")?,
        created_at: required_str(pr, "createdAt")?.to_string(),
    })
}

/// `viewerPermission` and the merge settings, plus what the viewer may do with the pull request.
pub fn decode_viewer_permissions(raw: &str) -> Decoded<GitHubViewerAccess> {
    let value = parse(raw)?;
    let repository = required_object(required_object(&value, "data")?, "repository")?;
    let merge = required_bool(repository, "mergeCommitAllowed")?;
    let squash = required_bool(repository, "squashMergeAllowed")?;
    let rebase = required_bool(repository, "rebaseMergeAllowed")?;
    if repository.get("pullRequest").is_none() {
        return fail("missing pullRequest");
    }
    let (can_update, did_author) = viewer_fields(repository.get("pullRequest").filter(|pr| !pr.is_null()));
    Ok(GitHubViewerAccess {
        can_write: can_write(repository.get("viewerPermission")),
        can_triage: can_triage(repository.get("viewerPermission")),
        can_update,
        did_author,
        can_update_branch: None,
        merge_capabilities: PullRequestMergeCapabilities { merge, squash, rebase },
    })
}

// ---------------------------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------------------------

fn is_review_verdict(state: Option<&str>) -> bool {
    matches!(state.map(str::to_uppercase).as_deref(), Some("APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"))
}

fn edited_at(node: &Value) -> Option<String> {
    node.get("lastEditedAt").and_then(Value::as_str).map(str::to_string)
}

/// Issue comments and reviews as one chronological list (`toComments`).
fn to_comments(comments: &[&Value], reviews: &[&Value]) -> Decoded<Vec<PullRequestComment>> {
    let mut remarks = vec![];
    for comment in comments {
        remarks.push(PullRequestComment {
            id: required_str(comment, "id")?.to_string(),
            kind: PullRequestCommentKind::IssueComment,
            author: to_actor(comment.get("author")),
            body: comment.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
            created_at: required_str(comment, "createdAt")?.to_string(),
            edited_at: Some(edited_at(comment)),
            url: trimmed(comment.get("url")),
            path: None,
            review_state: None,
            reactions: None,
        });
    }
    for review in reviews {
        let id = required_str(review, "id")?.to_string();
        let submitted_at = trimmed(review.get("submittedAt"));
        let review_state = trimmed(review.get("state"));
        let body = review.get("body").and_then(Value::as_str).unwrap_or("");
        let Some(submitted_at) = submitted_at else { continue };
        if trim_wire_string(body).is_empty() && !is_review_verdict(review_state.as_deref()) {
            continue;
        }
        remarks.push(PullRequestComment {
            id,
            kind: PullRequestCommentKind::Review,
            author: to_actor(review.get("author")),
            body: body.to_string(),
            created_at: submitted_at,
            edited_at: Some(edited_at(review)),
            url: trimmed(review.get("url")),
            path: None,
            review_state,
            reactions: None,
        });
    }
    remarks.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    Ok(remarks)
}

fn commit_actor(raw: &Value, login: Option<String>) -> Option<PullRequestActor> {
    let login = login.or_else(|| trimmed(raw.get("name"))).or_else(|| trimmed(raw.get("email")))?;
    Some(PullRequestActor {
        is_bot: None,
        login: TrimmedNonEmptyString::new(&login).ok()?,
        name: trimmed(raw.get("name")),
        avatar_url: None,
    })
}

/// One page of the activity read (`decodePullRequestActivityJson`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPage {
    /// Present only on the first page, which is the one that asks for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<Option<PullRequestActor>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commits: Option<Vec<PullRequestCommit>>,
    pub remarks: Vec<PullRequestComment>,
    pub next_comments_cursor: Option<String>,
    pub next_reviews_cursor: Option<String>,
}

pub fn decode_activity(raw: &str) -> Decoded<ActivityPage> {
    let value = parse(raw)?;
    let pr = required_object(required_object(required_object(&value, "data")?, "repository")?, "pullRequest")?;
    let commits = pr.get("commits").map(|commits| {
        nodes_of(Some(commits))
            .into_iter()
            .filter_map(|node| node.get("commit"))
            .map(|commit| {
                Ok(PullRequestCommit {
                    oid: required_str(commit, "oid")?.to_string(),
                    message_headline: commit.get("messageHeadline").and_then(Value::as_str).unwrap_or("").to_string(),
                    committed_date: required_str(commit, "committedDate")?.to_string(),
                    additions: None,
                    deletions: None,
                    authors: Some(
                        nodes_of(commit.get("authors"))
                            .into_iter()
                            .filter_map(|author| commit_actor(author, author.get("user").and_then(|user| trimmed(user.get("login")))))
                            .collect(),
                    ),
                })
            })
            .collect::<Decoded<Vec<_>>>()
    });
    Ok(ActivityPage {
        author: pr.get("author").map(|author| to_actor(Some(author))),
        commits: commits.transpose()?,
        remarks: to_comments(&nodes_of(pr.get("comments")), &nodes_of(pr.get("reviews")))?,
        next_comments_cursor: next_cursor_of(pr.get("comments").and_then(|connection| connection.get("pageInfo"))),
        next_reviews_cursor: next_cursor_of(pr.get("reviews").and_then(|connection| connection.get("pageInfo"))),
    })
}

// ---------------------------------------------------------------------------------------------
// Review threads
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThreadEntry {
    pub thread: PullRequestReviewThread,
    pub comment_count: u64,
    pub next_comment_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitStat {
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadViewer {
    pub can_update: bool,
    pub did_author: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThreadPage {
    pub threads: Vec<ReviewThreadEntry>,
    pub next_cursor: Option<String>,
    pub reactions: Vec<PullRequestReaction>,
    pub reactions_by_id: BTreeMap<String, Vec<PullRequestReaction>>,
    pub edited_at_by_id: BTreeMap<String, String>,
    pub reviewers: Vec<PullRequestActor>,
    pub avatars_by_login: BTreeMap<String, String>,
    pub bot_logins: BTreeSet<String>,
    pub commit_stats: BTreeMap<String, CommitStat>,
    pub commits: Vec<PullRequestCommit>,
    pub viewer: ThreadViewer,
    pub dismissals_by_review_id: BTreeMap<String, String>,
    pub next_dismissal_cursor: Option<String>,
}

fn thread_comment(comment: &Value, viewer: Option<&str>) -> Decoded<PullRequestThreadComment> {
    Ok(PullRequestThreadComment {
        id: required_str(comment, "id")?.to_string(),
        author: to_actor(comment.get("author")),
        body: comment.get("body").and_then(Value::as_str).unwrap_or("").to_string(),
        created_at: required_str(comment, "createdAt")?.to_string(),
        edited_at: Some(edited_at(comment)),
        url: trimmed(comment.get("url")),
        reactions: Some(to_reactions(comment.get("reactionGroups"), viewer)),
    })
}

fn dismissal_entries(nodes: &[&Value]) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    for node in nodes {
        let review_id = node.get("review").and_then(|review| trimmed(review.get("id")));
        let message = trimmed(node.get("dismissalMessage"));
        if let (Some(review_id), Some(message)) = (review_id, message) {
            entries.insert(review_id, message);
        }
    }
    entries
}

/// One page of `REVIEW_THREADS_GRAPHQL_QUERY` (`decodeReviewThreadsJson`).
pub fn decode_review_threads(raw: &str) -> Decoded<ReviewThreadPage> {
    let value = parse(raw)?;
    let data = required_object(&value, "data")?;
    let viewer = data.get("viewer").and_then(|viewer| trimmed(viewer.get("login")));
    let pr = required_object(required_object(data, "repository")?, "pullRequest")?;
    let threads = required_object(pr, "reviewThreads")?;
    let thread_nodes = threads
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| DecodeFailure("reviewThreads.nodes must be an array".into()))?;
    let mut entries = vec![];
    for thread in thread_nodes {
        let comments = thread.get("comments").filter(|comments| comments.is_object()).ok_or_else(|| DecodeFailure("thread comments must be an object".into()))?;
        let comment_nodes = comments
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| DecodeFailure("thread comments.nodes must be an array".into()))?;
        let (path, id) = (trimmed(thread.get("path")), trimmed(thread.get("id")));
        let (Some(path), Some(id)) = (path, id) else { continue };
        if comment_nodes.is_empty() {
            continue;
        }
        let line = thread.get("line").and_then(as_int).filter(|line| *line > 0).map(|line| line as u64);
        entries.push(ReviewThreadEntry {
            thread: PullRequestReviewThread {
                id,
                path,
                line,
                side: if upper(thread.get("diffSide")).as_deref() == Some("LEFT") {
                    PullRequestDiffSide::Left
                } else {
                    PullRequestDiffSide::Right
                },
                is_resolved: thread.get("isResolved").and_then(Value::as_bool) == Some(true),
                is_outdated: thread.get("isOutdated").and_then(Value::as_bool) == Some(true),
                comments: comment_nodes
                    .iter()
                    .map(|comment| thread_comment(comment, viewer.as_deref()))
                    .collect::<Decoded<Vec<_>>>()?,
                comment_count: None,
                next_comments_cursor: None,
            },
            comment_count: comments.get("totalCount").and_then(as_int).map(|count| count.max(0) as u64).unwrap_or(comment_nodes.len() as u64),
            next_comment_cursor: next_cursor_of(comments.get("pageInfo")),
        });
    }

    let mut avatars_by_login = BTreeMap::new();
    let mut bot_logins = BTreeSet::new();
    let mut people: Vec<Option<&Value>> = vec![pr.get("author")];
    people.extend(nodes_of(pr.get("comments")).into_iter().map(|node| node.get("author")));
    people.extend(nodes_of(pr.get("reviews")).into_iter().map(|node| node.get("author")));
    people.extend(nodes_of(pr.get("reviewRequests")).into_iter().map(|node| node.get("requestedReviewer")));
    people.extend(nodes_of(pr.get("latestReviews")).into_iter().map(|node| node.get("author")));
    for thread in thread_nodes {
        people.extend(nodes_of(thread.get("comments")).into_iter().map(|node| node.get("author")));
    }
    for person in people.into_iter().flatten().filter(|person| !person.is_null()) {
        let (login, avatar) = (trimmed(person.get("login")), trimmed(person.get("avatarUrl")));
        if let (Some(login), Some(avatar)) = (&login, avatar) {
            avatars_by_login.insert(login.clone(), avatar);
        }
        if let Some(login) = login {
            if to_actor(Some(person)).is_some_and(|actor| actor.is_bot == Some(Some(true))) {
                bot_logins.insert(login);
            }
        }
    }

    let mut reviewers: Vec<PullRequestActor> = vec![];
    let candidates = nodes_of(pr.get("reviewRequests"))
        .into_iter()
        .map(|node| node.get("requestedReviewer"))
        .chain(nodes_of(pr.get("latestReviews")).into_iter().map(|node| node.get("author")));
    for raw_actor in candidates.flatten().filter(|actor| !actor.is_null()) {
        if let Some(actor) = to_actor(Some(raw_actor)) {
            if !reviewers.iter().any(|known| known.login == actor.login) {
                reviewers.push(actor);
            }
        }
    }

    let mut commit_stats = BTreeMap::new();
    let mut commits = vec![];
    for node in nodes_of(pr.get("commits")) {
        let Some(commit) = node.get("commit") else { continue };
        let Some(oid) = trimmed(commit.get("oid")) else { continue };
        let parents = commit.get("parents").and_then(|parents| parents.get("totalCount")).and_then(as_int).unwrap_or(1);
        if parents <= 1 {
            if let (Some(additions), Some(deletions)) = (commit.get("additions").and_then(as_int), commit.get("deletions").and_then(as_int)) {
                commit_stats.insert(oid.clone(), CommitStat { additions: additions.max(0) as u64, deletions: deletions.max(0) as u64 });
            }
        }
        let Some(committed_date) = trimmed(commit.get("committedDate")) else { continue };
        commits.push(PullRequestCommit {
            oid,
            message_headline: commit.get("messageHeadline").and_then(Value::as_str).unwrap_or("").to_string(),
            committed_date,
            additions: None,
            deletions: None,
            authors: Some(
                nodes_of(commit.get("authors"))
                    .into_iter()
                    .filter_map(|author| {
                        let login = author.get("user").and_then(|user| trimmed(user.get("login"))).or_else(|| trimmed(author.get("name")))?;
                        Some(PullRequestActor {
                            is_bot: None,
                            login: TrimmedNonEmptyString::new(&login).ok()?,
                            name: trimmed(author.get("name")),
                            avatar_url: trimmed(author.get("avatarUrl")),
                        })
                    })
                    .collect(),
            ),
        });
    }

    let mut reactions_by_id = BTreeMap::new();
    let mut edited_at_by_id = BTreeMap::new();
    for node in nodes_of(pr.get("comments")).into_iter().chain(nodes_of(pr.get("reviews"))) {
        let Some(id) = trimmed(node.get("id")) else { continue };
        if let Some(edited) = trimmed(node.get("lastEditedAt")) {
            edited_at_by_id.insert(id.clone(), edited);
        }
        let reactions = to_reactions(node.get("reactionGroups"), viewer.as_deref());
        if !reactions.is_empty() {
            reactions_by_id.insert(id, reactions);
        }
    }

    let dismissals = pr.get("reviewDismissals");
    let (can_update, did_author) = viewer_fields(Some(pr));
    Ok(ReviewThreadPage {
        threads: entries,
        next_cursor: next_cursor_of(threads.get("pageInfo")),
        reactions: to_reactions(pr.get("reactionGroups"), viewer.as_deref()),
        reactions_by_id,
        edited_at_by_id,
        reviewers,
        avatars_by_login,
        bot_logins,
        commit_stats,
        commits,
        viewer: ThreadViewer { can_update, did_author },
        dismissals_by_review_id: dismissal_entries(&nodes_of(dismissals)),
        next_dismissal_cursor: next_cursor_of(dismissals.and_then(|connection| connection.get("pageInfo"))),
    })
}

/// A review thread's comments become flat conversation entries (`reviewThreadConversation`).
pub fn review_thread_conversation(threads: &[PullRequestReviewThread]) -> Vec<PullRequestComment> {
    threads
        .iter()
        .flat_map(|thread| {
            thread.comments.iter().map(|comment| PullRequestComment {
                id: comment.id.clone(),
                kind: PullRequestCommentKind::ReviewComment,
                author: comment.author.clone(),
                body: comment.body.clone(),
                created_at: comment.created_at.clone(),
                edited_at: Some(comment.edited_at.clone().flatten()),
                url: comment.url.clone(),
                path: Some(thread.path.clone()),
                review_state: None,
                reactions: Some(comment.reactions.clone().unwrap_or_default()),
            })
        })
        .collect()
}

/// The dismissal reasons of one timeline page (`decodeReviewDismissalsJson`).
pub fn decode_review_dismissals(raw: &str) -> Decoded<(BTreeMap<String, String>, Option<String>)> {
    let value = parse(raw)?;
    let pr = required_object(required_object(required_object(&value, "data")?, "repository")?, "pullRequest")?;
    let items = required_object(pr, "timelineItems")?;
    if !items.get("nodes").is_some_and(Value::is_array) {
        return fail("timelineItems.nodes must be an array");
    }
    Ok((dismissal_entries(&nodes_of(Some(items))), next_cursor_of(items.get("pageInfo"))))
}

/// One page of a thread's comments (`decodeReviewThreadCommentsJson`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCommentsPage {
    pub belongs_to_pull_request: bool,
    pub comments: Vec<PullRequestThreadComment>,
    pub next_cursor: Option<String>,
}

pub fn decode_review_thread_comments(raw: &str) -> Decoded<ThreadCommentsPage> {
    let value = parse(raw)?;
    let data = required_object(&value, "data")?;
    if data.get("repository").is_none() || data.get("node").is_none() {
        return fail("missing repository or node");
    }
    let viewer = data.get("viewer").and_then(|viewer| trimmed(viewer.get("login")));
    let repository_id = data
        .get("repository")
        .and_then(|repository| repository.get("pullRequest"))
        .and_then(|pr| pr.get("id"))
        .and_then(Value::as_str);
    let node = data.get("node").filter(|node| !node.is_null());
    let thread_pull_request_id = node.and_then(|node| node.get("pullRequest")).and_then(|pr| pr.get("id")).and_then(Value::as_str);
    let comments = node.and_then(|node| node.get("comments"));
    Ok(ThreadCommentsPage {
        belongs_to_pull_request: repository_id.is_some() && repository_id == thread_pull_request_id,
        comments: nodes_of(comments)
            .into_iter()
            .map(|comment| thread_comment(comment, viewer.as_deref()))
            .collect::<Decoded<Vec<_>>>()?,
        next_cursor: next_cursor_of(comments.and_then(|connection| connection.get("pageInfo"))),
    })
}

// ---------------------------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------------------------

/// How a file's name travels in a unified patch: quoted with C-style escapes when it holds a
/// character a header cannot carry (`quoteGitPatchPath`).
pub fn quote_git_patch_path(path: &str) -> String {
    let needs_quotes = path.chars().any(|c| matches!(c, '"' | '\\' | '\u{7}' | '\u{8}' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r') || (c as u32) < 0x20 || c as u32 == 0x7f);
    if !needs_quotes {
        return path.to_string();
    }
    let mut quoted = String::from("\"");
    for c in path.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\u{7}' => quoted.push_str("\\a"),
            '\u{8}' => quoted.push_str("\\b"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\u{b}' => quoted.push_str("\\v"),
            '\u{c}' => quoted.push_str("\\f"),
            '\r' => quoted.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => quoted.push_str(&format!("\\{:03o}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesPatch {
    pub patch: String,
    pub truncated: bool,
    pub raw_count: usize,
    pub omitted_file_stats: Vec<PullRequestOmittedFileStat>,
}

fn to_files_patch(entries: &[Value]) -> FilesPatch {
    let mut sections = String::new();
    let mut omitted = vec![];
    let mut truncated = false;
    for entry in entries {
        let Some(filename) = entry.get("filename").and_then(Value::as_str) else { continue };
        let hunks = entry.get("patch").and_then(Value::as_str).unwrap_or("");
        let status = entry.get("status").and_then(Value::as_str).map(|status| trim_wire_string(status).to_lowercase());
        if hunks.is_empty() {
            let additions = entry.get("additions").and_then(as_int).unwrap_or(0);
            let deletions = entry.get("deletions").and_then(as_int).unwrap_or(0);
            if additions + deletions > 0 {
                truncated = true;
                omitted.push(PullRequestOmittedFileStat { path: filename.to_string(), additions, deletions });
            }
        }
        let renamed = status.as_deref() == Some("renamed");
        let previous = entry.get("previous_filename").and_then(Value::as_str).filter(|previous| !previous.is_empty());
        let old_path = if renamed { previous.unwrap_or(filename) } else { filename };
        let mut header = vec![format!(
            "diff --git {} {}",
            quote_git_patch_path(&format!("a/{old_path}")),
            quote_git_patch_path(&format!("b/{filename}"))
        )];
        if status.as_deref() == Some("added") {
            header.push("new file mode 100644".into());
        }
        if status.as_deref() == Some("removed") {
            header.push("deleted file mode 100644".into());
        }
        if renamed {
            header.push(format!("rename from {}", quote_git_patch_path(old_path)));
            header.push(format!("rename to {}", quote_git_patch_path(filename)));
        }
        header.push(format!(
            "--- {}",
            if status.as_deref() == Some("added") { "/dev/null".to_string() } else { quote_git_patch_path(&format!("a/{old_path}")) }
        ));
        header.push(format!(
            "+++ {}",
            if status.as_deref() == Some("removed") { "/dev/null".to_string() } else { quote_git_patch_path(&format!("b/{filename}")) }
        ));
        let header = header.join("\n");
        if hunks.is_empty() {
            sections.push_str(&format!("{header}\n"));
        } else {
            sections.push_str(&format!("{header}\n{}", if hunks.ends_with('\n') { hunks.to_string() } else { format!("{hunks}\n") }));
        }
    }
    FilesPatch { patch: sections, truncated, raw_count: entries.len(), omitted_file_stats: omitted }
}

/// A page of `GET pulls/{n}/files`.
pub fn decode_pull_request_files(raw: &str) -> Decoded<FilesPatch> {
    match parse(raw)? {
        Value::Array(entries) => Ok(to_files_patch(&entries)),
        _ => fail("expected an array"),
    }
}

/// A commit, whose `files` are the same entries wrapped in an object.
pub fn decode_commit_files(raw: &str) -> Decoded<FilesPatch> {
    let value = parse(raw)?;
    if !value.is_object() {
        return fail("expected an object");
    }
    let files = value.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(to_files_patch(&files))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesViewedPage {
    pub files: Vec<PullRequestFileViewed>,
    pub next_cursor: Option<String>,
}

pub fn decode_files_viewed(raw: &str) -> Decoded<FilesViewedPage> {
    let value = parse(raw)?;
    let data = required_object(&value, "data")?;
    let files = data.get("repository").filter(|repository| !repository.is_null()).and_then(|repository| member(repository, "pullRequest")).and_then(|pr| pr.get("files"));
    let Some(files) = files.filter(|files| !files.is_null()) else {
        return Ok(FilesViewedPage { files: vec![], next_cursor: None });
    };
    let page_info = required_object(files, "pageInfo")?;
    let has_next = required_bool(page_info, "hasNextPage")?;
    let state_of = |raw: &str| match trim_wire_string(raw).to_uppercase().as_str() {
        "VIEWED" => PullRequestFileViewedState::Viewed,
        "DISMISSED" => PullRequestFileViewedState::Dismissed,
        _ => PullRequestFileViewedState::Unviewed,
    };
    Ok(FilesViewedPage {
        files: files
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|node| {
                let path = node.get("path").and_then(Value::as_str).filter(|path| !path.is_empty())?;
                Some(PullRequestFileViewed { path: path.to_string(), state: state_of(node.get("viewerViewedState").and_then(Value::as_str)?) })
            })
            .collect(),
        next_cursor: if has_next { page_info.get("endCursor").and_then(Value::as_str).map(str::to_string) } else { None },
    })
}

/// Aliased line-count answers by position (`decodePullRequestStatsJson`).
pub fn decode_stats(raw: &str) -> Decoded<BTreeMap<usize, (u64, u64)>> {
    let value = parse(raw)?;
    if !value.is_object() {
        return fail("expected an object");
    }
    let mut stats = BTreeMap::new();
    if let Some(Value::Object(data)) = value.get("data") {
        for (alias, entry) in data {
            let Some(index) = alias.strip_prefix('s').and_then(|index| index.parse::<usize>().ok()) else { continue };
            let Some(pr) = entry.get("pullRequest").filter(|pr| !pr.is_null()) else { continue };
            stats.insert(
                index,
                (
                    pr.get("additions").and_then(as_int).unwrap_or(0).max(0) as u64,
                    pr.get("deletions").and_then(as_int).unwrap_or(0).max(0) as u64,
                ),
            );
        }
    }
    Ok(stats)
}

/// Avatar URLs by login (`decodeActorAvatarsJson`).
pub fn decode_actor_avatars(raw: &str) -> Decoded<BTreeMap<String, String>> {
    let value = parse(raw)?;
    let nodes = required_object(&value, "data")?.get("nodes").and_then(Value::as_array).ok_or_else(|| DecodeFailure("nodes must be an array".into()))?;
    let mut avatars = BTreeMap::new();
    for node in nodes.iter().filter(|node| !node.is_null()) {
        if let (Some(login), Some(avatar)) = (trimmed(node.get("login")), trimmed(node.get("avatarUrl"))) {
            avatars.insert(login, avatar);
        }
    }
    Ok(avatars)
}

// ---------------------------------------------------------------------------------------------
// Listings, summaries and stacks
// ---------------------------------------------------------------------------------------------

fn stack_membership(raw: &Value) -> Option<t3_contracts::PullRequestStackMembership> {
    let stack = member(raw, "stack")?;
    let entry = member(raw, "stackEntry")?;
    Some(t3_contracts::PullRequestStackMembership {
        number: stack.get("number").and_then(as_int)?.max(0) as u64,
        size: stack.get("size").and_then(as_int)?.max(0) as u64,
        base: stack.get("baseRefName").and_then(Value::as_str)?.to_string(),
        position: entry.get("position").and_then(as_int)?.max(0) as u64,
    })
}

/// One listing row, in the shape a search or a repository's own list returns it. `None` when the
/// row does not decode, so one malformed pull request cannot hide the rest.
fn to_row(entry: &Value) -> Option<(GitHubListItem, Option<String>)> {
    if !entry.is_object() {
        return None;
    }
    let requests: Vec<Value> = nodes_of(entry.get("reviewRequests"))
        .into_iter()
        .filter_map(|request| {
            let reviewer = request.get("requestedReviewer")?;
            if let Some(login) = trimmed(reviewer.get("login")) {
                return Some(serde_json::json!({"login":login}));
            }
            trimmed(reviewer.get("slug")).map(|slug| serde_json::json!({"slug":slug}))
        })
        .collect();
    let requests: Vec<&Value> = requests.iter().collect();
    let rollup: Vec<Value> = nodes_of(entry.get("commits"))
        .into_iter()
        .filter_map(|node| {
            let state = trimmed(node.get("commit")?.get("statusCheckRollup")?.get("state"))?;
            Some(serde_json::json!({"state":state}))
        })
        .collect();
    let mut item = list_item(entry, &requests, entry.get("labels"), &rollup).ok()?;
    item.stack = stack_membership(entry);
    Some((item, trimmed(entry.get("repository").and_then(|repository| repository.get("nameWithOwner")))))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchItem {
    #[serde(flatten)]
    pub item: GitHubListItem,
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchBatch {
    pub items: Vec<SearchItem>,
    pub raw_count: usize,
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

/// One page of the host-wide search (`decodePullRequestSearchJson`).
pub fn decode_pull_request_search(raw: &str) -> Decoded<SearchBatch> {
    let value = parse(raw)?;
    let search = required_object(required_object(&value, "data")?, "search")?;
    let nodes: Vec<&Value> = match search.get("nodes") {
        Some(Value::Array(nodes)) => nodes.iter().collect(),
        Some(Value::Null) | None => vec![],
        Some(_) => return fail("search.nodes must be an array"),
    };
    let has_next = search.get("pageInfo").and_then(|info| info.get("hasNextPage")).and_then(Value::as_bool);
    let items = nodes
        .iter()
        .filter_map(|node| {
            let (item, repository) = to_row(node)?;
            Some(SearchItem { item, repository: repository? })
        })
        .collect();
    Ok(SearchBatch {
        items,
        raw_count: nodes.len(),
        has_next_page: has_next.unwrap_or(false),
        end_cursor: if has_next == Some(true) { trimmed(search.get("pageInfo").and_then(|info| info.get("endCursor"))) } else { None },
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListBatch {
    pub items: Vec<GitHubListItem>,
    pub raw_count: usize,
    pub end_cursor: Option<String>,
}

/// One page of a repository's own list (`decodePullRequestListJson`).
pub fn decode_pull_request_list(raw: &str) -> Decoded<ListBatch> {
    let value = parse(raw)?;
    let repository = required_object(&value, "data")?.get("repository").ok_or_else(|| DecodeFailure("missing repository".into()))?;
    let connection = match repository {
        Value::Null => None,
        repository => Some(required_object(repository, "pullRequests")?),
    };
    let nodes: Vec<&Value> = match connection.and_then(|connection| connection.get("nodes")) {
        Some(Value::Array(nodes)) => nodes.iter().collect(),
        Some(Value::Null) | None => vec![],
        Some(_) => return fail("pullRequests.nodes must be an array"),
    };
    let page_info = connection.and_then(|connection| connection.get("pageInfo"));
    Ok(ListBatch {
        items: nodes.iter().filter_map(|node| to_row(node).map(|row| row.0)).collect(),
        raw_count: nodes.len(),
        end_cursor: if page_info.and_then(|info| info.get("hasNextPage")).and_then(Value::as_bool) == Some(true) {
            trimmed(page_info.and_then(|info| info.get("endCursor")))
        } else {
            None
        },
    })
}

/// What a linked thread keeps current for one pull request (`GitHubPullRequestSummary`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubSummary {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub updated_at: String,
    pub author: Option<PullRequestActor>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub review_decision: Option<PullRequestReviewDecision>,
    pub checks_state: Option<PullRequestChecksState>,
    pub mergeability: PullRequestMergeability,
    /// `Some(None)` when GitHub says the pull request is in no stack; absent when not asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<Option<t3_contracts::PullRequestStackMembership>>,
}

/// Summaries by the position they were asked in (`decodePullRequestSummariesJson`). A pull
/// request GitHub answered nothing for, or whose fields no longer decode, is absent.
pub fn decode_summaries(raw: &str) -> Decoded<BTreeMap<usize, GitHubSummary>> {
    let value = parse(raw)?;
    if !value.is_object() {
        return fail("expected an object");
    }
    let mut summaries = BTreeMap::new();
    let Some(Value::Object(data)) = value.get("data") else { return Ok(summaries) };
    for (alias, entry) in data {
        let Some(index) = alias.strip_prefix('s').and_then(|index| index.parse::<usize>().ok()) else { continue };
        let Some(pr) = entry.get("pullRequest").filter(|pr| !pr.is_null()) else { continue };
        let build = || -> Decoded<GitHubSummary> {
            let rollup: Vec<Value> = nodes_of(pr.get("commits"))
                .into_iter()
                .filter_map(|node| {
                    let state = trimmed(node.get("commit")?.get("statusCheckRollup")?.get("state"))?;
                    Some(serde_json::json!({"state":state}))
                })
                .collect();
            Ok(GitHubSummary {
                number: required_int(pr, "number")?.max(0) as u64,
                title: required_str(pr, "title")?.to_string(),
                url: required_str(pr, "url")?.to_string(),
                head_branch: required_str(pr, "headRefName")?.to_string(),
                base_branch: required_str(pr, "baseRefName")?.to_string(),
                state: to_state(pr),
                is_draft: pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
                closed_at: trimmed(pr.get("closedAt")),
                merged_at: trimmed(pr.get("mergedAt")),
                updated_at: required_str(pr, "updatedAt")?.to_string(),
                author: to_actor(pr.get("author")),
                additions: pr.get("additions").and_then(as_int).unwrap_or(0).max(0) as u64,
                deletions: pr.get("deletions").and_then(as_int).unwrap_or(0).max(0) as u64,
                changed_files: pr.get("changedFiles").and_then(as_int).unwrap_or(0).max(0) as u64,
                review_decision: to_review_decision_with_reviews(pr.get("reviewDecision"), pr.get("latestReviews")),
                checks_state: rollup_checks_state(&rollup),
                mergeability: to_mergeability(pr.get("mergeable")),
                stack: pr.get("stack").map(|_| stack_membership(pr)),
            })
        };
        if let Ok(summary) = build() {
            summaries.insert(index, summary);
        }
    }
    Ok(summaries)
}

/// A host-native stack (`decodePullRequestStacksJson`); `None` when the host reports none.
pub fn decode_stacks(raw: &str) -> Decoded<Option<t3_contracts::PullRequestStack>> {
    let value = parse(raw)?;
    let Value::Array(stacks) = value else { return fail("expected an array") };
    let mut decoded = vec![];
    for stack in &stacks {
        let number = required_int(stack, "number")?.max(0) as u64;
        let url = required_str(stack, "url")?.to_string();
        let base = match stack.get("base") {
            Some(Value::String(base)) => base.clone(),
            Some(base) => required_str(base, "ref")?.to_string(),
            None => return fail("missing base"),
        };
        let layers = stack
            .get("pull_requests")
            .and_then(Value::as_array)
            .ok_or_else(|| DecodeFailure("pull_requests must be an array".into()))?
            .iter()
            .map(|pull_request| {
                let head = required_object(pull_request, "head")?;
                Ok(t3_contracts::PullRequestStackLayer {
                    number: required_int(pull_request, "number")?.max(0) as u64,
                    title: pull_request.get("title").and_then(Value::as_str).map(str::to_string),
                    is_draft: pull_request.get("draft").and_then(Value::as_bool),
                    head_sha: head.get("sha").and_then(Value::as_str).map(str::to_string),
                    head_branch: required_str(head, "ref")?.to_string(),
                    state: to_state(&serde_json::json!({"state":pull_request.get("state"),"mergedAt":pull_request.get("merged_at")})),
                })
            })
            .collect::<Decoded<Vec<_>>>()?;
        let id = match member(stack, "id") {
            Some(Value::String(id)) => id.clone(),
            Some(id) => id.to_string(),
            None => trimmed(stack.get("node_id")).unwrap_or_else(|| number.to_string()),
        };
        decoded.push(t3_contracts::PullRequestStack {
            id,
            number,
            url: trimmed(stack.get("html_url")).unwrap_or(url),
            base,
            layers,
        });
    }
    Ok(decoded.into_iter().next())
}

#[cfg(test)]
#[path = "pull_request_decode_tests.rs"]
mod tests;
