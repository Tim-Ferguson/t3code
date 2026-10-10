//! Azure DevOps pull requests behind the pull request provider interface (port of
//! `AzureDevOpsPullRequestCli.ts` and `AzureDevOpsPullRequestProvider.ts`). Everything goes
//! through the `az` CLI the source-control module already drives: `az repos pr` for the pull
//! request itself and its actions, `az devops invoke` for the REST routes `az repos` has no command
//! for (threads, iterations, the changes of an iteration, file contents). The organisation, project
//! and repository come from the checkout (`--detect true`). Azure serves no patch, so the one the
//! Code tab reads is built here out of the files an iteration changed and both sides of each.
//!
//! What this host cannot do is declared in `capabilities`: it has no search, no reactions, no
//! labels, and nothing to write a remark with, so the page hides all of them. Its reviewers can be
//! requested by name but not listed.
use crate::{
    pull_request_actions::RunAction,
    pull_request_azure_diff::{self as patch, DiffCursor, FileTexts},
    pull_request_azure_json::{self as json, AzurePullRequest, ChangeEntry, ChangePage, ItemContent, Iteration, Location},
    pull_request_github::{PrFailure, Reason, Read},
    pull_request_provider::*,
    source_control_azure_devops::{AzureDevOpsCli, AzureDevOpsCliError},
    source_control_host::Host,
};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use std::sync::Mutex;
use t3_contracts::*;
use tokio::sync::Semaphore;

/// Every request is `az` with the REST version pinned, so a new default cannot reshape a response.
const REST_API_VERSION: &str = "7.1";
const PULL_REQUEST_LIST_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// A full page of change entries is two thousand files, past the megabyte a read is given by default.
const CHANGE_ENTRIES_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Four times the megabyte of file the other hosts hand over: the file arrives inside a JSON
/// envelope, escaped if it is text and base64 if it is not.
const ITEM_CONTENT_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Neither the threads nor the iterations page, so each answers with the whole of it at once.
const REVIEW_HISTORY_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Azure's own ceiling for one page of an iteration's changes.
const CHANGE_ENTRIES_PER_PAGE: i64 = 2000;
/// Where following the pages stops, counted in the entries Azure was asked to skip.
const MAX_CHANGE_ENTRIES: i64 = 10_000;
/// How many of a slice's files are read at once: every file is two `az` invocations.
const DIFF_FILE_CONCURRENCY: usize = 4;
/// How many `az` processes the diff reads will have out at once, counted across every reader.
pub const MAX_DIFF_SPAWNS: usize = 2 * DIFF_FILE_CONCURRENCY;
/// How many pull requests' repository locations one provider remembers at once.
pub const LOCATION_CACHE_CAPACITY: usize = 128;

// ---- capabilities ---------------------------------------------------------------------------------

pub fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        // Azure serves no patch of its own, so the one the Code tab reads is built here.
        diff: true,
        // Posting a remark is not something this can claim without having run it.
        comment: false,
        actions: vec![
            PullRequestAction::Merge,
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
        ],
        // Azure squashes as a completion option; it has no rebase strategy of its own.
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash],
        update_methods: None,
        // `az repos pr list` filters by status, creator, reviewer and branch, and by no text.
        search: false,
        reactions: Some(false),
        // Azure keeps a viewed record of its own only behind an undocumented endpoint keyed on an
        // iteration, so the marks are kept in this environment.
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        review: PullRequestReviewCapabilities { inline_comment: false, reply: false, resolve: false, verdicts: vec![] },
        // Nothing in `az repos` lists the identities a repository could name, so a reviewer is
        // requested by name and the page offers no menu.
        reviewers: PullRequestReviewerCapabilities { request: true, list_candidates: false },
        edit: Some(PullRequestEditCapabilities { change_request: true, comment: false }),
        stacks: None,
        stack_actions: None,
        labels: None,
    }
}

/// Everything this host offers, granted to whoever is signed in: Azure states no permission
/// anywhere `az repos pr show` or `list` reach, so a viewer who may not take an action is told so
/// by Azure at the moment they try.
pub fn viewer_permissions() -> PullRequestViewerPermissions {
    let capabilities = capabilities();
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: capabilities.actions,
        comment: capabilities.comment,
        resolve: capabilities.review.resolve,
        verdicts: capabilities.review.verdicts,
        request_reviewers: capabilities.reviewers.request,
        update_methods: None,
        labels: None,
    }
}

// ---- failures -------------------------------------------------------------------------------------

/// The CLI tags that mean the tool itself is unusable, rather than one request failing
/// (`azureDevOpsProviderFailure`).
fn cli_failure(error: AzureDevOpsCliError) -> PrFailure {
    let reason = match error.tag {
        "AzureDevOpsCliUnavailableError" => Reason::MissingTool,
        "AzureDevOpsCliAuthenticationError" => Reason::Unauthenticated,
        "AzureDevOpsCliRateLimitError" => Reason::RateLimited,
        "AzureDevOpsPullRequestNotFoundError" => Reason::NotFound,
        _ => Reason::Failed,
    };
    PrFailure { reason, detail: error.detail, retry_at: None }
}

