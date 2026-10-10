//! What a host must implement for the pull request service to read and change its change
//! requests (port of `PullRequestProvider.ts`). Implementations own their tool and JSON shapes and
//! hand back the neutral types below; anything a host cannot do is declared in `capabilities`
//! rather than failing at call time. The service keeps caching, shared in-flight reads, epoch
//! invalidation and the refresh subscription above whichever provider answers.
//!
//! Every method returns a boxed future: the trait is used as `dyn`, and the box keeps each
//! provider's (large) state machine off the caller's stack.
use crate::{
    pull_request_actions::RunAction,
    pull_request_github::{PrFailure, Reason, Read},
};
use futures_util::future::BoxFuture;
use serde::Serialize;
use t3_contracts::*;

/// A change request addressed on its host: the checkout whose tool and credentials serve it, the
/// host (which `repository` deliberately leaves out), the repository and the number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub cwd: String,
    pub host: String,
    pub repository: String,
    pub number: u64,
}

/// A change request as a provider sees it, before the service attaches project context
/// (`ProviderChangeRequest`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderChangeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<PullRequestStackMembership>,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_repository_name_with_owner: Option<Option<String>>,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub additions: u64,
    pub deletions: u64,
    pub created_at: String,
    pub updated_at: String,
    /// Accounts with a review requested; team-level requests are excluded by each provider.
    pub review_request_logins: Vec<String>,
    pub labels: Vec<PullRequestLabel>,
    /// Outer `None`: a host that does not summarise its reviews. Inner `None`: no decision yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<Option<PullRequestReviewDecision>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks_state: Option<Option<PullRequestChecksState>>,
}

/// One repository's row inside an answer that spans several of them.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchedProviderChangeRequest {
    pub row: ProviderChangeRequest,
    /// Provider-native identity, exactly as it was asked for.
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeRequestPage {
    pub items: Vec<ProviderChangeRequest>,
    /// The host has more rows than the page size asked for.
    pub truncated: bool,
    /// A count-based cursor advance, for an offset-paged host that consumed malformed rows too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_advance: Option<usize>,
    /// The page can be carried on from; false where the order a cursor means nothing in.
    pub continues: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchedPage {
    pub items: Vec<BatchedProviderChangeRequest>,
    pub truncated: bool,
}

/// Where a repository's next slice starts, as the provider that has to ask for it needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCursor {
    /// The instant of the oldest row already handed over, asked for inclusively.
    pub updated_before: String,
    /// How many provider rows this repository has consumed, for a host that counts.
    pub delivered: u32,
}

/// The repositories one listing reads, and how.
#[derive(Debug, Clone)]
pub struct ListRequest {
    pub cwd: String,
    pub host: String,
    pub repositories: Vec<String>,
    pub state: PullRequestListState,
    pub involvement: PullRequestInvolvement,
    pub viewer: String,
    pub limit: u32,
    pub query: Option<String>,
    pub cursor: Option<ProviderCursor>,
    pub filters: Option<PullRequestListFilters>,
}

/// The fields that keep a linked thread's status live (`ProviderChangeRequestSummary`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeRequestSummary {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: Option<bool>,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub updated_at: String,
    pub author: Option<Option<PullRequestActor>>,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    pub changed_files: Option<u64>,
    pub review_decision: Option<Option<PullRequestReviewDecision>>,
    pub checks_state: Option<Option<PullRequestChecksState>>,
    pub mergeability: Option<PullRequestMergeability>,
    pub stack: Option<Option<PullRequestStackMembership>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeRequestDetail {
    #[serde(flatten)]
    pub row: ProviderChangeRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub body: String,
    pub changed_files: u64,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub reviewers: Vec<PullRequestActor>,
    pub checks: Vec<PullRequestCheck>,
    pub merge_capabilities: PullRequestMergeCapabilities,
    pub viewer_permissions: PullRequestViewerPermissions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_comparison: Option<PullRequestBaseComparison>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind_by: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_method: Option<PullRequestMergeMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_approvals_required: Option<u64>,
}

