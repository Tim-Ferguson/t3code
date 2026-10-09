//! Azure DevOps through the `az` CLI with its DevOps extension (port of `AzureDevOpsCli.ts`,
//! `azureDevOpsPullRequests.ts` and `AzureDevOpsSourceControlProvider.ts`). Every `az repos`
//! call passes `--detect true`, so the organisation, project and repository come from the
//! checkout the command runs in. Discovery asks `az account show` who is signed in.
use crate::{
    source_control_decode::*,
    source_control_detect::{RefSelector, source_branch, source_ref_from_input, transport_safe_value},
    source_control_discovery::{
        AuthProbeInput, CliSpec, combined_auth_output, first_safe_auth_line, provider_auth,
    },
    source_control_host::Host,
    source_control_provider::*,
};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::Value;
use std::{path::Path, sync::{Arc, OnceLock}, time::Duration};
use t3_contracts::{
    ChangeRequest, ChangeRequestState, ErrorCause, SourceControlProviderAuth, SourceControlProviderAuthStatus,
    SourceControlProviderError, SourceControlProviderKind, SourceControlRepositoryCloneUrls,
    SourceControlRepositoryVisibility,
};
use t3_vcs::{VcsError, process::VcsProcessInput};

const KIND: SourceControlProviderKind = SourceControlProviderKind::AzureDevops;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

// ---- auth -------------------------------------------------------------------------------------

fn parse_auth(input: &AuthProbeInput) -> SourceControlProviderAuth {
    use SourceControlProviderAuthStatus::*;
    let account = input.stdout.trim().split('\n').next().map(|line| line.trim_end_matches('\r')).map(t3_contracts::trim_wire_string);
    if input.exit_code != 0 {
        let detail = first_safe_auth_line(&combined_auth_output(input)).unwrap_or_else(|| "Run `az login` to authenticate Azure CLI.".into());
        return provider_auth(Unauthenticated, None, None, Some(&detail));
    }
    match account.filter(|account| !account.is_empty()) {
        Some(account) => provider_auth(Authenticated, Some(account), Some("dev.azure.com"), None),
        None => provider_auth(Unknown, None, Some("dev.azure.com"), Some("Azure CLI account status could not be parsed.")),
    }
}

pub fn cli_spec() -> CliSpec {
    CliSpec {
        kind: KIND,
        label: "Azure DevOps",
        install_hint: "Install the Azure command-line tools (`az`), then enable Azure DevOps support with `az extension add --name azure-devops`.",
        executable: "az",
        version_args: vec!["--version"],
        auth_args: vec!["account", "show", "--query", "user.name", "-o", "tsv"],
        remote_refinement_args: None,
        // `az` boots a fresh Python interpreter on every invocation, so even `az --version` can
        // take seconds and overrun the default budget on a slow machine.
        probe_timeout: Some(Duration::from_secs(20)),
        parse_auth: Arc::new(parse_auth),
        refine_unknown_remote: None,
    }
}

// ---- pull requests ----------------------------------------------------------------------------

fn encode_segment(segment: &str) -> String {
    crate::source_control_uri::encode_uri_component(segment)
}

/// The organization root a REST url belongs to, which is where a browser url has to be hung when
/// Azure answered with neither a web link nor a repository url.
fn organization_base_from_rest_url(value: Option<&str>) -> Option<String> {
    let raw = trim_optional(value)?;
    let url = url::Url::parse(&raw).ok()?;
    let hostname = url.host_str()?.to_lowercase();
    let segments: Vec<&str> = url.path().split('/').filter(|segment| !segment.is_empty()).collect();
    if !segments.iter().any(|segment| segment.to_lowercase() == "_apis") {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    if hostname == "dev.azure.com" {
        return segments.first().map(|organization| format!("{origin}/{organization}"));
    }
    hostname.ends_with(".visualstudio.com").then_some(origin)
}

/// Where a pull request lives in a browser. Azure answers with a web link when asked for one
/// and otherwise leaves it to be assembled, so all three routes are tried in the order they can
/// be trusted.
pub fn pull_request_web_url(
    pull_request_id: u64,
    web_link: Option<&str>,
    repository_web_url: Option<&str>,
    rest_api_url: Option<&str>,
    project_name: Option<&str>,
    repository_name: Option<&str>,
) -> String {
    if let Some(link) = trim_optional(web_link) {
        return link;
    }
    if let Some(web) = trim_optional(repository_web_url) {
        return format!("{}/pullrequest/{pull_request_id}", web.trim_end_matches('/'));
    }
    if let (Some(base), Some(project), Some(repository)) = (
        organization_base_from_rest_url(rest_api_url),
        trim_optional(project_name),
        trim_optional(repository_name),
    ) {
        return format!("{base}/{}/_git/{}/pullrequest/{pull_request_id}", encode_segment(&project), encode_segment(&repository));
    }
    trim_optional(rest_api_url).unwrap_or_default()
}

fn strip_heads(name: &str) -> String {
    let trimmed = t3_contracts::trim_wire_string(name);
    trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed).to_string()
}

