//! Forgejo and Gitea through `fj` and `tea` (port of `ForgejoCli.ts`, `forgejoPullRequests.ts` and
//! `ForgejoSourceControlProvider.ts`). One host kind, two tools: `fj` is preferred where it holds a
//! login for the server, and its requests are made over HTTP with the token it stores; `tea` is
//! the fallback, and runs every request itself through `tea api`. The repository, the server and
//! the account come from the checkout's remote and the logins, never from the request.
//!
//! The token `fj` stores is read from its own `keys.json` and goes only to the server it belongs
//! to: a request whose path leaves that server's API is refused, redirects are not followed, and
//! nothing about the token is ever put in an error.
use crate::{
    source_control_decode::*,
    source_control_detect::{source_branch, source_ref_from_input},
    source_control_discovery::{ManagedProbe, first_non_empty_line, provider_auth},
    source_control_host::Host,
    source_control_provider::*,
    source_control_uri::encode_uri_component,
};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::OnceLock,
    time::Duration,
};
use t3_contracts::{
    ChangeRequest, ChangeRequestState, ErrorCause, SourceControlDiscoveryStatus, SourceControlProviderAuthStatus, SourceControlProviderDiscoveryItem,
    SourceControlProviderError, SourceControlProviderInfo, SourceControlProviderKind, SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility,
};
use t3_vcs::{VcsError, process::VcsProcessInput};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Forgejo;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// How long a token `fj` renewed is believed without asking it again.
const AUTH_CACHE_MS: i64 = 30_000;
const LABEL: &str = "Forgejo / Gitea";
pub const INSTALL_HINT: &str = "Install `fj` 0.6 or later from https://codeberg.org/forgejo-contrib/forgejo-cli and run `fj --host <server-url> auth add-token`, or install `tea` 0.16 or later from https://gitea.com/gitea/tea and run `tea login add` for each Forgejo or Gitea server.";

// ---- errors ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    MissingCli,
    Authentication,
    Forbidden,
    NotFound,
    RateLimit,
    InvalidResponse,
}

/// `ForgejoCliError`: the detail is safe to show; the token is never in it.
#[derive(Debug, Clone)]
pub struct ForgejoCliError {
    pub command: &'static str,
    pub cwd: String,
    pub detail: String,
    pub reason: Option<Reason>,
    pub http_status: Option<u16>,
    pub cause: Option<ErrorCause>,
}

impl ForgejoCliError {
    fn new(command: &'static str, cwd: &str, detail: impl Into<String>) -> Self {
        Self { command, cwd: cwd.into(), detail: detail.into(), reason: None, http_status: None, cause: None }
    }
    fn because(mut self, reason: Reason) -> Self {
        self.reason = Some(reason);
        self
    }
    fn authentication(command: &'static str, cwd: &str, detail: &str) -> Self {
        Self::new(command, cwd, detail).because(Reason::Authentication)
    }
    /// The original's error carries its text in `detail`; its own message is empty.
    pub fn message(&self) -> String {
        String::new()
    }
}

type CliResult<T> = Result<T, ForgejoCliError>;

fn status_reason(status: u16) -> Option<Reason> {
    match status {
        401 => Some(Reason::Authentication),
        403 => Some(Reason::Forbidden),
        404 => Some(Reason::NotFound),
        429 => Some(Reason::RateLimit),
        _ => None,
    }
}

// ---- logins and remotes -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub name: String,
    pub url: String,
    pub ssh_host: Option<String>,
    pub valid: Option<String>,
    pub user: String,
    pub default: String,
}

/// `parseForgejoLogins`: `tea login list --output json`, or nothing at all when it does not read.
pub fn parse_logins(raw: &str) -> Vec<Login> {
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(raw) else { return vec![] };
    let login = |entry: &Value| -> Field<Login> {
        let object = entry.as_object().ok_or(())?;
        let text = |key: &str| match object.get(key) {
            Some(Value::String(text)) => Ok(text.clone()),
            _ => Err(()),
        };
        Ok(Login { name: text("name")?, url: text("url")?, ssh_host: optional_string(object.get("ssh_host"))?, valid: optional_string(object.get("valid"))?, user: text("user")?, default: text("default")? })
    };
    entries.iter().map(login).collect::<Result<_, _>>().unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub host: String,
    pub hostname: String,
    pub ssh: bool,
    pub path: String,
}

/// `parseForgejoRemote`: an `http(s)` or `ssh` URL, or an scp-style address that may omit the user.
pub fn parse_remote(value: &str) -> Option<Remote> {
    static URL_LIKE: OnceLock<Regex> = OnceLock::new();
    static SCP: OnceLock<Regex> = OnceLock::new();
    if URL_LIKE.get_or_init(|| Regex::new(r"(?i)^(?:https?|ssh)://").unwrap()).is_match(value) {
        let url = url::Url::parse(value).ok()?;
        let hostname = match url.host_str() {
            Some(host) => host.to_lowercase(),
            // `ssh://` names no host, which is not an error to a URL parser that has no special schemes.
            None if url.scheme() == "ssh" => String::new(),
            None => return None,
        };
        let host = match url.port() {
            Some(port) => format!("{hostname}:{port}"),
            None => hostname.clone(),
        };
        let path = url.path().trim_matches('/');
        return Some(Remote { host, hostname, ssh: url.scheme() == "ssh", path: path.strip_suffix(".git").unwrap_or(path).to_string() });
    }
    // SCP remotes may omit the username; URL treats these as a custom scheme.
    let captures = SCP.get_or_init(|| Regex::new(r"^(?:[^@/]+@)?([^:/]+):([^/].*)$").unwrap()).captures(value)?;
    let (host, path) = (captures.get(1)?.as_str(), captures.get(2)?.as_str());
    (!host.is_empty() && !path.is_empty()).then(|| Remote { host: host.to_lowercase(), hostname: host.to_lowercase(), ssh: true, path: path.strip_suffix(".git").unwrap_or(path).to_string() })
}

/// `matchForgejoLogin`: the one login that serves this remote, or the default of several that
/// share a server.
pub fn match_login(logins: &[Login], remote: &Remote, requested_host: Option<&str>, host_only: bool) -> Option<Login> {
    let mut matches: Vec<&Login> = vec![];
    for login in logins {
        let Some(url) = parse_remote(&login.url) else { continue };
        if requested_host.is_some_and(|wanted| url.host != wanted.to_lowercase()) {
            continue;
        }
        let serves = if remote.ssh {
            let ssh_host = login.ssh_host.as_ref().map(|host| host.to_lowercase());
            ssh_host.as_deref() == Some(remote.host.as_str()) || ssh_host.as_deref() == Some(remote.hostname.as_str()) || url.hostname == remote.hostname
        } else {
            url.host == remote.host && ((host_only && remote.path.is_empty()) || url.path.is_empty() || remote.path == url.path || remote.path.starts_with(&format!("{}/", url.path)))
        };
        if !serves {
            continue;
        }
        // A login name is unique: a later row of the same name replaces the earlier one in place.
        match matches.iter().position(|held| held.name == login.name) {
            Some(at) => matches[at] = login,
            None => matches.push(login),
        }
    }
    match matches.as_slice() {
        [only] => Some((*only).clone()),
        [] => None,
        many => {
            let urls: std::collections::BTreeSet<&str> = many.iter().map(|login| login.url.as_str()).collect();
            if urls.len() == 1 { many.iter().find(|login| login.default == "true").map(|login| (*login).clone()) } else { None }
        }
    }
}

