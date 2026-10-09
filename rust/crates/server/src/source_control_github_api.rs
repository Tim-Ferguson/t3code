//! GitHub's REST and GraphQL APIs over HTTP (port of `GitHubApi.ts`). Requests carry the
//! token `source_control_github_credentials` found for the host and go through the host's
//! rate-limit pause and, for GraphQL reads, the local quota ledger. Every answer is decided
//! once, from status, headers and body, into success or the error that says why it failed.
//!
//! The original reads three Effect context references: an interactive flag that lets a
//! request spend the GraphQL reserve and pass a pause, a credential already verified for one
//! host, and a rate-limit scope. They are task-locals here; wrap a call in `with_allow_reserve`
//! or `with_pinned_credential` to set them.
use crate::{
    source_control_github_credentials::{self as credentials, GitHubCredentialError, Secret},
    source_control_host::Host,
    source_control_rate_limit::{RateLimitPaused, retry_at_from_header},
};
use serde_json::{Value, json};
use std::{collections::HashMap, future::Future, time::Duration};
use t3_contracts::SourceControlProviderKind;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const API_VERSION: &str = "2022-11-28";

/// A credential already verified for one host. Every request made under it must target that
/// host, so a page cannot read one account's data with another's token mid-flight.
#[derive(Debug, Clone)]
pub struct PinnedGitHubCredential {
    pub host: String,
    pub token: Secret,
    pub credential_fingerprint: String,
}

tokio::task_local! {
    static ALLOW_RESERVE: bool;
    static PINNED_CREDENTIAL: PinnedGitHubCredential;
    static CREDENTIAL_SCOPE: String;
}

/// Runs `future` as an interactive caller (a user's read or write, not a background sweep):
/// its requests may spend the GraphQL reserve and go through a rate-limit pause.
pub async fn with_allow_reserve<F: Future>(future: F) -> F::Output {
    ALLOW_RESERVE.scope(true, future).await
}

pub async fn with_pinned_credential<F: Future>(pinned: PinnedGitHubCredential, future: F) -> F::Output {
    PINNED_CREDENTIAL.scope(pinned, future).await
}

pub async fn with_credential_scope<F: Future>(scope: String, future: F) -> F::Output {
    CREDENTIAL_SCOPE.scope(scope, future).await
}

/// Whether the current task runs as an interactive caller (see `with_allow_reserve`).
pub fn allows_reserve() -> bool {
    ALLOW_RESERVE.try_with(|allowed| *allowed).unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubApiError {
    Credential(GitHubCredentialError),
    Request { host: String, operation: String, cause: String },
    Authentication { host: String, operation: String },
    RateLimit { host: String, operation: String, retry_at: Option<i64> },
    NotFound { host: String, operation: String },
    /// `github_errors` carries GitHub's own messages, which name the field and the reason
    /// ("A pull request already exists for acme:feature"), never a token.
    Response { host: String, operation: String, status: u16, github_errors: Option<Vec<String>> },
    Paused(RateLimitPaused),
}

impl GitHubApiError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Credential(error) => error.tag(),
            Self::Request { .. } => "GitHubApiRequestError",
            Self::Authentication { .. } => "GitHubApiAuthenticationError",
            Self::RateLimit { .. } => "GitHubApiRateLimitError",
            Self::NotFound { .. } => "GitHubApiNotFoundError",
            Self::Response { .. } => "GitHubApiResponseError",
            Self::Paused(_) => "SourceControlRateLimitPausedError",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Credential(error) => error.message(),
            Self::Request { host, .. } => format!("Could not reach GitHub at {host}."),
            Self::Authentication { host, .. } => format!(
                "GitHub refused the credential for {host}. Run `gh auth login --hostname {host}` and retry."
            ),
            Self::RateLimit { .. } => "GitHub API rate limit exceeded.".into(),
            Self::NotFound { .. } => {
                "GitHub could not find the requested resource, or the credential cannot see it.".into()
            }
            Self::Response { github_errors: Some(errors), .. } if !errors.is_empty() => {
                format!("GitHub returned an error: {}", errors.join("; "))
            }
            Self::Response { status, .. } => format!("GitHub returned HTTP {status}."),
            Self::Paused(paused) => paused.message(),
        }
    }
}

