//! GitLab through the `glab` CLI (port of `GitLabCli.ts`, `gitLabMergeRequests.ts`,
//! `gitLabAuthStatus.ts` and `GitLabSourceControlProvider.ts`). Merge requests are read and
//! created with `glab mr` and `glab api`; the repository comes from the checkout (`:fullpath`),
//! so every operation runs in the working directory. Discovery reads `glab auth status`, which
//! also recognises a self-hosted GitLab on an ordinary-looking host name.
use crate::{
    source_control_decode::*,
    source_control_detect::{RefSelector, source_ref_from_input, transport_safe_value},
    source_control_discovery::{
        AuthProbeInput, CliSpec, combined_auth_output, first_safe_auth_line, match_first, parse_cli_host,
        provider_auth,
    },
    source_control_host::Host,
    source_control_provider::*,
    source_control_uri::encode_uri_component,
};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::Value;
use std::{sync::{Arc, OnceLock}, time::Duration};
use t3_contracts::{
    ChangeRequest, ChangeRequestState, ErrorCause, SourceControlProviderAuthStatus, SourceControlProviderInfo,
    SourceControlProviderKind, SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility,
};
use t3_vcs::{VcsError, process::VcsProcessInput};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Gitlab;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

// ---- auth status ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabAuthStatusHost {
    pub host: String,
    pub account: Option<String>,
}

fn host_line() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)^(?:[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?|\[[a-f0-9:.]+\])(?::\d+)?$").unwrap())
}

fn logged_in() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)Logged in to .+? as\s+([^\s(]+)").unwrap())
}

/// The hosts `glab auth status` lists: an unindented line naming a host, then the lines
/// below it, one of which says who is signed in.
pub fn parse_auth_status_hosts(text: &str) -> Vec<GitLabAuthStatusHost> {
    let mut hosts = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    let mut flush = |current: &mut Option<(String, Vec<String>)>| {
        if let Some((host, lines)) = current.take() {
            let account = logged_in()
                .captures(&lines.join("\n"))
                .and_then(|captures| captures.get(1))
                .map(|account| account.as_str().trim().to_string())
                .filter(|account| !account.is_empty());
            hosts.push(GitLabAuthStatusHost { host, account });
        }
    };
    for raw in text.split('\n') {
        let raw = raw.trim_end_matches('\r');
        let line = t3_contracts::trim_wire_string(raw);
        if line.is_empty() {
            continue;
        }
        let unindented = raw.chars().count() == raw.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}').chars().count();
        if unindented && host_line().is_match(line) {
            flush(&mut current);
            current = Some((line.to_lowercase(), Vec::new()));
            continue;
        }
        if let Some((_, lines)) = current.as_mut() {
            lines.push(line.to_string());
        }
    }
    flush(&mut current);
    hosts
}

fn parse_auth(input: &AuthProbeInput) -> t3_contracts::SourceControlProviderAuth {
    use SourceControlProviderAuthStatus::*;
    static AS: OnceLock<[Regex; 3]> = OnceLock::new();
    let patterns = AS.get_or_init(|| {
        [
            Regex::new(r"(?iu)Logged in to .* as\s+([^\s(]+)").unwrap(),
            Regex::new(r"(?iu)Logged in to .* account\s+([^\s(]+)").unwrap(),
            Regex::new(r"(?iu)account:\s*([^\s(]+)").unwrap(),
        ]
    });
    let output = combined_auth_output(input);
    let hosts = parse_auth_status_hosts(&output);
    let authenticated = hosts.iter().find(|host| host.account.is_some());
    let account = authenticated
        .and_then(|host| host.account.clone())
        .or_else(|| match_first(&output, &patterns.iter().collect::<Vec<_>>()));
    let host = authenticated.map(|host| host.host.clone()).or_else(|| parse_cli_host(&output));
    if let Some(account) = &account {
        return provider_auth(Authenticated, Some(account), host.as_deref(), None);
    }
    if input.exit_code != 0 {
        let detail = first_safe_auth_line(&output).unwrap_or_else(|| "Run `glab auth login` to authenticate GitLab CLI.".into());
        return provider_auth(Unauthenticated, None, host.as_deref(), Some(&detail));
    }
    let detail = first_safe_auth_line(&output).unwrap_or_else(|| "GitLab CLI auth status could not be parsed.".into());
    provider_auth(Unknown, None, host.as_deref(), Some(&detail))
}

