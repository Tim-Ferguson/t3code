//! GitHub as a `SourceControlProvider`, and GitHub's discovery probe (port of
//! `GitHubSourceControlProvider.ts`). GitHub is usable with a token saved in Settings, one from
//! the environment, or `gh` to hand one over; discovery reads the settings on every probe so a
//! saved account choice shows on rescan, and checks an environment or saved token against the
//! API because `gh auth status` may not know it.
use crate::{
    source_control_detect::transport_safe_value,
    source_control_discovery::{
        CliSpec, ManagedProbe, probe_cli, provider_auth,
    },
    source_control_github_api::{self as api, GitHubApiError, RestInput},
    source_control_github_auth::parse_github_auth,
    source_control_github_cli::{GitHubCli, GitHubCliError, PullRequestListState},
    source_control_host::Host,
    source_control_provider::*,
};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::Value;
use std::{sync::{Arc, OnceLock}, time::Duration};
use t3_contracts::{
    ChangeRequest, ErrorCause, GitHubHost, GitHubSettings, SourceControlDiscoveryStatus, SourceControlProviderAuthStatus,
    SourceControlProviderDiscoveryItem, SourceControlProviderError, SourceControlProviderKind,
    SourceControlRepositoryCloneUrls,
};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Github;
const INSTALL_HINT: &str = "Install the GitHub command-line tool (`gh`) via https://cli.github.com/ or your package manager (for example `brew install gh`).";

/// `gh` as a discovery CLI: its version and `auth status --json hosts`, read with the current
/// Settings.
pub fn cli_spec(settings: GitHubSettings) -> CliSpec {
    CliSpec {
        kind: KIND,
        label: "GitHub",
        install_hint: INSTALL_HINT,
        executable: "gh",
        version_args: vec!["--version"],
        auth_args: vec!["auth", "status", "--json", "hosts"],
        remote_refinement_args: None,
        probe_timeout: None,
        parse_auth: Arc::new(move |input| parse_github_auth(input, &settings)),
        refine_unknown_remote: None,
    }
}

fn cli_failure(operation: &str, cwd: &str, error: &GitHubCliError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, error.message.clone())
        .with_command(Some("gh"))
        .with_cause_chain(Some(
            ErrorCause::new(error.tag, error.message.clone()).with_nested(error.cause.clone()),
        ))
}

fn api_host(context: Option<&ProviderContext>) -> Option<String> {
    let url = url::Url::parse(&context?.provider.base_url).ok()?;
    Some(crate::source_control_detect::js_host(&url))
}

pub struct GitHubProvider {
    host: Host,
    cli: GitHubCli,
}

impl GitHubProvider {
    pub fn new(host: Host) -> Self {
        Self { cli: GitHubCli::new(host.clone()), host }
    }
}

