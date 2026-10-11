//! The OAuth authorization server MCP clients (Claude Code, Codex, any agent T3 Code did not launch) use to sign in to this
//! environment's `/mcp` (port of `apps/server/src/auth/McpOAuth.ts`; the routes are `mcp_oauth_http.rs`).
//!
//! Every URL is derived from the request's own origin, so the same server answers correctly over loopback, Tailscale Serve and a
//! T3 Connect tunnel. Client registration is stateless: a client id is its signed metadata (`<base64url JSON>.<HMAC-SHA256>`,
//! keyed by the secret `mcp-oauth-signing-key`), so an unauthenticated caller cannot grow server state. Redirects go to a loopback
//! address (a CLI agent on the user's machine) or any https address (a hosted agent); the approval page names where access goes,
//! and the user decides. An authorization code lives in memory for 60 seconds and is spent by the first request that presents it.
//! The token is a 30-day bearer session of the `mcp-client` kind that only `/mcp` accepts.
use crate::{
    auth::{AuthService, McpClientSession, mcp_client_scopes},
    persistence::Store,
    server_secret_store::ServerSecretStore,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use t3_contracts::{
    AuthEnvironmentScope, AuthMcpApprovalDecision, AuthMcpAuthorizationRequest, AuthMcpAuthorizationServerMetadata, AuthMcpClientAccess, AuthMcpClientRegistration, AuthMcpProtectedResourceMetadata,
    AuthMcpRegistrationError, AuthMcpRegistrationErrorCode, AuthMcpTokenError, AuthMcpTokenErrorCode, AuthMcpTokenRequest, AuthMcpTokenResult,
};

const SIGNING_SECRET_NAME: &str = "mcp-oauth-signing-key";
const AUTHORIZATION_CODE_TTL_MS: i64 = 60_000;
const MAX_CLIENT_NAME_LENGTH: usize = 100;
const MAX_REDIRECT_URIS: usize = 5;
const MAX_REDIRECT_URI_LENGTH: usize = 512;
const DEFAULT_CLIENT_NAME: &str = "MCP client";
const LOOPBACK_HOSTNAMES: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// `MCP_OAUTH_SCOPES`.
pub const MCP_OAUTH_SCOPES: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];

/// Issuer and MCP resource for the origin a request reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthUrls {
    pub issuer: String,
    pub resource: String,
}

/// `requestUrls`: the origin of the request's own `Host` (https when a proxy says so with `x-forwarded-proto`). A `Host` that is
/// not a valid authority falls back to localhost, which no client will have asked for. `target` is the request's path and query.
pub fn request_urls(host: Option<&str>, forwarded_proto: Option<&str>, target: &str) -> McpOAuthUrls {
    let protocol = if forwarded_proto == Some("https") { "https" } else { "http" };
    let base = format!("{protocol}://{}", host.unwrap_or("localhost"));
    let origin = url::Url::parse(&base).and_then(|base| base.join(target)).map_or_else(|_| "http://localhost".to_owned(), |url| url.origin().ascii_serialization());
    McpOAuthUrls { resource: format!("{origin}/mcp"), issuer: origin }
}

pub fn protected_resource_metadata(urls: &McpOAuthUrls) -> AuthMcpProtectedResourceMetadata {
    serde_json::from_value(json!({
        "resource": urls.resource,
        "authorization_servers": [urls.issuer],
        "scopes_supported": MCP_OAUTH_SCOPES,
        "bearer_methods_supported": ["header"],
        "resource_name": "T3 Code",
    }))
    .expect("protected resource metadata")
}

pub fn authorization_server_metadata(urls: &McpOAuthUrls) -> AuthMcpAuthorizationServerMetadata {
    serde_json::from_value(json!({
        "issuer": urls.issuer,
        "authorization_endpoint": format!("{}/oauth/mcp/authorize", urls.issuer),
        "token_endpoint": format!("{}/oauth/mcp/token", urls.issuer),
        "registration_endpoint": format!("{}/oauth/mcp/register", urls.issuer),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": MCP_OAUTH_SCOPES,
        "authorization_response_iss_parameter_supported": true,
    }))
    .expect("authorization server metadata")
}

