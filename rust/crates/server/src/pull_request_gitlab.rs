//! GitLab merge requests behind the pull request provider interface (port of
//! `GitLabPullRequestCli.ts` and `GitLabPullRequestProvider.ts`). Everything goes through the
//! `glab` CLI the source-control module already drives: `glab api` for the REST and GraphQL
//! calls, `glab mr` for the actions the CLI has a command for. The repository and the host come
//! from the checkout, so a self-hosted install works as soon as `glab` is signed in to it. A
//! payload that must never appear in argv (a comment, a description) is piped through stdin.
use crate::{
    pull_request_actions::RunAction,
    pull_request_github::{PrFailure, Reason, Read},
    pull_request_gitlab_json::{self as json, DiffRefs, MergeRequestDetail, MergeRequestListItem},
    pull_request_provider::*,
    source_control_gitlab::{GitLabCli, GitLabCliError},
    source_control_host::Host,
    source_control_uri::encode_uri_component,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::OnceLock, time::Duration};
use t3_contracts::*;
use t3_vcs::VcsProcessOutput;

/// GitLab's own ceiling on `per_page`, so a larger page has to be walked.
const MAX_PAGE_SIZE: usize = 100;
/// Commit history is read one page deep; the rest of a long history stays on GitLab.
const COMMIT_PAGE_SIZE: usize = 100;
/// Pages of the conversation to follow before it is reported as truncated: a thousand notes and a
/// thousand discussions, more than any merge request a person is reading holds.
const CONVERSATION_PAGES: usize = 10;
const DIFF_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const DIFF_TIMEOUT: Duration = Duration::from_secs(60);
/// GitLab charges the blobs query by how many paths it is handed.
const BLOB_PATHS_PER_REQUEST: usize = 100;

// ---- capabilities ---------------------------------------------------------------------------------

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
        ],
        // GitLab offers all three, though a project settles on one; `merge_capabilities` narrows it.
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        // Rebase alone: GitLab has nothing that merges the target back in the way GitHub's update
        // button can, so a request to merge the target in is refused instead of quietly rebasing.
        update_methods: Some(vec![PullRequestUpdateMethod::Rebase]),
        search: true,
        reactions: Some(true),
        // GitLab keeps a reader's viewed files in one browser's local storage, so the marks made
        // here are this environment's own.
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        // No "changes requested": GitLab has approval and unresolved discussions only.
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: true,
            resolve: true,
            verdicts: vec![PullRequestReviewVerdict::Comment, PullRequestReviewVerdict::Approve],
        },
        reviewers: PullRequestReviewerCapabilities { request: true, list_candidates: true },
        edit: Some(PullRequestEditCapabilities { change_request: true, comment: true }),
        stacks: None,
        stack_actions: None,
        labels: None,
    }
}

/// What the signed-in account may do here (`gitLabViewerPermissions`). GitLab answers exactly one
/// of these questions per viewer, `user.can_merge`, which is why merging is the only thing
/// narrowed: the rest stay granted, because withholding them from the author is the worse mistake
/// and GitLab explains any refusal itself.
pub fn viewer_permissions(viewer_can_merge: bool) -> PullRequestViewerPermissions {
    use PullRequestAction::*;
    let capabilities = capabilities();
    // Arming the merge and taking the arming back are the merge, deferred, so they answer to the
    // same `can_merge`; rebasing writes to the source branch, but GitLab reports nothing narrower.
    let merge_actions = [Merge, UpdateBranch, EnableAutoMerge, DisableAutoMerge];
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: capabilities.actions.iter().copied().filter(|action| !merge_actions.contains(action) || viewer_can_merge).collect(),
        comment: true,
        resolve: true,
        verdicts: capabilities.review.verdicts,
        request_reviewers: true,
        update_methods: viewer_can_merge.then(|| vec![PullRequestUpdateMethod::Rebase]),
        labels: None,
    }
}

// ---- failures -------------------------------------------------------------------------------------

/// The CLI tags that mean the tool itself is unusable, rather than one request failing
/// (`gitLabProviderFailure`).
fn cli_failure(error: GitLabCliError) -> PrFailure {
    let reason = match error.tag {
        "GitLabCliUnavailableError" => Reason::MissingTool,
        "GitLabCliAuthenticationError" => Reason::Unauthenticated,
        "GitLabCliRateLimitError" => Reason::RateLimited,
        "GitLabMergeRequestNotFoundError" => Reason::NotFound,
        _ => Reason::Failed,
    };
    PrFailure { reason, detail: error.detail, retry_at: None }
}

/// Names the read that produced unusable output, so a failure reports the call it came from.
fn unreadable(operation: &str) -> PrFailure {
    PrFailure::failed(format!("GitLab CLI returned an unreadable {operation} response."))
}

fn query(params: &[(&str, String)]) -> String {
    params.iter().map(|(key, value)| format!("{key}={}", encode_uri_component(value))).collect::<Vec<_>>().join("&")
}

/// The REST API addresses a project by its URL-encoded full path.
fn project_path(repository: &str) -> String {
    encode_uri_component(repository.trim())
}