fn refine_unknown_remote(context: &ProviderContext, auth: &AuthProbeInput) -> Option<SourceControlProviderInfo> {
    let host = context.provider.name.to_lowercase();
    let authenticated = parse_auth_status_hosts(&combined_auth_output(auth))
        .iter()
        .any(|entry| entry.account.is_some() && entry.host == host);
    authenticated.then(|| SourceControlProviderInfo {
        kind: KIND,
        name: "GitLab Self-Hosted".into(),
        base_url: context.provider.base_url.clone(),
    })
}

pub fn cli_spec() -> CliSpec {
    CliSpec {
        kind: KIND,
        label: "GitLab",
        install_hint: "Install the GitLab command-line tool (`glab`) from https://gitlab.com/gitlab-org/cli or your package manager (for example `brew install glab`).",
        executable: "glab",
        version_args: vec!["--version"],
        auth_args: vec!["auth", "status"],
        remote_refinement_args: None,
        probe_timeout: None,
        parse_auth: Arc::new(parse_auth),
        refine_unknown_remote: Some(Arc::new(refine_unknown_remote)),
    }
}

// ---- merge requests ---------------------------------------------------------------------------

fn project_path(project: Option<&Value>) -> Result<Option<String>, ()> {
    let Some(project) = project.filter(|project| !project.is_null()) else {
        return Ok(None);
    };
    let object = project.as_object().ok_or(())?;
    let explicit = trim_optional(optional_string(object.get("path_with_namespace"))?.as_deref())
        .or_else(|| trim_optional(optional_string(object.get("pathWithNamespace")).ok()?.as_deref()));
    let namespace = match object.get("namespace") {
        None | Some(Value::Null) => None,
        Some(Value::Object(namespace)) => {
            let get = |name: &str| optional_string(namespace.get(name));
            Some((get("path")?, get("full_path")?, get("fullPath")?))
        }
        Some(_) => return Err(()),
    };
    if explicit.is_some() {
        return Ok(explicit);
    }
    Ok(namespace.and_then(|(path, full_path, camel)| {
        trim_optional(full_path.as_deref())
            .or_else(|| trim_optional(camel.as_deref()))
            .or_else(|| trim_optional(path.as_deref()))
    }))
}

fn nullable_number(value: Option<&Value>) -> Field<Option<f64>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => Ok(number.as_f64()),
        Some(_) => Err(()),
    }
}

fn decode_merge_request_row(entry: &Value) -> Field<ChangeRequest> {
    let object = entry.as_object().ok_or(())?;
    let number = positive_int(object.get("iid"))?;
    let title = trimmed_non_empty(object.get("title"))?;
    let url = trimmed_non_empty(object.get("web_url"))?;
    let head_ref_name = trimmed_non_empty(object.get("source_branch"))?;
    let base_ref_name = trimmed_non_empty(object.get("target_branch"))?;
    let state = optional_nullable_string(object.get("state"))?;
    let draft = optional_bool(object.get("draft"))?;
    let work_in_progress = optional_bool(object.get("work_in_progress"))?;
    let closed_at = optional_nullable_string(object.get("closed_at"))?;
    let merged_at = optional_nullable_string(object.get("merged_at"))?;
    let updated_at = optional_date(object.get("updated_at"))?;
    let source_id = nullable_number(object.get("source_project_id"))?;
    let target_id = nullable_number(object.get("target_project_id"))?;
    let source_path = project_path(object.get("source_project"))?;
    let target_path = project_path(object.get("target_project"))?;
    let is_cross_repository = match (source_id, target_id) {
        (Some(source), Some(target)) => Some(source != target),
        _ => match (&source_path, &target_path) {
            (Some(source), Some(target)) => Some(source.to_lowercase() != target.to_lowercase()),
            _ => None,
        },
    };
    let owner = source_path
        .as_ref()
        .and_then(|path| path.split('/').next())
        .and_then(|owner| trim_optional(Some(owner)));
    let state = match state.as_deref().map(|state| state.trim().to_lowercase()).as_deref() {
        Some("merged") => ChangeRequestState::Merged,
        Some("closed") => ChangeRequestState::Closed,
        _ => ChangeRequestState::Open,
    };
    Ok(ChangeRequest {
        provider: KIND,
        number,
        title,
        url,
        base_ref_name,
        head_ref_name,
        head_sha: None,
        state,
        is_draft: (draft == Some(true) || work_in_progress == Some(true)).then_some(true),
        closed_at: Some(closed_at),
        merged_at: Some(merged_at),
        updated_at,
        is_cross_repository,
        head_repository_name_with_owner: source_path.map(Some),
        head_repository_owner_login: owner.map(Some),
    })
}

