//! Where GitHub tokens come from (port of `GitHubCredentials.ts`). Callers ask per host and
//! never see how the token was found. In order: a token saved in Settings (read from the
//! secret store through the settings snapshot each time), then `GH_TOKEN` and its kin from the
//! server environment, then `gh auth token`, honouring the account Settings pinned for the
//! host. A host turned off in Settings has no credential at all.
use crate::source_control_host::{Host, HostEnv};
use sha2::{Digest, Sha256};
use std::{fmt, time::Duration};
use t3_contracts::GitHubHost;
use t3_vcs::VcsError;

/// How long a token is reused before `gh` is asked again, so a `gh auth switch` applies soon.
const TOKEN_TTL: Duration = Duration::from_secs(5 * 60);
/// No credential is retried sooner, so a fresh `gh auth login` takes effect on the next read.
const MISSING_TTL: Duration = Duration::from_secs(10);
const GH_TOKEN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubCredentialSource {
    Settings,
    Env,
    Gh,
}

/// A token that never prints itself.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubCredential {
    pub host: String,
    pub token: Secret,
    pub source: GitHubCredentialSource,
    /// A digest of host and token: safe for cache keys and rate-limit scopes, never the token.
    pub fingerprint: String,
}

/// There is no token for the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubCredentialError {
    /// Nothing in the environment, and no `gh` on PATH to ask.
    CliMissing { host: String },
    /// `gh` is installed but holds no login for the host, or not the account chosen in Settings.
    NotSignedIn { host: String, account: Option<String> },
    /// The user turned the host off in Settings.
    HostDisabled { host: String },
    /// `gh auth token` timed out or failed for a reason other than having no login.
    CliFailed { host: String, cause: String },
}

impl GitHubCredentialError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::CliMissing { .. } => "GitHubCliMissingError",
            Self::NotSignedIn { .. } => "GitHubNotSignedInError",
            Self::HostDisabled { .. } => "GitHubHostDisabledError",
            Self::CliFailed { .. } => "GitHubCliFailedError",
        }
    }

    /// True for the errors that say what to do about them already (missing, signed out, off).
    pub fn is_unavailable(&self) -> bool {
        !matches!(self, Self::CliFailed { .. })
    }

    pub fn message(&self) -> String {
        match self {
            Self::CliMissing { host } => format!(
                "No GitHub credential for {host}: set GH_TOKEN, or install the GitHub CLI and run `gh auth login`."
            ),
            Self::NotSignedIn { host, account: None } => {
                format!("No GitHub credential for {host}: run `gh auth login --hostname {host}`.")
            }
            Self::NotSignedIn { host, account: Some(account) } => format!(
                "No GitHub credential for {account} on {host}: run `gh auth login --hostname {host}` for that account or pick another in Settings → Source Control."
            ),
            Self::HostDisabled { host } => {
                format!("GitHub host {host} is turned off in Settings → Source Control.")
            }
            Self::CliFailed { host, .. } => format!(
                "The GitHub CLI could not hand over a credential for {host}. Check `gh auth status` on the server."
            ),
        }
    }
}

impl fmt::Display for GitHubCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

fn is_github_dot_com(host: &str) -> bool {
    host == "github.com" || host.ends_with(".ghe.com")
}

/// The environment token for a host, in gh's precedence order. gh hands `GH_ENTERPRISE_TOKEN`
/// to any non-github.com host; here it only goes to the host `GH_HOST` names, because a remote
/// URL picks the host and a hostile one must not receive an enterprise token.
pub fn environment_token(host: &str, env: &HostEnv) -> Option<String> {
    let names: &[&str] = if is_github_dot_com(host) {
        &["GH_TOKEN", "GITHUB_TOKEN"]
    } else if env.get("GH_HOST").map(|value| value.trim().to_lowercase()).as_deref() == Some(host) {
        &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    } else {
        &[]
    };
    names.iter().find_map(|name| env.get_trimmed(name)).map(str::to_string)
}

fn normalize_host(host: &str) -> String {
    host.trim().to_lowercase()
}