fn state_param(state: PullRequestListState) -> &'static str {
    // GitLab's `closed` already excludes merged merge requests, and it spans every state under `all`.
    match state {
        PullRequestListState::Open => "opened",
        PullRequestListState::Closed => "closed",
        PullRequestListState::Merged => "merged",
        PullRequestListState::All => "all",
    }
}

/// A commit sha arrives from the reader and goes straight into a request path, so it is checked
/// rather than trusted: hexadecimal only, from the shortest abbreviation a host prints up to a
/// whole sha.
fn is_commit_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The page a diff cursor names, or none for anything this walk cannot have issued.
fn diff_cursor_page(cursor: &str) -> Option<usize> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"^[1-9][0-9]{0,6}$").unwrap()).is_match(cursor).then(|| cursor.parse().ok()).flatten()
}

struct ListBatch {
    items: Vec<MergeRequestListItem>,
    truncated: bool,
    cursor_advance: usize,
}

pub struct GitLabPullRequests {
    cli: GitLabCli,
}

impl GitLabPullRequests {
    pub fn new(host: Host) -> Self {
        Self { cli: GitLabCli::new(host) }
    }

    /// `glab api <path>`. A raw body goes through stdin: argv is visible in process listings, and
    /// `glab api --input` sends no Content-Type at all, which GitLab answers with HTTP 415.
    async fn api(&self, cwd: &str, path: &str, method: Option<&str>, stdin: Option<String>, max_output_bytes: Option<usize>, timeout: Option<Duration>) -> Read<VcsProcessOutput> {
        let mut args = vec!["api".to_string(), path.to_string()];
        if let Some(method) = method {
            args.extend(["--method".to_string(), method.to_string()]);
        }
        if stdin.is_some() {
            args.extend(["--input", "-", "--header", "Content-Type: application/json"].map(String::from));
        }
        self.cli.execute_output(cwd, args, stdin, timeout, max_output_bytes).await.map_err(cli_failure)
    }

    async fn get(&self, cwd: &str, path: &str) -> Read<String> {
        Ok(self.api(cwd, path, None, None, None, None).await?.stdout)
    }

    async fn send(&self, cwd: &str, path: &str, method: &str, body: Option<Value>) -> Read<()> {
        self.api(cwd, path, Some(method), body.map(|body| body.to_string()), None, None).await.map(|_| ())
    }

    // ---- reads -----------------------------------------------------------------------------------

    async fn viewer_username(&self, cwd: &str) -> Read<String> {
        let stdout = self.get(cwd, "user").await?;
        match json::decode_viewer_json(stdout.trim()) {
            Err(()) => Err(unreadable("getViewerUsername")),
            Ok(None) => Err(PrFailure::failed("GitLab CLI returned no username for the authenticated account.")),
            Ok(Some(username)) => Ok(username),
        }
    }

    /// `per_page` stops at 100, so a larger page is walked one request at a time. The walk is
    /// bounded twice over: it stops on a short page or once the extra row that reveals a next page
    /// has been read, and it never asks for more pages than the caller's page needs, which is what
    /// makes it terminate when every row on a page fails to decode.
    async fn list_merge_requests(&self, request: &ListRequest, repository: &str) -> Read<ListBatch> {
        // A continuation uses GitLab's offset pagination. Its timestamp filter is inclusive and has
        // no tie-breaker, so `delivered` is the stable offset the service has already handed over.
        let delivered = request.cursor.as_ref().map_or(0, |cursor| cursor.delivered as usize);
        let limit = request.limit as usize;
        let per_page = (limit + 1).min(MAX_PAGE_SIZE);
        let first_page = delivered / per_page + 1;
        let last_page = (delivered + limit) / per_page + 1;
        let (mut page, mut collected, mut advance) = (first_page, Vec::<MergeRequestListItem>::new(), 0usize);
        loop {
            let skip = if page == first_page { delivered % per_page } else { 0 };
            let mut params: Vec<(&str, String)> = vec![("state", state_param(request.state).to_string())];
            match request.involvement {
                PullRequestInvolvement::Authored => params.push(("author_username", request.viewer.clone())),
                PullRequestInvolvement::Reviewing => params.push(("reviewer_username", request.viewer.clone())),
                PullRequestInvolvement::All => {}
            }
            // The REST API's own `search`, which matches title and description and travels
            // URL-encoded like every other value, so no text in it can become a parameter.
            if let Some(search) = request.query.as_deref().map(str::trim).filter(|search| !search.is_empty()) {
                params.push(("search", search.to_string()));
            }
            params.extend([("order_by", "updated_at".to_string()), ("sort", "desc".to_string()), ("per_page", per_page.to_string()), ("page", page.to_string())]);
            let stdout = self.get(&request.cwd, &format!("projects/{}/merge_requests?{}", project_path(repository), query(&params))).await?;
            let raw = stdout.trim();
            if raw.is_empty() {
                return Ok(ListBatch { items: collected, truncated: false, cursor_advance: advance });
            }
            let decoded = json::decode_merge_request_list_json(raw).map_err(|()| unreadable("listMergeRequests"))?;
            let mut page_items = vec![];
            let mut page_raw_indexes = vec![];
            for (item, raw_index) in decoded.items.into_iter().zip(decoded.raw_indexes) {
                if raw_index < skip {
                    continue;
                }
                page_items.push(item);
                page_raw_indexes.push(raw_index);
            }
            let remaining = limit.saturating_sub(collected.len());
            if let Some(last_raw_index) = remaining.checked_sub(1).and_then(|at| page_raw_indexes.get(at).copied()) {
                let consumed = last_raw_index + 1 - skip;
                collected.extend(page_items.into_iter().take(remaining));
                return Ok(ListBatch { items: collected, truncated: last_raw_index + 1 < decoded.raw_count || decoded.raw_count == per_page, cursor_advance: advance + consumed });
            }
            collected.extend(page_items);
            let consumed = decoded.raw_count.saturating_sub(skip);
            // Counted before decoding, so a skipped malformed row cannot end paging early.
            if decoded.raw_count < per_page {
                return Ok(ListBatch { items: collected, truncated: false, cursor_advance: advance + consumed });
            }
            if page >= last_page {
                return Ok(ListBatch { items: collected, truncated: true, cursor_advance: advance + consumed });
            }
            advance += consumed;
            page += 1;
        }
    }

