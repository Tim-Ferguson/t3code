//! The source control settings and the clone step of adding a project: how a discovered tool or
//! host reads, which GitHub logins can be chosen, what a Bitbucket or GitHub credential save
//! sends, and where a clone lands. Port of `SourceControlSettings.tsx`, `GitHubAccountSettings`
//! (and its `.logic`), `GitHubTokenSettings`, `BitbucketCredentialsSettings`,
//! `RedactedSensitiveText` and the clone helpers of `operations/projects.ts`.
use crate::project_folder;
use regex::Regex;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::OnceLock};
use t3_contracts::{
    SourceControlDiscoveryResult, SourceControlDiscoveryStatus, SourceControlProviderAccount, SourceControlProviderAuth, SourceControlProviderAuthStatus,
    SourceControlProviderDiscoveryItem, SourceControlProviderKind, SourceControlRepositoryInfo, VcsDiscoveryItem, VcsDriverKindWire,
};

pub const DISCOVER: &str = "server.discoverSourceControl";
pub const LOOKUP: &str = "sourceControl.lookupRepository";
pub const CLONE: &str = "sourceControl.cloneRepository";

// ---- the list of tools and hosts ----

/// One row of the page: a version control tool or a hosting provider.
#[derive(Debug, Clone, Copy)]
pub enum Item<'a> {
    Vcs(&'a VcsDiscoveryItem),
    Provider(&'a SourceControlProviderDiscoveryItem),
}

/// A run of a row's summary: plain text, a command name, or an account (which the page hides
/// until asked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Code(String),
    Account(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// A tool this client lists but the server does not support yet.
    Off,
    Warning,
    Ok,
}

impl<'a> Item<'a> {
    pub fn key(&self) -> String {
        match self {
            Self::Vcs(item) => format!("vcs:{}", vcs_kind(item.kind)),
            Self::Provider(item) => format!("provider:{}", item.kind),
        }
    }
    pub fn label(&self) -> &'a str {
        match self {
            Self::Vcs(item) => &item.label,
            Self::Provider(item) => &item.label,
        }
    }
    pub fn version(&self) -> Option<&'a str> {
        match self {
            Self::Vcs(item) => item.version.as_deref(),
            Self::Provider(item) => item.version.as_deref(),
        }
    }
    fn available(&self) -> bool {
        match self {
            Self::Vcs(item) => item.status == SourceControlDiscoveryStatus::Available,
            Self::Provider(item) => item.status == SourceControlDiscoveryStatus::Available,
        }
    }
    fn install_hint(&self) -> &'a str {
        match self {
            Self::Vcs(item) => &item.install_hint,
            Self::Provider(item) => &item.install_hint,
        }
    }
    fn auth(&self) -> Option<&'a SourceControlProviderAuth> {
        match self {
            Self::Provider(item) => Some(&item.auth),
            Self::Vcs(_) => None,
        }
    }
    /// A tool the server lists and cannot use yet.
    pub fn not_ready(&self) -> bool {
        matches!(self, Self::Vcs(item) if !item.implemented)
    }
    /// What the (read-only) availability switch shows.
    pub fn enabled(&self) -> bool {
        match self {
            Self::Provider(item) => item.status == SourceControlDiscoveryStatus::Available && item.auth.status == SourceControlProviderAuthStatus::Authenticated,
            Self::Vcs(item) => item.status == SourceControlDiscoveryStatus::Available && item.implemented,
        }
    }
    pub fn dot(&self) -> Dot {
        if self.not_ready() {
            Dot::Off
        } else if !self.available() || self.auth().is_some_and(|auth| auth.status != SourceControlProviderAuthStatus::Authenticated) {
            Dot::Warning
        } else {
            Dot::Ok
        }
    }
    /// The badge beside the name: a warning for a host that is installed and not signed in.
    pub fn badge(&self) -> Option<&'static str> {
        if self.not_ready() {
            return Some("Coming Soon");
        }
        match self.auth() {
            Some(auth) if auth.status == SourceControlProviderAuthStatus::Unauthenticated => Some("Not authenticated"),
            _ => None,
        }
    }
    pub fn account(&self) -> Option<&'a str> {
        self.auth().and_then(|auth| auth.account.as_deref())
    }
    /// `itemSummary`.
    pub fn summary(&self) -> Vec<Part> {
        use Part::*;
        let label = self.label();
        if self.not_ready() {
            return vec![Text(format!("Support for {label} is coming soon."))];
        }
        if !self.available() {
            return vec![Text(format!("Not available on this server: {}", self.install_hint()))];
        }
        let Some(auth) = self.auth() else { return vec![Text("Available".into())] };
        let Self::Provider(item) = self else { unreachable!("only providers have auth") };
        match auth.status {
            SourceControlProviderAuthStatus::Authenticated => {
                // The server names the account its requests use, Settings choice included, and
                // says when an environment token overrides it.
                let mut parts = vec![Text("Authenticated".into())];
                if let Some(account) = auth.account.as_deref().filter(|account| !account.is_empty()) {
                    parts.push(Text("as".into()));
                    parts.push(Account(account.to_owned()));
                }
                if let Some(detail) = auth.detail.as_deref().filter(|detail| !detail.is_empty()) {
                    parts.push(Text(format!("\u{b7} {detail}")));
                }
                parts
            }
            // API integrations have no CLI to sign in with.
            SourceControlProviderAuthStatus::Unauthenticated if item.executable.is_none() => vec![Text(format!("Available. {}", item.install_hint))],
            // Signed in, but every login is turned off here: the fix is the switch, not the CLI.
            SourceControlProviderAuthStatus::Unauthenticated if auth.accounts.as_ref().is_some_and(|accounts| accounts.iter().any(|entry| entry.authenticated)) => {
                vec![Text(auth.detail.clone().unwrap_or_else(|| format!("Every {label} host is turned off.")))]
            }
            SourceControlProviderAuthStatus::Unauthenticated => vec![
                Text(format!("{label} is not authenticated on this server. Sign in or configure credentials using the")),
                Code(item.executable.clone().unwrap_or_default()),
                Text("tool on the server host to enable change request features.".into()),
            ],
            SourceControlProviderAuthStatus::Unknown => vec![Text(format!("Could not verify {label}. {}", auth.detail.clone().unwrap_or_else(|| item.install_hint.clone())))],
        }
    }
}
fn vcs_kind(kind: VcsDriverKindWire) -> &'static str {
    match kind {
        VcsDriverKindWire::Git => "git",
        VcsDriverKindWire::Jj => "jj",
        VcsDriverKindWire::Unknown => "unknown",
    }
}

