//! What a viewer may do to a GitHub pull request beyond remarking on it: merge, close, reopen,
//! mark ready, return to draft, update the branch, arm or disarm auto-merge, revert, approve a
//! fork's workflows, ask for reviewers and change labels (port of `runPullRequestAction` and its
//! neighbours in `GitHubPullRequestCli.ts`, and of the gates in `PullRequestService.ts`).
//!
//! Three layers, each kept apart so the refusals can be proved without a host:
//! - the gates (`authorize_*`): what the host offers and what the signed-in account may do, which
//!   decide before any write is sent; a refused call changes nothing;
//! - the requests (`impl GitHub`): exactly what each action sends, and in what order;
//! - the service methods, which run a gated write as an interactive caller and then strand what
//!   is held for the pull request.
//!
//! Not ported: native stack actions (`stackNumber`). The capabilities this server states leave
//! `stackActions` out, so the gate refuses them with the original's own sentence for a host that
//! cannot.
use crate::{
    pull_request_decode::{Decoded, DecodeFailure, trimmed},
    pull_request_github::{self as github, GitHub, PrFailure, Read, Reason, Target},
    pull_request_queries as queries,
    pull_request_service::{Env, PrError, PrResult, PullRequestService, Resolved},
    source_control_github_api::{self as api, GraphQlInput, Method, RestInput},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use t3_contracts::*;

// ---------------------------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------------------------

const MERGE_MESSAGE_GRAPHQL_QUERY: &str = r#"
query PullRequestMergeMessage($owner: String!, $name: String!, $number: Int!, $method: PullRequestMergeMethod!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      isMergeQueueEnabled
      headRefOid
      viewerMergeBodyText(mergeType: $method)
    }
  }
}"#;

/// What `gh pr merge` and `gh pr update-branch` read before they act.
const ACTION_STATE_GRAPHQL_QUERY: &str = r#"
query PullRequestActionState($owner: String!, $name: String!, $number: Int!, $headRef: String!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id
      headRefOid
      isMergeQueueEnabled
      mergeStateStatus
      baseRef { compare(headRef: $headRef) { behindBy } }
    }
  }
}"#;

const MERGE_PULL_REQUEST_GRAPHQL_MUTATION: &str = r#"mutation($input: MergePullRequestInput!) {
  mergePullRequest(input: $input) { clientMutationId }
}"#;
const ENABLE_AUTO_MERGE_GRAPHQL_MUTATION: &str = r#"mutation($input: EnablePullRequestAutoMergeInput!) {
  enablePullRequestAutoMerge(input: $input) { clientMutationId }
}"#;
const DISABLE_AUTO_MERGE_GRAPHQL_MUTATION: &str = r#"mutation($pullRequestId: ID!) {
  disablePullRequestAutoMerge(input: { pullRequestId: $pullRequestId }) { clientMutationId }
}"#;
const UPDATE_BRANCH_GRAPHQL_MUTATION: &str = r#"mutation($pullRequestId: ID!, $expectedHeadOid: GitObjectID!, $updateMethod: PullRequestBranchUpdateMethod!) {
  updatePullRequestBranch(input: { pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid, updateMethod: $updateMethod }) { clientMutationId }
}"#;
const READY_FOR_REVIEW_GRAPHQL_MUTATION: &str = r#"mutation($pullRequestId: ID!) {
  markPullRequestReadyForReview(input: { pullRequestId: $pullRequestId }) { clientMutationId }
}"#;
const CONVERT_TO_DRAFT_GRAPHQL_MUTATION: &str = r#"mutation($pullRequestId: ID!) {
  convertPullRequestToDraft(input: { pullRequestId: $pullRequestId }) { clientMutationId }
}"#;
const CLOSE_PULL_REQUEST_GRAPHQL_MUTATION: &str = r#"mutation($pullRequestId: ID!) {
  closePullRequest(input: { pullRequestId: $pullRequestId }) { clientMutationId }
}"#;
const REOPEN_PULL_REQUEST_GRAPHQL_MUTATION: &str = r#"mutation($pullRequestId: ID!) {
  reopenPullRequest(input: { pullRequestId: $pullRequestId }) { clientMutationId }
}"#;

/// States in which `gh pr merge --auto` merges at once instead of arming auto-merge.
const IMMEDIATELY_MERGEABLE: [&str; 3] = ["CLEAN", "HAS_HOOKS", "UNSTABLE"];
const WORKFLOW_APPROVAL_LIMIT: usize = 1_000;

// ---------------------------------------------------------------------------------------------
// Decoders and builders
// ---------------------------------------------------------------------------------------------

fn parse(raw: &str) -> Decoded<Value> {
    serde_json::from_str(raw).map_err(|error| DecodeFailure(error.to_string()))
}

fn bad<T>(what: &str) -> Decoded<T> {
    Err(DecodeFailure(what.to_string()))
}

/// An optional string member: absent or null is fine, anything else that is not a string is not.
fn optional_text<'a>(value: &'a Value, key: &str) -> Decoded<Option<&'a str>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => bad(&format!("{key} must be a string")),
    }
}

/// A node of a connection, which may be null, and must otherwise be an object.
fn array_of<'a>(value: Option<&'a Value>, what: &str) -> Decoded<&'a Vec<Value>> {
    value.and_then(Value::as_array).map_or_else(|| bad(&format!("{what} must be an array")), Ok)
}

fn actor_member<'a>(node: &'a Value, what: &str) -> Decoded<&'a Value> {
    for key in ["login", "name", "avatarUrl", "slug"] {
        optional_text(node, key).map_err(|_| DecodeFailure(format!("{what}.{key} must be a string")))?;
    }
    Ok(node)
}