    /// The merge request itself, which several calls need for different parts of it. How far
    /// behind the target branch this one is comes only when asked for by name.
    async fn merge_request_detail(&self, cwd: &str, repository: &str, number: u64) -> Read<MergeRequestDetail> {
        let stdout = self.get(cwd, &format!("projects/{}/merge_requests/{number}?{}", project_path(repository), query(&[("include_diverged_commits_count", "true".into())]))).await?;
        json::decode_merge_request_detail_json(stdout.trim()).map_err(|()| unreadable("getMergeRequestDetail"))
    }

    async fn project_merge_capabilities(&self, cwd: &str, repository: &str) -> Read<PullRequestMergeCapabilities> {
        let stdout = self.get(cwd, &format!("projects/{}?license=false", project_path(repository))).await?;
        json::decode_project_merge_capabilities_json(stdout.trim()).map_err(|()| unreadable("getProjectMergeCapabilities"))
    }

    /// The conversation, a page at a time. GitLab pages by offset and reports no total, so a short
    /// raw page is the only thing that says it is done: the notes GitLab wrote itself are dropped,
    /// and a whole page of them still means there is more to read.
    async fn list_notes(&self, cwd: &str, repository: &str, number: u64) -> Read<(Vec<PullRequestComment>, bool)> {
        let mut collected = vec![];
        for page in 1..=CONVERSATION_PAGES {
            let path = format!(
                "projects/{}/merge_requests/{number}/notes?{}",
                project_path(repository),
                query(&[("per_page", MAX_PAGE_SIZE.to_string()), ("page", page.to_string()), ("order_by", "created_at".into()), ("sort", "asc".into())])
            );
            let stdout = self.get(cwd, &path).await?;
            let decoded = json::decode_notes_json(stdout.trim()).map_err(|()| unreadable("listNotes"))?;
            collected.extend(decoded.comments);
            if decoded.raw_count < MAX_PAGE_SIZE {
                return Ok((collected, false));
            }
        }
        Ok((collected, true))
    }

    /// The positioned discussions, walked the same way and stopped by the same bound; the raw count
    /// again, since this endpoint returns the plain notes too.
    async fn list_discussions(&self, cwd: &str, repository: &str, number: u64) -> Read<(Vec<PullRequestReviewThread>, bool)> {
        let mut collected = vec![];
        for page in 1..=CONVERSATION_PAGES {
            let path = format!(
                "projects/{}/merge_requests/{number}/discussions?{}",
                project_path(repository),
                query(&[("per_page", MAX_PAGE_SIZE.to_string()), ("page", page.to_string())])
            );
            let stdout = self.get(cwd, &path).await?;
            let decoded = json::decode_discussions_json(stdout.trim()).map_err(|()| unreadable("listDiscussions"))?;
            collected.extend(decoded.threads);
            if decoded.raw_count < MAX_PAGE_SIZE {
                return Ok((collected, false));
            }
        }
        Ok((collected, true))
    }

    async fn list_commits(&self, cwd: &str, repository: &str, number: u64) -> Read<Vec<PullRequestCommit>> {
        let path = format!(
            "projects/{}/merge_requests/{number}/commits?{}",
            project_path(repository),
            query(&[("per_page", COMMIT_PAGE_SIZE.to_string()), ("with_stats", "true".into())])
        );
        let stdout = self.get(cwd, &path).await?;
        json::decode_commits_json(stdout.trim()).map_err(|()| unreadable("listCommits"))
    }