// ---- fj's key storage -----------------------------------------------------------------------------

/// `path.posix.join`: segments joined and normalised.
fn normalize_path(joined: &str, separator: char) -> String {
    let absolute = joined.starts_with(separator);
    let mut parts: Vec<&str> = vec![];
    for part in joined.split(separator) {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            other => parts.push(other),
        }
    }
    let body = parts.join(&separator.to_string());
    let trailing = if joined.ends_with(separator) && !body.is_empty() { separator.to_string() } else { String::new() };
    match (absolute, body.is_empty()) {
        (true, _) => format!("{separator}{body}{trailing}"),
        (false, true) => ".".into(),
        (false, false) => format!("{body}{trailing}"),
    }
}

/// Matches fj's `directories::ProjectDirs`, including its pre-0.6 organisation name.
pub fn keys_paths(platform: &str, home: &str, data_home: Option<&str>, app_data: Option<&str>) -> Vec<String> {
    let posix = |parts: &[&str]| normalize_path(&parts.join("/"), '/');
    match platform {
        "darwin" => ["forgejo-cli", "Cyborus"].iter().map(|organization| posix(&[home, "Library", "Application Support", &format!("{organization}.forgejo-cli"), "keys.json"])).collect(),
        "win32" => ["forgejo-cli", "Cyborus"]
            .iter()
            .map(|organization| {
                let base = match app_data.filter(|dir| !dir.is_empty()) {
                    Some(dir) => dir.to_string(),
                    None => normalize_path(&format!("{home}\\AppData\\Roaming").replace('/', "\\"), '\\'),
                };
                normalize_path(&format!("{base}\\{organization}\\forgejo-cli\\data\\keys.json").replace('/', "\\"), '\\')
            })
            .collect(),
        _ => {
            let base = match data_home.filter(|dir| dir.starts_with('/')) {
                Some(dir) => dir.to_string(),
                None => posix(&[home, ".local", "share"]),
            };
            vec![posix(&[&base, "forgejo-cli", "keys.json"])]
        }
    }
}

/// The platform in Node's spelling.
pub fn host_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        _ => "linux",
    }
}

#[derive(Debug, Clone, Default)]
struct Keys {
    /// Host to token.
    hosts: Vec<(String, String)>,
    aliases: Vec<(String, String)>,
}

impl Keys {
    fn token(&self, host: &str) -> Option<&str> {
        self.hosts.iter().find(|(name, _)| name == host).map(|(_, token)| token.as_str())
    }
}

/// `ForgejoKeysSchema`.
fn decode_keys(raw: &str) -> Option<Keys> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let hosts = value.get("hosts")?.as_object()?;
    let mut keys = Keys::default();
    for (name, entry) in hosts {
        let entry = entry.as_object()?;
        if !matches!(entry.get("type")?.as_str()?, "Application" | "OAuth") {
            return None;
        }
        keys.hosts.push((name.clone(), entry.get("token")?.as_str()?.to_string()));
    }
    match value.get("aliases") {
        None => {}
        Some(Value::Object(aliases)) => {
            for (alias, target) in aliases {
                keys.aliases.push((alias.clone(), target.as_str()?.to_string()));
            }
        }
        Some(_) => return None,
    }
    Some(keys)
}

// ---- the tool ----------------------------------------------------------------------------------------

/// What `fj` remembers between requests, once per host (`Shared::forgejo`): the token it last
/// renewed for a server, so the renewal is not asked for on every request.
#[derive(Default)]
pub struct ForgejoShared {
    authenticated: std::sync::Mutex<HashMap<String, (String, i64)>>,
    auth_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Default)]
pub struct RepositoryInput {
    pub cwd: String,
    pub context: Option<ProviderContext>,
    pub repository: Option<String>,
    pub reference: Option<String>,
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRepository {
    pub command: &'static str,
    pub login: String,
    pub repository: String,
    pub base_url: String,
}

pub struct ApiInput {
    pub target: RepositoryInput,
    pub path: String,
    pub method: Option<&'static str>,
    pub body: Option<Value>,
}

pub struct ApiOutput {
    pub stdout: String,
    /// `tea api --include` prints the response head on stderr; `fj` requests are given the same
    /// shape (status line, and the `link` header where there is one).
    pub stderr: String,
}

pub struct ForgejoCli {
    host: Host,
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    // A redirect is never followed: it would carry the token to wherever the server points.
    CLIENT.get_or_init(|| reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap_or_default())
}

fn is_missing_cli(error: &ForgejoCliError) -> bool {
    error.reason == Some(Reason::MissingCli)
}

impl ForgejoCli {
    pub fn new(host: Host) -> Self {
        Self { host }
    }

    fn shared(&self) -> &ForgejoShared {
        &self.host.shared.forgejo
    }

    pub async fn execute(&self, command: Option<&'static str>, cwd: &str, args: Vec<String>, stdin: Option<String>) -> CliResult<t3_vcs::VcsProcessOutput> {
        let name = command.unwrap_or("tea");
        let mut input: VcsProcessInput = self.host.cli("ForgejoCli.execute", name, args, cwd);
        input.stdin = stdin;
        input.timeout = Some(DEFAULT_TIMEOUT);
        self.host.process().run(input).await.map_err(|error| {
            let (reason, detail) = match &error {
                VcsError::ProcessSpawn { .. } => (Some(Reason::MissingCli), "Install Forgejo CLI (`fj` 0.6 or later) or Gitea CLI (`tea` 0.16 or later) and retry."),
                VcsError::ProcessExit { failure_kind: Some(t3_vcs::error::ExitFailureKind::Authentication), .. } => {
                    (Some(Reason::Authentication), "Authenticate this server with `fj auth login`, `fj auth add-token`, or `tea login add`.")
                }
                _ => (None, "Forgejo CLI command failed."),
            };
            let mut failure = ForgejoCliError::new(if name == "fj" { "fj" } else { "tea" }, cwd, detail);
            failure.reason = reason;
            if name != "fj" {
                failure.cause = Some(ErrorCause::new(error.tag(), error.to_string()));
            }
            failure
        })
    }

    /// Where `fj` keeps its tokens, from the host's environment.
    fn keys_paths(&self) -> Vec<String> {
        let env = self.host.env();
        let platform = host_platform();
        let home = env.get_trimmed(if platform == "win32" { "USERPROFILE" } else { "HOME" }).or_else(|| env.get_trimmed("HOME")).unwrap_or("/");
        keys_paths(platform, home, env.get_trimmed("XDG_DATA_HOME"), env.get_trimmed("APPDATA"))
    }