/// A redirect URI an MCP client may register: loopback http, or any https. No fragment, not even an empty one (RFC 6749 §3.1.2).
fn parse_redirect(value: &str) -> Option<url::Url> {
    if crate::text_generation::utf16_len(value) > MAX_REDIRECT_URI_LENGTH || value.contains('#') {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    let allowed = url.scheme() == "https" || (url.scheme() == "http" && url.host_str().is_some_and(|host| LOOPBACK_HOSTNAMES.contains(&host)));
    (allowed && url.username().is_empty() && url.password().is_none()).then_some(url)
}

/// Whether a presented redirect is one the client registered. Loopback redirects match on everything but the port (RFC 8252
/// §7.3), since CLI agents listen on whatever port is free; https redirects match exactly.
pub fn redirect_matches(registered: &str, presented: &str) -> bool {
    let (Some(left), Some(right)) = (parse_redirect(registered), parse_redirect(presented)) else { return false };
    if left.scheme() == "https" || right.scheme() == "https" {
        return left.as_str() == right.as_str();
    }
    left.host_str() == right.host_str() && left.path() == right.path() && left.query().unwrap_or("") == right.query().unwrap_or("")
}

fn same_resource(urls: &McpOAuthUrls, presented: Option<&str>) -> bool {
    let Some(presented) = presented else { return true };
    let Ok(url) = url::Url::parse(presented) else { return false };
    // `URL.hash` is empty for a missing and for an empty fragment alike.
    url.fragment().is_none_or(str::is_empty) && format!("{}{}", url.origin().ascii_serialization(), url.path().trim_end_matches('/')) == urls.resource
}

/// A client id decoded: its name and registered redirects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthClient {
    pub client_id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
}

/// Problems the authorize page shows the user without redirecting anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageError(pub String);

/// Problems reported back to the client through its (validated) redirect URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectError {
    pub error: &'static str,
    pub description: String,
    pub redirect_uri: String,
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationFailure {
    Page(PageError),
    Redirect(RedirectError),
}

/// A validated authorize request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationRequest {
    pub client: McpOAuthClient,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub state: Option<String>,
    pub resource: String,
    pub issuer: String,
}

struct PendingCode {
    client_id: String,
    client_name: String,
    redirect_uri: String,
    code_challenge: String,
    resource: String,
    access: AuthMcpClientAccess,
    expires_at_ms: i64,
}

/// The browser session that may approve, and what it holds.
#[derive(Debug, Clone)]
pub struct ApprovingSession {
    pub csrf_token: String,
    pub scopes: Vec<AuthEnvironmentScope>,
}