/// The page's rows in the server's order. Forgejo is not something this server supports, so a
/// provider of that kind is not listed.
pub fn rows(result: &SourceControlDiscoveryResult) -> (Vec<Item<'_>>, Vec<Item<'_>>) {
    (
        result.version_control_systems.iter().map(Item::Vcs).collect(),
        result.source_control_providers.iter().filter(|item| item.kind != SourceControlProviderKind::Forgejo).map(Item::Provider).collect(),
    )
}

pub fn decode_discovery(value: &Value) -> Result<SourceControlDiscoveryResult, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("Invalid source control discovery result: {error}"))
}

// ---- hiding an account until asked ----

const REDACTED_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// `redactedPlaceholder`: a stable stand-in of the same shape, so a screenshot shows no login.
pub fn redacted_placeholder(value: &str) -> String {
    let mut state: i32 = 0x811c_9dc5_u32 as i32;
    for unit in value.encode_utf16() {
        state ^= i32::from(unit);
        state = state.wrapping_mul(0x0100_0193);
    }
    let mut next = || {
        state = (state ^ ((state as u32) >> 13) as i32).wrapping_mul(0x85eb_ca6b_u32 as i32);
        state = (state ^ ((state as u32) >> 16) as i32).wrapping_mul(0xc2b2_ae35_u32 as i32);
        REDACTED_ALPHABET[(i64::from(state).unsigned_abs() % REDACTED_ALPHABET.len() as u64) as usize] as char
    };
    value.chars().map(|c| if matches!(c, '@' | '.' | '-' | '_') { c } else { next() }).collect()
}

// ---- GitHub logins ----

/// One host's logins, as `groupGitHubAccounts` gathers them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGroup {
    pub host: String,
    /// gh's active stored login, the one used when Settings pin nothing.
    pub active_account: Option<String>,
    /// Stored logins that work and can be pinned.
    pub selectable: Vec<String>,
    /// Logins gh holds but cannot use; shown with their error.
    pub broken: Vec<SourceControlProviderAccount>,
    /// Set when `GH_TOKEN` or a sibling overrides every stored login for this host.
    pub environment_variable: Option<String>,
}
pub fn group_github_accounts(accounts: &[SourceControlProviderAccount]) -> Vec<HostGroup> {
    let mut hosts: Vec<&str> = vec![];
    for entry in accounts {
        if !hosts.contains(&entry.host.as_str()) {
            hosts.push(&entry.host);
        }
    }
    hosts
        .into_iter()
        .map(|host| {
            let entries: Vec<_> = accounts.iter().filter(|entry| entry.host == host).collect();
            let stored: Vec<_> = entries.iter().filter(|entry| entry.environment_variable.is_none()).copied().collect();
            let selectable: Vec<_> = stored.iter().filter(|entry| entry.authenticated).copied().collect();
            HostGroup {
                host: host.to_owned(),
                active_account: selectable.iter().find(|entry| entry.active).or(selectable.first()).map(|entry| entry.account.clone()),
                selectable: selectable.iter().map(|entry| entry.account.clone()).collect(),
                broken: stored.iter().filter(|entry| !entry.authenticated).map(|entry| (*entry).clone()).collect(),
                environment_variable: entries.iter().find_map(|entry| entry.environment_variable.clone().filter(|name| !name.is_empty())),
            }
        })
        .collect()
}