/// A `glab mr list --output json` array; a row that does not decode is skipped.
pub fn decode_merge_request_list_json(raw: &str) -> Result<Vec<ChangeRequest>, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(entries)) => Ok(entries.iter().filter_map(|entry| decode_merge_request_row(entry).ok()).collect()),
        Ok(_) => Err("Expected an array".into()),
        Err(error) => Err(error.to_string()),
    }
}

pub fn decode_merge_request_json(raw: &str) -> Result<ChangeRequest, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    decode_merge_request_row(&value).map_err(|()| "Invalid merge request".to_string())
}

// ---- the CLI ----------------------------------------------------------------------------------

/// A failure of `glab`, as the original's `GitLabCli*Error` classes carry it: `detail` is the
/// provider-level detail and `message` the error's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabCliError {
    pub tag: &'static str,
    pub operation: &'static str,
    pub detail: String,
    pub cause: Option<ErrorCause>,
}

impl GitLabCliError {
    fn new(tag: &'static str, operation: &'static str, detail: impl Into<String>) -> Self {
        Self { tag, operation, detail: detail.into(), cause: None }
    }

    fn caused_by(mut self, cause: ErrorCause) -> Self {
        self.cause = Some(cause);
        self
    }

    pub fn message(&self) -> String {
        format!("GitLab CLI failed in {}: {}", self.operation, self.detail)
    }

    fn from_vcs(error: &VcsError, reference: Option<&str>) -> Self {
        use t3_vcs::error::ExitFailureKind::*;
        let cause = ErrorCause::new(error.tag(), error.to_string());
        let (tag, detail) = match error {
            VcsError::ProcessSpawn { .. } => (
                "GitLabCliUnavailableError",
                "GitLab CLI (`glab`) is required but not available on PATH.".to_string(),
            ),
            VcsError::ProcessExit { failure_kind: Some(Authentication), .. } => (
                "GitLabCliAuthenticationError",
                "GitLab CLI is not authenticated. Run `glab auth login` and retry.".to_string(),
            ),
            VcsError::ProcessExit { failure_kind: Some(RateLimited), .. } => {
                ("GitLabCliRateLimitError", "GitLab API rate limit exceeded.".to_string())
            }
            VcsError::ProcessExit { failure_kind: Some(NotFound), .. } if reference.is_some() => (
                "GitLabMergeRequestNotFoundError",
                format!(
                    "Merge request {} was not found. Check the MR number or URL and try again.",
                    reference.unwrap_or_default()
                ),
            ),
            _ => ("GitLabCliCommandError", "GitLab CLI command failed.".to_string()),
        };
        Self::new(tag, "execute", detail).caused_by(cause)
    }

    fn decode(tag: &'static str, operation: &'static str, detail: &str) -> Self {
        Self::new(tag, operation, detail)
    }
}

pub struct GitLabCli {
    host: Host,
}

impl GitLabCli {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    fn input(&self, cwd: &str, args: Vec<String>) -> VcsProcessInput {
        let mut input = self.host.cli("GitLabCli.execute", "glab", args, cwd);
        input.timeout = Some(DEFAULT_TIMEOUT);
        input
    }

    async fn run(&self, input: VcsProcessInput, reference: Option<&str>) -> Result<String, GitLabCliError> {
        self.host
            .process()
            .run(input)
            .await
            .map(|output| output.stdout)
            .map_err(|error| GitLabCliError::from_vcs(&error, reference))
    }

    /// One `glab` invocation: its stdout, or the classified failure.
    pub async fn execute(
        &self,
        cwd: &str,
        args: Vec<String>,
        timeout: Option<Duration>,
        max_output_bytes: Option<usize>,
    ) -> Result<String, GitLabCliError> {
        let mut input = self.input(cwd, args);
        if let Some(timeout) = timeout {
            input.timeout = Some(timeout);
        }
        input.max_output_bytes = max_output_bytes;
        self.run(input, None).await
    }