    /// The awards on the merge request and its notes, a page of notes at a time.
    async fn list_reactions(&self, cwd: &str, repository: &str, number: u64) -> Read<(Vec<PullRequestReaction>, BTreeMap<String, Vec<PullRequestReaction>>)> {
        let mut cursor: Option<String> = None;
        let mut collected: Option<(Vec<PullRequestReaction>, BTreeMap<String, Vec<PullRequestReaction>>)> = None;
        for page in 1..=CONVERSATION_PAGES {
            let body = json!({"query": json::AWARD_EMOJI_GRAPHQL_QUERY, "variables": {"fullPath": repository, "iid": number.to_string(), "cursor": cursor}});
            let stdout = self.api(cwd, "graphql", Some("POST"), Some(body.to_string()), None, None).await?.stdout;
            let decoded = json::decode_award_emoji_json(stdout.trim()).map_err(|()| unreadable("listReactions"))?;
            let held = collected.get_or_insert_with(|| (decoded.reactions.clone(), BTreeMap::new()));
            held.1.extend(decoded.reactions_by_note_id);
            match decoded.next_cursor {
                Some(next) if page < CONVERSATION_PAGES => cursor = Some(next),
                _ => break,
            }
        }
        Ok(collected.unwrap_or_default())
    }

    /// The revisions a positioned comment is written against. GitLab resolves a comment's line
    /// against these three shas, so a review with line comments cannot be sent without them.
    async fn diff_refs(&self, cwd: &str, repository: &str, number: u64) -> Read<DiffRefs> {
        let stdout = self.get(cwd, &format!("projects/{}/merge_requests/{number}", project_path(repository))).await?;
        match json::decode_diff_refs_json(stdout.trim()) {
            Err(()) => Err(unreadable("getDiffRefs")),
            // A merge request with no diff refs is well-formed but cannot carry a positioned comment.
            Ok(None) => Err(PrFailure::failed("The merge request reported no diff revisions.")),
            Ok(Some(refs)) => Ok(refs),
        }
    }

    /// One page of a merge request's files, as a patch that stands on its own. GitLab pages
    /// `/diffs` by offset and has no cursor of its own, so the page number is the cursor.
    async fn diff_page(&self, cwd: &str, repository: &str, number: u64, page: usize, commit: Option<&str>) -> Read<PullRequestDiffResult> {
        let subject = match commit {
            None => format!("merge_requests/{number}/diffs"),
            Some(commit) => format!("repository/commits/{commit}/diff"),
        };
        let path = format!("projects/{}/{subject}?{}", project_path(repository), query(&[("per_page", MAX_PAGE_SIZE.to_string()), ("page", page.to_string())]));
        let output = self.api(cwd, &path, None, None, Some(DIFF_MAX_OUTPUT_BYTES), Some(DIFF_TIMEOUT)).await?;
        // A byte-truncated response is a JSON prefix, so this page cannot be read at all; calling
        // the diff whole would silently drop it and every page after it.
        if output.stdout_truncated {
            return Err(unreadable("getMergeRequestDiff"));
        }
        let decoded = json::decode_merge_request_diffs_json(output.stdout.trim()).map_err(|()| unreadable("getMergeRequestDiff"))?;
        // Counted before decoding, so a page whose files all failed to decode still moves on.
        let more_pages = decoded.raw_count >= MAX_PAGE_SIZE;
        Ok(PullRequestDiffResult {
            // The slice ends on a newline, so a file GitLab gave a header and no hunks for does not
            // run into the first line of the next slice.
            patch: if decoded.patch.is_empty() { decoded.patch } else { json::ends_with_newline(&decoded.patch) },
            truncated: decoded.truncated,
            next_cursor: more_pages.then(|| (page + 1).to_string()),
            omitted_file_stats: None,
        })
    }

    async fn blobs_at(&self, cwd: &str, repository: &str, reference: &str, paths: &[String]) -> Read<Option<BTreeMap<String, String>>> {
        let body = json!({"query": json::REPOSITORY_BLOBS_GRAPHQL_QUERY, "variables": {"fullPath": repository, "ref": reference, "paths": paths}});
        let stdout = self.api(cwd, "graphql", Some("POST"), Some(body.to_string()), None, None).await?.stdout;
        json::decode_repository_blobs_json(stdout.trim()).map_err(|()| unreadable("getFileRevisions"))
    }

    /// What the merge request's head has of each of these paths, as blob ids. The head sha comes
    /// from the merge request's own diff refs, so the answer is the version a reader is looking at.
    /// A path the head does not have is the empty revision, since the batch it was asked in was
    /// looked at; a batch GitLab did not answer for is left out instead.
    async fn file_revisions_at(&self, cwd: &str, repository: &str, number: u64, paths: &[String]) -> Read<BTreeMap<String, String>> {
        if paths.is_empty() {
            return Ok(BTreeMap::new());
        }
        let refs = self.diff_refs(cwd, repository, number).await?;
        let batches: Vec<&[String]> = paths.chunks(BLOB_PATHS_PER_REQUEST).collect();
        let mut revisions = BTreeMap::new();
        // Two requests at a time, as the original reads them.
        for pair in batches.chunks(2) {
            let first = Box::pin(self.blobs_at(cwd, repository, &refs.head_sha, pair[0]));
            let second = async {
                match pair.get(1) {
                    Some(batch) => Some(Box::pin(self.blobs_at(cwd, repository, &refs.head_sha, batch)).await),
                    None => None,
                }
            };
            let (first, second) = tokio::join!(first, second);
            for (batch, page) in pair.iter().zip([Some(first), second]) {
                let Some(page) = page else { continue };
                // A batch GitLab did not answer says nothing about its paths, so they are left out
                // and the caller reads them as versions it could not learn.
                let Some(found) = page? else { continue };
                revisions.extend(found);
                for path in *batch {
                    revisions.entry(path.clone()).or_insert_with(String::new);
                }
            }
        }
        Ok(revisions)
    }

