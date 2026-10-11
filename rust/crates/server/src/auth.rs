use crate::persistence::{Store, StoreError};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use t3_contracts::{
    AuthClientMetadata, AuthClientMetadataDeviceType, AuthClientSession, AuthEnvironmentScope,
    AuthMcpClientAccess, AuthPairingLink, AuthSessionId, ServerAuthSessionMethod, TrimmedNonEmptyString, UtcDateTime,
};

/// The subject of the pairing link the server mints for itself at start. The original keeps it out of
/// the list of pairing links (`EnvironmentAuth.ts` `INTERNAL_ADMINISTRATIVE_BOOTSTRAP_SUBJECT`).
pub const ADMINISTRATIVE_BOOTSTRAP_SUBJECT: &str = "administrative-bootstrap";
const ONE_TIME_TOKEN_SUBJECT: &str = "one-time-token";
/// The subject of the sessions MCP OAuth issues to agents T3 Code did not launch (`MCP_CLIENT_SUBJECT`).
pub const MCP_CLIENT_SUBJECT: &str = "mcp-client";
const MCP_CLIENT_SESSION_TTL_DAYS: i64 = 30;
/// What `PairingGrantStore.ts` draws a pairing credential from (no 0/1/I/O) and how long it is.
const PAIRING_TOKEN_ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const PAIRING_TOKEN_LENGTH: usize = 12;

/// What an MCP client's token stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpClientSession {
    pub session_id: String,
    pub label: String,
    pub access: AuthMcpClientAccess,
}

/// `mcpClientScopes`: a read-only grant holds `orchestration:read` alone, any other also `orchestration:operate`.
pub fn mcp_client_scopes(access: AuthMcpClientAccess) -> Vec<AuthEnvironmentScope> {
    if access == AuthMcpClientAccess::ReadOnly {
        vec![AuthEnvironmentScope::OrchestrationRead]
    } else {
        vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]
    }
}

/// `ServerAuthMcpApprovalCodeError`, and the storage failure that is not the code's fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpApprovalCodeError {
    UnknownOrUsed,
    NotAPairingCode,
    InsufficientScope,
    Internal,
}
impl McpApprovalCodeError {
    /// `error.message` of the original.
    pub fn message(self) -> &'static str {
        match self {
            Self::InsufficientScope => "That pairing code cannot grant this access, and it is now used up. Create one with the standard scopes, or choose Read only with a new code.",
            Self::NotAPairingCode => "That is not a one-time pairing code.",
            Self::UnknownOrUsed => "That pairing code is unknown, expired, or already used.",
            Self::Internal => "The code could not be checked. Try again.",
        }
    }
}

/// A change to who can reach the server, the live half of `subscribeAuthAccess`.
#[derive(Debug, Clone)]
pub enum AccessChange {
    PairingLinkUpserted(AuthPairingLink),
    PairingLinkRemoved(String),
    /// `current` is false here: it is relative to the subscriber and set where the event is sent.
    ClientUpserted(AuthClientSession),
    ClientRemoved(String),
}

/// Sessions that hold a socket now (a count each, as one session may hold several) and the channel
/// that publishes access changes. Shared by every clone of the service.
struct Live {
    connected: Mutex<HashMap<String, usize>>,
    changes: tokio::sync::broadcast::Sender<AccessChange>,
}

/// Why a client session could not be revoked.
#[derive(Debug, thiserror::Error)]
pub enum RevokeClientError {
    /// A session may not revoke itself (`ServerAuthForbiddenOperationError`).
    #[error("The current authentication session cannot revoke itself.")]
    CurrentSession,
    #[error(transparent)]
    Auth(#[from] AuthError),
}

/// A pairing link as issued: the credential is shown here and nowhere after.
#[derive(Debug, Clone)]
pub struct IssuedPairingLink {
    pub id: String,
    pub credential: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Requested scope was not granted.")]
    ScopeNotGranted,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub session_id: String,
    pub subject: String,
    pub method: String,
    pub scopes: Vec<AuthEnvironmentScope>,
    pub client: Value,
    /// Absent on sessions stored before the list of clients existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// The thumbprint of the DPoP key the session is bound to (`proofKeyThumbprint`); set only on `dpop-access-token` sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_key_thumbprint: Option<String>,
    /// The runtime mode an MCP client session's threads never run above (`runtimeModeCeiling`); absent on every other
    /// session and on a read-only MCP client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_mode_ceiling: Option<AuthMcpClientAccess>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    v: u8,
    kind: String,
    sid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scopes: Option<Vec<AuthEnvironmentScope>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    iat: i64,
    exp: i64,
}

#[derive(Clone)]
struct DesktopBootstrapGrant {
    token: String,
    secret: Option<String>,
    expires_at: DateTime<Utc>,
}
/// The prefix of the session a reusable dev credential stands for (`REUSABLE_DEV_SESSION_PREFIX`).
pub const REUSABLE_DEV_SESSION_PREFIX: &str = "dev-auth-";
/// The database needs an expiry for a configured token with no normal session lifetime (`REUSABLE_DEV_SESSION_EXPIRES_AT`).
fn reusable_dev_session_expires_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("9999-12-31T23:59:59.999Z").expect("a fixed instant").with_timezone(&Utc)
}

/// One fixed credential, `T3CODE_DEV_AUTH_TOKEN`, that web dev servers on one hostname share (`auth/ReusableDevAuth.ts`). It is both the
/// bootstrap credential and the bearer of the session it stands for, kept in a cookie named for its hash so every dev server on the
/// hostname reads the same one. Only a web-mode server with a dev URL has it; the desktop app and a normal server ignore the value.
#[derive(Clone)]
pub struct ReusableDevAuth {
    pub credential: String,
    pub session_id: String,
    pub cookie_name: String,
    hash: [u8; 32],
}

impl ReusableDevAuth {
    /// `resolveReusableDevAuth`: `None` unless the server is in web mode with a dev URL and the token is not empty.
    pub fn resolve(mode: &str, has_dev_url: bool, token: Option<&str>) -> Option<Self> {
        let token = token.filter(|token| mode == "web" && has_dev_url && !token.is_empty())?;
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let id: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        Some(Self { credential: token.to_owned(), session_id: format!("{REUSABLE_DEV_SESSION_PREFIX}{id}"), cookie_name: format!("t3_dev_session_{id}"), hash })
    }
    /// Whether `credential` is the token, compared as hashes in constant time.
    pub fn matches(&self, credential: &str) -> bool {
        let presented: [u8; 32] = Sha256::digest(credential.as_bytes()).into();
        presented.iter().zip(&self.hash).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
    }
}

/// `DevAuthTokenConfig`: the value trimmed; nothing when it is empty; an error when it is too short to be a secret.
pub fn resolve_dev_auth_token(raw: Option<&str>) -> Result<Option<String>, &'static str> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(token) if token.chars().count() >= 32 => Ok(Some(token.to_owned())),
        Some(_) => Err("T3CODE_DEV_AUTH_TOKEN must contain at least 32 characters."),
    }
}

#[derive(Clone)]
pub struct AuthService {
    store: Store,
    dev: Option<Arc<ReusableDevAuth>>,
    secret: [u8; 32],
    pub cookie_name: String,
    pub policy: String,
    desktop_bootstrap: Option<DesktopBootstrapGrant>,
    desktop_mode: bool,
    live: Arc<Live>,
}