    async fn read_keys(&self, cwd: &str) -> CliResult<Keys> {
        let unreadable = || ForgejoCliError::authentication("fj", cwd, "Could not read fj authentication storage.");
        for path in self.keys_paths() {
            match tokio::fs::read_to_string(&path).await {
                Ok(raw) => return decode_keys(&raw).ok_or_else(|| ForgejoCliError::authentication("fj", cwd, "fj authentication storage is invalid. Authenticate again with fj.")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(unreadable()),
            }
        }
        Ok(Keys::default())
    }

    /// The logins `fj` holds, as `tea login list` would print them.
    fn public_logins(keys: &Keys, remote_url: Option<&str>) -> Vec<Login> {
        let remote = remote_url.and_then(parse_remote);
        let mut logins = vec![];
        for (host, _) in &keys.hosts {
            let Some(url) = parse_remote(&format!("https://{host}")) else { continue };
            // fj 0.6 drops URL mounts during whoami and OAuth renewal; tea supports them.
            if !url.path.is_empty() {
                continue;
            }
            // fj omits the scheme in storage. Only an explicit matching HTTP remote opts into HTTP.
            let scheme = if remote.as_ref().is_some_and(|remote| !remote.ssh && remote.host == url.host) && remote_url.is_some_and(|text| text.to_lowercase().starts_with("http://")) { "http" } else { "https" };
            let login = Login { name: host.clone(), url: format!("{scheme}://{host}"), ssh_host: None, valid: None, user: String::new(), default: "false".into() };
            let aliases: Vec<Login> = keys.aliases.iter().filter(|(_, target)| target == host).map(|(alias, _)| Login { ssh_host: Some(alias.clone()), ..login.clone() }).collect();
            logins.extend(if aliases.is_empty() { vec![login] } else { aliases });
        }
        logins
    }

    pub async fn list_logins(&self, cwd: &str, command: &'static str, remote_url: Option<&str>) -> CliResult<Vec<Login>> {
        if command == "fj" {
            return match self.read_keys(cwd).await {
                Ok(keys) => Ok(Self::public_logins(&keys, remote_url)),
                Err(failure) => {
                    // Stale credentials from an uninstalled fj must not disable an available tea login.
                    match self.execute(Some("fj"), cwd, vec!["version".into()], None).await {
                        Err(available) if is_missing_cli(&available) => Ok(vec![]),
                        _ => Err(failure),
                    }
                }
            };
        }
        let output = self.execute(None, cwd, ["login", "list", "--output", "json"].map(String::from).to_vec(), None).await?;
        Ok(parse_logins(&output.stdout))
    }

    async fn request_fj(&self, cwd: &str, base_url: &str, token: &str, path: &str, method: Option<&str>, body: Option<String>) -> CliResult<ApiOutput> {
        let invalid = || ForgejoCliError::new("fj", cwd, "Invalid Forgejo API path.");
        let base = url::Url::parse(&format!("{base_url}/api/v1/")).map_err(|_| invalid())?;
        let url = base.join(path).map_err(|_| invalid())?;
        if url.origin() != base.origin() || !url.path().starts_with(base.path()) || !url.username().is_empty() || url.password().is_some() {
            return Err(invalid());
        }
        let method = reqwest::Method::from_bytes(method.unwrap_or("GET").as_bytes()).map_err(|_| invalid())?;
        let mut request = http_client().request(method, url).header("Authorization", format!("token {token}")).timeout(DEFAULT_TIMEOUT);
        if let Some(body) = body {
            request = request.header("Content-Type", "application/json").body(body);
        }
        let failed = || ForgejoCliError::new("fj", cwd, "Forgejo API request failed or timed out.");
        let mut response = request.send().await.map_err(|_| failed())?;
        let status = response.status().as_u16();
        let link = response.headers().get("link").and_then(|value| value.to_str().ok()).map(str::to_string);
        let oversized = || ForgejoCliError::new("fj", cwd, "Forgejo returned an oversized or invalid response.").because(Reason::InvalidResponse);
        let mut collected: Vec<u8> = vec![];
        if status != 204 && status != 205 {
            while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
                collected.extend_from_slice(&chunk);
                if collected.len() > MAX_RESPONSE_BYTES {
                    return Err(oversized());
                }
            }
        }
        let text = String::from_utf8(collected).map_err(|_| oversized())?;
        if !(200..300).contains(&status) {
            // A server could echo what it was sent, so the token is taken out of whatever it says.
            let said = text.replace(token, "[redacted]");
            let detail = if status == 404 {
                "Forgejo repository or pull request was not found.".to_string()
            } else if !said.is_empty() {
                format!("Forgejo API request failed (HTTP {status}): {said}")
            } else {
                format!("Forgejo API request failed (HTTP {status}). Check this server's fj credentials and permissions.")
            };
            let mut failure = ForgejoCliError::new("fj", cwd, detail);
            failure.http_status = Some(status);
            failure.reason = status_reason(status);
            return Err(failure);
        }
        Ok(ApiOutput { stdout: text, stderr: format!("HTTP/1.1 {status}\n{}", link.map(|link| format!("link: {link}\n")).unwrap_or_default()) })
    }

    /// `fj` owns OAuth renewal: it is asked for `whoami`, and the file is read again after it has
    /// refreshed an expired token.
    async fn authenticate_fj(&self, cwd: &str, login: &Login) -> CliResult<String> {
        let _held = self.shared().auth_lock.lock().await;
        let keys = self.read_keys(cwd).await?;
        let token = keys.token(&login.name).map(str::to_string);
        let now = self.host.now_ms();
        if let Some(token) = &token
            && let Some((cached, time)) = self.shared().authenticated.lock().unwrap().get(&login.url)
            && cached == token
            && now - time < AUTH_CACHE_MS
        {
            return Ok(token.clone());
        }
        self.execute(Some("fj"), cwd, vec!["--host".into(), login.url.clone(), "whoami".into()], None).await?;
        let refreshed = self.read_keys(cwd).await?.token(&login.name).map(str::to_string);
        let Some(refreshed) = refreshed else { return Err(ForgejoCliError::authentication("fj", cwd, "fj has no credentials for this server. Authenticate again with fj.")) };
        self.shared().authenticated.lock().unwrap().insert(login.url.clone(), (refreshed.clone(), now));
        Ok(refreshed)
    }

    pub async fn get_account(&self, cwd: &str, base_url: &str) -> CliResult<String> {
        let logins = self.list_logins(cwd, "fj", Some(base_url)).await?;
        let Some(login) = logins.iter().find(|item| item.url.trim_end_matches('/') == base_url.trim_end_matches('/')) else {
            return Err(ForgejoCliError::authentication("fj", cwd, "fj has no credentials for this server."));
        };
        let token = Box::pin(self.authenticate_fj(cwd, login)).await?;
        let current = Box::pin(self.request_fj(cwd, login.url.trim_end_matches('/'), &token, "user", None, None)).await?;
        let user = serde_json::from_str::<Value>(&current.stdout).ok().and_then(|value| value.get("login").and_then(Value::as_str).map(str::to_string));
        user.filter(|user| !user.trim().is_empty()).ok_or_else(|| ForgejoCliError::new("fj", cwd, "Forgejo returned an invalid account response.").because(Reason::InvalidResponse))
    }

