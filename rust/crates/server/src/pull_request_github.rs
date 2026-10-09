//! What GitHub is asked for a pull request, and what its answers become (port of the read half
//! of `GitHubPullRequestCli.ts` and `GitHubPullRequestProvider.ts`). Every request goes through
//! `source_control_github_api`, so credentials, the rate-limit pause and the GraphQL quota
//! ledger apply; nothing here sees a token.
use crate::{
    pull_request_decode::{self as decode, Decoded, GitHubCore, GitHubViewerAccess},
    pull_request_queries as queries,
    source_control_github_api::{self as api, GitHubApiError, GraphQlInput, RestInput},
    source_control_host::Host,
};
use serde_json::{Value, json};
use t3_contracts::{
    PullRequestAction, PullRequestCapabilities, PullRequestDiffChangeType, PullRequestDiffResult,
    PullRequestEditCapabilities, PullRequestFileViewed, PullRequestFilesViewedResult, PullRequestMergeMethod,
    PullRequestReviewCapabilities, PullRequestReviewVerdict, PullRequestReviewerCapabilities,
    PullRequestUpdateMethod, PullRequestViewedFilesStore, PullRequestViewerPermissions,
};

const CHECK_CONTEXT_PAGES: usize = 10;
const REVIEW_THREAD_PAGES: usize = 10;
const FILES_VIEWED_MAX_PAGES: usize = 5;
const DIFF_FILES_PAGE_SIZE: usize = 100;
const DIFF_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const DIFF_FILE_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
pub const STAT_ALIASES_PER_REQUEST: usize = 25;
pub const SEARCH_MAX_ROWS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    MissingTool,
    Unauthenticated,
    RateLimited,
    NotFound,
    Failed,
}

/// `PullRequestProviderError`: why a host read or write did not happen, with text that is safe
/// to show (no credential ever reaches it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFailure {
    pub reason: Reason,
    pub detail: String,
    pub retry_at: Option<i64>,
}

impl PrFailure {
    pub fn failed(detail: impl Into<String>) -> Self {
        Self { reason: Reason::Failed, detail: detail.into(), retry_at: None }
    }
    pub fn unreadable(operation: &str) -> Self {
        Self::failed(format!("GitHub returned an unreadable {operation} response."))
    }
}

impl From<GitHubApiError> for PrFailure {
    fn from(error: GitHubApiError) -> Self {
        let (reason, retry_at) = match &error {
            GitHubApiError::Credential(credential) => match credential.tag() {
                "GitHubCliMissingError" => (Reason::MissingTool, None),
                "GitHubNotSignedInError" | "GitHubHostDisabledError" => (Reason::Unauthenticated, None),
                _ => (Reason::Failed, None),
            },
            GitHubApiError::Authentication { .. } => (Reason::Unauthenticated, None),
            GitHubApiError::RateLimit { retry_at, .. } => (Reason::RateLimited, *retry_at),
            GitHubApiError::Paused(paused) => (Reason::RateLimited, Some(paused.retry_at)),
            GitHubApiError::NotFound { .. } => (Reason::NotFound, None),
            GitHubApiError::Request { .. } | GitHubApiError::Response { .. } => (Reason::Failed, None),
        };
        Self { reason, detail: error.message(), retry_at }
    }
}

pub type Read<T> = Result<T, PrFailure>;

/// `owner/name` as the last two segments, the way the original reads a selector.
pub fn parse_repository_selector(value: &str) -> (String, String) {
    let parts: Vec<&str> = value.trim().split('/').filter(|part| !part.is_empty()).collect();
    let name = parts.last().copied().unwrap_or("").to_string();
    let owner = if parts.len() >= 2 { parts[parts.len() - 2] } else { "" }.to_string();
    (owner, name)
}

fn is_commit_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn diff_cursor_page(cursor: &str) -> Option<u32> {
    let valid = (1..=7).contains(&cursor.len())
        && cursor.bytes().all(|byte| byte.is_ascii_digit())
        && !cursor.starts_with('0');
    valid.then(|| cursor.parse().ok()).flatten()
}

fn is_github_com(host: &str) -> bool {
    host.eq_ignore_ascii_case("github.com")
}

// ---------------------------------------------------------------------------------------------
// Capabilities and what the viewer may do
// ---------------------------------------------------------------------------------------------

pub fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        comment: true,
        actions: vec![
            PullRequestAction::Merge,
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
            PullRequestAction::UpdateBranch,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
            PullRequestAction::Revert,
            PullRequestAction::ApproveWorkflows,
        ],
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        update_methods: Some(vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase]),
        search: true,
        reactions: Some(true),
        viewed_files: Some(PullRequestViewedFilesStore::Host),
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: true,
            resolve: true,
            verdicts: vec![PullRequestReviewVerdict::Comment, PullRequestReviewVerdict::Approve, PullRequestReviewVerdict::RequestChanges],
        },
        reviewers: PullRequestReviewerCapabilities { request: true, list_candidates: true },
        edit: Some(PullRequestEditCapabilities { change_request: true, comment: true }),
        stacks: Some(true),
        stack_actions: None,
        labels: Some(true),
    }
}