/// Names the read that produced unusable output, so a failure reports the call it came from.
fn unreadable(operation: &str) -> PrFailure {
    PrFailure::failed(format!("Azure CLI returned an unreadable {operation} response."))
}

/// What the capabilities already say this host cannot do.
fn cannot_write() -> PrFailure {
    PrFailure::failed("Azure DevOps reviews cannot be written from here yet.")
}

/// A failure that is one file's problem rather than the read's: a signed-out CLI, a rate limit or
/// no `az` at all belongs to the caller, which pauses the host.
fn is_file_problem(failure: &PrFailure) -> bool {
    matches!(failure.reason, Reason::NotFound | Reason::Failed)
}

// ---- the commands ---------------------------------------------------------------------------------

fn status_args(state: PullRequestListState) -> [&'static str; 2] {
    match state {
        PullRequestListState::Open => ["--status", "active"],
        PullRequestListState::Merged => ["--status", "completed"],
        PullRequestListState::Closed => ["--status", "abandoned"],
        PullRequestListState::All => ["--status", "all"],
    }
}

/// Azure moves a pull request by setting its state rather than by named commands: completing it is
/// the merge, abandoning it is the close, and reactivating it is the reopen.
fn action_args(action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>) -> Result<Vec<String>, PrFailure> {
    let squash = |method: PullRequestMergeMethod| if method == PullRequestMergeMethod::Squash { "true" } else { "false" };
    let args: Vec<&str> = match action {
        PullRequestAction::Merge => vec!["--status", "completed", "--squash", squash_or_false(merge_method, squash)],
        // Auto-complete is Azure's own name for it; the squash choice is stored with it.
        PullRequestAction::EnableAutoMerge => {
            let mut args = vec!["--auto-complete", "true"];
            if let Some(method) = merge_method {
                args.extend(["--squash", squash(method)]);
            }
            args
        }
        PullRequestAction::DisableAutoMerge => vec!["--auto-complete", "false"],
        PullRequestAction::Ready => vec!["--draft", "false"],
        PullRequestAction::Draft => vec!["--draft", "true"],
        PullRequestAction::Close => vec!["--status", "abandoned"],
        PullRequestAction::UpdateBranch => vec![],
        PullRequestAction::Reopen => vec!["--status", "active"],
        PullRequestAction::Revert | PullRequestAction::ApproveWorkflows => {
            let name = serde_json::to_value(action).ok().and_then(|name| name.as_str().map(str::to_string)).unwrap_or_default();
            return Err(PrFailure::failed(format!("Azure DevOps pull request action {name} is unsupported")));
        }
    };
    Ok(args.into_iter().map(String::from).collect())
}