    /// One `glab` invocation with everything the process reported: the pull request calls read
    /// whether a body was cut short or not text, and pipe payloads in through stdin so they never
    /// appear in argv (`GitLabCli.execute` with `stdin`).
    pub async fn execute_output(
        &self,
        cwd: &str,
        args: Vec<String>,
        stdin: Option<String>,
        timeout: Option<Duration>,
        max_output_bytes: Option<usize>,
    ) -> Result<t3_vcs::process::VcsProcessOutput, GitLabCliError> {
        let mut input = self.input(cwd, args);
        input.stdin = stdin;
        if let Some(timeout) = timeout {
            input.timeout = Some(timeout);
        }
        input.max_output_bytes = max_output_bytes;
        self.host.process().run(input).await.map_err(|error| GitLabCliError::from_vcs(&error, None))
    }

    pub async fn list_merge_requests(
        &self,
        cwd: &str,
        head_selector: &str,
        source: Option<&RefSelector>,
        state: ListState,
        limit: Option<u32>,
    ) -> Result<Vec<ChangeRequest>, GitLabCliError> {
        let mut args: Vec<String> = ["mr", "list", "--source-branch"].map(String::from).to_vec();
        args.push(source_ref_name(head_selector, source));
        args.extend(
            match state {
                ListState::Open => vec![],
                ListState::Closed => vec!["--closed"],
                ListState::Merged => vec!["--merged"],
                ListState::All => vec!["--all"],
            }
            .into_iter()
            .map(String::from),
        );
        args.extend(["--per-page".to_string(), limit.unwrap_or(20).to_string(), "--output".into(), "json".into()]);
        let stdout = self.run(self.input(cwd, args), None).await?;
        let raw = stdout.trim();
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        decode_merge_request_list_json(raw).map_err(|_| {
            GitLabCliError::decode("GitLabMergeRequestListDecodeError", "listMergeRequests", "GitLab CLI returned invalid MR list JSON.")
        })
    }

    pub async fn get_merge_request(&self, cwd: &str, reference: &str) -> Result<ChangeRequest, GitLabCliError> {
        let args = ["mr", "view", reference, "--output", "json"].map(String::from).to_vec();
        let stdout = self.run(self.input(cwd, args), Some(reference)).await?;
        decode_merge_request_json(stdout.trim()).map_err(|_| {
            GitLabCliError::decode("GitLabMergeRequestDecodeError", "getMergeRequest", "GitLab CLI returned invalid merge request JSON.")
        })
    }

    fn decode_repository(raw: &str, operation: &'static str) -> Result<SourceControlRepositoryCloneUrls, GitLabCliError> {
        let failure = || GitLabCliError::decode("GitLabRepositoryDecodeError", operation, "GitLab CLI returned invalid repository JSON.");
        let value: Value = serde_json::from_str(raw.trim()).map_err(|_| failure())?;
        let field = |name: &str| trimmed_non_empty(value.get(name)).map_err(|()| failure());
        Ok(SourceControlRepositoryCloneUrls {
            name_with_owner: field("path_with_namespace")?,
            url: field("web_url")?,
            ssh_url: {
                field("http_url_to_repo")?;
                field("ssh_url_to_repo")?
            },
        })
    }

    pub async fn get_repository_clone_urls(&self, cwd: &str, repository: &str) -> Result<SourceControlRepositoryCloneUrls, GitLabCliError> {
        let args = vec!["api".to_string(), format!("projects/{}", encode_uri_component(repository))];
        let stdout = self.run(self.input(cwd, args), None).await?;
        Self::decode_repository(&stdout, "getRepositoryCloneUrls")
    }

    pub async fn create_repository(
        &self,
        cwd: &str,
        repository: &str,
        visibility: SourceControlRepositoryVisibility,
    ) -> Result<SourceControlRepositoryCloneUrls, GitLabCliError> {
        let (namespace_path, project) = parse_repository_path(repository);
        let namespace_id = match namespace_path {
            Some(path) => {
                let args = vec!["api".to_string(), format!("namespaces/{}", encode_uri_component(&path))];
                let stdout = self.run(self.input(cwd, args), None).await?;
                let id = serde_json::from_str::<Value>(stdout.trim())
                    .ok()
                    .and_then(|value| value.get("id")?.as_f64())
                    .ok_or_else(|| {
                        GitLabCliError::decode("GitLabNamespaceDecodeError", "createRepository", "GitLab CLI returned invalid namespace JSON.")
                    })?;
                Some(id)
            }
            None => None,
        };
        let mut args: Vec<String> = ["api", "--method", "POST", "projects", "--raw-field"].map(String::from).to_vec();
        args.push(format!("path={project}"));
        args.extend(["--raw-field".into(), format!("name={project}"), "--raw-field".into(), format!("visibility={}", match visibility {
            SourceControlRepositoryVisibility::Private => "private",
            SourceControlRepositoryVisibility::Public => "public",
        })]);
        if let Some(id) = namespace_id {
            args.extend(["--raw-field".into(), format!("namespace_id={}", format_number(id))]);
        }
        let stdout = self.run(self.input(cwd, args), None).await?;
        Self::decode_repository(&stdout, "createRepository")
    }