/// What the signed-in account may do here, from the three things GitHub says about it
/// (`gitHubViewerPermissions`). Commenting and reviewing are not gated; merging needs a role
/// that can push; the author may close, reopen and move in and out of draft.
pub fn viewer_permissions(access: &GitHubViewerAccess) -> PullRequestViewerPermissions {
    let mut actions = vec![];
    if access.can_write {
        actions.extend([
            PullRequestAction::Merge,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
            PullRequestAction::Revert,
            PullRequestAction::ApproveWorkflows,
        ]);
    }
    if access.can_update {
        actions.extend([
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
        ]);
    }
    if access.can_update_branch == Some(true) {
        actions.push(PullRequestAction::UpdateBranch);
    }
    PullRequestViewerPermissions {
        stack_rebase: access.can_write.then_some(true),
        actions,
        comment: true,
        resolve: access.can_write || access.did_author,
        verdicts: if access.did_author {
            vec![PullRequestReviewVerdict::Comment]
        } else {
            capabilities().review.verdicts
        },
        request_reviewers: access.can_write,
        update_methods: (access.can_update_branch == Some(true))
            .then(|| vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase]),
        labels: Some(access.can_triage),
    }
}

/// A login's picture where GitHub gave none (`loginAvatarUrl`): only for a plain user login.
pub fn login_avatar_url(login: &str, host: &str) -> Option<String> {
    let bytes = login.as_bytes();
    let plain = (1..=39).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-');
    plain.then(|| format!("https://{host}/{login}.png?size=80"))
}

// ---------------------------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------------------------

/// One GitHub host's pull request API, bound to the process host (environment, clock, caches).
#[derive(Clone)]
pub struct GitHub {
    host: Host,
}

/// What a pull request read is addressed by.
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub repository: String,
    pub number: u64,
}

impl Target {
    fn selector(&self) -> (String, String) {
        parse_repository_selector(&self.repository)
    }
}

pub struct Core {
    pub core: GitHubCore,
}

impl GitHub {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    async fn graphql<T>(
        &self,
        gh_host: &str,
        operation: &str,
        query: &str,
        variables: Value,
        decoder: impl FnOnce(&str) -> Decoded<T>,
    ) -> Read<T> {
        let raw = Box::pin(api::graphql(
            &self.host,
            GraphQlInput {
                host: gh_host.to_string(),
                operation: operation.to_string(),
                query: query.to_string(),
                variables: Some(variables),
                ..Default::default()
            },
        ))
        .await?;
        decoder(raw.trim()).map_err(|_| PrFailure::unreadable(operation))
    }

    async fn rest(&self, input: RestInput) -> Read<api::RestResponse> {
        Ok(Box::pin(api::rest(&self.host, input)).await?)
    }

    /// The signed-in account (`GET user`): its numeric id is the account id routing compares.
    pub async fn routing_identity(&self, gh_host: &str) -> Read<(String, String)> {
        let response = self
            .rest(RestInput {
                host: gh_host.to_string(),
                operation: "getRoutingIdentity".into(),
                path: "user".into(),
                allow_reserve: Some(true),
                ..Default::default()
            })
            .await?;
        let value: Value = serde_json::from_str(response.body.trim()).map_err(|_| PrFailure::failed("GitHub returned no login for the authenticated account."))?;
        let id = value.get("id").and_then(Value::as_u64).filter(|id| *id > 0);
        let login = value.get("login").and_then(Value::as_str).map(|login| login.trim().to_string()).filter(|login| !login.is_empty());
        match (id, login) {
            (Some(id), Some(login)) => Ok((id.to_string(), login)),
            _ => Err(PrFailure::failed("GitHub returned no login for the authenticated account.")),
        }
    }

    pub async fn viewer_login(&self, gh_host: &str) -> Read<String> {
        self.routing_identity(gh_host).await.map(|(_, login)| login)
    }

