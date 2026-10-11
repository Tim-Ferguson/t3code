//! GitHub behind the pull request provider interface (port of `GitHubPullRequestProvider.ts`).
//! `GitHub` keeps the requests; this turns what it reads into the neutral shapes the service
//! assembles wire responses from, so nothing above the provider knows which host answered.
use crate::{
    pull_request_actions::{self as actions, RunAction},
    pull_request_decode as decode,
    pull_request_github::{self as github, GitHub, PrFailure, Read, SEARCH_MAX_ROWS, SearchInput, Target},
    pull_request_provider::*,
    pull_request_service::with_avatar,
    source_control_github_api as api,
    source_control_host::Host,
};
use futures_util::future::BoxFuture;
use std::collections::BTreeMap;
use t3_contracts::*;

pub struct GitHubProvider {
    gh: GitHub,
}

impl GitHubProvider {
    pub fn new(host: Host) -> Self {
        Self { gh: GitHub::new(host) }
    }
}

fn target(subject: &Subject) -> Target {
    Target { host: subject.host.clone(), repository: subject.repository.clone(), number: subject.number }
}

fn row(item: &decode::GitHubListItem, host: &str) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: item.stack.clone(),
        number: item.number,
        title: item.title.clone(),
        url: item.url.clone(),
        author: with_avatar(item.author.clone(), host),
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
        review_decision: Some(item.review_decision),
        checks_state: Some(item.checks_state),
    }
}

impl GitHubProvider {
    async fn search(&self, request: &ListRequest) -> Read<BatchedPage> {
        let query = github::search_query(&SearchInput {
            repositories: &request.repositories,
            state: request.state,
            involvement: request.involvement,
            viewer: &request.viewer,
            query: request.query.as_deref(),
            updated_before: request.cursor.as_ref().map(|cursor| cursor.updated_before.as_str()),
            filters: request.filters.as_ref(),
        })
        .ok_or_else(|| PrFailure::failed("A repository was named that GitHub cannot address."))?;
        let limit = request.limit as usize;
        // One extra row reveals that the host has more than the slice shows.
        let rows = (limit + 1).min(SEARCH_MAX_ROWS);
        let page = Box::pin(self.gh.search(&request.host, &query, rows, request.host.eq_ignore_ascii_case("github.com"))).await?;
        let truncated = page.raw_count > limit || page.has_next_page;
        let items = page.items.iter().take(limit).map(|item| BatchedProviderChangeRequest { row: row(&item.item, &request.host), repository: item.repository.clone() }).collect();
        Ok(BatchedPage { items, truncated })
    }

    async fn read_detail(&self, subject: &Subject) -> Read<ChangeRequestDetail> {
        let target = target(subject);
        let core = Box::pin(self.gh.detail(&target)).await?;
        // A fork's pull request also shows the workflows waiting for a maintainer's approval.
        let (checks, workflow_approvals_required) = actions::read_workflow_approvals(&self.gh, &target, &core).await?;
        let (item, extra) = (&core.detail.item, &core.detail);
        let mut access = core.viewer_access.clone();
        access.can_update_branch = Some(core.comparison.as_ref().is_some_and(|comparison| comparison.viewer_can_update));
        let (comparison, behind_by) = match &core.comparison {
            Some(comparison) => match comparison.behind_by {
                None => (PullRequestBaseComparison::Unknown, None),
                Some(0) => (PullRequestBaseComparison::UpToDate, Some(0)),
                Some(behind) => (PullRequestBaseComparison::Behind, Some(behind)),
            },
            None => (PullRequestBaseComparison::Unknown, None),
        };
        let reviewers = item
            .review_request_logins
            .iter()
            .filter_map(|login| Some(PullRequestActor { is_bot: None, login: TrimmedNonEmptyString::new(login).ok()?, name: None, avatar_url: None }))
            .collect();
        Ok(ChangeRequestDetail {
            row: row(item, &subject.host),
            head_sha: extra.head_sha.clone().filter(|sha| !sha.is_empty()),
            body: extra.body.clone(),
            changed_files: extra.changed_files,
            merged_at: extra.merged_at.clone(),
            closed_at: extra.closed_at.clone(),
            reviewers,
            checks,
            merge_capabilities: core.viewer_access.merge_capabilities,
            viewer_permissions: github::viewer_permissions(&access),
            base_comparison: Some(comparison),
            behind_by,
            auto_merge_enabled: extra.auto_merge_enabled,
            auto_merge_method: extra.auto_merge_method,
            workflow_approvals_required,
        })
    }