    pub async fn create_merge_request(
        &self,
        cwd: &str,
        base_branch: &str,
        head_selector: &str,
        source: Option<&RefSelector>,
        target: Option<&RefSelector>,
        title: &str,
        body_file: &str,
    ) -> Result<(), GitLabCliError> {
        let mut args: Vec<String> = ["api", "--method", "POST", "projects/:fullpath/merge_requests", "--raw-field"].map(String::from).to_vec();
        args.push(format!("source_branch={}", source_ref_name(head_selector, source)));
        args.extend(["--raw-field".into(), format!("target_branch={}", target.map_or(base_branch, |target| target.ref_name.as_str()))]);
        if let Some(project) = source.and_then(|source| source.repository.as_deref().or(source.owner.as_deref())) {
            args.extend(["--raw-field".into(), format!("source_project_id={project}")]);
        }
        args.extend(["--raw-field".into(), format!("title={title}"), "--field".into(), format!("description=@{body_file}")]);
        self.run(self.input(cwd, args), None).await.map(|_| ())
    }

    pub async fn get_default_branch(&self, cwd: &str) -> Result<Option<String>, GitLabCliError> {
        let args = ["api", "projects/:fullpath"].map(String::from).to_vec();
        let stdout = self.run(self.input(cwd, args), None).await?;
        let failure = || GitLabCliError::decode("GitLabRepositoryDecodeError", "getDefaultBranch", "GitLab CLI returned invalid repository JSON.");
        let value: Value = serde_json::from_str(stdout.trim()).map_err(|_| failure())?;
        optional_trimmed_non_empty(value.get("default_branch")).map_err(|()| failure())
    }

    pub async fn checkout_merge_request(&self, cwd: &str, reference: &str) -> Result<(), GitLabCliError> {
        let args = ["mr", "checkout", reference].map(String::from).to_vec();
        self.run(self.input(cwd, args), Some(reference)).await.map(|_| ())
    }
}

/// A JSON number the way JavaScript prints it.
fn format_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 { format!("{}", number as i64) } else { number.to_string() }
}

fn normalize_head_selector(head_selector: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let trimmed = t3_contracts::trim_wire_string(head_selector);
    let pattern = PATTERN.get_or_init(|| Regex::new(r"^[^:]+:(.+)$").unwrap());
    pattern
        .captures(trimmed)
        .and_then(|captures| trim_optional(Some(&captures[1])))
        .unwrap_or_else(|| trimmed.to_string())
}

fn source_ref_name(head_selector: &str, source: Option<&RefSelector>) -> String {
    source
        .map(|source| source.ref_name.clone())
        .unwrap_or_else(|| normalize_head_selector(head_selector))
}

fn parse_repository_path(repository: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = repository
        .split('/')
        .map(t3_contracts::trim_wire_string)
        .filter(|part| !part.is_empty())
        .collect();
    let project = parts.last().map_or_else(|| t3_contracts::trim_wire_string(repository).to_string(), |last| last.to_string());
    let namespace = (parts.len() > 1).then(|| parts[..parts.len() - 1].join("/"));
    (namespace, project)
}

// ---- the provider -----------------------------------------------------------------------------

fn cli_failure(operation: &str, cwd: &str, error: &GitLabCliError) -> t3_contracts::SourceControlProviderError {
    t3_contracts::SourceControlProviderError::new(KIND, operation, cwd, error.detail.clone())
        .with_command(Some("glab"))
        .with_cause_chain(Some(ErrorCause::new(error.tag, error.message()).with_nested(error.cause.clone())))
}

pub struct GitLabProvider {
    host: Host,
    cli: GitLabCli,
}

impl GitLabProvider {
    pub fn new(host: Host) -> Self {
        Self { cli: GitLabCli::new(host.clone()), host }
    }
}