    /// The core read, walking the rest of the check contexts when the first hundred are not all
    /// of them. Each page names the head it read, so a push mid-walk fails rather than mixing
    /// two revisions' checks.
    pub async fn detail(&self, target: &Target) -> Read<GitHubCore> {
        let (owner, name) = target.selector();
        let github_com = is_github_com(&target.host);
        let mut core = self
            .graphql(
                &target.host,
                "getPullRequestDetail",
                queries::core_query(github_com),
                json!({"owner":owner,"name":name,"number":target.number,"headRef":format!("refs/pull/{}/head",target.number)}),
                decode::decode_core,
            )
            .await?;
        if !core.checks_truncated {
            return Ok(core);
        }
        let unreadable = || PrFailure::unreadable("getPullRequestDetail");
        let mut contexts: Vec<Value> = vec![];
        let mut head: Option<String> = None;
        let mut after: Option<String> = None;
        for _ in 0..CHECK_CONTEXT_PAGES {
            let page = self
                .graphql(
                    &target.host,
                    "getPullRequestDetail",
                    queries::check_contexts_query(github_com),
                    json!({"owner":owner,"name":name,"number":target.number,"after":after}),
                    decode::decode_check_contexts,
                )
                .await?;
            if head.as_ref().is_some_and(|head| *head != page.head_sha) {
                return Err(unreadable());
            }
            head = Some(page.head_sha);
            contexts.extend(page.contexts);
            after = page.next_cursor;
            if after.is_none() {
                break;
            }
        }
        if head.as_deref() != core.detail.head_sha.as_deref() {
            return Err(unreadable());
        }
        let (checks, state) = decode::checks_from_contexts(&contexts);
        core.detail.checks = checks;
        core.detail.item.checks_state = state;
        core.checks_truncated = after.is_some();
        Ok(core)
    }

    /// A summary, read together with whatever else is asked for in the same moment.
    pub async fn summary(&self, target: &Target) -> Read<decode::GitHubSummary> {
        crate::pull_request_batch::read(self, target).await
    }

    /// One pull request's summary on its own, through the document the batch is written in.
    pub async fn summary_single(&self, target: &Target) -> Read<decode::GitHubSummary> {
        let (owner, name) = target.selector();
        let query = if is_github_com(&target.host) { queries::SUMMARY_SINGLE_WITH_STACKS } else { queries::SUMMARY_SINGLE };
        let summaries = self
            .graphql(&target.host, "getPullRequestSummary", query, json!({"owner":owner,"name":name,"number":target.number}), decode::decode_summaries)
            .await?;
        summaries.into_iter().next().map(|(_, summary)| summary).ok_or_else(|| {
            PrFailure::unreadable("getPullRequestSummary")
        })
    }

    pub async fn preview(&self, target: &Target) -> Read<decode::GitHubPreview> {
        let (owner, name) = target.selector();
        self.graphql(
            &target.host,
            "getPullRequestPreview",
            queries::PULL_REQUEST_PREVIEW_GRAPHQL_QUERY,
            json!({"owner":owner,"name":name,"number":target.number}),
            decode::decode_preview,
        )
        .await
    }

    /// What the viewer may do, read on its own (`getViewerAccess`); the interactive reserve may
    /// be spent on it because a press is waiting.
    pub async fn viewer_access(&self, target: &Target) -> Read<GitHubViewerAccess> {
        let (owner, name) = target.selector();
        let raw = Box::pin(api::graphql(
            &self.host,
            GraphQlInput {
                host: target.host.clone(),
                operation: "getViewerAccess".into(),
                query: queries::VIEWER_PERMISSIONS_GRAPHQL_QUERY.into(),
                variables: Some(json!({"owner":owner,"name":name,"number":target.number})),
                allow_reserve: Some(true),
                ..Default::default()
            },
        ))
        .await?;
        decode::decode_viewer_permissions(raw.trim()).map_err(|_| PrFailure::unreadable("getViewerAccess"))
    }

    /// The conversation outside the review threads: the author, issue comments, reviews and
    /// the newest commits. The two remark lists page on their own.
    pub async fn activity(&self, target: &Target) -> Read<ActivityRead> {
        let (owner, name) = target.selector();
        let mut author = None;
        let mut commits = vec![];
        let mut remarks = vec![];
        let (mut comments_after, mut reviews_after): (Option<String>, Option<String>) = (None, None);
        let (mut with_comments, mut with_reviews) = (true, true);
        let mut page = 0;
        while page < REVIEW_THREAD_PAGES && (with_comments || with_reviews) {
            let read = self
                .graphql(
                    &target.host,
                    "getPullRequestActivity",
                    queries::PULL_REQUEST_ACTIVITY_GRAPHQL_QUERY,
                    json!({"owner":owner,"name":name,"number":target.number,"head":page == 0,"withComments":with_comments,"commentsAfter":comments_after,"withReviews":with_reviews,"reviewsAfter":reviews_after}),
                    decode::decode_activity,
                )
                .await?;
            if page == 0 {
                author = read.author.flatten();
                commits = read.commits.unwrap_or_default();
            }
            remarks.extend(read.remarks);
            comments_after = read.next_comments_cursor;
            reviews_after = read.next_reviews_cursor;
            with_comments = with_comments && comments_after.is_some();
            with_reviews = with_reviews && reviews_after.is_some();
            page += 1;
        }
        remarks.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        Ok(ActivityRead { author, comments: remarks, commits })
    }