    /// `getViewerPermissions`: the light read alone, or with the branch comparison when asked; a
    /// failed heavy read (other than a rate limit) still lets the light one answer, withholding
    /// only update-branch.
    async fn read_permissions(&self, subject: &Subject, include_update_branch: bool) -> Read<PullRequestViewerPermissions> {
        let target = target(subject);
        let light = async { Box::pin(self.gh.viewer_access(&target)).await.map(|access| github::viewer_permissions(&access)) };
        if !include_update_branch {
            return light.await;
        }
        match api::with_allow_reserve(Box::pin(self.gh.detail(&target))).await {
            Ok(core) => {
                let access = decode::GitHubViewerAccess {
                    can_update_branch: Some(core.detail.item.state == PullRequestState::Open && core.comparison.as_ref().is_some_and(|comparison| comparison.viewer_can_update)),
                    ..core.viewer_access
                };
                Ok(github::viewer_permissions(&access))
            }
            Err(failure) if failure.reason != github::Reason::RateLimited => light.await,
            Err(failure) => Err(failure),
        }
    }
}

impl PullRequestProvider for GitHubProvider {
    fn kind(&self) -> SourceControlProviderKind {
        SourceControlProviderKind::Github
    }

    fn capabilities(&self) -> PullRequestCapabilities {
        github::capabilities()
    }