    async fn project_users(&self, cwd: &str, repository: &str) -> Read<json::ProjectUsers> {
        let stdout = self.get(cwd, &format!("projects/{}/users?{}", project_path(repository), query(&[("per_page", MAX_PAGE_SIZE.to_string())]))).await?;
        json::decode_project_users_json(stdout.trim()).map_err(|()| unreadable("listReviewerCandidates"))
    }

    fn award_subject_path(repository: &str, number: u64, note_id: Option<&str>) -> String {
        let merge_request = format!("projects/{}/merge_requests/{number}", project_path(repository));
        match note_id {
            None => format!("{merge_request}/award_emoji"),
            Some(note) => format!("{merge_request}/notes/{}/award_emoji", encode_uri_component(note)),
        }
    }

    // ---- writes ----------------------------------------------------------------------------------

    async fn do_set_reaction(&self, subject: &Subject, note_id: Option<&str>, content: PullRequestReactionContent, reacted: bool) -> Read<()> {
        let path = Self::award_subject_path(&subject.repository, subject.number, note_id);
        if reacted {
            return self.send(&subject.cwd, &format!("{path}?{}", query(&[("name", json::award_name(content).into())])), "POST", None).await;
        }
        // GitLab deletes an award by its id and takes no emoji name there, so the reader's own
        // award of that name is looked up first. Nothing to delete is success: the reaction the
        // caller asked to take back is already gone.
        let viewer = self.viewer_username(&subject.cwd).await?;
        let listed = self.get(&subject.cwd, &path).await?;
        let own = json::decode_own_award_id_json(listed.trim(), content, &viewer).map_err(|()| unreadable("setReaction"))?;
        match own {
            None => Ok(()),
            Some(id) => self.send(&subject.cwd, &format!("{path}/{id}"), "DELETE", None).await,
        }
    }

    async fn do_run_action(&self, subject: &Subject, run: &RunAction) -> Read<()> {
        let (cwd, repository, number) = (&subject.cwd, &subject.repository, subject.number);
        // `glab mr merge` arms auto-merge and never disarms it, so the one direction the CLI has no
        // flag for is asked of GitLab directly through the same `api` passthrough.
        if run.action == PullRequestAction::DisableAutoMerge {
            return self.send(cwd, &format!("projects/{}/merge_requests/{number}/cancel_merge_when_pipeline_succeeds", project_path(repository)), "POST", None).await;
        }
        let method_flags: Vec<&str> = match run.merge_method {
            Some(PullRequestMergeMethod::Squash) => vec!["--squash"],
            Some(PullRequestMergeMethod::Rebase) => vec!["--rebase"],
            _ => vec![],
        };
        let (subcommand, flags): (&str, Vec<&str>) = match run.action {
            // glab turns on auto-merge whenever a pipeline is running. The button means merge now.
            PullRequestAction::Merge => ("merge", ["--auto-merge=false", "--yes"].into_iter().chain(method_flags).collect()),
            // The same command with the flag the other way up: here the wait is the whole point.
            PullRequestAction::EnableAutoMerge => ("merge", ["--auto-merge=true", "--yes"].into_iter().chain(method_flags).collect()),
            PullRequestAction::Ready => ("update", vec!["--ready"]),
            PullRequestAction::Draft => ("update", vec!["--draft"]),
            PullRequestAction::Close => ("close", vec![]),
            // A rebase, because GitLab has no other way to move a branch onto its target.
            PullRequestAction::UpdateBranch => ("rebase", vec![]),
            PullRequestAction::Reopen => ("reopen", vec![]),
            // Never reached: this host does not declare the action, so the service refuses it first.
            PullRequestAction::DisableAutoMerge | PullRequestAction::Revert | PullRequestAction::ApproveWorkflows => {
                return Err(PrFailure::failed(format!("GitLab merge request action {} is unsupported", serde_json::to_value(run.action).ok().and_then(|name| name.as_str().map(str::to_string)).unwrap_or_default())));
            }
        };
        let mut args = vec!["mr".to_string(), subcommand.to_string(), number.to_string(), "--repo".to_string(), repository.to_string()];
        args.extend(flags.into_iter().map(String::from));
        self.cli.execute_output(cwd, args, None, None, None).await.map(|_| ()).map_err(cli_failure)
    }