    /// Every review thread with its first comments, the people on the review and the commits.
    pub async fn review_threads(&self, target: &Target) -> Read<ThreadsRead> {
        let (owner, name) = target.selector();
        let mut entries = vec![];
        let mut avatars = std::collections::BTreeMap::new();
        let mut bots = std::collections::BTreeSet::new();
        let mut first: Option<decode::ReviewThreadPage> = None;
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let read = self
                .graphql(
                    &target.host,
                    "listReviewThreadComments",
                    queries::REVIEW_THREADS_GRAPHQL_QUERY,
                    json!({"owner":owner,"name":name,"number":target.number,"cursor":cursor}),
                    decode::decode_review_threads,
                )
                .await?;
            entries.extend(read.threads.iter().cloned());
            bots.extend(read.bot_logins.iter().cloned());
            avatars.extend(read.avatars_by_login.iter().map(|(login, avatar)| (login.clone(), avatar.clone())));
            cursor = read.next_cursor.clone();
            pages += 1;
            if first.is_none() {
                first = Some(read);
            }
            if cursor.is_none() || pages >= REVIEW_THREAD_PAGES {
                break;
            }
        }
        let first = first.expect("one page was read");
        // Almost never entered: the embedded page holds every dismissal a pull request ordinarily
        // accrues; followed so a review whose event fell past that page still finds its reason.
        let mut dismissals = first.dismissals_by_review_id.clone();
        let mut dismissal_cursor = first.next_dismissal_cursor.clone();
        let mut dismissal_pages = 0;
        while let Some(after) = dismissal_cursor.take() {
            if dismissal_pages >= REVIEW_THREAD_PAGES {
                break;
            }
            let (more, next) = self
                .graphql(
                    &target.host,
                    "listReviewThreadComments",
                    queries::REVIEW_DISMISSALS_GRAPHQL_QUERY,
                    json!({"owner":owner,"name":name,"number":target.number,"cursor":after}),
                    decode::decode_review_dismissals,
                )
                .await?;
            dismissals.extend(more);
            dismissal_cursor = next;
            dismissal_pages += 1;
        }
        let threads: Vec<t3_contracts::PullRequestReviewThread> = entries
            .iter()
            .map(|entry| t3_contracts::PullRequestReviewThread {
                comment_count: Some(entry.comment_count),
                next_comments_cursor: entry.next_comment_cursor.clone(),
                ..entry.thread.clone()
            })
            .collect();
        Ok(ThreadsRead {
            comment_count: entries.iter().map(|entry| entry.comment_count).sum(),
            truncated: cursor.is_some() || entries.iter().any(|entry| entry.next_comment_cursor.is_some()),
            review_threads_truncated: cursor.is_some(),
            comments: decode::review_thread_conversation(&threads),
            review_threads: threads,
            dismissals_by_review_id: dismissals,
            page: first,
            avatars_by_login: avatars,
            bot_logins: bots,
        })
    }

    /// The rest of one thread's conversation; refused for a thread of another pull request.
    pub async fn thread_comments(&self, target: &Target, thread_id: &str, cursor: &str) -> Read<t3_contracts::PullRequestThreadCommentsResult> {
        let (owner, name) = target.selector();
        let page = self
            .graphql(
                &target.host,
                "getReviewThreadComments",
                queries::REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY,
                json!({"owner":owner,"name":name,"number":target.number,"threadId":thread_id,"cursor":cursor}),
                decode::decode_review_thread_comments,
            )
            .await?;
        if !page.belongs_to_pull_request {
            return Err(PrFailure::failed("The named subject did not belong to the named pull request."));
        }
        Ok(t3_contracts::PullRequestThreadCommentsResult { comments: page.comments, next_cursor: page.next_cursor })
    }

    async fn diff_files_page(&self, target: &Target, page: u32, commit: Option<&str>) -> Read<PullRequestDiffResult> {
        let (owner, name) = target.selector();
        let paging = format!("per_page={DIFF_FILES_PAGE_SIZE}&page={page}");
        let path = match commit {
            None => format!("repos/{owner}/{name}/pulls/{}/files?{paging}", target.number),
            Some(commit) => format!("repos/{owner}/{name}/commits/{commit}?{paging}"),
        };
        let response = self
            .rest(RestInput {
                host: target.host.clone(),
                operation: "getPullRequestDiff".into(),
                path,
                max_response_bytes: Some(DIFF_MAX_OUTPUT_BYTES),
                timeout: Some(DIFF_TIMEOUT),
                ..Default::default()
            })
            .await?;
        // A byte-truncated response is a JSON prefix: nothing of this page can be shown, and an
        // empty patch would render as a change with no files rather than as the failure it is.
        if response.truncated {
            return Err(PrFailure::unreadable("getPullRequestDiff"));
        }
        let files = if commit.is_none() { decode::decode_pull_request_files(response.body.trim()) } else { decode::decode_commit_files(response.body.trim()) }
            .map_err(|_| PrFailure::unreadable("getPullRequestDiff"))?;
        Ok(PullRequestDiffResult {
            patch: files.patch,
            truncated: files.truncated,
            next_cursor: (files.raw_count >= DIFF_FILES_PAGE_SIZE).then(|| (page + 1).to_string()),
            omitted_file_stats: (!files.omitted_file_stats.is_empty()).then_some(files.omitted_file_stats),
        })
    }

    /// One slice of the patch: the whole pull request in one read when GitHub serves it, else a
    /// page of changed files at a time.
    pub async fn diff(&self, target: &Target, cursor: Option<&str>, commit: Option<&str>) -> Read<PullRequestDiffResult> {
        if commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(PrFailure::failed("The named commit was not a commit sha."));
        }
        if let Some(cursor) = cursor {
            let Some(page) = diff_cursor_page(cursor) else {
                return Err(PrFailure::failed("The diff cursor was not one this pull request handed out."));
            };
            return self.diff_files_page(target, page, commit).await;
        }
        if commit.is_some() {
            return self.diff_files_page(target, 1, commit).await;
        }
        let (owner, name) = target.selector();
        let direct = self
            .rest(RestInput {
                host: target.host.clone(),
                operation: "getPullRequestDiff".into(),
                path: format!("repos/{owner}/{name}/pulls/{}", target.number),
                accept: Some("application/vnd.github.diff".into()),
                max_response_bytes: Some(DIFF_MAX_OUTPUT_BYTES),
                timeout: Some(DIFF_TIMEOUT),
                ..Default::default()
            })
            .await;
        match direct {
            // A patch cut at a byte boundary ends mid-file; the files API serves the same change
            // a whole number of files at a time.
            Ok(response) if response.truncated => self.diff_files_page(target, 1, None).await,
            Ok(response) => Ok(PullRequestDiffResult { patch: response.body, truncated: false, next_cursor: None, omitted_file_stats: None }),
            // GitHub answers 406 rather than a diff past 300 changed files. Only once the direct
            // read was answered and refused; a fallback that fails too reports the original refusal.
            Err(failure) if failure.reason == Reason::Failed => {
                self.diff_files_page(target, 1, None).await.map_err(|_| failure)
            }
            Err(failure) => Err(failure),
        }
    }

    /// Both sides of one file, so unchanged lines around a hunk can be expanded.
    pub async fn diff_file_contents(
        &self,
        target: &Target,
        commit: Option<&str>,
        change_type: PullRequestDiffChangeType,
        old_path: &str,
        new_path: &str,
    ) -> Read<t3_contracts::PullRequestDiffFileContentsResult> {
        if commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(PrFailure::failed("The named commit was not a commit sha."));
        }
        let (owner, name) = target.selector();
        let refs = self
            .rest(RestInput {
                host: target.host.clone(),
                operation: "getPullRequestDiffFileContents".into(),
                path: match commit {
                    None => format!("repos/{owner}/{name}/pulls/{}", target.number),
                    Some(commit) => format!("repos/{owner}/{name}/commits/{commit}"),
                },
                timeout: Some(DIFF_TIMEOUT),
                ..Default::default()
            })
            .await?;
        let parsed: Option<Value> = serde_json::from_str(refs.body.trim()).ok();
        let sha_of = |value: Option<&Value>| value.and_then(|v| v.get("sha")).and_then(Value::as_str).map(str::to_string);
        let (base_ref, head_ref) = match (&parsed, commit) {
            (None, _) => (None, None),
            (Some(parsed), None) => (sha_of(parsed.get("base")), sha_of(parsed.get("head"))),
            (Some(parsed), Some(_)) => (
                Some(sha_of(parsed.get("parents").and_then(|parents| parents.get(0))).unwrap_or_default()),
                parsed.get("sha").and_then(Value::as_str).map(str::to_string),
            ),
        };
        let root_commit_new_file = commit.is_some() && change_type == PullRequestDiffChangeType::New && base_ref.as_deref() == Some("");
        let unusable = head_ref.as_deref().is_none_or(|head| !is_commit_sha(head))
            || (!root_commit_new_file && base_ref.as_deref().is_none_or(|base| !is_commit_sha(base)));
        if unusable {
            return Err(PrFailure::failed(match commit {
                None => format!("Pull request #{} reported no usable base and head revisions.", target.number),
                Some(commit) => format!("Commit {commit} reported no usable revisions for this file."),
            }));
        }
        let (base_ref, head_ref) = (base_ref.unwrap_or_default(), head_ref.unwrap_or_default());
        let old = async {
            if change_type == PullRequestDiffChangeType::New { Ok(String::new()) } else { self.read_file(target, &base_ref, old_path).await }
        };
        let new = async {
            if change_type == PullRequestDiffChangeType::Deleted { Ok(String::new()) } else { self.read_file(target, &head_ref, new_path).await }
        };
        let (old_contents, new_contents) = tokio::join!(old, new);
        Ok(t3_contracts::PullRequestDiffFileContentsResult { old_contents: old_contents?, new_contents: new_contents? })
    }

    /// One file at one revision, refused when it is too large to expand or is not text.
    async fn read_file(&self, target: &Target, revision: &str, file_path: &str) -> Read<String> {
        let (owner, name) = target.selector();
        let encoded: Vec<String> = file_path.split('/').map(percent_encode).collect();
        let response = self
            .rest(RestInput {
                host: target.host.clone(),
                operation: "getPullRequestDiffFileContents".into(),
                accept: Some("application/vnd.github.raw+json".into()),
                path: format!("repos/{owner}/{name}/contents/{}?ref={}", encoded.join("/"), percent_encode(revision)),
                max_response_bytes: Some(DIFF_FILE_MAX_OUTPUT_BYTES),
                timeout: Some(DIFF_TIMEOUT),
                ..Default::default()
            })
            .await?;
        if response.truncated {
            return Err(PrFailure::failed(format!("The diff file '{file_path}' exceeds the 1 MB expansion limit.")));
        }
        if response.body.contains('\0') || response.invalid_utf8 {
            return Err(PrFailure::failed(format!("The diff file '{file_path}' is binary.")));
        }
        Ok(response.body)
    }

    /// The reader's own marks, as GitHub's web UI shows them.
    pub async fn files_viewed(&self, target: &Target) -> Read<PullRequestFilesViewedResult> {
        let (owner, name) = target.selector();
        let mut files: Vec<PullRequestFileViewed> = vec![];
        let mut after: Option<String> = None;
        for page in 0..FILES_VIEWED_MAX_PAGES {
            let read = self
                .graphql(
                    &target.host,
                    "getPullRequestFilesViewed",
                    queries::PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY,
                    json!({"owner":owner,"name":name,"number":target.number,"after":after}),
                    decode::decode_files_viewed,
                )
                .await?;
            files.extend(read.files);
            match read.next_cursor {
                None => return Ok(PullRequestFilesViewedResult { files, truncated: false }),
                Some(next) if page + 1 == FILES_VIEWED_MAX_PAGES => {
                    let _ = next;
                    return Ok(PullRequestFilesViewedResult { files, truncated: true });
                }
                Some(next) => after = Some(next),
            }
        }
        Ok(PullRequestFilesViewedResult { files, truncated: true })
    }

    /// The host-native stack, or none when the host has no stacks preview (a 404).
    pub async fn stack(&self, target: &Target, include_details: bool) -> Read<Option<t3_contracts::PullRequestStack>> {
        let (owner, name) = target.selector();
        let read = async {
            let response = self
                .rest(RestInput {
                    host: target.host.clone(),
                    operation: "getPullRequestStack".into(),
                    path: format!("repos/{owner}/{name}/stacks?pull_request={}", target.number),
                    ..Default::default()
                })
                .await?;
            let stack = decode::decode_stacks(response.body.trim()).map_err(|_| PrFailure::unreadable("getPullRequestStack"))?;
            let Some(stack) = stack else { return Ok(None) };
            if !include_details {
                return Ok(Some(stack));
            }
            let detailed = self
                .rest(RestInput {
                    host: target.host.clone(),
                    operation: "getPullRequestStack".into(),
                    path: format!("repos/{owner}/{name}/stacks/{}", stack.number),
                    ..Default::default()
                })
                .await?;
            decode::decode_stacks(&format!("[{}]", detailed.body.trim())).map_err(|_| PrFailure::unreadable("getPullRequestStack"))
        };
        match read.await {
            Err(failure) if failure.reason == Reason::NotFound => Ok(None),
            other => other,
        }
    }

    /// A page of the host-wide search (`searchPullRequests`).
    pub async fn search(&self, host: &str, query: &str, rows: usize, include_stacks: bool) -> Read<decode::SearchBatch> {
        self.graphql(
            host,
            "searchPullRequests",
            &search_graphql_query(rows, include_stacks, false),
            json!({"q":query,"after":Value::Null}),
            decode::decode_pull_request_search,
        )
        .await
    }

    /// Line counts for pull requests anywhere on one host, 25 aliased lookups per request.
    pub async fn stats(&self, host: &str, requests: &[(String, u64)]) -> Read<Vec<(String, u64, u64, u64)>> {
        let mut found = vec![];
        for chunk in requests.chunks(STAT_ALIASES_PER_REQUEST) {
            let Some(query) = build_stats_query(chunk) else {
                return Err(PrFailure::failed("A repository was named that GitHub cannot address."));
            };
            let stats = self.graphql(host, "listPullRequestStats", &query, json!({}), decode::decode_stats).await?;
            for (index, (repository, number)) in chunk.iter().enumerate() {
                if let Some((additions, deletions)) = stats.get(&index) {
                    found.push((repository.clone(), *number, *additions, *deletions));
                }
            }
        }
        Ok(found)
    }
}

