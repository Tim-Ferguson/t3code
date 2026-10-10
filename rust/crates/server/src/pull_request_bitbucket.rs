//! Bitbucket Cloud pull requests behind the pull request provider interface (port of
//! `BitbucketPullRequestApi.ts` and `BitbucketPullRequestProvider.ts`). Everything goes through
//! `BitbucketApi::request`, the raw request path of the source-control module: it carries the
//! account's credential to the configured Bitbucket only, follows the redirects Bitbucket serves a
//! diff behind, and hands the body back undecoded. The workspace and repository come from the
//! change request's `workspace/repository`; credentials are the server's, so the account is the
//! same whichever checkout asks.
//!
//! Bitbucket has no reaction, reopen or draft, and no labels, stacks or search across repositories;
//! none is declared, so none is offered.
use crate::{
    pull_request_actions::RunAction,
    pull_request_bitbucket_json::{self as json, BitbucketPullRequest, DiffStat, Page, locale_cmp},
    pull_request_bitbucket_revisions::parse_diff_file_revisions,
    pull_request_github::{PrFailure, Reason, Read},
    pull_request_provider::*,
    source_control_bitbucket::{BitbucketApi, BitbucketError},
    source_control_host::Host,
    source_control_uri::encode_uri_component,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use t3_contracts::*;

/// Bitbucket's own ceiling. Asking for more does not fail: it answers an empty page and no error
/// at all, so this is a number to respect rather than to push against.
const MAX_PAGE_SIZE: usize = 50;
/// Pages to walk before a listing is reported as truncated.
const MAX_LIST_PAGES: usize = 10;
/// The page size for pull request conversations, commits, and checks.
const CONVERSATION_PAGE_SIZE: usize = 50;
/// Pages of the conversation to follow before it is reported as truncated: five hundred comments,
/// beyond any pull request a person is reading, and an end to a walk whose only other stop is
/// Bitbucket running out.
const CONVERSATION_PAGES: usize = 10;
/// The same ceiling the gh and glab diff reads use.
const DIFF_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Deliberately far shorter than the window the caller holds versions for: a refresh drops what the
/// caller holds precisely so the next read reaches Bitbucket, and this must not be what answers it
/// instead.
const REVISION_PATCH_TTL: Duration = Duration::from_secs(5);
pub const REVISION_PATCH_CAPACITY: usize = 16;

/// What the version reads that come one tick at a time want out of a pull request's whole patch,
/// shared between them: the parsed answer rather than the patch, which at this capacity would hold
/// sixteen bodies of up to the byte ceiling each resident.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedPatch {
    pub revisions: BTreeMap<String, String>,
    pub truncated: bool,
}

// ---- capabilities ---------------------------------------------------------------------------------

pub fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        comment: true,
        // Bitbucket has no endpoint that reopens a declined pull request, and nothing documented
        // that moves one in or out of draft, so neither is offered rather than failing when pressed.
        actions: vec![PullRequestAction::Merge, PullRequestAction::Close],
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        update_methods: None,
        search: true,
        // Bitbucket Cloud's API exposes no reaction on a pull request or on a comment.
        reactions: Some(false),
        // Bitbucket Cloud states nothing about what a reviewer has already read: no endpoint carries
        // a viewed file, and the per-pull-request properties it does offer are one value shared by
        // everyone rather than one per reader. So the marks are kept here, and the client says whose
        // they are rather than implying bitbucket.org will show them.
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: true,
            resolve: true,
            verdicts: vec![PullRequestReviewVerdict::Comment, PullRequestReviewVerdict::Approve, PullRequestReviewVerdict::RequestChanges],
        },
        reviewers: PullRequestReviewerCapabilities { request: true, list_candidates: true },
        edit: Some(PullRequestEditCapabilities { change_request: true, comment: true }),
        stacks: None,
        stack_actions: None,
        labels: None,
    }
}

/// What the configured account may do here, from the one thing Bitbucket states per viewer: the
/// repository permission (`bitbucketViewerPermissions`). Merging needs `write` or `admin`, so that is
/// what narrows. Declining stays offered whatever the permission: Bitbucket lets the author of a
/// pull request decline their own with no more than read access, and the permission response says
/// nothing about who opened this one. Commenting, reviewing and asking for a review are not
/// narrowed either: read access is enough to say something, to approve and to ask for changes.
pub fn viewer_permissions(can_write: bool) -> PullRequestViewerPermissions {
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: capabilities().actions.into_iter().filter(|action| *action != PullRequestAction::Merge || can_write).collect(),
        comment: true,
        resolve: true,
        verdicts: capabilities().review.verdicts,
        request_reviewers: true,
        update_methods: None,
        labels: None,
    }
}

// ---- failures -------------------------------------------------------------------------------------

