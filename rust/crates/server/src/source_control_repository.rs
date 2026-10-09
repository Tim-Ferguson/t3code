//! Looking up, cloning and publishing repositories through a host (port of
//! `SourceControlRepositoryService.ts`). Lookup asks the host for clone URLs; clone resolves
//! them (or takes a pasted URL) and runs `git clone`; publish creates the repository on the
//! host, wires it up as a remote and pushes.
//!
//! Failures reach the wire as `SourceControlRepositoryError` with a fixed detail; the typed
//! cause (provider error, git error) is kept as its name and message only.
use crate::source_control_registry::SourceControlRegistry;
use regex::Regex;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use t3_contracts::{
    SourceControlCloneProtocol, SourceControlCloneRepositoryInput, SourceControlCloneRepositoryResult,
    SourceControlProviderError, SourceControlProviderKind, SourceControlPublishRepositoryInput,
    SourceControlPublishRepositoryResult, SourceControlPublishStatus, SourceControlRepositoryCloneUrls,
    SourceControlRepositoryError, SourceControlRepositoryInfo, SourceControlRepositoryLookupInput, ProjectCloneStage,
};
use t3_vcs::{
    core::{ExecuteGitInput, GitCommandError, GitTimeout},
    progress::GitProgress,
};

use crate::source_control_provider::{CreateRepositoryInput, RepositoryUrlsInput};

const DETAIL: &str = "The source control operation could not be completed.";
const BITBUCKET_LOCATOR_DETAIL: &str = "Bitbucket repositories must be specified as workspace/repository.";
/// The synchronous RPC keeps a deadline: nothing else tells the user a clone stalled.
const CLONE_TIMEOUT: Duration = Duration::from_secs(120);

/// A prepared clone: everything `clone_repository` checks before running git.
#[derive(Debug, Clone)]
pub struct PreparedClone {
    pub destination_path: PathBuf,
    /// Credential-free; safe to show and to store in snapshots.
    pub remote_url: String,
    /// What git is given; may carry embedded credentials.
    pub clone_url: String,
    pub repository: Option<SourceControlRepositoryInfo>,
}

/// What a repository operation can fail with before it is mapped to the wire error.
#[derive(Debug)]
enum Failure {
    Repository(SourceControlRepositoryError),
    Provider(SourceControlProviderError),
    Git(GitCommandError),
}

impl From<SourceControlRepositoryError> for Failure {
    fn from(error: SourceControlRepositoryError) -> Self {
        Self::Repository(error)
    }
}
impl From<SourceControlProviderError> for Failure {
    fn from(error: SourceControlProviderError) -> Self {
        Self::Provider(error)
    }
}
impl From<GitCommandError> for Failure {
    fn from(error: GitCommandError) -> Self {
        Self::Git(error)
    }
}

/// `mapRepositoryError`: a repository error stays as it is; anything else becomes the fixed
/// detail with its cause named (and the Bitbucket locator mistake spelled out).
fn map_failure(operation: &str, provider: SourceControlProviderKind, failure: Failure) -> SourceControlRepositoryError {
    match failure {
        Failure::Repository(error) => error,
        Failure::Provider(error) => {
            let locator = error.provider == SourceControlProviderKind::Bitbucket
                && error.cause.as_ref().is_some_and(|cause| cause.name == "BitbucketRepositoryLocatorError");
            SourceControlRepositoryError::new(provider, operation, if locator { BITBUCKET_LOCATOR_DETAIL } else { DETAIL })
                .with_cause("SourceControlProviderError", error.message())
        }
        Failure::Git(error) => {
            SourceControlRepositoryError::new(provider, operation, DETAIL).with_cause("GitCommandError", error.to_string())
        }
    }
}

fn repository_info(provider: SourceControlProviderKind, urls: &SourceControlRepositoryCloneUrls) -> SourceControlRepositoryInfo {
    SourceControlRepositoryInfo {
        provider,
        name_with_owner: urls.name_with_owner.clone(),
        url: urls.url.clone(),
        ssh_url: urls.ssh_url.clone(),
    }
}