/// What changes for one host: its switch, or the login pinned for it (Some(None) unpins).
#[derive(Debug, Clone, Default)]
pub struct HostChange {
    pub enabled: Option<bool>,
    pub account: Option<Option<String>>,
}
/// `nextGitHubHosts`: the whole `hosts` map after one host changes. Hosts left on gh's defaults
/// (enabled, no pinned account) are dropped so settings only hold real choices.
pub fn next_github_hosts(current: &Value, host: &str, change: &HostChange) -> Value {
    let previous = &current[host];
    let enabled = change.enabled.unwrap_or_else(|| previous["enabled"].as_bool().unwrap_or(true));
    let account = match &change.account {
        None => previous["account"].as_str().map(str::to_owned),
        Some(account) => account.clone(),
    };
    let mut hosts = current.as_object().cloned().unwrap_or_default();
    hosts.remove(host);
    if !(enabled && account.is_none()) {
        let mut choice = json!({"enabled":enabled});
        if let Some(account) = account {
            choice["account"] = json!(account);
        }
        hosts.insert(host.to_owned(), choice);
    }
    Value::Object(hosts)
}
/// The stale pin: a chosen login gh no longer holds.
pub fn stale_pin(choice: &Value, group: &HostGroup) -> bool {
    choice["account"].as_str().is_some_and(|account| !group.selectable.iter().any(|entry| entry == account))
}

// ---- credentials ----

pub const DEFAULT_GITHUB_HOST: &str = "github.com";
/// Where to create a token for a host: GitHub's own page, or the same path on an Enterprise host.
pub fn new_github_token_url(host: &str) -> String {
    format!("https://{host}/settings/personal-access-tokens/new")
}
pub fn normalize_host(host: &str) -> String {
    host.trim().to_lowercase()
}
/// Hosts with a token saved, in the order settings hold them.
pub fn saved_token_hosts(tokens: &Value) -> Vec<String> {
    tokens.as_object().map(|tokens| tokens.iter().filter(|(_, token)| token.as_str().is_some_and(|token| !token.is_empty())).map(|(host, _)| host.clone()).collect()).unwrap_or_default()
}
pub fn github_token_patch(host: &str, token: &str) -> Value {
    json!({"patch":{"github":{"tokens":{host:token}}}})
}
pub fn github_hosts_patch(hosts: Value) -> Value {
    json!({"patch":{"github":{"hosts":hosts}}})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    AccessToken,
    ApiToken,
}
impl Credential {
    pub fn label(self) -> &'static str {
        match self {
            Self::AccessToken => "Access token",
            Self::ApiToken => "API token",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::AccessToken => "Scoped to one repository, project, or workspace. Create it in that item's Bitbucket settings.",
            Self::ApiToken => "Uses your Atlassian account, so it reaches every repository you can. Give it read and write access to repositories and pull requests, and read:user:bitbucket.",
        }
    }
    pub fn link(self) -> (&'static str, &'static str) {
        match self {
            Self::AccessToken => ("https://support.atlassian.com/bitbucket-cloud/docs/access-tokens/", "Learn more"),
            Self::ApiToken => ("https://id.atlassian.com/manage-profile/security/api-tokens", "Create an API token"),
        }
    }
}
/// The method the saved credentials use, from the settings' `bitbucket` object (tokens arrive
/// redacted: set or empty).
pub fn saved_credential(saved: &Value) -> Option<Credential> {
    let set = |name: &str| saved[name].as_str().is_some_and(|value| !value.is_empty());
    if set("accessToken") {
        Some(Credential::AccessToken)
    } else if set("email") && set("apiToken") {
        Some(Credential::ApiToken)
    } else {
        None
    }
}