/// `encodeURIComponent` for the characters a repository path can hold.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Debug, Clone)]
pub struct ActivityRead {
    pub author: Option<t3_contracts::PullRequestActor>,
    pub comments: Vec<t3_contracts::PullRequestComment>,
    pub commits: Vec<t3_contracts::PullRequestCommit>,
}

#[derive(Debug, Clone)]
pub struct ThreadsRead {
    pub comments: Vec<t3_contracts::PullRequestComment>,
    pub dismissals_by_review_id: std::collections::BTreeMap<String, String>,
    pub review_threads: Vec<t3_contracts::PullRequestReviewThread>,
    pub comment_count: u64,
    pub truncated: bool,
    pub review_threads_truncated: bool,
    pub page: decode::ReviewThreadPage,
    pub avatars_by_login: std::collections::BTreeMap<String, String>,
    pub bot_logins: std::collections::BTreeSet<String>,
}

// ---------------------------------------------------------------------------------------------
// Query builders
// ---------------------------------------------------------------------------------------------

fn repository_part(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// `buildPullRequestStatsGraphQlQuery`: owner, name and number are written into the document, so
/// each is checked first; `None` for anything GitHub cannot name or an empty request.
pub fn build_stats_query(changes: &[(String, u64)]) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    let mut selections = vec![];
    for (index, (repository, number)) in changes.iter().enumerate() {
        let parts: Vec<&str> = repository.trim().split('/').collect();
        let [owner, name] = parts.as_slice() else { return None };
        if !repository_part(owner) || !repository_part(name) || *number == 0 || *number > 9_007_199_254_740_991 {
            return None;
        }
        selections.push(format!("  s{index}: repository(owner: \"{owner}\", name: \"{name}\") {{ pullRequest(number: {number}) {{ additions deletions }} }}"));
    }
    Some(format!("query {{\n{}\n}}", selections.join("\n")))
}