/// The URL clients see. A pasted `https://user:token@host/…` must not travel back to every
/// reader; git still gets the original.
fn redact_remote_url(remote_url: &str) -> String {
    let Ok(mut url) = url::Url::parse(remote_url) else {
        return remote_url.to_string();
    };
    // Clone URLs have no legitimate query; when one is present it is a token.
    if url.username().is_empty() && url.password().is_none() && url.query().is_none() {
        return remote_url.to_string();
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.to_string()
}

/// Drops `user:token@` from any URL embedded in free text.
fn redact_url_credentials(text: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/]+@").unwrap())
        .replace_all(text, "$1")
        .into_owned()
}

fn select_remote_url(urls: &SourceControlRepositoryCloneUrls, protocol: Option<SourceControlCloneProtocol>) -> String {
    match protocol.unwrap_or(SourceControlCloneProtocol::Auto) {
        SourceControlCloneProtocol::Https => urls.url.clone(),
        SourceControlCloneProtocol::Ssh | SourceControlCloneProtocol::Auto => urls.ssh_url.clone(),
    }
}

/// A leading `~` or `~/…` is the home directory; everything else is the path as given.
fn expand_home(value: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from).or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from));
    if value == "~" {
        return home().unwrap_or_else(|| PathBuf::from(value));
    }
    match (value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")), home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(value),
    }
}

/// `path.resolve`: absolute, with `.` and `..` folded away, without touching the disk.
fn resolve_path(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// One parsed line of `git clone --progress` stderr (`parseGitCloneProgressLine`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneProgress {
    pub stage: ProjectCloneStage,
    /// Percent of the current stage.
    pub percent: Option<u8>,
    /// Transfer detail after the count, e.g. `12.30 MiB | 5.00 MiB/s`.
    pub detail: Option<String>,
}

pub type CloneProgressCallback = Arc<dyn Fn(CloneProgress) + Send + Sync>;

/// Git redraws each counter with a bare `\r`, so callers hand over each redraw as its own line.
/// Lines that are not progress counters (hints, warnings, `Cloning into ...`) return `None` and
/// are left for the error surface.
pub fn parse_clone_progress_line(line: &str) -> Option<CloneProgress> {
    static STAGES: OnceLock<Vec<(Regex, ProjectCloneStage)>> = OnceLock::new();
    static PERCENT: OnceLock<Regex> = OnceLock::new();
    let stages = STAGES.get_or_init(|| {
        [
            (r"^remote: Enumerating objects", ProjectCloneStage::Counting),
            (r"^remote: Counting objects", ProjectCloneStage::Counting),
            (r"^remote: Compressing objects", ProjectCloneStage::Counting),
            (r"^Receiving objects", ProjectCloneStage::Receiving),
            (r"^Resolving deltas", ProjectCloneStage::Resolving),
            (r"^Updating files", ProjectCloneStage::Checkout),
            (r"^Checking out files", ProjectCloneStage::Checkout),
        ]
        .into_iter()
        .map(|(pattern, stage)| (Regex::new(pattern).unwrap(), stage))
        .collect()
    });
    let percent = PERCENT.get_or_init(|| Regex::new(r":\s+(\d+)%\s+\((\d+)/(\d+)\)(?:,\s*(.*?))?\s*(?:,\s*done\.)?\s*$").unwrap());
    let trimmed = line.trim();
    let stage = stages.iter().find(|(pattern, _)| pattern.is_match(trimmed))?.1;
    let Some(captures) = percent.captures(trimmed) else { return Some(CloneProgress { stage, percent: None, detail: None }) };
    let value = captures.get(1).and_then(|value| value.as_str().parse::<u64>().ok()).map(|value| value.min(100) as u8);
    let raw = captures.get(4).map(|detail| detail.as_str().trim()).unwrap_or("");
    // The trailer of a finished line is "done." which carries no information.
    let detail = (!raw.is_empty() && raw != "done.").then(|| raw.to_string());
    Some(CloneProgress { stage, percent: value, detail })
}