fn object_field<'a>(value: Option<&'a Value>, name: &str) -> Field<Option<&'a Value>> {
    match value {
        None => Ok(None),
        Some(Value::Object(object)) => Ok(object.get(name)),
        Some(_) => Err(()),
    }
}

fn decode_row(entry: &Value) -> Field<ChangeRequest> {
    let object = entry.as_object().ok_or(())?;
    let number = positive_int(object.get("pullRequestId"))?;
    let title = trimmed_non_empty(object.get("title"))?;
    let rest_url = optional_string(object.get("url"))?;
    let repository = match object.get("repository") {
        None => None,
        Some(Value::Object(repository)) => Some(repository),
        Some(_) => return Err(()),
    };
    let repository_name = optional_string(repository.and_then(|r| r.get("name")))?;
    let repository_web_url = optional_string(repository.and_then(|r| r.get("webUrl")))?;
    let project = repository.and_then(|r| r.get("project"));
    let project_name = optional_string(object_field(project, "name")?)?;
    let source = trimmed_non_empty(object.get("sourceRefName"))?;
    let target = trimmed_non_empty(object.get("targetRefName"))?;
    let status = match object.get("status") {
        Some(Value::String(status)) => status.clone(),
        _ => return Err(()),
    };
    let is_draft = optional_bool(object.get("isDraft"))?;
    let created = optional_date(object.get("creationDate"))?;
    let closed = optional_date(object.get("closedDate"))?;
    let links = object.get("_links");
    let web = object_field(links, "web")?;
    let web_link = match web {
        None => None,
        Some(Value::Object(web)) => match web.get("href") {
            Some(Value::String(href)) => Some(href.clone()),
            _ => return Err(()),
        },
        Some(_) => return Err(()),
    };
    let state = match status.trim().to_lowercase().as_str() {
        "completed" => ChangeRequestState::Merged,
        "abandoned" => ChangeRequestState::Closed,
        _ => ChangeRequestState::Open,
    };
    Ok(ChangeRequest {
        provider: KIND,
        number,
        title,
        url: pull_request_web_url(
            number,
            web_link.as_deref(),
            repository_web_url.as_deref(),
            rest_url.as_deref(),
            project_name.as_deref(),
            repository_name.as_deref(),
        ),
        base_ref_name: strip_heads(&target),
        head_ref_name: strip_heads(&source),
        head_sha: None,
        state,
        is_draft: (is_draft == Some(true)).then_some(true),
        closed_at: Some(if state == ChangeRequestState::Closed { closed.clone() } else { None }),
        merged_at: Some(if state == ChangeRequestState::Merged { closed.clone() } else { None }),
        updated_at: closed.or(created),
        is_cross_repository: Some(false),
        head_repository_name_with_owner: None,
        head_repository_owner_login: None,
    })
}

pub fn decode_pull_request_list_json(raw: &str) -> Result<Vec<ChangeRequest>, String> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(entries)) => Ok(entries.iter().filter_map(|entry| decode_row(entry).ok()).collect()),
        Ok(_) => Err("Expected an array".into()),
        Err(error) => Err(error.to_string()),
    }
}

pub fn decode_pull_request_json(raw: &str) -> Result<ChangeRequest, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    decode_row(&value).map_err(|()| "Invalid pull request".to_string())
}

// ---- the CLI ----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureDevOpsCliError {
    pub tag: &'static str,
    pub operation: &'static str,
    pub detail: String,
    pub cause: Option<ErrorCause>,
}

