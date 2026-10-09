//! Reading `gh auth status --json hosts` (port of `gitHubAuthStatus.ts` and `parseGitHubAuth`
//! of `GitHubSourceControlProvider.ts`). The headline account is the one GitHub requests will
//! use: Settings can pin a login per host or turn a host off, and an environment token beats
//! both.
use crate::source_control_discovery::{
    AuthProbeInput, combined_auth_output, first_safe_auth_line, provider_auth,
};
use serde_json::Value;
use std::sync::OnceLock;
use regex::Regex;
use t3_contracts::{
    GitHubHost, GitHubSettings, SourceControlProviderAccount, SourceControlProviderAuth,
    SourceControlProviderAuthStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubAuthStatusAccount {
    pub host: String,
    pub account: String,
    pub authenticated: bool,
    pub active: bool,
    pub error: Option<String>,
    /// The variable (GH_TOKEN and kin) the login came from; it overrides every stored login.
    pub environment_variable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitHubAuthStatus {
    pub parsed: bool,
    pub accounts: Vec<GitHubAuthStatusAccount>,
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = t3_contracts::trim_wire_string(value);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// One entry of `hosts`: `state`, `active`, `host` and `login` are required; `error` and
/// `tokenSource` may be absent but not null.
fn decode_account(entry: &Value) -> Option<(String, bool, String, String, Option<String>, Option<String>)> {
    let object = entry.as_object()?;
    let text = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_string);
    let optional = |name: &str| match object.get(name) {
        None => Some(None),
        Some(Value::String(text)) => Some(Some(text.clone())),
        Some(_) => None,
    };
    Some((
        text("state")?,
        object.get("active")?.as_bool()?,
        text("host")?,
        text("login")?,
        optional("error")?,
        optional("tokenSource")?,
    ))
}

pub fn parse_github_auth_status(text: &str) -> GitHubAuthStatus {
    static SOURCE: OnceLock<Regex> = OnceLock::new();
    let source = SOURCE.get_or_init(|| Regex::new(r"^(?:GH|GITHUB)_(?:ENTERPRISE_)?TOKEN$").unwrap());
    let unparsed = GitHubAuthStatus { parsed: false, accounts: Vec::new() };
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return unparsed;
    };
    let Some(hosts) = value.get("hosts").and_then(Value::as_object) else {
        return unparsed;
    };
    let mut decoded = Vec::new();
    for accounts in hosts.values() {
        let Some(accounts) = accounts.as_array() else {
            return unparsed;
        };
        for entry in accounts {
            let Some(row) = decode_account(entry) else {
                return unparsed;
            };
            decoded.push(row);
        }
    }
    let accounts = decoded
        .into_iter()
        .filter_map(|(state, active, host, login, error, token_source)| {
            let host = non_empty(&host)?;
            let account = non_empty(&login)?;
            Some(GitHubAuthStatusAccount {
                host: host.to_lowercase(),
                account,
                authenticated: state == "success",
                active,
                error: error.as_deref().and_then(non_empty),
                environment_variable: token_source.filter(|source_name| source.is_match(source_name)),
            })
        })
        .collect();
    GitHubAuthStatus { parsed: true, accounts }
}

pub fn find_authenticated_account(accounts: &[GitHubAuthStatusAccount]) -> Option<&GitHubAuthStatusAccount> {
    accounts
        .iter()
        .find(|account| account.authenticated && account.active)
        .or_else(|| accounts.iter().find(|account| account.authenticated))
}

/// The login GitHub requests for a host will use, honouring Settings: an environment token
/// first (gh's own precedence), then the pinned account, then gh's active login. `None` when
/// the host is turned off or has no usable login.
pub fn effective_account<'a>(
    host: &str,
    accounts: &'a [GitHubAuthStatusAccount],
    settings: &GitHubSettings,
) -> Option<&'a GitHubAuthStatusAccount> {
    let choice = settings.hosts.get(&GitHubHost(host.to_string()));
    if choice.is_some_and(|choice| !choice.enabled) {
        return None;
    }
    let usable: Vec<&GitHubAuthStatusAccount> = accounts
        .iter()
        .filter(|account| account.host == host && account.authenticated)
        .collect();
    usable
        .iter()
        .find(|account| account.environment_variable.is_some())
        .copied()
        .or_else(|| {
            let pinned = choice?.account.as_ref()?.as_str();
            usable.iter().find(|account| account.account == pinned).copied()
        })
        .or_else(|| {
            usable
                .iter()
                .find(|account| account.active)
                .or_else(|| usable.first())
                .copied()
        })
}