/// `BitbucketPullRequestApiError`: the host layer's errors and the ones this layer adds.
#[derive(Debug, Clone)]
enum Fault {
    Api(BitbucketError),
    /// Bitbucket answered with something this layer cannot read; names the read.
    Unreadable(&'static str),
    /// Bitbucket answered, but the account it answered for has no handle.
    ViewerUnavailable,
    /// A repository that is not `workspace/slug`, which is the only form Bitbucket addresses.
    RepositoryUnsupported,
    /// The reader named a commit that is not a sha this repository could hold.
    DiffCommit,
}

impl From<BitbucketError> for Fault {
    fn from(error: BitbucketError) -> Self {
        Fault::Api(error)
    }
}

impl Fault {
    /// `bitbucketProviderFailure`: Bitbucket is read over HTTP with credentials from the
    /// environment, so there is no tool to be missing; unusable always means the credentials are
    /// absent or refused. Every failure states its own fact; no credential reaches it.
    fn failure(self) -> PrFailure {
        match self {
            Fault::Api(error) => {
                let response = error.tag == "BitbucketResponseError";
                let body = error.tag == "BitbucketResponseBodyReadError";
                let (reason, retry_at) = match error.status {
                    Some(401) if response => (Reason::Unauthenticated, None),
                    Some(429) if response || body => (Reason::RateLimited, error.retry_at),
                    Some(404) if response || body => (Reason::NotFound, None),
                    _ => (Reason::Failed, None),
                };
                PrFailure { reason, detail: error.detail().to_string(), retry_at }
            }
            Fault::Unreadable(operation) => PrFailure::failed(format!("Bitbucket returned an unreadable {operation} response.")),
            Fault::ViewerUnavailable => PrFailure::failed("Bitbucket returned no account name for the configured credentials."),
            Fault::RepositoryUnsupported => PrFailure::failed("A Bitbucket repository is addressed as workspace/repository."),
            Fault::DiffCommit => PrFailure::failed("The named commit was not a commit sha."),
        }
    }

    fn is_rate_limited(&self) -> bool {
        matches!(self, Fault::Api(error) if error.status == Some(429) && (error.tag == "BitbucketResponseError" || error.tag == "BitbucketResponseBodyReadError"))
    }
}

type Outcome<T> = Result<T, Fault>;

/// `recoverRead`: a read that may fail without costing the page its other parts. A rate limit is
/// never recovered, or a paused host would be asked again at once.
fn recover<T>(read: Outcome<T>, fallback: T) -> Outcome<T> {
    match read {
        Err(fault) if fault.is_rate_limited() => Err(fault),
        Err(_) => Ok(fallback),
        ok => ok,
    }
}

// ---- addressing -----------------------------------------------------------------------------------

/// `workspace/slug`; Bitbucket has no deeper nesting to address.
fn repository_segments(repository: &str) -> Outcome<(String, String)> {
    let segments: Vec<&str> = repository.split('/').map(t3_contracts::trim_wire_string).filter(|segment| !segment.is_empty()).collect();
    match segments.as_slice() {
        [workspace, slug] => Ok(((*workspace).to_string(), (*slug).to_string())),
        _ => Err(Fault::RepositoryUnsupported),
    }
}

/// A commit sha arrives from the reader and goes straight into a request path, so it is checked
/// rather than trusted: hexadecimal only, from the shortest abbreviation a host prints up to a
/// whole sha.
fn is_commit_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Bitbucket unions repeated `state` parameters, so a tab that spans several of its states asks for
/// each. It separates a declined pull request from one superseded by another, and both read as
/// closed here.
fn state_params(state: PullRequestListState) -> &'static [&'static str] {
    match state {
        PullRequestListState::Open => &["OPEN"],
        PullRequestListState::Merged => &["MERGED"],
        PullRequestListState::Closed => &["DECLINED", "SUPERSEDED"],
        PullRequestListState::All => &["OPEN", "MERGED", "DECLINED", "SUPERSEDED"],
    }
}

/// Text as a string literal of Bitbucket's filter grammar. The backslash is escaped first, or
/// escaping the quote would only produce a literal backslash followed by a live quote.
fn filter_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Bitbucket has no search term, only a filter expression, so free text becomes one: a
/// case-insensitive contains against the two fields a pull request carries words in. The
/// parentheses matter, because the expression is ANDed with the state filter beside it and an
/// unbracketed `OR` would swallow it. The reader's text is escaped before it goes inside a literal,
/// and the whole expression is URL-encoded, so nothing in it reaches the query string as a
/// parameter of its own.
fn search_filter(query: &str) -> String {
    let literal = filter_literal(query);
    format!("(title ~ \"{literal}\" OR description ~ \"{literal}\")")
}