/// The people this pull request may be sent to, with whoever is already on it marked. The author
/// is dropped; whoever has been asked leads the list even where GitHub does not count them
/// assignable, because a request that cannot be seen cannot be taken back
/// (`decodeReviewerCandidatesJson`).
pub fn decode_reviewer_candidates(raw: &str) -> Decoded<PullRequestReviewerCandidateList> {
    let value = parse(raw)?;
    let Some(repository) = value.get("data").and_then(|data| data.get("repository")).filter(|repository| repository.is_object()) else {
        return bad("data.repository must be an object");
    };
    let assignable = repository.get("assignableUsers").filter(|users| users.is_object()).map_or_else(|| bad("assignableUsers must be an object"), Ok)?;
    let users = array_of(assignable.get("nodes"), "assignableUsers.nodes")?;
    for user in users.iter().filter(|user| !user.is_null()) {
        actor_member(user, "assignableUsers")?;
    }
    let pull_request = match repository.get("pullRequest") {
        Some(Value::Object(_)) => repository.get("pullRequest"),
        Some(Value::Null) => None,
        _ => return bad("pullRequest must be an object or null"),
    };
    let author = pull_request.and_then(|pull_request| pull_request.get("author")).and_then(|author| trimmed(author.get("login")));
    let mut candidates: Vec<(String, PullRequestReviewerCandidate)> = vec![];
    let requests = match pull_request.and_then(|pull_request| pull_request.get("reviewRequests")) {
        Some(requests) if requests.is_object() => array_of(requests.get("nodes"), "reviewRequests.nodes")?.iter().collect(),
        _ => vec![],
    };
    for request in requests {
        let reviewer = request.get("requestedReviewer").filter(|reviewer| reviewer.is_object());
        if let Some(reviewer) = reviewer {
            actor_member(reviewer, "requestedReviewer")?;
        }
        let slug = reviewer.and_then(|reviewer| trimmed(reviewer.get("slug")));
        let Some(id) = slug.clone().or_else(|| reviewer.and_then(|reviewer| trimmed(reviewer.get("login")))) else { continue };
        let kind = if slug.is_some() { PullRequestReviewerKind::Team } else { PullRequestReviewerKind::User };
        let key = format!("{} {id}", if slug.is_some() { "team" } else { "user" });
        let candidate = PullRequestReviewerCandidate {
            is_bot: None,
            login: id.clone(),
            name: reviewer.and_then(|reviewer| trimmed(reviewer.get("name"))),
            avatar_url: reviewer.and_then(|reviewer| trimmed(reviewer.get("avatarUrl"))),
            id,
            kind,
            is_requested: true,
        };
        // A later entry for the same reviewer replaces the earlier one in place, as a Map does.
        match candidates.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = candidate,
            None => candidates.push((key, candidate)),
        }
    }
    for user in users {
        let Some(login) = trimmed(user.get("login")) else { continue };
        let key = format!("user {login}");
        if Some(&login) == author.as_ref() || candidates.iter().any(|(existing, _)| *existing == key) {
            continue;
        }
        candidates.push((
            key,
            PullRequestReviewerCandidate {
                is_bot: None,
                id: login.clone(),
                kind: PullRequestReviewerKind::User,
                login,
                name: trimmed(user.get("name")),
                avatar_url: trimmed(user.get("avatarUrl")),
                is_requested: false,
            },
        ));
    }
    let truncated = assignable.get("pageInfo").and_then(|info| info.get("hasNextPage")).and_then(Value::as_bool) == Some(true);
    Ok(PullRequestReviewerCandidateList { candidates: candidates.into_iter().map(|(_, candidate)| candidate).collect(), truncated })
}

/// The repository's labels, with the ones already on this pull request marked. A label the pull
/// request wears that the repository no longer defines leads the list anyway, because a label
/// that cannot be seen cannot be taken off (`decodeLabelCandidatesJson`).
pub fn decode_label_candidates(raw: &str) -> Decoded<PullRequestLabelCandidateList> {
    let value = parse(raw)?;
    let Some(repository) = value.get("data").and_then(|data| data.get("repository")).filter(|repository| repository.is_object()) else {
        return bad("data.repository must be an object");
    };
    let label_nodes = |connection: Option<&Value>, what: &str| -> Decoded<Vec<Value>> {
        match connection {
            None | Some(Value::Null) => Ok(vec![]),
            Some(connection) if connection.is_object() => {
                let nodes = array_of(connection.get("nodes"), what)?;
                for node in nodes.iter().filter(|node| !node.is_null()) {
                    if !node.get("name").is_some_and(Value::is_string) {
                        return bad("label.name must be a string");
                    }
                    optional_text(node, "color")?;
                    optional_text(node, "description")?;
                }
                Ok(nodes.clone())
            }
            Some(_) => bad(&format!("{what} must be an object")),
        }
    };
    let defined = label_nodes(repository.get("labels"), "labels.nodes")?;
    let pull_request = match repository.get("pullRequest") {
        Some(Value::Object(_)) => repository.get("pullRequest"),
        Some(Value::Null) => None,
        _ => return bad("pullRequest must be an object or null"),
    };
    let worn = label_nodes(pull_request.and_then(|pull_request| pull_request.get("labels")), "pullRequest.labels.nodes")?;
    let applied: BTreeSet<String> = worn.iter().filter_map(|label| trimmed(label.get("name"))).collect();
    let mut candidates: Vec<PullRequestLabelCandidate> = vec![];
    for label in &defined {
        let Some(name) = trimmed(label.get("name")) else { continue };
        let candidate = PullRequestLabelCandidate { color: trimmed(label.get("color")), description: trimmed(label.get("description")), is_applied: applied.contains(&name), name };
        match candidates.iter_mut().find(|existing| existing.name == candidate.name) {
            Some(slot) => *slot = candidate,
            None => candidates.push(candidate),
        }
    }
    let mut leading: Vec<PullRequestLabelCandidate> = applied
        .iter()
        .filter(|name| !candidates.iter().any(|candidate| &candidate.name == *name))
        .map(|name| PullRequestLabelCandidate { name: name.clone(), color: None, description: None, is_applied: true })
        .collect();
    // Applied labels keep the order the pull request reported them in, not alphabetical order.
    leading.sort_by_key(|candidate| worn.iter().position(|label| trimmed(label.get("name")).as_deref() == Some(candidate.name.as_str())));
    leading.extend(candidates);
    let truncated = repository.get("labels").and_then(|labels| labels.get("pageInfo")).and_then(|info| info.get("hasNextPage")).and_then(Value::as_bool) == Some(true);
    Ok(PullRequestLabelCandidateList { candidates: leading, truncated })
}

/// A workflow run waiting on a maintainer (`GitHubWorkflowRunApproval`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub id: u64,
    pub name: String,
    pub url: Option<String>,
}

pub struct WorkflowRunPage {
    pub runs: Vec<WorkflowRun>,
    /// Runs on the page, counted before decoding, which is what decides whether to page on.
    pub raw_count: usize,
}

/// One page of `GET /repos/{owner}/{repo}/actions/runs` (`decodeWorkflowRunsJson`).
pub fn decode_workflow_runs(raw: &str) -> Decoded<WorkflowRunPage> {
    let value = parse(raw)?;
    let runs = array_of(value.get("workflow_runs"), "workflow_runs")?;
    let mut decoded = vec![];
    for run in runs {
        let Some(id) = run.get("id").and_then(|id| id.as_u64().or_else(|| id.as_f64().filter(|float| float.fract() == 0.0 && *float >= 0.0).map(|float| float as u64))) else {
            return bad("workflow_runs.id must be an integer");
        };
        optional_text(run, "name")?;
        optional_text(run, "html_url")?;
        decoded.push(WorkflowRun { id, name: trimmed(run.get("name")).unwrap_or_else(|| format!("Workflow run {id}")), url: trimmed(run.get("html_url")) });
    }
    Ok(WorkflowRunPage { raw_count: runs.len(), runs: decoded })
}

