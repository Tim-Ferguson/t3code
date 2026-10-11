//! The server's outbound HTTP to the T3 Connect relay (the "control plane"). Two credentials are
//! in play: the CLI's OAuth access token for the `/v1/client/*` routes, and the relay-issued
//! environment credential for the `/v1/environments/*` routes. Every route and body here follows
//! `packages/contracts/src/relay.ts`; timeouts and failure handling follow `cloud/CloudLink.ts`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::relay_error::{RelayRequestError, filter_relay_response};
use crate::types::*;

/// Default per-request timeout of the relay calls the original makes (`Effect.timeout("10 seconds")`).
pub const DEFAULT_RELAY_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Provisioning a tunnel can take longer than ten seconds.
pub const MANAGED_ENDPOINT_PROVISION_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
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

pub type RelayResult<T> = Result<T, RelayRequestError>;

#[async_trait]
pub trait ControlPlane: Send + Sync {
    // Routes authenticated with the CLI's OAuth access token.

    /// `POST /v1/client/environment-link-challenges`
    async fn create_link_challenge(
        &self,
        relay_url: &str,
        access_token: &str,
        request: &LinkChallengeRequest,
    ) -> RelayResult<LinkChallengeResponse>;

    /// `POST /v1/client/environment-links` (two-minute timeout: it may provision a tunnel)
    async fn link_environment(
        &self,
        relay_url: &str,
        access_token: &str,
        request: &LinkRequest,
    ) -> RelayResult<LinkResponse>;

    /// `DELETE /v1/client/environment-links/:environmentId`
    async fn unlink_environment(
        &self,
        relay_url: &str,
        access_token: &str,
        environment_id: &str,
    ) -> RelayResult<OkResponse>;

    /// `DELETE /v1/client/environment-links/:environmentId/tunnel`: deletes the provisioned tunnel
    /// but keeps the link and its hostname. `ok: false` means a concurrent provision owns it.
    async fn release_environment_tunnel(
        &self,
        relay_url: &str,
        access_token: &str,
        environment_id: &str,
    ) -> RelayResult<OkResponse>;

    // Routes authenticated with the environment credential installed at link time.

    /// `POST /v1/environments/:environmentId/tunnel/recovery`
    async fn register_tunnel_recovery(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        request: &RecoveryRegistrationRequest,
    ) -> RelayResult<RecoveryRegistrationResponse>;

    /// `POST /v1/environments/:environmentId/tunnel` (two-minute timeout)
    async fn recover_tunnel(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        request: &RecoveryRequest,
    ) -> RelayResult<RecoveryResponse>;

    /// `POST /v1/environments/:environmentId/link-preferences`
    async fn update_link_preferences(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        request: &LinkPreferencesRequest,
    ) -> RelayResult<LinkPreferencesRequest>;

    /// `POST /v1/environments/:environmentId/hooks/wake`
    async fn wake_held_hooks(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
    ) -> RelayResult<WakeHeldHooksResponse>;

    /// `POST /v1/environments/:environmentId/threads/:threadId/agent-activity`
    async fn publish_agent_activity(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        thread_id: &str,
        request: &AgentActivityPublishRequest,
    ) -> RelayResult<PublishResponse>;
}

/// The production [`ControlPlane`]: reqwest over HTTPS. The relay URL is passed per call because it
/// comes from the stored link (the relay a link was installed against), not from current config.
#[derive(Clone)]
pub struct HttpControlPlane {
    client: reqwest::Client,
}

impl Default for HttpControlPlane {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpControlPlane {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("t3-remote/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("http client"),
        }
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        url: String,
        token: &str,
        body: Option<serde_json::Value>,
        timeout: Duration,
    ) -> RelayResult<T> {
        let mut request = self.client.request(method, url).bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let exchange = async {
            let response = request
                .send()
                .await
                .map_err(|_| RelayRequestError::transport("TransportError"))?;
            let status = response.status().as_u16();
            let ray = response
                .headers()
                .get("cf-ray")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let bytes = response
                .bytes()
                .await
                .map_err(|_| RelayRequestError::transport("TransportError"))?;
            filter_relay_response(status, ray.as_deref(), &bytes)?;
            serde_json::from_slice(&bytes).map_err(|_| RelayRequestError::unexpected_response())
        };
        // A timeout reports as an unexpected response, like the original (see `unexpected_response`).
        tokio::time::timeout(timeout, exchange)
            .await
            .unwrap_or_else(|_| Err(RelayRequestError::unexpected_response()))
    }

