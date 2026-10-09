//! Which source control host a remote URL belongs to (port of the detection half of
//! `packages/shared/src/sourceControl.ts`), plus the small identifier helpers every host shares
//! (`SourceControlProvider.ts`). Pure; the registry decides what to do with the answer.
//!
//! Detection looks at the host name only. A host is GitHub, GitLab, Bitbucket or Forgejo when a
//! whole DNS label says so (`git.github.acme.test`), Azure DevOps by its fixed domains, and
//! anything else is `unknown` and may be refined later by asking a signed-in CLI (see
//! `source_control_registry`). A dotted alias is never trusted to name a host it does not.
use regex::Regex;
use std::sync::OnceLock;
use t3_contracts::{SourceControlProviderInfo, SourceControlProviderKind, trim_wire_string};

fn scp_remote() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^[a-zA-Z0-9._-]+@([^:/]+):").unwrap())
}

/// An SCP-style (`git@host:path`) or `ssh://` remote.
pub fn is_ssh_remote_url(remote_url: &str) -> bool {
    let trimmed = trim_wire_string(remote_url);
    scp_remote().is_match(trimmed) || trimmed.to_lowercase().starts_with("ssh://")
}

/// JavaScript's `URL.host`: the hostname plus a non-default port.
pub fn js_host(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// The normalized host used for detection. SCP-style and SSH remotes return the hostname only;
/// other schemes keep an explicit port, which can name a self-hosted web endpoint.
fn parse_remote_host(remote_url: &str) -> Option<String> {
    let trimmed = trim_wire_string(remote_url);
    if trimmed.is_empty() {
        return None;
    }
    if let Some(captures) = scp_remote().captures(trimmed)
        && let Some(host) = captures.get(1)
    {
        return Some(host.as_str().to_lowercase());
    }
    let url = url::Url::parse(trimmed).ok()?;
    Some(
        if url.scheme() == "ssh" {
            url.host_str().unwrap_or("").to_string()
        } else {
            js_host(&url)
        }
        .to_lowercase(),
    )
}

fn parse_host_name(host: &str) -> String {
    match url::Url::parse(&format!("https://{host}")) {
        Ok(url) => url.host_str().unwrap_or("").to_lowercase(),
        Err(_) => {
            static PORT: OnceLock<Regex> = OnceLock::new();
            PORT.get_or_init(|| Regex::new(r":\d+$").unwrap())
                .replace(host, "")
                .to_lowercase()
        }
    }
}

fn has_dns_label(host: &str, label: &str) -> bool {
    host.split('.').any(|part| part == label)
}

fn is_azure_dev_ops_host(host: &str) -> bool {
    // `ssh.dev.azure.com` is the default SSH clone host, so any `*.dev.azure.com` matches;
    // legacy hosts stay under `.visualstudio.com` (including `vs-ssh.visualstudio.com`).
    host == "dev.azure.com" || host.ends_with(".dev.azure.com") || host.ends_with(".visualstudio.com")
}

fn info(kind: SourceControlProviderKind, name: &str, base_url: String) -> SourceControlProviderInfo {
    SourceControlProviderInfo {
        kind,
        name: name.to_string(),
        base_url,
    }
}

/// `detectSourceControlProviderFromRemoteUrl`: `None` when the URL names no host at all.
pub fn detect_provider_from_remote_url(remote_url: &str) -> Option<SourceControlProviderInfo> {
    use SourceControlProviderKind::*;
    let host = parse_remote_host(remote_url)?;
    if host.is_empty() {
        return None;
    }
    let hostname = parse_host_name(&host);
    let base = format!("https://{host}");

    if hostname == "codeberg.org" || has_dns_label(&hostname, "forgejo") || has_dns_label(&hostname, "gitea") {
        let trimmed = trim_wire_string(remote_url);
        let web = trimmed.to_lowercase();
        let base_url = if web.starts_with("http:") || web.starts_with("https:") {
            url::Url::parse(trimmed)
                .map(|url| url.origin().ascii_serialization())
                .unwrap_or(base)
        } else {
            base
        };
        return Some(info(Forgejo, "Forgejo", base_url));
    }
    if hostname == "github.com" || has_dns_label(&hostname, "github") {
        let name = if hostname == "github.com" { "GitHub" } else { "GitHub Self-Hosted" };
        return Some(info(Github, name, base));
    }
    if hostname == "gitlab.com" || has_dns_label(&hostname, "gitlab") {
        let name = if hostname == "gitlab.com" { "GitLab" } else { "GitLab Self-Hosted" };
        return Some(info(Gitlab, name, base));
    }
    if is_azure_dev_ops_host(&hostname) {
        return Some(info(AzureDevops, "Azure DevOps", base));
    }
    if hostname == "bitbucket.org" || has_dns_label(&hostname, "bitbucket") {
        let name = if hostname == "bitbucket.org" { "Bitbucket" } else { "Bitbucket Self-Hosted" };
        return Some(info(Bitbucket, name, base));
    }
    Some(info(Unknown, &host, base))
}

/// What `sourceControlRepositorySelector` reads from a repository identity.
#[derive(Debug, Clone, Default)]
pub struct RepositoryIdentityParts {
    pub provider: Option<SourceControlProviderKind>,
    pub display_name: Option<String>,
    pub owner: Option<String>,
    pub name: Option<String>,
}

/// The provider-native repository selector. `display_name` is the full path below the host,
/// which nested GitLab groups need; owner/name is the fallback for identities recorded before
/// that field existed. Azure DevOps takes the repository name alone (the organisation and
/// project come from the checkout).
pub fn repository_selector(identity: Option<&RepositoryIdentityParts>) -> Option<String> {
    let identity = identity?;
    if identity.provider == Some(SourceControlProviderKind::AzureDevops) {
        let display = identity.display_name.clone().unwrap_or_default();
        let last = display.split('/').filter(|part| *part != "_git").next_back();
        return identity
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .or_else(|| last.filter(|last| !last.is_empty()).map(str::to_string));
    }
    if let Some(display) = identity.display_name.as_ref().filter(|display| !display.is_empty()) {
        return Some(display.clone());
    }
    match (&identity.owner, &identity.name) {
        (Some(owner), Some(name)) if !owner.is_empty() && !name.is_empty() => {
            Some(format!("{owner}/{name}"))
        }
        _ => None,
    }
}

/// `canonicalRepositoryKey`: Azure DevOps SSH and legacy `visualstudio.com` spellings map to
/// the `dev.azure.com` web form.
pub fn canonical_repository_key(key: &str) -> String {
    static SSH: OnceLock<Regex> = OnceLock::new();
    static LEGACY: OnceLock<Regex> = OnceLock::new();
    let ssh = SSH.get_or_init(|| {
        Regex::new(r"^(?:ssh\.dev\.azure\.com|vs-ssh\.visualstudio\.com)/v3/([^/]+)/([^/]+)/([^/]+)$").unwrap()
    });
    let legacy = LEGACY.get_or_init(|| {
        Regex::new(r"^([^.]+)\.visualstudio\.com/(?:defaultcollection/)?([^/]+)/_git/([^/]+)$").unwrap()
    });
    let rewritten = ssh.replace(key, "dev.azure.com/$1/$2/_git/$3").into_owned();
    legacy
        .replace(&rewritten, "dev.azure.com/$1/$2/_git/$3")
        .into_owned()
}

const MAX_ERROR_TRANSPORT_VALUE_LENGTH: usize = 256;

/// `transportSafeSourceControlErrorValue`: drops control characters, URL credentials, query
/// and fragment, and bounds the length (in UTF-16 units, like the original).
pub fn transport_safe_value(value: &str) -> String {
    let printable: String = value
        .chars()
        .map(|c| if (c as u32) < 32 || c as u32 == 127 { ' ' } else { c })
        .collect();
    let trimmed = trim_wire_string(&printable);
    let normalized = trimmed.split_whitespace_js().join(" ");
    let safe = match url::Url::parse(&normalized) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => normalized,
    };
    let units: Vec<u16> = safe.encode_utf16().take(MAX_ERROR_TRANSPORT_VALUE_LENGTH).collect();
    String::from_utf16_lossy(&units)
}