impl std::fmt::Display for GitHubApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Method {
    #[default]
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, Default)]
pub struct RestInput {
    pub host: String,
    pub operation: String,
    pub method: Method,
    /// Relative to the API root, e.g. `repos/acme/web/pulls/7`; query string included.
    pub path: String,
    pub body: Option<Value>,
    /// Defaults to `application/vnd.github+json`.
    pub accept: Option<String>,
    /// Revalidates a cached answer. A 304 is returned rather than failed.
    pub if_none_match: Option<String>,
    pub max_response_bytes: Option<usize>,
    pub timeout: Option<Duration>,
    /// Overrides the interactive flag for this one request.
    pub allow_reserve: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct GraphQlInput {
    pub host: String,
    pub operation: String,
    pub query: String,
    pub variables: Option<Value>,
    pub allow_reserve: Option<bool>,
    pub max_response_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RestResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub truncated: bool,
    /// The body was not valid UTF-8, which a raw file read takes to mean binary.
    pub invalid_utf8: bool,
}

fn normalize_host(host: &str) -> String {
    host.trim().to_lowercase()
}

/// Where the API for a host lives. github.com and GHE.com data residency serve it from an
/// `api.` subdomain; GitHub Enterprise Server serves it under `/api` on the instance itself.
pub fn github_api_urls(host: &str) -> (String, String) {
    let normalized = normalize_host(host);
    if normalized == "github.com" {
        return ("https://api.github.com".into(), "https://api.github.com/graphql".into());
    }
    if normalized.ends_with(".ghe.com") {
        return (
            format!("https://api.{normalized}"),
            format!("https://api.{normalized}/graphql"),
        );
    }
    (
        format!("https://{normalized}/api/v3"),
        format!("https://{normalized}/api/graphql"),
    )
}

fn urls_for(host: &Host, name: &str) -> (String, String) {
    match &host.shared.github_api {
        Some(resolve) => resolve(name),
        None => github_api_urls(name),
    }
}

/// What one GitHub answer means.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Ok,
    RateLimited,
    Unauthorized,
    NotFound,
    Failed(Option<Vec<String>>),
}

type GraphQlErrors = Vec<(Option<String>, Option<String>)>;

/// `{ errors: [{ type?, message? }, ...] }` with at least one error, or `None`.
fn parse_graphql_errors(body: &str) -> Option<GraphQlErrors> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let errors = parsed.get("errors")?.as_array().filter(|errors| !errors.is_empty())?;
    errors
        .iter()
        .map(|error| {
            let field = |name: &str| match error.get(name) {
                None => Some(None),
                Some(Value::String(text)) => Some(Some(text.clone())),
                Some(_) => None,
            };
            error.is_object().then_some(())?;
            Some((field("type")?, field("message")?))
        })
        .collect()
}

/// A REST failure body: `{ message, errors: [string | { message } | { field, code }] }`.
fn rest_error_messages(body: &str) -> Option<Vec<String>> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let object = parsed.as_object()?;
    let message = match object.get("message") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return None,
    };
    let mut details = Vec::new();
    match object.get("errors") {
        None => {}
        Some(Value::Array(errors)) => {
            for error in errors {
                match error {
                    Value::String(text) => details.push(text.clone()),
                    Value::Object(fields) => {
                        let text = |name: &str| match fields.get(name) {
                            None => Some(None),
                            Some(Value::String(text)) => Some(Some(text.clone())),
                            Some(_) => None,
                        };
                        let (message, field, code) = (text("message")?, text("field")?, text("code")?);
                        match (message, field, code) {
                            (Some(message), _, _) => details.push(message),
                            (None, Some(field), Some(code)) => details.push(format!("{field} {code}")),
                            _ => {}
                        }
                    }
                    _ => return None,
                }
            }
        }
        Some(_) => return None,
    }
    let messages: Vec<String> = message.into_iter().chain(details).collect();
    (!messages.is_empty()).then_some(messages)
}

fn mentions_rate_limit(text: &str) -> bool {
    text.to_lowercase().contains("rate limit")
}

fn untyped_quota_message(message: &str) -> bool {
    // /rate limit (already )?exceeded/i
    let lower = message.to_lowercase();
    lower.contains("rate limit exceeded") || lower.contains("rate limit already exceeded")
}