fn fingerprint(host: &str, token: &str) -> String {
    format!("{host}:{}", hex(&Sha256::digest(token.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn cache_key(host: &str, account: Option<&str>) -> String {
    match account {
        None => host.to_string(),
        Some(account) => format!("{host}\u{0}{account}"),
    }
}

/// What Settings say about a host: its pinned account and whether it is off.
async fn host_choice(host: &Host, name: &str) -> (Option<String>, bool) {
    let Some(settings) = host.settings().await else {
        return (None, true);
    };
    match settings.github.hosts.get(&GitHubHost(name.to_string())) {
        Some(choice) => (
            choice.account.as_ref().map(|account| account.as_str().to_string()),
            choice.enabled,
        ),
        None => (None, true),
    }
}

async fn saved_token(host: &Host, name: &str) -> Option<String> {
    let settings = host.settings().await?;
    let token = settings.github.tokens.get(&GitHubHost(name.to_string()))?;
    let trimmed = token.as_str().trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

async fn from_gh(host: &Host, name: &str, account: Option<&str>) -> Result<String, GitHubCredentialError> {
    let mut args = vec!["auth".to_string(), "token".into(), "--hostname".into(), name.into()];
    if let Some(account) = account {
        args.extend(["--user".to_string(), account.into()]);
    }
    let mut input = host.cli("GitHubCredentials.get", "gh", args, host.cwd());
    // Never let gh print the token into a debug log.
    input.env.push(("GH_DEBUG".into(), Some(String::new())));
    input.env.push(("GH_PROMPT_DISABLED".into(), Some("1".into())));
    input.timeout = Some(GH_TOKEN_TIMEOUT);
    let not_signed_in = || GitHubCredentialError::NotSignedIn {
        host: name.to_string(),
        account: account.map(str::to_string),
    };
    match host.process().run(input).await {
        Ok(output) => {
            let token = output.stdout.trim();
            if token.is_empty() {
                Err(not_signed_in())
            } else {
                Ok(token.to_string())
            }
        }
        Err(VcsError::ProcessSpawn { cause, .. }) if cause.kind() == std::io::ErrorKind::NotFound => {
            Err(GitHubCredentialError::CliMissing { host: name.to_string() })
        }
        // gh exits non-zero with "no oauth token" when it has no login for the host.
        Err(VcsError::ProcessExit { .. }) => Err(not_signed_in()),
        Err(other) => Err(GitHubCredentialError::CliFailed {
            host: name.to_string(),
            cause: other.to_string(),
        }),
    }
}

async fn lookup(host: &Host, key: &str) -> Result<GitHubCredential, GitHubCredentialError> {
    let (name, choice) = match key.split_once('\u{0}') {
        Some((name, account)) => (name, Some(account)),
        None => (key, None),
    };
    // An environment token wins over a pinned account, exactly as it does in gh.
    let from_env = environment_token(name, host.env());
    let token = match &from_env {
        Some(token) => token.clone(),
        None => match from_gh(host, name, choice).await {
            // A pinned login gh no longer holds falls back to the active one, which is what
            // discovery reports as the account in use.
            Err(GitHubCredentialError::NotSignedIn { .. }) if choice.is_some() => {
                from_gh(host, name, None).await?
            }
            other => other?,
        },
    };
    Ok(GitHubCredential {
        host: name.to_string(),
        fingerprint: fingerprint(name, &token),
        token: Secret::new(token),
        source: if from_env.is_some() {
            GitHubCredentialSource::Env
        } else {
            GitHubCredentialSource::Gh
        },
    })
}

/// The credential a request to `raw_host` carries now.
pub async fn get(host: &Host, raw_host: &str) -> Result<GitHubCredential, GitHubCredentialError> {
    let name = normalize_host(raw_host);
    let (account, enabled) = host_choice(host, &name).await;
    if !enabled {
        return Err(GitHubCredentialError::HostDisabled { host: name });
    }
    // A token saved in Settings is the most deliberate choice, so it comes before the
    // environment and gh. It is read from the secret store each time, so it needs no cache.
    if let Some(saved) = saved_token(host, &name).await {
        return Ok(GitHubCredential {
            fingerprint: fingerprint(&name, &saved),
            host: name,
            token: Secret::new(saved),
            source: GitHubCredentialSource::Settings,
        });
    }
    let key = cache_key(&name, account.as_deref());
    host.shared
        .github
        .credentials
        .get(key.clone(), lookup(host, &key), |result| match result {
            Ok(_) => TOKEN_TTL,
            // A transient gh failure (a timeout, a locked keyring) is asked again next time.
            Err(GitHubCredentialError::CliFailed { .. }) => Duration::ZERO,
            Err(_) => MISSING_TTL,
        })
        .await
}

/// Drops the held token after GitHub refused it, so the next read asks its source again.
pub async fn invalidate(host: &Host, raw_host: &str) {
    let name = normalize_host(raw_host);
    let (account, _) = host_choice(host, &name).await;
    host.shared.github.credentials.invalidate(&cache_key(&name, account.as_deref()));
}