/// An open pull request whose head branch has the name that was asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestHead {
    pub number: u64,
    pub head_sha: String,
    pub is_cross_repository: Option<bool>,
    pub head_repository_owner: Option<String>,
}

pub struct HeadsPage {
    pub heads: Vec<PullRequestHead>,
    pub next_cursor: Option<String>,
}

/// `decodePullRequestHeadsJson`.
pub fn decode_pull_request_heads(raw: &str) -> Decoded<HeadsPage> {
    let value = parse(raw)?;
    let repository = match value.get("data").and_then(|data| data.get("repository")) {
        Some(Value::Object(repository)) => Some(repository),
        Some(Value::Null) => None,
        _ => return bad("data.repository must be an object or null"),
    };
    let Some(connection) = repository.map(|repository| repository.get("pullRequests")) else {
        return Ok(HeadsPage { heads: vec![], next_cursor: None });
    };
    let Some(connection) = connection.filter(|connection| connection.is_object()) else { return bad("pullRequests must be an object") };
    let mut heads = vec![];
    for node in array_of(connection.get("nodes"), "pullRequests.nodes")? {
        let number = node.get("number").and_then(Value::as_u64);
        let head_sha = node.get("headRefOid").and_then(Value::as_str);
        let (Some(number), Some(head_sha)) = (number, head_sha) else { return bad("head must carry number and headRefOid") };
        heads.push(PullRequestHead {
            number,
            head_sha: head_sha.to_string(),
            is_cross_repository: node.get("isCrossRepository").and_then(Value::as_bool),
            head_repository_owner: node.get("headRepositoryOwner").and_then(|owner| trimmed(owner.get("login"))),
        });
    }
    let page_info = connection.get("pageInfo");
    let next_cursor = (page_info.and_then(|info| info.get("hasNextPage")).and_then(Value::as_bool) == Some(true)).then(|| trimmed(page_info.and_then(|info| info.get("endCursor")))).flatten();
    Ok(HeadsPage { heads, next_cursor })
}

/// The body of `POST`/`DELETE /repos/{owner}/{repo}/pulls/{number}/requested_reviewers`, which
/// takes people and teams in two lists of its own (`buildReviewerRequest`).
pub fn build_reviewer_request(reviewers: &[PullRequestReviewerRequestTarget]) -> Value {
    let of_kind = |kind: PullRequestReviewerKind| reviewers.iter().filter(|reviewer| reviewer.kind == kind).map(|reviewer| reviewer.id.as_str()).collect::<Vec<_>>();
    json!({"reviewers":of_kind(PullRequestReviewerKind::User),"team_reviewers":of_kind(PullRequestReviewerKind::Team)})
}

/// The body of `POST /repos/{owner}/{repo}/issues/{number}/labels`, which adds to what is there.
pub fn build_label_request(labels: &[&str]) -> Value {
    json!({"labels":labels})
}

// ---------------------------------------------------------------------------------------------
// Agent credits
// ---------------------------------------------------------------------------------------------

const AGENT_EMAILS: [&str; 7] = [
    "noreply@anthropic.com",
    "claude@anthropic.com",
    "noreply@openai.com",
    "codex@openai.com",
    "cursoragent@cursor.com",
    "copilot@github.com",
    "noreply@github.com",
];

/// Removes standalone agent credits while keeping human signatures and quoted examples
/// (`removeAgentCredits`).
pub fn remove_agent_credits(message: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;
    struct Patterns {
        delimiter: Regex,
        blank: Regex,
        author: Regex,
        link: Regex,
        generated: Regex,
        tail: Regex,
        bullet: Regex,
    }
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| Patterns {
        delimiter: Regex::new(r"^ {0,3}(`{3,}|~{3,})([^\n\r\u{2028}\u{2029}]*)$").unwrap(),
        blank: Regex::new(r"^[ \t]*$").unwrap(),
        author: Regex::new(r"(?i)^co-authored-by:\s*([^\n\r\u{2028}\u{2029}]*?)\s*<([^<>]+)>\s*$").unwrap(),
        link: Regex::new(r"(?i)\[([^\]]+)\]\(https://(?:claude\.ai|www\.anthropic\.com|chatgpt\.com|openai\.com|cursor\.com|github\.com|opencode\.ai|x\.ai|antigravity\.google)(?:/[^\s)]*)?\)").unwrap(),
        generated: Regex::new(r"(?i)^(?:🤖\s*)?generated (?:with|by)\s+(?:claude(?: code)?|codex|cursor|github copilot|copilot|opencode|grok|antigravity)[.!]?$").unwrap(),
        tail: Regex::new(r"(?:\r?\n[ \t]*)+$").unwrap(),
        bullet: Regex::new(r"^[-*]\s+").unwrap(),
    });
    let newline = if message.contains("\r\n") { "\r\n" } else { "\n" };
    let mut fence: Option<String> = None;
    let mut removed = false;
    let mut kept: Vec<&str> = vec![];
    for line in message.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)) {
        if line.starts_with("    ") || line.starts_with('\t') {
            kept.push(line);
            continue;
        }
        if let Some(captures) = patterns.delimiter.captures(line) {
            let marker = captures.get(1).map_or("", |found| found.as_str());
            let remainder = captures.get(2).map_or("", |found| found.as_str());
            match &fence {
                None => {
                    if marker.starts_with('~') || !remainder.contains('`') {
                        fence = Some(marker.to_string());
                    }
                }
                Some(open) => {
                    if marker.starts_with(open.chars().next().unwrap_or(' ')) && marker.len() >= open.len() && patterns.blank.is_match(remainder) {
                        fence = None;
                    }
                }
            }
            kept.push(line);
            continue;
        }
        if fence.is_some() {
            kept.push(line);
            continue;
        }
        let trimmed_line = line.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
        let text = patterns.bullet.replace(trimmed_line, "");
        let agent_author = patterns.author.captures(&text).is_some_and(|author| {
            let email = author.get(2).map(|found| found.as_str().to_lowercase()).unwrap_or_default();
            let name = author.get(1).map_or("", |found| found.as_str());
            // GitHub's generic noreply address is also used by people; require Copilot's name.
            AGENT_EMAILS.contains(&email.as_str()) && (email != "noreply@github.com" || name.eq_ignore_ascii_case("github copilot") || name.eq_ignore_ascii_case("copilot"))
        });
        let footer = patterns.link.replace_all(&text, "$1");
        if !agent_author && !patterns.generated.is_match(&footer) {
            kept.push(line);
            continue;
        }
        removed = true;
    }
    if removed { patterns.tail.replace(&kept.join(newline), "").into_owned() } else { message.to_string() }
}