    pub async fn resolve_repository(&self, input: &RepositoryInput) -> CliResult<ResolvedRepository> {
        Box::pin(self.resolve_target(input, false)).await
    }

    async fn resolve_target(&self, input: &RepositoryInput, host_only: bool) -> CliResult<ResolvedRepository> {
        let cwd = input.cwd.as_str();
        let context_remote = input.context.as_ref().map(|context| context.remote_url.as_str());
        let reference_remote = input.reference.as_deref().and_then(parse_remote);
        fn truthy(value: Option<&str>) -> Option<&str> {
            value.filter(|text| !text.is_empty())
        }
        let mut remote_url: Option<String> = [truthy(input.reference.as_deref()), truthy(input.repository.as_deref()), truthy(context_remote)].into_iter().flatten().find(|value| parse_remote(value).is_some()).map(str::to_string);
        let mut remote = reference_remote.clone().or_else(|| input.repository.as_deref().and_then(parse_remote)).or_else(|| context_remote.and_then(parse_remote));
        if remote.is_none() && (input.repository.is_none() || input.host.is_some()) {
            let args: Vec<&str> = if input.host.is_some() { vec!["remote", "-v"] } else { vec!["remote", "get-url", "origin"] };
            let mut git: VcsProcessInput = self.host.cli("ForgejoCli.remote", "git", args, cwd);
            git.allow_non_zero_exit = true;
            let result = self.host.process().run(git).await.map_err(|error| {
                let mut failure = ForgejoCliError::new("tea", cwd, "Could not resolve the Forgejo repository remote.");
                failure.cause = Some(ErrorCause::new(error.tag(), error.to_string()));
                failure
            })?;
            if let Some(wanted) = &input.host {
                static FETCH: OnceLock<Regex> = OnceLock::new();
                let fetch = FETCH.get_or_init(|| Regex::new(r"^\S+\s+(https?://\S+)\s+\(fetch\)$").unwrap());
                let mut urls: Vec<String> = vec![];
                for line in result.stdout.split('\n') {
                    if let Some(url) = fetch.captures(t3_contracts::trim_wire_string(line)).map(|captures| captures[1].to_string())
                        && parse_remote(&url).is_some_and(|parsed| parsed.host == wanted.to_lowercase())
                        && !urls.contains(&url)
                    {
                        urls.push(url);
                    }
                }
                let mut origins: Vec<String> = vec![];
                for url in &urls {
                    if let Ok(parsed) = url::Url::parse(url) {
                        let origin = parsed.origin().ascii_serialization();
                        if !origins.contains(&origin) {
                            origins.push(origin);
                        }
                    }
                }
                remote_url = if urls.len() == 1 { urls.first().cloned() } else if origins.len() == 1 { origins.first().cloned() } else { None };
            } else {
                remote_url = Some(result.stdout.trim().to_string());
            }
            remote = remote_url.as_deref().filter(|url| !url.is_empty()).and_then(parse_remote);
        }
        if let Some(wanted) = &input.host {
            let lowered = wanted.to_lowercase();
            if !remote.as_ref().is_some_and(|remote| remote.ssh) && remote.as_ref().is_none_or(|remote| remote.host != lowered && remote.hostname != lowered) {
                remote = Some(Remote { host: lowered, hostname: wanted.split(':').next().unwrap_or(wanted).to_string(), ssh: false, path: remote.as_ref().map(|remote| remote.path.clone()).unwrap_or_default() });
            }
        }
        let scheme_remote_url = if remote.as_ref().is_some_and(|remote| remote.ssh) { input.context.as_ref().map(|context| context.provider.base_url.as_str()) } else { remote_url.as_deref() };
        let fj_logins = self.list_logins(cwd, "fj", scheme_remote_url.filter(|url| !url.is_empty())).await?;
        let requested_host = input.host.as_deref().or_else(|| input.context.as_ref().and_then(|context| context.requested_host.as_deref()));
        let match_host_only = host_only || (input.host.is_some() && remote.as_ref().is_none_or(|remote| remote.path.is_empty()));
        let select = |logins: &[Login]| -> Option<Login> {
            match &remote {
                Some(remote) => match_login(logins, remote, if remote.ssh { requested_host } else { None }, match_host_only),
                None => logins.iter().find(|item| item.default == "true").cloned().or_else(|| {
                    let names: std::collections::BTreeSet<&str> = logins.iter().map(|item| item.name.as_str()).collect();
                    if names.len() == 1 { logins.first().cloned() } else { None }
                }),
            }
        };
        let mut login = select(&fj_logins);
        let mut command: &'static str = "fj";
        if login.is_none() && fj_logins.iter().any(|item| remote.as_ref().is_none_or(|remote| match_login(std::slice::from_ref(item), remote, if remote.ssh { requested_host } else { None }, match_host_only).is_some())) {
            match self.execute(Some("fj"), cwd, vec!["version".into()], None).await {
                Ok(_) => return Err(ForgejoCliError::authentication("fj", cwd, "Multiple fj logins match this repository. Specify its full server URL.")),
                Err(available) if !is_missing_cli(&available) => return Err(available),
                Err(_) => {}
            }
        }
        if let Some(chosen) = &login {
            match self.authenticate_fj(cwd, chosen).await {
                Ok(_) => {}
                Err(failure) if is_missing_cli(&failure) => login = None,
                Err(failure) => return Err(failure),
            }
        }
        if login.is_none() {
            command = "tea";
            login = select(&self.list_logins(cwd, "tea", None).await?);
        }
        let Some(mut login) = login else {
            return Err(ForgejoCliError::authentication(
                "tea",
                cwd,
                "No matching Forgejo login. Use `fj auth login`, `fj auth add-token`, or `tea login add` for this server; choose a default when multiple tea accounts match.",
            ));
        };
        let base_url = login.url.trim_end_matches('/').to_string();
        if host_only {
            return Ok(ResolvedRepository { command, login: login.name, repository: String::new(), base_url });
        }
        let path = match (&reference_remote, input.repository.as_deref()) {
            (Some(reference), _) => reference.path.clone(),
            (None, Some(repository)) if !repository.is_empty() && parse_remote(repository).is_none() => repository.to_string(),
            _ => remote.as_ref().map(|remote| remote.path.clone()).unwrap_or_default(),
        };
        let base_path = url::Url::parse(&login.url).map(|url| url.path().trim_matches('/').to_string()).unwrap_or_default();
        let relative = if !base_path.is_empty() && path.split('/').count() > 2 && path.starts_with(&format!("{base_path}/")) { path[base_path.len() + 1..].to_string() } else { path };
        static PULLS: OnceLock<Regex> = OnceLock::new();
        let without_pull = PULLS.get_or_init(|| Regex::new(r"/pulls/\d+.*$").unwrap()).replace(&relative, "").into_owned();
        let repository_path = without_pull.strip_suffix(".git").unwrap_or(&without_pull).to_string();
        if command == "fj" && !repository_path.contains('/') {
            login.user = Box::pin(self.get_account(cwd, &login.url)).await?;
        }
        let repository = if repository_path.contains('/') { repository_path } else { format!("{}/{}", login.user, repository_path) };
        static SHAPE: OnceLock<Regex> = OnceLock::new();
        if !SHAPE.get_or_init(|| Regex::new(r"^[^/\s]+/[^/\s]+$").unwrap()).is_match(&repository) {
            return Err(ForgejoCliError::new(command, cwd, "Specify a Forgejo repository as owner/repository or its full server URL."));
        }
        Ok(ResolvedRepository { command, login: login.name, repository, base_url })
    }