/// The Bitbucket form as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BitbucketDraft {
    pub choice: Option<Credential>,
    pub access_token: String,
    pub email: Option<String>,
    pub api_token: String,
}
impl BitbucketDraft {
    pub fn method(&self, saved: &Value) -> Credential {
        self.choice.or_else(|| saved_credential(saved)).unwrap_or(Credential::AccessToken)
    }
    pub fn email(&self, saved: &Value) -> String {
        self.email.clone().unwrap_or_else(|| saved["email"].as_str().unwrap_or_default().to_owned())
    }
    /// The `bitbucket` patch this draft saves, or None while it is incomplete. Saving one method
    /// clears the other, so a hidden credential never wins over the visible one.
    pub fn patch(&self, saved: &Value) -> Option<Value> {
        let method = self.method(saved);
        let new_access = self.access_token.trim();
        let new_api = self.api_token.trim();
        let email = self.email(saved).trim().to_owned();
        match method {
            Credential::AccessToken => (!new_access.is_empty()).then(|| json!({"accessToken":new_access,"email":"","apiToken":""})),
            Credential::ApiToken => {
                let saved_api = saved["apiToken"].as_str().unwrap_or_default();
                // Resending the saved token's redacted value keeps it.
                (!email.is_empty() && (!new_api.is_empty() || !saved_api.is_empty())).then(|| json!({"accessToken":"","email":email,"apiToken": if new_api.is_empty() { saved_api } else { new_api }}))
            }
        }
    }
    pub fn can_save(&self, saved: &Value) -> bool {
        let method = self.method(saved);
        let saved_here = saved_credential(saved) == Some(method);
        self.patch(saved).is_some() && (method == Credential::AccessToken || !saved_here || !self.api_token.trim().is_empty() || self.email(saved).trim() != saved["email"].as_str().unwrap_or_default())
    }
}
pub fn bitbucket_patch(bitbucket: Value) -> Value {
    json!({"patch":{"bitbucket":bitbucket}})
}
pub fn bitbucket_footer(saved: &Value, draft: &BitbucketDraft) -> Option<String> {
    match saved_credential(saved) {
        None => Some("Without a saved token, the server falls back to its T3CODE_BITBUCKET_* environment variables.".into()),
        Some(current) if current == draft.method(saved) => None,
        Some(current) => Some(format!("Saving replaces your {}.", current.label().to_lowercase())),
    }
}

// ---- adding a project by cloning it ----

/// Where a clone can come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    Url,
    Github,
    Gitlab,
    Bitbucket,
    AzureDevops,
}
impl Source {
    pub const PROVIDERS: [Source; 4] = [Self::Github, Self::Gitlab, Self::Bitbucket, Self::AzureDevops];
    pub fn label(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Gitlab => "GitLab",
            Self::Bitbucket => "Bitbucket",
            Self::AzureDevops => "Azure DevOps",
            Self::Url => "Git URL",
        }
    }
    pub fn path_hint(self) -> &'static str {
        match self {
            Self::Github => "owner/repo",
            Self::Gitlab => "group/project",
            Self::Bitbucket => "workspace/repository",
            Self::AzureDevops => "project/repository",
            Self::Url => "URL",
        }
    }
    pub fn provider(self) -> Option<SourceControlProviderKind> {
        match self {
            Self::Url => None,
            Self::Github => Some(SourceControlProviderKind::Github),
            Self::Gitlab => Some(SourceControlProviderKind::Gitlab),
            Self::Bitbucket => Some(SourceControlProviderKind::Bitbucket),
            Self::AzureDevops => Some(SourceControlProviderKind::AzureDevops),
        }
    }
    pub fn value(self) -> &'static str {
        match self {
            Self::Url => "url",
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Bitbucket => "bitbucket",
            Self::AzureDevops => "azure-devops",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Url].into_iter().chain(Self::PROVIDERS).find(|source| source.value() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    pub ready: bool,
    pub hint: Option<String>,
}
/// `buildAddProjectRemoteSourceReadiness`.
pub fn readiness(discovery: Option<&SourceControlDiscoveryResult>) -> BTreeMap<Source, Readiness> {
    let unavailable = Readiness { ready: false, hint: Some("Provider status unavailable. Open Source Control settings and rescan.".into()) };
    let mut map = BTreeMap::from([(Source::Url, Readiness { ready: true, hint: None })]);
    for source in Source::PROVIDERS {
        let provider = discovery.and_then(|discovery| discovery.source_control_providers.iter().find(|provider| Some(provider.kind) == source.provider()));
        let ready = match provider {
            None => unavailable.clone(),
            Some(provider) if provider.status != SourceControlDiscoveryStatus::Available => Readiness { ready: false, hint: Some(provider.install_hint.clone()) },
            Some(provider) if provider.auth.status == SourceControlProviderAuthStatus::Unauthenticated => Readiness {
                ready: false,
                hint: Some(provider.auth.detail.clone().unwrap_or_else(|| format!("{} is not authenticated. Open Source Control settings for setup guidance.", provider.label))),
            },
            Some(_) => Readiness { ready: true, hint: None },
        };
        map.insert(source, ready);
    }
    map
}
/// `sortAddProjectProviderSources`: ready ones first, then by name.
pub fn sorted_providers(readiness: &BTreeMap<Source, Readiness>) -> Vec<Source> {
    let mut sources = Source::PROVIDERS.to_vec();
    sources.sort_by(|left, right| readiness[right].ready.cmp(&readiness[left].ready).then(left.label().cmp(right.label())));
    sources
}

