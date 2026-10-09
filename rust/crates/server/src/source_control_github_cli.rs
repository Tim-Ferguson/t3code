//! GitHub operations (port of `GitHubCli.ts`). Despite the name nothing here spawns `gh` for
//! API work: pull requests and repositories are read and written through the GitHub API with
//! whatever credential `source_control_github_credentials` finds, and `gh` is only the last
//! place a token can come from. Git does the local half (picking the repository from remotes,
//! checking a pull request out).
//!
//! Divergence from the original: head lookups that arrive together are not batched into one
//! GraphQL document. `build_pull_requests_by_head_query` builds the batched document and is
//! tested for it, but each call here sends its own single-lookup document.
use crate::{
    source_control_decode::trimmed_non_empty,
    source_control_detect::{detect_provider_from_remote_url, is_ssh_remote_url, js_host},
    source_control_github_api::{self as api, GitHubApiError, GraphQlInput, Method, RestInput},
    source_control_github_credentials::GitHubCredentialError,
    source_control_github_prs::{decode_pull_request, decode_pull_requests},
    source_control_host::Host,
    source_control_uri::encode_uri_component,
};
use regex::Regex;
use serde_json::{Value, json};
use std::{path::Path, sync::OnceLock, time::Duration};
use t3_contracts::{
    ChangeRequest, ErrorCause, SourceControlProviderKind, SourceControlRepositoryCloneUrls,
    SourceControlRepositoryVisibility,
};
use t3_vcs::{
    core::{ExecuteGitInput, GitCommandError},
    remote_refs::normalize_git_remote_url,
};

/// A failure of a GitHub operation, as the original's `GitHubCli*Error` classes carry it.
/// `tag` is the class name; `message` is what the provider layer reports as the detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubCliError {
    pub tag: &'static str,
    pub cwd: String,
    pub message: String,
    pub retry_at: Option<i64>,
    pub http_status: Option<u16>,
    /// The error this one wraps, as it reaches the wire.
    pub cause: Option<ErrorCause>,
}

impl GitHubCliError {
    fn new(tag: &'static str, cwd: &str, message: impl Into<String>) -> Self {
        Self { tag, cwd: cwd.into(), message: message.into(), retry_at: None, http_status: None, cause: None }
    }

    fn caused_by(self, cause: ErrorCause) -> Self {
        Self { cause: Some(cause), ..self }
    }

    /// `GitHubCliCommandError` over a plain `Error(reason)`: its message is the reason, or the
    /// generic one when that is blank.
    fn command(cwd: &str, reason: &str) -> Self {
        let trimmed = reason.trim();
        Self::new(
            "GitHubCliCommandError",
            cwd,
            if trimmed.is_empty() { "GitHub request failed." } else { trimmed },
        )
        .caused_by(ErrorCause::new("Error", reason))
    }

    /// The same error over a cause that is not an `Error` (a schema failure), which has no
    /// message of its own.
    fn command_without_reason(cwd: &str) -> Self {
        Self::new("GitHubCliCommandError", cwd, "GitHub request failed.")
    }

    fn not_found(cwd: &str, cause: ErrorCause) -> Self {
        Self::new(
            "GitHubPullRequestNotFoundError",
            cwd,
            "Pull request not found. Check the PR number or URL and try again.",
        )
        .caused_by(cause)
    }

    /// A decode failure; its schema-issue cause has no wire form here.
    fn decode(tag: &'static str, cwd: &str, message: &str) -> Self {
        Self::new(tag, cwd, message)
    }

    /// Maps a GitHub API failure onto the errors callers of this service already handle.
    pub fn from_api(cwd: &str, error: GitHubApiError) -> Self {
        let cause = ErrorCause::new(error.tag(), error.message());
        match error {
            GitHubApiError::Credential(GitHubCredentialError::CliMissing { .. }) => Self::new(
                "GitHubCliUnavailableError",
                cwd,
                "No GitHub credential on the server. Set GH_TOKEN, or install the GitHub CLI and run `gh auth login`.",
            )
            .caused_by(cause),
            GitHubApiError::Credential(credential) if credential.is_unavailable() => {
                Self::new("GitHubCliAuthenticationError", cwd, credential.message()).caused_by(cause)
            }
            GitHubApiError::Authentication { .. } => Self::new(
                "GitHubCliAuthenticationError",
                cwd,
                "GitHub is not authenticated. Run `gh auth login` (or set GH_TOKEN) and retry.",
            )
            .caused_by(cause),
            GitHubApiError::Credential(_) | GitHubApiError::Request { .. } => {
                Self::new("GitHubCliCommandError", cwd, error.message()).caused_by(cause)
            }
            GitHubApiError::RateLimit { retry_at, .. } => Self::rate_limit(cwd, retry_at).caused_by(cause),
            GitHubApiError::Paused(paused) => Self::rate_limit(cwd, Some(paused.retry_at)).caused_by(cause),
            GitHubApiError::NotFound { .. } => Self::not_found(cwd, cause),
            GitHubApiError::Response { status, .. } => Self {
                http_status: Some(status),
                ..Self::new("GitHubCliCommandError", cwd, error.message()).caused_by(cause)
            },
        }
    }

