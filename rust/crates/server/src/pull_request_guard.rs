//! The rate-limit pause in front of a host's provider (`withRateLimitBackoff` in
//! `PullRequestService.ts`). A provider that answers rate-limited pauses its host: reads are then
//! refused without being sent, and an interactive call (a press, a write) is let through the
//! pause without clearing it. GitHub keeps its own pause per credential below `GitHub`, so only
//! the other hosts are wrapped.
use crate::{
    pull_request_actions::RunAction,
    pull_request_github::{PrFailure, Reason, Read},
    pull_request_provider::*,
    source_control_rate_limit::RateLimit,
};
use futures_util::future::BoxFuture;
use std::{future::Future, sync::Arc};
use t3_contracts::*;
use t3_vcs::Clock;

pub struct Guarded {
    inner: Arc<dyn PullRequestProvider>,
    limits: Arc<RateLimit>,
    clock: Arc<dyn Clock>,
}

impl Guarded {
    pub fn new(inner: Arc<dyn PullRequestProvider>, limits: Arc<RateLimit>, clock: Arc<dyn Clock>) -> Self {
        Self { inner, limits, clock }
    }

    /// Asks the pause first, runs the call, and tells the pause how it went. `interactive` lets a
    /// call through a pause that is still running.
    async fn protect<T>(&self, host: &str, interactive: bool, call: impl Future<Output = Read<T>>) -> Read<T> {
        let kind = self.inner.kind();
        let now = self.clock.utc_now().timestamp_millis();
        let lease = self.limits.check(kind, host, "", now, interactive).map_err(|paused| PrFailure {
            reason: Reason::RateLimited,
            detail: paused.message(),
            retry_at: Some(paused.retry_at),
        })?;
        let result = call.await;
        let now = self.clock.utc_now().timestamp_millis();
        match &result {
            Ok(_) => self.limits.record_success(kind, host, "", lease, now),
            Err(failure) if failure.reason == Reason::RateLimited => self.limits.record_rate_limit(kind, host, "", lease, failure.retry_at, now),
            Err(_) => {}
        }
        result
    }
}

