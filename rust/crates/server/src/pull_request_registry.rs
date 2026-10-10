//! The hosts this build can read change requests from (port of `PullRequestProviderRegistry.ts`).
//! A host with no entry still shows up in the provider list as unimplemented, so its projects are
//! explained rather than missing. Every host but GitHub, which pauses itself per credential, is
//! wrapped in the rate-limit pause the service owns.
use crate::{
    pull_request_github_provider::GitHubProvider, pull_request_gitlab::GitLabPullRequests, pull_request_guard::Guarded,
    pull_request_provider::PullRequestProvider, source_control_host::Host, source_control_rate_limit::RateLimit,
};
use std::sync::Arc;
use t3_contracts::SourceControlProviderKind;
use t3_vcs::Clock;

pub fn supported(kind: SourceControlProviderKind) -> bool {
    matches!(kind, SourceControlProviderKind::Github | SourceControlProviderKind::Gitlab)
}

pub fn provider(host: &Host, kind: SourceControlProviderKind, limits: &Arc<RateLimit>, clock: &Arc<dyn Clock>) -> Option<Arc<dyn PullRequestProvider>> {
    let guarded = |inner: Arc<dyn PullRequestProvider>| -> Arc<dyn PullRequestProvider> { Arc::new(Guarded::new(inner, limits.clone(), clock.clone())) };
    match kind {
        SourceControlProviderKind::Github => Some(Arc::new(GitHubProvider::new(host.clone()))),
        SourceControlProviderKind::Gitlab => Some(guarded(Arc::new(GitLabPullRequests::new(host.clone())))),
        _ => None,
    }
}