/// Bitbucket's merge strategies, named differently from the three the contract carries.
fn merge_strategy(method: Option<PullRequestMergeMethod>) -> &'static str {
    match method {
        Some(PullRequestMergeMethod::Squash) => "squash",
        // The linear history GitHub calls "rebase and merge".
        Some(PullRequestMergeMethod::Rebase) => "rebase_fast_forward",
        _ => "merge_commit",
    }
}

fn review_position(position: &PullRequestReviewPosition) -> (&'static str, u64) {
    match position {
        PullRequestReviewPosition::Added { new_line } => ("to", *new_line),
        PullRequestReviewPosition::Deleted { old_line } => ("from", *old_line),
        PullRequestReviewPosition::Context { old_line, new_line, side } => match side {
            PullRequestDiffSide::Left => ("from", *old_line),
            PullRequestDiffSide::Right => ("to", *new_line),
        },
    }
}

/// `Number(text)` as JSON.stringify writes it into a request body: the number the text names, and
/// the `null` a number JSON cannot hold (NaN, an infinity) becomes. An empty text is zero.
fn comment_id_number(text: &str) -> Value {
    let text = text.trim();
    let radix = |prefixes: [&str; 2], radix: u32| prefixes.iter().find_map(|prefix| text.strip_prefix(prefix)).map(|digits| u64::from_str_radix(digits, radix).map(|number| number as f64));
    let number = if text.is_empty() {
        Some(0.0)
    } else if let Some(parsed) = radix(["0x", "0X"], 16).or_else(|| radix(["0b", "0B"], 2)).or_else(|| radix(["0o", "0O"], 8)) {
        parsed.ok()
    } else {
        text.parse::<f64>().ok()
    };
    match number.filter(|number| number.is_finite()) {
        Some(number) if number.fract() == 0.0 && number.abs() < 9.0e15 => json!(number as i64),
        Some(number) => json!(number),
        None => Value::Null,
    }
}

fn to_change_request(pull_request: &BitbucketPullRequest) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number: pull_request.number,
        title: pull_request.title.clone(),
        url: pull_request.url.clone(),
        author: pull_request.author.clone(),
        head_branch: pull_request.head_branch.clone(),
        head_repository_name_with_owner: pull_request.head_repository_name_with_owner.clone().filter(|name| !name.is_empty()).map(Some),
        base_branch: pull_request.base_branch.clone(),
        state: pull_request.state,
        is_draft: pull_request.is_draft,
        mergeability: pull_request.mergeability,
        // Line counts are a separate read, which only the detail is worth spending on.
        additions: 0,
        deletions: 0,
        created_at: pull_request.created_at.clone(),
        updated_at: pull_request.updated_at.clone(),
        review_request_logins: pull_request.review_request_logins.clone(),
        // Bitbucket has no labels on a pull request.
        labels: vec![],
        review_decision: None,
        checks_state: None,
    }
}

// ---- the API --------------------------------------------------------------------------------------

struct Conversation {
    comments: Vec<PullRequestComment>,
    threads: Vec<PullRequestReviewThread>,
    truncated: bool,
}

pub struct BitbucketPullRequests {
    host: Host,
    api: BitbucketApi,
}

impl BitbucketPullRequests {
    pub fn new(host: Host) -> Self {
        Self { api: BitbucketApi::new(host.clone()), host }
    }

    async fn get(&self, url: &str, max_bytes: Option<usize>) -> Outcome<crate::source_control_bitbucket::RawResponse> {
        Ok(self.api.request(reqwest::Method::GET, url, None, max_bytes).await?)
    }

    async fn send(&self, method: reqwest::Method, url: &str, body: Option<Value>) -> Outcome<()> {
        let body = body.map(|body| body.to_string());
        self.api.request(method, url, body.as_deref(), None).await?;
        Ok(())
    }

    async fn read_page<A>(&self, operation: &'static str, url: &str, decode: impl FnOnce(&str) -> Result<A, ()>) -> Outcome<A> {
        let response = Box::pin(self.get(url, None)).await?;
        decode(&response.body).map_err(|()| Fault::Unreadable(operation))
    }

    async fn pull_request(&self, path: &str, number: u64) -> Outcome<BitbucketPullRequest> {
        Box::pin(self.read_page("getPullRequest", &format!("{path}/pullrequests/{number}"), json::decode_pull_request_json)).await
    }

    /// Bitbucket pages with a cursor rather than an offset, so the walk follows the `next` URL it
    /// sends. It stops once the caller's page is filled, when Bitbucket reports no next page, or at
    /// the page cap; anything but running out of pages means there is more to be had.
    async fn list_pages(&self, first: String, limit: usize) -> Outcome<(Vec<BitbucketPullRequest>, bool)> {
        let (mut url, mut page, mut collected) = (first, 1, Vec::new());
        loop {
            let decoded = Box::pin(self.read_page("listPullRequests", &url, json::decode_pull_request_page_json)).await?;
            collected.extend(decoded.items);
            let next = decoded.next;
            let Some(following) = next.clone().filter(|_| collected.len() < limit && page < MAX_LIST_PAGES) else {
                // Bitbucket pages in fifties whatever was asked for, so a walk that stopped on the
                // count rather than on the last page is holding rows it is about to drop. Those are
                // more results just as surely as another page would be.
                let truncated = next.is_some() || collected.len() > limit;
                collected.truncate(limit);
                return Ok((collected, truncated));
            };
            url = following;
            page += 1;
        }
    }

