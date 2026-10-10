//! What every source control host shares: how to run a host CLI, read the server environment,
//! reach the settings, ask git about a checkout, and the process-wide state (token cache, rate
//! limit pauses, GraphQL budget) that must outlive a single request.
//!
//! A `Host` is cheap to clone. Clones share one `Shared`; `with_settings` makes a view that
//! reads another settings service, which is how a request brings its server's settings
//! without rebuilding any state.
//!
//! Host CLIs run through `VcsProcess`, the same runner git uses, so they get its limits,
//! timeouts and (for `gh`) its concurrency bound. Nothing here reads the real environment
//! unless `HostEnv::from_process` is asked to; tests build their environment explicitly and
//! put fake CLIs on `HostOptions::path`.
use crate::{
    server_settings::SettingsService,
    source_control_github_state::GitHubShared,
    source_control_provider::ProviderContext,
    source_control_registry::{PROVIDER_DETECTION_CACHE_CAPACITY},
    vcs_cache::TtlCache,
    vcs_registry::VcsRegistry,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use t3_contracts::{ServerSettings, SourceControlProviderError};
use t3_vcs::{Clock, GitVcs, SystemClock, VcsProcess, process::VcsProcessInput};

/// The server's environment variables as the host layer sees them (`HostProcessEnvironment`).
#[derive(Debug, Clone, Default)]
pub struct HostEnv(HashMap<String, String>);

impl HostEnv {
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self(pairs.into_iter().map(|(k, v)| (k.into(), v.into())).collect())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// The value with surrounding whitespace removed, or `None` when blank.
    pub fn get_trimmed(&self, name: &str) -> Option<&str> {
        self.get(name).map(str::trim).filter(|value| !value.is_empty())
    }
}

/// Where the GitHub API for a host lives: `(rest, graphql)` endpoints.
pub type GitHubApiResolver = Arc<dyn Fn(&str) -> (String, String) + Send + Sync>;

pub struct HostOptions {
    pub env: HostEnv,
    /// The server's working directory, used to probe CLIs outside any checkout.
    pub cwd: PathBuf,
    /// Replaces `PATH` for every host CLI that is spawned. Tests use it to put a fake CLI first.
    pub path: Option<String>,
    pub clock: Arc<dyn Clock>,
    /// Redirects GitHub API requests; `None` uses the real endpoints.
    pub github_api: Option<GitHubApiResolver>,
    pub process: VcsProcess,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            env: HostEnv::from_process(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            path: None,
            clock: Arc::new(SystemClock),
            github_api: None,
            process: VcsProcess::new(),
        }
    }
}

pub struct Shared {
    pub process: VcsProcess,
    pub git: GitVcs,
    pub vcs: VcsRegistry,
    pub env: HostEnv,
    pub cwd: PathBuf,
    pub path: Option<String>,
    pub clock: Arc<dyn Clock>,
    pub http: reqwest::Client,
    pub github_api: Option<GitHubApiResolver>,
    pub github: GitHubShared,
    /// Detected hosts per working directory (`SourceControlRegistry::resolve_handle`).
    pub(crate) contexts: TtlCache<String, Option<ProviderContext>, SourceControlProviderError>,
    /// A Bitbucket pull request's file versions, parsed out of its patch and shared by the ticks that
    /// follow one another (`BitbucketPullRequestApi`'s `revisionPatches`).
    pub(crate) bitbucket_patches: TtlCache<String, Arc<crate::pull_request_bitbucket::ParsedPatch>, crate::pull_request_github::PrFailure>,
    /// What an Azure DevOps provider remembers between requests: the registry builds a provider
    /// per request here, where the original builds one once, so the allowance of `az` processes and
    /// the pull request locations live with the host.
    pub(crate) azure: crate::pull_request_azure::AzureShared,
    /// The token `fj` last renewed for each server, believed for thirty seconds.
    pub(crate) forgejo: crate::source_control_forgejo::ForgejoShared,
}

static PROCESS_HOST: std::sync::OnceLock<Host> = std::sync::OnceLock::new();

#[derive(Clone)]
pub struct Host {
    pub shared: Arc<Shared>,
    settings: Option<SettingsService>,
}

impl Host {
    pub fn new(options: HostOptions) -> Self {
        let git = GitVcs::new(options.process.clone());
        let vcs = VcsRegistry::new(git.clone(), options.clock.clone());
        Self {
            shared: Arc::new(Shared {
                process: options.process,
                git,
                vcs,
                env: options.env,
                cwd: options.cwd,
                path: options.path,
                github: GitHubShared::new(options.clock.clone()),
                contexts: TtlCache::new(PROVIDER_DETECTION_CACHE_CAPACITY, options.clock.clone()),
                bitbucket_patches: TtlCache::new(crate::pull_request_bitbucket::REVISION_PATCH_CAPACITY, options.clock.clone()),
                azure: Default::default(),
                forgejo: Default::default(),
                clock: options.clock,
                http: reqwest::Client::new(),
                github_api: options.github_api,
            }),
            settings: None,
        }
    }

    /// The process's host: the real environment and system clock unless `install` put another
    /// in place first. Settings come per request.
    pub fn process_default() -> Host {
        PROCESS_HOST.get_or_init(|| Host::new(HostOptions::default())).clone()
    }

    /// Makes `host` the process's host, for tests that need a fake `PATH` or API endpoint. It
    /// must run before the first use; `false` when a host is already in place.
    #[doc(hidden)]
    pub fn install(host: Host) -> bool {
        PROCESS_HOST.set(host).is_ok()
    }

    pub fn with_settings(&self, settings: Option<SettingsService>) -> Host {
        Host {
            shared: self.shared.clone(),
            settings,
        }
    }

    pub fn env(&self) -> &HostEnv {
        &self.shared.env
    }

    pub fn cwd(&self) -> &Path {
        &self.shared.cwd
    }

    pub fn process(&self) -> &VcsProcess {
        &self.shared.process
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.shared.clock
    }

    /// Milliseconds since the Unix epoch on the host's clock.
    pub fn now_ms(&self) -> i64 {
        self.shared.clock.utc_now().timestamp_millis()
    }

    /// The materialized settings (tokens included), or `None` when there is no settings
    /// service or it cannot be read. Callers fall back to defaults, as the original does.
    pub async fn settings(&self) -> Option<ServerSettings> {
        self.settings.as_ref()?.snapshot().await.ok()
    }

    /// A host CLI invocation: `PATH` replaced when the host was built with one.
    pub fn cli(
        &self,
        operation: &str,
        command: &str,
        args: impl IntoIterator<Item = impl Into<String>>,
        cwd: impl Into<PathBuf>,
    ) -> VcsProcessInput {
        let mut input = VcsProcessInput::new(operation, command, args, cwd);
        if let Some(path) = &self.shared.path {
            input.env.push(("PATH".into(), Some(path.clone())));
        }
        input
    }
}