impl AuthService {
    pub fn new(
        store: Store,
        secret: [u8; 32],
        cookie_name: String,
        policy: String,
    ) -> Result<Self, AuthError> {
        store.transaction(|transaction| {
            transaction.execute_batch("CREATE TABLE IF NOT EXISTS rust_auth_sessions(session_id TEXT PRIMARY KEY,session_json TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS rust_pairing_credentials(token_hash TEXT PRIMARY KEY,scopes_json TEXT NOT NULL,expires_at INTEGER NOT NULL,consumed_at INTEGER);")?;
            // What the list of pairing links needs, added to stores made before it. A credential from
            // before has no id and is never listed, but it still pairs.
            for (column, definition) in [
                ("id", "TEXT"),
                ("subject", "TEXT NOT NULL DEFAULT 'one-time-token'"),
                ("label", "TEXT"),
                ("created_at", "INTEGER"),
                ("revoked_at", "INTEGER"),
                // The DPoP key the link was issued for, if any: only a request proving that key may use it.
                ("proof_key_thumbprint", "TEXT"),
            ] {
                let present: i64 = transaction.query_row(
                    "SELECT count(*) FROM pragma_table_info('rust_pairing_credentials') WHERE name=?1",
                    [column],
                    |row| row.get(0),
                )?;
                if present == 0 {
                    transaction.execute_batch(&format!(
                        "ALTER TABLE rust_pairing_credentials ADD COLUMN {column} {definition}"
                    ))?;
                }
            }
            Ok(())
        })?;
        Ok(Self {
            store,
            secret,
            cookie_name,
            policy,
            dev: None,
            desktop_bootstrap: None,
            desktop_mode: false,
            live: Arc::new(Live {
                connected: Mutex::new(HashMap::new()),
                changes: tokio::sync::broadcast::channel(256).0,
            }),
        })
    }

    /// The trusted supervisor's reusable grant is process-local, as in the
    /// original PairingGrantStore. Never persist or include these IPC secrets in
    /// the public descriptor or diagnostics.
    pub fn with_desktop_bootstrap(
        mut self,
        token: String,
        secret: Option<String>,
        now: DateTime<Utc>,
    ) -> Self {
        self.desktop_bootstrap = Some(DesktopBootstrapGrant {
            token,
            secret,
            expires_at: now + chrono::Duration::hours(24),
        });
        self
    }
    /// Seeds the session the reusable credential stands for, if this database has none yet (`createIfAbsent`): every server keeps its own
    /// database, signing key and revocation state, and only the credential is shared.
    pub fn with_reusable_dev_auth(mut self, dev: ReusableDevAuth, now: DateTime<Utc>) -> Result<Self, AuthError> {
        let session = Session {
            session_id: dev.session_id.clone(),
            subject: "reusable-dev-token".into(),
            method: "browser-session-cookie".into(),
            scopes: t3_contracts::AUTH_ADMINISTRATIVE_SCOPES.to_vec(),
            client: json!({"label":"Reusable dev token","deviceType":"unknown"}),
            issued_at: Some(now),
            last_connected_at: None,
            expires_at: reusable_dev_session_expires_at(),
            revoked_at: None,
            proof_key_thumbprint: None,
            runtime_mode_ceiling: None,
        };
        self.store.transaction(|transaction| {
            transaction.execute("INSERT OR IGNORE INTO rust_auth_sessions(session_id,session_json) VALUES(?1,?2)", params![session.session_id, serde_json::to_string(&session)?])?;
            Ok(())
        })?;
        self.dev = Some(Arc::new(dev));
        Ok(self)
    }
    /// The reusable dev credential, when this server has one.
    pub fn reusable_dev(&self) -> Option<&ReusableDevAuth> {
        self.dev.as_deref()
    }
    pub fn with_desktop_mode(mut self) -> Self {
        self.desktop_mode = true;
        self
    }
    pub fn descriptor(&self) -> Value {
        let methods = if self.policy == "desktop-managed-local" {
            vec!["desktop-bootstrap"]
        } else if self.desktop_mode {
            vec!["desktop-bootstrap", "one-time-token"]
        } else {
            vec!["one-time-token"]
        };
        json!({"policy":self.policy,"bootstrapMethods":methods,"sessionMethods":["browser-session-cookie","bearer-access-token","dpop-access-token"],"sessionCookieName":self.cookie_name,"serverUpdateScope":"environment:maintain"})
    }

    pub fn session_state(&self, session: Option<&Session>) -> Value {
        let mut state = json!({"authenticated":session.is_some(),"auth":self.descriptor()});
        if let Some(session) = session {
            let grants = t3_contracts::auth_scope_response(&session.scopes);
            state["scopes"] = json!(grants.scopes);
            state["permissions"] = json!(grants.permissions);
            state["sessionMethod"] = json!(session.method);
            state["expiresAt"] = json!(session.expires_at);
        }
        state
    }