/// What the approval endpoint is asked to approve with.
pub enum Approval<'a> {
    PairingCode { access: AuthMcpClientAccess, code: &'a str },
    BrowserSession { access: AuthMcpClientAccess, csrf_token: &'a str },
}

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub struct McpOAuth {
    auth: AuthService,
    signing_key: Vec<u8>,
    codes: Mutex<HashMap<String, PendingCode>>,
    clock: Clock,
}

#[derive(Serialize, Deserialize)]
struct ClientIdPayload {
    v: u8,
    n: String,
    r: Vec<String>,
}

fn base64_url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Node's `Buffer.from(text, "base64url")`: either alphabet, anything else skipped, no padding needed.
fn lenient_base64_url(text: &str) -> Vec<u8> {
    let mut cleaned: String = text.chars().filter_map(|c| match c {
        '+' => Some('-'),
        '/' => Some('_'),
        c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => Some(c),
        _ => None,
    }).collect();
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    URL_SAFE_NO_PAD.decode(cleaned.trim_end_matches('=')).unwrap_or_default()
}

/// `timingSafeEqualBase64Url`.
fn timing_safe_equal(left: &str, right: &str) -> bool {
    let (left, right) = (lenient_base64_url(left), lenient_base64_url(right));
    left.len() == right.len() && left.iter().zip(&right).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

impl McpOAuth {
    /// Over the server's own sessions, with the signing key kept in the secret store (made on first use).
    pub fn new(auth: AuthService, secrets: &ServerSecretStore) -> Result<Self, crate::server_secret_store::SecretStoreError> {
        Ok(Self::with_clock(auth, secrets.get_or_create_random(SIGNING_SECRET_NAME, 32)?, Arc::new(Utc::now)))
    }

    pub fn with_clock(auth: AuthService, signing_key: Vec<u8>, clock: Clock) -> Self {
        Self { auth, signing_key, codes: Mutex::default(), clock }
    }

    fn sign(&self, domain: &str, payload: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.signing_key).expect("HMAC accepts any key length");
        mac.update(format!("{domain}.{payload}").as_bytes());
        base64_url(&mac.finalize().into_bytes())
    }

    fn sign_client_id(&self, name: &str, redirect_uris: &[String]) -> String {
        let body = base64_url(serde_json::to_string(&ClientIdPayload { v: 1, n: name.to_owned(), r: redirect_uris.to_vec() }).expect("a payload encodes").as_bytes());
        format!("{body}.{}", self.sign("mcp-client-id", &body))
    }

    fn parse_client_id(&self, client_id: &str) -> Option<McpOAuthClient> {
        let mut parts = client_id.split('.');
        let (body, signature) = (parts.next().filter(|part| !part.is_empty())?, parts.next().filter(|part| !part.is_empty())?);
        if parts.next().is_some() || !timing_safe_equal(signature, &self.sign("mcp-client-id", body)) {
            return None;
        }
        let json = URL_SAFE_NO_PAD.decode(body.trim_end_matches('=')).ok()?;
        let payload: Value = serde_json::from_slice(&json).ok()?;
        let redirect_uris = payload["r"].as_array()?.iter().map(|uri| uri.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()?;
        (payload["v"] == 1).then_some(())?;
        Some(McpOAuthClient { client_id: client_id.to_owned(), name: payload["n"].as_str()?.to_owned(), redirect_uris })
    }

    /// The token the approval page must send back with a one-click approval: bound to the browser session and the request.
    pub fn csrf_token(&self, session_id: &str, authorization: &AuthorizationRequest) -> String {
        self.sign("mcp-csrf", &serde_json::to_string(&[session_id, authorization.client.client_id.as_str(), authorization.redirect_uri.as_str(), authorization.code_challenge.as_str()]).expect("strings encode"))
    }

    /// `register` (RFC 7591). Requested grant types, scopes and token endpoint auth methods are ignored rather than rejected: the
    /// server answers with what it supports. Every client is registered as public and proves itself with PKCE instead.
    pub fn register(&self, metadata: &AuthMcpClientRegistration) -> Result<McpOAuthClient, AuthMcpRegistrationError> {
        let raw_name = metadata.client_name.as_deref().map_or("", crate::text_generation::js_trim);
        let name = crate::text_generation::utf16_prefix(if raw_name.is_empty() { DEFAULT_CLIENT_NAME } else { raw_name }, MAX_CLIENT_NAME_LENGTH).to_owned();
        let redirect_uris = metadata.redirect_uris.clone().unwrap_or_default();
        let rejected = |description: String| AuthMcpRegistrationError { error: AuthMcpRegistrationErrorCode::InvalidRedirectUri, error_description: description };
        if redirect_uris.is_empty() || redirect_uris.len() > MAX_REDIRECT_URIS {
            return Err(rejected(format!("Register between 1 and {MAX_REDIRECT_URIS} redirect URIs.")));
        }
        if !redirect_uris.iter().all(|uri| parse_redirect(uri).is_some()) {
            return Err(rejected("Redirect URIs must be https, or http on localhost, 127.0.0.1 or [::1].".into()));
        }
        Ok(McpOAuthClient { client_id: self.sign_client_id(&name, &redirect_uris), name, redirect_uris })
    }

    /// Validates an authorize request. Page errors must never redirect.
    pub fn validate_authorization(&self, urls: &McpOAuthUrls, request: &AuthMcpAuthorizationRequest) -> Result<AuthorizationRequest, AuthorizationFailure> {
        let page = |description: &str| AuthorizationFailure::Page(PageError(description.into()));
        let Some(client) = self.parse_client_id(request.client_id.as_deref().unwrap_or("")) else {
            return Err(page("This sign-in link names an unknown app. Start the sign-in again from it."));
        };
        let Some(redirect_uri) = request.redirect_uri.clone().filter(|presented| client.redirect_uris.iter().any(|registered| redirect_matches(registered, presented))) else {
            return Err(page("This sign-in link sends you to an address the app did not register."));
        };
        let fail = |error: &'static str, description: String| AuthorizationFailure::Redirect(RedirectError { error, description, redirect_uri: redirect_uri.clone(), state: request.state.clone() });
        if request.response_type.as_deref() != Some("code") {
            return Err(fail("unsupported_response_type", "Only response_type=code is supported.".into()));
        }
        let challenge = request.code_challenge.as_deref().filter(|challenge| is_code_challenge(challenge));
        let Some(code_challenge) = challenge.filter(|_| request.code_challenge_method.as_deref() == Some("S256")) else {
            return Err(fail("invalid_request", "PKCE with code_challenge_method=S256 is required.".into()));
        };
        if !same_resource(urls, request.resource.as_deref()) {
            return Err(fail("invalid_target", format!("This server only issues tokens for {}.", urls.resource)));
        }
        Ok(AuthorizationRequest { client, redirect_uri: redirect_uri.clone(), code_challenge: code_challenge.to_owned(), state: request.state.clone(), resource: urls.resource.clone(), issuer: urls.issuer.clone() })
    }

    /// The signed-in owner on this origin, when their browser session may approve at least read-only access. `approve` checks
    /// the chosen access against the session's scopes.
    pub fn approving_browser_session(&self, cookie_token: Option<&str>, authorization: &AuthorizationRequest) -> Option<ApprovingSession> {
        let session = self.auth.authenticate_browser_session(cookie_token?, (self.clock)()).ok()?;
        // Approving manages access, and a session may only hand out scopes it holds.
        (session.scopes.contains(&AuthEnvironmentScope::AccessWrite) && session.scopes.contains(&AuthEnvironmentScope::OrchestrationRead))
            .then(|| ApprovingSession { csrf_token: self.csrf_token(&session.session_id, authorization), scopes: session.scopes })
    }

    fn mint_code(&self, authorization: &AuthorizationRequest, access: AuthMcpClientAccess) -> String {
        let now = (self.clock)().timestamp_millis();
        let mut bytes = [0u8; 32];
        rand::TryRngCore::try_fill_bytes(&mut rand::rngs::OsRng, &mut bytes).expect("the operating system supplies random bytes");
        let code = base64_url(&bytes);
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|_, pending| pending.expires_at_ms > now);
        codes.insert(code.clone(), PendingCode {
            client_id: authorization.client.client_id.clone(),
            client_name: authorization.client.name.clone(),
            redirect_uri: authorization.redirect_uri.clone(),
            code_challenge: authorization.code_challenge.clone(),
            resource: authorization.resource.clone(),
            access,
            expires_at_ms: now + AUTHORIZATION_CODE_TTL_MS,
        });
        code
    }

    /// Approves and returns the URL to send the browser to, or the message the page shows.
    pub fn approve(&self, cookie_token: Option<&str>, authorization: &AuthorizationRequest, approval: Approval<'_>) -> Result<String, String> {
        let access = match approval {
            Approval::PairingCode { access, code } => {
                self.auth.consume_mcp_approval_code(code, access, (self.clock)()).map_err(|error| error.message().to_owned())?;
                access
            }
            Approval::BrowserSession { access, csrf_token } => {
                let allowed = self.approving_browser_session(cookie_token, authorization).filter(|session| timing_safe_equal(csrf_token, &session.csrf_token) && mcp_client_scopes(access).iter().all(|scope| session.scopes.contains(scope)));
                if allowed.is_none() {
                    return Err("Your session cannot approve this request. Enter a pairing code instead.".into());
                }
                access
            }
        };
        let code = self.mint_code(authorization, access);
        tracing::info!(client = %authorization.client.name, access = ?access, "Approved an MCP client sign-in.");
        Ok(redirect_with(&authorization.redirect_uri, &[("code", Some(&code)), ("state", authorization.state.as_deref()), ("iss", Some(&authorization.issuer))]))
    }

    pub fn deny(&self, authorization: &AuthorizationRequest) -> String {
        redirect_with(&authorization.redirect_uri, &[("error", Some("access_denied")), ("state", authorization.state.as_deref()), ("iss", Some(&authorization.issuer))])
    }

    /// The token request. `client` is what the list of clients shows for the session it issues.
    pub fn exchange_code(&self, client: Value, urls: &McpOAuthUrls, token: &AuthMcpTokenRequest) -> Result<AuthMcpTokenResult, AuthMcpTokenError> {
        let fail = |error: AuthMcpTokenErrorCode, description: &str| AuthMcpTokenError { error, error_description: description.into() };
        if token.grant_type.as_deref() != Some("authorization_code") {
            return Err(fail(AuthMcpTokenErrorCode::UnsupportedGrantType, "Only authorization_code is supported."));
        }
        fn present(value: &Option<String>) -> Option<&str> {
            value.as_deref().filter(|value| !value.is_empty())
        }
        let (Some(code), Some(redirect_uri), Some(client_id), Some(verifier)) = (present(&token.code), present(&token.redirect_uri), present(&token.client_id), present(&token.code_verifier)) else {
            return Err(fail(AuthMcpTokenErrorCode::InvalidRequest, "code, redirect_uri, client_id and code_verifier are required."));
        };
        if self.parse_client_id(client_id).is_none() {
            return Err(fail(AuthMcpTokenErrorCode::InvalidClient, "The client is unknown."));
        }
        // A presented code is spent even when a later check fails (RFC 6749 §4.1.2).
        let pending = self.codes.lock().unwrap().remove(code);
        let now = (self.clock)();
        let valid = pending.as_ref().is_some_and(|pending| {
            pending.expires_at_ms > now.timestamp_millis()
                && pending.client_id == client_id
                && pending.redirect_uri == redirect_uri
                && pending.resource == urls.resource
                && same_resource(urls, token.resource.as_deref())
                && pkce_verifies(verifier, &pending.code_challenge)
        });
        let pending = pending.filter(|_| valid).ok_or_else(|| fail(AuthMcpTokenErrorCode::InvalidGrant, "The authorization code is invalid or expired."))?;
        let (session, bearer) = self.auth.issue_mcp_client_session(&pending.client_name, pending.access, client, now).map_err(|error| {
            tracing::error!(%error, "Could not issue an MCP client session.");
            fail(AuthMcpTokenErrorCode::InvalidGrant, "The session could not be issued.")
        })?;
        let scope = mcp_client_scopes(pending.access).iter().map(|scope| serde_json::to_value(scope).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()).collect::<Vec<_>>().join(" ");
        Ok(AuthMcpTokenResult {
            access_token: bearer,
            token_type: t3_contracts::BearerTokenType::Bearer,
            expires_in: serde_json::Number::from(((session.expires_at - now).num_milliseconds() / 1000).max(0)),
            scope,
        })
    }

    /// `/mcp`'s admission of a bearer token that is not a provider session's: an agent signed in through OAuth.
    pub fn authenticate_client(&self, token: &str) -> Option<McpClientSession> {
        match self.auth.authenticate_mcp_client(token, (self.clock)()) {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(%error, "MCP client authentication failed.");
                None
            }
        }
    }
}