impl SourceControlProvider for GitLabProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn resolve_link(&self, cwd: &str, url: &url::Url) -> Option<BoxFuture<'static, ProviderResult<LinkSubject>>> {
        static PATH: OnceLock<Regex> = OnceLock::new();
        // Automatic enrichment must not send ambient CLI credentials to a host from message text.
        if crate::source_control_detect::js_host(url) != "gitlab.com" {
            return None;
        }
        let path = PATH.get_or_init(|| Regex::new(r"^/(.+)/-/(merge_requests|issues)/([1-9]\d*)(?:/.*)?$").unwrap());
        let captures = path.captures(url.path())?;
        let endpoint = format!("projects/{}/{}/{}", encode_uri_component(&captures[1]), &captures[2], &captures[3]);
        let (host, cwd, link_host) = (self.host.clone(), cwd.to_string(), crate::source_control_detect::js_host(url));
        Some(Box::pin(async move {
            let cli = GitLabCli::new(host);
            let unreadable = |operation: &str, cause: Option<&GitLabCliError>| {
                let error = t3_contracts::SourceControlProviderError::new(KIND, operation, &cwd, "The linked subject could not be read.");
                match cause {
                    Some(cause) => error.with_cause(cause.tag, cause.message()),
                    None => error,
                }
            };
            let stdout = cli
                .execute(
                    &cwd,
                    ["api", "--hostname", &link_host, &endpoint].map(String::from).to_vec(),
                    Some(Duration::from_secs(3)),
                    Some(32_000),
                )
                .await
                .map_err(|error| unreadable("resolveLink", Some(&error)))?;
            let subject: Value = serde_json::from_str(&stdout).map_err(|_| unreadable("resolveLink.decode", None))?;
            let title = subject.get("title").and_then(Value::as_str).ok_or_else(|| unreadable("resolveLink.decode", None))?;
            let body = match subject.get("description") {
                Some(Value::Null) => None,
                Some(Value::String(body)) => Some(body.clone()),
                _ => return Err(unreadable("resolveLink.decode", None)),
            };
            Ok(LinkSubject { title: title.to_string(), body })
        }))
    }

    fn list_change_requests(&self, input: ListChangeRequestsInput) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>> {
        Box::pin(async move {
            let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
            self.cli
                .list_merge_requests(&input.cwd, &input.head_selector, source.as_ref(), input.state, input.limit)
                .await
                .map_err(|error| {
                    cli_failure("listChangeRequests", &input.cwd, &error).with_reference(&transport_safe_value(&input.head_selector))
                })
        })
    }

    fn get_change_request(&self, input: GetChangeRequestInput) -> BoxFuture<'_, ProviderResult<ChangeRequest>> {
        Box::pin(async move {
            self.cli.get_merge_request(&input.cwd, &input.reference).await.map_err(|error| {
                cli_failure("getChangeRequest", &input.cwd, &error).with_reference(&transport_safe_value(&input.reference))
            })
        })
    }

    fn create_change_request(&self, input: CreateChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
            self.cli
                .create_merge_request(
                    &input.cwd,
                    &input.base_ref_name,
                    &input.head_selector,
                    source.as_ref(),
                    input.target.as_ref(),
                    &input.title,
                    &input.body_file,
                )
                .await
                .map_err(|error| {
                    cli_failure("createChangeRequest", &input.cwd, &error).with_reference(&transport_safe_value(&input.head_selector))
                })
        })
    }

    fn get_repository_clone_urls(&self, input: RepositoryUrlsInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.cli.get_repository_clone_urls(&input.cwd, &input.repository).await.map_err(|error| {
                cli_failure("getRepositoryCloneUrls", &input.cwd, &error).with_repository(&transport_safe_value(&input.repository))
            })
        })
    }

    fn create_repository(&self, input: CreateRepositoryInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.cli.create_repository(&input.cwd, &input.repository, input.visibility).await.map_err(|error| {
                cli_failure("createRepository", &input.cwd, &error).with_repository(&transport_safe_value(&input.repository))
            })
        })
    }

    fn get_default_branch(&self, input: DefaultBranchInput) -> BoxFuture<'_, ProviderResult<Option<String>>> {
        Box::pin(async move {
            self.cli
                .get_default_branch(&input.cwd)
                .await
                .map_err(|error| cli_failure("getDefaultBranch", &input.cwd, &error))
        })
    }

    fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            self.cli.checkout_merge_request(&input.cwd, &input.reference).await.map_err(|error| {
                cli_failure("checkoutChangeRequest", &input.cwd, &error).with_reference(&transport_safe_value(&input.reference))
            })
        })
    }
}