// ---------------------------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------------------------

/// Why an action is refused to this viewer, said as the access it would take rather than as the
/// refusal the host would have answered with. Merging is the one that needs write and nothing
/// else; the other four are also the author's to take, whatever access they have.
pub fn access_refusal(action: PullRequestAction) -> &'static str {
    use PullRequestAction::*;
    match action {
        Merge => "You need write access on this repository to merge.",
        Ready => "You need write access on this repository, or to have opened this change request, to mark it ready for review.",
        Draft => "You need write access on this repository, or to have opened this change request, to return it to a draft.",
        Close => "You need write access on this repository, or to have opened this change request, to close it.",
        UpdateBranch => "You need write access on this repository, or to have opened this change request, to update its branch.",
        Reopen => "You need write access on this repository, or to have opened this change request, to reopen it.",
        EnableAutoMerge => "You need write access on this repository to have it merged for you once it is ready.",
        DisableAutoMerge => "You need write access on this repository to stop it being merged for you once it is ready.",
        Revert => "You need write access on this repository to open a revert pull request.",
        ApproveWorkflows => "You need write access on this repository to approve workflows from a fork pull request.",
    }
}

const REVIEWER_REQUEST_REFUSAL: &str = "You need write access on this repository to ask for a review.";
const LABEL_CHANGE_REFUSAL: &str = "You need triage access on this repository to change its labels.";

fn action_name(action: PullRequestAction) -> String {
    serde_json::to_value(action).ok().and_then(|value| value.as_str().map(str::to_string)).unwrap_or_default()
}

fn merge_method_name(method: PullRequestMergeMethod) -> &'static str {
    match method {
        PullRequestMergeMethod::Merge => "merge",
        PullRequestMergeMethod::Squash => "squash",
        PullRequestMergeMethod::Rebase => "rebase",
    }
}

fn update_method_name(method: PullRequestUpdateMethod) -> &'static str {
    match method {
        PullRequestUpdateMethod::Merge => "merge",
        PullRequestUpdateMethod::Rebase => "rebase",
    }
}

/// The viewer's permissions for an action, read fresh on demand: `true` asks for the update-branch
/// answer too, which costs the heavier read.
pub type ViewerFetch<'a> = Box<dyn FnOnce(bool) -> BoxFuture<'a, PrResult<PullRequestViewerPermissions>> + Send + 'a>;

/// Decides whether a run may be sent. Everything the host's capabilities settle is checked first,
/// because it costs nothing; the account's permissions are asked last, because they cost a
/// request. Returns `Ok` only when every gate said yes (`runAction` in `PullRequestService.ts`).
pub async fn authorize_action(input: &PullRequestActionInput, capabilities: &PullRequestCapabilities, fetch: ViewerFetch<'_>) -> PrResult<()> {
    let refuse = |detail: String| Err(PrError::operation("runAction", detail));
    if input.stack_number.is_some()
        && (capabilities.stack_actions != Some(true)
            || !matches!(input.action, PullRequestAction::Merge | PullRequestAction::UpdateBranch)
            || input.expected_stack_heads.is_none()
            || (input.action == PullRequestAction::UpdateBranch && input.update_method != Some(PullRequestUpdateMethod::Rebase)))
    {
        return refuse("This stack action is not supported or has no expected head revision.".into());
    }
    // The surface hides what a host cannot do, and this refuses it as well: a request that reached
    // here anyway must not be handed to a provider that never claimed the action.
    if !capabilities.actions.contains(&input.action) {
        return refuse(format!("This host cannot {} a change request.", action_name(input.action)));
    }
    // A strategy the host does not offer must be refused rather than passed on: every provider
    // maps an unrecognised method to its own default, so asking one that cannot rebase to rebase
    // would quietly merge instead of failing.
    if let Some(method) = input.merge_method {
        if !capabilities.merge_methods.contains(&method) {
            return refuse(format!("This host cannot merge with the {} strategy.", merge_method_name(method)));
        }
    }
    if let Some(method) = input.update_method {
        if !capabilities.update_methods.as_deref().unwrap_or_default().contains(&method) {
            return refuse(format!("This host cannot update a branch by {}.", update_method_name(method)));
        }
    }
    let viewer = fetch(input.action == PullRequestAction::UpdateBranch).await?;
    let stack_rebase = input.stack_number.is_some() && input.action == PullRequestAction::UpdateBranch;
    if if stack_rebase { viewer.stack_rebase != Some(true) } else { !viewer.actions.contains(&input.action) } {
        return refuse(access_refusal(input.action).into());
    }
    if !stack_rebase {
        if let Some(method) = input.update_method {
            if !viewer.update_methods.as_deref().unwrap_or_default().contains(&method) {
                return refuse(access_refusal(PullRequestAction::UpdateBranch).into());
            }
        }
    }
    Ok(())
}

/// Who may be asked is only ever wanted by somebody about to ask, because the menu it fills is
/// the one the request is made from, so the same permission guards both.
pub async fn authorize_reviewer_candidates(capabilities: &PullRequestCapabilities, fetch: ViewerFetch<'_>) -> PrResult<()> {
    if !capabilities.reviewers.list_candidates {
        return Err(PrError::operation("reviewerCandidates", "This host cannot say who may review a change request."));
    }
    if !fetch(false).await?.request_reviewers {
        return Err(PrError::operation("reviewerCandidates", REVIEWER_REQUEST_REFUSAL));
    }
    Ok(())
}

pub async fn authorize_request_reviewers(capabilities: &PullRequestCapabilities, fetch: ViewerFetch<'_>) -> PrResult<()> {
    if !capabilities.reviewers.request {
        return Err(PrError::operation("requestReviewers", "This host cannot ask somebody for a review."));
    }
    if !fetch(false).await?.request_reviewers {
        return Err(PrError::operation("requestReviewers", REVIEWER_REQUEST_REFUSAL));
    }
    Ok(())
}

