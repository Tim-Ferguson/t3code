//! The CLI's T3 Connect authorization: Clerk OAuth tokens, kept in the secret store and refreshed
//! five minutes before they expire (`cloud/CliTokenManager.ts`). Two ways to obtain one: the
//! authorization-code flow with PKCE through a loopback listener, and the device authorization
//! grant for machines without a browser (SSH). The browser leg of the first goes through the
//! hosted `/connect` page; this crate never calls Clerk's `/oauth/authorize` itself.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, oneshot};

use crate::clock::Clock;
use crate::config::{CloudCliOAuthConfig, build_connect_authorize_request_url};
use crate::secrets::{CLOUD_CLI_OAUTH_TOKEN_SECRET, SecretStore};

pub const CLOUD_CLI_OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CLOUD_CLI_OAUTH_REFRESH_EARLY_MS: i64 = 5 * 60 * 1_000;
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// RFC 8628 default, used only when Clerk omits the interval.
pub const DEVICE_AUTHORIZATION_DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// RFC 8628 section 3.5: `slow_down` means "add 5 seconds to the interval".
pub const DEVICE_AUTHORIZATION_SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

/// The stored credential: `cloud-cli-oauth-token`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_epoch_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CliTokenError {
    #[error("Could not remove the stored T3 Connect CLI credential.")]
    CredentialRemoval,
    #[error("Could not refresh the T3 Connect CLI credential.")]
    CredentialRefresh(String),
    #[error("Could not read the stored T3 Connect CLI credential.")]
    CredentialRead,
    #[error("Could not authorize the T3 Connect CLI.")]
    Authorization(String),
    #[error("Timed out waiting for T3 Connect authorization.")]
    AuthorizationTimeout,
    #[error("T3 Connect authorization was denied in the browser.")]
    AuthorizationDenied,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: f64,
    #[allow(dead_code)]
    token_type: String,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: f64,
    interval: Option<f64>,
}

/// What the user is shown while a device authorization waits for approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorizationPrompt {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_in: Duration,
}