/// `normalizePastedCloneUrl`: the common owner/repository shorthand is a public GitHub HTTPS URL.
pub fn normalize_pasted_clone_url(input: &str) -> String {
    static SHORTHAND: OnceLock<Regex> = OnceLock::new();
    let trimmed = input.trim();
    if !SHORTHAND.get_or_init(|| Regex::new(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})/[A-Za-z0-9._-]+(?:\.git)?$").expect("shorthand")).is_match(trimmed) {
        return trimmed.to_owned();
    }
    let repository = if trimmed.ends_with(".git") { trimmed.to_owned() } else { format!("{trimmed}.git") };
    format!("https://github.com/{repository}")
}
/// `getDefaultCloneUrl`: GitHub and Forgejo default to HTTPS; other providers keep SSH.
pub fn default_clone_url(repository: &SourceControlRepositoryInfo) -> String {
    if matches!(repository.provider, SourceControlProviderKind::Github | SourceControlProviderKind::Forgejo) { repository.url.clone() } else { repository.ssh_url.clone() }
}
/// `getCloneDirectoryName`: the folder `git clone` would pick, from a looked-up repository or a
/// pasted clone URL in any of its forms.
pub fn clone_directory_name(repository_or_url: &str) -> String {
    static HOST_FORM: OnceLock<Regex> = OnceLock::new();
    static SEPARATORS: OnceLock<Regex> = OnceLock::new();
    let without_query = repository_or_url.split(['?', '#']).next().unwrap_or_default().trim();
    let scheme = without_query.find("://");
    // A remote URL carries a host before the repository path. The host is never the repository,
    // so a link that stops at the host, or at a port, names nothing.
    let has_host = scheme.is_some() || HOST_FORM.get_or_init(|| Regex::new(r"^[^/\\:]+@[^/\\:]+:").expect("host form")).is_match(without_query);
    let path = scheme.map_or(without_query, |index| &without_query[index + 3..]);
    let segments: Vec<&str> = SEPARATORS.get_or_init(|| Regex::new(r"[/\\:]+").expect("separators")).split(path).filter(|segment| !segment.trim().is_empty()).collect();
    if has_host && segments.len() < 2 {
        return String::new();
    }
    let last = segments.last().map_or("", |segment| segment.trim());
    // A port can only sit directly behind the authority; deeper numeric segments are path.
    if has_host && segments.len() == 2 && !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
        return String::new();
    }
    last.strip_suffix(".git").unwrap_or(last).to_owned()
}
/// `getCloneDestinationPath`: the picked directory plus the repository folder inside it.
pub fn clone_destination_path(directory: &str, name: &str) -> String {
    let name = name.trim();
    if name.is_empty() { directory.to_owned() } else { format!("{}{name}", project_folder::directory(directory)) }
}

fn windows_drive(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes.len() == 2 || matches!(bytes[2], b'/' | b'\\'))
}
fn windows_absolute(value: &str) -> bool {
    value.starts_with("\\\\") || windows_drive(value)
}
/// `normalizeProjectPathForDispatch`: trimmed, without trailing separators (a root keeps its own).
pub fn normalize_project_path(value: &str) -> String {
    let value = value.trim();
    let root = value == "/" || value == "\\" || (value.len() == 3 && windows_drive(value) && value.as_bytes()[2] != b':');
    if value.is_empty() || root {
        return value.to_owned();
    }
    let trimmed = if value.starts_with('/') { value.trim_end_matches('/') } else { value.trim_end_matches(['/', '\\']) };
    if trimmed.is_empty() {
        return value.to_owned();
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' { format!("{trimmed}\\") } else { trimmed.to_owned() }
}
/// `inferProjectTitleFromPath`: the last folder of a path; a bare root is its own title.
pub fn project_title(path: &str) -> String {
    let normalized = normalize_project_path(path);
    let separators = |text: &str| -> Vec<String> { text.split(['/', '\\']).filter(|segment| !segment.is_empty()).map(str::to_owned).collect() };
    let segments = if windows_drive(&normalized) {
        separators(normalized.get(3..).unwrap_or_default())
    } else if normalized.starts_with("\\\\") {
        let all = separators(&normalized);
        if all.len() < 2 { separators(&normalized) } else { all[2..].to_vec() }
    } else if normalized.starts_with('/') {
        separators(&normalized[1..])
    } else {
        separators(&normalized)
    };
    segments.last().cloned().unwrap_or(normalized)
}
/// What `isExplicitRelativeProjectPath` calls relative.
pub fn explicit_relative(value: &str) -> bool {
    matches!(value, "." | "..") || ["./", "../", ".\\", "..\\"].iter().any(|prefix| value.starts_with(prefix))
}
/// The destination a clone is sent to, or why it cannot be: a Windows path on a machine that is
/// not Windows, and a relative path (there is no open project to resolve it against).
pub fn validate_destination(raw: &str, platform_os: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Enter a destination path.".into());
    }
    let windows = platform_os.to_lowercase().starts_with("win");
    if windows_absolute(raw) && !windows {
        return Err("Windows-style paths are only supported on Windows.".into());
    }
    if explicit_relative(raw) {
        return Err("Relative paths need an active project; enter the full path.".into());
    }
    Ok(normalize_project_path(raw))
}