/// The labels, like the reviewer candidates, are wanted only by somebody about to change them, so
/// the same permission guards the list and the change.
pub async fn authorize_labels(operation: &str, capabilities: &PullRequestCapabilities, fetch: ViewerFetch<'_>) -> PrResult<()> {
    if capabilities.labels != Some(true) {
        return Err(PrError::operation(operation, "This host cannot change the labels on a change request."));
    }
    if fetch(false).await?.labels == Some(false) {
        return Err(PrError::operation(operation, LABEL_CHANGE_REFUSAL));
    }
    Ok(())
}

/// Whether a merge strips agent credits: the project's own choice, else the environment's, and
/// only for GitHub merges made directly (`removeAgentCreditsOnMerge` in the service).
pub fn removes_agent_credits(settings: Option<&ServerSettings>, project_id: &ProjectId, action: PullRequestAction, stack: bool) -> bool {
    if stack || !matches!(action, PullRequestAction::Merge | PullRequestAction::EnableAutoMerge) {
        return false;
    }
    let Some(settings) = settings else { return false };
    settings
        .project_settings_overrides
        .get(project_id)
        .and_then(|overrides| overrides.remove_agent_credits_on_merge)
        .unwrap_or(settings.remove_agent_credits_on_merge)
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

/// What a run is asked to do, as the host-facing layer takes it.
#[derive(Debug, Clone)]
pub struct RunAction {
    pub action: PullRequestAction,
    pub merge_method: Option<PullRequestMergeMethod>,
    pub update_method: Option<PullRequestUpdateMethod>,
    pub remove_agent_credits: bool,
}

struct ActionState {
    id: String,
    head_oid: String,
    merge_queue: Option<bool>,
    merge_state: Option<String>,
    behind_by: Option<i64>,
}

fn decode_action_state(raw: &str) -> Decoded<ActionState> {
    let value = parse(raw)?;
    let pull_request = value.pointer("/data/repository/pullRequest").filter(|pull_request| pull_request.is_object()).map_or_else(|| bad("pullRequest must be an object"), Ok)?;
    let (Some(id), Some(head_oid)) = (pull_request.get("id").and_then(Value::as_str), pull_request.get("headRefOid").and_then(Value::as_str)) else {
        return bad("id and headRefOid must be strings");
    };
    let behind_by = match pull_request.get("baseRef") {
        None | Some(Value::Null) => None,
        Some(base) => match base.get("compare") {
            Some(Value::Null) | None => return bad("compare must be present"),
            Some(compare) => Some(compare.get("behindBy").and_then(Value::as_i64).map_or_else(|| bad("behindBy must be an integer"), Ok)?),
        },
    };
    Ok(ActionState {
        id: id.to_string(),
        head_oid: head_oid.to_string(),
        merge_queue: pull_request.get("isMergeQueueEnabled").and_then(Value::as_bool),
        merge_state: pull_request.get("mergeStateStatus").and_then(Value::as_str).map(str::to_string),
        behind_by,
    })
}

struct MergeMessage {
    merge_queue: bool,
    head_oid: String,
    body: String,
}

fn decode_merge_message(raw: &str) -> Decoded<MergeMessage> {
    let value = parse(raw)?;
    let pull_request = value.pointer("/data/repository/pullRequest").filter(|pull_request| pull_request.is_object()).map_or_else(|| bad("pullRequest must be an object"), Ok)?;
    match (
        pull_request.get("isMergeQueueEnabled").and_then(Value::as_bool),
        pull_request.get("headRefOid").and_then(Value::as_str),
        pull_request.get("viewerMergeBodyText").and_then(Value::as_str),
    ) {
        (Some(merge_queue), Some(head_oid), Some(body)) => Ok(MergeMessage { merge_queue, head_oid: head_oid.to_string(), body: body.to_string() }),
        _ => bad("merge message fields must be present"),
    }
}

fn graphql_merge_method(method: Option<PullRequestMergeMethod>) -> &'static str {
    match method.unwrap_or(PullRequestMergeMethod::Merge) {
        PullRequestMergeMethod::Merge => "MERGE",
        PullRequestMergeMethod::Squash => "SQUASH",
        PullRequestMergeMethod::Rebase => "REBASE",
    }
}

impl GitHub {
    async fn read_document<T>(&self, target: &Target, operation: &str, query: &str, variables: Value, decoder: impl FnOnce(&str) -> Decoded<T>) -> Read<T> {
        let raw = Box::pin(api::graphql(
            self.host(),
            GraphQlInput { host: target.host.clone(), operation: operation.into(), query: query.into(), variables: Some(variables), allow_reserve: Some(true), ..Default::default() },
        ))
        .await?;
        decoder(raw.trim()).map_err(|_| PrFailure::unreadable(operation))
    }

    /// Sends a run's single request, which may be a mutation GitHub answers in its own words.
    async fn send_mutation(&self, target: &Target, query: &str, variables: Value) -> Read<()> {
        self.mutate(&target.host, "runPullRequestAction", query, variables).await
    }

    /// `runPullRequestAction`: every action, with the reads it makes before it writes. Anything
    /// that is not native to one pull request (a stack) never gets here.
    pub async fn run_action(&self, target: &Target, run: &RunAction) -> Read<()> {
        match run.action {
            PullRequestAction::Revert => {
                let id = self.node_id(target, "revertPullRequest").await?;
                self.mutate(&target.host, "revertPullRequest", queries::REVERT_PULL_REQUEST_GRAPHQL_MUTATION, json!({"pullRequestId":id})).await
            }
            PullRequestAction::ApproveWorkflows => self.approve_workflows(target).await,
            PullRequestAction::Ready | PullRequestAction::Draft | PullRequestAction::Close | PullRequestAction::Reopen | PullRequestAction::DisableAutoMerge => {
                let id = self.node_id(target, "runPullRequestAction").await?;
                let query = match run.action {
                    PullRequestAction::Ready => READY_FOR_REVIEW_GRAPHQL_MUTATION,
                    PullRequestAction::Draft => CONVERT_TO_DRAFT_GRAPHQL_MUTATION,
                    PullRequestAction::Close => CLOSE_PULL_REQUEST_GRAPHQL_MUTATION,
                    PullRequestAction::Reopen => REOPEN_PULL_REQUEST_GRAPHQL_MUTATION,
                    _ => DISABLE_AUTO_MERGE_GRAPHQL_MUTATION,
                };
                self.send_mutation(target, query, json!({"pullRequestId":id})).await
            }
            PullRequestAction::UpdateBranch | PullRequestAction::Merge | PullRequestAction::EnableAutoMerge => self.merge_or_update(target, run).await,
        }
    }

