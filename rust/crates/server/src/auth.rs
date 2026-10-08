use crate::persistence::{Store, StoreError};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use t3_contracts::AuthEnvironmentScope;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Invalid(&'static str),
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
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
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
pub struct AuthService {
    store: Store,
    secret: [u8; 32],
    pub cookie_name: String,
    pub policy: String,
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
            Ok(())
        })?;
        Ok(Self {
            store,
            secret,
            cookie_name,
            policy,
        })
    }

    pub fn descriptor(&self) -> Value {
        json!({"policy":self.policy,"bootstrapMethods":["one-time-token"],"sessionMethods":["browser-session-cookie","bearer-access-token"],"sessionCookieName":self.cookie_name,"serverUpdateScope":"environment:maintain"})
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
        if !matches!(method, "browser-session-cookie" | "bearer-access-token") {
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
            expires_at: now + ttl,
            revoked_at: None,
        };
        let token = self.session_token(&session, now)?;
        self.store.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO rust_auth_sessions(session_id,session_json) VALUES(?1,?2)",
                params![session.session_id, serde_json::to_string(&session)?],
            )?;
            Ok(())
        })?;
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
        let session = self.active_session(&claims.sid, now)?;
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
        if session.subject == "mcp-client" {
            return Err(AuthError::Invalid(
                "MCP credentials cannot access the client API.",
            ));
        }
        Ok(session)
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
        self.active_session(&claims.sid, now)
    }

    pub fn revoke(&self, session_id: &str, now: DateTime<Utc>) -> Result<bool, AuthError> {
        Ok(self.store.transaction(|transaction| {
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
        })?)
    }

    /// Pairing credentials are one-use and consume atomically with issuing the session.
    pub fn create_pairing_credential(
        &self,
        scopes: &[AuthEnvironmentScope],
        now: DateTime<Utc>,
        ttl: chrono::Duration,
    ) -> Result<String, AuthError> {
        if scopes.iter().any(|scope| !scope.is_grantable()) || ttl <= chrono::Duration::zero() {
            return Err(AuthError::Invalid("Invalid pairing grant."));
        }
        let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let hash = token_hash(&token);
        self.store.transaction(|transaction| {
            transaction.execute("INSERT INTO rust_pairing_credentials(token_hash,scopes_json,expires_at) VALUES(?1,?2,?3)",params![hash,serde_json::to_string(scopes)?,(now+ttl).timestamp_millis()])?;
            Ok(())
        })?;
        Ok(token)
    }

    pub fn exchange_pairing_credential(
        &self,
        credential: &str,
        client: Value,
        now: DateTime<Utc>,
    ) -> Result<(Session, String), AuthError> {
        let result=self.store.transaction(|transaction| {
            let hash=token_hash(credential);
            let raw:Option<(String,i64,Option<i64>)>=transaction.query_row("SELECT scopes_json,expires_at,consumed_at FROM rust_pairing_credentials WHERE token_hash=?1",[&hash],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
            let Some((scopes,expires_at,consumed))=raw else {return Ok(None)};
            if consumed.is_some() || expires_at<=now.timestamp_millis() {return Ok(None)};
            let session=Session{session_id:uuid::Uuid::new_v4().to_string(),subject:"client".into(),method:"browser-session-cookie".into(),scopes:serde_json::from_str(&scopes)?,client,expires_at:now+chrono::Duration::days(30),revoked_at:None};
            transaction.execute("UPDATE rust_pairing_credentials SET consumed_at=?1 WHERE token_hash=?2",params![now.timestamp_millis(),hash])?;
            transaction.execute("INSERT INTO rust_auth_sessions(session_id,session_json) VALUES(?1,?2)",params![session.session_id,serde_json::to_string(&session)?])?;
            Ok(Some(session))
        })?.ok_or(AuthError::Invalid("Pairing credential is unknown, expired, or consumed."))?;
        let token = self.session_token(&result, now)?;
        Ok((result, token))
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

fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
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