/// GitHub reports a failed GraphQL document with HTTP 200 and an `errors` list, so a GraphQL
/// body is read for its error types as well as its status.
fn classify(
    status: u16,
    headers: &HashMap<String, String>,
    body: &str,
    graphql: bool,
    accept_not_modified: bool,
) -> Answer {
    let errors = if graphql { parse_graphql_errors(body) } else { None };
    let types: Vec<&str> = errors
        .iter()
        .flatten()
        .filter_map(|(kind, _)| kind.as_deref())
        .collect();
    let messages: Option<Vec<&str>> = errors
        .as_ref()
        .map(|errors| errors.iter().filter_map(|(_, message)| message.as_deref()).collect());
    let remaining_is_zero = headers.get("x-ratelimit-remaining").map(String::as_str) == Some("0");
    if status == 429
        || types.contains(&"RATE_LIMITED")
        // An exhausted GraphQL quota can answer HTTP 200 with an untyped "API rate limit
        // already exceeded" error; the headers say the same thing.
        || (graphql && errors.is_some() && remaining_is_zero)
        || (graphql
            && messages
                .as_ref()
                .is_some_and(|messages| messages.iter().any(|message| untyped_quota_message(message))))
        || (status == 403
            && (remaining_is_zero
                || headers.contains_key("retry-after")
                || mentions_rate_limit(body)))
    {
        return Answer::RateLimited;
    }
    if status == 401 {
        return Answer::Unauthorized;
    }
    if status == 404
        || errors
            .as_ref()
            .is_some_and(|errors| errors.iter().all(|(kind, _)| kind.as_deref() == Some("NOT_FOUND")))
    {
        return Answer::NotFound;
    }
    if errors.is_some() {
        return Answer::Failed(messages.map(|messages| messages.into_iter().map(String::from).collect()));
    }
    if (200..300).contains(&status) || (status == 304 && accept_not_modified) {
        return Answer::Ok;
    }
    Answer::Failed(if graphql { None } else { rest_error_messages(body) })
}

/// The credential a request to `raw_host` would carry right now, as `(token, fingerprint)`.
pub async fn credential(host: &Host, raw_host: &str) -> Result<(Secret, String), GitHubApiError> {
    let name = normalize_host(raw_host);
    if let Ok(pinned) = PINNED_CREDENTIAL.try_with(Clone::clone) {
        // A pinned page only ever talks to the host it verified.
        if pinned.host != name {
            return Err(GitHubApiError::Authentication { host: name, operation: "credential".into() });
        }
        return Ok((pinned.token, pinned.credential_fingerprint));
    }
    let held = credentials::get(host, &name).await.map_err(GitHubApiError::Credential)?;
    Ok((held.token, held.fingerprint))
}

/// The credential this task was pinned to, if any, for work that runs under it on another task.
pub fn current_pinned() -> Option<PinnedGitHubCredential> {
    PINNED_CREDENTIAL.try_with(Clone::clone).ok()
}

/// The rate-limit scope this task runs under, for work that runs under it on another task.
pub fn current_credential_scope() -> String {
    current_scope()
}

fn current_scope() -> String {
    CREDENTIAL_SCOPE.try_with(Clone::clone).unwrap_or_default()
}

struct Outgoing<'a> {
    host: &'a str,
    operation: &'a str,
    method: Method,
    url: String,
    accept: String,
    if_none_match: Option<String>,
    body: Option<Value>,
    max_response_bytes: usize,
    timeout: Duration,
    allow_reserve: bool,
    accept_not_modified: bool,
    graphql: bool,
}

async fn read_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<(String, bool, bool), reqwest::Error> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await? {
        if truncated {
            continue;
        }
        let remaining = max_bytes.saturating_sub(bytes.len());
        if remaining == 0 {
            truncated = true;
            continue;
        }
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        truncated = chunk.len() > remaining;
    }
    let invalid = std::str::from_utf8(&bytes).is_err();
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated, invalid))
}