impl AzureDevOpsCliError {
    fn new(tag: &'static str, operation: &'static str, detail: &str) -> Self {
        Self { tag, operation, detail: detail.into(), cause: None }
    }

    pub fn message(&self) -> String {
        format!("Azure DevOps CLI failed in {}: {}", self.operation, self.detail)
    }

    fn from_vcs(error: &VcsError, cwd: &str) -> Self {
        use t3_vcs::error::ExitFailureKind::*;
        let (tag, detail) = match error {
            // A missing working directory fails the spawn too; that is not a missing `az`.
            VcsError::ProcessSpawn { cause, .. } if cause.kind() == std::io::ErrorKind::NotFound && Path::new(cwd).exists() => (
                "AzureDevOpsCliUnavailableError",
                "Azure CLI (`az`) with the Azure DevOps extension is required but not available on PATH.",
            ),
            VcsError::ProcessExit { failure_kind: Some(Authentication), .. } => (
                "AzureDevOpsCliAuthenticationError",
                "Azure DevOps CLI is not authenticated. Run `az devops login` and retry.",
            ),
            VcsError::ProcessExit { failure_kind: Some(RateLimited), .. } => {
                ("AzureDevOpsCliRateLimitError", "Azure DevOps API rate limit exceeded.")
            }
            VcsError::ProcessExit { failure_kind: Some(NotFound), .. } => (
                "AzureDevOpsPullRequestNotFoundError",
                "Pull request not found. Check the PR number or URL and try again.",
            ),
            _ => ("AzureDevOpsCommandFailedError", "Azure DevOps CLI command failed."),
        };
        Self { cause: Some(ErrorCause::new(error.tag(), error.to_string())), ..Self::new(tag, "execute", detail) }
    }
}

fn normalize_change_request_id(reference: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let trimmed = t3_contracts::trim_wire_string(reference);
    let trimmed = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let pattern = PATTERN.get_or_init(|| Regex::new(r"(?i)(?:pullrequest|pull-request|pull|_pulls?)/(\d+)(?:\D.*)?$").unwrap());
    pattern.captures(trimmed).map_or_else(|| trimmed.to_string(), |captures| captures[1].to_string())
}

fn normalize_default_branch(value: Option<&str>) -> Option<String> {
    let trimmed = t3_contracts::trim_wire_string(value.unwrap_or(""));
    let stripped = trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed);
    (!stripped.is_empty()).then(|| stripped.to_string())
}

/// `[project/]name`, from the last two non-empty parts.
fn parse_repository_specifier(repository: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = repository
        .split('/')
        .map(t3_contracts::trim_wire_string)
        .filter(|part| !part.is_empty())
        .collect();
    let name = parts.last().map_or_else(|| t3_contracts::trim_wire_string(repository).to_string(), |last| last.to_string());
    let project = (parts.len() > 1).then(|| parts[parts.len() - 2].to_string());
    (project, name)
}

pub struct AzureDevOpsCli {
    host: Host,
}

struct RawRepository {
    name: String,
    remote_url: String,
    ssh_url: String,
    project: Option<String>,
    default_branch: Option<String>,
}

impl AzureDevOpsCli {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    async fn execute(&self, cwd: &str, args: Vec<String>, json: bool) -> Result<String, AzureDevOpsCliError> {
        let mut args = args;
        if json {
            args.extend(["--only-show-errors".to_string(), "--output".into(), "json".into()]);
        }
        let mut input: VcsProcessInput = self.host.cli("AzureDevOpsCli.execute", "az", args, cwd);
        input.timeout = Some(DEFAULT_TIMEOUT);
        self.host
            .process()
            .run(input)
            .await
            .map(|output| output.stdout)
            .map_err(|error| AzureDevOpsCliError::from_vcs(&error, cwd))
    }

