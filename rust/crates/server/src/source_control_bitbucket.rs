//! Bitbucket Cloud over its REST API (port of `BitbucketApi.ts`, `bitbucketPullRequests.ts` and
//! `BitbucketSourceControlProvider.ts`). There is no CLI: the credential is an access token or
//! an email and API token, saved in Settings or read from `T3CODE_BITBUCKET_*` variables, and
//! the repository comes from the checkout's Bitbucket remote or an explicit `workspace/repo`.
//!
//! Divergences from the original: requests carry a 30 second deadline (the original has none),
//! and `request` (the raw escape hatch for diffs) is not ported; the pull request work that
//! needs it follows manual same-origin redirects with `Shared::http_manual`.
use crate::{
    source_control_decode::*,
    source_control_detect::{
        RefSelector, detect_provider_from_remote_url, is_ssh_remote_url, parse_owner_ref, source_branch,
        source_ref_from_input, transport_safe_value,
    },
    source_control_discovery::{ApiProbe, provider_auth},
    source_control_host::Host,
    source_control_provider::*,
    source_control_uri::encode_uri_component,
};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{Value, json};
use std::{path::Path, sync::OnceLock, time::Duration};
use t3_contracts::{
    ChangeRequest, ChangeRequestState, ErrorCause, SourceControlProviderAuth, SourceControlProviderAuthStatus,
    SourceControlProviderError, SourceControlProviderKind,
    SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility,
};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Bitbucket;
const DEFAULT_API_BASE_URL: &str = "https://api.bitbucket.org/2.0";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---- errors -----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketError {
    pub tag: &'static str,
    pub message: String,
    pub retry_at: Option<i64>,
}

impl BitbucketError {
    fn new(tag: &'static str, message: String) -> Self {
        Self { tag, message, retry_at: None }
    }

    fn locator() -> Self {
        Self::new(
            "BitbucketRepositoryLocatorError",
            format!("Bitbucket API failed: {BITBUCKET_LOCATOR_DETAIL}"),
        )
    }

    fn request(operation: &str) -> Self {
        Self::new("BitbucketRequestError", format!("Bitbucket API failed in {operation}: Failed to send the Bitbucket request."))
    }

    fn response(operation: &str, status: u16, retry_at: Option<i64>) -> Self {
        Self {
            retry_at,
            ..Self::new("BitbucketResponseError", format!("Bitbucket API failed in {operation}: Bitbucket returned HTTP {status}."))
        }
    }

    fn decode(operation: &str) -> Self {
        Self::new(
            "BitbucketResponseDecodeError",
            format!("Bitbucket API failed in {operation}: Bitbucket returned invalid JSON for the requested resource."),
        )
    }

    fn resolve(detail: String) -> Self {
        Self::new("BitbucketRepositoryError", format!("Bitbucket API failed in resolveRepository: {detail}"))
    }

    pub fn cause(&self) -> ErrorCause {
        ErrorCause::new(self.tag, self.message.clone())
    }
}

pub const BITBUCKET_LOCATOR_DETAIL: &str = "Bitbucket repositories must be specified as workspace/repository.";

type BitbucketResult<T> = Result<T, BitbucketError>;

// ---- credentials ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BitbucketCredential {
    AccessToken(String),
    ApiToken { email: String, api_token: String },
}

