//! Matching a checkout to its host (port of `SourceControlProviderRegistry.ts`). The registry
//! lists the checkout's remotes, detects a host for each from its URL, prefers `origin`, then
//! any recognised host, and asks the signed-in CLIs about a remote nobody recognised (a
//! self-hosted GitLab on an ordinary-looking host name). The answer is kept for five seconds
//! per working directory, failures not at all.
//!
//! It also hands out the provider for a kind, bound to the detected context so callers need not
//! carry it, and runs discovery across every registered host.
use crate::{
    source_control_detect::detect_provider_from_remote_url,
    source_control_discovery::{
        DiscoverySpec, ManagedProbe, discover, refine_unknown_remote_provider,
    },
    source_control_host::Host,
    source_control_provider::*,
};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use t3_contracts::{
    SourceControlDiscoveryResult, SourceControlProviderError, SourceControlProviderInfo,
    SourceControlProviderKind,
};

pub const PROVIDER_DETECTION_CACHE_CAPACITY: usize = 2_048;
pub const PROVIDER_DETECTION_CACHE_TTL: Duration = Duration::from_secs(5);

pub struct Registration {
    pub kind: SourceControlProviderKind,
    pub provider: Arc<dyn SourceControlProvider>,
    pub discovery: DiscoverySpec,
}

#[derive(Clone)]
pub struct ProviderHandle {
    pub provider: Arc<dyn SourceControlProvider>,
    pub context: Option<ProviderContext>,
}

/// The registrations of every host implemented so far.
pub fn default_registrations(host: &Host) -> Vec<Registration> {
    use crate::source_control_github_provider::{GitHubProbe, GitHubProvider};
    let github_probe: Arc<dyn ManagedProbe> = Arc::new(GitHubProbe::new(host.clone()));
    vec![
        Registration {
            kind: SourceControlProviderKind::Github,
            provider: Arc::new(GitHubProvider::new(host.clone())),
            discovery: DiscoverySpec::Managed(github_probe),
        },
        Registration {
            kind: SourceControlProviderKind::Gitlab,
            provider: Arc::new(crate::source_control_gitlab::GitLabProvider::new(host.clone())),
            discovery: DiscoverySpec::Cli(crate::source_control_gitlab::cli_spec()),
        },
        Registration {
            kind: SourceControlProviderKind::AzureDevops,
            provider: Arc::new(crate::source_control_azure_devops::AzureDevOpsProvider::new(host.clone())),
            discovery: DiscoverySpec::Cli(crate::source_control_azure_devops::cli_spec()),
        },
        Registration {
            kind: SourceControlProviderKind::Bitbucket,
            provider: Arc::new(crate::source_control_bitbucket::BitbucketProvider::new(host.clone())),
            discovery: DiscoverySpec::Api(Arc::new(crate::source_control_bitbucket::BitbucketProbe::new(host.clone()))),
        },
    ]
}

pub struct SourceControlRegistry {
    host: Host,
    providers: HashMap<SourceControlProviderKind, Arc<dyn SourceControlProvider>>,
    specs: Vec<DiscoverySpec>,
}

/// `selectProviderContext`: `origin` first, then the first remote with a recognised host, then
/// the first remote with any host at all.
pub fn select_provider_context(remotes: &[(String, String)]) -> Option<ProviderContext> {
    let candidates: Vec<ProviderContext> = remotes
        .iter()
        .filter_map(|(name, url)| {
            Some(ProviderContext {
                provider: detect_provider_from_remote_url(url)?,
                remote_name: name.clone(),
                remote_url: url.clone(),
                requested_host: None,
            })
        })
        .collect();
    candidates
        .iter()
        .find(|candidate| candidate.remote_name == "origin")
        .or_else(|| candidates.iter().find(|candidate| candidate.provider.kind != SourceControlProviderKind::Unknown))
        .or(candidates.first())
        .cloned()
}

fn detection_failure(cwd: &str, cause: &t3_vcs::VcsError) -> SourceControlProviderError {
    SourceControlProviderError::new(
        SourceControlProviderKind::Unknown,
        "detectProvider",
        cwd,
        "Failed to detect source control provider.",
    )
    .with_cause(cause.tag(), cause.to_string())
}

impl SourceControlRegistry {
    pub fn new(host: Host) -> Self {
        Self::with_registrations(host.clone(), default_registrations(&host))
    }