    fn decode_repository(raw: &str, operation: &'static str) -> Result<RawRepository, AzureDevOpsCliError> {
        let failure = || AzureDevOpsCliError::new("AzureDevOpsRepositoryDecodeError", operation, "Azure DevOps CLI returned invalid repository JSON.");
        let value: Value = serde_json::from_str(raw).map_err(|_| failure())?;
        let field = |name: &str| trimmed_non_empty(value.get(name)).map_err(|()| failure());
        let name = field("name")?;
        field("webUrl")?;
        let remote_url = field("remoteUrl")?;
        let ssh_url = field("sshUrl")?;
        let project = match value.get("project") {
            None => None,
            Some(Value::Object(project)) => Some(trimmed_non_empty(project.get("name")).map_err(|()| failure())?),
            Some(_) => return Err(failure()),
        };
        let default_branch = optional_nullable_string(value.get("defaultBranch")).map_err(|()| failure())?;
        Ok(RawRepository { name, remote_url, ssh_url, project, default_branch })
    }

    fn clone_urls(raw: RawRepository) -> SourceControlRepositoryCloneUrls {
        let project = raw.project.as_deref().map(str::trim).filter(|project| !project.is_empty());
        SourceControlRepositoryCloneUrls {
            name_with_owner: match project {
                Some(project) => format!("{project}/{}", raw.name),
                None => raw.name,
            },
            url: raw.remote_url,
            ssh_url: raw.ssh_url,
        }
    }

    pub async fn list_pull_requests(
        &self,
        cwd: &str,
        head_selector: &str,
        source: Option<&RefSelector>,
        state: ListState,
        limit: Option<u32>,
    ) -> Result<Vec<ChangeRequest>, AzureDevOpsCliError> {
        let status = match state {
            ListState::Open => "active",
            ListState::Closed => "abandoned",
            ListState::Merged => "completed",
            ListState::All => "all",
        };
        let args = vec![
            "repos".to_string(), "pr".into(), "list".into(), "--detect".into(), "true".into(), "--source-branch".into(),
            source_branch(head_selector, source), "--status".into(), status.into(), "--top".into(), limit.unwrap_or(20).to_string(),
        ];
        let stdout = self.execute(cwd, args, true).await?;
        let raw = stdout.trim();
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        decode_pull_request_list_json(raw).map_err(|_| AzureDevOpsCliError::new(
            "AzureDevOpsPullRequestListDecodeError",
            "listPullRequests",
            "Azure DevOps CLI returned invalid PR list JSON.",
        ))
    }

    pub async fn get_pull_request(&self, cwd: &str, reference: &str) -> Result<ChangeRequest, AzureDevOpsCliError> {
        let args = ["repos", "pr", "show", "--detect", "true", "--id"].map(String::from).into_iter().chain([normalize_change_request_id(reference)]).collect();
        let stdout = self.execute(cwd, args, true).await?;
        decode_pull_request_json(stdout.trim()).map_err(|_| AzureDevOpsCliError::new(
            "AzureDevOpsPullRequestDecodeError",
            "getPullRequest",
            "Azure DevOps CLI returned invalid pull request JSON.",
        ))
    }

    pub async fn get_repository_clone_urls(&self, cwd: &str, repository: &str) -> Result<SourceControlRepositoryCloneUrls, AzureDevOpsCliError> {
        let args = ["repos", "show", "--detect", "true", "--repository"].map(String::from).into_iter().chain([repository.to_string()]).collect();
        let stdout = self.execute(cwd, args, true).await?;
        Self::decode_repository(stdout.trim(), "getRepositoryCloneUrls").map(Self::clone_urls)
    }

    /// Azure Repos access is governed by project and organization permissions, and `az repos
    /// create` has no per-repository visibility flag, so the visibility is not passed on.
    pub async fn create_repository(
        &self,
        cwd: &str,
        repository: &str,
        _visibility: SourceControlRepositoryVisibility,
    ) -> Result<SourceControlRepositoryCloneUrls, AzureDevOpsCliError> {
        let (project, name) = parse_repository_specifier(repository);
        let mut args: Vec<String> = ["repos", "create", "--detect", "true", "--name"].map(String::from).to_vec();
        args.push(name);
        if let Some(project) = project {
            args.extend(["--project".to_string(), project]);
        }
        let stdout = self.execute(cwd, args, true).await?;
        Self::decode_repository(stdout.trim(), "createRepository").map(Self::clone_urls)
    }