fn page_rows(rows: usize) -> usize {
    rows.clamp(1, SEARCH_MAX_ROWS)
}

fn row_selection(include_stacks: bool, include_stats: bool) -> String {
    format!(
        "{}\n        number\n        title\n        url\n        author {{ __typename login avatarUrl ... on User {{ name }} }}\n        headRefName\n        baseRefName\n        state\n        isDraft\n        mergeable\n        reviewDecision\n        latestReviews(first: 20) {{ nodes {{ state author {{ login }} }} }}\n        {}\n        createdAt\n        updatedAt\n        mergedAt\n        repository {{ nameWithOwner }}\n        reviewRequests(first: 20) {{ nodes {{ requestedReviewer {{ ... on User {{ login }} ... on Team {{ slug }} }} }} }}\n        labels(first: 20) {{ nodes {{ name color }} }}\n        commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }}",
        if include_stacks { "stack { number size baseRefName } stackEntry { position }" } else { "" },
        if include_stats { "additions deletions" } else { "" },
    )
}

/// `pullRequestSearchGraphQlQuery`: every repository of a host in one read.
pub fn search_graphql_query(rows: usize, include_stacks: bool, include_stats: bool) -> String {
    format!(
        "query($q: String!, $after: String) {{\n  search(query: $q, type: ISSUE, first: {}, after: $after) {{\n    pageInfo {{ hasNextPage endCursor }}\n    nodes {{\n      ... on PullRequest {{\n        {}\n      }}\n    }}\n  }}\n}}",
        page_rows(rows),
        row_selection(include_stacks, include_stats)
    )
}