    /// One API request, made as `fj` or `tea` according to who holds the login.
    pub async fn api(&self, input: ApiInput) -> CliResult<ApiOutput> {
        let cwd = input.target.cwd.as_str();
        let method = input.method.unwrap_or("GET");
        let path = input.path.trim_start_matches('/').to_string();
        let repository = Box::pin(self.resolve_target(&input.target, path == "user" && method == "GET")).await?;
        let stdin = input.body.as_ref().map(Value::to_string);
        let mut path = path;
        if let Some(asked) = input.target.repository.as_deref().filter(|asked| *asked != repository.repository) {
            // Repository identities retain the server mount path; API routes do not.
            let encode = |name: &str| name.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/");
            let prefix = format!("repos/{}", encode(asked));
            if path == prefix || path.starts_with(&format!("{prefix}/")) || path.starts_with(&format!("{prefix}?")) {
                path = format!("repos/{}{}", encode(&repository.repository), &path[prefix.len()..]);
            }
        }
        if repository.command == "fj" {
            let keys = self.read_keys(cwd).await?;
            let Some(token) = keys.token(&repository.login) else { return Err(ForgejoCliError::authentication("fj", cwd, "fj has no credentials for this server.")) };
            return Box::pin(self.request_fj(cwd, &repository.base_url, token, &path, input.method, stdin)).await;
        }
        let mut args: Vec<String> = ["api", "--include", "--login"].map(String::from).to_vec();
        args.push(repository.login.clone());
        if !repository.repository.is_empty() {
            args.extend(["--repo".to_string(), repository.repository.clone()]);
        }
        args.extend(["--method".to_string(), method.to_string()]);
        if stdin.is_some() {
            args.extend(["--data".to_string(), "@-".to_string()]);
        }
        args.push(format!("{}/api/v1/{path}", repository.base_url));
        let result = self.execute(None, cwd, args, stdin).await?;
        // tea reports HTTP failures with exit code zero; use its response status.
        static STATUS: OnceLock<Regex> = OnceLock::new();
        let status: Option<u16> = STATUS.get_or_init(|| Regex::new(r"(?m)^HTTP/\S+ (\d{3})").unwrap()).captures(&result.stderr).and_then(|captures| captures[1].parse().ok());
        match status {
            Some(status) if status < 400 => Ok(ApiOutput { stdout: result.stdout, stderr: result.stderr }),
            _ => {
                let detail = match status {
                    Some(401 | 403) => "Forgejo denied access. Check this server's `tea login` credentials and permissions.".to_string(),
                    Some(404) => "Forgejo repository or pull request was not found.".to_string(),
                    Some(429) => "Forgejo API rate limit exceeded.".to_string(),
                    Some(status) => format!("Forgejo API request failed (HTTP {status})."),
                    None => "Forgejo API request failed without an HTTP status.".to_string(),
                };
                let mut failure = ForgejoCliError::new("tea", cwd, detail);
                failure.http_status = status;
                failure.reason = status.and_then(status_reason);
                Err(failure)
            }
        }
    }
}

// ---- pull requests as change requests ---------------------------------------------------------------

/// `ForgejoPullRequestSchema` and `toForgejoChangeRequest`; `None` where the schema does not read.
pub fn decode_change_request(value: &Value) -> Option<ChangeRequest> {
    fn text<'a>(object: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
        object.get(key)?.as_str()
    }
    // A branch: its ref and sha, and the repository it lives in, which must be said (or be null).
    fn branch(value: Option<&Value>) -> Option<(String, Option<(String, String)>)> {
        let object = value?.as_object()?;
        let repository = match object.get("repo")? {
            Value::Null => None,
            Value::Object(repository) => Some((text(repository, "full_name")?.to_string(), text(repository.get("owner")?.as_object()?, "login")?.to_string())),
            _ => return None,
        };
        text(object, "sha")?;
        Some((text(object, "ref")?.to_string(), repository))
    }
    let object = value.as_object()?;
    let number = positive_integer(object.get("number"))?;
    let (title, url, state) = (text(object, "title")?, text(object, "html_url")?, text(object, "state")?);
    let merged = object.get("merged")?.as_bool()?;
    let draft = optional_bool(object.get("draft")).ok()?;
    let ((base_ref, base_repo), (head_ref, head_repo)) = (branch(object.get("base"))?, branch(object.get("head"))?);
    let closed_at = optional_nullable_string(object.get("closed_at")).ok()?;
    let merged_at = optional_nullable_string(object.get("merged_at")).ok()?;
    let updated = optional_date(object.get("updated_at")).ok()?;
    static WIP: OnceLock<Regex> = OnceLock::new();
    Some(ChangeRequest {
        provider: KIND,
        number,
        title: title.to_string(),
        url: url.to_string(),
        base_ref_name: base_ref,
        head_ref_name: head_ref,
        head_sha: None,
        state: if merged { ChangeRequestState::Merged } else if state == "closed" { ChangeRequestState::Closed } else { ChangeRequestState::Open },
        is_draft: Some(draft.unwrap_or_else(|| WIP.get_or_init(|| Regex::new(r"(?i)^(?:\[WIP\]|WIP:)").unwrap()).is_match(title))),
        closed_at: Some(closed_at),
        merged_at: Some(merged_at),
        updated_at: updated,
        is_cross_repository: Some(matches!((&head_repo, &base_repo), (Some(head), Some(base)) if head.0 != base.0)),
        head_repository_name_with_owner: Some(head_repo.as_ref().map(|head| head.0.clone())),
        head_repository_owner_login: Some(head_repo.map(|head| head.1)),
    })
}

fn positive_integer(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_f64()?;
    (number.fract() == 0.0 && number >= 1.0 && number <= 9_007_199_254_740_991.0).then_some(number as u64)
}

// ---- the provider -------------------------------------------------------------------------------------

fn cli_failure(operation: &str, cwd: &str, error: &ForgejoCliError) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, error.detail.clone())
        .with_command(Some(error.command))
        .with_cause_chain(Some(ErrorCause::new("ForgejoCliError", error.message()).with_nested(error.cause.clone())))
}

fn operation_failure(operation: &str, cwd: &str) -> SourceControlProviderError {
    SourceControlProviderError::new(KIND, operation, cwd, "Forgejo operation failed.")
}

