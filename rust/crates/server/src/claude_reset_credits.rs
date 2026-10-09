//! Claude banked resets (the CLI's `cedar_ember` program): `claudeResetCredits.ts` and the redemption
//! lock of `resetCreditCoordinator.ts`. The CLI reads the grants from the OAuth usage endpoint and
//! claims one against the organization; this does the same with the credentials the CLI keeps in
//! its config directory. macOS keeps them in the keychain, so there the feature is not offered.
//!
//! The API origin is `https://api.anthropic.com`; the instance environment variable
//! `T3CODE_CLAUDE_API_BASE` replaces it so tests can serve a loopback origin, and
//! `T3CODE_HOST_PLATFORM` replaces the platform check.
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

pub const API_BASE: &str = "https://api.anthropic.com";
const PROGRAM: &str = "cedar_ember";

/// Why a redemption failed. All but `RequestFailed` and `Unconfirmed` are final answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    MalformedCredit,
    LoginUnreadable,
    AccountUnreadable,
    SignedOut,
    RateLimited,
    CoolingDown,
    Unconfirmed,
    RequestFailed,
}
impl Failure {
    pub fn message(self) -> &'static str {
        match self {
            Failure::MalformedCredit => "Claude returned a malformed reset credit.",
            Failure::LoginUnreadable => "Claude could not read its login.",
            Failure::AccountUnreadable => "Claude could not read its account.",
            Failure::SignedOut => "Sign in to Claude again to redeem resets.",
            Failure::RateLimited => "Claude is rate limiting resets. Try again soon.",
            Failure::CoolingDown => "Claude resets are cooling down. Try again later.",
            Failure::Unconfirmed => "Claude could not confirm the reset. If you are still limited in a moment, try again.",
            Failure::RequestFailed => "Claude could not redeem the reset.",
        }
    }
    /// `isSettledClaudeResetCreditFailure`: an unanswered or unconfirmed claim retries with the same request id.
    pub fn is_settled(self) -> bool {
        !matches!(self, Failure::RequestFailed | Failure::Unconfirmed)
    }
}

fn host_platform(environment: &HashMap<String, String>) -> String {
    environment.get("T3CODE_HOST_PLATFORM").cloned().unwrap_or_else(|| if cfg!(target_os = "macos") { "darwin".into() } else { std::env::consts::OS.into() })
}
pub fn api_base(environment: &HashMap<String, String>) -> String {
    environment.get("T3CODE_CLAUDE_API_BASE").map(|base| base.trim_end_matches('/').to_owned()).filter(|base| !base.is_empty()).unwrap_or_else(|| API_BASE.into())
}

/// Reads `file` as JSON; a missing file reads as `{}`, as `readJson` does.
fn read_json(file: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.to_string()),
    }
}
fn read_access_token(config_dir: &Path, platform: &str) -> Result<Option<String>, String> {
    if platform == "darwin" {
        return Ok(None);
    }
    let credentials = read_json(&config_dir.join(".credentials.json"))?;
    if !credentials.is_object() || !(credentials["claudeAiOauth"].is_null() || credentials["claudeAiOauth"].is_object()) {
        return Err("The credentials file does not have the expected shape.".into());
    }
    Ok(credentials["claudeAiOauth"]["accessToken"].as_str().map(str::trim).filter(|token| !token.is_empty()).map(str::to_owned))
}

fn headers(request: reqwest::RequestBuilder, token: &str, version: &str) -> reqwest::RequestBuilder {
    request.bearer_auth(token).header("anthropic-beta", "oauth-2025-04-20").header("user-agent", format!("claude-cli/{version} (external, cli)"))
}

/// `readClaudeResetCredits`: any failure reads as "no resets" so the usage bars never break on this
/// optional extra.
pub async fn read_credits(config_dir: &Path, version: &str, environment: &HashMap<String, String>) -> Option<Value> {
    let token = read_access_token(config_dir, &host_platform(environment)).ok().flatten()?;
    let url = format!("{}/api/oauth/usage", api_base(environment));
    let request = headers(client().get(&url).query(&[("cedar_ember", "1"), ("skip_spend", "1")]), &token, version);
    let response = tokio::time::timeout(Duration::from_secs(10), async { request.send().await?.error_for_status()?.json::<Value>().await }).await.ok()?.ok()?;
    if !response.is_object() {
        return None;
    }
    crate::claude_usage_limits::reset_credits_to_contract(&response["cedar_ember"], chrono::Utc::now().timestamp_millis())
}

fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new).clone()
}

/// The CLI keeps the account record beside its settings, or in the home directory by default.
pub fn account_config_path(config_dir: Option<&Path>, home: Option<&Path>) -> PathBuf {
    match config_dir {
        Some(dir) => dir.join(".claude.json"),
        None => home.map(|home| home.join(".claude.json")).unwrap_or_else(|| PathBuf::from(".claude.json")),
    }
}

fn is_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
}
fn is_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

pub struct Claim<'a> {
    pub config_dir: &'a Path,
    pub account_config_path: &'a Path,
    pub version: &'a str,
    pub grant_id: &'a str,
    /// The idempotency key: a retry with the same id is the same claim.
    pub request_id: &'a str,
    pub environment: &'a HashMap<String, String>,
}