    fn viewer<'a>(&'a self, _cwd: &'a str, host: &'a str) -> BoxFuture<'a, Read<String>> {
        Box::pin(self.gh.viewer_login(host))
    }

    fn list<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<ChangeRequestPage>> {
        Box::pin(async move {
            let page = Box::pin(self.search(request)).await?;
            let wanted = request.repositories.first().map(|repository| repository.trim().to_lowercase());
            Ok(ChangeRequestPage {
                items: page.items.into_iter().filter(|item| Some(item.repository.trim().to_lowercase()) == wanted).map(|item| item.row).collect(),
                truncated: page.truncated,
                cursor_advance: None,
                continues: true,
            })
        })
    }

    fn lists_across(&self) -> bool {
        true
    }

    fn list_across<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<BatchedPage>> {
        Box::pin(self.search(request))
    }

    fn lists_stats(&self) -> bool {
        true
    }

    fn list_stats<'a>(&'a self, _cwd: &'a str, host: &'a str, changes: &'a [(String, u64)]) -> BoxFuture<'a, Read<Vec<ChangeRequestStat>>> {
        Box::pin(async move {
            let found = Box::pin(self.gh.stats(host, changes)).await?;
            Ok(found.into_iter().map(|(repository, number, additions, deletions)| ChangeRequestStat { repository, number, additions, deletions }).collect())
        })
    }

    fn checks<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestChecks>> {
        Box::pin(async move {
            let target = target(subject);
            let core = Box::pin(self.gh.detail(&target)).await?;
            let (checks, _) = actions::read_workflow_approvals(&self.gh, &target, &core).await?;
            Ok(PullRequestChecks { state: core.detail.item.state, checks })
        })
    }

    fn detail<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestDetail>> {
        Box::pin(self.read_detail(subject))
    }

    fn preview<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestPreview>> {
        Box::pin(async move {
            let read = Box::pin(self.gh.preview(&target(subject))).await?;
            Ok(ChangeRequestPreview {
                number: read.number,
                title: read.title,
                url: read.url,
                author: with_avatar(read.author, &subject.host),
                state: read.state,
                is_draft: read.is_draft,
                created_at: read.created_at,
            })
        })
    }

    fn summary<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestSummary>> {
        Box::pin(async move {
            let read = self.gh.summary(&target(subject)).await?;
            Ok(ChangeRequestSummary {
                number: read.number,
                title: read.title,
                url: read.url,
                head_branch: read.head_branch,
                base_branch: read.base_branch,
                state: read.state,
                is_draft: Some(read.is_draft),
                closed_at: read.closed_at,
                merged_at: read.merged_at,
                updated_at: read.updated_at,
                author: Some(with_avatar(read.author, &subject.host)),
                additions: Some(read.additions),
                deletions: Some(read.deletions),
                changed_files: Some(read.changed_files),
                review_decision: Some(read.review_decision),
                checks_state: Some(read.checks_state),
                mergeability: Some(read.mergeability),
                stack: read.stack,
            })
        })
    }

    fn stack<'a>(&'a self, subject: &'a Subject, include_details: bool) -> BoxFuture<'a, Read<Option<PullRequestStack>>> {
        Box::pin(async move { self.gh.stack(&target(subject), include_details).await })
    }

    fn activity<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestActivity>> {
        Box::pin(async move {
            let target = target(subject);
            let (activity, threads) = tokio::join!(Box::pin(self.gh.activity(&target)), Box::pin(self.gh.review_threads(&target)));
            let activity = activity?;
            // A GraphQL hiccup degrades to a truncated conversation rather than blanking activity.
            let threads = threads.unwrap_or_else(|_| degraded_threads());
            Ok(assemble_activity(activity, threads, &subject.host))
        })
    }

    fn thread_comments<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, cursor: &'a str) -> BoxFuture<'a, Read<PullRequestThreadCommentsResult>> {
        Box::pin(async move { self.gh.thread_comments(&target(subject), thread_id, cursor).await })
    }

    fn viewer_permissions<'a>(&'a self, subject: &'a Subject, include_update_branch: bool) -> BoxFuture<'a, Read<PullRequestViewerPermissions>> {
        Box::pin(self.read_permissions(subject, include_update_branch))
    }

    fn diff<'a>(&'a self, subject: &'a Subject, cursor: Option<&'a str>, commit: Option<&'a str>) -> BoxFuture<'a, Read<PullRequestDiffResult>> {
        Box::pin(async move { self.gh.diff(&target(subject), cursor, commit).await })
    }

    fn diff_file_contents<'a>(
        &'a self,
        subject: &'a Subject,
        commit: Option<&'a str>,
        change_type: PullRequestDiffChangeType,
        old_path: &'a str,
        new_path: &'a str,
    ) -> BoxFuture<'a, Read<PullRequestDiffFileContentsResult>> {
        Box::pin(async move { self.gh.diff_file_contents(&target(subject), commit, change_type, old_path, new_path).await })
    }

    fn files_viewed<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestFilesViewedResult>> {
        Box::pin(async move { self.gh.files_viewed(&target(subject)).await })
    }

    fn set_files_viewed<'a>(&'a self, subject: &'a Subject, files: &'a [PullRequestSetFileViewed]) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.set_files_viewed(&target(subject), files).await })
    }

    fn run_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.run_action(&target(subject), run).await })
    }

    fn run_stack_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction, stack: &'a crate::pull_request_stack::StackRun) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.run_stack_action(&target(subject), run, stack).await })
    }

    fn update<'a>(&'a self, subject: &'a Subject, title: Option<&'a str>, body: Option<&'a str>) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.update_pull_request(&target(subject), title, body).await })
    }

    fn comment<'a>(&'a self, subject: &'a Subject, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.comment(&target(subject), body).await })
    }

    fn update_comment<'a>(&'a self, subject: &'a Subject, comment_id: &'a str, kind: PullRequestCommentUpdateKind, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.update_comment(&target(subject), comment_id, kind, body).await })
    }

    fn submit_review<'a>(&'a self, subject: &'a Subject, verdict: PullRequestReviewVerdict, body: &'a str, comments: &'a [PullRequestReviewCommentDraft]) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.submit_review(&target(subject), verdict, body, comments).await })
    }

    fn reviewer_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestReviewerCandidateList>> {
        Box::pin(async move { self.gh.list_reviewer_candidates(&target(subject)).await })
    }

    fn set_reviewer_request<'a>(&'a self, subject: &'a Subject, reviewers: &'a [PullRequestReviewerRequestTarget], requested: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.set_reviewer_request(&target(subject), reviewers, requested).await })
    }

    fn label_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestLabelCandidateList>> {
        Box::pin(async move { self.gh.list_label_candidates(&target(subject)).await })
    }

    fn set_labels<'a>(&'a self, subject: &'a Subject, labels: &'a [&'a str], applied: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.set_labels(&target(subject), labels, applied).await })
    }

    fn reply_to_thread<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.reply_to_thread(&target(subject), thread_id, body).await })
    }

    fn set_reaction<'a>(&'a self, subject: &'a Subject, subject_id: Option<&'a str>, content: PullRequestReactionContent, reacted: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.set_reaction(&target(subject), subject_id, content, reacted).await })
    }

    fn set_thread_resolution<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, resolved: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(async move { self.gh.set_thread_resolution(&target(subject), thread_id, resolved).await })
    }
}

// ---------------------------------------------------------------------------------------------
// Activity assembly
// ---------------------------------------------------------------------------------------------