    async fn do_submit_review(&self, subject: &Subject, verdict: PullRequestReviewVerdict, body: &str, comments: &[PullRequestReviewCommentDraft]) -> Read<()> {
        let merge_request = format!("projects/{}/merge_requests/{}", project_path(&subject.repository), subject.number);
        // GitLab has no pending review to attach comments to, so a review is replayed as the
        // requests it is made of: the line comments, then the summary, then the verdict. A failure
        // part-way leaves what was already posted in place, which is why the verdict goes last: a
        // half-sent review is never an approval.
        if !comments.is_empty() {
            let refs = self.diff_refs(&subject.cwd, &subject.repository, subject.number).await?;
            for comment in comments {
                let mut position = serde_json::Map::new();
                position.insert("base_sha".into(), json!(refs.base_sha));
                position.insert("head_sha".into(), json!(refs.head_sha));
                position.insert("start_sha".into(), json!(refs.start_sha));
                position.insert("position_type".into(), json!("text"));
                // Both paths are sent because GitLab resolves a position against both sides of the
                // diff; they differ only for a renamed file.
                position.insert("old_path".into(), json!(comment.old_path.as_ref().unwrap_or(&comment.path).as_str()));
                position.insert("new_path".into(), json!(comment.path.as_str()));
                match &comment.position {
                    PullRequestReviewPosition::Added { new_line } => {
                        position.insert("new_line".into(), json!(new_line));
                    }
                    PullRequestReviewPosition::Deleted { old_line } => {
                        position.insert("old_line".into(), json!(old_line));
                    }
                    PullRequestReviewPosition::Context { old_line, new_line, .. } => {
                        position.insert("old_line".into(), json!(old_line));
                        position.insert("new_line".into(), json!(new_line));
                    }
                }
                self.send(&subject.cwd, &format!("{merge_request}/discussions"), "POST", Some(json!({"body": comment.body, "position": position}))).await?;
            }
        }
        if !body.trim().is_empty() {
            self.send(&subject.cwd, &format!("{merge_request}/notes"), "POST", Some(json!({"body": body}))).await?;
        }
        if verdict == PullRequestReviewVerdict::Approve {
            self.send(&subject.cwd, &format!("{merge_request}/approve"), "POST", None).await?;
        }
        Ok(())
    }

    async fn do_set_reviewer_request(&self, subject: &Subject, reviewers: &[PullRequestReviewerRequestTarget], requested: bool) -> Read<()> {
        let detail = self.merge_request_detail(&subject.cwd, &subject.repository, subject.number).await?;
        // GitLab has no endpoint that adds or removes one reviewer: `reviewer_ids` replaces the
        // whole set, so the set that is already there is read first and the change applied to it.
        // Asking again for somebody already on it writes the same set back, which is how GitLab
        // re-requests a review.
        let mut ids: Vec<i64> = vec![];
        for id in detail.reviewer_ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        for reviewer in reviewers {
            // A candidate GitLab did not name is not an id it would accept, and sending it would
            // rewrite the reviewer set around a number nobody chose.
            let Some(id) = js_safe_positive_integer(reviewer.id.as_str()) else { continue };
            if requested {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            } else {
                ids.retain(|held| *held != id);
            }
        }
        self.send(&subject.cwd, &format!("projects/{}/merge_requests/{}", project_path(&subject.repository), subject.number), "PUT", Some(json!({"reviewer_ids": ids}))).await
    }
}

/// `Number(text)` when it is a safe integer above zero, as the reviewer ids are checked.
fn js_safe_positive_integer(text: &str) -> Option<i64> {
    let text = text.trim();
    let number = if text.is_empty() {
        0.0
    } else if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()? as f64
    } else if let Some(binary) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        i64::from_str_radix(binary, 2).ok()? as f64
    } else if let Some(octal) = text.strip_prefix("0o").or_else(|| text.strip_prefix("0O")) {
        i64::from_str_radix(octal, 8).ok()? as f64
    } else if text.chars().any(|c| c.is_alphabetic() && !matches!(c, 'e' | 'E')) {
        return None;
    } else {
        text.parse::<f64>().ok()?
    };
    (number.is_finite() && number.fract() == 0.0 && number > 0.0 && number <= 9_007_199_254_740_991.0).then_some(number as i64)
}

fn to_change_request(item: &MergeRequestListItem) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number: item.number,
        title: item.title.clone(),
        url: item.url.clone(),
        author: item.author.clone(),
        head_branch: item.head_branch.clone(),
        head_repository_name_with_owner: None,
        base_branch: item.base_branch.clone(),
        state: item.state,
        is_draft: item.is_draft,
        mergeability: item.mergeability,
        additions: item.additions,
        deletions: item.deletions,
        created_at: item.created_at.clone(),
        updated_at: item.updated_at.clone(),
        review_request_logins: item.review_request_logins.clone(),
        labels: item.labels.clone(),
        // GitLab summarises neither reviews nor checks on a listing.
        review_decision: None,
        checks_state: None,
    }
}

impl PullRequestProvider for GitLabPullRequests {
    fn kind(&self) -> SourceControlProviderKind {
        SourceControlProviderKind::Gitlab
    }

    fn capabilities(&self) -> PullRequestCapabilities {
        capabilities()
    }