/// `^[A-Za-z0-9_-]{43}$`.
fn is_code_challenge(value: &str) -> bool {
    value.len() == 43 && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// `pkceVerifies` (RFC 7636 S256): the verifier is 43 to 128 unreserved characters and its SHA-256 is the challenge.
pub fn pkce_verifies(verifier: &str, challenge: &str) -> bool {
    let allowed = (43..=128).contains(&verifier.len()) && verifier.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'));
    allowed && timing_safe_equal(&base64_url(&Sha256::digest(verifier.as_bytes())), challenge)
}

/// `redirectWith`: appends OAuth response parameters, keeping any the client put in its redirect URI.
pub fn redirect_with(redirect_uri: &str, params: &[(&str, Option<&str>)]) -> String {
    let mut url = url::Url::parse(redirect_uri).expect("a validated redirect URI");
    let mut pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let mut changed = false;
    for (name, value) in params {
        let Some(value) = value else { continue };
        changed = true;
        // `URLSearchParams.set`: the first pair of the name takes the value, later ones go.
        match pairs.iter().position(|(existing, _)| existing == name) {
            Some(at) => {
                pairs[at].1 = (*value).to_owned();
                let mut index = 0;
                pairs.retain(|(existing, _)| {
                    let keep = existing != name || index == at;
                    index += 1;
                    keep
                });
            }
            None => pairs.push(((*name).to_owned(), (*value).to_owned())),
        }
    }
    if changed {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    url.to_string()
}

pub fn redirect_for_error(error: &RedirectError, issuer: &str) -> String {
    redirect_with(&error.redirect_uri, &[("error", Some(error.error)), ("error_description", Some(&error.description)), ("state", error.state.as_deref()), ("iss", Some(issuer))])
}

/// `renderErrorPage` (`mcpOAuthHtml.ts`): plain server HTML for a sign-in link that cannot be trusted, so it never depends on the
/// web app, and it does not redirect anywhere.
pub fn render_error_page(description: &str) -> String {
    let escape = |value: &str| value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;");
    const STYLES: &str = r#"
  :root { color-scheme: light dark; font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; padding: 32px 16px; background: #f6f7f9; color: #17191f; }
  main { width: min(100%, 440px); }
  h1 { margin: 0 0 8px; font-size: 20px; }
  p { margin: 0 0 12px; line-height: 1.5; color: #4b5060; }
  @media (prefers-color-scheme: dark) {
    body { background: #0f1115; color: #e6e8ee; }
    p { color: #a0a6b4; }
  }
"#;
    let body = format!("<h1>This sign-in cannot continue</h1>\n    <p>{}</p>\n    <p>Close this page and start the sign-in again from your agent.</p>", escape(description));
    format!("<!doctype html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"utf-8\" />\n    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n    <meta name=\"color-scheme\" content=\"light dark\" />\n    <title>{}</title>\n    <style>{STYLES}</style>\n  </head>\n  <body>\n    <main>{body}</main>\n  </body>\n</html>", escape("Sign-in failed"))
}

// ---- the server's one ----

#[derive(Default)]
struct Slot(OnceLock<Arc<McpOAuth>>);

/// Installs the server's authorization server; a server that never installed one has none of the routes.
pub fn install(store: &Store, oauth: Arc<McpOAuth>) {
    let _ = store.extension(Slot::default).0.set(oauth);
}

pub fn current(store: &Store) -> Option<Arc<McpOAuth>> {
    store.extension(Slot::default).0.get().cloned()
}

/// Which decision a body carries, for the routes.
pub fn approval_of(decision: &AuthMcpApprovalDecision) -> Option<Approval<'_>> {
    match decision {
        AuthMcpApprovalDecision::Deny => None,
        AuthMcpApprovalDecision::PairingCode { access, code } => Some(Approval::PairingCode { access: *access, code: code.as_str() }),
        AuthMcpApprovalDecision::BrowserSession { access, csrf_token } => Some(Approval::BrowserSession { access: *access, csrf_token }),
    }
}