    async fn merge_or_update(&self, target: &Target, run: &RunAction) -> Read<()> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        // Read fresh rather than from the node id cache: merging and updating act on the head as
        // it stands now, and the merge queue decides which mutation a merge is.
        let state = self
            .read_document(
                target,
                "runPullRequestAction",
                ACTION_STATE_GRAPHQL_QUERY,
                json!({"owner":owner,"name":name,"number":target.number,"headRef":format!("refs/pull/{}/head",target.number)}),
                decode_action_state,
            )
            .await?;
        if run.action == PullRequestAction::UpdateBranch {
            // Already current is done, the way `gh pr update-branch` reports it.
            if state.behind_by == Some(0) {
                return Ok(());
            }
            // GitHub updates with a merge commit unless asked to rebase, which is its own default.
            let method = if run.update_method == Some(PullRequestUpdateMethod::Rebase) { "REBASE" } else { "MERGE" };
            return self
                .send_mutation(target, UPDATE_BRANCH_GRAPHQL_MUTATION, json!({"pullRequestId":state.id,"expectedHeadOid":state.head_oid,"updateMethod":method}))
                .await;
        }
        let mut body: Option<String> = None;
        let mut expected_head: Option<String> = None;
        if run.remove_agent_credits && run.merge_method != Some(PullRequestMergeMethod::Rebase) {
            let message = self
                .read_document(
                    target,
                    "runPullRequestAction",
                    MERGE_MESSAGE_GRAPHQL_QUERY,
                    json!({"owner":owner,"name":name,"number":target.number,"method":if run.merge_method == Some(PullRequestMergeMethod::Squash) { "SQUASH" } else { "MERGE" }}),
                    decode_merge_message,
                )
                .await?;
            // GitHub's merge queue chooses its own message and ignores custom text.
            if !message.merge_queue {
                let cleaned = remove_agent_credits(&message.body);
                if cleaned != message.body {
                    body = Some(cleaned);
                    expected_head = Some(message.head_oid);
                }
            }
        }
        // A merge queue takes a pull request through auto-merge rather than a direct merge, and
        // `--auto` on a pull request that is mergeable right now simply merges it, as `gh` does.
        // GitHub stores the strategy with a standing instruction rather than choosing one at merge
        // time, so arming still names it.
        let auto = state.merge_queue == Some(true)
            || (run.action == PullRequestAction::EnableAutoMerge && !IMMEDIATELY_MERGEABLE.contains(&state.merge_state.as_deref().unwrap_or("").to_uppercase().as_str()));
        let mut input = json!({"pullRequestId":state.id,"mergeMethod":graphql_merge_method(run.merge_method)});
        if let Some(head) = expected_head {
            input["expectedHeadOid"] = json!(head);
        }
        if let Some(body) = body {
            input["commitBody"] = json!(body);
        }
        self.send_mutation(target, if auto { ENABLE_AUTO_MERGE_GRAPHQL_MUTATION } else { MERGE_PULL_REQUEST_GRAPHQL_MUTATION }, json!({"input":input})).await
    }

    /// The open pull requests with this head branch's name, from whichever repository it lives
    /// in, up to one past the limit.
    async fn list_heads_by_branch(&self, target: &Target, head_branch: &str) -> Read<Vec<PullRequestHead>> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        let mut heads = vec![];
        let mut after: Option<String> = None;
        loop {
            let page = self
                .read_document(
                    target,
                    "listWorkflowRunsRequiringApproval",
                    queries::PULL_REQUEST_HEADS_GRAPHQL_QUERY,
                    json!({"owner":owner,"name":name,"head":head_branch,"after":after}),
                    decode_pull_request_heads,
                )
                .await?;
            heads.extend(page.heads);
            after = page.next_cursor;
            if after.is_none() || heads.len() > WORKFLOW_APPROVAL_LIMIT {
                return Ok(heads);
            }
        }
    }

    /// The runs waiting on a maintainer for this exact head, up to one past the limit.
    /// `head_sha` is what scopes them; `head_branch` is checked as well, the way
    /// `gh run list --branch` did.
    async fn list_action_required_runs(&self, target: &Target, head_sha: &str, head_branch: &str) -> Read<Vec<WorkflowRun>> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        let mut runs = vec![];
        let mut page = 1;
        while runs.len() <= WORKFLOW_APPROVAL_LIMIT {
            let path = format!(
                "repos/{owner}/{name}/actions/runs?head_sha={}&branch={}&event=pull_request&status=action_required&per_page=100&page={page}",
                encode_uri_component(head_sha),
                encode_uri_component(head_branch)
            );
            let response = Box::pin(api::rest(self.host(), RestInput { host: target.host.clone(), operation: "listWorkflowRunsRequiringApproval".into(), path, ..Default::default() })).await?;
            let read = decode_workflow_runs(response.body.trim()).map_err(|_| PrFailure::unreadable("listWorkflowRunsRequiringApproval"))?;
            runs.extend(read.runs);
            if read.raw_count < 100 {
                break;
            }
            page += 1;
        }
        Ok(runs)
    }

    /// `listWorkflowRunsRequiringApproval`: the head is looked up first and must be this pull
    /// request's alone, so a run is never approved on the strength of a branch name.
    pub async fn list_workflow_runs_requiring_approval(&self, target: &Target, head_sha: &str, head_branch: &str, head_owner: &str) -> Read<Vec<WorkflowRun>> {
        let heads = self.list_heads_by_branch(target, head_branch).await?;
        if heads.len() > WORKFLOW_APPROVAL_LIMIT {
            return Err(PrFailure::failed(format!("GitHub returned more than {WORKFLOW_APPROVAL_LIMIT} pull requests for this head branch.")));
        }
        let exact: Vec<&PullRequestHead> = heads
            .iter()
            .filter(|head| head.head_sha == head_sha && head.is_cross_repository == Some(true) && head.head_repository_owner.as_deref().map(str::to_lowercase) == Some(head_owner.to_lowercase()))
            .collect();
        if exact.len() != 1 || exact[0].number != target.number {
            return Err(PrFailure::failed(format!("The head revision matched {} pull requests instead of uniquely matching #{}.", exact.len(), target.number)));
        }
        let runs = self.list_action_required_runs(target, head_sha, head_branch).await?;
        if runs.len() > WORKFLOW_APPROVAL_LIMIT {
            return Err(PrFailure::failed(format!("GitHub returned more than {WORKFLOW_APPROVAL_LIMIT} workflow runs awaiting approval.")));
        }
        Ok(runs)
    }

    /// Approves the runs a fork's pull request is waiting on, one at a time, and only while the
    /// head the candidates were read for is still the pull request's head.
    async fn approve_workflows(&self, target: &Target) -> Read<()> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        let head_unavailable = || PrFailure::failed(format!("GitHub did not report a complete head revision for #{}.", target.number));
        let detail = self.detail(target).await?.detail;
        if detail.is_cross_repository != Some(true) {
            return Ok(());
        }
        let (Some(expected_sha), Some(expected_owner)) = (detail.head_sha.clone(), detail.head_repository_owner.clone()) else { return Err(head_unavailable()) };
        let expected_branch = detail.item.head_branch.clone();
        let runs = self.list_workflow_runs_requiring_approval(target, &expected_sha, &expected_branch, &expected_owner).await?;
        for run in runs {
            let current = self.detail(target).await?.detail;
            let (Some(sha), Some(owner_now)) = (current.head_sha.clone(), current.head_repository_owner.clone()) else { return Err(head_unavailable()) };
            if current.is_cross_repository != Some(true) || sha != expected_sha || current.item.head_branch != expected_branch || owner_now.to_lowercase() != expected_owner.to_lowercase() {
                return Err(PrFailure::failed(format!("The head revision of #{} changed before its workflows could be approved.", target.number)));
            }
            let still_waiting = self.list_workflow_runs_requiring_approval(target, &sha, &current.item.head_branch, &owner_now).await?;
            if still_waiting.iter().any(|waiting| waiting.id == run.id) {
                Box::pin(api::rest(
                    self.host(),
                    RestInput { host: target.host.clone(), operation: "approveWorkflowRun".into(), method: Method::Post, path: format!("repos/{owner}/{name}/actions/runs/{}/approve", run.id), ..Default::default() },
                ))
                .await?;
            }
        }
        Ok(())
    }

    pub async fn list_reviewer_candidates(&self, target: &Target) -> Read<PullRequestReviewerCandidateList> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        self.read_document(target, "listReviewerCandidates", queries::REVIEWER_CANDIDATES_GRAPHQL_QUERY, json!({"owner":owner,"name":name,"number":target.number}), decode_reviewer_candidates).await
    }

    pub async fn list_label_candidates(&self, target: &Target) -> Read<PullRequestLabelCandidateList> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        self.read_document(target, "listLabelCandidates", queries::LABEL_CANDIDATES_GRAPHQL_QUERY, json!({"owner":owner,"name":name,"number":target.number}), decode_label_candidates).await
    }

    /// Posting to a login GitHub has already been asked about is what a re-request is, and
    /// GitHub takes a request back from exactly whoever it was made of.
    pub async fn set_reviewer_request(&self, target: &Target, reviewers: &[PullRequestReviewerRequestTarget], requested: bool) -> Read<()> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        Box::pin(api::rest(
            self.host(),
            RestInput {
                host: target.host.clone(),
                operation: "setReviewerRequest".into(),
                method: if requested { Method::Post } else { Method::Delete },
                path: format!("repos/{owner}/{name}/pulls/{}/requested_reviewers", target.number),
                body: Some(build_reviewer_request(reviewers)),
                ..Default::default()
            },
        ))
        .await?;
        Ok(())
    }

    /// A pull request is an issue to the labels API. Adding posts a list and leaves what was
    /// already there; taking off is one delete per label, since the endpoint names one in its
    /// path. The name goes into the path encoded, because a label may carry a space or a slash.
    pub async fn set_labels(&self, target: &Target, labels: &[&str], applied: bool) -> Read<()> {
        let (owner, name) = github::parse_repository_selector(&target.repository);
        let issue = format!("repos/{owner}/{name}/issues/{}/labels", target.number);
        if applied {
            Box::pin(api::rest(self.host(), RestInput { host: target.host.clone(), operation: "setLabels".into(), method: Method::Post, path: issue, body: Some(build_label_request(labels)), ..Default::default() })).await?;
            return Ok(());
        }
        for label in labels {
            Box::pin(api::rest(
                self.host(),
                RestInput { host: target.host.clone(), operation: "setLabels".into(), method: Method::Delete, path: format!("{issue}/{}", encode_uri_component(label)), ..Default::default() },
            ))
            .await?;
        }
        Ok(())
    }
}