fn squash_or_false(method: Option<PullRequestMergeMethod>, squash: impl Fn(PullRequestMergeMethod) -> &'static str) -> &'static str {
    method.map_or("false", squash)
}

/// A reviewer Azure could be given: an email address, a display name or an identity guid, and
/// nothing that starts with a dash — these are argv, and a value that looks like a flag stops being
/// a value.
fn is_reviewer_name(value: &str) -> bool {
    let name = t3_contracts::trim_wire_string(value);
    !name.is_empty() && !name.starts_with('-')
}

/// Azure names its own items with a leading slash, which the repository paths carried around here
/// have had taken off.
fn to_item_path(path: &str) -> String {
    if path.starts_with('/') { path.to_string() } else { format!("/{path}") }
}

fn repository_route(location: &Location) -> Vec<String> {
    vec![format!("project={}", location.project), format!("repositoryId={}", location.repository)]
}

fn pull_request_route(location: &Location, number: u64) -> Vec<String> {
    let mut route = repository_route(location);
    route.push(format!("pullRequestId={number}"));
    route
}

/// What an iteration changed, and whether following its pages reached the end of it.
pub struct IterationChanges {
    pub changes: Vec<ChangeEntry>,
    pub truncated: bool,
}

struct ListPage {
    items: Vec<AzurePullRequest>,
    truncated: bool,
    cursor_advance: usize,
}

/// What is held across requests, once per host (`Shared::azure`): the build's whole allowance of
/// `az` processes for diff reads, and where each pull request's repository lives.
pub struct AzureShared {
    spawns: Semaphore,
    /// Where a pull request's repository lives, which is the route every other read of it needs and
    /// the one thing only the pull request states. A pull request cannot move between
    /// repositories, so it is remembered; least recently used, so a listing walking cold pull
    /// requests cannot evict the one being read.
    locations: Mutex<IndexMap<String, Location>>,
}

impl Default for AzureShared {
    fn default() -> Self {
        Self { spawns: Semaphore::new(MAX_DIFF_SPAWNS), locations: Mutex::new(IndexMap::new()) }
    }
}

pub struct AzurePullRequests {
    cli: AzureDevOpsCli,
    host: Host,
}

impl AzurePullRequests {
    pub fn new(host: Host) -> Self {
        Self { cli: AzureDevOpsCli::new(host.clone()), host }
    }

    fn shared(&self) -> &AzureShared {
        &self.host.shared.azure
    }

    /// `az <args> --only-show-errors --output json`.
    async fn execute_json(&self, cwd: &str, mut args: Vec<String>, max_output_bytes: Option<usize>) -> Read<String> {
        args.extend(["--only-show-errors", "--output", "json"].map(String::from));
        self.cli.run(cwd, args, max_output_bytes).await.map_err(cli_failure)
    }

    /// A REST route reached through `az devops invoke`, which addresses it by area, resource and
    /// route parameters rather than by URL. Used in place of `az rest` because it signs in the way
    /// the azure-devops extension does.
    async fn invoke<T>(
        &self,
        cwd: &str,
        operation: &str,
        resource: &str,
        route_parameters: Vec<String>,
        query_parameters: Option<Vec<String>>,
        max_output_bytes: usize,
        decode: impl FnOnce(&str) -> Result<T, ()>,
    ) -> Read<T> {
        let mut args: Vec<String> = ["devops", "invoke", "--detect", "true", "--area", "git", "--resource", resource, "--api-version", REST_API_VERSION, "--route-parameters"].map(String::from).to_vec();
        args.extend(route_parameters);
        if let Some(query) = query_parameters {
            args.push("--query-parameters".into());
            args.extend(query);
        }
        let stdout = self.execute_json(cwd, args, Some(max_output_bytes)).await?;
        decode(stdout.trim()).map_err(|()| unreadable(operation))
    }

    // ---- reads -----------------------------------------------------------------------------------

    async fn get_viewer(&self, cwd: &str) -> Read<String> {
        let stdout = self.execute_json(cwd, ["account", "show", "--query", "user"].map(String::from).to_vec(), None).await?;
        // `--query user` narrows the payload to the account, so it is nested back under the key
        // the decoder reads to keep one shape for the signed-in user.
        let trimmed = stdout.trim();
        let wrapped = format!("{{\"user\":{}}}", if trimmed.is_empty() { "null" } else { trimmed });
        match json::decode_viewer_json(&wrapped) {
            Err(()) => Err(unreadable("getViewer")),
            Ok(None) => Err(PrFailure::failed("Azure CLI returned no account for the current sign-in.")),
            Ok(Some(account)) => Ok(account),
        }
    }

    /// Azure pages by raw offset. Keep reading when malformed rows leave the decoded page short, and
    /// retain the raw count so the next public cursor skips every row this walk consumed.
    async fn list_pull_request_page(&self, request: &ListRequest, repository: &str) -> Read<ListPage> {
        let (limit, mut skip) = (request.limit as usize, request.cursor.as_ref().map_or(0, |cursor| cursor.delivered as usize));
        let (mut cursor_advance, mut items) = (0usize, Vec::<AzurePullRequest>::new());
        loop {
            let remaining = limit - items.len();
            let top = remaining + 1;
            let mut args: Vec<String> = ["repos", "pr", "list", "--detect", "true", "--repository"].map(String::from).to_vec();
            args.push(repository.to_string());
            args.extend(status_args(request.state).map(String::from));
            match request.involvement {
                PullRequestInvolvement::Authored => args.extend(["--creator".to_string(), request.viewer.clone()]),
                PullRequestInvolvement::Reviewing => args.extend(["--reviewer".to_string(), request.viewer.clone()]),
                PullRequestInvolvement::All => {}
            }
            // A web link per row, which is the only url that needs no assembling.
            args.push("--include-links".into());
            if skip != 0 {
                args.extend(["--skip".to_string(), skip.to_string()]);
            }
            args.extend(["--top".to_string(), top.to_string()]);
            let stdout = self.execute_json(&request.cwd, args, Some(PULL_REQUEST_LIST_MAX_OUTPUT_BYTES)).await?;
            let raw = stdout.trim();
            if raw.is_empty() {
                return Ok(ListPage { items, truncated: false, cursor_advance });
            }
            let decoded = json::decode_pull_request_list_json(raw).map_err(|()| unreadable("listPullRequests"))?;
            if let Some(last) = remaining.checked_sub(1).and_then(|at| decoded.raw_indexes.get(at).copied()) {
                let consumed = last + 1;
                items.extend(decoded.items.into_iter().take(remaining));
                // A full raw response may have more rows even when malformed entries used the probe.
                return Ok(ListPage { items, truncated: consumed < decoded.raw_count || decoded.raw_count == top, cursor_advance: cursor_advance + consumed });
            }
            items.extend(decoded.items);
            if decoded.raw_count < top {
                return Ok(ListPage { items, truncated: false, cursor_advance: cursor_advance + decoded.raw_count });
            }
            skip += decoded.raw_count;
            cursor_advance += decoded.raw_count;
        }
    }

    async fn get_pull_request(&self, cwd: &str, number: u64) -> Read<AzurePullRequest> {
        let args = ["repos", "pr", "show", "--detect", "true", "--id"].map(String::from).into_iter().chain([number.to_string()]).collect();
        let stdout = self.execute_json(cwd, args, None).await?;
        match json::decode_pull_request_json(stdout.trim()) {
            Err(()) => Err(unreadable("getPullRequest")),
            // Azure answered with a well-formed pull request that carries no branch or link.
            Ok(None) => Err(PrFailure::failed("Azure DevOps returned no branch or link for the pull request.")),
            Ok(Some(pull_request)) => Ok(pull_request),
        }
    }

    async fn list_threads(&self, cwd: &str, location: &Location, number: u64) -> Read<Vec<PullRequestComment>> {
        self.invoke(cwd, "listThreads", "pullRequestThreads", pull_request_route(location, number), None, REVIEW_HISTORY_MAX_OUTPUT_BYTES, json::decode_threads_json).await
    }

    /// The pushes a pull request has had, oldest first. Azure hangs the changed files off an
    /// iteration rather than off the pull request, so reading a diff starts here.
    async fn list_iterations(&self, cwd: &str, location: &Location, number: u64) -> Read<Vec<Iteration>> {
        self.invoke(cwd, "listIterations", "pullRequestIterations", pull_request_route(location, number), None, REVIEW_HISTORY_MAX_OUTPUT_BYTES, json::decode_iterations_json).await
    }

    /// What one iteration changed, against the merge base rather than the previous push.
    async fn list_iteration_changes(&self, cwd: &str, location: &Location, number: u64, iteration_id: i64) -> Read<IterationChanges> {
        let (mut skip, mut changes) = (0i64, Vec::<ChangeEntry>::new());
        loop {
            let mut route = pull_request_route(location, number);
            route.push(format!("iterationId={iteration_id}"));
            // Azure pages this route at 1000 entries by default; this is its own maximum per page.
            let query = vec![format!("$top={CHANGE_ENTRIES_PER_PAGE}"), format!("$skip={skip}")];
            let answer: ChangePage = self.invoke(cwd, "listIterationChanges", "pullRequestIterationChanges", route, Some(query), CHANGE_ENTRIES_MAX_OUTPUT_BYTES, json::decode_iteration_changes_json).await?;
            changes.extend(answer.changes);
            // The last page names no page after it, and only that is the end of the change.
            let Some(next) = answer.next_skip else { return Ok(IterationChanges { changes, truncated: false }) };
            // A page pointing at where the read already is would be followed forever, and one past
            // the ceiling is a change nobody will read to the end of.
            if next <= skip || next >= MAX_CHANGE_ENTRIES {
                return Ok(IterationChanges { changes, truncated: true });
            }
            skip = next;
        }
    }

    /// One file's text at one commit, held to the build's allowance of `az` processes.
    async fn read_item_content(&self, cwd: &str, location: &Location, path: &str, commit: &str) -> Read<ItemContent> {
        let _permit = self.shared().spawns.acquire().await.expect("the diff semaphore is never closed");
        let query = vec![
            format!("path={}", to_item_path(path)),
            "versionDescriptor.versionType=commit".to_string(),
            format!("versionDescriptor.version={commit}"),
            "includeContent=true".to_string(),
            // Azure leaves `contentMetadata` out unless asked, and with it its own word on binary.
            "includeContentMetadata=true".to_string(),
            // Without this Azure answers with the file's own bytes, which `az` refuses.
            "$format=json".to_string(),
        ];
        self.invoke(cwd, "readItemContent", "items", repository_route(location), Some(query), ITEM_CONTENT_MAX_OUTPUT_BYTES, json::decode_item_content_json).await
    }

    // ---- locations and scopes -----------------------------------------------------------------

    async fn location_of(&self, cwd: &str, number: u64) -> Read<Option<Location>> {
        let key = format!("{cwd} {number}");
        {
            let mut held = self.shared().locations.lock().unwrap();
            if let Some(location) = held.shift_remove(&key) {
                held.insert(key, location.clone());
                return Ok(Some(location));
            }
        }
        let location = self.get_pull_request(cwd, number).await?.location;
        if let Some(location) = &location {
            let mut held = self.shared().locations.lock().unwrap();
            if held.len() >= LOCATION_CACHE_CAPACITY {
                held.shift_remove_index(0);
            }
            held.insert(key, location.clone());
        }
        Ok(location)
    }

    /// Everything a diff read needs before it can ask for a file: where the repository lives, and
    /// which pushes the pull request has had. The iterations are read afresh every time because
    /// the newest one is what a push adds.
    async fn diff_scope(&self, cwd: &str, number: u64) -> Read<Option<(Location, Vec<Iteration>)>> {
        let Some(location) = self.location_of(cwd, number).await? else { return Ok(None) };
        let iterations = self.list_iterations(cwd, &location, number).await?;
        Ok(Some((location, iterations)))
    }

    /// Both sides of one changed file, read at once because neither answer depends on the other.
    /// Only the sides a change actually has are asked for: Azure answers for a file that is not at a
    /// commit with a failure rather than with nothing.
    async fn read_texts(&self, cwd: &str, location: &Location, iteration: &Iteration, kind: json::ChangeKind, path: &str, old_path: &str) -> Read<FileTexts> {
        let old = async {
            if kind == json::ChangeKind::New {
                return Ok(ItemContent { contents: String::new(), is_binary: false });
            }
            self.read_item_content(cwd, location, old_path, &iteration.merge_base_commit).await
        };
        let new = async {
            if kind == json::ChangeKind::Deleted {
                return Ok(ItemContent { contents: String::new(), is_binary: false });
            }
            self.read_item_content(cwd, location, path, &iteration.head_commit).await
        };
        // Both sides are always asked for, whichever answers first: a side that fails does not end
        // the other's process half-way, and the failure reported is the old side's before the new's.
        let (old, new) = tokio::join!(Box::pin(old), Box::pin(new));
        let (old, new) = (old?, new?);
        // Azure hands a file it calls binary over in an encoding of its own, so its own word on
        // that is taken rather than looked for in bytes it may never have sent verbatim.
        Ok(FileTexts { binary: old.is_binary || new.is_binary, old_contents: old.contents, new_contents: new.contents })
    }

    /// What the whole pull request changed, taken from its latest push.
    async fn latest_changes(&self, cwd: &str, location: &Location, number: u64, iterations: &[Iteration]) -> Read<IterationChanges> {
        match iterations.last() {
            None => Ok(IterationChanges { changes: vec![], truncated: false }),
            Some(latest) => self.list_iteration_changes(cwd, location, number, latest.id).await,
        }
    }

    async fn changed_file_count(&self, cwd: &str, location: &Location, number: u64) -> Read<usize> {
        let iterations = self.list_iterations(cwd, location, number).await?;
        Ok(self.latest_changes(cwd, location, number, &iterations).await?.changes.len())
    }

    // ---- the diff ----------------------------------------------------------------------------------

    async fn diff_slice(&self, cwd: &str, number: u64, cursor: Option<&str>) -> Read<PullRequestDiffResult> {
        let empty = || PullRequestDiffResult { patch: String::new(), truncated: false, next_cursor: None, omitted_file_stats: None };
        let Some((location, iterations)) = self.diff_scope(cwd, number).await? else { return Ok(empty()) };
        let cursor = patch::parse_cursor(cursor);
        // Reading on stays with the push the first slice was taken against.
        let iteration = match cursor {
            None => iterations.last(),
            Some(cursor) => iterations.iter().find(|candidate| candidate.id == cursor.iteration_id),
        };
        let Some(iteration) = iteration else { return Ok(empty()) };
        let listed = self.list_iteration_changes(cwd, &location, number, iteration.id).await?;
        let changes = listed.changes;
        let (mut sections, mut truncated, mut bytes, mut edits) = (Vec::<String>::new(), listed.truncated, 0usize, 0usize);
        let mut index = cursor.map_or(0, |cursor| cursor.file_index);
        let mut full = false;
        // How many files to read at once. Everything read past the point the slice fills is thrown
        // away and read again by the next slice, so the batch narrows as the budgets do: what is
        // left of each, over what a file has spent of it on average so far. At least one, so a
        // file heavier than the whole budget still moves the cursor.
        let batch_width = |sections: usize, bytes: usize, edits: usize| -> usize {
            if sections == 0 {
                return DIFF_FILE_CONCURRENCY;
            }
            let admits = |left: i64, spent: usize| -> i64 { (left as f64 / f64::max(1.0, spent as f64 / sections as f64)).ceil() as i64 };
            let width = (DIFF_FILE_CONCURRENCY as i64)
                .min(patch::MAX_DIFF_SLICE_FILES as i64 - sections as i64)
                .min(admits(patch::MAX_DIFF_SLICE_BYTES as i64 - bytes as i64, bytes))
                .min(admits(patch::MAX_DIFF_SLICE_EDITS as i64 - patch::MAX_FILE_DIFF_EDITS as i64 - edits as i64, edits));
            width.max(1) as usize
        };
        while !full && index < changes.len() {
            let batch = &changes[index..(index + batch_width(sections.len(), bytes, edits)).min(changes.len())];
            // A pair Azure refuses is one file rather than the whole slice; a signed-out CLI, a rate
            // limit or no `az` at all fails the read.
            let location = &location;
            let reads = batch.iter().map(|change| {
                Box::pin(async move {
                    match self.read_texts(cwd, location, iteration, change.change_kind, &change.path, &change.old_path).await {
                        Ok(texts) => Ok(Some(texts)),
                        Err(failure) if is_file_problem(&failure) => Ok(None),
                        Err(failure) => Err(failure),
                    }
                })
            });
            let read = futures_util::future::try_join_all(reads).await?;
            for (change, texts) in batch.iter().zip(read) {
                // The diff is synchronous and the reads no longer stand between one file and the
                // next, so the thread is handed back here.
                tokio::task::yield_now().await;
                let file = match texts {
                    None => patch::unreadable_file_patch(change),
                    Some(texts) => patch::file_patch(change, &texts),
                };
                bytes += file.section.len();
                edits += file.edits;
                truncated = truncated || file.truncated;
                sections.push(file.section);
                index += 1;
                // Checked after the file is added, so every slice carries at least one.
                if bytes >= patch::MAX_DIFF_SLICE_BYTES || edits + patch::MAX_FILE_DIFF_EDITS > patch::MAX_DIFF_SLICE_EDITS || sections.len() >= patch::MAX_DIFF_SLICE_FILES || file.abandoned {
                    full = true;
                    break;
                }
            }
        }
        let next_cursor = (index < changes.len()).then(|| patch::format_cursor(DiffCursor { iteration_id: iteration.id, file_index: index }));
        Ok(PullRequestDiffResult { patch: sections.concat(), truncated, next_cursor, omitted_file_stats: None })
    }

    // ---- writes ------------------------------------------------------------------------------------

    async fn update_command(&self, cwd: &str, number: u64, edits: Vec<String>) -> Read<()> {
        let mut args: Vec<String> = ["repos", "pr", "update", "--detect", "true", "--id"].map(String::from).to_vec();
        args.push(number.to_string());
        args.extend(edits);
        self.execute_json(cwd, args, None).await.map(|_| ())
    }
}

fn to_change_request(pull_request: &AzurePullRequest) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number: pull_request.number,
        title: pull_request.title.clone(),
        url: pull_request.url.clone(),
        author: pull_request.author.clone(),
        head_branch: pull_request.head_branch.clone(),
        head_repository_name_with_owner: None,
        base_branch: pull_request.base_branch.clone(),
        state: pull_request.state,
        is_draft: pull_request.is_draft,
        mergeability: pull_request.mergeability,
        // Azure counts a pull request's files but never its lines, and counting them would mean
        // reading every file on both sides of every row of a listing.
        additions: 0,
        deletions: 0,
        created_at: pull_request.created_at.clone(),
        updated_at: pull_request.updated_at.clone(),
        review_request_logins: pull_request.review_request_logins.clone(),
        // Azure keeps labels on work items rather than on the pull request.
        labels: vec![],
        review_decision: None,
        checks_state: None,
    }
}

