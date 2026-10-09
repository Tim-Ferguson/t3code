//! `packages/contracts/src/pullRequest.ts`: the wire shapes of the `pullRequests.*` methods and
//! their two errors. Optional fields are skipped when absent; a field the original types
//! `optional(NullOr(..))` is an `Option<Option<..>>`, where `Some(None)` writes `null`.
use crate::base::deserialize_optional;
use crate::{ProjectId, PullRequestActor, PullRequestChecksState, PullRequestMergeMethod, PullRequestMergeability, PullRequestReviewDecision, PullRequestState, SourceControlProviderKind, ThreadId, TrimmedNonEmptyString};
use serde::{Deserialize, Serialize};

/// Reads `optional(NullOr(T))`: a present key keeps its null.
pub fn deserialize_optional_nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

macro_rules! kebab_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum $name { $($variant),+ }
    };
}

kebab_enum!(PullRequestInvolvement { All, Reviewing, Authored });
kebab_enum!(PullRequestListState { All, Open, Closed, Merged });
kebab_enum!(PullRequestAction {
    Merge, Ready, Draft, Close, Reopen, UpdateBranch, EnableAutoMerge, DisableAutoMerge, Revert, ApproveWorkflows
});
kebab_enum!(PullRequestUpdateMethod { Merge, Rebase });
kebab_enum!(PullRequestBaseComparison { UpToDate, Behind, Unknown });
kebab_enum!(PullRequestCheckStatus {
    Pending, ActionRequired, Success, Failure, Skipped, Neutral, Cancelled
});
kebab_enum!(PullRequestReactionContent {
    ThumbsUp, ThumbsDown, Laugh, Hooray, Confused, Heart, Rocket, Eyes
});
kebab_enum!(PullRequestCommentKind { IssueComment, ReviewComment, Review });
kebab_enum!(PullRequestDiffSide { Left, Right });
kebab_enum!(PullRequestReviewVerdict { Comment, Approve, RequestChanges });
kebab_enum!(PullRequestReviewerKind { User, Team });
kebab_enum!(PullRequestViewedFilesStore { Host, Environment });
kebab_enum!(PullRequestFileViewedState { Unviewed, Viewed, Dismissed });
kebab_enum!(PullRequestCommentUpdateKind { IssueComment, ReviewComment });
kebab_enum!(PullRequestDiffChangeType { Change, RenamePure, RenameChanged, New, Deleted });
kebab_enum!(PullRequestUnavailableReason { CliMissing, CliUnauthenticated, ProviderUnsupported });

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestListFilters {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub draft: Option<PullRequestDraftFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub review: Option<PullRequestReviewFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub checks: Option<PullRequestChecksFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub labels: Option<Vec<Vec<TrimmedNonEmptyString>>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub excluded_labels: Option<Vec<TrimmedNonEmptyString>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub author: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullRequestDraftFilter {
    Only,
    Hide,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullRequestReviewFilter {
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullRequestChecksFilter {
    Passing,
    Failing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestLabel {
    pub name: String,
    pub color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestCheck {
    pub name: String,
    pub status: PullRequestCheckStatus,
    pub description: Option<String>,
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestReaction {
    pub content: PullRequestReactionContent,
    pub count: u64,
    pub actors: Vec<String>,
    pub viewer_has_reacted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestComment {
    pub id: String,
    pub kind: PullRequestCommentKind,
    pub author: Option<PullRequestActor>,
    pub body: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub edited_at: Option<Option<String>>,
    pub url: Option<String>,
    pub path: Option<String>,
    pub review_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<Vec<PullRequestReaction>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestThreadComment {
    pub id: String,
    pub author: Option<PullRequestActor>,
    pub body: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub edited_at: Option<Option<String>>,
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<Vec<PullRequestReaction>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestReviewThread {
    pub id: String,
    pub path: String,
    pub line: Option<u64>,
    pub side: PullRequestDiffSide,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<PullRequestThreadComment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_comments_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestReviewerCandidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_bot: Option<bool>,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub id: String,
    pub kind: PullRequestReviewerKind,
    pub is_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestReviewerCandidateList {
    pub candidates: Vec<PullRequestReviewerCandidate>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestLabelCandidate {
    pub name: String,
    pub color: Option<String>,
    pub description: Option<String>,
    pub is_applied: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestLabelCandidateList {
    pub candidates: Vec<PullRequestLabelCandidate>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestCommit {
    pub oid: String,
    pub message_headline: String,
    pub committed_date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<PullRequestActor>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestReviewCapabilities {
    #[serde(rename = "inlineComment")]
    pub inline_comment: bool,
    pub reply: bool,
    pub resolve: bool,
    pub verdicts: Vec<PullRequestReviewVerdict>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestEditCapabilities {
    pub change_request: bool,
    pub comment: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestReviewerCapabilities {
    pub request: bool,
    pub list_candidates: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestCapabilities {
    pub diff: bool,
    pub comment: bool,
    pub actions: Vec<PullRequestAction>,
    pub merge_methods: Vec<PullRequestMergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_methods: Option<Vec<PullRequestUpdateMethod>>,
    pub search: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewed_files: Option<PullRequestViewedFilesStore>,
    pub review: PullRequestReviewCapabilities,
    pub reviewers: PullRequestReviewerCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<PullRequestEditCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_actions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestViewerPermissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_rebase: Option<bool>,
    pub actions: Vec<PullRequestAction>,
    pub comment: bool,
    pub resolve: bool,
    pub verdicts: Vec<PullRequestReviewVerdict>,
    pub request_reviewers: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_methods: Option<Vec<PullRequestUpdateMethod>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestMergeCapabilities {
    pub merge: bool,
    pub squash: bool,
    pub rebase: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestStackMembership {
    pub number: u64,
    pub position: u64,
    pub size: u64,
    pub base: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestListEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<PullRequestStackMembership>,
    pub provider: SourceControlProviderKind,
    pub host: String,
    pub project_id: ProjectId,
    pub project_title: String,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub additions: u64,
    pub deletions: u64,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    pub viewer_review_requested: bool,
    pub labels: Vec<PullRequestLabel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<PullRequestReviewDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_state: Option<PullRequestChecksState>,
}

pub type PullRequestListCursors = std::collections::BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestListInput {
    pub state: PullRequestListState,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub involvement: Option<PullRequestInvolvement>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub filters: Option<PullRequestListFilters>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub project_ids: Option<Vec<ProjectId>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub host: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub cursors: Option<PullRequestListCursors>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub query: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestProviderSummary {
    pub host: String,
    pub kind: SourceControlProviderKind,
    pub searches_on_host: bool,
    pub project_count: u64,
    pub configured: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestListProjectError {
    pub project_id: ProjectId,
    pub project_title: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestListResult {
    pub viewers: std::collections::BTreeMap<String, String>,
    pub providers: Vec<PullRequestProviderSummary>,
    pub entries: Vec<PullRequestListEntry>,
    pub errors: Vec<PullRequestListProjectError>,
    pub truncated: bool,
    pub next_cursors: PullRequestListCursors,
}

/// Addresses one pull request for reads and writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestRef {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub host: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub expected_account_id: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub allow_stale: Option<bool>,
    pub repository: TrimmedNonEmptyString,
    pub number: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestRoutingIdentityInput {
    pub host: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestRoutingIdentityResult {
    pub account_id: String,
    pub host: String,
    pub provider: SourceControlProviderKind,
    pub viewer: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestRoutingResult {
    pub account_id: String,
    pub host: String,
    pub provider: SourceControlProviderKind,
    pub viewer: String,
    pub project_title: String,
    pub workspace_root: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestLinkedThread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestLinkedThreadsResult {
    pub threads: Vec<PullRequestLinkedThread>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestPreview {
    pub project_id: ProjectId,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestSummary {
    pub provider: SourceControlProviderKind,
    pub project_id: ProjectId,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PullRequestState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_draft: Option<bool>,
    pub head_branch: String,
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub closed_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub merged_at: Option<Option<String>>,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub author: Option<Option<PullRequestActor>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub review_decision: Option<Option<PullRequestReviewDecision>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub checks_state: Option<Option<PullRequestChecksState>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mergeability: Option<PullRequestMergeability>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub stack: Option<Option<PullRequestStackMembership>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestStackLayer {
    pub number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub head_branch: String,
    pub state: PullRequestState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestStack {
    pub id: String,
    pub number: u64,
    pub url: String,
    pub base: String,
    pub layers: Vec<PullRequestStackLayer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDiffStat {
    pub project_id: ProjectId,
    pub repository: String,
    pub number: u64,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestListStatsInput {
    pub refs: Vec<PullRequestRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestListStatsResult {
    pub stats: Vec<PullRequestDiffStat>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestInvalidateInput {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub reference: Option<PullRequestRef>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub files_viewed_only: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDetail {
    pub provider: SourceControlProviderKind,
    pub capabilities: PullRequestCapabilities,
    pub viewer_permissions: PullRequestViewerPermissions,
    pub project_id: ProjectId,
    pub project_title: String,
    pub workspace_root: String,
    pub repository: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub head_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub head_repository_name_with_owner: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub base_branch: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub reviewers: Vec<PullRequestActor>,
    pub labels: Vec<PullRequestLabel>,
    pub checks: Vec<PullRequestCheck>,
    pub merge_capabilities: PullRequestMergeCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_comparison: Option<PullRequestBaseComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind_by: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_merge_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_merge_method: Option<PullRequestMergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_approvals_required: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestChecks {
    pub state: PullRequestState,
    pub checks: Vec<PullRequestCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestActivity {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional_nullable")]
    pub author: Option<Option<PullRequestActor>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewers: Option<Vec<PullRequestActor>>,
    pub comments: Vec<PullRequestComment>,
    pub comment_count: u64,
    pub comments_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_threads_truncated: Option<bool>,
    pub review_threads: Vec<PullRequestReviewThread>,
    pub commits: Vec<PullRequestCommit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reactions: Option<Vec<PullRequestReaction>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestOmittedFileStat {
    pub path: String,
    pub additions: i64,
    pub deletions: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDiffInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub cursor: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub commit: Option<TrimmedNonEmptyString>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDiffResult {
    pub patch: String,
    pub truncated: bool,
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_file_stats: Option<Vec<PullRequestOmittedFileStat>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDiffFileContentsInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub commit: Option<TrimmedNonEmptyString>,
    pub change_type: PullRequestDiffChangeType,
    pub old_path: TrimmedNonEmptyString,
    pub new_path: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestDiffFileContentsResult {
    pub old_contents: String,
    pub new_contents: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestFileViewed {
    pub path: String,
    pub state: PullRequestFileViewedState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestFilesViewedResult {
    pub files: Vec<PullRequestFileViewed>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestSetFileViewed {
    pub path: String,
    pub viewed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestSetFilesViewedInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub files: Vec<PullRequestSetFileViewed>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestStackHead {
    pub number: u64,
    pub head_sha: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestActionInput {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub stack_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub expected_stack_heads: Option<Vec<PullRequestStackHead>>,
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub action: PullRequestAction,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub merge_method: Option<PullRequestMergeMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub update_method: Option<PullRequestUpdateMethod>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestCommentInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestUpdateInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub title: Option<TrimmedNonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestCommentUpdateInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub comment_id: TrimmedNonEmptyString,
    pub kind: PullRequestCommentUpdateKind,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PullRequestReviewPosition {
    #[serde(rename = "added", rename_all = "camelCase")]
    Added { new_line: u64 },
    #[serde(rename = "deleted", rename_all = "camelCase")]
    Deleted { old_line: u64 },
    #[serde(rename = "context", rename_all = "camelCase")]
    Context { old_line: u64, new_line: u64, side: PullRequestDiffSide },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestReviewCommentDraft {
    pub path: TrimmedNonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub old_path: Option<TrimmedNonEmptyString>,
    pub position: PullRequestReviewPosition,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestSubmitReviewInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub verdict: PullRequestReviewVerdict,
    pub body: String,
    pub comments: Vec<PullRequestReviewCommentDraft>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestThreadCommentsInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub thread_id: TrimmedNonEmptyString,
    pub cursor: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestThreadCommentsResult {
    pub comments: Vec<PullRequestThreadComment>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestThreadReplyInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub thread_id: TrimmedNonEmptyString,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestThreadResolutionInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub thread_id: TrimmedNonEmptyString,
    pub resolved: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestReactionInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub subject_id: Option<TrimmedNonEmptyString>,
    pub content: PullRequestReactionContent,
    pub reacted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestReviewerRequestTarget {
    pub id: TrimmedNonEmptyString,
    pub kind: PullRequestReviewerKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestReviewerRequestInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub reviewers: Vec<PullRequestReviewerRequestTarget>,
    pub requested: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PullRequestLabelChangeInput {
    #[serde(flatten)]
    pub reference: PullRequestRef,
    pub labels: Vec<TrimmedNonEmptyString>,
    pub applied: bool,
}

/// `PullRequestUnavailableError`: the host cannot be read at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestUnavailableError {
    pub reason: PullRequestUnavailableReason,
    pub provider: Option<SourceControlProviderKind>,
}

const PROVIDER_REQUIREMENT: &[(SourceControlProviderKind, &str, &str)] = &[
    (
        SourceControlProviderKind::Github,
        "No GitHub credential on the server. Set GH_TOKEN, or install the GitHub CLI (https://cli.github.com/) and run `gh auth login`.",
        "GitHub has no working credential for this host. Run `gh auth login`, or check the account and hosts in Settings → Source Control.",
    ),
    (
        SourceControlProviderKind::Forgejo,
        "Install Forgejo CLI (`fj` 0.6 or later) from https://codeberg.org/forgejo-contrib/forgejo-cli or Gitea CLI (`tea` 0.16 or later) from https://gitea.com/gitea/tea to browse Forgejo pull requests.",
        "Authenticate your Forgejo or Gitea server with `fj --host <server-url> auth add-token` on the T3 Code server. If fj is missing or unconfigured for that server, use `tea login add`. A configured fj account must be repaired with fj.",
    ),
    (
        SourceControlProviderKind::Gitlab,
        "GitLab CLI (`glab`) is required to browse change requests on this host. Install it from https://gitlab.com/gitlab-org/cli and reload.",
        "GitLab CLI is not authenticated. Run `glab auth login` and retry.",
    ),
    (
        SourceControlProviderKind::AzureDevops,
        "Azure CLI (`az`) with the Azure DevOps extension is required. Install `az`, then run `az extension add --name azure-devops`.",
        "Azure CLI is not signed in. Run `az login` and retry.",
    ),
    (
        SourceControlProviderKind::Bitbucket,
        "Bitbucket needs API credentials on the server. Add them in Settings → Source Control.",
        "Bitbucket rejected the configured credentials. Check them in Settings → Source Control.",
    ),
];

/// What to do about an unreadable host, or `None` for a kind with no requirement text.
pub fn pull_request_provider_requirement(
    provider: SourceControlProviderKind,
    reason: PullRequestUnavailableReason,
) -> Option<&'static str> {
    let (_, missing, unauthenticated) = PROVIDER_REQUIREMENT.iter().find(|(kind, ..)| *kind == provider)?;
    match reason {
        PullRequestUnavailableReason::CliMissing => Some(missing),
        PullRequestUnavailableReason::CliUnauthenticated => Some(unauthenticated),
        PullRequestUnavailableReason::ProviderUnsupported => None,
    }
}

impl PullRequestUnavailableError {
    pub fn message(&self) -> String {
        let requirement = self
            .provider
            .and_then(|provider| pull_request_provider_requirement(provider, self.reason));
        match self.reason {
            PullRequestUnavailableReason::CliMissing => requirement
                .unwrap_or("The tool this host is read through is not installed or set up.")
                .into(),
            PullRequestUnavailableReason::CliUnauthenticated => {
                requirement.unwrap_or("This host has no working credentials.").into()
            }
            PullRequestUnavailableReason::ProviderUnsupported => {
                "Change requests cannot be browsed for this project's host yet.".into()
            }
        }
    }

    pub fn wire(&self) -> serde_json::Value {
        let mut value = serde_json::json!({"_tag":"PullRequestUnavailableError","reason":self.reason});
        if let Some(provider) = self.provider {
            value["provider"] = serde_json::json!(provider);
        }
        value
    }
}

/// `PullRequestOperationError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestOperationError {
    pub operation: String,
    pub detail: String,
    pub not_found: bool,
}

impl PullRequestOperationError {
    pub fn new(operation: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { operation: operation.into(), detail: detail.into(), not_found: false }
    }

    pub fn message(&self) -> String {
        format!("Pull request operation {} failed: {}", self.operation, self.detail)
    }

    pub fn wire(&self) -> serde_json::Value {
        let mut value = serde_json::json!({"_tag":"PullRequestOperationError","operation":self.operation,"detail":self.detail});
        if self.not_found {
            value["reason"] = serde_json::json!("not-found");
        }
        value
    }
}