/// Visible ASCII only. A value the HTTP stack rejects makes it throw an error quoting the whole
/// header, so an unusable token is treated as unset.
fn header_safe(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn credential_from(access_token: &str, email: &str, api_token: &str) -> Option<BitbucketCredential> {
    if header_safe(access_token) {
        return Some(BitbucketCredential::AccessToken(access_token.to_string()));
    }
    (header_safe(email) && header_safe(api_token))
        .then(|| BitbucketCredential::ApiToken { email: email.to_string(), api_token: api_token.to_string() })
}

/// Credentials saved in settings win over the `T3CODE_BITBUCKET_*` variables, which stay as a
/// fallback. Within each source the access token wins.
pub async fn current_credential(host: &Host) -> Option<BitbucketCredential> {
    if let Some(settings) = host.settings().await
        && let Some(saved) = credential_from(
            settings.bitbucket.access_token.as_str(),
            settings.bitbucket.email.as_str(),
            settings.bitbucket.api_token.as_str(),
        )
    {
        return Some(saved);
    }
    let env = host.env();
    credential_from(
        env.get("T3CODE_BITBUCKET_ACCESS_TOKEN").unwrap_or(""),
        env.get("T3CODE_BITBUCKET_EMAIL").unwrap_or(""),
        env.get("T3CODE_BITBUCKET_API_TOKEN").unwrap_or(""),
    )
}

fn auth_from_credential(credential: Option<&BitbucketCredential>) -> SourceControlProviderAuth {
    use SourceControlProviderAuthStatus::*;
    match credential {
        Some(BitbucketCredential::AccessToken(_)) => {
            provider_auth(Unknown, None, Some("bitbucket.org"), Some("An access token is configured."))
        }
        Some(BitbucketCredential::ApiToken { email, .. }) => {
            provider_auth(Unknown, Some(email), Some("bitbucket.org"), Some("An API token is configured."))
        }
        None => provider_auth(
            Unauthenticated,
            None,
            Some("bitbucket.org"),
            Some("Add a Bitbucket token in Settings → Source Control, or set the T3CODE_BITBUCKET_* environment variables on the server."),
        ),
    }
}

// ---- pull request rows ------------------------------------------------------------------------

fn repository_ref(value: Option<&Value>) -> Field<Option<(Option<String>, Option<String>)>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(repository)) => {
            let full_name = optional_trimmed_non_empty(repository.get("full_name"))?;
            let slug = match repository.get("workspace") {
                None | Some(Value::Null) => None,
                Some(Value::Object(workspace)) => optional_trimmed_non_empty(workspace.get("slug"))?,
                Some(_) => return Err(()),
            };
            Ok(Some((full_name, slug)))
        }
        Some(_) => Err(()),
    }
}

fn branch_side(value: Option<&Value>) -> Field<(Option<(Option<String>, Option<String>)>, String)> {
    let object = value.and_then(Value::as_object).ok_or(())?;
    let repository = repository_ref(object.get("repository"))?;
    let branch = object.get("branch").and_then(Value::as_object).ok_or(())?;
    Ok((repository, trimmed_non_empty(branch.get("name"))?))
}

fn decode_row(entry: &Value) -> Field<ChangeRequest> {
    let object = entry.as_object().ok_or(())?;
    let number = positive_int(object.get("id"))?;
    let title = trimmed_non_empty(object.get("title"))?;
    let state = optional_nullable_string(object.get("state"))?;
    let draft = optional_bool(object.get("draft"))?;
    let updated_at = optional_date(object.get("updated_on"))?;
    let html = object.get("links").and_then(|links| links.get("html")).and_then(Value::as_object).ok_or(())?;
    let url = trimmed_non_empty(html.get("href"))?;
    let (source_repository, head_ref_name) = branch_side(object.get("source"))?;
    let (destination_repository, base_ref_name) = branch_side(object.get("destination"))?;
    let head_name = trim_optional(source_repository.as_ref().and_then(|(full, _)| full.as_deref()));
    let base_name = trim_optional(destination_repository.as_ref().and_then(|(full, _)| full.as_deref()));
    let owner = source_repository.as_ref().and_then(|(full, slug)| {
        trim_optional(slug.as_deref()).or_else(|| {
            full.as_ref()
                .filter(|full| full.contains('/'))
                .and_then(|full| full.split('/').next().map(str::to_string))
                .filter(|owner| !owner.is_empty())
        })
    });
    let is_cross = matches!((&head_name, &base_name), (Some(head), Some(base)) if head != base);
    Ok(ChangeRequest {
        provider: KIND,
        number,
        title,
        url,
        base_ref_name,
        head_ref_name,
        head_sha: None,
        state: match state.as_deref().map(|state| state.trim().to_uppercase()).as_deref() {
            Some("MERGED") => ChangeRequestState::Merged,
            Some("DECLINED" | "SUPERSEDED") => ChangeRequestState::Closed,
            _ => ChangeRequestState::Open,
        },
        is_draft: (draft == Some(true)).then_some(true),
        closed_at: None,
        merged_at: None,
        updated_at,
        is_cross_repository: is_cross.then_some(true),
        head_repository_name_with_owner: head_name.map(Some),
        head_repository_owner_login: owner.map(Some),
    })
}