impl PullRequestProvider for Guarded {
    fn kind(&self) -> SourceControlProviderKind {
        self.inner.kind()
    }
    fn capabilities(&self) -> PullRequestCapabilities {
        self.inner.capabilities()
    }
    fn viewer<'a>(&'a self, cwd: &'a str, host: &'a str) -> BoxFuture<'a, Read<String>> {
        Box::pin(self.protect(host, false, self.inner.viewer(cwd, host)))
    }
    fn list<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<ChangeRequestPage>> {
        Box::pin(self.protect(&request.host, false, self.inner.list(request)))
    }
    fn lists_across(&self) -> bool {
        self.inner.lists_across()
    }
    fn list_across<'a>(&'a self, request: &'a ListRequest) -> BoxFuture<'a, Read<BatchedPage>> {
        Box::pin(self.protect(&request.host, false, self.inner.list_across(request)))
    }
    fn lists_stats(&self) -> bool {
        self.inner.lists_stats()
    }
    fn list_stats<'a>(&'a self, cwd: &'a str, host: &'a str, changes: &'a [(String, u64)]) -> BoxFuture<'a, Read<Vec<ChangeRequestStat>>> {
        Box::pin(self.protect(host, false, self.inner.list_stats(cwd, host, changes)))
    }
    fn checks<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestChecks>> {
        Box::pin(self.protect(&subject.host, false, self.inner.checks(subject)))
    }
    fn detail<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestDetail>> {
        Box::pin(self.protect(&subject.host, false, self.inner.detail(subject)))
    }
    fn preview<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestPreview>> {
        Box::pin(self.protect(&subject.host, false, self.inner.preview(subject)))
    }
    fn summary<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<ChangeRequestSummary>> {
        Box::pin(self.protect(&subject.host, false, self.inner.summary(subject)))
    }
    fn stack<'a>(&'a self, subject: &'a Subject, include_details: bool) -> BoxFuture<'a, Read<Option<PullRequestStack>>> {
        Box::pin(self.protect(&subject.host, false, self.inner.stack(subject, include_details)))
    }
    fn activity<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestActivity>> {
        Box::pin(self.protect(&subject.host, false, self.inner.activity(subject)))
    }
    fn thread_comments<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, cursor: &'a str) -> BoxFuture<'a, Read<PullRequestThreadCommentsResult>> {
        Box::pin(self.protect(&subject.host, false, self.inner.thread_comments(subject, thread_id, cursor)))
    }
    fn viewer_permissions<'a>(&'a self, subject: &'a Subject, include_update_branch: bool) -> BoxFuture<'a, Read<PullRequestViewerPermissions>> {
        Box::pin(self.protect(&subject.host, true, self.inner.viewer_permissions(subject, include_update_branch)))
    }
    fn diff<'a>(&'a self, subject: &'a Subject, cursor: Option<&'a str>, commit: Option<&'a str>) -> BoxFuture<'a, Read<PullRequestDiffResult>> {
        Box::pin(self.protect(&subject.host, false, self.inner.diff(subject, cursor, commit)))
    }
    fn diff_file_contents<'a>(
        &'a self,
        subject: &'a Subject,
        commit: Option<&'a str>,
        change_type: PullRequestDiffChangeType,
        old_path: &'a str,
        new_path: &'a str,
    ) -> BoxFuture<'a, Read<PullRequestDiffFileContentsResult>> {
        Box::pin(self.protect(&subject.host, false, self.inner.diff_file_contents(subject, commit, change_type, old_path, new_path)))
    }
    fn files_viewed<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestFilesViewedResult>> {
        Box::pin(self.protect(&subject.host, false, self.inner.files_viewed(subject)))
    }
    fn set_files_viewed<'a>(&'a self, subject: &'a Subject, files: &'a [PullRequestSetFileViewed]) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.set_files_viewed(subject, files)))
    }
    fn file_revisions<'a>(&'a self, subject: &'a Subject, paths: &'a [String]) -> BoxFuture<'a, Read<FileRevisions>> {
        Box::pin(self.protect(&subject.host, false, self.inner.file_revisions(subject, paths)))
    }
    fn run_action<'a>(&'a self, subject: &'a Subject, run: &'a RunAction) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.run_action(subject, run)))
    }
    fn update<'a>(&'a self, subject: &'a Subject, title: Option<&'a str>, body: Option<&'a str>) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.update(subject, title, body)))
    }
    fn comment<'a>(&'a self, subject: &'a Subject, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.comment(subject, body)))
    }
    fn update_comment<'a>(&'a self, subject: &'a Subject, comment_id: &'a str, kind: PullRequestCommentUpdateKind, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.update_comment(subject, comment_id, kind, body)))
    }
    fn submit_review<'a>(&'a self, subject: &'a Subject, verdict: PullRequestReviewVerdict, body: &'a str, comments: &'a [PullRequestReviewCommentDraft]) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.submit_review(subject, verdict, body, comments)))
    }
    fn reviewer_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestReviewerCandidateList>> {
        Box::pin(self.protect(&subject.host, true, self.inner.reviewer_candidates(subject)))
    }
    fn set_reviewer_request<'a>(&'a self, subject: &'a Subject, reviewers: &'a [PullRequestReviewerRequestTarget], requested: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.set_reviewer_request(subject, reviewers, requested)))
    }
    fn label_candidates<'a>(&'a self, subject: &'a Subject) -> BoxFuture<'a, Read<PullRequestLabelCandidateList>> {
        Box::pin(self.protect(&subject.host, true, self.inner.label_candidates(subject)))
    }
    fn set_labels<'a>(&'a self, subject: &'a Subject, labels: &'a [&'a str], applied: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.set_labels(subject, labels, applied)))
    }
    fn reply_to_thread<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, body: &'a str) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.reply_to_thread(subject, thread_id, body)))
    }
    fn set_reaction<'a>(&'a self, subject: &'a Subject, subject_id: Option<&'a str>, content: PullRequestReactionContent, reacted: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.set_reaction(subject, subject_id, content, reacted)))
    }
    fn set_thread_resolution<'a>(&'a self, subject: &'a Subject, thread_id: &'a str, resolved: bool) -> BoxFuture<'a, Read<()>> {
        Box::pin(self.protect(&subject.host, true, self.inner.set_thread_resolution(subject, thread_id, resolved)))
    }
}