/// `encodeURIComponent`: everything but letters, digits and `-_.!~*'()` is percent-encoded.
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Workflow approvals on the checks
// ---------------------------------------------------------------------------------------------

/// The runs a fork pull request is waiting on, folded into its checks as ones that need a
/// maintainer; and, where GitHub could not say, a check that says so (`withWorkflowApprovals`).
pub fn with_workflow_approvals(checks: &[PullRequestCheck], runs: &[WorkflowRun], unavailable: bool) -> Vec<PullRequestCheck> {
    static RUN_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = RUN_ID.get_or_init(|| regex::Regex::new(r"/actions/runs/(\d+)(?:/|$)").unwrap());
    let represented: BTreeSet<u64> = checks
        .iter()
        .filter(|check| check.status == PullRequestCheckStatus::ActionRequired)
        .filter_map(|check| check.url.as_deref())
        .filter_map(|url| pattern.captures(url).and_then(|found| found.get(1)).and_then(|id| id.as_str().parse().ok()))
        .collect();
    let mut all = checks.to_vec();
    all.extend(runs.iter().filter(|run| !represented.contains(&run.id)).map(|run| PullRequestCheck {
        name: run.name.clone(),
        status: PullRequestCheckStatus::ActionRequired,
        description: Some("A maintainer must approve this workflow before it can run.".into()),
        url: run.url.clone(),
        required: None,
    }));
    if unavailable {
        all.push(PullRequestCheck {
            name: "Workflow approval status".into(),
            status: PullRequestCheckStatus::ActionRequired,
            description: Some("GitHub could not determine whether workflows are awaiting approval.".into()),
            url: None,
            required: None,
        });
    }
    all
}

/// The pull request's checks with the approvals a fork is waiting on (`readChecks`): open fork
/// pull requests only, and a read that could not be made is reported as such rather than as
/// nothing waiting. A rate limit still fails the read, because every later request would too.
/// `list` reads the runs for a head (sha, branch, owner).
pub async fn workflow_approvals_with<'f, F>(detail: &crate::pull_request_decode::GitHubDetail, list: F) -> Read<(Vec<PullRequestCheck>, Option<u64>)>
where
    F: FnOnce(String, String, String) -> BoxFuture<'f, Read<Vec<WorkflowRun>>>,
{
    let (runs, unavailable) = if detail.item.state != PullRequestState::Open || detail.is_cross_repository != Some(true) {
        (vec![], false)
    } else {
        match (&detail.head_sha, &detail.head_repository_owner) {
            (Some(sha), Some(owner)) => match list(sha.clone(), detail.item.head_branch.clone(), owner.clone()).await {
                Ok(runs) => (runs, false),
                Err(failure) if failure.reason == Reason::RateLimited => return Err(failure),
                Err(_) => (vec![], true),
            },
            _ => (vec![], true),
        }
    };
    let required = (!unavailable).then_some(runs.len() as u64);
    Ok((with_workflow_approvals(&detail.checks, &runs, unavailable), required))
}