pub fn decode_pull_request(raw: &Value) -> Option<ChangeRequest> {
    decode_row(raw).ok()
}

/// A page of pull requests: every row must decode, or the page does.
pub fn decode_pull_request_page(raw: &Value) -> Option<Vec<ChangeRequest>> {
    // `next`, when present, must be a non-blank string.
    optional_trimmed_non_empty(raw.get("next")).ok()?;
    if raw.get("next").is_some_and(Value::is_null) {
        return None;
    }
    let values = raw.get("values")?.as_array()?;
    values.iter().map(|entry| decode_row(entry).ok()).collect()
}

// ---- repository selectors ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketLocator {
    pub workspace: String,
    pub repo_slug: String,
}

pub fn parse_repository_slug(value: &str) -> Option<BitbucketLocator> {
    let trimmed = t3_contracts::trim_wire_string(value);
    let normalized = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let parts: Vec<&str> = normalized.split('/').filter(|part| !part.is_empty()).collect();
    match parts.as_slice() {
        [.., workspace, repo_slug] => Some(BitbucketLocator { workspace: (*workspace).into(), repo_slug: (*repo_slug).into() }),
        _ => None,
    }
}

pub fn parse_remote_url(remote_url: &str) -> Option<BitbucketLocator> {
    static SCP: OnceLock<Regex> = OnceLock::new();
    let trimmed = t3_contracts::trim_wire_string(remote_url);
    let scp = SCP.get_or_init(|| Regex::new(r"^[a-zA-Z0-9._-]+@[^:/]+:(.+)$").unwrap());
    if let Some(captures) = scp.captures(trimmed)
        && !captures[1].is_empty()
    {
        return parse_repository_slug(&captures[1]);
    }
    url::Url::parse(trimmed).ok().and_then(|url| parse_repository_slug(url.path()))
}

fn normalize_change_request_id(reference: &str) -> String {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let trimmed = t3_contracts::trim_wire_string(reference);
    let trimmed = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let pattern = PATTERN.get_or_init(|| Regex::new(r"(?i)(?:pull-requests|pullrequests|pull-request|pull|pr)/(\d+)(?:\D.*)?$").unwrap());
    pattern.captures(trimmed).map_or_else(|| trimmed.to_string(), |captures| captures[1].to_string())
}

/// `sanitizeBranchFragment`: a lowercase ref-safe fragment of at most 64 characters.
pub fn sanitize_branch_fragment(raw: &str) -> String {
    static EDGES: OnceLock<[Regex; 4]> = OnceLock::new();
    let [edge, bad, repeated_dash, outer] = EDGES.get_or_init(|| {
        [
            Regex::new(r"^[./\s_-]+|[./\s_-]+$").unwrap(),
            Regex::new(r"[^a-z0-9/_-]+").unwrap(),
            Regex::new(r"-+").unwrap(),
            Regex::new(r"^[./_-]+|[./_-]+$").unwrap(),
        ]
    });
    let lowered = t3_contracts::trim_wire_string(raw).to_lowercase().replace(['\'', '"', '`'], "");
    let normalized = edge.replace_all(&lowered, "").into_owned();
    let dashed = bad.replace_all(&normalized, "-").into_owned();
    let mut slashes = String::new();
    for c in dashed.chars() {
        if !(c == '/' && slashes.ends_with('/')) {
            slashes.push(c);
        }
    }
    let collapsed = repeated_dash.replace_all(&slashes, "-").into_owned();
    let trimmed = outer.replace_all(&collapsed, "").into_owned();
    let cut: String = trimmed.chars().take(64).collect();
    let fragment = Regex::new(r"[./_-]+$").unwrap().replace(&cut, "").into_owned();
    if fragment.is_empty() { "update".into() } else { fragment }
}