/// JavaScript `\s+` splitting, which counts U+FEFF as whitespace and Rust's `char::is_whitespace`
/// does not.
trait SplitJsWhitespace {
    fn split_whitespace_js(&self) -> Vec<&str>;
}

fn is_js_whitespace(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

impl SplitJsWhitespace for str {
    fn split_whitespace_js(&self) -> Vec<&str> {
        self.split(is_js_whitespace).filter(|part| !part.is_empty()).collect()
    }
}

/// `parseSourceControlOwnerRef`: `owner:branch` for a fork's head.
pub fn parse_owner_ref(head_selector: &str) -> Option<(String, String)> {
    let trimmed = trim_wire_string(head_selector);
    let (owner, ref_name) = trimmed.split_once(':')?;
    let owner_ok = !owner.is_empty() && !owner.contains('/') && !owner.chars().any(is_js_whitespace);
    let ref_ok = !ref_name.is_empty()
        && !ref_name
            .chars()
            .any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'));
    if !owner_ok || !ref_ok {
        return None;
    }
    let (owner, ref_name) = (trim_wire_string(owner), trim_wire_string(ref_name));
    (!owner.is_empty() && !ref_name.is_empty()).then(|| (owner.to_string(), ref_name.to_string()))
}

/// A branch, owner and repository naming one side of a change request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RefSelector {
    pub ref_name: String,
    pub owner: Option<String>,
    pub repository: Option<String>,
}

/// `sourceBranch`: the explicit source's ref, else the branch of `owner:branch`, else the
/// selector itself.
pub fn source_branch(head_selector: &str, source: Option<&RefSelector>) -> String {
    match source {
        Some(source) => source.ref_name.clone(),
        None => parse_owner_ref(head_selector)
            .map(|(_, ref_name)| ref_name)
            .unwrap_or_else(|| trim_wire_string(head_selector).to_string()),
    }
}

/// `sourceControlRefFromInput`.
pub fn source_ref_from_input(head_selector: &str, source: Option<&RefSelector>) -> Option<RefSelector> {
    source.cloned().or_else(|| {
        parse_owner_ref(head_selector).map(|(owner, ref_name)| RefSelector {
            ref_name,
            owner: Some(owner),
            repository: None,
        })
    })
}