    pub fn issue_session(
        &self,
        subject: &str,
        method: &str,
        scopes: Vec<AuthEnvironmentScope>,
        client: Value,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<(Session, String), AuthError> {
        self.issue_session_bound(subject, method, scopes, client, now, ttl, None)
    }

    /// A session that, when `proof_key_thumbprint` is given, only answers to requests that prove that DPoP key.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_session_bound(
        &self,
        subject: &str,
        method: &str,
        scopes: Vec<AuthEnvironmentScope>,
        client: Value,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
        proof_key_thumbprint: Option<String>,
    ) -> Result<(Session, String), AuthError> {
        self.issue_session_with_ceiling(subject, method, scopes, client, now, ttl, proof_key_thumbprint, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn issue_session_with_ceiling(
        &self,
        subject: &str,
        method: &str,
        scopes: Vec<AuthEnvironmentScope>,
        client: Value,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
        proof_key_thumbprint: Option<String>,
        runtime_mode_ceiling: Option<AuthMcpClientAccess>,
    ) -> Result<(Session, String), AuthError> {
        if !matches!(method, "browser-session-cookie" | "bearer-access-token" | "dpop-access-token") {
            return Err(AuthError::Invalid("Unsupported session method."));
        }
        if ttl <= chrono::Duration::zero() {
            return Err(AuthError::Invalid("Session lifetime must be positive."));
        }
        let session = Session {
            session_id: uuid::Uuid::new_v4().to_string(),
            subject: subject.into(),
            method: method.into(),
            scopes,
            client,
            issued_at: Some(now),
            last_connected_at: None,
            expires_at: now + ttl,
            revoked_at: None,
            proof_key_thumbprint,
            runtime_mode_ceiling,
        };
        let token = self.session_token(&session, now)?;
        let replaced = self.store.transaction(|transaction| {
            let mut replaced = Vec::new();
            if subject == "desktop-bootstrap" && method == "bearer-access-token" {
                let mut statement =
                    transaction.prepare("SELECT session_json FROM rust_auth_sessions")?;
                let rows = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                for raw in rows {
                    let mut old: Session = serde_json::from_str(&raw)?;
                    if old.subject == subject && old.method == method && old.revoked_at.is_none() {
                        old.revoked_at = Some(now);
                        transaction.execute(
                            "UPDATE rust_auth_sessions SET session_json=?1 WHERE session_id=?2",
                            params![serde_json::to_string(&old)?, old.session_id],
                        )?;
                        replaced.push(old.session_id);
                    }
                }
            }
            transaction.execute(
                "INSERT INTO rust_auth_sessions(session_id,session_json) VALUES(?1,?2)",
                params![session.session_id, serde_json::to_string(&session)?],
            )?;
            Ok(replaced)
        })?;
        self.publish_issued(&session, replaced);
        Ok((session, token))
    }

    fn session_token(&self, session: &Session, now: DateTime<Utc>) -> Result<String, AuthError> {
        self.sign(&Claims {
            v: 2,
            kind: "session".into(),
            sid: session.session_id.clone(),
            sub: Some(session.subject.clone()),
            scopes: Some(session.scopes.clone()),
            method: Some(session.method.clone()),
            iat: now.timestamp_millis(),
            exp: session.expires_at.timestamp_millis(),
        })
    }

    pub fn verify_session(&self, token: &str, now: DateTime<Utc>) -> Result<Session, AuthError> {
        self.verify_session_for(token, now, false)
    }

    fn verify_session_for(&self, token: &str, now: DateTime<Utc>, mcp_client: bool) -> Result<Session, AuthError> {
        // The reusable dev credential is its own session's bearer; it is not a signed token.
        if let Some(dev) = self.dev.as_ref().filter(|dev| dev.matches(token)) {
            return self.stored_active_session(&dev.session_id, now, mcp_client);
        }
        let claims = self.decode(token)?;
        if claims.kind != "session"
            || !matches!(claims.v, 1 | 2)
            || claims.sub.is_none()
            || claims.scopes.is_none()
            || claims.method.is_none()
        {
            return Err(AuthError::Invalid("Invalid session token payload."));
        }
        if claims.exp <= now.timestamp_millis() {
            return Err(AuthError::Invalid("Session token expired."));
        }
        let session = self.stored_active_session(&claims.sid, now, mcp_client)?;
        if claims.sub.as_deref() != Some(&session.subject)
            || claims.method.as_deref() != Some(&session.method)
            || claims.exp != session.expires_at.timestamp_millis()
        {
            return Err(AuthError::Invalid(
                "Session token does not match its persisted session.",
            ));
        }
        Ok(session)
    }

    pub fn active_session(&self, id: &str, now: DateTime<Utc>) -> Result<Session, AuthError> {
        self.stored_active_session(id, now, false)
    }

    /// The live session `id` names. An MCP client's session (`mcp-client`) is returned only to a caller that asks for it:
    /// everything else refuses it, so an agent's token cannot reach the API around the access it was approved for.
    fn stored_active_session(&self, id: &str, now: DateTime<Utc>, mcp_client: bool) -> Result<Session, AuthError> {
        let session = self
            .store
            .read(|connection| {
                let json = connection
                    .query_row(
                        "SELECT session_json FROM rust_auth_sessions WHERE session_id=?1",
                        [id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                json.map(|json| serde_json::from_str::<Session>(&json))
                    .transpose()
                    .map_err(Into::into)
            })?
            .ok_or(AuthError::Invalid("Unknown session token."))?;
        if session.expires_at <= now {
            return Err(AuthError::Invalid("Session token expired."));
        }
        if session.revoked_at.is_some() {
            return Err(AuthError::Invalid("Session token revoked."));
        }
        if session.subject == MCP_CLIENT_SUBJECT && !mcp_client {
            return Err(AuthError::Invalid(
                "MCP credentials cannot access the client API.",
            ));
        }
        Ok(session)
    }

    // ---- MCP clients (`EnvironmentAuth.ts`: `issueMcpClientSession`, `authenticateMcpClient`, `consumeMcpApprovalCode`) ----

    /// The session an agent signed in through MCP OAuth holds for 30 days: `orchestration:read` alone for read-only access,
    /// otherwise also `orchestration:operate`, with the runtime mode it may never run above.
    pub fn issue_mcp_client_session(&self, label: &str, access: AuthMcpClientAccess, client: Value, now: DateTime<Utc>) -> Result<(Session, String), AuthError> {
        let mut client = client;
        if let Some(object) = client.as_object_mut() {
            object.insert("label".into(), json!(label));
            object.insert("deviceType".into(), json!("bot"));
        }
        let ceiling = (access != AuthMcpClientAccess::ReadOnly).then_some(access);
        self.issue_session_with_ceiling(MCP_CLIENT_SUBJECT, "bearer-access-token", mcp_client_scopes(access), client, now, chrono::Duration::days(MCP_CLIENT_SESSION_TTL_DAYS), None, ceiling)
    }

    /// An MCP client's bearer token: only a live `mcp-client` bearer session passes, and nothing else does here.
    pub fn authenticate_mcp_client(&self, token: &str, now: DateTime<Utc>) -> Result<McpClientSession, AuthError> {
        let session = self.verify_session_for(token, now, true)?;
        if session.subject != MCP_CLIENT_SUBJECT || session.method != "bearer-access-token" {
            return Err(AuthError::Invalid("Only MCP client sessions are accepted here."));
        }
        let access = if session.scopes.contains(&AuthEnvironmentScope::OrchestrationOperate) {
            session.runtime_mode_ceiling.unwrap_or(AuthMcpClientAccess::ApprovalRequired)
        } else {
            AuthMcpClientAccess::ReadOnly
        };
        let label = session.client["label"].as_str().unwrap_or("MCP client").to_owned();
        Ok(McpClientSession { session_id: session.session_id, label, access })
    }

    /// A signed-in browser: its cookie names a live browser session that no DPoP key binds.
    pub fn authenticate_browser_session(&self, token: &str, now: DateTime<Utc>) -> Result<Session, AuthError> {
        let session = self.verify_session(token, now)?;
        if session.method != "browser-session-cookie" || session.proof_key_thumbprint.is_some() {
            return Err(AuthError::Invalid("Not a browser session."));
        }
        Ok(session)
    }

    /// Spends a one-time pairing code as the approval of an MCP sign-in. The code is used up even when it cannot grant
    /// `access`. A code bound to a DPoP key (a T3 Connect client's) is refused without being spent.
    pub fn consume_mcp_approval_code(&self, code: &str, access: AuthMcpClientAccess, now: DateTime<Utc>) -> Result<(), McpApprovalCodeError> {
        let credential = code.trim();
        let (method, scopes): (&str, Vec<AuthEnvironmentScope>) = 'resolved: {
            if let Some(grant) = &self.desktop_bootstrap {
                let valid = if let Some(secret) = &grant.secret {
                    valid_desktop_bootstrap_token(secret, credential, now.timestamp_millis())
                } else {
                    now < grant.expires_at && !grant.token.is_empty() && constant_time_token_eq(&grant.token, credential)
                };
                if valid {
                    break 'resolved ("desktop-bootstrap", t3_contracts::AUTH_ADMINISTRATIVE_SCOPES.to_vec());
                }
            }
            let consumed = self
                .store
                .transaction(|transaction| {
                    let hash = token_hash(credential);
                    let row: Option<(String, i64, Option<i64>, String, Option<i64>, Option<String>)> = transaction
                        .query_row("SELECT scopes_json,expires_at,consumed_at,id,revoked_at,proof_key_thumbprint FROM rust_pairing_credentials WHERE token_hash=?1", [&hash], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))
                        .optional()?;
                    let Some((scopes, expires_at, consumed, id, revoked, bound)) = row else { return Ok(None) };
                    if consumed.is_some() || revoked.is_some() || expires_at <= now.timestamp_millis() || bound.is_some() {
                        return Ok(None);
                    }
                    transaction.execute("UPDATE rust_pairing_credentials SET consumed_at=?1 WHERE token_hash=?2", params![now.timestamp_millis(), hash])?;
                    Ok(Some((serde_json::from_str::<Vec<AuthEnvironmentScope>>(&scopes)?, id)))
                })
                .map_err(|_| McpApprovalCodeError::Internal)?;
            let Some((scopes, id)) = consumed else { return Err(McpApprovalCodeError::UnknownOrUsed) };
            self.publish(AccessChange::PairingLinkRemoved(id));
            ("one-time-token", scopes)
        };
        if method != "one-time-token" {
            return Err(McpApprovalCodeError::NotAPairingCode);
        }
        if mcp_client_scopes(access).iter().all(|scope| scopes.contains(scope)) {
            Ok(())
        } else {
            Err(McpApprovalCodeError::InsufficientScope)
        }
    }

    pub fn issue_websocket_ticket(
        &self,
        session: &Session,
        now: DateTime<Utc>,
    ) -> Result<Value, AuthError> {
        let active = self.active_session(&session.session_id, now)?;
        let expiry = (now + chrono::Duration::minutes(5)).min(active.expires_at);
        let ticket = self.sign(&Claims {
            v: 1,
            kind: "websocket".into(),
            sid: active.session_id,
            sub: None,
            scopes: None,
            method: None,
            iat: now.timestamp_millis(),
            exp: expiry.timestamp_millis(),
        })?;
        Ok(json!({"ticket":ticket,"expiresAt":expiry}))
    }

    pub fn verify_websocket_ticket(
        &self,
        ticket: &str,
        now: DateTime<Utc>,
    ) -> Result<Session, AuthError> {
        let claims = self.decode(ticket)?;
        if claims.v != 1 || claims.kind != "websocket" {
            return Err(AuthError::Invalid("Invalid websocket token payload."));
        }
        if claims.exp <= now.timestamp_millis() {
            return Err(AuthError::Invalid("Websocket token expired."));
        }
        // A ticket for a reusable dev session is honoured only by the server whose credential that session stands for.
        if claims.sid.starts_with(REUSABLE_DEV_SESSION_PREFIX) && Some(claims.sid.as_str()) != self.dev.as_ref().map(|dev| dev.session_id.as_str()) {
            return Err(AuthError::Invalid("Unknown websocket session."));
        }
        self.active_session(&claims.sid, now)
    }

    pub fn revoke(&self, session_id: &str, now: DateTime<Utc>) -> Result<bool, AuthError> {
        let revoked = self.store.transaction(|transaction| {
            let raw = transaction
                .query_row(
                    "SELECT session_json FROM rust_auth_sessions WHERE session_id=?1",
                    [session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(raw) = raw else { return Ok(false) };
            let mut session: Session = serde_json::from_str(&raw)?;
            if session.revoked_at.is_some() {
                return Ok(false);
            };
            session.revoked_at = Some(now);
            transaction.execute(
                "UPDATE rust_auth_sessions SET session_json=?1 WHERE session_id=?2",
                params![serde_json::to_string(&session)?, session_id],
            )?;
            Ok(true)
        })?;
        if revoked {
            self.live.connected.lock().unwrap().remove(session_id);
            self.publish(AccessChange::ClientRemoved(session_id.to_owned()));
        }
        Ok(revoked)
    }

    /// Revokes every session but `current`, answering how many it revoked.
    pub fn revoke_all_except(&self, current: &str, now: DateTime<Utc>) -> Result<usize, AuthError> {
        let revoked = self.store.transaction(|transaction| {
            let mut statement = transaction.prepare(
                "SELECT session_id,session_json FROM rust_auth_sessions WHERE session_id<>?1 AND json_extract(session_json,'$.revokedAt') IS NULL",
            )?;
            let rows = statement
                .query_map([current], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            let mut revoked = Vec::new();
            for (id, raw) in rows {
                let mut session: Session = serde_json::from_str(&raw)?;
                session.revoked_at = Some(now);
                transaction.execute(
                    "UPDATE rust_auth_sessions SET session_json=?1 WHERE session_id=?2",
                    params![serde_json::to_string(&session)?, id],
                )?;
                revoked.push(id);
            }
            Ok(revoked)
        })?;
        {
            let mut connected = self.live.connected.lock().unwrap();
            for id in &revoked {
                connected.remove(id);
            }
        }
        for id in &revoked {
            self.publish(AccessChange::ClientRemoved(id.clone()));
        }
        Ok(revoked.len())
    }

    /// `EnvironmentAuth.revokeClientSession`: another session, never the caller's own.
    pub fn revoke_client_session(
        &self,
        current: &str,
        target: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, RevokeClientError> {
        if current == target {
            return Err(RevokeClientError::CurrentSession);
        }
        Ok(self.revoke(target, now)?)
    }

    fn publish(&self, change: AccessChange) {
        // Nobody listening is the ordinary case.
        let _ = self.live.changes.send(change);
    }

    /// Changes from now on; the snapshot a subscriber starts from is read after subscribing.
    pub fn subscribe_access(&self) -> tokio::sync::broadcast::Receiver<AccessChange> {
        self.live.changes.subscribe()
    }

    fn publish_issued(&self, session: &Session, replaced: Vec<String>) {
        if !replaced.is_empty() {
            let mut connected = self.live.connected.lock().unwrap();
            for id in &replaced {
                connected.remove(id);
            }
        }
        for id in replaced {
            self.publish(AccessChange::ClientRemoved(id));
        }
        if let Some(view) = self.client_view(session, false) {
            self.publish(AccessChange::ClientUpserted(view));
        }
    }

    /// The session as a client lists it, or nothing for one a list does not show: revoked, or expired
    /// with no socket open.
    fn client_view(&self, session: &Session, current: bool) -> Option<AuthClientSession> {
        let connected = self.live.connected.lock().unwrap().contains_key(&session.session_id);
        client_session_view(session, connected, current)
    }

    /// A socket opened for the session. The first one stamps its last connection time.
    pub fn mark_connected(&self, session_id: &str, now: DateTime<Utc>) {
        let first = {
            let mut connected = self.live.connected.lock().unwrap();
            let count = connected.entry(session_id.to_owned()).or_insert(0);
            *count += 1;
            *count == 1
        };
        if first {
            let _ = self.update_session(session_id, |session| {
                if session.revoked_at.is_none() {
                    session.last_connected_at = Some(now);
                }
            });
        }
        self.publish_loaded(session_id, now, false);
    }

    /// A socket closed for the session; the session leaves the connected set with its last one.
    pub fn mark_disconnected(&self, session_id: &str, now: DateTime<Utc>) {
        {
            let mut connected = self.live.connected.lock().unwrap();
            match connected.get(session_id).copied().unwrap_or(0) {
                0 | 1 => {
                    connected.remove(session_id);
                }
                count => {
                    connected.insert(session_id.to_owned(), count - 1);
                }
            }
        }
        self.publish_loaded(session_id, now, true);
    }

    /// Publishes the stored session if a list would show it. `removal` says to publish its removal
    /// if not (a closing socket); a connecting one that is gone publishes nothing.
    fn publish_loaded(&self, session_id: &str, now: DateTime<Utc>, removal: bool) {
        let session = self.stored_session(session_id).ok().flatten();
        let view = session.and_then(|session| {
            self.client_view(&session, false).filter(|view| view.connected || view.expires_at.0 > now)
        });
        match view {
            Some(view) => self.publish(AccessChange::ClientUpserted(view)),
            None if removal => self.publish(AccessChange::ClientRemoved(session_id.to_owned())),
            None => {}
        }
    }

    fn stored_session(&self, id: &str) -> Result<Option<Session>, AuthError> {
        Ok(self.store.read(|connection| {
            let json = connection
                .query_row("SELECT session_json FROM rust_auth_sessions WHERE session_id=?1", [id], |row| row.get::<_, String>(0))
                .optional()?;
            json.map(|json| serde_json::from_str::<Session>(&json)).transpose().map_err(Into::into)
        })?)
    }

    fn update_session(&self, id: &str, change: impl FnOnce(&mut Session)) -> Result<(), AuthError> {
        Ok(self.store.transaction(|transaction| {
            let raw = transaction
                .query_row("SELECT session_json FROM rust_auth_sessions WHERE session_id=?1", [id], |row| row.get::<_, String>(0))
                .optional()?;
            if let Some(raw) = raw {
                let mut session: Session = serde_json::from_str(&raw)?;
                change(&mut session);
                transaction.execute("UPDATE rust_auth_sessions SET session_json=?1 WHERE session_id=?2", params![serde_json::to_string(&session)?, id])?;
            }
            Ok(())
        })?)
    }

    /// `EnvironmentAuth.listClientSessions`: the sessions that can be revoked, those that may manage
    /// access first, then those with a socket open, then the newest.
    pub fn list_client_sessions(&self, current: &str, now: DateTime<Utc>) -> Result<Vec<AuthClientSession>, AuthError> {
        let mut rows: Vec<Session> = self.store.read(|connection| {
            let mut statement = connection.prepare("SELECT session_json FROM rust_auth_sessions WHERE json_extract(session_json,'$.revokedAt') IS NULL")?;
            let raw = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            raw.iter().map(|json| serde_json::from_str::<Session>(json).map_err(StoreError::from)).collect::<Result<Vec<_>, _>>()
        })?;
        // The query's own order (newest first, then id), which the stable sort below keeps among equals.
        rows.sort_by(|left, right| (right.issued_at(), &right.session_id).cmp(&(left.issued_at(), &left.session_id)));
        let connected = self.live.connected.lock().unwrap().clone();
        let mut views: Vec<AuthClientSession> = rows
            .iter()
            .filter(|session| connected.contains_key(&session.session_id) || session.expires_at > now)
            .filter_map(|session| client_session_view(session, connected.contains_key(&session.session_id), session.session_id == current))
            .collect();
        views.sort_by_key(|view| (!can_manage_access(view), !view.connected));
        Ok(views)
    }

    /// `EnvironmentAuth.listPairingLinks`: the links that can still be used, newest first.
    pub fn list_pairing_links(&self, now: DateTime<Utc>) -> Result<Vec<AuthPairingLink>, AuthError> {
        Ok(self.store.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id,scopes_json,subject,label,created_at,expires_at FROM rust_pairing_credentials WHERE id IS NOT NULL AND revoked_at IS NULL AND consumed_at IS NULL AND expires_at>?1 AND subject<>?2 ORDER BY created_at DESC,id DESC",
            )?;
            let rows = statement
                .query_map(params![now.timestamp_millis(), ADMINISTRATIVE_BOOTSTRAP_SUBJECT], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?, row.get::<_, Option<i64>>(4)?, row.get::<_, i64>(5)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|(id, scopes, subject, label, created, expires)| {
                    let scopes: Vec<AuthEnvironmentScope> = serde_json::from_str(&scopes)?;
                    let created = created.unwrap_or(expires);
                    Ok(pairing_link_view(&id, &scopes, &subject, label.as_deref(), millis(created), millis(expires)))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        })?)
    }

    /// `EnvironmentAuth.revokePairingLink`: false for a link already used, revoked or never issued.
    pub fn revoke_pairing_link(&self, id: &str, now: DateTime<Utc>) -> Result<bool, AuthError> {
        let revoked = self.store.transaction(|transaction| {
            Ok(transaction.execute(
                "UPDATE rust_pairing_credentials SET revoked_at=?1 WHERE id=?2 AND revoked_at IS NULL AND consumed_at IS NULL",
                params![now.timestamp_millis(), id],
            )? > 0)
        })?;
        if revoked {
            self.publish(AccessChange::PairingLinkRemoved(id.to_owned()));
        }
        Ok(revoked)
    }

    /// Pairing credentials are one-use and consume atomically with issuing the session.
    pub fn create_pairing_credential(
        &self,
        scopes: &[AuthEnvironmentScope],
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<String, AuthError> {
        Ok(self.issue_pairing_link(scopes, ONE_TIME_TOKEN_SUBJECT, None, now, ttl)?.credential)
    }

    /// `PairingGrantStore.issueOneTimeToken`: a link another device pairs with once, until it expires or
    /// is revoked. Its label names the session it becomes.
    pub fn issue_pairing_link(
        &self,
        scopes: &[AuthEnvironmentScope],
        subject: &str,
        label: Option<&str>,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<IssuedPairingLink, AuthError> {
        self.issue_pairing_link_bound(scopes, subject, label, now, ttl, None)
    }

    /// A link only a request that proves the DPoP key `proof_key_thumbprint` can use (`proofKeyThumbprint` of
    /// `issueOneTimeToken`); T3 Connect issues these for the clients its relay vouches for.
    pub fn issue_pairing_link_bound(
        &self,
        scopes: &[AuthEnvironmentScope],
        subject: &str,
        label: Option<&str>,
        now: DateTime<Utc>,
        ttl: chrono::Duration,
        proof_key_thumbprint: Option<&str>,
    ) -> Result<IssuedPairingLink, AuthError> {
        if scopes.iter().any(|scope| !scope.is_grantable()) || ttl <= chrono::Duration::zero() {
            return Err(AuthError::Invalid("Invalid pairing grant."));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let credential = pairing_credential();
        let hash = token_hash(&credential);
        let expires_at = now + ttl;
        let label = label.map(str::to_owned);
        self.store.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO rust_pairing_credentials(token_hash,scopes_json,expires_at,id,subject,label,created_at,proof_key_thumbprint) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![hash, serde_json::to_string(scopes)?, expires_at.timestamp_millis(), id, subject, label, now.timestamp_millis(), proof_key_thumbprint],
            )?;
            Ok(())
        })?;
        if subject != ADMINISTRATIVE_BOOTSTRAP_SUBJECT {
            self.publish(AccessChange::PairingLinkUpserted(pairing_link_view(&id, scopes, subject, label.as_deref(), millis(now.timestamp_millis()), millis(expires_at.timestamp_millis()))));
        }
        Ok(IssuedPairingLink { id, credential, label, created_at: now, expires_at })
    }

    pub fn exchange_pairing_credential(
        &self,
        credential: &str,
        client: Value,
        now: DateTime<Utc>,
    ) -> Result<(Session, String), AuthError> {
        self.exchange_pairing_credential_replacing(credential, client, now, None)
    }

    /// A browser pairing that, when it comes with the cookie of an earlier browser session, ends that
    /// session in the same step (the original's `replaceSessionId`).
    pub fn exchange_pairing_credential_replacing(
        &self,
        credential: &str,
        client: Value,
        now: DateTime<Utc>,
        previous_token: Option<&str>,
    ) -> Result<(Session, String), AuthError> {
        let replace = previous_token
            .and_then(|token| self.verify_session(token, now).ok())
            .filter(|session| session.method == "browser-session-cookie")
            .map(|session| session.session_id);
        self.exchange_pairing(credential, client, now, "browser-session-cookie", None, replace, None)
    }

    pub fn exchange_pairing_bearer(
        &self,
        credential: &str,
        scopes: Option<&[AuthEnvironmentScope]>,
        client: Value,
        now: DateTime<Utc>,
    ) -> Result<(Session, String), AuthError> {
        self.exchange_pairing(credential, client, now, "bearer-access-token", scopes, None, None)
    }

    /// The exchange of a client that proved a DPoP key: the session is bound to it and lives an hour
    /// (`exchangeBootstrapCredentialForAccessToken` with a `proofKeyThumbprint`). A link issued for another key is refused
    /// and stays unused.
    pub fn exchange_pairing_dpop(
        &self,
        credential: &str,
        scopes: Option<&[AuthEnvironmentScope]>,
        client: Value,
        now: DateTime<Utc>,
        proof_key_thumbprint: &str,
    ) -> Result<(Session, String), AuthError> {
        self.exchange_pairing(credential, client, now, "dpop-access-token", scopes, None, Some(proof_key_thumbprint))
    }

    fn exchange_pairing(
        &self,
        credential: &str,
        client: Value,
        now: DateTime<Utc>,
        method: &str,
        requested: Option<&[AuthEnvironmentScope]>,
        replace: Option<String>,
        proof_key_thumbprint: Option<&str>,
    ) -> Result<(Session, String), AuthError> {
        // A DPoP session is short: the client proves its key again with every request and exchanges again when it ends.
        let session_ttl = if proof_key_thumbprint.is_some() { chrono::Duration::hours(1) } else { chrono::Duration::days(30) };
        // The reusable dev credential grants what its own session holds (`resolveBootstrapGrant`), to a child session of its own.
        if let Some(dev) = self.dev.as_ref().filter(|dev| dev.matches(credential)) {
            let holder = self.stored_active_session(&dev.session_id, now, false)?;
            let scopes: Vec<AuthEnvironmentScope> = match requested {
                Some(requested) => requested.iter().copied().filter(|scope| scope.is_grantable() && holder.scopes.contains(scope)).fold(Vec::new(), |mut scopes, scope| {
                    if !scopes.contains(&scope) {
                        scopes.push(scope);
                    }
                    scopes
                }),
                None => holder.scopes.clone(),
            };
            if scopes.is_empty() {
                return Err(AuthError::ScopeNotGranted);
            }
            return self.issue_session_bound("reusable-dev-token-child", method, scopes, client, now, session_ttl, proof_key_thumbprint.map(str::to_owned));
        }
        if let Some(grant) = &self.desktop_bootstrap {
            let valid = if let Some(secret) = &grant.secret {
                valid_desktop_bootstrap_token(secret, credential, now.timestamp_millis())
            } else {
                now < grant.expires_at
                    && !grant.token.is_empty()
                    && constant_time_token_eq(&grant.token, credential)
            };
            if valid {
                let grants = t3_contracts::AUTH_ADMINISTRATIVE_SCOPES;
                let scopes = requested
                    .map(|requested| {
                        let mut scopes = Vec::new();
                        for scope in requested {
                            if grants.contains(scope) && !scopes.contains(scope) {
                                scopes.push(*scope);
                            }
                        }
                        scopes
                    })
                    .unwrap_or_else(|| grants.to_vec());
                if scopes.is_empty() {
                    return Err(AuthError::ScopeNotGranted);
                }
                return self.issue_session_bound(
                    "desktop-bootstrap",
                    method,
                    scopes,
                    client,
                    now,
                    session_ttl,
                    proof_key_thumbprint.map(str::to_owned),
                );
            }
        }
        let (session, link_id, replaced) = self.store.transaction(|transaction| {
            let hash=token_hash(credential);
            let raw:Option<(String,i64,Option<i64>,Option<String>,String,Option<String>,Option<i64>,Option<String>)>=transaction.query_row("SELECT scopes_json,expires_at,consumed_at,id,subject,label,revoked_at,proof_key_thumbprint FROM rust_pairing_credentials WHERE token_hash=?1",[&hash],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).optional()?;
            let Some((scopes,expires_at,consumed,id,subject,label,revoked,bound_to))=raw else {return Ok(None)};
            if consumed.is_some() || revoked.is_some() || expires_at<=now.timestamp_millis() {return Ok(None)};
            // A link issued for a DPoP key is used by a request that proves that key, and by nothing else; a refusal leaves it unused.
            if bound_to.as_deref().is_some_and(|bound| Some(bound)!=proof_key_thumbprint) {return Ok(None)};
            let grants:Vec<AuthEnvironmentScope>=serde_json::from_str(&scopes)?;
            let scopes=requested.map(|requested|{let mut granted=Vec::new();for scope in requested {if scope.is_grantable() && grants.contains(scope) && !granted.contains(scope) {granted.push(*scope);}}granted}).unwrap_or(grants);
            if scopes.is_empty() { return Ok(Some(Err(AuthError::ScopeNotGranted))); }
            // The link's label names the client that used it.
            let mut client=client.clone();
            if let (Some(label),Some(object))=(label.as_deref().filter(|label|!label.is_empty()),client.as_object_mut()) {object.insert("label".into(),Value::String(label.to_owned()));}
            let session=Session{session_id:uuid::Uuid::new_v4().to_string(),subject,method:method.into(),scopes,client,issued_at:Some(now),last_connected_at:None,expires_at:now+session_ttl,revoked_at:None,proof_key_thumbprint:proof_key_thumbprint.map(str::to_owned),runtime_mode_ceiling:None};
            let mut replaced=Vec::new();
            if let Some(previous)=&replace {
                // Only a live session of the same method is replaced, as the original's UPDATE does.
                let stored=transaction.query_row("SELECT session_json FROM rust_auth_sessions WHERE session_id=?1",[previous],|row|row.get::<_,String>(0)).optional()?;
                if let Some(stored)=stored {
                    let mut old:Session=serde_json::from_str(&stored)?;
                    if old.revoked_at.is_none() && old.method==method && old.expires_at>now {
                        old.revoked_at=Some(now);
                        transaction.execute("UPDATE rust_auth_sessions SET session_json=?1 WHERE session_id=?2",params![serde_json::to_string(&old)?,previous])?;
                        replaced.push(previous.clone());
                    }
                }
            }
            transaction.execute("UPDATE rust_pairing_credentials SET consumed_at=?1 WHERE token_hash=?2",params![now.timestamp_millis(),hash])?;
            transaction.execute("INSERT INTO rust_auth_sessions(session_id,session_json) VALUES(?1,?2)",params![session.session_id,serde_json::to_string(&session)?])?;
            Ok(Some(Ok((session,id,replaced))))
        })?.ok_or(AuthError::Invalid("Pairing credential is unknown, expired, or consumed."))??;
        if let Some(id) = link_id {
            self.publish(AccessChange::PairingLinkRemoved(id));
        }
        self.publish_issued(&session, replaced);
        let token = self.session_token(&session, now)?;
        Ok((session, token))
    }

    fn sign(&self, claims: &Claims) -> Result<String, AuthError> {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts a 32-byte key");
        mac.update(payload.as_bytes());
        Ok(format!(
            "{payload}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        ))
    }

    fn decode(&self, token: &str) -> Result<Claims, AuthError> {
        let mut parts = token.split('.');
        let (Some(payload), Some(signature), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(AuthError::Invalid("Malformed token."));
        };
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| AuthError::Invalid("Invalid token signature."))?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC accepts a 32-byte key");
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| AuthError::Invalid("Invalid token signature."))?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| AuthError::Invalid("Invalid token payload."))?;
        serde_json::from_slice(&payload).map_err(Into::into)
    }
}

/// What a client says about itself when it pairs (the token exchange form carries these).
#[derive(Debug, Default)]
pub struct PresentedClient {
    pub label: Option<String>,
    pub device_type: Option<AuthClientMetadataDeviceType>,
    pub os: Option<String>,
}

/// `deriveAuthClientMetadata` (`auth/utils.ts`): what the list of clients shows for a session, from the
/// user agent and the address the request came from, with what the client presented taking precedence.
pub fn derive_client_metadata(user_agent: Option<&str>, remote: Option<std::net::IpAddr>, presented: &PresentedClient) -> Value {
    let user_agent = user_agent.map(str::trim).filter(|value| !value.is_empty());
    let lowered = user_agent.map(str::to_lowercase);
    let has = |needle: &str| lowered.as_deref().is_some_and(|agent| agent.contains(needle));
    let any = |needles: &[&str]| needles.iter().any(|needle| has(needle));
    let device_type = presented.device_type.unwrap_or(if lowered.is_none() {
        AuthClientMetadataDeviceType::Unknown
    } else if any(&["bot", "crawler", "spider", "slurp", "curl", "wget"]) {
        AuthClientMetadataDeviceType::Bot
    } else if any(&["ipad", "tablet"]) {
        AuthClientMetadataDeviceType::Tablet
    } else if any(&["iphone", "mobile"]) {
        AuthClientMetadataDeviceType::Mobile
    } else {
        AuthClientMetadataDeviceType::Desktop
    });
    let os = presented.os.clone().or_else(|| {
        let inferred = if any(&["iphone", "ipad", "ipod"]) {
            "iOS"
        } else if has("android") {
            "Android"
        } else if any(&["mac os x", "macintosh"]) {
            "macOS"
        } else if has("windows nt") {
            "Windows"
        } else if has("linux") {
            "Linux"
        } else {
            return None;
        };
        Some(inferred.to_owned())
    });
    let browser = if has("edg/") {
        Some("Edge")
    } else if has("opr/") {
        Some("Opera")
    } else if has("firefox/") {
        Some("Firefox")
    } else if has("electron/") {
        Some("Electron")
    } else if any(&["chrome/", "crios/"]) {
        Some("Chrome")
    } else if has("safari/") {
        Some("Safari")
    } else {
        None
    };
    let mut client = serde_json::Map::new();
    if let Some(label) = presented.label.as_deref().filter(|label| !label.is_empty()) {
        client.insert("label".into(), json!(label));
    }
    if let Some(address) = remote {
        let address = address.to_string();
        client.insert("ipAddress".into(), json!(address.strip_prefix("::ffff:").unwrap_or(&address)));
    }
    if let Some(agent) = user_agent {
        client.insert("userAgent".into(), json!(agent));
    }
    client.insert("deviceType".into(), serde_json::to_value(device_type).expect("device type"));
    if let Some(os) = os {
        client.insert("os".into(), json!(os));
    }
    if let Some(browser) = browser {
        client.insert("browser".into(), json!(browser));
    }
    Value::Object(client)
}

fn millis(value: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(value).unwrap_or_default()
}

fn pairing_credential() -> String {
    let bytes = rand::random::<[u8; PAIRING_TOKEN_LENGTH]>();
    bytes
        .iter()
        .map(|byte| PAIRING_TOKEN_ALPHABET[*byte as usize % PAIRING_TOKEN_ALPHABET.len()] as char)
        .collect()
}

fn pairing_link_view(
    id: &str,
    scopes: &[AuthEnvironmentScope],
    subject: &str,
    label: Option<&str>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> AuthPairingLink {
    let grants = t3_contracts::auth_scope_response(scopes);
    AuthPairingLink {
        id: TrimmedNonEmptyString::new(id).expect("stored id"),
        scopes: grants.scopes,
        permissions: Some(grants.permissions),
        subject: TrimmedNonEmptyString::new(subject).expect("stored subject"),
        label: label.and_then(|label| TrimmedNonEmptyString::new(label).ok()),
        created_at: created_at.into(),
        expires_at: expires_at.into(),
    }
}

fn client_session_view(session: &Session, connected: bool, current: bool) -> Option<AuthClientSession> {
    let method = match session.method.as_str() {
        "browser-session-cookie" => ServerAuthSessionMethod::BrowserSessionCookie,
        "bearer-access-token" => ServerAuthSessionMethod::BearerAccessToken,
        "dpop-access-token" => ServerAuthSessionMethod::DpopAccessToken,
        _ => return None,
    };
    if session.revoked_at.is_some() {
        return None;
    }
    let grants = t3_contracts::auth_scope_response(&session.scopes);
    let text = |key: &str| session.client[key].as_str().and_then(|value| TrimmedNonEmptyString::new(value).ok());
    let device_type = serde_json::from_value(session.client["deviceType"].clone()).unwrap_or(AuthClientMetadataDeviceType::Unknown);
    Some(AuthClientSession {
        session_id: AuthSessionId::new(&session.session_id).ok()?,
        subject: TrimmedNonEmptyString::new(&session.subject).ok()?,
        scopes: grants.scopes,
        permissions: Some(grants.permissions),
        method,
        client: AuthClientMetadata {
            label: text("label"),
            ip_address: text("ipAddress"),
            user_agent: text("userAgent"),
            device_type,
            os: text("os"),
            browser: text("browser"),
        },
        issued_at: session.issued_at().into(),
        expires_at: session.expires_at.into(),
        last_connected_at: session.last_connected_at.map(UtcDateTime::from),
        connected,
        current,
    })
}

impl Session {
    /// Sessions stored before the list of clients existed carry no issue time; they were all issued
    /// for the thirty days that end at their expiry.
    fn issued_at(&self) -> DateTime<Utc> {
        self.issued_at.unwrap_or(self.expires_at - chrono::Duration::days(30))
    }
}

/// Whether a listed session may manage access: the original lists those first.
fn can_manage_access(session: &AuthClientSession) -> bool {
    session.permissions.as_deref().is_some_and(|permissions| permissions.contains(&AuthEnvironmentScope::AccessWrite))
}

pub fn is_remote_reachable_host(host: &str) -> bool {
    !host.is_empty()
        && !matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        && !host.starts_with("127.")
}
const DESKTOP_TOKEN_WINDOW_MS: i64 = 12 * 60 * 60 * 1000;
fn desktop_bootstrap_token(secret: &str, window: i64) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts every key length");
    mac.update(format!("t3-desktop-bootstrap:{window}").as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn constant_time_token_eq(expected: &str, presented: &str) -> bool {
    // Fixed-key HMAC verification keeps arbitrary UTF-8 token comparisons
    // constant-time once their byte lengths match, including legacy IPC tokens.
    if expected.len() != presented.len() {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(b"t3-desktop-comparison").unwrap();
    mac.update(expected.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut presented_mac = Hmac::<Sha256>::new_from_slice(b"t3-desktop-comparison").unwrap();
    presented_mac.update(presented.as_bytes());
    presented_mac.verify_slice(&digest).is_ok()
}
fn valid_desktop_bootstrap_token(secret: &str, token: &str, now_ms: i64) -> bool {
    let window = now_ms.div_euclid(DESKTOP_TOKEN_WINDOW_MS);
    [window - 1, window, window + 1]
        .iter()
        .any(|window| constant_time_token_eq(&desktop_bootstrap_token(secret, *window), token))
}
fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_auth_matches_original_token_and_policy_witnesses() {
        let mut count = 0;
        for row in include_str!("../tests/fixtures/desktop-auth.jsonl").lines() {
            let row: Value = serde_json::from_str(row).unwrap();
            if row["op"] == "token" {
                let secret = row["secret"].as_str().unwrap();
                let now = row["now"].as_i64().unwrap();
                assert_eq!(
                    desktop_bootstrap_token(secret, now.div_euclid(DESKTOP_TOKEN_WINDOW_MS)),
                    row["current"]
                );
                assert_eq!(
                    valid_desktop_bootstrap_token(secret, row["token"].as_str().unwrap(), now),
                    row["valid"].as_bool().unwrap()
                );
            } else {
                let desktop = row["mode"] == "desktop";
                let remote = is_remote_reachable_host(row["host"].as_str().unwrap());
                let policy = if remote {
                    "remote-reachable"
                } else if desktop {
                    "desktop-managed-local"
                } else {
                    "loopback-browser"
                };
                let mut auth = AuthService::new(
                    Store::memory().unwrap(),
                    [7; 32],
                    "fixture-session".into(),
                    policy.into(),
                )
                .unwrap();
                if desktop {
                    auth = auth.with_desktop_mode();
                }
                let descriptor = auth.descriptor();
                assert_eq!(descriptor["policy"], row["policy"]);
                assert_eq!(descriptor["bootstrapMethods"], row["bootstrapMethods"]);
            }
            count += 1;
        }
        assert_eq!(count, 209);
    }
    #[test]
    fn reusable_desktop_grants_enforce_static_expiry_rotation_and_scope_intersection() {
        let now = now();
        let scopes = [AuthEnvironmentScope::OrchestrationRead];
        let static_auth = service().with_desktop_bootstrap("trusted IPC token".into(), None, now);
        for _ in 0..2 {
            let (session, _) = static_auth
                .exchange_pairing_bearer("trusted IPC token", Some(&scopes), json!({}), now)
                .unwrap();
            assert_eq!(session.subject, "desktop-bootstrap");
            assert_eq!(session.scopes, scopes);
        }
        let (_, old) = static_auth
            .exchange_pairing_bearer("trusted IPC token", Some(&scopes), json!({}), now)
            .unwrap();
        let (_, fresh) = static_auth
            .exchange_pairing_bearer("trusted IPC token", Some(&scopes), json!({}), now)
            .unwrap();
        assert!(static_auth.verify_session(&old, now).is_err());
        assert!(static_auth.verify_session(&fresh, now).is_ok());
        assert!(
            static_auth
                .exchange_pairing_bearer(
                    "trusted IPC token",
                    None,
                    json!({}),
                    now + chrono::Duration::hours(24)
                )
                .is_err()
        );
        let auth =
            service().with_desktop_bootstrap("legacy-token".into(), Some("密钥".into()), now);
        assert!(
            auth.exchange_pairing_bearer("legacy-token", None, json!({}), now)
                .is_err()
        );
        let token = desktop_bootstrap_token(
            "密钥",
            now.timestamp_millis().div_euclid(DESKTOP_TOKEN_WINDOW_MS),
        );
        assert!(matches!(
            auth.exchange_pairing_bearer(&token, Some(&[]), json!({}), now),
            Err(AuthError::ScopeNotGranted)
        ));
        assert!(
            auth.exchange_pairing_bearer(&token, Some(&scopes), json!({}), now)
                .is_ok()
        );
        assert!(
            auth.exchange_pairing_bearer(
                &token,
                None,
                json!({}),
                now + chrono::Duration::hours(12)
            )
            .is_ok()
        );
        assert!(
            auth.exchange_pairing_bearer(
                &token,
                None,
                json!({}),
                now + chrono::Duration::hours(24)
            )
            .is_err()
        );
        assert!(!auth.descriptor().to_string().contains("密钥"));
    }

    fn now() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }
    fn service() -> AuthService {
        AuthService::new(
            Store::memory().unwrap(),
            [7; 32],
            "t3_session_test".into(),
            "loopback-browser".into(),
        )
        .unwrap()
    }
    #[test]
    fn signed_sessions_and_tickets_enforce_expiry_and_revocation() {
        let service = service();
        let (session, token) = service
            .issue_session(
                "client",
                "bearer-access-token",
                vec![AuthEnvironmentScope::OrchestrationRead],
                json!({"deviceType":"unknown"}),
                now(),
                chrono::Duration::hours(1),
            )
            .unwrap();
        assert_eq!(
            service.verify_session(&token, now()).unwrap().session_id,
            session.session_id
        );
        let ticket = service.issue_websocket_ticket(&session, now()).unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            service
                .verify_websocket_ticket(&ticket, now() + chrono::Duration::minutes(5))
                .is_err()
        );
        service.revoke(&session.session_id, now()).unwrap();
        assert!(service.verify_session(&token, now()).is_err());
        assert!(service.verify_websocket_ticket(&ticket, now()).is_err());
    }
    #[test]
    fn tampering_and_cross_purpose_tokens_are_rejected() {
        let service = service();
        let (session, token) = service
            .issue_session(
                "client",
                "bearer-access-token",
                vec![],
                json!({}),
                now(),
                chrono::Duration::hours(1),
            )
            .unwrap();
        assert!(service.verify_websocket_ticket(&token, now()).is_err());
        let ticket = service.issue_websocket_ticket(&session, now()).unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(service.verify_session(&ticket, now()).is_err());
        let mut tampered = token.into_bytes();
        tampered[4] = if tampered[4] == b'A' { b'B' } else { b'A' };
        assert!(
            service
                .verify_session(std::str::from_utf8(&tampered).unwrap(), now())
                .is_err()
        );
    }
    #[test]
    fn pairing_is_one_use_and_cannot_restore_ungranted_permissions() {
        let service = service();
        let credential = service
            .create_pairing_credential(
                &[AuthEnvironmentScope::OrchestrationRead],
                now(),
                chrono::Duration::minutes(5),
            )
            .unwrap();
        let (session, _) = service
            .exchange_pairing_credential(&credential, json!({}), now())
            .unwrap();
        assert_eq!(
            session.scopes,
            vec![AuthEnvironmentScope::OrchestrationRead]
        );
        assert!(
            service
                .exchange_pairing_credential(&credential, json!({}), now())
                .is_err()
        );
    }
}