    fn rate_limit(cwd: &str, retry_at: Option<i64>) -> Self {
        Self {
            retry_at,
            ..Self::new(
                "GitHubCliRateLimitError",
                cwd,
                "GitHub API rate limit exceeded. Requests resume when the limit resets.",
            )
        }
    }

    fn git(cwd: &str, error: &GitCommandError) -> Self {
        Self::new("GitHubCliCommandError", cwd, error.to_string())
            .caused_by(ErrorCause::new("GitCommandError", error.to_string()))
    }
}

impl std::fmt::Display for GitHubCliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// A repository on a GitHub host: the API it is read through, and its owner and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepositoryLocator {
    pub host: String,
    pub owner: String,
    pub name: String,
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

fn fetch_remote_line() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    regex(&PATTERN, r"^(\S+)\s+(\S+)\s+\(fetch\)$")
}

/// `owner/name` or `host/owner/name`, as `gh --repo` and GH_REPO take them, or a repository URL.
pub fn parse_repository_selector(selector: &str, default_host: &str) -> Option<GitHubRepositoryLocator> {
    static SUFFIX: OnceLock<Regex> = OnceLock::new();
    static HTTP: OnceLock<Regex> = OnceLock::new();
    let trimmed = t3_contracts::trim_wire_string(selector);
    let trimmed = regex(&SUFFIX, r"(?i)\.git$").replace(trimmed, "").into_owned();
    if regex(&HTTP, r"(?i)^https?://").is_match(&trimmed) {
        let url = url::Url::parse(&trimmed).ok()?;
        let segments: Vec<&str> = url.path().split('/').filter(|part| !part.is_empty()).collect();
        return match segments.as_slice() {
            [owner, name] => Some(GitHubRepositoryLocator {
                host: js_host(&url).to_lowercase(),
                owner: owner.to_string(),
                name: name.to_string(),
            }),
            _ => None,
        };
    }
    let parts: Vec<&str> = trimmed.split('/').filter(|part| !part.is_empty()).collect();
    match parts.as_slice() {
        [owner, name] => Some(GitHubRepositoryLocator {
            host: default_host.to_string(),
            owner: owner.to_string(),
            name: name.to_string(),
        }),
        [host, owner, name] => Some(GitHubRepositoryLocator {
            host: host.to_lowercase(),
            owner: owner.to_string(),
            name: name.to_string(),
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestReference {
    Number(u64),
    Url { locator: GitHubRepositoryLocator, number: u64 },
    Branch(String),
}

/// A pull request reference the way `gh pr view` takes one: a number (`#7` too), a pull
/// request URL, or a branch name.
pub fn parse_pull_request_reference(reference: &str) -> PullRequestReference {
    static NUMBERED: OnceLock<Regex> = OnceLock::new();
    static HTTP: OnceLock<Regex> = OnceLock::new();
    static PATH: OnceLock<Regex> = OnceLock::new();
    let trimmed = t3_contracts::trim_wire_string(reference);
    if let Some(captures) = regex(&NUMBERED, r"^#?([1-9]\d*)$").captures(trimmed) {
        return PullRequestReference::Number(captures[1].parse().unwrap_or(u64::MAX));
    }
    if regex(&HTTP, r"(?i)^https?://").is_match(trimmed)
        && let Ok(url) = url::Url::parse(trimmed)
        && let Some(captures) =
            regex(&PATH, r"^/([^/]+)/([^/]+)/pull/([1-9]\d*)(?:/.*)?$").captures(url.path())
    {
        return PullRequestReference::Url {
            locator: GitHubRepositoryLocator {
                host: js_host(&url).to_lowercase(),
                owner: captures[1].to_string(),
                name: captures[2].to_string(),
            },
            number: captures[3].parse().unwrap_or(u64::MAX),
        };
    }
    PullRequestReference::Branch(trimmed.to_string())
}

/// The GitHub host a remote URL is served from, or `None` for a remote that is not GitHub. An
/// SSH alias (`git@github-work:owner/repo`) names no API host of its own; it is read through
/// `github.com`, which is what such an alias almost always stands for.
pub fn github_api_host_for_remote(remote_url: &str) -> Option<String> {
    let provider = detect_provider_from_remote_url(remote_url)?;
    let host = url::Url::parse(&provider.base_url).ok().map(|url| js_host(&url).to_lowercase())?;
    // A dotless SSH host is an alias from ~/.ssh/config, never a real API host.
    if is_ssh_remote_url(remote_url) && !host.contains('.') {
        return host.contains("github").then(|| "github.com".to_string());
    }
    (provider.kind == SourceControlProviderKind::Github).then_some(host)
}

/// A caller's host hint, read the way a remote's host is: a dotless alias is not an API host.
fn api_host_for_hint(host: &str) -> String {
    let normalized = host.to_lowercase();
    if !normalized.contains('.') && normalized.contains("github") {
        "github.com".into()
    } else {
        normalized
    }
}

/// The repository `gh pr list` reads in a checkout, picked the way gh picks one without a
/// prompt: the remote `gh repo set-default` marked, else the first of upstream, github, origin
/// (in any case), else the only remote. `remotes` is `git remote -v` output and `resolved` is
/// the output of `git config --get-regexp '^remote\..*\.gh-resolved$'`.
///
/// `None` whenever gh might weigh the remotes differently: a remote on another host or under
/// an SSH alias, more than one mark, or several remotes with none of those names.
pub fn select_base_repository(remotes: &str, resolved: &str, host: &str) -> Option<(String, String)> {
    static MARK: OnceLock<Regex> = OnceLock::new();
    let host = host.to_lowercase();
    let mut repositories: Vec<(String, (String, String))> = Vec::new();
    for line in remotes.split('\n') {
        let Some(captures) = fetch_remote_line().captures(line.trim()) else {
            continue;
        };
        let normalized = normalize_git_remote_url(&captures[2]);
        let mut parts = normalized.split('/');
        let (remote_host, owner, name) = (parts.next(), parts.next(), parts.next());
        let (Some(remote_host), Some(owner), Some(name)) = (remote_host, owner, name) else {
            return None;
        };
        if remote_host != host || owner.is_empty() || name.is_empty() || parts.next().is_some() {
            return None;
        }
        let entry = (owner.to_string(), name.to_string());
        match repositories.iter_mut().find(|(remote, _)| *remote == captures[1]) {
            Some(existing) => existing.1 = entry,
            None => repositories.push((captures[1].to_string(), entry)),
        }
    }
    let marks: Vec<(String, String)> = resolved
        .split('\n')
        .filter_map(|line| {
            let captures = regex(&MARK, r"^remote\.(.+)\.gh-resolved\s+(\S+)$").captures(line.trim())?;
            repositories
                .iter()
                .any(|(remote, _)| *remote == captures[1])
                .then(|| (captures[1].to_string(), captures[2].to_string()))
        })
        .collect();
    if marks.len() > 1 {
        return None;
    }
    if let Some((remote, value)) = marks.first() {
        if value == "base" {
            return repositories.iter().find(|(name, _)| name == remote).map(|(_, repo)| repo.clone());
        }
        let lowered = value.to_lowercase();
        let mut parts = lowered.split('/');
        return match (parts.next(), parts.next(), parts.next()) {
            (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty() => {
                Some((owner.to_string(), name.to_string()))
            }
            _ => None,
        };
    }
    // gh sorts remotes by these names, case-insensitively, and takes the first. A tie for the
    // top place has no defined winner.
    let score = |name: &str| ["origin", "github", "upstream"].iter().position(|known| *known == name.to_lowercase()).map_or(0, |at| at + 1);
    let mut ranked: Vec<&(String, (String, String))> = repositories.iter().collect();
    ranked.sort_by_key(|(name, _)| std::cmp::Reverse(score(name)));
    match (ranked.first(), ranked.get(1)) {
        (Some(top), next) if next.is_none_or(|next| score(&top.0) > score(&next.0)) => Some(top.1.clone()),
        _ => None,
    }
}

/// The local branch a pull request checks out into, the way `gh pr checkout` names it. gh
/// prefixes the owner only where the fork's branch would take over the default branch's name.
pub fn checkout_branch_name(
    head_ref_name: &str,
    head_owner: Option<&str>,
    is_cross_repository: bool,
    default_branch: Option<&str>,
) -> String {
    match (head_owner, default_branch) {
        (Some(owner), Some(default)) if is_cross_repository && head_ref_name == default => {
            format!("{owner}/{head_ref_name}")
        }
        _ => head_ref_name.to_string(),
    }
}

/// The pull request fields every read selects, in GraphQL.
const PULL_REQUEST_NODE_SELECTION: &str = "number title url baseRefName headRefName headRefOid state isDraft mergedAt closedAt updatedAt isCrossRepository headRepository { name nameWithOwner } headRepositoryOwner { login }";
/// Rows read for a fork's `owner:branch` head, which GitHub cannot filter by owner.
const OWNER_HEAD_SCAN_LIMIT: u32 = 100;
/// A full document is 5,000 rows of well under 2 KB each.
const HEAD_LOOKUP_MAX_RESPONSE_BYTES: usize = 16_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestListState {
    Open,
    Closed,
    Merged,
    All,
}

impl PullRequestListState {
    fn graphql(self) -> Vec<&'static str> {
        match self {
            Self::Open => vec!["OPEN"],
            Self::Closed => vec!["CLOSED"],
            Self::Merged => vec!["MERGED"],
            Self::All => vec!["OPEN", "CLOSED", "MERGED"],
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeadLookup {
    pub head_ref_name: String,
    pub state: PullRequestListState,
    pub limit: u32,
}

/// One aliased `pullRequests` connection per lookup, each head and state passed as a variable.
/// `gh pr list` orders the same way, so a head with more matches than the limit keeps the same
/// rows.
pub fn build_pull_requests_by_head_query(lookups: &[HeadLookup]) -> (String, Value) {
    let mut variables = serde_json::Map::new();
    let mut declarations = vec!["$owner: String!".to_string(), "$name: String!".to_string()];
    let mut selections = Vec::new();
    for (index, lookup) in lookups.iter().enumerate() {
        variables.insert(format!("h{index}"), json!(lookup.head_ref_name));
        variables.insert(format!("s{index}"), json!(lookup.state.graphql()));
        declarations.push(format!("$h{index}: String!"));
        declarations.push(format!("$s{index}: [PullRequestState!]"));
        selections.push(format!(
            "    h{index}: pullRequests(headRefName: $h{index}, states: $s{index}, first: {}, orderBy: {{ field: CREATED_AT, direction: DESC }}) {{ nodes {{ {PULL_REQUEST_NODE_SELECTION} }} }}",
            lookup.limit
        ));
    }
    (
        format!(
            "query PullRequestsByHead({}) {{\n  repository(owner: $owner, name: $name) {{\n{}\n  }}\n}}",
            declarations.join(", "),
            selections.join("\n")
        ),
        Value::Object(variables),
    )
}

const PULL_REQUEST_BY_NUMBER_QUERY: &str = "query PullRequestByNumber($owner: String!, $name: String!, $number: Int!) {\n  repository(owner: $owner, name: $name) {\n    pullRequest(number: $number) { number title url baseRefName headRefName headRefOid state isDraft mergedAt closedAt updatedAt isCrossRepository headRepository { name nameWithOwner } headRepositoryOwner { login } }\n  }\n}";

/// A repository read from the REST API.
struct RawRepository {
    full_name: String,
    html_url: String,
    ssh_url: String,
    default_branch: Option<String>,
}

fn decode_raw_repository(body: &str) -> Result<RawRepository, String> {
    let value: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let field = |name: &str| trimmed_non_empty(value.get(name)).map_err(|()| format!("Invalid {name}"));
    let default_branch = match value.get("default_branch") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err("Invalid default_branch".into()),
    };
    Ok(RawRepository {
        full_name: field("full_name")?,
        html_url: field("html_url")?,
        ssh_url: field("ssh_url")?,
        default_branch,
    })
}

fn clone_urls(raw: &RawRepository) -> SourceControlRepositoryCloneUrls {
    SourceControlRepositoryCloneUrls {
        name_with_owner: raw.full_name.clone(),
        url: raw.html_url.clone(),
        ssh_url: raw.ssh_url.clone(),
    }
}

/// `^([^:/\s]+):(.+)$` over the trimmed selector: the fork owner and the branch.
fn owner_match(head_selector: &str) -> Option<(String, String)> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let trimmed = t3_contracts::trim_wire_string(head_selector);
    let captures = regex(&PATTERN, r"^([^:/\s]+):(.+)$").captures(trimmed)?;
    Some((captures[1].to_string(), captures[2].to_string()))
}

pub struct GitHubCli {
    host: Host,
}

impl GitHubCli {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    async fn git_read(&self, cwd: &str, args: &[&str]) -> Option<(i32, String)> {
        let mut input = self.host.cli("GitHubCli.resolveRepository", "git", args.iter().copied(), cwd);
        input.allow_non_zero_exit = true;
        input.timeout = Some(Duration::from_secs(5));
        let output = self.host.process().run(input).await.ok()?;
        Some((output.exit_code, output.stdout))
    }

    /// The repository `gh` would act on in `cwd`: GH_REPO, else the remote `gh` would pick,
    /// else the remote the caller resolved the provider from, else the best-ranked GitHub
    /// remote.
    pub async fn resolve_repository(
        &self,
        cwd: &str,
        host_hint: Option<&str>,
    ) -> Result<GitHubRepositoryLocator, GitHubCliError> {
        let env = self.host.env();
        let default_host = host_hint
            .map(str::to_string)
            .or_else(|| env.get("GH_HOST").map(str::to_string))
            .unwrap_or_else(|| "github.com".into())
            .to_lowercase();
        if let Some(repository) = env.get_trimmed("GH_REPO")
            && let Some(locator) = parse_repository_selector(repository, &default_host)
        {
            return Ok(locator);
        }
        let remotes = self.git_read(cwd, &["remote", "-v"]).await;
        let resolved = self.git_read(cwd, &["config", "--get-regexp", "^remote\\..*\\.gh-resolved$"]).await;
        let (remotes, resolved) = match (remotes, resolved) {
            (Some(remotes), Some(resolved)) => (Some(remotes), Some(resolved)),
            _ => (None, None),
        };
        let remote_output = remotes.filter(|(code, _)| *code == 0).map(|(_, out)| out).unwrap_or_default();
        let resolved_output = resolved.filter(|(code, _)| *code == 0).map(|(_, out)| out).unwrap_or_default();
        struct FetchRemote {
            name: String,
            url: String,
            host: Option<String>,
        }
        let fetch_remotes: Vec<FetchRemote> = remote_output
            .split('\n')
            .filter_map(|line| fetch_remote_line().captures(line.trim()))
            .map(|captures| FetchRemote {
                name: captures[1].to_string(),
                url: captures[2].to_string(),
                host: github_api_host_for_remote(&captures[2]),
            })
            .collect();
        let host = host_hint
            .map(api_host_for_hint)
            .or_else(|| fetch_remotes.iter().find(|r| r.name == "origin" && r.host.is_some()).and_then(|r| r.host.clone()))
            .or_else(|| fetch_remotes.iter().find_map(|r| r.host.clone()))
            .unwrap_or(default_host);
        if let Some((owner, name)) = select_base_repository(&remote_output, &resolved_output, &host) {
            return Ok(GitHubRepositoryLocator { host, owner, name });
        }
        // gh's own order without its prompt: upstream, github, origin, then the first remote,
        // among the ones on this host (an SSH alias counts as its API host).
        let rank = |name: &str| ["upstream", "github", "origin"].iter().position(|known| *known == name.to_lowercase()).unwrap_or(99);
        let mut candidates: Vec<&FetchRemote> =
            fetch_remotes.iter().filter(|remote| remote.host.as_deref() == Some(host.as_str())).collect();
        candidates.sort_by_key(|remote| rank(&remote.name));
        for remote in candidates {
            let normalized = normalize_git_remote_url(&remote.url);
            let mut parts = normalized.split('/').skip(1);
            if let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next())
                && !owner.is_empty()
                && !name.is_empty()
            {
                return Ok(GitHubRepositoryLocator { host, owner: owner.into(), name: name.into() });
            }
        }
        Err(GitHubCliError::command(
            cwd,
            &format!("No GitHub repository on {host} was found among this checkout's git remotes."),
        ))
    }

    async fn graphql(&self, cwd: &str, input: GraphQlInput) -> Result<Value, GitHubCliError> {
        let raw = api::graphql(&self.host, input).await.map_err(|error| GitHubCliError::from_api(cwd, error))?;
        serde_json::from_str(&raw).map_err(|error| {
            GitHubCliError::decode("GitHubPullRequestDecodeError", cwd, &error.to_string())
        })
    }

    async fn rest(&self, cwd: &str, input: RestInput) -> Result<api::RestResponse, GitHubCliError> {
        api::rest(&self.host, input).await.map_err(|error| GitHubCliError::from_api(cwd, error))
    }

    async fn list_by_head(
        &self,
        cwd: &str,
        head_selector: &str,
        state: PullRequestListState,
        limit: u32,
        rate_limit_host: Option<&str>,
        allow_reserve: bool,
    ) -> Result<Vec<ChangeRequest>, GitHubCliError> {
        let locator = self.resolve_repository(cwd, rate_limit_host).await?;
        let limit = limit.clamp(1, 100);
        // `owner:branch` names a fork's branch. GitHub filters on the branch name only, so the
        // owner is matched on the rows it returns.
        let owner = owner_match(head_selector);
        let head_ref_name = owner
            .as_ref()
            .map(|(_, branch)| branch.clone())
            .unwrap_or_else(|| t3_contracts::trim_wire_string(head_selector).to_string());
        let (document, variables) = build_pull_requests_by_head_query(&[HeadLookup {
            head_ref_name,
            state,
            limit: if owner.is_some() { OWNER_HEAD_SCAN_LIMIT } else { limit },
        }]);
        let mut all_variables = json!({"owner":locator.owner,"name":locator.name});
        all_variables.as_object_mut().unwrap().extend(variables.as_object().unwrap().clone());
        let decoded = self
            .graphql(
                cwd,
                GraphQlInput {
                    host: locator.host.clone(),
                    operation: "listPullRequestsByHead".into(),
                    query: document,
                    variables: Some(all_variables),
                    allow_reserve: Some(allow_reserve),
                    max_response_bytes: Some(HEAD_LOOKUP_MAX_RESPONSE_BYTES),
                },
            )
            .await
            .map_err(|error| remap_decode(error, "GitHubChangeRequestListDecodeError", "GitHub returned an invalid change request list."))?;
        let repository = match decoded.pointer("/data/repository") {
            Some(Value::Object(repository)) => repository,
            Some(Value::Null) => {
                return Err(GitHubCliError::not_found(cwd, ErrorCause::new("Error", "The repository could not be read.")));
            }
            _ => {
                return Err(GitHubCliError::decode(
                    "GitHubChangeRequestListDecodeError",
                    cwd,
                    "GitHub returned an invalid change request list.",
                ));
            }
        };
        let nodes: Vec<Value> = match repository.get("h0") {
            Some(Value::Object(alias)) => match alias.get("nodes") {
                Some(Value::Array(nodes)) => nodes.clone(),
                _ => {
                    return Err(GitHubCliError::decode(
                        "GitHubChangeRequestListDecodeError",
                        cwd,
                        "GitHub returned an invalid change request list.",
                    ));
                }
            },
            None | Some(Value::Null) => Vec::new(),
            Some(_) => {
                return Err(GitHubCliError::decode(
                    "GitHubChangeRequestListDecodeError",
                    cwd,
                    "GitHub returned an invalid change request list.",
                ));
            }
        };
        let rows = decode_pull_requests(&nodes);
        let Some((head_owner, _)) = owner else {
            return Ok(rows);
        };
        let head_owner = head_owner.to_lowercase();
        Ok(rows
            .into_iter()
            .filter(|row| {
                row.head_repository_owner_login
                    .as_ref()
                    .and_then(|login| login.as_deref())
                    .is_some_and(|login| login.to_lowercase() == head_owner)
            })
            .take(limit as usize)
            .collect())
    }

    /// Open pull requests with this head. Reads without reserve are what a background sweep
    /// does; this one is interactive.
    pub async fn list_open_pull_requests(
        &self,
        cwd: &str,
        head_selector: &str,
        limit: Option<u32>,
        rate_limit_host: Option<&str>,
    ) -> Result<Vec<ChangeRequest>, GitHubCliError> {
        self.list_by_head(cwd, head_selector, PullRequestListState::Open, limit.unwrap_or(1), rate_limit_host, true)
            .await
    }

    /// Pull requests whose head is `head_selector` (a branch, or `owner:branch` for a fork),
    /// in the repository `gh pr list` would read in `cwd`.
    pub async fn list_pull_requests_by_head(
        &self,
        cwd: &str,
        head_selector: &str,
        state: PullRequestListState,
        limit: u32,
        rate_limit_host: Option<&str>,
    ) -> Result<Vec<ChangeRequest>, GitHubCliError> {
        let allow_reserve = api::allows_reserve();
        self.list_by_head(cwd, head_selector, state, limit, rate_limit_host, allow_reserve).await
    }

    pub async fn get_pull_request(
        &self,
        cwd: &str,
        reference: &str,
        rate_limit_host: Option<&str>,
    ) -> Result<ChangeRequest, GitHubCliError> {
        let parsed = parse_pull_request_reference(reference);
        let (locator, number) = match parsed {
            PullRequestReference::Branch(head_selector) => {
                // `gh pr view <branch>` prefers an open pull request, then the newest of any state.
                let open = self
                    .list_by_head(cwd, &head_selector, PullRequestListState::Open, 1, rate_limit_host, true)
                    .await?;
                let found = match open.into_iter().next() {
                    Some(open) => Some(open),
                    None => self
                        .list_by_head(cwd, &head_selector, PullRequestListState::All, 1, rate_limit_host, true)
                        .await?
                        .into_iter()
                        .next(),
                };
                return found.ok_or_else(|| {
                    GitHubCliError::not_found(cwd, ErrorCause::new("Error", "No pull request has this head branch."))
                });
            }
            PullRequestReference::Url { locator, number } => (locator, number),
            PullRequestReference::Number(number) => (self.resolve_repository(cwd, rate_limit_host).await?, number),
        };
        let decode_failure = || {
            GitHubCliError::decode("GitHubPullRequestDecodeError", cwd, "GitHub returned an invalid pull request.")
        };
        let decoded = self
            .graphql(
                cwd,
                GraphQlInput {
                    host: locator.host.clone(),
                    operation: "getPullRequest".into(),
                    query: PULL_REQUEST_BY_NUMBER_QUERY.into(),
                    variables: Some(json!({"owner":locator.owner,"name":locator.name,"number":number})),
                    allow_reserve: Some(true),
                    max_response_bytes: None,
                },
            )
            .await?;
        let node = match decoded.pointer("/data/repository") {
            Some(Value::Null) => Value::Null,
            Some(Value::Object(repository)) => match repository.get("pullRequest") {
                Some(node) => node.clone(),
                None => return Err(decode_failure()),
            },
            _ => return Err(decode_failure()),
        };
        if node.is_null() {
            return Err(GitHubCliError::not_found(cwd, ErrorCause::new("Error", "The pull request does not exist.")));
        }
        decode_pull_request(&node).ok_or_else(decode_failure)
    }

    async fn read_repository(&self, cwd: &str, locator: &GitHubRepositoryLocator) -> Result<RawRepository, GitHubCliError> {
        let response = self
            .rest(
                cwd,
                RestInput {
                    host: locator.host.clone(),
                    operation: "getRepository".into(),
                    path: format!(
                        "repos/{}/{}",
                        encode_uri_component(&locator.owner),
                        encode_uri_component(&locator.name)
                    ),
                    allow_reserve: Some(true),
                    ..RestInput::default()
                },
            )
            .await?;
        decode_raw_repository(&response.body)
            .map_err(|_| GitHubCliError::decode("GitHubRepositoryDecodeError", cwd, "GitHub returned an invalid repository."))
    }

    async fn read_viewer_login(&self, cwd: &str, host: &str) -> Result<String, GitHubCliError> {
        let response = self
            .rest(
                cwd,
                RestInput {
                    host: host.into(),
                    operation: "getViewer".into(),
                    path: "user".into(),
                    ..RestInput::default()
                },
            )
            .await?;
        serde_json::from_str::<Value>(&response.body)
            .ok()
            .and_then(|value| trimmed_non_empty(value.get("login")).ok())
            .ok_or_else(|| GitHubCliError::command_without_reason(cwd))
    }

    pub async fn get_repository_clone_urls(
        &self,
        cwd: &str,
        repository: &str,
    ) -> Result<SourceControlRepositoryCloneUrls, GitHubCliError> {
        let fallback_host = match self.resolve_repository(cwd, None).await {
            Ok(locator) => locator.host,
            Err(_) => self.host.env().get("GH_HOST").unwrap_or("github.com").to_string(),
        }
        .to_lowercase();
        let Some(locator) = parse_repository_selector(repository, &fallback_host) else {
            return Err(GitHubCliError::command(cwd, "Repositories are named owner/name."));
        };
        Ok(clone_urls(&self.read_repository(cwd, &locator).await?))
    }

    pub async fn create_repository(
        &self,
        cwd: &str,
        repository: &str,
        visibility: SourceControlRepositoryVisibility,
    ) -> Result<SourceControlRepositoryCloneUrls, GitHubCliError> {
        let default_host = self.host.env().get("GH_HOST").unwrap_or("github.com").to_lowercase();
        let locator = parse_repository_selector(repository, &default_host);
        let viewer = match &locator {
            Some(locator) => Some(self.read_viewer_login(cwd, &locator.host).await?),
            None => None,
        };
        let owner = locator.as_ref().map(|l| l.owner.clone()).or_else(|| viewer.clone());
        let name = locator.as_ref().map(|l| l.name.clone()).unwrap_or_else(|| repository.trim().to_string());
        let host = locator.as_ref().map(|l| l.host.clone()).unwrap_or_else(|| {
            self.host
                .env()
                .get("GH_HOST")
                .map(|value| value.trim().to_lowercase())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "github.com".into())
        });
        let is_viewer = match (&viewer, &owner) {
            (Some(viewer), Some(owner)) => owner.to_lowercase() == viewer.to_lowercase(),
            _ => false,
        };
        let path = match (&owner, is_viewer) {
            (Some(owner), false) => format!("orgs/{}/repos", encode_uri_component(owner)),
            _ => "user/repos".into(),
        };
        let response = self
            .rest(
                cwd,
                RestInput {
                    host,
                    operation: "createRepository".into(),
                    method: Method::Post,
                    path,
                    body: Some(json!({"name":name,"private":visibility == SourceControlRepositoryVisibility::Private})),
                    ..RestInput::default()
                },
            )
            .await?;
        decode_raw_repository(&response.body)
            .map(|raw| clone_urls(&raw))
            .map_err(|_| GitHubCliError::decode("GitHubRepositoryDecodeError", cwd, "GitHub returned an invalid repository."))
    }

    pub async fn create_pull_request(
        &self,
        cwd: &str,
        base_branch: &str,
        head_selector: &str,
        title: &str,
        body_file: &str,
    ) -> Result<(), GitHubCliError> {
        let locator = self.resolve_repository(cwd, None).await?;
        let body = tokio::fs::read_to_string(body_file)
            .await
            .map_err(|error| GitHubCliError::command(cwd, &error.to_string()))?;
        // `owner:branch` is how the REST API takes a fork's head, the same as `gh --head`. gh
        // allows maintainer edits unless told otherwise; the API's default is not documented.
        self.rest(
            cwd,
            RestInput {
                host: locator.host.clone(),
                operation: "createPullRequest".into(),
                method: Method::Post,
                path: format!(
                    "repos/{}/{}/pulls",
                    encode_uri_component(&locator.owner),
                    encode_uri_component(&locator.name)
                ),
                body: Some(json!({"base":base_branch,"head":head_selector,"title":title,"body":body,"maintainer_can_modify":true})),
                ..RestInput::default()
            },
        )
        .await?;
        Ok(())
    }

    pub async fn get_default_branch(
        &self,
        cwd: &str,
        rate_limit_host: Option<&str>,
    ) -> Result<Option<String>, GitHubCliError> {
        let locator = self.resolve_repository(cwd, rate_limit_host).await?;
        let repository = self.read_repository(cwd, &locator).await?;
        let branch = repository.default_branch.as_deref().map(str::trim).unwrap_or("");
        Ok((!branch.is_empty()).then(|| branch.to_string()))
    }

    /// `gh pr checkout` in plain git: the head branch is fetched from the remote that holds it
    /// (a fork gets a remote of its own), checked out under the name gh would give it, and set
    /// to track the head. A head branch that is gone is read from the base's
    /// `refs/pull/<n>/head`. An existing branch fast-forwards, or with `force` is reset to the
    /// pull request.
    pub async fn checkout_pull_request(&self, cwd: &str, reference: &str, force: bool) -> Result<(), GitHubCliError> {
        let git_failure = |error: GitCommandError| GitHubCliError::git(cwd, &error);
        let core = self.host.shared.git.core();
        let path = Path::new(cwd);
        let parsed = parse_pull_request_reference(reference);
        let pull_request = self.get_pull_request(cwd, reference, None).await?;
        let base = match &parsed {
            PullRequestReference::Url { locator, .. } => locator.clone(),
            _ => self.resolve_repository(cwd, None).await?,
        };
        let base_name_with_owner = format!("{}/{}", base.owner, base.name).to_lowercase();
        let head_name_with_owner = pull_request.head_repository_name_with_owner.clone().flatten();
        let is_cross_repository = pull_request.is_cross_repository.unwrap_or_else(|| {
            head_name_with_owner
                .as_ref()
                .is_some_and(|head| head.to_lowercase() != base_name_with_owner)
        });
        let head_owner = pull_request
            .head_repository_owner_login
            .clone()
            .flatten()
            .or_else(|| head_name_with_owner.as_ref().and_then(|head| head.split('/').next().map(str::to_string)));

        let remotes = self
            .git_read(cwd, &["remote", "-v"])
            .await
            .filter(|(code, _)| *code == 0)
            .map(|(_, out)| out)
            .unwrap_or_default();
        let remote_for = |name_with_owner: &str| -> Option<String> {
            remotes
                .split('\n')
                .filter_map(|line| fetch_remote_line().captures(line.trim()))
                .find(|captures| {
                    let normalized = normalize_git_remote_url(&captures[2]);
                    normalized.split('/').skip(1).collect::<Vec<_>>().join("/") == name_with_owner.to_lowercase()
                })
                .map(|captures| captures[1].to_string())
        };
        let base_remote = match remote_for(&base_name_with_owner) {
            Some(known) => known,
            None => core.resolve_primary_remote_name(path).await.map_err(git_failure)?,
        };
        // A fork's branch named like the base's default branch is checked out under the
        // owner's prefix. Without the default branch that collision cannot be ruled out, and
        // the checkout would reset the local default branch to the fork's commit.
        let default_branch = if is_cross_repository {
            self.read_repository(cwd, &base).await?.default_branch
        } else {
            None
        };
        let local_branch = checkout_branch_name(
            &pull_request.head_ref_name,
            head_owner.as_deref(),
            is_cross_repository,
            default_branch.as_deref(),
        );

        // The remote the head branch lives on; a fork the checkout does not know yet is added.
        let head_remote: Result<String, GitHubCliError> = async {
            if !is_cross_repository {
                return Ok(base_remote.clone());
            }
            let Some(head_name_with_owner) = head_name_with_owner.as_deref() else {
                return Err(GitHubCliError::command(cwd, "The fork is gone."));
            };
            if let Some(known) = remote_for(head_name_with_owner) {
                return Ok(known);
            }
            let (owner, name) = head_name_with_owner.split_once('/').unwrap_or((head_name_with_owner, ""));
            let fork = self
                .read_repository(cwd, &GitHubRepositoryLocator { host: base.host.clone(), owner: owner.into(), name: name.into() })
                .await?;
            let origin_url = core.read_config_value(path, "remote.origin.url").await.map_err(git_failure)?;
            let url = if origin_url.as_deref().is_some_and(is_ssh_remote_url) { fork.ssh_url } else { fork.html_url };
            core.ensure_remote(path, head_owner.as_deref().unwrap_or("fork"), &url).await.map_err(git_failure)
        }
        .await;

        let exists = core.list_local_branch_names(path).await.map_err(git_failure)?.contains(&local_branch);

        // The commit to check out, fetched from the head branch where it still exists.
        let fetched = async {
            let remote_name = head_remote?;
            core.fetch_remote_tracking_branch(path, &remote_name, &pull_request.head_ref_name).await.map_err(git_failure)?;
            Ok::<_, GitHubCliError>((
                format!("refs/remotes/{remote_name}/{}", pull_request.head_ref_name),
                Some((remote_name, pull_request.head_ref_name.clone())),
            ))
        }
        .await;
        let (target_ref, upstream) = match fetched {
            Ok(target) => target,
            Err(_) => {
                self.run_git(cwd, &["fetch", "--quiet", "--no-tags", &base_remote, &format!("refs/pull/{}/head", pull_request.number)])
                    .await?;
                let commit = core.resolve_commit(path, "FETCH_HEAD").await.map_err(git_failure)?;
                (commit, None)
            }
        };

        if !exists {
            self.run_git(cwd, &["branch", &local_branch, &target_ref]).await?;
        }
        core.switch_ref(path, &local_branch).await.map_err(git_failure)?;
        if exists {
            if force {
                self.run_git(cwd, &["reset", "--hard", "--quiet", &target_ref]).await?;
            } else {
                self.run_git(cwd, &["merge", "--ff-only", "--quiet", &target_ref]).await?;
            }
        }
        // Tracking is set once the branch is the pull request's, so a sync that fails leaves an
        // existing branch's upstream as it was.
        if let Some((remote_name, remote_branch)) = upstream {
            core.set_branch_upstream(path, &local_branch, &remote_name, &remote_branch)
                .await
                .map_err(git_failure)?;
        }
        Ok(())
    }

    async fn run_git(&self, cwd: &str, args: &[&str]) -> Result<(), GitHubCliError> {
        self.host
            .shared
            .git
            .core()
            .execute(ExecuteGitInput {
                operation: "GitHubCli.checkoutPullRequest".into(),
                cwd: cwd.into(),
                args: args.iter().map(|arg| arg.to_string()).collect(),
                ..ExecuteGitInput::default()
            })
            .await
            .map(|_| ())
            .map_err(|error| GitHubCliError::git(cwd, &error))
    }
}

/// A GraphQL answer that is not the JSON shape a read expects fails as that read's decode
/// error; every other failure passes through.
fn remap_decode(error: GitHubCliError, tag: &'static str, message: &str) -> GitHubCliError {
    if error.tag == "GitHubPullRequestDecodeError" {
        GitHubCliError::decode(tag, &error.cwd, message)
    } else {
        error
    }
}