/// `pullRequestListGraphQlQuery`: a repository's own list, newest created first.
pub fn list_graphql_query(rows: usize, include_stacks: bool) -> String {
    format!(
        "query($owner: String!, $name: String!, $states: [PullRequestState!], $after: String) {{\n  repository(owner: $owner, name: $name) {{\n    pullRequests(first: {}, after: $after, states: $states, orderBy: {{ field: CREATED_AT, direction: DESC }}) {{\n      pageInfo {{ hasNextPage endCursor }}\n      nodes {{ {} }}\n    }}\n  }}\n}}",
        page_rows(rows),
        row_selection(include_stacks, true)
    )
}

fn search_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('\\', "\\\\").replace('"', "\\\""))
}

fn qualifier_value(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "").trim())
}

/// `resolvePullRequestAuthorFilter`: `me` and `@me` mean the signed-in account.
pub fn resolve_author_filter(author: &str, viewer: &str) -> String {
    let trimmed = author.trim();
    let is_me = trimmed.eq_ignore_ascii_case("me") || trimmed.eq_ignore_ascii_case("@me");
    if !is_me || viewer.trim().is_empty() { trimmed.to_string() } else { viewer.to_string() }
}

pub struct SearchInput<'a> {
    pub repositories: &'a [String],
    pub state: t3_contracts::PullRequestListState,
    pub involvement: t3_contracts::PullRequestInvolvement,
    pub viewer: &'a str,
    pub query: Option<&'a str>,
    pub updated_before: Option<&'a str>,
    pub filters: Option<&'a t3_contracts::PullRequestListFilters>,
}