fn checkout_branch_name(pull_request_id: u64, head_branch: &str, is_cross_repository: bool) -> String {
    if is_cross_repository {
        format!("t3code/pr-{pull_request_id}/{}", sanitize_branch_fragment(head_branch))
    } else {
        head_branch.to_string()
    }
}

fn clone_urls(raw: &Value) -> Field<SourceControlRepositoryCloneUrls> {
    let full_name = trimmed_non_empty(raw.get("full_name"))?;
    let links = raw.get("links").and_then(Value::as_object).ok_or(())?;
    let html = match links.get("html") {
        None => None,
        Some(Value::Object(html)) => Some(trimmed_non_empty(html.get("href"))?),
        Some(_) => return Err(()),
    };
    let mut clones = Vec::new();
    match links.get("clone") {
        None => {}
        Some(Value::Array(entries)) => {
            for entry in entries {
                clones.push((trimmed_non_empty(entry.get("name"))?, trimmed_non_empty(entry.get("href"))?));
            }
        }
        Some(_) => return Err(()),
    }
    match raw.get("mainbranch") {
        None | Some(Value::Null) => {}
        Some(Value::Object(branch)) => {
            trimmed_non_empty(branch.get("name"))?;
        }
        Some(_) => return Err(()),
    }
    let find = |wanted: &str| clones.iter().find(|(name, _)| name.to_lowercase() == wanted).map(|(_, href)| href.clone());
    let http = find("https").or_else(|| html.clone());
    let ssh = find("ssh");
    Ok(SourceControlRepositoryCloneUrls {
        name_with_owner: full_name.clone(),
        url: http.clone().or(html).unwrap_or_else(|| full_name.clone()),
        ssh_url: ssh.or(http).unwrap_or(full_name),
    })
}

/// The branch change requests target by default: the development branch of the branching model
/// unless it is the main branch, invalid or unnamed.
fn default_target_branch(repository: &Value, branching_model: Option<&Value>) -> Option<String> {
    let main = repository.get("mainbranch").and_then(|branch| branch.get("name")).and_then(Value::as_str).map(str::to_string);
    let Some(development) = branching_model.and_then(|model| model.get("development")).filter(|d| d.is_object()) else {
        return main;
    };
    if development["use_mainbranch"] == true || development["is_valid"] == false {
        return main;
    }
    let named = development
        .get("branch")
        .and_then(|branch| branch.get("name"))
        .and_then(Value::as_str)
        .map(|name| name.trim().to_string())
        .or_else(|| development.get("name").and_then(Value::as_str).map(|name| name.trim().to_string()))
        .unwrap_or_default();
    if named.is_empty() || named == "null" { main } else { Some(named) }
}

fn states_of(state: ListState) -> Vec<&'static str> {
    match state {
        ListState::Open => vec!["OPEN"],
        ListState::Closed => vec!["DECLINED", "SUPERSEDED"],
        ListState::Merged => vec!["MERGED"],
        ListState::All => vec!["OPEN", "MERGED", "DECLINED", "SUPERSEDED"],
    }
}

fn state_filter(states: &[&str]) -> String {
    if states.len() == 1 {
        format!("state = \"{}\"", states[0])
    } else {
        format!("({})", states.iter().map(|state| format!("state = \"{state}\"")).collect::<Vec<_>>().join(" OR "))
    }
}

// ---- the API ----------------------------------------------------------------------------------

pub struct BitbucketApi {
    host: Host,
}

impl BitbucketApi {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    fn api_url(&self, path: &str) -> String {
        let base = self.host.env().get("T3CODE_BITBUCKET_API_BASE_URL").unwrap_or(DEFAULT_API_BASE_URL);
        format!("{}{path}", base.trim_end_matches('/'))
    }