/// Best-effort read of `email` (or a fallback claim) from an OIDC id_token. Only used to show the
/// operator which account they linked, so a malformed token yields no identity rather than an error.
pub fn id_token_identity(id_token: Option<&str>) -> Option<String> {
    let payload = id_token?.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    ["email", "preferred_username", "sub"]
        .iter()
        .filter_map(|key| claims.get(*key).and_then(|v| v.as_str()))
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The text after an unsuccessful device poll or token exchange.
fn read_token_response(
    body: &[u8],
    refresh_token_param: Option<&str>,
    now_ms: i64,
) -> Result<(PersistedToken, Option<String>), CliTokenError> {
    let parsed: OAuthTokenResponse = serde_json::from_slice(body)
        .map_err(|_| CliTokenError::Authorization("unexpected token response".into()))?;
    let identity = id_token_identity(parsed.id_token.as_deref());
    let token = PersistedToken {
        access_token: parsed.access_token,
        refresh_token: parsed
            .refresh_token
            .or_else(|| refresh_token_param.map(str::to_owned))
            .unwrap_or_default(),
        expires_at_epoch_ms: now_ms + (parsed.expires_in * 1_000.0) as i64,
        identity: identity.clone(),
    };
    Ok((token, identity))
}

/// PKCE material: verifier, S256 challenge, and a state value.
pub struct PkceRequest {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

pub fn make_pkce_request() -> PkceRequest {
    let mut random = [0u8; 48];
    rand::fill(&mut random[..]);
    let verifier = URL_SAFE_NO_PAD.encode(&random[..32]);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = URL_SAFE_NO_PAD.encode(&random[32..]);
    PkceRequest {
        verifier,
        challenge,
        state,
    }
}

/// What the user asked for while the loopback flow waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAction {
    OpenBrowser,
    /// Switch to the device authorization flow (pressing `H`).
    Headless,
}

/// The terminal side of the loopback flow, injected so the flow is testable.
#[async_trait]
pub trait LoginInteraction: Send + Sync {
    /// Prints the authorization URL and the key hints.
    fn show_authorization_url(&self, authorization_url: &str);
    /// Waits for the next key press that means something (Enter opens the browser, H goes
    /// headless). Pends forever when there is no terminal.
    async fn next_action(&self) -> LoginAction;
    async fn launch_browser(&self, url: &str) -> Result<(), String>;
    fn warn(&self, message: &str);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopbackAuthorization {
    AuthorizationCode(String),
    HeadlessRequested,
}

/// Races the loopback callback against the user's key presses (`waitForLoopbackAuthorization`).
pub async fn wait_for_loopback_authorization<F>(
    authorization_url: &str,
    callback: F,
    interaction: &dyn LoginInteraction,
) -> Result<LoopbackAuthorization, CliTokenError>
where
    F: std::future::Future<Output = Result<String, CliTokenError>>,
{
    tokio::pin!(callback);
    loop {
        tokio::select! {
            code = &mut callback => return code.map(LoopbackAuthorization::AuthorizationCode),
            action = interaction.next_action() => match action {
                LoginAction::Headless => return Ok(LoopbackAuthorization::HeadlessRequested),
                LoginAction::OpenBrowser => {
                    if interaction.launch_browser(authorization_url).await.is_err() {
                        interaction.warn("Could not open a browser on this device. Open the URL above manually, or press H to switch to headless mode.");
                    }
                }
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GetOutcome {
    Authorized(PersistedToken),
    HeadlessRequested,
}

/// How long the device flow waits by default and how much a `slow_down` adds. The RFC values; a
/// test shrinks them to run a polling flow in real time.
#[derive(Debug, Clone, Copy)]
pub struct DeviceTiming {
    pub default_interval: Duration,
    pub slow_down_increment: Duration,
}

impl Default for DeviceTiming {
    fn default() -> Self {
        Self {
            default_interval: DEVICE_AUTHORIZATION_DEFAULT_INTERVAL,
            slow_down_increment: DEVICE_AUTHORIZATION_SLOW_DOWN_INCREMENT,
        }
    }
}

pub struct CliTokenManager {
    pub device_timing: DeviceTiming,
    secrets: Arc<dyn SecretStore>,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    oauth: CloudCliOAuthConfig,
    hosted_app_url: String,
    lock: Mutex<()>,
}

impl CliTokenManager {
    pub fn new(
        secrets: Arc<dyn SecretStore>,
        clock: Arc<dyn Clock>,
        oauth: CloudCliOAuthConfig,
        hosted_app_url: String,
    ) -> Self {
        Self {
            device_timing: DeviceTiming::default(),
            secrets,
            http: reqwest::Client::new(),
            clock,
            oauth,
            hosted_app_url,
            lock: Mutex::new(()),
        }
    }

    /// A manager for a build that has no Clerk values: it reads what is stored and every use of the OAuth endpoints fails at once.
    pub fn unconfigured(secrets: Arc<dyn SecretStore>, clock: Arc<dyn Clock>, hosted_app_url: String) -> Self {
        Self::new(secrets, clock, CloudCliOAuthConfig::unconfigured(), hosted_app_url)
    }

    fn read(&self) -> Result<Option<PersistedToken>, CliTokenError> {
        let Some(bytes) = self
            .secrets
            .get(CLOUD_CLI_OAUTH_TOKEN_SECRET)
            .map_err(|e| CliTokenError::CredentialRefresh(e.message))?
        else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| CliTokenError::CredentialRead)
    }

    fn persist(&self, token: &PersistedToken) -> Result<(), CliTokenError> {
        let encoded = serde_json::to_vec(token).expect("token json");
        self.secrets
            .set(CLOUD_CLI_OAUTH_TOKEN_SECRET, &encoded)
            .map_err(|e| CliTokenError::Authorization(e.message))
    }

    async fn exchange(&self, params: &[(&str, &str)]) -> Result<PersistedToken, CliTokenError> {
        let refresh_param = params
            .iter()
            .find(|(k, _)| *k == "refresh_token")
            .map(|(_, v)| *v);
        let response = self
            .http
            .post(&self.oauth.token_endpoint)
            .form(params)
            .send()
            .await
            .map_err(|e| CliTokenError::Authorization(e.to_string()))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| CliTokenError::Authorization(e.to_string()))?;
        if !status.is_success() {
            return Err(CliTokenError::Authorization(format!(
                "token endpoint answered HTTP {status}"
            )));
        }
        read_token_response(&body, refresh_param, self.clock.now_ms()).map(|(token, _)| token)
    }

    async fn refresh(&self, token: &PersistedToken) -> Result<PersistedToken, CliTokenError> {
        let refreshed = self
            .exchange(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &token.refresh_token),
                ("client_id", &self.oauth.client_id),
            ])
            .await?;
        Ok(match (&refreshed.identity, &token.identity) {
            (None, Some(identity)) => PersistedToken {
                identity: Some(identity.clone()),
                ..refreshed
            },
            _ => refreshed,
        })
    }

    async fn get_existing_no_lock(&self) -> Result<Option<PersistedToken>, CliTokenError> {
        let Some(token) = self.read()? else {
            return Ok(None);
        };
        if token.expires_at_epoch_ms - CLOUD_CLI_OAUTH_REFRESH_EARLY_MS > self.clock.now_ms() {
            return Ok(Some(token));
        }
        let refreshed = self.refresh(&token).await?;
        self.persist(&refreshed)?;
        Ok(Some(refreshed))
    }

    /// The stored token, refreshed when it expires within five minutes.
    pub async fn get_existing(&self) -> Result<Option<PersistedToken>, CliTokenError> {
        let _guard = self.lock.lock().await;
        self.get_existing_no_lock()
            .await
            .map_err(|error| match error {
                CliTokenError::CredentialRefresh(cause) => CliTokenError::CredentialRefresh(cause),
                other => CliTokenError::CredentialRefresh(other.to_string()),
            })
    }

    pub async fn has_credential(&self) -> Result<bool, CliTokenError> {
        let _guard = self.lock.lock().await;
        self.read().map(|token| token.is_some())
    }

    pub async fn store(&self, token: &PersistedToken) -> Result<(), CliTokenError> {
        let _guard = self.lock.lock().await;
        self.persist(token)
    }

    pub async fn clear(&self) -> Result<(), CliTokenError> {
        let _guard = self.lock.lock().await;
        self.secrets
            .remove(CLOUD_CLI_OAUTH_TOKEN_SECRET)
            .map_err(|_| CliTokenError::CredentialRemoval)
    }

    /// A usable token: the stored one, or a fresh loopback login. A stored credential that cannot
    /// be read or refreshed (corrupt, revoked, expired grant) falls through to a fresh login
    /// rather than dead-ending the command.
    pub async fn get(
        &self,
        interaction: &dyn LoginInteraction,
    ) -> Result<GetOutcome, CliTokenError> {
        let _guard = self.lock.lock().await;
        if let Ok(Some(token)) = self.get_existing_no_lock().await {
            return Ok(GetOutcome::Authorized(token));
        }
        match self.login_loopback(interaction).await? {
            LoopbackLogin::Authorized(token) => {
                self.persist(&token)?;
                Ok(GetOutcome::Authorized(token))
            }
            LoopbackLogin::HeadlessRequested => Ok(GetOutcome::HeadlessRequested),
        }
    }

    async fn login_loopback(
        &self,
        interaction: &dyn LoginInteraction,
    ) -> Result<LoopbackLogin, CliTokenError> {
        let pkce = make_pkce_request();
        let (code_tx, code_rx) = oneshot::channel::<String>();
        let code_tx = Arc::new(std::sync::Mutex::new(Some(code_tx)));
        let state = pkce.state.clone();
        let app = axum::Router::new().route(
            "/callback",
            axum::routing::get(
                move |axum::extract::Query(query): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| {
                    let code_tx = code_tx.clone();
                    let state = state.clone();
                    async move {
                        match (query.get("state"), query.get("code")) {
                            (Some(got), Some(code)) if *got == state && !code.is_empty() => {
                                if let Some(tx) = code_tx.lock().expect("callback").take() {
                                    let _ = tx.send(code.clone());
                                }
                                (
                                    axum::http::StatusCode::OK,
                                    [("content-type", "text/html; charset=utf-8")],
                                    authorization_complete_html(),
                                )
                            }
                            _ => (
                                axum::http::StatusCode::BAD_REQUEST,
                                [("content-type", "text/plain; charset=utf-8")],
                                "Invalid T3 Connect authorization callback.".to_owned(),
                            ),
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", self.oauth.loopback_port))
            .await
            .map_err(|e| CliTokenError::Authorization(e.to_string()))?;
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // The hosted /connect page establishes a Clerk session before forwarding the request to
        // /oauth/authorize; sending a signed-out browser there directly loses the parameters.
        let authorization_url = build_connect_authorize_request_url(
            &self.hosted_app_url,
            &pkce.state,
            &pkce.challenge,
            self.oauth.loopback_port,
        )
        .map_err(|e| CliTokenError::Authorization(e.to_string()))?;
        interaction.show_authorization_url(&authorization_url);
        let callback = async {
            match tokio::time::timeout(CLOUD_CLI_OAUTH_CALLBACK_TIMEOUT, code_rx).await {
                Ok(Ok(code)) => Ok(code),
                Ok(Err(_)) => Err(CliTokenError::Authorization("callback closed".into())),
                Err(_) => Err(CliTokenError::AuthorizationTimeout),
            }
        };
        let outcome =
            wait_for_loopback_authorization(&authorization_url, callback, interaction).await;
        server.abort();
        match outcome? {
            LoopbackAuthorization::HeadlessRequested => Ok(LoopbackLogin::HeadlessRequested),
            LoopbackAuthorization::AuthorizationCode(code) => {
                let token = self
                    .exchange(&[
                        ("grant_type", "authorization_code"),
                        ("code", &code),
                        ("redirect_uri", &self.oauth.redirect_uri),
                        ("client_id", &self.oauth.client_id),
                        ("code_verifier", &pkce.verifier),
                    ])
                    .await?;
                Ok(LoopbackLogin::Authorized(token))
            }
        }
    }

    /// OAuth device authorization grant for machines without a local browser (SSH). The user
    /// approves a short code on Clerk's hosted device page while this polls the token endpoint;
    /// no redirect URI is involved. `show_prompt` is called once, before polling.
    pub async fn device_authorization_login(
        &self,
        show_prompt: impl FnOnce(&DeviceAuthorizationPrompt),
    ) -> Result<(PersistedToken, Option<String>), CliTokenError> {
        let response = self
            .http
            .post(&self.oauth.device_authorization_endpoint)
            .form(&[
                ("client_id", self.oauth.client_id.as_str()),
                ("scope", &self.oauth.scopes.join(" ")),
            ])
            .send()
            .await
            .map_err(|e| CliTokenError::Authorization(e.to_string()))?;
        if !response.status().is_success() {
            return Err(CliTokenError::Authorization(format!(
                "device authorization answered HTTP {}",
                response.status()
            )));
        }
        let authorization: DeviceAuthorizationResponse = response
            .json()
            .await
            .map_err(|e| CliTokenError::Authorization(e.to_string()))?;
        // Clerk's advertised lifetime and interval are authoritative.
        let expires_in = Duration::from_secs_f64(authorization.expires_in.max(0.0));
        let interval = authorization
            .interval
            .map(|seconds| Duration::from_secs_f64(seconds.max(0.0)))
            .unwrap_or(self.device_timing.default_interval);
        show_prompt(&DeviceAuthorizationPrompt {
            verification_uri: authorization.verification_uri,
            verification_uri_complete: authorization.verification_uri_complete,
            user_code: authorization.user_code,
            expires_in,
        });
        tokio::time::timeout(
            expires_in,
            self.poll_device_token(&authorization.device_code, interval),
        )
        .await
        .unwrap_or(Err(CliTokenError::AuthorizationTimeout))
    }

    /// Polls until the user approves or denies (RFC 8628 3.4/3.5). `authorization_pending` keeps
    /// waiting; `slow_down` and transient failures widen the interval before the next tick.
    async fn poll_device_token(
        &self,
        device_code: &str,
        initial_interval: Duration,
    ) -> Result<(PersistedToken, Option<String>), CliTokenError> {
        let params = [
            ("grant_type", DEVICE_CODE_GRANT_TYPE),
            ("device_code", device_code),
            ("client_id", self.oauth.client_id.as_str()),
        ];
        let mut interval = initial_interval;
        loop {
            tokio::time::sleep(interval).await;
            let response = match self
                .http
                .post(&self.oauth.token_endpoint)
                .form(&params)
                .send()
                .await
            {
                Ok(response) => Some(response),
                // Transport failures are transient while the device code is still valid.
                Err(error) if error.is_connect() || error.is_timeout() || error.is_request() => {
                    None
                }
                Err(error) => return Err(CliTokenError::Authorization(error.to_string())),
            };
            let Some(response) = response else {
                interval += self.device_timing.slow_down_increment;
                continue;
            };
            let status = response.status();
            let body = response.bytes().await.unwrap_or_default();
            // Upstream 5xx is transient too; RFC 8628 asks clients to back off like slow_down.
            if status.as_u16() >= 500 {
                interval += self.device_timing.slow_down_increment;
                continue;
            }
            if status.is_success() {
                return read_token_response(&body, None, self.clock.now_ms());
            }
            let failure: OAuthErrorResponse = serde_json::from_slice(&body).map_err(|_| {
                CliTokenError::Authorization("unexpected device token response".into())
            })?;
            match failure.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => interval += self.device_timing.slow_down_increment,
                "expired_token" => return Err(CliTokenError::AuthorizationTimeout),
                "access_denied" => return Err(CliTokenError::AuthorizationDenied),
                _ => {
                    return Err(CliTokenError::Authorization(
                        failure.error_description.unwrap_or(failure.error),
                    ));
                }
            }
        }
    }
}

enum LoopbackLogin {
    Authorized(PersistedToken),
    HeadlessRequested,
}

/// The page the browser lands on after the loopback callback.
/// TODO(cosmetic): the original renders a styled, branded page (`cloud/cliAuthHtml.ts`); this is
/// the same message without the styling.
fn authorization_complete_html() -> String {
    "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\" /><title>T3 Connect authorization complete</title></head><body><h1>T3 Connect authorization complete</h1><p>You can close this tab and return to your terminal.</p></body></html>".to_owned()
}