/// `consumeClaudeResetCredit`: ids are checked before anything is sent.
pub async fn consume(claim: Claim<'_>) -> Result<&'static str, Failure> {
    if !is_grant_id(claim.grant_id) || !is_request_id(claim.request_id) {
        return Err(Failure::MalformedCredit);
    }
    let token = read_access_token(claim.config_dir, &host_platform(claim.environment)).map_err(|_| Failure::LoginUnreadable)?;
    let account = read_json(claim.account_config_path).map_err(|_| Failure::AccountUnreadable)?;
    if !account.is_object() || !(account["oauthAccount"].is_null() || account["oauthAccount"].is_object()) {
        return Err(Failure::AccountUnreadable);
    }
    let organization = account["oauthAccount"]["organizationUuid"].as_str().map(str::trim).filter(|value| !value.is_empty());
    let (Some(token), Some(organization)) = (token, organization) else { return Err(Failure::SignedOut) };
    let mut url = url::Url::parse(&api_base(claim.environment)).map_err(|_| Failure::RequestFailed)?;
    url.set_path(&format!("/api/organizations/{}/reset_rate_limits", urlencoding_component(organization)));
    let request = headers(client().post(url), &token, claim.version).json(&json!({"program":PROGRAM,"grant_id":claim.grant_id,"request_id":claim.request_id}));
    let response = tokio::time::timeout(Duration::from_secs(25), request.send()).await.map_err(|_| Failure::RequestFailed)?.map_err(|_| Failure::RequestFailed)?;
    match response.status().as_u16() {
        429 => return Err(Failure::RateLimited),
        401 | 403 => return Err(Failure::SignedOut),
        status if !(200..300).contains(&status) => return Err(Failure::RequestFailed),
        _ => {}
    }
    let body = tokio::time::timeout(Duration::from_secs(25), response.json::<Value>()).await.map_err(|_| Failure::RequestFailed)?.map_err(|_| Failure::RequestFailed)?;
    match body["result"].as_str() {
        Some("reset") => Ok("reset"),
        Some("not_limited") => Ok("nothingToReset"),
        Some("already_used") => Ok("alreadyRedeemed"),
        Some("ineligible") => Ok("noCredit"),
        Some("cooldown") => Err(Failure::CoolingDown),
        // Claude could not say whether the claim landed, so, like the CLI, keep the request id.
        Some("unavailable") => Err(Failure::Unconfirmed),
        _ => Err(Failure::RequestFailed),
    }
}
/// `encodeURIComponent`
fn urlencoding_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Redeeming is an account-level action: instances that share the directory holding the login
/// share the credit, so redemptions serialize on that directory and keep one idempotency key until
/// Claude answers.
#[derive(Default)]
struct Account {
    pending_key: Option<String>,
}
fn accounts() -> &'static Mutex<HashMap<String, Arc<tokio::sync::Mutex<Account>>>> {
    static ACCOUNTS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Account>>>>> = OnceLock::new();
    ACCOUNTS.get_or_init(Default::default)
}
/// `ResetCreditCoordinator.redeem`: `consume` runs under the account's lock with a stable key. The
/// key is cleared when Claude reports an outcome or a final failure; any other failure keeps it so
/// the next attempt is the same attempt.
pub async fn redeem<F, Fut>(account_key: &str, consume: F) -> Result<&'static str, Failure>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<&'static str, Failure>>,
{
    let account = accounts().lock().unwrap().entry(account_key.to_owned()).or_default().clone();
    let mut account = account.lock().await;
    let key = account.pending_key.get_or_insert_with(|| uuid::Uuid::new_v4().to_string()).clone();
    let outcome = consume(key).await;
    if outcome.as_ref().err().is_none_or(|failure| failure.is_settled()) {
        account.pending_key = None;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_retry_after_an_unanswered_claim_reuses_its_request_id_and_an_answer_clears_it() {
        let seen = Arc::new(Mutex::new(vec![]));
        let attempt = |outcome: Result<&'static str, Failure>| {
            let seen = seen.clone();
            async move {
                redeem("account-a", move |key| async move {
                    seen.lock().unwrap().push(key);
                    outcome
                })
                .await
            }
        };
        assert_eq!(attempt(Err(Failure::RequestFailed)).await, Err(Failure::RequestFailed));
        assert_eq!(attempt(Err(Failure::Unconfirmed)).await, Err(Failure::Unconfirmed));
        assert_eq!(attempt(Err(Failure::CoolingDown)).await, Err(Failure::CoolingDown));
        assert_eq!(attempt(Ok("reset")).await, Ok("reset"));
        let keys = seen.lock().unwrap().clone();
        assert_eq!(keys[0], keys[1]);
        assert_eq!(keys[1], keys[2], "an unconfirmed claim keeps its key until Claude answers");
        assert_ne!(keys[2], keys[3], "a final failure ends the claim");
    }

    #[test]
    fn ids_are_checked_before_anything_is_sent() {
        assert!(is_grant_id("g_1-a") && !is_grant_id("BAD") && !is_grant_id("") && !is_grant_id(&"a".repeat(41)));
        assert!(is_request_id("AbC_-9") && !is_request_id("a b") && !is_request_id(&"a".repeat(65)));
        assert_eq!(urlencoding_component("a b/ü"), "a%20b%2F%C3%BC");
    }
}