    pub async fn create_pull_request(
        &self,
        cwd: &str,
        base_branch: &str,
        head_selector: &str,
        source: Option<&RefSelector>,
        target: Option<&RefSelector>,
        title: &str,
        body_file: &str,
    ) -> Result<(), AzureDevOpsCliError> {
        let args = vec![
            "repos".to_string(), "pr".into(), "create".into(), "--only-show-errors".into(), "--detect".into(), "true".into(),
            "--target-branch".into(), target.map_or(base_branch, |target| target.ref_name.as_str()).to_string(),
            "--source-branch".into(), source_branch(head_selector, source),
            "--title".into(), title.to_string(), "--description".into(), format!("@{body_file}"),
        ];
        self.execute(cwd, args, false).await.map(|_| ())
    }

    pub async fn get_default_branch(&self, cwd: &str) -> Result<Option<String>, AzureDevOpsCliError> {
        let args = ["repos", "show", "--detect", "true"].map(String::from).to_vec();
        let stdout = self.execute(cwd, args, true).await?;
        Self::decode_repository(stdout.trim(), "getDefaultBranch").map(|raw| normalize_default_branch(raw.default_branch.as_deref()))
    }

    pub async fn checkout_pull_request(&self, cwd: &str, reference: &str, remote_name: Option<&str>) -> Result<(), AzureDevOpsCliError> {
        let args = vec![
            "repos".to_string(), "pr".into(), "checkout".into(), "--only-show-errors".into(), "--detect".into(), "true".into(),
            "--id".into(), normalize_change_request_id(reference), "--remote-name".into(), remote_name.unwrap_or("origin").to_string(),
        ];
        self.execute(cwd, args, false).await.map(|_| ())
    }
}

// ---- the provider -----------------------------------------------------------------------------

fn cli_failure(operation: &str, cwd: &str, error: &AzureDevOpsCliError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, error.detail.clone())
        .with_command(Some("az"))
        .with_cause_chain(Some(ErrorCause::new(error.tag, error.message()).with_nested(error.cause.clone())))
}

pub struct AzureDevOpsProvider {
    cli: AzureDevOpsCli,
}

impl AzureDevOpsProvider {
    pub fn new(host: Host) -> Self {
        Self { cli: AzureDevOpsCli::new(host) }
    }
}

impl SourceControlProvider for AzureDevOpsProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn list_change_requests(&self, input: ListChangeRequestsInput) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>> {
        Box::pin(async move {
            let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
            self.cli
                .list_pull_requests(&input.cwd, &input.head_selector, source.as_ref(), input.state, input.limit)
                .await
                .map_err(|error| cli_failure("listChangeRequests", &input.cwd, &error).with_reference(&transport_safe_value(&input.head_selector)))
        })
    }

    fn get_change_request(&self, input: GetChangeRequestInput) -> BoxFuture<'_, ProviderResult<ChangeRequest>> {
        Box::pin(async move {
            self.cli
                .get_pull_request(&input.cwd, &input.reference)
                .await
                .map_err(|error| cli_failure("getChangeRequest", &input.cwd, &error).with_reference(&transport_safe_value(&input.reference)))
        })
    }

    fn create_change_request(&self, input: CreateChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
            self.cli
                .create_pull_request(
                    &input.cwd,
                    &input.base_ref_name,
                    &input.head_selector,
                    source.as_ref(),
                    input.target.as_ref(),
                    &input.title,
                    &input.body_file,
                )
                .await
                .map_err(|error| cli_failure("createChangeRequest", &input.cwd, &error).with_reference(&transport_safe_value(&input.head_selector)))
        })
    }

    fn get_repository_clone_urls(&self, input: RepositoryUrlsInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.cli
                .get_repository_clone_urls(&input.cwd, &input.repository)
                .await
                .map_err(|error| cli_failure("getRepositoryCloneUrls", &input.cwd, &error).with_repository(&transport_safe_value(&input.repository)))
        })
    }

    fn create_repository(&self, input: CreateRepositoryInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.cli
                .create_repository(&input.cwd, &input.repository, input.visibility)
                .await
                .map_err(|error| cli_failure("createRepository", &input.cwd, &error).with_repository(&transport_safe_value(&input.repository)))
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
            let remote = input.context.as_ref().map(|context| context.remote_name.as_str());
            self.cli
                .checkout_pull_request(&input.cwd, &input.reference, remote)
                .await
                .map_err(|error| cli_failure("checkoutChangeRequest", &input.cwd, &error).with_reference(&transport_safe_value(&input.reference)))
        })
    }
}
