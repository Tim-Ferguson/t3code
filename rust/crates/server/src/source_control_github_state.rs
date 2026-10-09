//! The GitHub state that outlives a request: the held tokens, the rate-limit pauses, the
//! GraphQL ledger and the bound on concurrent API requests. Owned by `Host::Shared`.
use crate::{
    source_control_github_budget::GraphQlBudget,
    source_control_github_credentials::{GitHubCredential, GitHubCredentialError},
    source_control_rate_limit::RateLimit,
    vcs_cache::TtlCache,
};
use std::sync::Arc;
use t3_vcs::Clock;
use tokio::sync::Semaphore;

/// Polite to GitHub's secondary limits, which punish bursts of concurrent requests.
const API_CONCURRENCY: usize = 8;
const CREDENTIAL_CACHE_CAPACITY: usize = 32;

pub struct GitHubShared {
    pub limits: RateLimit,
    pub budget: GraphQlBudget,
    pub gate: Semaphore,
    pub(crate) credentials: TtlCache<String, GitHubCredential, GitHubCredentialError>,
}

impl GitHubShared {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            limits: RateLimit::default(),
            budget: GraphQlBudget::default(),
            gate: Semaphore::new(API_CONCURRENCY),
            credentials: TtlCache::new(CREDENTIAL_CACHE_CAPACITY, clock),
        }
    }
}