impl SourceControlProvider for GitHubProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn resolve_link(
        &self,
        cwd: &str,
        url: &url::Url,
    ) -> Option<BoxFuture<'static, ProviderResult<LinkSubject>>> {
        static PATH: OnceLock<Regex> = OnceLock::new();
        // Automatic enrichment must not send ambient credentials to a host from message text.
        if crate::source_control_detect::js_host(url) != "github.com" {
            return None;
        }
        let path = PATH.get_or_init(|| {
            Regex::new(r"^/([\w.-]+)/([\w.-]+)/(?:pull|issues)/([1-9]\d*)(?:/.*)?$").unwrap()
        });
        let captures = path.captures(url.path())?;
        let endpoint = format!("repos/{}/{}/issues/{}", &captures[1], &captures[2], &captures[3]);
        let (host, cwd, link_host) = (self.host.clone(), cwd.to_string(), crate::source_control_detect::js_host(url));
        Some(Box::pin(async move {
            let unreadable = |operation: &str, cause: Option<&GitHubApiError>| {
                let error = SourceControlProviderError::new(KIND, operation, &cwd, "The linked subject could not be read.");
                match cause {
                    Some(cause) => error.with_cause(cause.tag(), cause.message()),
                    None => error,
                }
            };
            let read = api::rest(
                &host,
                RestInput {
                    host: link_host,
                    operation: "resolveLink".into(),
                    path: endpoint,
                    max_response_bytes: Some(1_000_000),
                    ..RestInput::default()
                },
            );
            let response = match tokio::time::timeout(Duration::from_secs(3), read).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => return Err(unreadable("resolveLink", Some(&error))),
                Err(_) => return Err(unreadable("resolveLink", None)),
            };
            let subject: Value = serde_json::from_str(&response.body).map_err(|_| unreadable("resolveLink.decode", None))?;
            let title = subject.get("title").and_then(Value::as_str).ok_or_else(|| unreadable("resolveLink.decode", None))?;
            let body = match subject.get("body") {
                None | Some(Value::Null) => None,
                Some(Value::String(body)) => Some(body.clone()),
                Some(_) => return Err(unreadable("resolveLink.decode", None)),
            };
            Ok(LinkSubject { title: title.to_string(), body })
        }))
    }

    fn list_change_requests(
        &self,
        input: ListChangeRequestsInput,
    ) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>> {
        Box::pin(async move {
            let rate_limit_host = api_host(input.context.as_ref());
            let result = if input.state == ListState::Open {
                self.cli
                    .list_open_pull_requests(&input.cwd, &input.head_selector, input.limit, rate_limit_host.as_deref())
                    .await
            } else {
                let state = match input.state {
                    ListState::Closed => PullRequestListState::Closed,
                    ListState::Merged => PullRequestListState::Merged,
                    _ => PullRequestListState::All,
                };
                self.cli
                    .list_pull_requests_by_head(
                        &input.cwd,
                        &input.head_selector,
                        state,
                        input.limit.unwrap_or(20),
                        rate_limit_host.as_deref(),
                    )
                    .await
            };
            result.map_err(|error| {
                cli_failure("listChangeRequests", &input.cwd, &error)
                    .with_reference(&transport_safe_value(&input.head_selector))
            })
        })
    }

    fn get_change_request(&self, input: GetChangeRequestInput) -> BoxFuture<'_, ProviderResult<ChangeRequest>> {
        Box::pin(async move {
            let rate_limit_host = api_host(input.context.as_ref());
            self.cli
                .get_pull_request(&input.cwd, &input.reference, rate_limit_host.as_deref())
                .await
                .map_err(|error| {
                    cli_failure("getChangeRequest", &input.cwd, &error)
                        .with_reference(&transport_safe_value(&input.reference))
                })
        })
    }

    fn create_change_request(&self, input: CreateChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            self.cli
                .create_pull_request(&input.cwd, &input.base_ref_name, &input.head_selector, &input.title, &input.body_file)
                .await
                .map_err(|error| {
                    cli_failure("createChangeRequest", &input.cwd, &error)
                        .with_reference(&transport_safe_value(&input.head_selector))
                })
        })
    }

    fn get_repository_clone_urls(
        &self,
        input: RepositoryUrlsInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.cli
                .get_repository_clone_urls(&input.cwd, &input.repository)
                .await
                .map_err(|error| {
                    cli_failure("getRepositoryCloneUrls", &input.cwd, &error)
                        .with_repository(&transport_safe_value(&input.repository))
                })
        })
    }

    fn create_repository(
        &self,
        input: CreateRepositoryInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            self.cli
                .create_repository(&input.cwd, &input.repository, input.visibility)
                .await
                .map_err(|error| {
                    cli_failure("createRepository", &input.cwd, &error)
                        .with_repository(&transport_safe_value(&input.repository))
                })
        })
    }

    fn get_default_branch(&self, input: DefaultBranchInput) -> BoxFuture<'_, ProviderResult<Option<String>>> {
        Box::pin(async move {
            let rate_limit_host = api_host(input.context.as_ref());
            self.cli
                .get_default_branch(&input.cwd, rate_limit_host.as_deref())
                .await
                .map_err(|error| cli_failure("getDefaultBranch", &input.cwd, &error))
        })
    }

    fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            self.cli
                .checkout_pull_request(&input.cwd, &input.reference, input.force)
                .await
                .map_err(|error| {
                    cli_failure("checkoutChangeRequest", &input.cwd, &error)
                        .with_reference(&transport_safe_value(&input.reference))
                })
        })
    }
}