    /// One JSON request; the decoded body, or the error that says why not.
    async fn execute_json(
        &self,
        operation: &str,
        method: reqwest::Method,
        url: String,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> BitbucketResult<Value> {
        let mut request = self.host.shared.http.request(method, url).header("accept", "application/json").timeout(REQUEST_TIMEOUT);
        if !query.is_empty() {
            request = request.query(query);
        }
        match current_credential(&self.host).await {
            Some(BitbucketCredential::AccessToken(token)) => request = request.bearer_auth(token),
            Some(BitbucketCredential::ApiToken { email, api_token }) => request = request.basic_auth(email, Some(api_token)),
            None => {}
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| BitbucketError::request(operation))?;
        let status = response.status().as_u16();
        let retry_at = crate::source_control_rate_limit::retry_at_from_header(
            response.headers().get("retry-after").and_then(|value| value.to_str().ok()),
            self.host.now_ms(),
        );
        let bytes = response.bytes().await.map_err(|_| BitbucketError::response(operation, status, retry_at))?;
        let bytes = &bytes[..bytes.len().min(MAX_RESPONSE_BYTES)];
        if !(200..300).contains(&status) {
            return Err(BitbucketError::response(operation, status, retry_at));
        }
        serde_json::from_slice(bytes).map_err(|_| BitbucketError::decode(operation))
    }

    async fn resolve_repository(
        &self,
        cwd: &str,
        context: Option<&ProviderContext>,
        repository: Option<&str>,
    ) -> BitbucketResult<BitbucketLocator> {
        if let Some(repository) = repository {
            return parse_repository_slug(repository).ok_or_else(BitbucketError::locator);
        }
        if let Some(context) = context.filter(|context| context.provider.kind == KIND)
            && let Some(locator) = parse_remote_url(&context.remote_url)
        {
            return Ok(locator);
        }
        let vcs = &self.host.shared.vcs;
        let handle = vcs
            .resolve(cwd, None)
            .await
            .map_err(|_| BitbucketError::resolve(format!("Failed to resolve VCS repository for {cwd}.")))?;
        let driver = vcs
            .driver(handle.kind)
            .map_err(|_| BitbucketError::resolve(format!("Failed to resolve VCS repository for {cwd}.")))?;
        let remotes = driver
            .list_remotes(Path::new(cwd))
            .await
            .map_err(|_| BitbucketError::resolve(format!("Failed to list remotes for {cwd}.")))?;
        for remote in remotes.remotes {
            if detect_provider_from_remote_url(&remote.url).map(|info| info.kind) != Some(KIND) {
                continue;
            }
            if let Some(locator) = parse_remote_url(&remote.url) {
                return Ok(locator);
            }
        }
        Err(BitbucketError::resolve(format!("No Bitbucket repository remote was detected for {cwd}.")))
    }

    fn repository_path(locator: &BitbucketLocator) -> String {
        format!("/repositories/{}/{}", encode_uri_component(&locator.workspace), encode_uri_component(&locator.repo_slug))
    }

    async fn get_repository(&self, locator: &BitbucketLocator) -> BitbucketResult<Value> {
        self.execute_json("getRepository", reqwest::Method::GET, self.api_url(&Self::repository_path(locator)), &[], None).await
    }

    async fn get_raw_pull_request(&self, locator: &BitbucketLocator, reference: &str) -> BitbucketResult<Value> {
        let path = format!("{}/pullrequests/{}", Self::repository_path(locator), encode_uri_component(&normalize_change_request_id(reference)));
        self.execute_json("getPullRequest", reqwest::Method::GET, self.api_url(&path), &[], None).await
    }

    pub async fn probe_auth(&self) -> SourceControlProviderAuth {
        let user = self.execute_json("probeAuth", reqwest::Method::GET, self.api_url("/user"), &[], None).await;
        if let Ok(user) = user {
            let account = ["username", "display_name", "account_id"]
                .iter()
                .find_map(|key| trimmed_non_empty(user.get(*key)).ok());
            return provider_auth(SourceControlProviderAuthStatus::Authenticated, account.as_deref(), Some("bitbucket.org"), None);
        }
        auth_from_credential(current_credential(&self.host).await.as_ref())
    }

    pub async fn list_pull_requests(
        &self,
        cwd: &str,
        context: Option<&ProviderContext>,
        head_selector: &str,
        source: Option<&RefSelector>,
        state: ListState,
        limit: Option<u32>,
    ) -> BitbucketResult<Vec<ChangeRequest>> {
        let locator = self.resolve_repository(cwd, context, None).await?;
        let states = states_of(state);
        let branch = source_branch(head_selector, source).replace('"', "\\\"");
        let mut query = vec![
            ("pagelen".to_string(), limit.unwrap_or(20).clamp(1, 50).to_string()),
            ("sort".into(), "-updated_on".into()),
            ("q".into(), format!("source.branch.name = \"{branch}\" AND {}", state_filter(&states))),
        ];
        query.extend(states.iter().map(|state| ("state".to_string(), (*state).to_string())));
        let path = format!("{}/pullrequests", Self::repository_path(&locator));
        let page = self.execute_json("listPullRequests", reqwest::Method::GET, self.api_url(&path), &query, None).await?;
        decode_pull_request_page(&page).ok_or_else(|| BitbucketError::decode("listPullRequests"))
    }

    pub async fn get_pull_request(&self, cwd: &str, context: Option<&ProviderContext>, reference: &str) -> BitbucketResult<ChangeRequest> {
        let locator = self.resolve_repository(cwd, context, None).await?;
        let raw = self.get_raw_pull_request(&locator, reference).await?;
        decode_pull_request(&raw).ok_or_else(|| BitbucketError::decode("getPullRequest"))
    }

    pub async fn get_repository_clone_urls(
        &self,
        cwd: &str,
        context: Option<&ProviderContext>,
        repository: &str,
    ) -> BitbucketResult<SourceControlRepositoryCloneUrls> {
        let locator = self.resolve_repository(cwd, context, Some(repository)).await?;
        clone_urls(&self.get_repository(&locator).await?).map_err(|()| BitbucketError::decode("getRepository"))
    }

    pub async fn create_repository(
        &self,
        repository: &str,
        visibility: SourceControlRepositoryVisibility,
    ) -> BitbucketResult<SourceControlRepositoryCloneUrls> {
        let locator = parse_repository_slug(repository).ok_or_else(BitbucketError::locator)?;
        let body = json!({"scm":"git","is_private":visibility == SourceControlRepositoryVisibility::Private});
        let raw = self
            .execute_json("createRepository", reqwest::Method::POST, self.api_url(&Self::repository_path(&locator)), &[], Some(body))
            .await?;
        clone_urls(&raw).map_err(|()| BitbucketError::decode("createRepository"))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_pull_request(
        &self,
        cwd: &str,
        context: Option<&ProviderContext>,
        base_branch: &str,
        head_selector: &str,
        source: Option<&RefSelector>,
        target: Option<&RefSelector>,
        title: &str,
        body_file: &str,
    ) -> BitbucketResult<()> {
        let locator = self.resolve_repository(cwd, context, None).await?;
        let description = tokio::fs::read_to_string(body_file).await.map_err(|_| {
            BitbucketError::new(
                "BitbucketPullRequestBodyReadError",
                format!("Bitbucket API failed in createPullRequest: Failed to read pull request body file {body_file}."),
            )
        })?;
        let owner = source
            .and_then(|source| source.owner.clone())
            .or_else(|| parse_owner_ref(head_selector).map(|(owner, _)| owner))
            .filter(|owner| !owner.is_empty());
        let mut source_side = json!({"branch":{"name":source_branch(head_selector, source)}});
        if let Some(owner) = owner {
            let slug = source.and_then(|source| source.repository.clone()).unwrap_or_else(|| locator.repo_slug.clone());
            source_side["repository"] = json!({"full_name":format!("{owner}/{slug}")});
        }
        let body = json!({
            "title":title,"description":description,"source":source_side,
            "destination":{"branch":{"name":target.map_or(base_branch, |target| target.ref_name.as_str())}},
        });
        let path = format!("{}/pullrequests", Self::repository_path(&locator));
        let created = self.execute_json("createPullRequest", reqwest::Method::POST, self.api_url(&path), &[], Some(body)).await?;
        decode_pull_request(&created).map(|_| ()).ok_or_else(|| BitbucketError::decode("createPullRequest"))
    }

    pub async fn get_default_branch(&self, cwd: &str, context: Option<&ProviderContext>) -> BitbucketResult<Option<String>> {
        let locator = self.resolve_repository(cwd, context, None).await?;
        let model_path = format!("{}/branching-model", Self::repository_path(&locator));
        let (repository, model) = futures_util::future::join(
            self.get_repository(&locator),
            self.execute_json("getBranchingModel", reqwest::Method::GET, self.api_url(&model_path), &[], None),
        )
        .await;
        Ok(default_target_branch(&repository?, model.ok().as_ref()))
    }

    /// Bitbucket has no checkout CLI, so the pull request's branch is fetched from the remote
    /// that holds it with plain git and checked out, tracking it.
    pub async fn checkout_pull_request(
        &self,
        cwd: &str,
        context: Option<&ProviderContext>,
        reference: &str,
        force: bool,
    ) -> BitbucketResult<()> {
        let failed = |detail: String| {
            BitbucketError::new(
                "BitbucketCheckoutError",
                "Bitbucket API failed in checkoutPullRequest: Failed to check out the Bitbucket pull request.".to_string(),
            )
            .tap(|error| tracing::debug!(detail = %detail, tag = error.tag, "bitbucket checkout failed"))
        };
        let destination = self.resolve_repository(cwd, context, None).await?;
        let pull_request = self.get_raw_pull_request(&destination, reference).await?;
        let side_name = |side: &Value| trim_optional(side.get("repository").and_then(|r| r.get("full_name")).and_then(Value::as_str));
        let destination_name = side_name(&pull_request["destination"])
            .unwrap_or_else(|| format!("{}/{}", destination.workspace, destination.repo_slug));
        let source_name = side_name(&pull_request["source"]).unwrap_or_else(|| destination_name.clone());
        let is_cross = source_name != destination_name;
        let remote_branch = trimmed_non_empty(pull_request["source"].get("branch").and_then(|b| b.get("name")))
            .map_err(|()| BitbucketError::decode("getPullRequest"))?;
        let id = pull_request["id"].as_u64().unwrap_or_default();
        let core = self.host.shared.git.core();
        let path = Path::new(cwd);
        let git = |error: t3_vcs::GitCommandError| failed(error.to_string());

        let remote_name = if context.is_some_and(|c| c.provider.kind == KIND && parse_remote_url(&c.remote_url).is_some()) && !is_cross {
            context.map(|c| c.remote_name.clone()).unwrap_or_default()
        } else if let Some(primary) = (!is_cross).then_some(()).and(core.resolve_primary_remote_name(path).await.ok()) {
            primary
        } else {
            let source_locator = parse_repository_slug(&source_name).ok_or_else(BitbucketError::locator)?;
            let urls = clone_urls(&self.get_repository(&source_locator).await?).map_err(|()| BitbucketError::decode("getRepository"))?;
            let origin = core.read_config_value(path, "remote.origin.url").await.ok().flatten();
            let url = if origin.as_deref().is_some_and(is_ssh_remote_url) { urls.ssh_url } else { urls.url };
            let preferred = if is_cross {
                source_name.split('/').next().map(str::trim).filter(|owner| !owner.is_empty()).unwrap_or("bitbucket").to_string()
            } else {
                destination.workspace.clone()
            };
            core.ensure_remote(path, &preferred, &url).await.map_err(git)?
        };
        let local_branch = checkout_branch_name(id, &remote_branch, is_cross);
        let exists = core.list_local_branch_names(path).await.map_err(git)?.contains(&local_branch);
        if force || !exists {
            core.fetch_remote_branch(path, &remote_name, &remote_branch, &local_branch).await.map_err(git)?;
        } else {
            core.fetch_remote_tracking_branch(path, &remote_name, &remote_branch).await.map_err(git)?;
        }
        core.set_branch_upstream(path, &local_branch, &remote_name, &remote_branch).await.map_err(git)?;
        core.switch_ref(path, &local_branch).await.map_err(git)?;
        Ok(())
    }
}

trait Tap: Sized {
    fn tap(self, observe: impl FnOnce(&Self)) -> Self {
        observe(&self);
        self
    }
}
impl Tap for BitbucketError {}

// ---- discovery and the provider ---------------------------------------------------------------

pub struct BitbucketProbe {
    host: Host,
}

impl BitbucketProbe {
    pub fn new(host: Host) -> Self {
        Self { host }
    }
}

impl ApiProbe for BitbucketProbe {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }
    fn label(&self) -> &'static str {
        "Bitbucket"
    }
    fn install_hint(&self) -> &'static str {
        "Add a Bitbucket token in Settings → Source Control."
    }
    fn probe_auth(&self) -> BoxFuture<'_, SourceControlProviderAuth> {
        Box::pin(async move { BitbucketApi::new(self.host.clone()).probe_auth().await })
    }
}