    pub fn with_registrations(host: Host, registrations: Vec<Registration>) -> Self {
        let mut providers = HashMap::new();
        let mut specs = Vec::new();
        for registration in registrations {
            providers.insert(registration.kind, registration.provider);
            specs.push(registration.discovery);
        }
        Self { host, providers, specs }
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The provider for a kind; one that is not registered fails every operation by name.
    pub fn get(&self, kind: SourceControlProviderKind) -> Arc<dyn SourceControlProvider> {
        self.providers
            .get(&kind)
            .cloned()
            .unwrap_or_else(|| Arc::new(UnsupportedProvider(kind)))
    }

    async fn detect_provider_context(&self, cwd: &str) -> Result<Option<ProviderContext>, SourceControlProviderError> {
        let handle = self
            .host
            .shared
            .vcs
            .resolve(cwd, None)
            .await
            .map_err(|error| detection_failure(cwd, &error))?;
        let driver = self.host.shared.vcs.driver(handle.kind).map_err(|error| detection_failure(cwd, &error))?;
        let remotes = driver
            .list_remotes(Path::new(cwd))
            .await
            .map_err(|error| detection_failure(cwd, &error))?;
        let remotes: Vec<(String, String)> =
            remotes.remotes.into_iter().map(|remote| (remote.name, remote.url)).collect();
        Ok(refine_unknown_remote_provider(&self.host, &self.specs, cwd, select_provider_context(&remotes)).await)
    }

    /// The provider for the checkout at `cwd`, with the context it was detected from. An
    /// explicit `context` skips detection (and is still refined when its host is unknown).
    pub async fn resolve_handle(
        &self,
        cwd: &str,
        context: Option<ProviderContext>,
    ) -> Result<ProviderHandle, SourceControlProviderError> {
        let context = match context {
            Some(context) => refine_unknown_remote_provider(&self.host, &self.specs, cwd, Some(context)).await,
            None => {
                self.host
                    .shared
                    .contexts
                    .get(
                        cwd.to_string(),
                        self.detect_provider_context(cwd),
                        |result| {
                            if result.is_ok() { PROVIDER_DETECTION_CACHE_TTL } else { Duration::ZERO }
                        },
                    )
                    .await?
            }
        };
        let kind = context.as_ref().map_or(SourceControlProviderKind::Unknown, |context| context.provider.kind);
        let provider = self.get(kind);
        Ok(ProviderHandle {
            provider: match &context {
                Some(context) => Arc::new(BoundProvider { inner: provider, context: context.clone() }),
                None => provider,
            },
            context,
        })
    }

    pub async fn resolve(&self, cwd: &str) -> Result<Arc<dyn SourceControlProvider>, SourceControlProviderError> {
        Ok(self.resolve_handle(cwd, None).await?.provider)
    }

    /// The host a status badge shows for `cwd` on `branch`: the URL of the remote the branch
    /// tracks (else `origin`), and for a host nobody recognises by name, what the signed-in
    /// CLIs say. `None` when the checkout has no remote with a host.
    pub async fn resolve_hosting_provider(
        &self,
        core: &t3_vcs::GitCore,
        cwd: &str,
        branch: Option<&str>,
    ) -> Option<SourceControlProviderInfo> {
        let path = Path::new(cwd);
        let preferred = match branch {
            None => "origin".to_string(),
            Some(branch) => core
                .read_config_value(path, &format!("branch.{branch}.remote"))
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "origin".into()),
        };
        let remote_url = match core.read_config_value(path, &format!("remote.{preferred}.url")).await.ok()? {
            Some(url) => url,
            None => core.read_config_value(path, "remote.origin.url").await.ok()??,
        };
        let provider = detect_provider_from_remote_url(&remote_url);
        match &provider {
            Some(info) if info.kind == SourceControlProviderKind::Unknown => {}
            _ => return provider,
        }
        let context = ProviderContext {
            provider: provider.clone()?,
            remote_name: preferred,
            remote_url,
            requested_host: None,
        };
        match self.resolve_handle(cwd, Some(context)).await {
            Ok(handle) => handle.context.map(|context| context.provider).or(provider),
            Err(_) => provider,
        }
    }

    /// A linked subject by URL, independent of any checkout. Only `https` URLs without
    /// credentials are looked at, and only by the host that recognises them.
    pub fn resolve_link(
        &self,
        cwd: &str,
        url: &url::Url,
    ) -> Option<futures_util::future::BoxFuture<'static, ProviderResult<LinkSubject>>> {
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return None;
        }
        let kind = detect_provider_from_remote_url(url.as_str())?.kind;
        self.providers.get(&kind)?.resolve_link(cwd, url)
    }

    pub async fn discover(&self) -> SourceControlDiscoveryResult {
        discover(&self.host, &self.specs).await
    }
}

/// The `sourceControlProvider` of a local status result, as its wire value: the host of the
/// remote the checked-out branch tracks.
pub async fn hosting_provider_for_status(
    core: &t3_vcs::GitCore,
    cwd: &str,
    branch: Option<&str>,
) -> Option<serde_json::Value> {
    hosting_provider_for_status_with(Host::process_default(), core, cwd, branch).await
}

/// The same, asked through `host`, which is the one that carries the server's settings.
pub async fn hosting_provider_for_status_with(
    host: Host,
    core: &t3_vcs::GitCore,
    cwd: &str,
    branch: Option<&str>,
) -> Option<serde_json::Value> {
    let registry = SourceControlRegistry::new(host);
    let provider = registry.resolve_hosting_provider(core, cwd, branch).await?;
    serde_json::to_value(provider).ok()
}