    async fn post<T: DeserializeOwned>(
        &self,
        url: String,
        token: &str,
        body: &impl Serialize,
        timeout: Duration,
    ) -> RelayResult<T> {
        let body = serde_json::to_value(body).expect("request json");
        self.send(reqwest::Method::POST, url, token, Some(body), timeout)
            .await
    }
}

#[async_trait]
impl ControlPlane for HttpControlPlane {
    async fn create_link_challenge(
        &self,
        relay_url: &str,
        access_token: &str,
        request: &LinkChallengeRequest,
    ) -> RelayResult<LinkChallengeResponse> {
        self.post(
            format!("{relay_url}/v1/client/environment-link-challenges"),
            access_token,
            request,
            DEFAULT_RELAY_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn link_environment(
        &self,
        relay_url: &str,
        access_token: &str,
        request: &LinkRequest,
    ) -> RelayResult<LinkResponse> {
        self.post(
            format!("{relay_url}/v1/client/environment-links"),
            access_token,
            request,
            MANAGED_ENDPOINT_PROVISION_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn unlink_environment(
        &self,
        relay_url: &str,
        access_token: &str,
        environment_id: &str,
    ) -> RelayResult<OkResponse> {
        self.send(
            reqwest::Method::DELETE,
            format!(
                "{relay_url}/v1/client/environment-links/{}",
                encode_uri_component(environment_id)
            ),
            access_token,
            None,
            // The original sets no timeout here; the CLI waits on the relay as long as it takes.
            Duration::from_secs(24 * 60 * 60),
        )
        .await
    }

    async fn release_environment_tunnel(
        &self,
        relay_url: &str,
        access_token: &str,
        environment_id: &str,
    ) -> RelayResult<OkResponse> {
        self.send(
            reqwest::Method::DELETE,
            format!(
                "{relay_url}/v1/client/environment-links/{}/tunnel",
                encode_uri_component(environment_id)
            ),
            access_token,
            None,
            // The caller bounds shutdown (ten seconds) itself.
            Duration::from_secs(24 * 60 * 60),
        )
        .await
    }

    async fn register_tunnel_recovery(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        request: &RecoveryRegistrationRequest,
    ) -> RelayResult<RecoveryRegistrationResponse> {
        self.post(
            format!(
                "{relay_url}/v1/environments/{}/tunnel/recovery",
                encode_uri_component(environment_id)
            ),
            environment_credential,
            request,
            DEFAULT_RELAY_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn recover_tunnel(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        request: &RecoveryRequest,
    ) -> RelayResult<RecoveryResponse> {
        self.post(
            format!(
                "{relay_url}/v1/environments/{}/tunnel",
                encode_uri_component(environment_id)
            ),
            environment_credential,
            request,
            MANAGED_ENDPOINT_PROVISION_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn update_link_preferences(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        request: &LinkPreferencesRequest,
    ) -> RelayResult<LinkPreferencesRequest> {
        self.post(
            format!(
                "{relay_url}/v1/environments/{}/link-preferences",
                encode_uri_component(environment_id)
            ),
            environment_credential,
            request,
            DEFAULT_RELAY_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn wake_held_hooks(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
    ) -> RelayResult<WakeHeldHooksResponse> {
        self.post(
            format!(
                "{relay_url}/v1/environments/{}/hooks/wake",
                encode_uri_component(environment_id)
            ),
            environment_credential,
            &serde_json::json!({}),
            DEFAULT_RELAY_REQUEST_TIMEOUT,
        )
        .await
    }

    async fn publish_agent_activity(
        &self,
        relay_url: &str,
        environment_credential: &str,
        environment_id: &str,
        thread_id: &str,
        request: &AgentActivityPublishRequest,
    ) -> RelayResult<PublishResponse> {
        self.post(
            format!(
                "{relay_url}/v1/environments/{}/threads/{}/agent-activity",
                encode_uri_component(environment_id),
                encode_uri_component(thread_id)
            ),
            environment_credential,
            request,
            DEFAULT_RELAY_REQUEST_TIMEOUT,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::encode_uri_component;

    #[test]
    fn encodes_like_encode_uri_component() {
        assert_eq!(encode_uri_component("env_123"), "env_123");
        assert_eq!(encode_uri_component("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(encode_uri_component("ü"), "%C3%BC");
        assert_eq!(encode_uri_component("-_.!~*'()"), "-_.!~*'()");
    }
}