/// The `sourceControl.lookupRepository` payload.
pub fn lookup_input(source: Source, repository: &str) -> Option<Value> {
    Some(json!({"provider":source.provider()?.as_str(),"repository":repository.trim()}))
}
/// The `sourceControl.cloneRepository` payload; the provider and repository are not sent, only
/// the URL the lookup chose, as the original does.
pub fn clone_input(remote_url: &str, destination: &str) -> Value {
    json!({"remoteUrl":remote_url,"destinationPath":destination})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovery(value: Value) -> SourceControlDiscoveryResult {
        serde_json::from_value(value).unwrap()
    }
    fn provider(kind: &str, status: &str, executable: Option<&str>, auth: Value) -> Value {
        let mut item = json!({"kind":kind,"label":match kind {"github"=>"GitHub","gitlab"=>"GitLab","azure-devops"=>"Azure DevOps","bitbucket"=>"Bitbucket",_=>"Forgejo"},"status":status,"version":{"_tag":"Some","value":"1.0"},"installHint":"Install it.","detail":{"_tag":"None"},"auth":auth});
        if let Some(executable) = executable {
            item["executable"] = json!(executable);
        }
        item
    }
    fn auth(status: &str, account: Option<&str>, detail: Option<&str>) -> Value {
        let option = |value: Option<&str>| value.map_or(json!({"_tag":"None"}), |value| json!({"_tag":"Some","value":value}));
        json!({"status":status,"account":option(account),"host":{"_tag":"None"},"detail":option(detail)})
    }
    fn text(parts: Vec<Part>) -> String {
        parts.into_iter().map(|part| match part {
            Part::Text(text) => text,
            Part::Code(code) => format!("`{code}`"),
            Part::Account(account) => format!("<{account}>"),
        }).collect::<Vec<_>>().join(" ")
    }

    /// Rows the original produced: `node tools/generate_client_source_control_fixtures.mjs`.
    #[test]
    fn the_original_redaction_logins_hosts_and_clone_paths_are_reproduced() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/source-control-settings.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let s = |name: &str| row[name].as_str().unwrap();
            match s("type") {
                "redacted" => assert_eq!(redacted_placeholder(s("value")), s("placeholder"), "{row}"),
                "groups" => {
                    let accounts: Vec<SourceControlProviderAccount> = serde_json::from_value(row["accounts"].clone()).unwrap();
                    let groups: Vec<Value> = group_github_accounts(&accounts)
                        .into_iter()
                        .map(|group| json!({"host":group.host,"activeAccount":group.active_account,"selectable":group.selectable,"broken":group.broken,"environmentVariable":group.environment_variable}))
                        .collect();
                    assert_eq!(Value::Array(groups), row["groups"], "{row}");
                }
                "hosts" => {
                    let change = HostChange {
                        enabled: row["change"]["enabled"].as_bool(),
                        account: row["change"].get("account").map(|account| account.as_str().map(str::to_owned)),
                    };
                    assert_eq!(next_github_hosts(&row["current"], s("host"), &change), row["next"], "{row}");
                }
                "directory" => assert_eq!(clone_directory_name(s("input")), s("name"), "{row}"),
                "destination" => assert_eq!(clone_destination_path(s("directory"), row["name"].as_str().unwrap_or_default()), s("path"), "{row}"),
                "pasted" => assert_eq!(normalize_pasted_clone_url(s("input")), s("url"), "{row}"),
                "default-url" => {
                    let repository: SourceControlRepositoryInfo = serde_json::from_value(row["repository"].clone()).unwrap();
                    assert_eq!(default_clone_url(&repository), s("url"), "{row}");
                }
                "readiness" => {
                    let result = (!row["discovery"].is_null()).then(|| discovery(row["discovery"].clone()));
                    let actual = readiness(result.as_ref());
                    for source in [Source::Url].into_iter().chain(Source::PROVIDERS) {
                        let expected = &row["readiness"][source.value()];
                        assert_eq!((actual[&source].ready, actual[&source].hint.as_deref()), (expected["ready"].as_bool().unwrap(), expected["hint"].as_str()), "{} in {row}", source.value());
                    }
                    let order: Vec<&str> = sorted_providers(&actual).into_iter().map(Source::value).collect();
                    // The original also lists Forgejo, which this server does not support.
                    let expected: Vec<Value> = row["order"].as_array().unwrap().iter().filter(|source| *source != "forgejo").cloned().collect();
                    assert_eq!(Value::from(order), Value::from(expected), "{row}");
                }
                "path" => {
                    assert_eq!(project_title(s("input")), s("title"), "{row}");
                    assert_eq!(normalize_project_path(s("input")), s("normalized"), "{row}");
                    assert_eq!(explicit_relative(s("input").trim()), row["relative"].as_bool().unwrap(), "{row}");
                }
                "windows" => {
                    let refused = validate_destination(s("input"), s("platform")).err().is_some_and(|message| message.starts_with("Windows-style"));
                    assert_eq!(refused, row["unsupported"].as_bool().unwrap(), "{row}");
                }
                "labels" => {
                    let source = Source::parse(s("source")).unwrap();
                    assert_eq!((source.label(), source.path_hint()), (s("label"), s("hint")));
                }
                other => panic!("unknown fixture row {other}"),
            }
            checked += 1;
        }
        assert!(checked > 100, "{checked}");
    }

    #[test]
    fn a_row_says_what_the_original_says_for_each_state_of_a_tool_or_host() {
        let result = discovery(json!({
            "versionControlSystems":[
                {"kind":"git","implemented":true,"label":"Git","executable":"git","status":"available","version":{"_tag":"Some","value":"git version 2.50"},"installHint":"Install Git.","detail":{"_tag":"None"}},
                {"kind":"jj","implemented":false,"label":"Jujutsu","executable":"jj","status":"missing","version":{"_tag":"None"},"installHint":"Install Jujutsu.","detail":{"_tag":"None"}}
            ],
            "sourceControlProviders":[
                provider("github","available",Some("gh"),auth("authenticated",Some("octo"),Some("Using the token saved in Settings."))),
                provider("gitlab","available",Some("glab"),auth("unauthenticated",None,None)),
                provider("azure-devops","missing",Some("az"),auth("unknown",None,Some("not found"))),
                provider("bitbucket","available",None,auth("unauthenticated",None,None)),
                provider("forgejo","available",Some("fj"),auth("authenticated",Some("x"),None)),
            ]
        }));
        let (vcs, providers) = rows(&result);
        assert_eq!(providers.len(), 4, "Forgejo is not listed");
        assert_eq!((vcs[0].dot(), vcs[0].badge(), text(vcs[0].summary()), vcs[0].version(), vcs[0].enabled()), (Dot::Ok, None, "Available".to_owned(), Some("git version 2.50"), true));
        assert_eq!((vcs[1].dot(), vcs[1].badge(), text(vcs[1].summary()), vcs[1].enabled()), (Dot::Off, Some("Coming Soon"), "Support for Jujutsu is coming soon.".to_owned(), false));
        assert_eq!(text(providers[0].summary()), "Authenticated as <octo> \u{b7} Using the token saved in Settings.");
        assert_eq!((providers[0].dot(), providers[0].badge(), providers[0].enabled(), providers[0].account()), (Dot::Ok, None, true, Some("octo")));
        assert_eq!(text(providers[1].summary()), "GitLab is not authenticated on this server. Sign in or configure credentials using the `glab` tool on the server host to enable change request features.");
        assert_eq!((providers[1].dot(), providers[1].badge(), providers[1].enabled()), (Dot::Warning, Some("Not authenticated"), false));
        assert_eq!(text(providers[2].summary()), "Not available on this server: Install it.");
        assert_eq!((providers[2].dot(), providers[2].badge()), (Dot::Warning, None));
        assert_eq!(text(providers[3].summary()), "Available. Install it.", "an API integration has no CLI to sign in with");
        assert_eq!(providers[3].key(), "provider:bitbucket");
        assert_eq!(vcs[0].key(), "vcs:git");
        // An unreadable status reads as unverified, and every login turned off says so.
        let unknown = discovery(json!({"versionControlSystems":[],"sourceControlProviders":[provider("gitlab","available",Some("glab"),auth("unknown",None,Some("timed out")))]}));
        assert_eq!(text(rows(&unknown).1[0].summary()), "Could not verify GitLab. timed out");
        let mut off = provider("github","available",Some("gh"),auth("unauthenticated",None,None));
        off["auth"]["accounts"] = json!([{"host":"github.com","account":"octo","active":true,"authenticated":true}]);
        let off = discovery(json!({"versionControlSystems":[],"sourceControlProviders":[off]}));
        assert_eq!(text(rows(&off).1[0].summary()), "Every GitHub host is turned off.");
        assert!(decode_discovery(&json!({"versionControlSystems":"no"})).is_err());
    }

    #[test]
    fn a_login_is_hidden_behind_a_stand_in_of_the_same_shape() {
        let hidden = redacted_placeholder("tim.r_ferguson@example.com");
        assert_eq!(hidden.len(), 26);
        for (shown, original) in hidden.chars().zip("tim.r_ferguson@example.com".chars()) {
            assert!(if "@.-_".contains(original) { shown == original } else { REDACTED_ALPHABET.contains(&(shown as u8)) });
        }
        assert_eq!(hidden, redacted_placeholder("tim.r_ferguson@example.com"));
        assert_ne!(hidden, redacted_placeholder("tim.r_ferguson@example.org"));
    }

    #[test]
    fn bitbucket_saves_one_method_clears_the_other_and_keeps_a_saved_api_token_when_only_the_email_changes() {
        let none = json!({"email":"","accessToken":"","apiToken":""});
        let mut draft = BitbucketDraft::default();
        assert_eq!((draft.method(&none), draft.patch(&none), draft.can_save(&none)), (Credential::AccessToken, None, false));
        draft.access_token = " tok ".into();
        assert_eq!(draft.patch(&none), Some(json!({"accessToken":"tok","email":"","apiToken":""})));
        assert!(draft.can_save(&none));
        assert_eq!(bitbucket_footer(&none, &draft).as_deref(), Some("Without a saved token, the server falls back to its T3CODE_BITBUCKET_* environment variables."));

        let saved = json!({"email":"me@example.com","accessToken":"","apiToken":"••••••"});
        let mut draft = BitbucketDraft::default();
        assert_eq!(saved_credential(&saved), Some(Credential::ApiToken));
        assert_eq!(draft.method(&saved), Credential::ApiToken, "the saved method is the starting point");
        assert!(!draft.can_save(&saved), "nothing changed");
        draft.email = Some("you@example.com".into());
        assert_eq!(draft.patch(&saved), Some(json!({"accessToken":"","email":"you@example.com","apiToken":"••••••"})), "the redacted value keeps the stored token");
        assert!(draft.can_save(&saved));
        draft.email = None;
        draft.api_token = "new".into();
        assert_eq!(draft.patch(&saved).unwrap()["apiToken"], "new");
        // Choosing the other method previews that saving replaces the saved one.
        draft.choice = Some(Credential::AccessToken);
        assert_eq!(bitbucket_footer(&saved, &draft).as_deref(), Some("Saving replaces your api token."));
        assert!(!draft.can_save(&saved));
        draft.access_token = "t".into();
        assert!(draft.can_save(&saved));
        assert_eq!(saved_credential(&json!({"email":"x","accessToken":"","apiToken":""})), None, "an email alone is not a credential");
    }

    #[test]
    fn github_tokens_and_hosts_are_written_as_the_original_writes_them() {
        assert_eq!(github_token_patch("github.com", "ghp_x"), json!({"patch":{"github":{"tokens":{"github.com":"ghp_x"}}}}));
        assert_eq!(saved_token_hosts(&json!({"github.com":"••••••","ghe.example.com":""})), ["github.com"]);
        assert_eq!(normalize_host("  GitHub.Example.COM "), "github.example.com");
        assert_eq!(new_github_token_url("ghe.example.com"), "https://ghe.example.com/settings/personal-access-tokens/new");
        let group = HostGroup { host: "github.com".into(), active_account: None, selectable: vec!["octo".into()], broken: vec![], environment_variable: None };
        assert!(stale_pin(&json!({"account":"gone"}), &group));
        assert!(!stale_pin(&json!({"account":"octo"}), &group));
        assert!(!stale_pin(&json!({"enabled":false}), &group));
    }

    #[test]
    fn a_clone_goes_into_the_folder_git_would_pick_and_a_pasted_shorthand_means_github() {
        assert_eq!(clone_directory_name("git@github.com:acme/web.git"), "web");
        assert_eq!(clone_directory_name("https://host/acme/123"), "123");
        assert_eq!(clone_directory_name("https://host:8080"), "");
        assert_eq!(clone_directory_name("acme/web"), "web");
        assert_eq!(clone_destination_path("/home/me/code", "web"), "/home/me/code/web");
        assert_eq!(clone_destination_path("/home/me/code", " "), "/home/me/code");
        assert_eq!(normalize_pasted_clone_url(" acme/web "), "https://github.com/acme/web.git");
        assert_eq!(normalize_pasted_clone_url("git@host:acme/web.git"), "git@host:acme/web.git");
        assert_eq!(lookup_input(Source::Github, " acme/web ").unwrap(), json!({"provider":"github","repository":"acme/web"}));
        assert_eq!(lookup_input(Source::Url, "x"), None);
        assert_eq!(clone_input("u", "/d"), json!({"remoteUrl":"u","destinationPath":"/d"}));
        assert_eq!(project_title("/home/me/code/web/"), "web");
        assert_eq!(project_title("C:\\work\\repo"), "repo");
        assert_eq!(project_title("/"), "/");
        assert_eq!(normalize_project_path("  /a/b//  "), "/a/b");
        assert_eq!(normalize_project_path("C:"), "C:\\");
        assert_eq!(normalize_project_path("C:/"), "C:/");
        assert_eq!(validate_destination(" ~/code/web/ ", "darwin").as_deref(), Ok("~/code/web"));
        assert_eq!(validate_destination("C:\\code", "linux").unwrap_err(), "Windows-style paths are only supported on Windows.");
        assert_eq!(validate_destination("C:\\code", "win32").as_deref(), Ok("C:\\code"));
        assert_eq!(validate_destination("../web", "linux").unwrap_err(), "Relative paths need an active project; enter the full path.");
        assert_eq!(validate_destination("  ", "linux").unwrap_err(), "Enter a destination path.");
        assert_eq!(Source::parse("azure-devops"), Some(Source::AzureDevops));
        assert_eq!(Source::parse("forgejo"), None);
    }
}