fn repository_path(repository: &str) -> String {
    format!("repos/{}", repository.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/"))
}

fn invalid_response(cwd: &str) -> ForgejoCliError {
    ForgejoCliError::new("tea", cwd, "Forgejo API returned an invalid response.").because(Reason::InvalidResponse)
}

fn target(cwd: &str, context: &Option<ProviderContext>, repository: Option<&str>, reference: Option<&str>) -> RepositoryInput {
    RepositoryInput { cwd: cwd.into(), context: context.clone(), repository: repository.map(str::to_string), reference: reference.map(str::to_string), host: None }
}

struct Repository {
    full_name: String,
    clone_url: String,
    ssh_url: String,
    default_branch: Option<String>,
}

fn decode_repository(raw: &str, cwd: &str) -> CliResult<Repository> {
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid_response(cwd))?;
    let read = || -> Field<Repository> {
        let object = value.as_object().ok_or(())?;
        let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string).ok_or(());
        Ok(Repository { full_name: text("full_name")?, clone_url: text("clone_url")?, ssh_url: text("ssh_url")?, default_branch: optional_nullable_string(object.get("default_branch"))? })
    };
    read().map_err(|()| invalid_response(cwd))
}

impl Repository {
    fn urls(&self) -> SourceControlRepositoryCloneUrls {
        SourceControlRepositoryCloneUrls { name_with_owner: self.full_name.clone(), url: self.clone_url.clone(), ssh_url: self.ssh_url.clone() }
    }
}

pub struct ForgejoProvider {
    host: Host,
    cli: ForgejoCli,
}

impl ForgejoProvider {
    pub fn new(host: Host) -> Self {
        Self { cli: ForgejoCli::new(host.clone()), host }
    }

    /// One request whose answer must read as `decode` says.
    async fn request<T>(&self, cwd: &str, input: ApiInput, decode: impl FnOnce(&str) -> Option<T>) -> CliResult<T> {
        let output = self.cli.api(input).await?;
        decode(&output.stdout).ok_or_else(|| invalid_response(cwd))
    }

    async fn get_pull(&self, cwd: &str, context: &Option<ProviderContext>, reference: &str) -> CliResult<(ResolvedRepository, Value)> {
        let repo = self.cli.resolve_repository(&target(cwd, context, None, Some(reference))).await?;
        static NUMBER: OnceLock<Regex> = OnceLock::new();
        let number = NUMBER.get_or_init(|| Regex::new(r"(?:^#?|/pulls/)(\d+)(?:/[^?#]*)?(?:[?#].*)?$").unwrap()).captures(reference).map(|captures| captures[1].to_string());
        let Some(number) = number else { return Err(ForgejoCliError::new("tea", cwd, "Specify a pull request number or Forgejo pull request URL.")) };
        let value = self
            .request(cwd, ApiInput { target: target(cwd, context, None, Some(reference)), path: format!("{}/pulls/{number}", repository_path(&repo.repository)), method: None, body: None }, |raw| {
                serde_json::from_str::<Value>(raw).ok().filter(|value| decode_change_request(value).is_some())
            })
            .await?;
        Ok((repo, value))
    }
}