/// The card a hover shows; the service adds the project and repository.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeRequestPreview {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub created_at: String,
}

/// The line counts for a row a listing left for a second read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequestStat {
    pub repository: String,
    pub number: u64,
    pub additions: u64,
    pub deletions: u64,
}

/// What version the head has of each asked-for file: opaque strings compared only against one
/// another, with the empty string for a file the change request deletes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileRevisions {
    pub revisions: std::collections::BTreeMap<String, String>,
    /// Every file the change request carries rather than only the paths asked about.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub complete: bool,
}

pub fn unsupported(operation: &str) -> PrFailure {
    PrFailure::failed(format!("This host does not support {operation}."))
}

/// A refusal or a missing capability that is the request's own: not the host being down.
pub fn refused(detail: &str) -> PrFailure {
    PrFailure { reason: Reason::Failed, detail: detail.to_string(), retry_at: None }
}

pub trait PullRequestProvider: Send + Sync {
    fn kind(&self) -> SourceControlProviderKind;
    fn capabilities(&self) -> PullRequestCapabilities;

    /// The signed-in account, which involvement filtering compares against.
    fn viewer<'a>(&'a self, cwd: &'a str, host: &'a str) -> BoxFuture<'a, Read<String>>;

    /// One repository's rows; every host reads this.
    fn list<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<ChangeRequestPage>>;

    /// Whether the host has a search across repositories (`listChangeRequestsAcross` is present).
    fn lists_across(&self) -> bool {
        false
    }
    fn list_across<'a>(&'a self, _request: &'a ListRequest) -> BoxFuture<'a, Read<BatchedPage>> {
        Box::pin(async { Err(unsupported("a search across repositories")) })
    }

    /// Line counts for rows a listing handed over, or `None` where the listing carries them.
    fn lists_stats(&self) -> bool {
        false
    }
    fn list_stats<'a>(&'a self, _cwd: &'a str, _host: &'a str, _changes: &'a [(String, u64)]) -> BoxFuture<'a, Read<Vec<ChangeRequestStat>>> {
        Box::pin(async { Ok(vec![]) })
    }

    /// Whether the host has a checks read (`getChangeRequestChecks` is present). A host without one
    /// answers the checks call with null, and `checks` is never called.
    fn has_checks(&self) -> bool {
        true
    }
    fn checks<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestChecks>>;
    fn detail<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestDetail>>;

    /// Hosts without a narrow read use their detail response.
    fn preview<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestPreview>> {
        Box::pin(async move {
            let detail = self.detail(subject).await?;
            Ok(ChangeRequestPreview {
                number: detail.row.number,
                title: detail.row.title,
                url: detail.row.url,
                author: detail.row.author,
                state: detail.row.state,
                is_draft: detail.row.is_draft,
                created_at: detail.row.created_at,
            })
        })
    }

    /// The cheap live fields; a host without a narrow endpoint answers from its detail.
    fn summary<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestSummary>> {
        Box::pin(async move {
            let detail = self.detail(subject).await?;
            let row = detail.row;
            Ok(ChangeRequestSummary {
                number: row.number,
                title: row.title,
                url: row.url,
                head_branch: row.head_branch,
                base_branch: row.base_branch,
                state: row.state,
                is_draft: Some(row.is_draft),
                closed_at: detail.closed_at,
                merged_at: detail.merged_at,
                updated_at: row.updated_at,
                author: Some(row.author),
                additions: Some(row.additions),
                deletions: Some(row.deletions),
                changed_files: Some(detail.changed_files),
                review_decision: row.review_decision,
                checks_state: row.checks_state,
                mergeability: Some(row.mergeability),
                stack: row.stack.map(Some),
            })
        })
    }

    /// The host-native stack, or none where the host has no such object.
    fn stack<'a>(&'a self, _subject: &'a Subject, _include_details: bool) -> BoxFuture<'a, Read<Option<PullRequestStack>>> {
        Box::pin(async { Ok(None) })
    }

    /// Comments, line threads and commits, kept off the critical path of the detail.
    fn activity<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestActivity>>;

    fn thread_comments<'a>(&'a self, _subject: &'a Subject, _thread_id: &'a str, _cursor: &'a str) -> BoxFuture<'a, Read<PullRequestThreadCommentsResult>> {
        Box::pin(async { Err(refused("This host does not page review thread comments.")) })
    }

    /// The viewer's standing, read fresh and as cheaply as the host allows.
    fn viewer_permissions<'a>(&'a self, subject: &'a Subject, include_update_branch: bool) -> BoxFuture<'a, Read<PullRequestViewerPermissions>>;

    fn diff<'a>(&'a self, subject: &'a Subject, cursor: Option<&'a str>, commit: Option<&'a str>) -> BoxFuture<'a, Read<PullRequestDiffResult>>;
    fn diff_file_contents<'a>(
        &'a self,
        _subject: &'a Subject,
        _commit: Option<&'a str>,
        _change_type: PullRequestDiffChangeType,
        _old_path: &'a str,
        _new_path: &'a str,
    ) -> BoxFuture<'a, Read<PullRequestDiffFileContentsResult>> {
        Box::pin(async { Err(refused("This host cannot expand unchanged pull request lines.")) })
    }

    /// Only called when `capabilities.viewed_files` is `Host`.
    fn files_viewed<'a>(&'a self, _subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestFilesViewedResult>> {
        Box::pin(async { Err(unsupported("viewed files")) })
    }
    fn set_files_viewed<'a>(&'a self, _subject: &'a Subject, _files: &'a [PullRequestSetFileViewed]) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(unsupported("viewed files")) })
    }
    /// Required of a host whose `viewed_files` is `Environment`.
    fn file_revisions<'a>(&'a self, _subject: &'a Subject, _paths: &'a [String]) -> BoxFuture<'a, Read<FileRevisions>> {
        Box::pin(async { Err(unsupported("file revisions")) })
    }

    fn run_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction) -> BoxFuture<'a, Read<()>>;
    fn update<'a>(&'a self, _subject: &'a Subject, _title: Option<&'a str>, _body: Option<&'a str>) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(unsupported("editing a change request")) })
    }
    fn comment<'a>(&'a self, subject: &'a Subject, body: &'a str) -> BoxFuture<'a, Read<()>>;
    fn update_comment<'a>(&'a self, _subject: &'a Subject, _comment_id: &'a str, _kind: PullRequestCommentUpdateKind, _body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(unsupported("editing a comment")) })
    }
    fn submit_review<'a>(&'a self, subject: &'a Subject, verdict: PullRequestReviewVerdict, body: &'a str, comments: &'a [PullRequestReviewCommentDraft]) -> BoxFuture<'a, Read<()>>;
    fn reviewer_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestReviewerCandidateList>>;
    fn set_reviewer_request<'a>(&'a self, subject: &'a Subject, reviewers: &'a [PullRequestReviewerRequestTarget], requested: bool) -> BoxFuture<'a, Read<()>>;
    fn label_candidates<'a>(&'a self, _subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestLabelCandidateList>> {
        Box::pin(async { Err(unsupported("labels")) })
    }
    fn set_labels<'a>(&'a self, _subject: &'a Subject, _labels: &'a [&'a str], _applied: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(unsupported("labels")) })
    }
    fn reply_to_thread<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, body: &'a str) -> BoxFuture<'a, Read<()>>;
    fn set_reaction<'a>(&'a self, subject: &'a Subject, subject_id: Option<&'a str>, content: PullRequestReactionContent, reacted: bool) -> BoxFuture<'a, Read<()>>;
    fn set_thread_resolution<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, resolved: bool) -> BoxFuture<'a, Read<()>>;
}