pub struct SourceControlRepositoryService {
    registry: SourceControlRegistry,
}

impl SourceControlRepositoryService {
    pub fn new(registry: SourceControlRegistry) -> Self {
        Self { registry }
    }

    fn concrete(
        operation: &str,
        provider: SourceControlProviderKind,
    ) -> Result<SourceControlProviderKind, SourceControlRepositoryError> {
        if provider == SourceControlProviderKind::Unknown {
            return Err(SourceControlRepositoryError::new(
                provider,
                operation,
                "Choose a source control provider before continuing.",
            ));
        }
        Ok(provider)
    }

    async fn lookup(&self, input: &SourceControlRepositoryLookupInput) -> Result<SourceControlRepositoryInfo, Failure> {
        let kind = Self::concrete("lookupRepository", input.provider)?;
        let cwd = input
            .cwd
            .as_ref()
            .map(|cwd| cwd.as_str().to_string())
            .unwrap_or_else(|| self.registry.host().cwd().to_string_lossy().into_owned());
        let urls = self
            .registry
            .get(kind)
            .get_repository_clone_urls(RepositoryUrlsInput {
                cwd,
                context: None,
                repository: input.repository.as_str().trim().to_string(),
            })
            .await?;
        Ok(repository_info(kind, &urls))
    }

    pub async fn lookup_repository(
        &self,
        input: &SourceControlRepositoryLookupInput,
    ) -> Result<SourceControlRepositoryInfo, SourceControlRepositoryError> {
        self.lookup(input).await.map_err(|failure| map_failure("lookupRepository", input.provider, failure))
    }

    fn normalize_destination(destination: &str) -> Result<PathBuf, SourceControlRepositoryError> {
        let trimmed = t3_contracts::trim_wire_string(destination);
        if trimmed.is_empty() {
            return Err(SourceControlRepositoryError::new(
                SourceControlProviderKind::Unknown,
                "cloneRepository",
                "Choose a destination path before cloning.",
            ));
        }
        Ok(resolve_path(expand_home(trimmed)))
    }