    /// The conversation, following the `next` Bitbucket sends until it sends none. Threads are
    /// assembled once at the end rather than per page, because a reply and the remark it answers can
    /// land either side of a page boundary.
    async fn conversation(&self, path: &str, number: u64) -> Outcome<Conversation> {
        let mut url = format!("{path}/pullrequests/{number}/comments?pagelen={CONVERSATION_PAGE_SIZE}");
        let (mut comments, mut entries) = (Vec::new(), Vec::new());
        for page in 1.. {
            let decoded = Box::pin(self.read_page("listComments", &url, json::decode_comments_json)).await?;
            comments.extend(decoded.comments);
            entries.extend(decoded.entries);
            match decoded.next {
                Some(next) if page < CONVERSATION_PAGES => url = next,
                next => return Ok(Conversation { comments, threads: json::build_review_threads(&entries), truncated: next.is_some() }),
            }
        }
        unreachable!("the walk returns from inside the loop")
    }

    /// Walks a Bitbucket cursor to its end and combines every decoded item. Commit pages are
    /// individually oldest-first, so older pages are prepended.
    async fn item_pages<A>(&self, operation: &'static str, first: String, decode: fn(&str) -> Result<Page<A>, ()>, prepend: bool) -> Outcome<Vec<A>> {
        let (mut url, mut items) = (first, Vec::new());
        loop {
            let page = Box::pin(self.read_page(operation, &url, decode)).await?;
            items = if prepend { page.items.into_iter().chain(items).collect() } else { items.into_iter().chain(page.items).collect() };
            match page.next {
                None => return Ok(items),
                Some(next) => url = next,
            }
        }
    }

    /// Diffstat has one aggregate per page, so its totals are folded while following `next`.
    async fn diff_stat(&self, path: &str, number: u64) -> Outcome<DiffStat> {
        let (mut url, mut total) = (format!("{path}/pullrequests/{number}/diffstat?pagelen={MAX_PAGE_SIZE}"), DiffStat::default());
        loop {
            let (page, next) = Box::pin(self.read_page("getDiffStat", &url, json::decode_diffstat_json)).await?;
            total = DiffStat { additions: total.additions + page.additions, deletions: total.deletions + page.deletions, changed_files: total.changed_files + page.changed_files };
            match next {
                None => return Ok(total),
                Some(next) => url = next,
            }
        }
    }