    fn viewer<'a>(&'a self, cwd: &'a str, _host: &'a str) -> BoxFuture<'a, Read<String>> {
        Box::pin(self.viewer_username(cwd))
    }

    fn list<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<ChangeRequestPage>> {
        Box::pin(async move {
            let repository = request.repositories.first().cloned().unwrap_or_default();
            let batch = self.list_merge_requests(request, &repository).await?;
            // GitLab is asked for its merge requests by update, newest first, whether or not it is
            // being carried on from, so every page it answers is one a cursor can continue.
            Ok(ChangeRequestPage { items: batch.items.iter().map(to_change_request).collect(), truncated: batch.truncated, cursor_advance: Some(batch.cursor_advance), continues: true })
        })
    }

    fn checks<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestChecks>> {
        Box::pin(async move {
            let detail = self.merge_request_detail(&subject.cwd, &subject.repository, subject.number).await?;
            Ok(PullRequestChecks { state: detail.item.state, checks: detail.checks })
        })
    }

    fn detail<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestDetail>> {
        Box::pin(async move {
            // Two reads at once; the first to fail ends the other.
            let (detail, merge_capabilities) = tokio::try_join!(
                Box::pin(self.merge_request_detail(&subject.cwd, &subject.repository, subject.number)),
                Box::pin(self.project_merge_capabilities(&subject.cwd, &subject.repository))
            )?;
            let viewer_permissions = viewer_permissions(detail.viewer_can_merge);
            // A GitLab too old to count the divergence says nothing here rather than "up to date":
            // the banner is worth missing, and a wrong all-clear is not worth showing.
            let (base_comparison, behind_by) = match detail.diverged_commits {
                None => (PullRequestBaseComparison::Unknown, None),
                Some(diverged) if diverged > 0 => (PullRequestBaseComparison::Behind, Some(diverged.max(0) as u64)),
                Some(diverged) => (PullRequestBaseComparison::UpToDate, Some(diverged.max(0) as u64)),
            };
            Ok(ChangeRequestDetail {
                row: to_change_request(&detail.item),
                head_sha: None,
                body: detail.body,
                changed_files: detail.changed_files,
                merged_at: detail.merged_at,
                closed_at: detail.closed_at,
                reviewers: detail.reviewers,
                checks: detail.checks,
                merge_capabilities,
                viewer_permissions,
                base_comparison: Some(base_comparison),
                behind_by,
                auto_merge_enabled: detail.auto_merge_enabled,
                auto_merge_method: detail.auto_merge_method,
                workflow_approvals_required: None,
            })
        })
    }

    fn activity<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestActivity>> {
        Box::pin(async move {
            let (cwd, repository, number) = (subject.cwd.as_str(), subject.repository.as_str(), subject.number);
            // A failed read costs the conversation that part rather than the whole page: the
            // original degrades each of the four to an empty, truncated answer.
            let (notes, commits, discussions, awards) = tokio::join!(
                Box::pin(self.list_notes(cwd, repository, number)),
                Box::pin(self.list_commits(cwd, repository, number)),
                Box::pin(self.list_discussions(cwd, repository, number)),
                // The notes endpoint carries no award of any kind, so they are read alongside it.
                Box::pin(self.list_reactions(cwd, repository, number))
            );
            let (notes, notes_truncated) = notes.unwrap_or((vec![], true));
            let commits = commits.unwrap_or_default();
            let (threads, threads_truncated) = discussions.unwrap_or((vec![], true));
            let (reactions, by_note) = awards.unwrap_or_default();
            let with_reactions = |id: &str| Some(by_note.get(id).cloned().unwrap_or_default());
            Ok(PullRequestActivity {
                author: None,
                reviewers: None,
                // GitLab reports no count of its own, so the walk's own total is the host's: the
                // notes endpoint carries every comment, including the ones under a discussion.
                comment_count: notes.len() as u64,
                comments: notes.into_iter().map(|comment| PullRequestComment { reactions: with_reactions(&comment.id), ..comment }).collect(),
                comments_truncated: notes_truncated || threads_truncated,
                review_threads_truncated: None,
                review_threads: threads
                    .into_iter()
                    .map(|thread| PullRequestReviewThread {
                        comments: thread.comments.into_iter().map(|comment| PullRequestThreadComment { reactions: with_reactions(&comment.id), ..comment }).collect(),
                        ..thread
                    })
                    .collect(),
                commits,
                reactions: Some(reactions),
            })
        })
    }

    // The same read the detail takes it from, on its own: `user.can_merge` lives on the merge
    // request, so there is no cheaper thing to ask GitLab.
    fn viewer_permissions<'a>(&'a self, subject: &'a Subject, _include_update_branch: bool) -> BoxFuture<'a, Read<PullRequestViewerPermissions>> {
        Box::pin(async move {
            let detail = self.merge_request_detail(&subject.cwd, &subject.repository, subject.number).await?;
            Ok(viewer_permissions(detail.viewer_can_merge))
        })
    }

    fn diff<'a>(&'a self, subject: &'a Subject, cursor: Option<&'a str>, commit: Option<&'a str>) -> BoxFuture<'a, Read<PullRequestDiffResult>> {
        Box::pin(async move {
            if commit.is_some_and(|commit| !is_commit_sha(commit)) {
                return Err(PrFailure::failed("The named commit was not a commit sha."));
            }
            let page = match cursor {
                None => 1,
                Some(cursor) => diff_cursor_page(cursor).ok_or_else(|| PrFailure::failed("The diff cursor was not one this merge request handed out."))?,
            };
            self.diff_page(&subject.cwd, &subject.repository, subject.number, page, commit).await
        })
    }

    fn file_revisions<'a>(&'a self, subject: &'a Subject, paths: &'a [String]) -> BoxFuture<'a, Read<FileRevisions>> {
        Box::pin(async move { Ok(FileRevisions { revisions: self.file_revisions_at(&subject.cwd, &subject.repository, subject.number, paths).await?, complete: false }) })
    }

    fn run_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.do_run_action(subject, run))
    }

    fn update<'a>(&'a self, subject: &'a Subject, title: Option<&'a str>, body: Option<&'a str>) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            // Only the fields the caller asked to change: GitLab leaves out what it is not sent and
            // clears what it is sent empty, so a title corrected on its own carries no description.
            let mut fields = serde_json::Map::new();
            if let Some(title) = title {
                fields.insert("title".into(), json!(title));
            }
            if let Some(body) = body {
                fields.insert("description".into(), json!(body));
            }
            self.send(&subject.cwd, &format!("projects/{}/merge_requests/{}", project_path(&subject.repository), subject.number), "PUT", Some(Value::Object(fields))).await
        })
    }

    fn comment<'a>(&'a self, subject: &'a Subject, body: &'a str) -> BoxFuture<'a, Read<()>> {
        // A JSON body rather than a `--raw-field`: glab coerces a field that reads as a literal
        // `true` or a number, and a comment body is text either way.
        Box::pin(async move { self.send(&subject.cwd, &format!("projects/{}/merge_requests/{}/notes", project_path(&subject.repository), subject.number), "POST", Some(json!({"body": body}))).await })
    }

    // The kind is not read: every comment this provider hands out, positioned or not, carries a
    // plain REST note id, and one endpoint rewrites both.
    fn update_comment<'a>(&'a self, subject: &'a Subject, comment_id: &'a str, _kind: PullRequestCommentUpdateKind, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let path = format!("projects/{}/merge_requests/{}/notes/{}", project_path(&subject.repository), subject.number, encode_uri_component(comment_id));
            self.send(&subject.cwd, &path, "PUT", Some(json!({"body": body}))).await
        })
    }

    fn submit_review<'a>(&'a self, subject: &'a Subject, verdict: PullRequestReviewVerdict, body: &'a str, comments: &'a [PullRequestReviewCommentDraft]) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.do_submit_review(subject, verdict, body, comments))
    }

    // Users only: GitLab requests a review of a person, and the groups that can stand in for one
    // appear in approval rules rather than in a merge request's reviewers.
    fn reviewer_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestReviewerCandidateList>> {
        Box::pin(async move {
            let (detail, users) = tokio::try_join!(
                Box::pin(self.merge_request_detail(&subject.cwd, &subject.repository, subject.number)),
                Box::pin(self.project_users(&subject.cwd, &subject.repository))
            )?;
            let author = detail.item.author.as_ref().map(|author| author.login.as_str().to_string());
            let requested = &detail.item.review_request_logins;
            Ok(PullRequestReviewerCandidateList {
                // The author is dropped rather than shown unusable: GitLab refuses to make the
                // person who opened a merge request its reviewer.
                candidates: users
                    .candidates
                    .into_iter()
                    .filter(|candidate| Some(&candidate.login) != author.as_ref())
                    .map(|candidate| PullRequestReviewerCandidate { is_requested: requested.contains(&candidate.login), ..candidate })
                    .collect(),
                truncated: users.raw_count >= MAX_PAGE_SIZE,
            })
        })
    }

    fn set_reviewer_request<'a>(&'a self, subject: &'a Subject, reviewers: &'a [PullRequestReviewerRequestTarget], requested: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.do_set_reviewer_request(subject, reviewers, requested))
    }

    fn reply_to_thread<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let path = format!("projects/{}/merge_requests/{}/discussions/{}/notes", project_path(&subject.repository), subject.number, encode_uri_component(thread_id));
            self.send(&subject.cwd, &path, "POST", Some(json!({"body": body}))).await
        })
    }

    fn set_reaction<'a>(&'a self, subject: &'a Subject, subject_id: Option<&'a str>, content: PullRequestReactionContent, reacted: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.do_set_reaction(subject, subject_id, content, reacted))
    }

    fn set_thread_resolution<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, resolved: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let path = format!("projects/{}/merge_requests/{}/discussions/{}", project_path(&subject.repository), subject.number, encode_uri_component(thread_id));
            self.send(&subject.cwd, &path, "PUT", Some(json!({"resolved": resolved}))).await
        })
    }
}