    async fn prepare_destination(destination: &str) -> Result<(PathBuf, PathBuf), SourceControlRepositoryError> {
        let unknown = SourceControlProviderKind::Unknown;
        let normalized = Self::normalize_destination(destination)?;
        if tokio::fs::try_exists(&normalized).await.unwrap_or(false) {
            let mut entries = tokio::fs::read_dir(&normalized).await.map_err(|error| {
                SourceControlRepositoryError::new(unknown, "cloneRepository", "Destination path already exists and is not a directory.")
                    .with_cause("PlatformError", error.to_string())
            })?;
            if entries.next_entry().await.ok().flatten().is_some() {
                return Err(SourceControlRepositoryError::new(
                    unknown,
                    "cloneRepository",
                    "Destination path already exists and is not empty.",
                ));
            }
        } else if let Some(parent) = normalized.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| {
                SourceControlRepositoryError::new(unknown, "cloneRepository", DETAIL).with_cause("PlatformError", error.to_string())
            })?;
        }
        let parent = normalized.parent().map(Path::to_path_buf).unwrap_or_else(|| normalized.clone());
        Ok((normalized, parent))
    }

    async fn prepare(&self, input: &SourceControlCloneRepositoryInput) -> Result<PreparedClone, Failure> {
        let (destination, parent) = Self::prepare_destination(input.destination_path.as_str()).await?;
        let mut repository = None;
        let mut remote_url = input.remote_url.as_ref().map(|url| url.as_str().trim().to_string());
        let mut provider = input.provider.unwrap_or(SourceControlProviderKind::Unknown);
        if let (Some(kind), Some(name)) = (input.provider, input.repository.as_ref()) {
            let info = self
                .lookup(&SourceControlRepositoryLookupInput {
                    provider: kind,
                    repository: name.clone(),
                    cwd: t3_contracts::TrimmedNonEmptyString::new(parent.to_string_lossy()).ok(),
                })
                .await?;
            remote_url = Some(select_remote_url(
                &SourceControlRepositoryCloneUrls {
                    name_with_owner: info.name_with_owner.clone(),
                    url: info.url.clone(),
                    ssh_url: info.ssh_url.clone(),
                },
                input.protocol,
            ));
            repository = Some(info);
            provider = kind;
        }
        let Some(remote_url) = remote_url.filter(|url| !url.is_empty()) else {
            return Err(SourceControlRepositoryError::new(
                provider,
                "cloneRepository",
                "Enter a repository path or clone URL before cloning.",
            )
            .into());
        };
        Ok(PreparedClone {
            destination_path: destination,
            remote_url: redact_remote_url(&remote_url),
            clone_url: remote_url,
            repository,
        })
    }

    /// Everything `clone_repository` checks before running git, so a caller can create the
    /// project first and clone afterwards.
    pub async fn prepare_clone(
        &self,
        input: &SourceControlCloneRepositoryInput,
    ) -> Result<PreparedClone, SourceControlRepositoryError> {
        self.prepare(input)
            .await
            .map_err(|failure| map_failure("cloneRepository", input.provider.unwrap_or(SourceControlProviderKind::Unknown), failure))
    }

    pub async fn clone_repository(
        &self,
        input: &SourceControlCloneRepositoryInput,
    ) -> Result<SourceControlCloneRepositoryResult, SourceControlRepositoryError> {
        let provider = input.provider.unwrap_or(SourceControlProviderKind::Unknown);
        let prepared = self.prepare(input).await.map_err(|failure| map_failure("cloneRepository", provider, failure))?;
        self.run_clone(&prepared, provider, None, GitTimeout::After(CLONE_TIMEOUT)).await?;
        Ok(SourceControlCloneRepositoryResult {
            cwd: prepared.destination_path.to_string_lossy().into_owned(),
            remote_url: prepared.remote_url,
            repository: prepared.repository,
        })
    }

    /// Runs `git clone` for a prepared destination. Git interleaves progress redraws with its real
    /// messages on stderr: progress lines go to `on_progress`, and the last non-progress lines
    /// (credentials removed) explain a failure ("Repository not found", "Permission denied").
    /// Dropping the returned future ends git, which is how a tracked clone is cancelled.
    pub async fn run_clone(
        &self,
        prepared: &PreparedClone,
        provider: SourceControlProviderKind,
        on_progress: Option<CloneProgressCallback>,
        timeout: GitTimeout,
    ) -> Result<(), SourceControlRepositoryError> {
        let tail = Arc::new(Mutex::new(Vec::<String>::new()));
        let collector = tail.clone();
        let progress = GitProgress {
            on_stderr_line: Some(Arc::new(move |line: &str| {
                if let Some(parsed) = parse_clone_progress_line(line) {
                    if let Some(on_progress) = &on_progress {
                        on_progress(parsed);
                    }
                    return;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("Cloning into") {
                    return;
                }
                let mut lines = collector.lock().unwrap();
                lines.push(redact_url_credentials(trimmed));
                if lines.len() > 4 {
                    lines.remove(0);
                }
            })),
            keep_line_callbacks_after_truncation: true,
            ..GitProgress::default()
        };
        let parent = prepared.destination_path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = prepared.destination_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let result = self
            .registry
            .host()
            .shared
            .git
            .core()
            .execute(ExecuteGitInput {
                operation: "SourceControlRepositoryService.cloneRepository".into(),
                cwd: parent,
                args: vec!["clone".into(), "--progress".into(), "--".into(), prepared.clone_url.clone(), name],
                timeout,
                max_output_bytes: Some(256 * 1024),
                append_truncation_marker: true,
                env: vec![
                    ("GIT_PROGRESS_DELAY".into(), Some("0".into())),
                    ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
                    ("LC_ALL".into(), Some("C".into())),
                ],
                progress: Some(progress),
                ..ExecuteGitInput::default()
            })
            .await;
        if let Err(error) = result {
            let lines = tail.lock().unwrap().clone();
            let detail = if lines.is_empty() { "The repository could not be cloned.".to_string() } else { lines.join(" ") };
            return Err(SourceControlRepositoryError::new(provider, "cloneRepository", detail).with_cause("GitCommandError", error.to_string()));
        }
        Ok(())
    }

    async fn publish(
        &self,
        input: &SourceControlPublishRepositoryInput,
    ) -> Result<SourceControlPublishRepositoryResult, Failure> {
        let kind = Self::concrete("publishRepository", input.provider)?;
        let cwd = input.cwd.as_str();
        let urls = self
            .registry
            .get(kind)
            .create_repository(CreateRepositoryInput {
                cwd: cwd.to_string(),
                repository: input.repository.as_str().trim().to_string(),
                visibility: input.visibility,
            })
            .await?;
        let remote_url = select_remote_url(&urls, input.protocol);
        let core = self.registry.host().shared.git.core();
        let path = Path::new(cwd);
        let preferred = input.remote_name.as_ref().map(|name| name.as_str().trim()).filter(|name| !name.is_empty()).unwrap_or("origin");
        let remote_name = core.ensure_remote(path, preferred, &remote_url).await?;

        // An empty local repo (no commits) would make `git push HEAD:...` fail with an opaque
        // "src refspec HEAD does not match any". Treat this as a partial success: the remote
        // was created and wired up, but there is nothing to push yet.
        let has_commits = core.resolve_commit(path, "HEAD").await.is_ok();
        if !has_commits {
            let branch = core.status_details(path).await.ok().and_then(|details| details.branch).unwrap_or_else(|| "main".into());
            return Ok(SourceControlPublishRepositoryResult {
                repository: repository_info(kind, &urls),
                remote_name,
                remote_url,
                branch,
                upstream_branch: None,
                status: SourceControlPublishStatus::RemoteAdded,
            });
        }
        let pushed = core.push_current_branch(path, None, Some(&remote_name)).await?;
        Ok(SourceControlPublishRepositoryResult {
            repository: repository_info(kind, &urls),
            remote_name,
            remote_url,
            branch: pushed.branch,
            upstream_branch: pushed.upstream_branch,
            status: SourceControlPublishStatus::Pushed,
        })
    }

    pub async fn publish_repository(
        &self,
        input: &SourceControlPublishRepositoryInput,
    ) -> Result<SourceControlPublishRepositoryResult, SourceControlRepositoryError> {
        self.publish(input).await.map_err(|failure| map_failure("publishRepository", input.provider, failure))
    }

    /// Removes a partial or failed clone so the destination is empty again. Only what git left
    /// behind may go: anything without a `.git` inside was put there by someone else since.
    pub async fn discard_clone(&self, destination: &str) -> Result<(), SourceControlRepositoryError> {
        let unknown = SourceControlProviderKind::Unknown;
        let failure = |detail: &str| SourceControlRepositoryError::new(unknown, "discardClone", detail);
        let normalized = Self::normalize_destination(destination).map_err(|error| SourceControlRepositoryError { operation: "discardClone".into(), ..error })?;
        let mut names = Vec::new();
        match tokio::fs::read_dir(&normalized).await {
            Ok(mut entries) => {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(failure("The clone destination could not be inspected.").with_cause("PlatformError", error.to_string()));
            }
        }
        if !names.is_empty() && !names.iter().any(|name| name == ".git") {
            return Err(failure("Destination path contains files that are not from the clone."));
        }
        // The directory itself is the project's workspace root and must stay; only git's partial
        // contents go. An aborted git may still be closing files, so removal retries briefly.
        let mut removed = Ok(());
        for attempt in 0..=5 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            removed = async {
                match tokio::fs::remove_dir_all(&normalized).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                tokio::fs::create_dir_all(&normalized).await
            }
            .await;
            if removed.is_ok() {
                break;
            }
        }
        removed.map_err(|error| failure("The partial clone could not be removed.").with_cause("PlatformError", error.to_string()))
    }
}