impl SourceControlProvider for ForgejoProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn list_change_requests(&self, input: ListChangeRequestsInput) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>> {
        Box::pin(async move {
            let run = async {
                let cwd = input.cwd.as_str();
                let repo = self.cli.resolve_repository(&target(cwd, &input.context, None, None)).await?;
                let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
                let branch = source_branch(&input.head_selector, input.source.as_ref());
                let limit = input.limit.unwrap_or(20) as usize;
                let state = match input.state {
                    ListState::Open => "open",
                    ListState::Closed | ListState::Merged => "closed",
                    ListState::All => "all",
                };
                let mut results: Vec<ChangeRequest> = vec![];
                let mut page = 1;
                while results.len() < limit {
                    let path = format!("{}/pulls?state={state}&sort=recentupdate&limit=50&page={page}", repository_path(&repo.repository));
                    let items = self
                        .request(cwd, ApiInput { target: target(cwd, &input.context, None, None), path, method: None, body: None }, |raw| match serde_json::from_str::<Value>(raw).ok()? {
                            Value::Array(items) => items.iter().map(|item| decode_change_request(item).map(|decoded| (decoded, item.clone()))).collect::<Option<Vec<_>>>(),
                            _ => None,
                        })
                        .await?;
                    for (normalized, raw) in &items {
                        let head = raw.get("head");
                        let head_repo = head.and_then(|head| head.get("repo")).filter(|repo| !repo.is_null());
                        let wrong_branch = head.and_then(|head| head.get("ref")).and_then(Value::as_str) != Some(branch.as_str());
                        let wrong_repository = source.as_ref().and_then(|source| source.repository.as_deref()).is_some_and(|wanted| head_repo.and_then(|repo| repo.get("full_name")).and_then(Value::as_str) != Some(wanted));
                        let wrong_owner = source.as_ref().and_then(|source| source.owner.as_deref()).is_some_and(|wanted| head_repo.and_then(|repo| repo.get("owner")).and_then(|owner| owner.get("login")).and_then(Value::as_str) != Some(wanted));
                        if wrong_branch || wrong_repository || wrong_owner {
                            continue;
                        }
                        let wanted = match input.state {
                            ListState::All => true,
                            ListState::Open => normalized.state == ChangeRequestState::Open,
                            ListState::Closed => normalized.state == ChangeRequestState::Closed,
                            ListState::Merged => normalized.state == ChangeRequestState::Merged,
                        };
                        if wanted {
                            results.push(normalized.clone());
                        }
                    }
                    if items.is_empty() {
                        break;
                    }
                    page += 1;
                }
                results.truncate(limit);
                Ok::<_, ForgejoCliError>(results)
            };
            run.await.map_err(|error| cli_failure("listChangeRequests", &input.cwd, &error))
        })
    }

    fn get_change_request(&self, input: GetChangeRequestInput) -> BoxFuture<'_, ProviderResult<ChangeRequest>> {
        Box::pin(async move {
            let (_, value) = self.get_pull(&input.cwd, &input.context, &input.reference).await.map_err(|error| cli_failure("getChangeRequest", &input.cwd, &error))?;
            decode_change_request(&value).ok_or_else(|| operation_failure("getChangeRequest", &input.cwd))
        })
    }

    fn create_change_request(&self, input: CreateChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            let run = async {
                let cwd = input.cwd.as_str();
                let repo = self.cli.resolve_repository(&target(cwd, &input.context, None, None)).await?;
                let source = source_ref_from_input(&input.head_selector, input.source.as_ref());
                let owner = source.as_ref().and_then(|source| source.owner.clone().or_else(|| source.repository.as_deref().and_then(|repository| repository.split('/').next()).map(str::to_string)));
                let head = source_branch(&input.head_selector, input.source.as_ref());
                let body = tokio::fs::read_to_string(&input.body_file).await.map_err(|_| ForgejoCliError::new("tea", cwd, "Forgejo operation failed."))?;
                let repository = input.target.as_ref().and_then(|target| target.repository.clone()).unwrap_or(repo.repository);
                self.cli
                    .api(ApiInput {
                        target: target(cwd, &input.context, None, None),
                        path: format!("{}/pulls", repository_path(&repository)),
                        method: Some("POST"),
                        body: Some(json!({
                            "base": input.target.as_ref().map_or(input.base_ref_name.as_str(), |target| target.ref_name.as_str()),
                            "head": match &owner { Some(owner) => format!("{owner}:{head}"), None => head },
                            "title": input.title,
                            "body": body,
                        })),
                    })
                    .await?;
                Ok::<_, ForgejoCliError>(())
            };
            run.await.map_err(|error| cli_failure("createChangeRequest", &input.cwd, &error))
        })
    }

    fn get_repository_clone_urls(&self, input: RepositoryUrlsInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            let run = async {
                let cwd = input.cwd.as_str();
                let repo = self.cli.resolve_repository(&target(cwd, &input.context, Some(&input.repository), None)).await?;
                let output = self.cli.api(ApiInput { target: target(cwd, &input.context, Some(&input.repository), None), path: repository_path(&repo.repository), method: None, body: None }).await?;
                Ok::<_, ForgejoCliError>(decode_repository(&output.stdout, cwd)?.urls())
            };
            run.await.map_err(|error| cli_failure("getRepositoryCloneUrls", &input.cwd, &error))
        })
    }

    fn create_repository(&self, input: CreateRepositoryInput) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            let run = async {
                let cwd = input.cwd.as_str();
                let repo = self.cli.resolve_repository(&target(cwd, &None, Some(&input.repository), None)).await?;
                let user = self
                    .request(cwd, ApiInput { target: target(cwd, &None, Some(&input.repository), None), path: "user".into(), method: None, body: None }, |raw| {
                        serde_json::from_str::<Value>(raw).ok()?.get("login")?.as_str().map(str::to_string)
                    })
                    .await?;
                let (owner, name) = repo.repository.split_once('/').unwrap_or((repo.repository.as_str(), ""));
                let path = if owner == user { "user/repos".to_string() } else { format!("orgs/{}/repos", encode_uri_component(owner)) };
                let output = self
                    .cli
                    .api(ApiInput {
                        target: target(cwd, &None, Some(&input.repository), None),
                        path,
                        method: Some("POST"),
                        body: Some(json!({"name": name, "private": input.visibility == SourceControlRepositoryVisibility::Private, "auto_init": false})),
                    })
                    .await?;
                Ok::<_, ForgejoCliError>(decode_repository(&output.stdout, cwd)?.urls())
            };
            run.await.map_err(|error| cli_failure("createRepository", &input.cwd, &error))
        })
    }

    fn get_default_branch(&self, input: DefaultBranchInput) -> BoxFuture<'_, ProviderResult<Option<String>>> {
        Box::pin(async move {
            let run = async {
                let cwd = input.cwd.as_str();
                let repo = self.cli.resolve_repository(&target(cwd, &input.context, None, None)).await?;
                let output = self.cli.api(ApiInput { target: target(cwd, &input.context, None, None), path: repository_path(&repo.repository), method: None, body: None }).await?;
                Ok::<_, ForgejoCliError>(decode_repository(&output.stdout, cwd)?.default_branch)
            };
            run.await.map_err(|error| cli_failure("getDefaultBranch", &input.cwd, &error))
        })
    }

    fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            let run = async {
                let cwd = input.cwd.as_str();
                let repo = self.cli.resolve_repository(&target(cwd, &input.context, None, Some(&input.reference))).await?;
                let (_, pull) = self.get_pull(cwd, &input.context, &input.reference).await?;
                let number = pull["number"].as_u64().unwrap_or_default();
                let head_sha = pull["head"]["sha"].as_str().unwrap_or_default().to_string();
                let git = |args: &[&str], allow_failure: bool| {
                    let mut process: VcsProcessInput = self.host.cli("ForgejoSourceControlProvider.checkoutChangeRequest", "git", args.iter().copied(), cwd);
                    process.allow_non_zero_exit = allow_failure;
                    let host = self.host.clone();
                    async move {
                        host.process().run(process).await.map_err(|error| {
                            let mut failure = ForgejoCliError::new("tea", cwd, "Forgejo operation failed.");
                            failure.cause = Some(ErrorCause::new(error.tag(), error.to_string()));
                            failure
                        })
                    }
                };
                if repo.command == "fj" {
                    // fj checkout cannot target a repository outside the local remotes.
                    let urls = self
                        .request(cwd, ApiInput { target: target(cwd, &input.context, None, Some(&input.reference)), path: repository_path(&repo.repository), method: None, body: None }, |raw| {
                            decode_repository(raw, cwd).ok().map(|repository| (repository.clone_url, repository.ssh_url))
                        })
                        .await?;
                    let use_ssh = input.context.as_ref().and_then(|context| parse_remote(&context.remote_url)).is_some_and(|remote| remote.ssh);
                    git(&["fetch", "--", if use_ssh { &urls.1 } else { &urls.0 }, &format!("refs/pull/{number}/head")], false).await?;
                    let branch = format!("pulls/{number}");
                    let existing = git(&["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")], true).await?;
                    if existing.exit_code == 0 {
                        git(&["checkout", &branch], false).await?;
                    } else {
                        git(&["checkout", "-b", &branch, "FETCH_HEAD"], false).await?;
                    }
                } else {
                    let args: Vec<String> = ["pulls", "checkout", "--login", &repo.login, "--repo", &repo.repository, "--branch", &number.to_string()].iter().map(|arg| arg.to_string()).collect();
                    self.cli.execute(None, cwd, args, None).await?;
                }
                if input.force {
                    // tea leaves an existing PR branch at its old tip. Keep dirty files safe while
                    // bringing the selected branch to the PR revision we fetched.
                    git(&["reset", "--keep", &head_sha], false).await?;
                }
                Ok::<_, ForgejoCliError>(())
            };
            run.await.map_err(|error| cli_failure("checkoutChangeRequest", &input.cwd, &error))
        })
    }
}

// ---- discovery -----------------------------------------------------------------------------------------

/// Both tools are probed: `fj` first when it holds a login for the checkout's server, otherwise
/// `tea`.
pub struct ForgejoProbe {
    host: Host,
    cli: ForgejoCli,
}

impl ForgejoProbe {
    pub fn new(host: Host) -> Self {
        Self { cli: ForgejoCli::new(host.clone()), host }
    }