pub async fn read_workflow_approvals(gh: &GitHub, target: &Target, core: &crate::pull_request_decode::GitHubCore) -> Read<(Vec<PullRequestCheck>, Option<u64>)> {
    workflow_approvals_with(&core.detail, |sha, branch, owner| Box::pin(async move { gh.list_workflow_runs_requiring_approval(target, &sha, &branch, &owner).await })).await
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

impl PullRequestService {
    /// What the signed-in account may do with the pull request, read fresh. `include_update_branch`
    /// asks for the heavier read that also says whether the branch can be updated; when that read
    /// fails (other than by a rate limit) the lighter one still answers, withholding only
    /// update-branch (`getViewerPermissions`).
    pub(crate) async fn viewer_permissions_for(&self, env: &Env, resolved: &Resolved, operation: &str, include_update_branch: bool) -> PrResult<PullRequestViewerPermissions> {
        let provider = self.provider_of(env, resolved)?;
        provider
            .viewer_permissions(&resolved.subject(), include_update_branch)
            .await
            .map_err(|failure| PrError::from_failure(operation, failure, resolved.project.kind))
    }

    fn fetcher<'a>(&'a self, env: &'a Env, resolved: &'a Resolved, operation: &'a str) -> ViewerFetch<'a> {
        Box::new(move |include| Box::pin(api::with_allow_reserve(self.viewer_permissions_for(env, resolved, operation, include))))
    }

    /// `runAction`: the gates, then the action as an interactive caller, then everything held for
    /// the pull request is stranded. A merge is confirmed afterwards, because a successful merge
    /// request can merely have queued the pull request or armed auto-merge.
    pub async fn run_action(&self, env: &Env, input: &PullRequestActionInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        let capabilities = provider.capabilities();
        Box::pin(authorize_action(input, &capabilities, self.fetcher(env, &resolved, "runAction"))).await?;
        // The gates above stop a stack; nothing past them knows how to run one.
        if input.stack_number.is_some() {
            return Err(PrError::operation("runAction", "This stack action is not supported or has no expected head revision."));
        }
        let settings = env.host.settings().await;
        let run = RunAction {
            action: input.action,
            merge_method: input.merge_method,
            update_method: input.update_method,
            remove_agent_credits: resolved.project.kind == SourceControlProviderKind::Github
                && removes_agent_credits(settings.as_ref(), &input.reference.project_id, input.action, false),
        };
        let kind = resolved.project.kind;
        let result = api::with_allow_reserve(Box::pin(provider.run_action(&resolved.subject(), &run))).await;
        if let Err(failure) = result {
            // Once the action has started a failure may leave partial remote updates, so what is
            // held for the pull request is dropped on this path too.
            self.invalidate_reference(&resolved);
            return Err(PrError::from_failure("runAction", failure, kind));
        }
        self.after_mutation(&resolved).await;
        if input.action == PullRequestAction::Merge {
            self.confirm_merge(env, resolved).await;
        }
        Ok(())
    }

    pub async fn reviewer_candidates(&self, env: &Env, reference: &PullRequestRef) -> PrResult<PullRequestReviewerCandidateList> {
        let resolved = self.resolve(env, reference, None).await?;
        let provider = self.provider_of(env, &resolved)?;
        Box::pin(authorize_reviewer_candidates(&provider.capabilities(), self.fetcher(env, &resolved, "reviewerCandidates"))).await?;
        api::with_allow_reserve(Box::pin(provider.reviewer_candidates(&resolved.subject())))
            .await
            .map_err(|failure| PrError::from_failure("reviewerCandidates", failure, resolved.project.kind))
    }

    pub async fn request_reviewers(&self, env: &Env, input: &PullRequestReviewerRequestInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        Box::pin(authorize_request_reviewers(&provider.capabilities(), self.fetcher(env, &resolved, "requestReviewers"))).await?;
        self.written(&resolved, "requestReviewers", provider.set_reviewer_request(&resolved.subject(), &input.reviewers, input.requested)).await
    }

    pub async fn label_candidates(&self, env: &Env, reference: &PullRequestRef) -> PrResult<PullRequestLabelCandidateList> {
        let resolved = self.resolve(env, reference, None).await?;
        let provider = self.provider_of(env, &resolved)?;
        Box::pin(authorize_labels("labelCandidates", &provider.capabilities(), self.fetcher(env, &resolved, "labelCandidates"))).await?;
        api::with_allow_reserve(Box::pin(provider.label_candidates(&resolved.subject())))
            .await
            .map_err(|failure| PrError::from_failure("labelCandidates", failure, resolved.project.kind))
    }

    pub async fn set_labels(&self, env: &Env, input: &PullRequestLabelChangeInput, namespace: Option<String>) -> PrResult<()> {
        let resolved = self.resolve(env, &input.reference, namespace).await?;
        let provider = self.provider_of(env, &resolved)?;
        Box::pin(authorize_labels("setLabels", &provider.capabilities(), self.fetcher(env, &resolved, "setLabels"))).await?;
        let labels: Vec<&str> = input.labels.iter().map(|label| label.as_str()).collect();
        self.written(&resolved, "setLabels", provider.set_labels(&resolved.subject(), &labels, input.applied)).await
    }

    /// After a merge request was accepted, asks the host whether the pull request really is
    /// merged now (it may only have been queued) and, if so, tells whoever listens.
    async fn confirm_merge(&self, env: &Env, resolved: Resolved) {
        let project_id = resolved.project.project.id.clone();
        let (repository, number) = (resolved.project.repository.clone(), resolved.number);
        let confirmed = Box::pin(self.summary_uncached(env.clone(), resolved)).await;
        match confirmed {
            Ok(summary) if summary.state == PullRequestState::Merged => {
                self.publish_merge(PullRequestMergeEvent { project_id, repository, number, merged_at: self.clock.utc_now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true) });
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(error = %error.wire(), "failed to confirm pull request merge"),
        }
    }
}

/// A merge the host confirmed (`PullRequestMergeEvent`), for whatever settles the threads that
/// were working on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestMergeEvent {
    pub project_id: ProjectId,
    pub repository: String,
    pub number: u64,
    pub merged_at: String,
}