fn filter_qualifiers(filters: Option<&t3_contracts::PullRequestListFilters>, viewer: &str) -> Vec<String> {
    use t3_contracts::{PullRequestChecksFilter, PullRequestDraftFilter, PullRequestReviewFilter};
    let Some(filters) = filters else { return vec![] };
    let mut qualifiers = vec![];
    for group in filters.labels.iter().flatten() {
        if !group.is_empty() {
            qualifiers.push(format!("label:{}", group.iter().map(|label| qualifier_value(label.as_str())).collect::<Vec<_>>().join(",")));
        }
    }
    for label in filters.excluded_labels.iter().flatten() {
        qualifiers.push(format!("-label:{}", qualifier_value(label.as_str())));
    }
    if let Some(author) = &filters.author {
        qualifiers.push(format!("author:{}", qualifier_value(&resolve_author_filter(author.as_str(), viewer))));
    }
    if let Some(draft) = filters.draft {
        qualifiers.push(format!("draft:{}", draft == PullRequestDraftFilter::Only));
    }
    if let Some(review) = filters.review {
        qualifiers.push(format!(
            "review:{}",
            match review {
                PullRequestReviewFilter::Approved => "approved",
                PullRequestReviewFilter::ChangesRequested => "changes_requested",
                PullRequestReviewFilter::ReviewRequired => "required",
                PullRequestReviewFilter::None => "none",
            }
        ));
    }
    if let Some(checks) = filters.checks {
        qualifiers.push(format!("status:{}", if checks == PullRequestChecksFilter::Passing { "success" } else { "failure" }));
    }
    qualifiers
}

/// The search query for a listing, or `None` when a repository is not one GitHub can name
/// (`searchQuery`). The reader's text is one quoted phrase so it cannot become a qualifier.
pub fn search_query(input: &SearchInput) -> Option<String> {
    use t3_contracts::{PullRequestInvolvement, PullRequestListState};
    if input.repositories.is_empty() {
        return None;
    }
    let repositories: Vec<&str> = input.repositories.iter().map(|repository| repository.trim()).collect();
    let nameable = |repository: &&str| {
        let parts: Vec<&str> = repository.split('/').collect();
        matches!(parts.as_slice(), [owner, name] if repository_part(owner) && repository_part(name))
    };
    if !repositories.iter().all(nameable) {
        return None;
    }
    let query = input.query.map(str::trim).unwrap_or("");
    let mut terms = vec!["is:pr".to_string()];
    match input.state {
        PullRequestListState::Open => terms.push("is:open".into()),
        PullRequestListState::Closed => terms.extend(["is:closed".into(), "is:unmerged".into()]),
        PullRequestListState::Merged => terms.push("is:merged".into()),
        PullRequestListState::All => {}
    }
    match input.involvement {
        PullRequestInvolvement::Authored => terms.push(format!("author:{}", input.viewer)),
        PullRequestInvolvement::Reviewing => terms.push(format!("review-requested:{}", input.viewer)),
        PullRequestInvolvement::All => {}
    }
    if !query.is_empty() {
        terms.push(search_phrase(query));
    }
    if let Some(updated_before) = input.updated_before {
        terms.push(format!("updated:<={updated_before}"));
    }
    terms.extend(filter_qualifiers(input.filters, input.viewer));
    terms.push("sort:updated-desc".into());
    terms.extend(repositories.iter().map(|repository| format!("repo:{repository}")));
    Some(terms.join(" "))
}