fn wire_accounts(accounts: &[GitHubAuthStatusAccount]) -> Vec<SourceControlProviderAccount> {
    accounts
        .iter()
        .map(|entry| SourceControlProviderAccount {
            host: entry.host.clone(),
            account: entry.account.clone(),
            active: entry.active,
            authenticated: entry.authenticated,
            error: entry.error.clone(),
            environment_variable: entry.environment_variable.clone(),
        })
        .collect()
}

/// Reads `gh auth status --json hosts`.
pub fn parse_github_auth(input: &AuthProbeInput, settings: &GitHubSettings) -> SourceControlProviderAuth {
    use SourceControlProviderAuthStatus::*;
    let output = combined_auth_output(input);
    let status = parse_github_auth_status(&input.stdout);
    let mut hosts: Vec<&str> = Vec::new();
    for account in &status.accounts {
        if !hosts.contains(&account.host.as_str()) {
            hosts.push(&account.host);
        }
    }
    let fallback = find_authenticated_account(&status.accounts);
    // Lead with the host gh would pick, unless Settings turned it off.
    let ordered: Vec<&str> = match fallback {
        Some(fallback) => std::iter::once(fallback.host.as_str())
            .chain(hosts.iter().copied().filter(|host| *host != fallback.host))
            .collect(),
        None => hosts.clone(),
    };
    let chosen = ordered
        .iter()
        .find_map(|host| effective_account(host, &status.accounts, settings));
    let with_accounts = |mut auth: SourceControlProviderAuth| {
        if status.parsed {
            auth.accounts = Some(wire_accounts(&status.accounts));
        }
        auth
    };
    if let Some(chosen) = chosen {
        let detail = chosen.environment_variable.as_ref().map(|variable| {
            format!("Using {variable} from the server environment; it overrides the account chosen in Settings.")
        });
        return with_accounts(provider_auth(Authenticated, Some(&chosen.account), Some(&chosen.host), detail.as_deref()));
    }
    if let Some(fallback) = fallback {
        return with_accounts(provider_auth(
            Unauthenticated,
            None,
            Some(&fallback.host),
            Some("Every GitHub host gh is signed in to is turned off in Settings → Source Control."),
        ));
    }
    let failed = status.accounts.iter().find(|entry| entry.active).or(status.accounts.first());
    if status.parsed {
        return with_accounts(provider_auth(
            Unauthenticated,
            None,
            failed.map(|failed| failed.host.as_str()),
            Some(
                failed
                    .and_then(|failed| failed.error.as_deref())
                    .unwrap_or("Run `gh auth login` to authenticate GitHub CLI with an active account."),
            ),
        ));
    }
    // gh gained `auth status --json` in 2.81.0. Older versions reject the flag and exit
    // non-zero, which reads exactly like a signed-out CLI. Name the real problem instead.
    if input.exit_code != 0 && output.contains("unknown flag: --json") {
        return provider_auth(
            Unknown,
            None,
            None,
            Some("GitHub CLI is too old to report sign-in status. Update `gh` to 2.81.0 or newer (for example `brew upgrade gh`) and rescan."),
        );
    }
    if input.exit_code != 0 {
        let detail = first_safe_auth_line(&output)
            .unwrap_or_else(|| "Run `gh auth login` to authenticate GitHub CLI.".into());
        return provider_auth(Unauthenticated, None, None, Some(&detail));
    }
    let detail = first_safe_auth_line(&output).unwrap_or_else(|| "GitHub CLI auth status could not be parsed.".into());
    provider_auth(Unknown, None, None, Some(&detail))
}