/// Sends one request under the host's rate-limit pause and fails a refusal with the error that
/// says why. `scope` is the rate-limit scope, already resolved to the credential.
async fn send(host: &Host, request: Outgoing<'_>, token: &Secret, scope: &str) -> Result<RestResponse, GitHubApiError> {
    let name = normalize_host(request.host);
    let state = &host.shared.github;
    let provider = SourceControlProviderKind::Github;
    let _permit = state.gate.acquire().await.expect("semaphore is never closed");
    let now = host.now_ms();
    let lease = state
        .limits
        .check(provider, &name, scope, now, request.allow_reserve)
        .map_err(GitHubApiError::Paused)?;
    let transport = |cause: String| GitHubApiError::Request {
        host: name.clone(),
        operation: request.operation.into(),
        cause,
    };
    let client = &host.shared.http;
    let builder = match request.method {
        Method::Get => client.get(&request.url),
        Method::Post => client.post(&request.url),
        Method::Put => client.put(&request.url),
        Method::Patch => client.patch(&request.url),
        Method::Delete => client.delete(&request.url),
    }
    .bearer_auth(token.expose())
    .header("accept", &request.accept)
    .header("x-github-api-version", API_VERSION)
    .header("user-agent", "t3code");
    let builder = match &request.if_none_match {
        Some(etag) => builder.header("if-none-match", etag),
        None => builder,
    };
    let builder = match &request.body {
        Some(body) => builder.json(body),
        None => builder,
    };
    // One deadline covers the headers and the body: a host that answers headers and then
    // stalls must not hold a slot of the shared gate.
    let exchange = async {
        let response = builder.send().await.map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        let headers: HashMap<String, String> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str().to_lowercase(), value.to_str().ok()?.to_string())))
            .collect();
        let (body, truncated, invalid_utf8) =
            read_body(response, request.max_response_bytes).await.map_err(|error| error.to_string())?;
        Ok::<_, String>(RestResponse { status, headers, body, truncated, invalid_utf8 })
    };
    let answered = match tokio::time::timeout(request.timeout, exchange).await {
        Ok(Ok(answered)) => answered,
        Ok(Err(cause)) => return Err(transport(cause)),
        Err(_) => return Err(transport("The request timed out.".into())),
    };
    let context = (name.clone(), request.operation.to_string());
    match classify(
        answered.status,
        &answered.headers,
        &answered.body,
        request.graphql,
        request.accept_not_modified,
    ) {
        Answer::Ok => {
            state.limits.record_success(provider, &name, scope, lease, host.now_ms());
            Ok(answered)
        }
        Answer::RateLimited => {
            let now = host.now_ms();
            let retry_at = retry_at_from_header(answered.headers.get("retry-after").map(String::as_str), now)
                .or_else(|| {
                    let reset = answered
                        .headers
                        .get("x-ratelimit-reset")?
                        .trim()
                        .parse::<f64>()
                        .ok()?
                        * 1000.0;
                    (reset.is_finite() && reset > now as f64).then_some(reset as i64)
                });
            state.limits.record_rate_limit(provider, &name, scope, lease, retry_at, now);
            Err(GitHubApiError::RateLimit { host: context.0, operation: context.1, retry_at })
        }
        // The source may hold a newer token than the one that was refused.
        Answer::Unauthorized => {
            credentials::invalidate(host, &name).await;
            Err(GitHubApiError::Authentication { host: context.0, operation: context.1 })
        }
        Answer::NotFound => Err(GitHubApiError::NotFound { host: context.0, operation: context.1 }),
        Answer::Failed(messages) => Err(GitHubApiError::Response {
            host: context.0,
            operation: context.1,
            status: answered.status,
            github_errors: messages,
        }),
    }
}

pub async fn rest(host: &Host, input: RestInput) -> Result<RestResponse, GitHubApiError> {
    let name = normalize_host(&input.host);
    let (rest_base, _) = urls_for(host, &name);
    let url = format!("{rest_base}/{}", input.path.trim_start_matches('/'));
    let (token, fingerprint) = credential(host, &name).await?;
    let scope = Some(current_scope()).filter(|scope| !scope.is_empty()).unwrap_or(fingerprint);
    let accept_not_modified = input.if_none_match.is_some();
    send(
        host,
        Outgoing {
            host: &name,
            operation: &input.operation,
            method: input.method,
            url,
            accept: input.accept.unwrap_or_else(|| "application/vnd.github+json".into()),
            if_none_match: input.if_none_match,
            body: input.body,
            max_response_bytes: input.max_response_bytes.unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
            timeout: input.timeout.unwrap_or(DEFAULT_TIMEOUT),
            allow_reserve: input.allow_reserve.unwrap_or_else(allows_reserve),
            accept_not_modified,
            graphql: false,
        },
        &token,
        &scope,
    )
    .await
}

/// The raw JSON body of a successful GraphQL answer, with `rateLimit` recorded.
pub async fn graphql(host: &Host, input: GraphQlInput) -> Result<String, GitHubApiError> {
    let name = normalize_host(&input.host);
    let (_, graphql_url) = urls_for(host, &name);
    let (token, fingerprint) = credential(host, &name).await?;
    let scope = Some(current_scope()).filter(|scope| !scope.is_empty()).unwrap_or(fingerprint);
    let allow_reserve = input.allow_reserve.unwrap_or_else(allows_reserve);
    let query = host
        .shared
        .github
        .budget
        .query(&name, &input.query, &scope, host.now_ms(), allow_reserve)
        .map_err(GitHubApiError::Paused)?;
    let response = send(
        host,
        Outgoing {
            host: &name,
            operation: &input.operation,
            method: Method::Post,
            url: graphql_url,
            accept: "application/json".into(),
            if_none_match: None,
            body: Some(json!({"query":query,"variables":input.variables.unwrap_or_else(|| json!({}))})),
            max_response_bytes: input.max_response_bytes.unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
            timeout: DEFAULT_TIMEOUT,
            allow_reserve,
            accept_not_modified: false,
            graphql: true,
        },
        &token,
        &scope,
    )
    .await?;
    // A body cut at the byte cap hides any `errors` past the cut and cannot be decoded.
    if response.truncated {
        return Err(GitHubApiError::Response {
            host: name,
            operation: input.operation,
            status: response.status,
            github_errors: None,
        });
    }
    host.shared.github.budget.observe(&name, &response.body, &scope);
    Ok(response.body)
}