    /// Already a unified patch, so it needs no decoding at all, only a bound, which a diff of any
    /// size would otherwise ignore. A commit's own patch sits beside the pull request's at
    /// `/diff/{sha}` and reads the same way.
    async fn pull_request_diff(&self, repository: &str, number: u64, commit: Option<&str>) -> Outcome<(String, bool)> {
        if commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(Fault::DiffCommit);
        }
        let (workspace, slug) = repository_segments(repository)?;
        let path = repository_path(&workspace, &slug);
        let url = match commit {
            None => format!("{path}/pullrequests/{number}/diff"),
            Some(commit) => format!("{path}/diff/{commit}"),
        };
        let response = Box::pin(self.get(&url, Some(DIFF_MAX_BYTES))).await?;
        Ok((response.body, response.truncated))
    }

    async fn parsed_patch(&self, repository: &str, number: u64) -> Read<Arc<ParsedPatch>> {
        let key = json!([repository, number]).to_string();
        let load = async move {
            match Box::pin(self.pull_request_diff(repository, number, None)).await {
                Ok((patch, truncated)) => Ok(Arc::new(ParsedPatch { revisions: parse_diff_file_revisions(&patch), truncated })),
                Err(fault) => Err(fault.failure()),
            }
        };
        // A failure is not held: the tick after it should reach Bitbucket rather than be handed the
        // same error for as long as a good patch would have lasted.
        self.host.shared.bitbucket_patches.get(key, load, |result| if result.is_ok() { REVISION_PATCH_TTL } else { Duration::ZERO }).await
    }

    /// Nothing on the repository, the pull request or the workspace states what the credentials may
    /// do, so this endpoint is the one request Bitbucket makes unavoidable. Bitbucket permanently
    /// removed it (CHANGE-2770): every account now gets HTTP 410 in place of an answer, whatever it
    /// may do. That is the deprecated-endpoint signal, not a permission being refused, so it is read
    /// as a permission that could not be learned, which grants rather than blocks, and leaves the
    /// actual merge or write to say why if the account may not do it. Any other failure (a bad
    /// token, a network fault, an unreadable body) still fails.
    async fn repository_permission(&self, repository: &str) -> Outcome<bool> {
        repository_segments(repository)?;
        let url = format!("/user/permissions/repositories?q={}", encode_uri_component(&format!("repository.full_name=\"{}\"", filter_literal(t3_contracts::trim_wire_string(repository)))));
        match Box::pin(self.read_page("getRepositoryPermission", &url, json::decode_repository_permission_json)).await {
            Err(Fault::Api(error)) if error.tag == "BitbucketResponseError" && error.status == Some(410) => Ok(true),
            other => other,
        }
    }

    async fn viewer_account(&self) -> Outcome<String> {
        match Box::pin(self.read_page("getViewer", "/user", json::decode_viewer_json)).await? {
            None => Err(Fault::ViewerUnavailable),
            Some(account) => Ok(account),
        }
    }

    async fn list_rows(&self, request: &ListRequest) -> Outcome<(Vec<BitbucketPullRequest>, bool)> {
        let repository = request.repositories.first().cloned().unwrap_or_default();
        let (workspace, slug) = repository_segments(&repository)?;
        let search = request.query.as_deref().map(t3_contracts::trim_wire_string).unwrap_or("");
        // Both narrowings share the one `q` Bitbucket takes, so they are ANDed rather than one
        // replacing the other. The boundary instant is read inclusively: the rows already sent at it
        // come back and the caller drops them, which is what keeps their neighbours at the same
        // instant from being skipped. A date is a bare literal in this grammar, and this one was
        // checked against a timestamp's shape before it got here.
        let mut predicates = vec![];
        if !search.is_empty() {
            predicates.push(search_filter(search));
        }
        if let Some(cursor) = &request.cursor {
            predicates.push(format!("updated_on <= {}", cursor.updated_before));
        }
        let states = state_params(request.state).iter().map(|state| format!("state={state}")).collect::<Vec<_>>().join("&");
        // Reviewers are not on a listing by default, and `viewerReviewRequested` needs them.
        let mut url = format!("{}/pullrequests?{states}&pagelen={MAX_PAGE_SIZE}&sort=-updated_on&fields=%2Bvalues.reviewers", repository_path(&workspace, &slug));
        if !predicates.is_empty() {
            url.push_str(&format!("&q={}", encode_uri_component(&predicates.join(" AND "))));
        }
        self.list_pages(url, request.limit as usize).await
    }

    async fn do_detail(&self, subject: &Subject) -> Outcome<ChangeRequestDetail> {
        let (workspace, slug) = repository_segments(&subject.repository)?;
        let path = repository_path(&workspace, &slug);
        let number = subject.number;
        let repository = subject.repository.as_str();
        // Five reads at once; the first to fail ends the rest. A part Bitbucket cannot answer leaves
        // that part unknown rather than failing the page. A permission that could not be read is an
        // unknown one, which is granted: a hidden Merge leaves someone entitled to it with no way
        // through, and one Bitbucket refuses at least says why.
        let (pull_request, diff_stat, mergeability, checks, can_write) = tokio::try_join!(
            Box::pin(self.pull_request(&path, number)),
            Box::pin(self.diff_stat(&path, number)),
            async { recover(Box::pin(self.read_page("getMergeability", &format!("{path}/pullrequests/{number}/conflicts"), json::decode_conflicts_json)).await, PullRequestMergeability::Unknown) },
            async { recover(Box::pin(self.item_pages("listChecks", format!("{path}/pullrequests/{number}/statuses?pagelen={CONVERSATION_PAGE_SIZE}"), json::decode_statuses_json, false)).await, vec![]) },
            async { recover(Box::pin(self.repository_permission(repository)).await, true) },
        )?;
        let row = ProviderChangeRequest { mergeability, additions: diff_stat.additions, deletions: diff_stat.deletions, ..to_change_request(&pull_request) };
        Ok(ChangeRequestDetail {
            row,
            head_sha: None,
            body: pull_request.body,
            changed_files: diff_stat.changed_files,
            merged_at: None,
            closed_at: None,
            reviewers: pull_request.reviewers,
            checks,
            // Bitbucket publishes no per-repository list of allowed strategies, so the ones it
            // supports are all offered and a strategy the repository forbids fails on merge.
            merge_capabilities: PullRequestMergeCapabilities { merge: true, squash: true, rebase: true },
            viewer_permissions: viewer_permissions(can_write),
            base_comparison: None,
            behind_by: None,
            auto_merge_enabled: None,
            auto_merge_method: None,
            workflow_approvals_required: None,
        })
    }

    async fn do_activity(&self, subject: &Subject) -> Outcome<PullRequestActivity> {
        let (workspace, slug) = repository_segments(&subject.repository)?;
        let path = repository_path(&workspace, &slug);
        let number = subject.number;
        let (pull_request, conversation, commits) = tokio::try_join!(
            // Reviews ride on the pull request itself, so this inexpensive core read is repeated here
            // rather than making the core response wait for the conversation endpoints.
            Box::pin(self.pull_request(&path, number)),
            async { recover(Box::pin(self.conversation(&path, number)).await, Conversation { comments: vec![], threads: vec![], truncated: true }) },
            async { recover(Box::pin(self.item_pages("listCommits", format!("{path}/pullrequests/{number}/commits?pagelen={CONVERSATION_PAGE_SIZE}"), json::decode_commits_json, true)).await, vec![]) },
        )?;
        let comment_count = (conversation.comments.len() + pull_request.reviews.len()) as u64;
        let mut comments = conversation.comments;
        comments.extend(pull_request.reviews);
        comments.sort_by(|left, right| locale_cmp(&left.created_at, &right.created_at));
        Ok(PullRequestActivity {
            author: None,
            reviewers: None,
            comments,
            comment_count,
            comments_truncated: conversation.truncated,
            review_threads_truncated: None,
            review_threads: conversation.threads,
            commits,
            reactions: None,
        })
    }

    /// Who this pull request may be sent to, and who it has already been sent to. Two reads at once,
    /// because Bitbucket keeps the people on the workspace and the reviewers on the pull request,
    /// and neither answers for the other. Users only: Bitbucket requests a review of an account, and
    /// has no group that stands in for one on a pull request.
    async fn do_reviewer_candidates(&self, subject: &Subject) -> Outcome<PullRequestReviewerCandidateList> {
        let (workspace, slug) = repository_segments(&subject.repository)?;
        let path = repository_path(&workspace, &slug);
        let members_url = format!("/workspaces/{}/members?pagelen={MAX_PAGE_SIZE}", encode_uri_component(&workspace));
        let (pull_request, members) = tokio::try_join!(
            Box::pin(self.pull_request(&path, subject.number)),
            Box::pin(self.read_page("listReviewerCandidates", &members_url, json::decode_workspace_members_json)),
        )?;
        let author = pull_request.author.as_ref().map(|author| author.login.as_str().to_string());
        Ok(PullRequestReviewerCandidateList {
            // The author is dropped rather than shown unusable: Bitbucket refuses to make the person
            // who opened a pull request its reviewer.
            candidates: members
                .items
                .into_iter()
                .filter(|candidate| Some(&candidate.login) != author.as_ref())
                .map(|candidate| PullRequestReviewerCandidate { is_requested: pull_request.reviewer_ids.contains(&candidate.id), ..candidate })
                .collect(),
            truncated: members.next.is_some(),
        })
    }

    async fn do_set_reviewer_request(&self, subject: &Subject, reviewers: &[PullRequestReviewerRequestTarget], requested: bool) -> Outcome<()> {
        let (workspace, slug) = repository_segments(&subject.repository)?;
        let path = repository_path(&workspace, &slug);
        let current = self.pull_request(&path, subject.number).await?;
        // Bitbucket has no endpoint that adds or removes one reviewer: the pull request's
        // `reviewers` is written whole, so the set that is already there is read first and the
        // change applied to it. Everything else about the pull request is left out of the body,
        // which leaves it as it was.
        let mut uuids: Vec<String> = vec![];
        for uuid in current.reviewer_ids {
            if !uuids.contains(&uuid) {
                uuids.push(uuid);
            }
        }
        for reviewer in reviewers {
            let id = reviewer.id.as_str().to_string();
            if requested {
                if !uuids.contains(&id) {
                    uuids.push(id);
                }
            } else {
                uuids.retain(|held| *held != id);
            }
        }
        let body = json!({"reviewers": uuids.iter().map(|uuid| json!({"uuid": uuid})).collect::<Vec<_>>()});
        self.send(reqwest::Method::PUT, &format!("{path}/pullrequests/{}", subject.number), Some(body)).await
    }

    /// Only merge and close reach here: the provider declares the others unsupported, so the
    /// surface never offers them.
    async fn do_run_action(&self, subject: &Subject, run: &RunAction) -> Outcome<()> {
        let (workspace, slug) = repository_segments(&subject.repository)?;
        let pull_request = format!("{}/pullrequests/{}", repository_path(&workspace, &slug), subject.number);
        if run.action == PullRequestAction::Merge {
            return self.send(reqwest::Method::POST, &format!("{pull_request}/merge"), Some(json!({"merge_strategy": merge_strategy(run.merge_method)}))).await;
        }
        self.send(reqwest::Method::POST, &format!("{pull_request}/decline"), None).await
    }

    /// Bitbucket has no pending review, so a review is replayed as the requests it is made of: the
    /// line comments, then the summary, then the verdict. The verdict goes last so a review that
    /// fails part-way is never left standing as an approval.
    async fn do_submit_review(&self, subject: &Subject, verdict: PullRequestReviewVerdict, body: &str, comments: &[PullRequestReviewCommentDraft]) -> Outcome<()> {
        let (workspace, slug) = repository_segments(&subject.repository)?;
        let pull_request = format!("{}/pullrequests/{}", repository_path(&workspace, &slug), subject.number);
        for comment in comments {
            let (side, line) = review_position(&comment.position);
            let mut inline = serde_json::Map::new();
            inline.insert("path".into(), json!(comment.path.as_str()));
            inline.insert(side.into(), json!(line));
            self.send(reqwest::Method::POST, &format!("{pull_request}/comments"), Some(json!({"content": {"raw": comment.body}, "inline": inline}))).await?;
        }
        if !t3_contracts::trim_wire_string(body).is_empty() {
            self.send(reqwest::Method::POST, &format!("{pull_request}/comments"), Some(json!({"content": {"raw": body}}))).await?;
        }
        match verdict {
            PullRequestReviewVerdict::Approve => self.send(reqwest::Method::POST, &format!("{pull_request}/approve"), None).await?,
            PullRequestReviewVerdict::RequestChanges => self.send(reqwest::Method::POST, &format!("{pull_request}/request-changes"), None).await?,
            PullRequestReviewVerdict::Comment => {}
        }
        Ok(())
    }
}