fn degraded_threads() -> github::ThreadsRead {
    github::ThreadsRead {
        comments: vec![],
        dismissals_by_review_id: BTreeMap::new(),
        review_threads: vec![],
        comment_count: 0,
        truncated: true,
        review_threads_truncated: true,
        page: decode::ReviewThreadPage {
            threads: vec![],
            next_cursor: None,
            reactions: vec![],
            reactions_by_id: BTreeMap::new(),
            edited_at_by_id: BTreeMap::new(),
            reviewers: vec![],
            avatars_by_login: BTreeMap::new(),
            bot_logins: Default::default(),
            commit_stats: BTreeMap::new(),
            commits: vec![],
            viewer: decode::ThreadViewer { can_update: true, did_author: false },
            dismissals_by_review_id: BTreeMap::new(),
            next_dismissal_cursor: None,
        },
        avatars_by_login: BTreeMap::new(),
        bot_logins: Default::default(),
    }
}

/// The login's picture as the review read collected it, else the one every install serves; and
/// whether GitHub says the account is an app.
fn avatar_for(actor: Option<PullRequestActor>, avatars: &BTreeMap<String, String>, host: &str, bots: &std::collections::BTreeSet<String>) -> Option<PullRequestActor> {
    let mut actor = actor?;
    if bots.contains(actor.login.as_str()) {
        actor.is_bot = Some(Some(true));
    }
    if actor.avatar_url.is_some() {
        return Some(actor);
    }
    actor.avatar_url = avatars.get(actor.login.as_str()).cloned().or_else(|| github::login_avatar_url(actor.login.as_str(), host));
    Some(actor)
}

/// True where markdown would render nothing: whitespace, or only HTML comments.
fn renders_empty(body: &str) -> bool {
    let mut rest = body;
    let mut visible = String::new();
    while let Some(start) = rest.find("<!--") {
        visible.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    visible.push_str(rest);
    visible.trim().is_empty()
}

fn assemble_activity(activity: github::ActivityRead, threads: github::ThreadsRead, host: &str) -> PullRequestActivity {
    let (avatars, bots) = (&threads.avatars_by_login, &threads.bot_logins);
    let commits_source = if threads.page.commits.is_empty() { activity.commits.clone() } else { threads.page.commits.clone() };
    let commits = commits_source
        .into_iter()
        .map(|commit| {
            let stat = threads.page.commit_stats.get(&commit.oid);
            PullRequestCommit {
                additions: stat.map(|stat| stat.additions).or(commit.additions),
                deletions: stat.map(|stat| stat.deletions).or(commit.deletions),
                authors: commit.authors.as_ref().map(|authors| {
                    authors.iter().map(|author| avatar_for(Some(author.clone()), avatars, host, bots).unwrap_or_else(|| author.clone())).collect()
                }),
                ..commit
            }
        })
        .collect();
    let comment_count = (activity.comments.len() + threads.comment_count as usize) as u64;
    let mut comments: Vec<PullRequestComment> = activity
        .comments
        .iter()
        .cloned()
        .chain(threads.comments.iter().cloned())
        .map(|comment| {
            // GitHub keeps the dismissal reason on the timeline event, not on the review, so a
            // dismissed review with nothing visible of its own reads its words from there.
            let body = if comment.kind == PullRequestCommentKind::Review
                && comment.review_state.as_deref().is_some_and(|state| state.eq_ignore_ascii_case("DISMISSED"))
                && renders_empty(&comment.body)
            {
                threads.dismissals_by_review_id.get(&comment.id).cloned().unwrap_or(comment.body.clone())
            } else {
                comment.body.clone()
            };
            PullRequestComment {
                body,
                author: avatar_for(comment.author.clone(), avatars, host, bots),
                reactions: Some(comment.reactions.clone().or_else(|| threads.page.reactions_by_id.get(&comment.id).cloned()).unwrap_or_default()),
                edited_at: Some(threads.page.edited_at_by_id.get(&comment.id).cloned().or_else(|| comment.edited_at.clone().flatten())),
                ..comment
            }
        })
        .collect();
    comments.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    let review_threads = threads
        .review_threads
        .iter()
        .cloned()
        .map(|thread| PullRequestReviewThread {
            comments: thread
                .comments
                .iter()
                .cloned()
                .map(|comment| PullRequestThreadComment { author: avatar_for(comment.author.clone(), avatars, host, bots), ..comment })
                .collect(),
            ..thread
        })
        .collect();
    PullRequestActivity {
        author: Some(avatar_for(activity.author, avatars, host, bots)),
        reviewers: Some(threads.page.reviewers.clone()),
        comments,
        comment_count,
        comments_truncated: threads.truncated,
        review_threads_truncated: Some(threads.review_threads_truncated),
        review_threads,
        commits,
        reactions: Some(threads.page.reactions.clone()),
    }
}