fn change_kind_of(change_type: PullRequestDiffChangeType) -> json::ChangeKind {
    match change_type {
        PullRequestDiffChangeType::New => json::ChangeKind::New,
        PullRequestDiffChangeType::Deleted => json::ChangeKind::Deleted,
        PullRequestDiffChangeType::Change => json::ChangeKind::Change,
        PullRequestDiffChangeType::RenamePure => json::ChangeKind::RenamePure,
        PullRequestDiffChangeType::RenameChanged => json::ChangeKind::RenameChanged,
    }
}

fn closed_at(pull_request: &AzurePullRequest) -> Option<String> {
    (pull_request.state == PullRequestState::Closed).then(|| pull_request.closed_at.clone()).flatten()
}

fn merged_at(pull_request: &AzurePullRequest) -> Option<String> {
    (pull_request.state == PullRequestState::Merged).then(|| pull_request.closed_at.clone()).flatten()
}

impl PullRequestProvider for AzurePullRequests {
    fn kind(&self) -> SourceControlProviderKind {
        SourceControlProviderKind::AzureDevops
    }

    fn capabilities(&self) -> PullRequestCapabilities {
        capabilities()
    }

    fn viewer<'a>(&'a self, cwd: &'a str, _host: &'a str) -> BoxFuture<'a, Read<String>> {
        Box::pin(self.get_viewer(cwd))
    }

    // `request.query` is deliberately dropped: `az repos pr list` filters by status, creator,
    // reviewer and branch, and has nothing that matches text. Sending it as one of those would
    // narrow by the wrong thing, so the page comes back unnarrowed.
    fn list<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<ChangeRequestPage>> {
        Box::pin(async move {
            let repository = request.repositories.first().cloned().unwrap_or_default();
            let page = self.list_pull_request_page(request, &repository).await?;
            // Azure answers in one order whether or not it is being carried on from, so a slice can
            // always be stepped past, by counting, which is all Azure offers.
            Ok(ChangeRequestPage { items: page.items.iter().map(to_change_request).collect(), truncated: page.truncated, cursor_advance: Some(page.cursor_advance), continues: true })
        })
    }

    // Azure has no checks route, so the original's provider declares none and the page reads null.
    fn has_checks(&self) -> bool {
        false
    }

    fn checks<'a>(&'a self, _subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestChecks>> {
        Box::pin(async { Err(PrFailure::failed("Azure DevOps has no checks to read.")) })
    }

    fn detail<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestDetail>> {
        Box::pin(async move {
            let pull_request = self.get_pull_request(&subject.cwd, subject.number).await?;
            // The file count is two reads past the pull request itself, and it is the only thing
            // riding on them, so a failure leaves it unknown rather than losing the whole detail.
            let changed_files = match &pull_request.location {
                None => 0,
                Some(location) => self.changed_file_count(&subject.cwd, location, subject.number).await.unwrap_or(0),
            };
            Ok(ChangeRequestDetail {
                row: to_change_request(&pull_request),
                head_sha: None,
                body: pull_request.body.clone(),
                changed_files: changed_files as u64,
                merged_at: merged_at(&pull_request),
                closed_at: closed_at(&pull_request),
                reviewers: pull_request.reviewers.clone(),
                checks: vec![],
                merge_capabilities: PullRequestMergeCapabilities { merge: true, squash: true, rebase: false },
                viewer_permissions: viewer_permissions(),
                base_comparison: None,
                behind_by: None,
                auto_merge_enabled: Some(pull_request.auto_merge_enabled),
                auto_merge_method: pull_request.auto_merge_method,
                workflow_approvals_required: None,
            })
        })
    }

    // The polled path a linked thread's row stays live on: one `az` read, no iterations or changes
    // behind it, since the file count that would cost is not shown here.
    fn summary<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestSummary>> {
        Box::pin(async move {
            let pull_request = self.get_pull_request(&subject.cwd, subject.number).await?;
            Ok(ChangeRequestSummary {
                number: pull_request.number,
                title: pull_request.title.clone(),
                url: pull_request.url.clone(),
                head_branch: pull_request.head_branch.clone(),
                base_branch: pull_request.base_branch.clone(),
                state: pull_request.state,
                is_draft: Some(pull_request.is_draft),
                closed_at: closed_at(&pull_request),
                merged_at: merged_at(&pull_request),
                updated_at: pull_request.updated_at.clone(),
                author: Some(pull_request.author.clone()),
                additions: None,
                deletions: None,
                changed_files: None,
                review_decision: None,
                checks_state: None,
                mergeability: Some(pull_request.mergeability),
                stack: None,
            })
        })
    }

    fn activity<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestActivity>> {
        Box::pin(async move {
            let pull_request = self.get_pull_request(&subject.cwd, subject.number).await?;
            let (comments, comments_truncated) = match &pull_request.location {
                None => (vec![], true),
                Some(location) => match self.list_threads(&subject.cwd, location, subject.number).await {
                    Ok(comments) => (comments, false),
                    Err(_) => (vec![], true),
                },
            };
            Ok(PullRequestActivity {
                author: None,
                reviewers: None,
                comment_count: comments.len() as u64,
                comments,
                comments_truncated,
                review_threads_truncated: None,
                review_threads: vec![],
                commits: vec![],
                reactions: None,
            })
        })
    }

    // No request at all: Azure has nothing to say about the viewer that a pull request read can
    // reach, so the answer is the same constant the detail carries.
    fn viewer_permissions<'a>(&'a self, _subject: &'a Subject, _include_update_branch: bool) -> BoxFuture<'a, Read<PullRequestViewerPermissions>> {
        Box::pin(async { Ok(viewer_permissions()) })
    }

    // `commit` is deliberately dropped: Azure states no commit list on a pull request, so the Code
    // tab has nothing to scope itself to and always asks for the whole change.
    fn diff<'a>(&'a self, subject: &'a Subject, cursor: Option<&'a str>, _commit: Option<&'a str>) -> BoxFuture<'a, Read<PullRequestDiffResult>> {
        Box::pin(self.diff_slice(&subject.cwd, subject.number, cursor))
    }

    // The patch is built from whole files, so opening the lines around a hunk is the same two reads
    // over again. Read against the latest iteration: nothing in the request says which push the
    // reader is looking at.
    fn diff_file_contents<'a>(
        &'a self,
        subject: &'a Subject,
        _commit: Option<&'a str>,
        change_type: PullRequestDiffChangeType,
        old_path: &'a str,
        new_path: &'a str,
    ) -> BoxFuture<'a, Read<PullRequestDiffFileContentsResult>> {
        Box::pin(async move {
            let empty = PullRequestDiffFileContentsResult { old_contents: String::new(), new_contents: String::new() };
            let Some((location, iterations)) = self.diff_scope(&subject.cwd, subject.number).await? else { return Ok(empty) };
            let Some(iteration) = iterations.last() else { return Ok(empty) };
            let texts = self.read_texts(&subject.cwd, &location, iteration, change_kind_of(change_type), new_path, old_path).await?;
            Ok(PullRequestDiffFileContentsResult { old_contents: texts.old_contents, new_contents: texts.new_contents })
        })
    }

    // What the head has of each marked file, which is the blob Azure already names on the change it
    // reports. One read covers every path, since the latest iteration lists the whole change. A path
    // the change does not carry is at the empty revision, where a file the pull request deletes
    // sits; when the change was too long to follow to its end those paths are left out instead,
    // since reporting them as deleted would clear a file nobody has read.
    fn file_revisions<'a>(&'a self, subject: &'a Subject, paths: &'a [String]) -> BoxFuture<'a, Read<FileRevisions>> {
        Box::pin(async move {
            let mut revisions = std::collections::BTreeMap::new();
            if paths.is_empty() {
                return Ok(FileRevisions { revisions, complete: false });
            }
            let Some((location, iterations)) = self.diff_scope(&subject.cwd, subject.number).await? else { return Ok(FileRevisions { revisions, complete: false }) };
            let listed = self.latest_changes(&subject.cwd, &location, subject.number, &iterations).await?;
            for change in &listed.changes {
                if let (true, Some(object_id)) = (paths.contains(&change.path), &change.object_id) {
                    revisions.insert(change.path.clone(), object_id.clone());
                }
            }
            if !listed.truncated {
                for path in paths {
                    revisions.entry(path.clone()).or_insert_with(String::new);
                }
            }
            Ok(FileRevisions { revisions, complete: false })
        })
    }

    fn run_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            let edits = action_args(run.action, run.merge_method)?;
            self.update_command(&subject.cwd, subject.number, edits).await
        })
    }

    fn update<'a>(&'a self, subject: &'a Subject, title: Option<&'a str>, body: Option<&'a str>) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            // One argument rather than a flag and a value beside it: a description usually opens
            // with a bullet, and az reads a dash in the next argv slot as a flag of its own.
            let mut edits = vec![];
            if let Some(title) = title {
                edits.push(format!("--title={title}"));
            }
            if let Some(body) = body {
                edits.push(format!("--description={body}"));
            }
            self.update_command(&subject.cwd, subject.number, edits).await
        })
    }

    // Never called: `capabilities.comment` is false, and the service refuses a comment without it.
    fn comment<'a>(&'a self, _subject: &'a Subject, _body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(cannot_write()) })
    }

    // Declared unsupported above, so the service refuses these before a provider is reached. They
    // exist because every provider answers the whole port.
    fn submit_review<'a>(&'a self, _subject: &'a Subject, _verdict: PullRequestReviewVerdict, _body: &'a str, _comments: &'a [PullRequestReviewCommentDraft]) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(cannot_write()) })
    }

    // Never called: `capabilities.reviewers.list_candidates` is false.
    fn reviewer_candidates<'a>(&'a self, _subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestReviewerCandidateList>> {
        Box::pin(async { Err(PrFailure::failed("Azure DevOps cannot say who may review a pull request.")) })
    }

    fn set_reviewer_request<'a>(&'a self, subject: &'a Subject, reviewers: &'a [PullRequestReviewerRequestTarget], requested: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move {
            // Azure names an identity by an email address or a guid, and has no team to ask, so a
            // candidate's id is the whole of what it takes.
            let names: Vec<String> = reviewers.iter().map(|reviewer| reviewer.id.as_str().to_string()).collect();
            if names.iter().any(|name| !is_reviewer_name(name)) {
                return Err(PrFailure::failed("A reviewer is named by an email address or an identity id."));
            }
            let mut args: Vec<String> = ["repos", "pr", "reviewer", if requested { "add" } else { "remove" }, "--detect", "true", "--id"].map(String::from).to_vec();
            args.push(subject.number.to_string());
            // One `--reviewers` takes them all, because az reads the flag as a list.
            args.push("--reviewers".into());
            args.extend(names);
            self.execute_json(&subject.cwd, args, None).await.map(|_| ())
        })
    }

    fn reply_to_thread<'a>(&'a self, _subject: &'a Subject, _thread_id: &'a str, _body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(cannot_write()) })
    }

    fn set_reaction<'a>(&'a self, _subject: &'a Subject, _subject_id: Option<&'a str>, _content: PullRequestReactionContent, _reacted: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(cannot_write()) })
    }

    fn set_thread_resolution<'a>(&'a self, _subject: &'a Subject, _thread_id: &'a str, _resolved: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async { Err(cannot_write()) })
    }
}