fn failure(operation: &str, cwd: &str, detail: &str, error: &BitbucketError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, detail).with_cause_chain(Some(error.cause()))
}

pub struct BitbucketProvider {
    api: BitbucketApi,
}

impl BitbucketProvider {
    pub fn new(host: Host) -> Self {
        Self { api: BitbucketApi::new(host) }
    }
}

impl SourceControlProvider for BitbucketProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn list_change_requests(&self, input: ListChangeRequestsInput) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>> {
        Box::pin(async move {
            let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
            self.api
                .list_pull_requests(&input.cwd, input.context.as_ref(), &input.head_selector, source.as_ref(), input.state, input.limit)
                .await
                .map_err(|error| {
                    failure("listChangeRequests", &input.cwd, "Failed to list change requests.", &error)
                        .with_reference(&transport_safe_value(&input.head_selector))
                })
        })
    }

    fn get_change_request(&self, input: GetChangeRequestInput) -> BoxFuture<'_, ProviderResult<ChangeRequest>> {
        Box::pin(async move {
            self.api.get_pull_request(&input.cwd, input.context.as_ref(), &input.reference).await.map_err(|error| {
                failure("getChangeRequest", &input.cwd, "Failed to get change request.", &error)
                    .with_reference(&transport_safe_value(&input.reference))
            })
        })
    }

    fn create_change_request(&self, input: CreateChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
            self.api
                .create_pull_request(
                    &input.cwd,
                    input.context.as_ref(),
                    &input.base_ref_name,
                    &input.head_selector,
                    source.as_ref(),
                    input.target.as_ref(),
                    &input.title,
                    &input.body_file,
                )
                .await
                .map_err(|error| {
                    failure("createChangeRequest", &input.cwd, "Failed to create change request.", &error)
                        .with_reference(&transport_safe_value(&input.head_selector))
                })
        })
    }

    fn get_repository_clone_urls(&self, input: RepositoryUrlsInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.api.get_repository_clone_urls(&input.cwd, input.context.as_ref(), &input.repository).await.map_err(|error| {
                failure("getRepositoryCloneUrls", &input.cwd, "Failed to get repository clone URLs.", &error)
                    .with_repository(&transport_safe_value(&input.repository))
            })
        })
    }

    fn create_repository(&self, input: CreateRepositoryInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.api.create_repository(&input.repository, input.visibility).await.map_err(|error| {
                failure("createRepository", &input.cwd, "Failed to create repository.", &error)
                    .with_repository(&transport_safe_value(&input.repository))
            })
        })
    }

    fn get_default_branch(&self, input: DefaultBranchInput) -> BoxFuture<'_, ProviderResult<Option<String>>> {
        Box::pin(async move {
            self.api
                .get_default_branch(&input.cwd, input.context.as_ref())
                .await
                .map_err(|error| failure("getDefaultBranch", &input.cwd, "Failed to get default branch.", &error))
        })
    }

    fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            self.api.checkout_pull_request(&input.cwd, input.context.as_ref(), &input.reference, input.force).await.map_err(|error| {
                failure("checkoutChangeRequest", &input.cwd, "Failed to check out change request.", &error)
                    .with_reference(&transport_safe_value(&input.reference))
            })
        })
    }
}