    async fn probe_executable(&self, cwd: &str, executable: &'static str, version_args: &[&str], auth_args: Vec<String>) -> (SourceControlProviderDiscoveryItem, Option<(String, String, i32)>) {
        let mut version: VcsProcessInput = self.host.cli("source-control.discovery.probe", executable, version_args.iter().copied(), cwd);
        version.timeout = Some(Duration::from_secs(5));
        version.max_output_bytes = Some(8_000);
        version.append_truncation_marker = true;
        let item = |status, version: Option<String>, detail: Option<String>, auth| SourceControlProviderDiscoveryItem {
            kind: KIND,
            label: LABEL.into(),
            executable: Some(executable.into()),
            status,
            version,
            install_hint: INSTALL_HINT.into(),
            detail,
            auth,
        };
        let version = match self.host.process().run(version).await {
            Ok(result) => first_non_empty_line(&result.stdout).or_else(|| first_non_empty_line(&result.stderr)),
            Err(error) => return (item(SourceControlDiscoveryStatus::Missing, None, crate::source_control_discovery::detail_from_error(&error), provider_auth(SourceControlProviderAuthStatus::Unknown, None, None, Some("Hosting integration command was not found on the server PATH."))), None),
        };
        let mut auth: VcsProcessInput = self.host.cli("source-control.discovery.auth", executable, auth_args, cwd);
        auth.allow_non_zero_exit = true;
        auth.timeout = Some(Duration::from_secs(5));
        auth.max_output_bytes = Some(8_000);
        auth.append_truncation_marker = true;
        match self.host.process().run(auth).await {
            Ok(result) => (item(SourceControlDiscoveryStatus::Available, version, None, provider_auth(SourceControlProviderAuthStatus::Unknown, None, None, None)), Some((result.stdout, result.stderr, result.exit_code))),
            Err(error) => (item(SourceControlDiscoveryStatus::Available, version, None, provider_auth(SourceControlProviderAuthStatus::Unknown, None, None, crate::source_control_discovery::detail_from_error(&error).as_deref())), None),
        }
    }
}

fn tea_auth(stdout: &str) -> t3_contracts::SourceControlProviderAuth {
    let logins = parse_logins(stdout);
    match logins.iter().find(|entry| entry.default == "true").or(logins.first()) {
        Some(login) => provider_auth(
            if login.valid.as_deref() == Some("true") { SourceControlProviderAuthStatus::Authenticated } else { SourceControlProviderAuthStatus::Unauthenticated },
            Some(&login.user),
            parse_remote(&login.url).map(|remote| remote.host).as_deref(),
            None,
        ),
        None => provider_auth(SourceControlProviderAuthStatus::Unauthenticated, None, None, Some("Run `tea login add` to authenticate a Forgejo or Gitea server.")),
    }
}

impl ManagedProbe for ForgejoProbe {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn probe<'a>(&'a self, cwd: &'a str) -> BoxFuture<'a, SourceControlProviderDiscoveryItem> {
        Box::pin(async move {
            let mut remote: VcsProcessInput = self.host.cli("source-control.discovery.remote", "git", ["remote", "get-url", "origin"], cwd);
            remote.allow_non_zero_exit = true;
            remote.timeout = Some(Duration::from_secs(5));
            remote.max_output_bytes = Some(8_000);
            let remote_url = self.host.process().run(remote).await.map(|result| result.stdout.trim().to_string()).unwrap_or_default();
            let credentials = self.cli.list_logins(cwd, "fj", Some(&remote_url).filter(|url| !url.is_empty()).map(String::as_str)).await;
            let logins = credentials.as_ref().cloned().unwrap_or_default();
            let parsed = parse_remote(&remote_url);
            let login = parsed.as_ref().and_then(|remote| match_login(&logins, remote, None, false)).or_else(|| logins.iter().find(|entry| entry.default == "true").cloned()).or_else(|| logins.first().cloned());
            let auth_args: Vec<String> = match &login {
                Some(login) => vec!["--host".into(), login.url.clone(), "whoami".into()],
                None => vec!["auth".into(), "list".into()],
            };
            let (mut fj, outcome) = self.probe_executable(cwd, "fj", &["version"], auth_args).await;
            if fj.status == SourceControlDiscoveryStatus::Available {
                let host_of = login.as_ref().and_then(|login| parse_remote(&login.url)).map(|remote| remote.host);
                fj.auth = if credentials.is_err() {
                    provider_auth(SourceControlProviderAuthStatus::Unknown, None, None, Some("Could not read fj authentication storage. Authenticate again with fj."))
                } else if login.is_some() && outcome.as_ref().is_some_and(|(_, _, code)| *code == 0) {
                    provider_auth(SourceControlProviderAuthStatus::Authenticated, login.as_ref().map(|login| login.user.as_str()), host_of.as_deref(), None)
                } else {
                    provider_auth(SourceControlProviderAuthStatus::Unauthenticated, None, None, Some("Authenticate this server with `fj --host <server-url> auth add-token`."))
                };
            }
            // A configured fj account owns its requests, including authentication errors.
            if fj.status == SourceControlDiscoveryStatus::Available && (login.is_some() || credentials.is_err()) {
                if let Some(login) = &login
                    && fj.auth.status == SourceControlProviderAuthStatus::Authenticated
                {
                    let host_of = parse_remote(&login.url).map(|remote| remote.host);
                    fj.auth = match self.cli.get_account(cwd, &login.url).await {
                        Ok(account) => provider_auth(SourceControlProviderAuthStatus::Authenticated, Some(&account), host_of.as_deref(), None),
                        Err(failure) => provider_auth(SourceControlProviderAuthStatus::Unknown, None, host_of.as_deref(), Some(&failure.detail)),
                    };
                }
                return fj;
            }
            let (mut tea, outcome) = self.probe_executable(cwd, "tea", &["--version"], ["login", "status", "--output", "json"].map(String::from).to_vec()).await;
            if let Some((stdout, _, _)) = outcome {
                tea.auth = tea_auth(&stdout);
            }
            if tea.status == SourceControlDiscoveryStatus::Available || fj.status == SourceControlDiscoveryStatus::Missing { tea } else { fj }
        })
    }

    fn refine_unknown_remote<'a>(&'a self, cwd: &'a str, context: &'a ProviderContext) -> BoxFuture<'a, Option<SourceControlProviderInfo>> {
        Box::pin(async move {
            let remote = parse_remote(&context.remote_url)?;
            for command in ["fj", "tea"] {
                let logins = self.cli.list_logins(cwd, command, Some(&context.remote_url)).await.unwrap_or_default();
                if let Some(login) = match_login(&logins, &remote, context.requested_host.as_deref(), false) {
                    return Some(SourceControlProviderInfo { kind: KIND, name: LABEL.into(), base_url: login.url });
                }
            }
            None
        })
    }
}

/// What the original's identity resolver makes of a Forgejo checkout once a login has named its
/// server: the browser address of the repository, with the server's mount path taken out of the
/// remote's path (an ssh remote has none to take out).
pub fn web_url(remote_url: &str, base_url: &str) -> Option<String> {
    let remote = parse_remote(remote_url)?;
    let base_url = base_url.trim_end_matches('/');
    let base_path = url::Url::parse(base_url).ok()?.path().trim_matches('/').to_string();
    let path = if !remote.ssh && !base_path.is_empty() && remote.path.starts_with(&format!("{base_path}/")) { remote.path[base_path.len() + 1..].to_string() } else { remote.path };
    Some(format!("{base_url}/{path}"))
}