/// GitHub's own probe: `gh`'s view first, then a token that outranks it.
pub struct GitHubProbe {
    host: Host,
}

impl GitHubProbe {
    pub fn new(host: Host) -> Self {
        Self { host }
    }
}

/// The environment variable gh would take a github.com token from, if one is set.
fn environment_token_variable(host: &Host) -> Option<&'static str> {
    ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find(|name| host.env().get_trimmed(name).is_some())
}

impl ManagedProbe for GitHubProbe {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn probe<'a>(&'a self, cwd: &'a str) -> BoxFuture<'a, SourceControlProviderDiscoveryItem> {
        Box::pin(async move {
            let settings = self.host.settings().await.map(|settings| settings.github).unwrap_or_default();
            let mut item = probe_cli(&self.host, &cli_spec(settings.clone()), cwd).await;
            let github_com = GitHubHost("github.com".into());
            // A token saved in Settings wins over the environment, which wins over gh.
            let saved_token = settings.tokens.get(&github_com).is_some_and(|token| !token.as_str().trim().is_empty());
            let variable = environment_token_variable(&self.host);
            let token_source = if saved_token {
                Some("the token saved in Settings".to_string())
            } else {
                variable.map(str::to_string)
            };
            // A host turned off in Settings stays off even with a token.
            let Some(token_source) = token_source else {
                return item;
            };
            if settings.hosts.get(&github_com).is_some_and(|choice| !choice.enabled) {
                return item;
            }
            let viewer = api::rest(
                &self.host,
                RestInput {
                    host: "github.com".into(),
                    operation: "discovery".into(),
                    path: "user".into(),
                    ..RestInput::default()
                },
            )
            .await;
            let login = viewer.as_ref().ok().and_then(|response| {
                serde_json::from_str::<Value>(&response.body)
                    .ok()?
                    .get("login")?
                    .as_str()
                    .map(str::to_string)
            });
            let auth = if let Some(login) = &login {
                let detail = if saved_token {
                    "Using the token saved in Settings; it overrides GH_TOKEN and the gh login.".to_string()
                } else {
                    format!(
                        "Using {} from the server environment; it overrides the account chosen in Settings.",
                        variable.unwrap_or_default()
                    )
                };
                provider_auth(SourceControlProviderAuthStatus::Authenticated, Some(login), Some("github.com"), Some(&detail))
            } else if let Err(failure) = &viewer
                // Only a refusal says the token is bad; a network error or a pause says nothing.
                && !matches!(failure, GitHubApiError::Authentication { .. })
            {
                let subject = if saved_token {
                    token_source.clone()
                } else {
                    format!("the token in {token_source}")
                };
                provider_auth(
                    SourceControlProviderAuthStatus::Unknown,
                    None,
                    Some("github.com"),
                    Some(&format!("Could not check {subject}: {}", failure.message())),
                )
            } else {
                let detail = if saved_token {
                    "GitHub refused the token saved in Settings. Replace or remove it in Settings → Source Control.".to_string()
                } else {
                    format!(
                        "GitHub refused the token in {token_source}. Replace it, or unset it to use `gh auth login`."
                    )
                };
                provider_auth(SourceControlProviderAuthStatus::Unauthenticated, None, Some("github.com"), Some(&detail))
            };
            // The per-host logins stay, so the account picker still lists every host gh knows.
            let accounts = item.auth.accounts.take();
            item.status = SourceControlDiscoveryStatus::Available;
            item.auth = t3_contracts::SourceControlProviderAuth { accounts, ..auth };
            item
        })
    }
}