fn repository_path(workspace: &str, slug: &str) -> String {
    format!("/repositories/{}/{}", encode_uri_component(workspace), encode_uri_component(slug))
}

impl PullRequestProvider for BitbucketPullRequests {
    fn kind(&self) -> SourceControlProviderKind {
        SourceControlProviderKind::Bitbucket
    }

    fn capabilities(&self) -> PullRequestCapabilities {
        capabilities()
    }

    // Bitbucket credentials come from the server's environment rather than a checkout, so the
    // account is the same whichever workspace asks.
    fn viewer<'a>(&'a self, _cwd: &'a str, _host: &'a str) -> BoxFuture<'a, Read<String>> {
        Box::pin(async move { self.viewer_account().await.map_err(Fault::failure) })
    }

    fn list<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<ChangeRequestPage>> {
        Box::pin(async move {
            let (rows, truncated) = self.list_rows(request).await.map_err(Fault::failure)?;
            // Bitbucket is asked for `-updated_on` whether or not it is being carried on from, so
            // every page it answers is one a cursor can continue.
            Ok(ChangeRequestPage { items: rows.iter().map(to_change_request).collect(), truncated, cursor_advance: None, continues: true })
        })
    }

    fn checks<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestChecks>> {
        Box::pin(async move {
            let run = async {
                let (workspace, slug) = repository_segments(&subject.repository)?;
                let path = repository_path(&workspace, &slug);
                let number = subject.number;
                let (pull_request, checks) = tokio::try_join!(
                    Box::pin(self.pull_request(&path, number)),
                    Box::pin(self.item_pages("listChecks", format!("{path}/pullrequests/{number}/statuses?pagelen={CONVERSATION_PAGE_SIZE}"), json::decode_statuses_json, false))
                )?;
                Ok::<_, Fault>(PullRequestChecks { state: pull_request.state, checks })
            };
            run.await.map_err(Fault::failure)
        })
    }

    fn detail<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestDetail>> {
        Box::pin(async move { self.do_detail(subject).await.map_err(Fault::failure) })
    }

    fn activity<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestActivity>> {
        Box::pin(async move { self.do_activity(subject).await.map_err(Fault::failure) })
    }

    fn viewer_permissions<'a>(&'a self, subject: &'a Subject, _include_update_branch: bool) -> BoxFuture<'a, Read<PullRequestViewerPermissions>> {
        Box::pin(async move { self.repository_permission(&subject.repository).await.map(viewer_permissions).map_err(Fault::failure) })
    }

    // `/diff` answers with the whole patch and pages nothing, so the first slice is the last.
    fn diff<'a>(&'a self, subject: &'a Subject, _cursor: Option<&'a str>, commit: Option<&'a str>) -> BoxFuture<'a, Read<PullRequestDiffResult>> {
        Box::pin(async move {
            let (patch, truncated) = self.pull_request_diff(&subject.repository, subject.number, commit).await.map_err(Fault::failure)?;
            Ok(PullRequestDiffResult { patch, truncated, next_cursor: None, omitted_file_stats: None })
        })
    }

    /// What the pull request's head has of each of these paths, as opaque ids, read off the pull
    /// request's own patch, the only place Bitbucket states a file's version. A path the patch does
    /// not carry is answered as the empty revision, and left out altogether when the patch was cut
    /// short at the byte ceiling and so cannot be spoken for. Answers with every file the patch
    /// carries rather than only the paths asked about, since reading one file's version here means
    /// parsing all of them.
    fn file_revisions<'a>(&'a self, subject: &'a Subject, paths: &'a [String]) -> BoxFuture<'a, Read<FileRevisions>> {
        Box::pin(async move {
            if paths.is_empty() {
                return Ok(FileRevisions { revisions: BTreeMap::new(), complete: false });
            }
            let patch = self.parsed_patch(&subject.repository, subject.number).await?;
            let mut revisions = patch.revisions.clone();
            // A patch cut short at the byte ceiling says nothing about the files past the cut, so
            // those paths are left out rather than reported as removed.
            if !patch.truncated {
                for path in paths {
                    revisions.entry(path.clone()).or_default();
                }
            }
            Ok(FileRevisions { revisions, complete: !patch.truncated })
        })
    }

    fn run_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.do_run_action(subject, run).await.map_err(Fault::failure) })
    }

    fn update<'a>(&'a self, subject: &'a Subject, title: Option<&'a str>, body: Option<&'a str>) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let (workspace, slug) = repository_segments(&subject.repository).map_err(Fault::failure)?;
            // Only the words this call rewrites travel in the body: Bitbucket's PUT is a partial
            // update, so any field left out is left as it was; sending `reviewers` back here would
            // overwrite a change another user made to it between this call being issued and the
            // request landing.
            let mut fields = serde_json::Map::new();
            if let Some(title) = title {
                fields.insert("title".into(), json!(title));
            }
            if let Some(body) = body {
                fields.insert("description".into(), json!(body));
            }
            self.send(reqwest::Method::PUT, &format!("{}/pullrequests/{}", repository_path(&workspace, &slug), subject.number), Some(Value::Object(fields))).await.map_err(Fault::failure)
        })
    }

    fn comment<'a>(&'a self, subject: &'a Subject, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let (workspace, slug) = repository_segments(&subject.repository).map_err(Fault::failure)?;
            // A JSON document rather than a form field, so the body stays text whatever it says.
            let url = format!("{}/pullrequests/{}/comments", repository_path(&workspace, &slug), subject.number);
            self.send(reqwest::Method::POST, &url, Some(json!({"content": {"raw": body}}))).await.map_err(Fault::failure)
        })
    }

    // Bitbucket keeps a pull request's remarks and its line comments in the one collection, so this
    // endpoint rewrites either kind.
    fn update_comment<'a>(&'a self, subject: &'a Subject, comment_id: &'a str, _kind: PullRequestCommentUpdateKind, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let (workspace, slug) = repository_segments(&subject.repository).map_err(Fault::failure)?;
            let url = format!("{}/pullrequests/{}/comments/{}", repository_path(&workspace, &slug), subject.number, encode_uri_component(comment_id));
            self.send(reqwest::Method::PUT, &url, Some(json!({"content": {"raw": body}}))).await.map_err(Fault::failure)
        })
    }

    fn submit_review<'a>(&'a self, subject: &'a Subject, verdict: PullRequestReviewVerdict, body: &'a str, comments: &'a [PullRequestReviewCommentDraft]) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.do_submit_review(subject, verdict, body, comments).await.map_err(Fault::failure) })
    }

    fn reviewer_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestReviewerCandidateList>> {
        Box::pin(async move { self.do_reviewer_candidates(subject).await.map_err(Fault::failure) })
    }

    fn set_reviewer_request<'a>(&'a self, subject: &'a Subject, reviewers: &'a [PullRequestReviewerRequestTarget], requested: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.do_set_reviewer_request(subject, reviewers, requested).await.map_err(Fault::failure) })
    }

    fn reply_to_thread<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let (workspace, slug) = repository_segments(&subject.repository).map_err(Fault::failure)?;
            let url = format!("{}/pullrequests/{}/comments", repository_path(&workspace, &slug), subject.number);
            self.send(reqwest::Method::POST, &url, Some(json!({"content": {"raw": body}, "parent": {"id": comment_id_number(thread_id)}}))).await.map_err(Fault::failure)
        })
    }

    // Never called: `capabilities.reactions` is false, and the service refuses without it.
    fn set_reaction<'a>(&'a self, _subject: &'a Subject, _subject_id: Option<&'a str>, _content: PullRequestReactionContent, _reacted: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(PrFailure::failed("Bitbucket does not support reactions.")) })
    }

    // Resolving is a sub-resource that is created and deleted, rather than a field.
    fn set_thread_resolution<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, resolved: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let (workspace, slug) = repository_segments(&subject.repository).map_err(Fault::failure)?;
            let url = format!("{}/pullrequests/{}/comments/{}/resolve", repository_path(&workspace, &slug), subject.number, encode_uri_component(thread_id));
            let method = if resolved { reqwest::Method::POST } else { reqwest::Method::DELETE };
            self.send(method, &url, None).await.map_err(Fault::failure)
        })
    }
}
