use url::Url;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConnectionError {
    #[error("Enter an http:// or https:// server address")]
    InvalidAddress,
    #[error("Server addresses must not contain embedded credentials")]
    EmbeddedCredentials,
}

/// Endpoints are resolved against the selected environment, never against a
/// build-time localhost origin. Existing path prefixes are retained for proxies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentEndpoint {
    base: Url,
}

impl EnvironmentEndpoint {
    pub fn new(address: &str) -> Result<Self, ConnectionError> {
        let mut base = Url::parse(address.trim()).map_err(|_| ConnectionError::InvalidAddress)?;
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(ConnectionError::InvalidAddress);
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(ConnectionError::EmbeddedCredentials);
        }
        base.set_query(None);
        base.set_fragment(None);
        base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
        Ok(Self { base })
    }

    pub fn http(&self, path: &str) -> Url {
        self.base
            .join(path.trim_start_matches('/'))
            .expect("relative endpoint")
    }

    pub fn socket(&self, ticket: Option<&str>, surface: &str) -> Url {
        let mut url = self.http("ws");
        url.set_scheme(if self.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .expect("websocket scheme");
        {
            let mut query = url.query_pairs_mut();
            if let Some(ticket) = ticket {
                query.append_pair("wsTicket", ticket);
            }
            query.append_pair("orchestrationProtocol", "2");
            query.append_pair("clientSurface", surface);
        }
        url
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Blocked(String),
    Interrupted(String),
}

/// Reconnect delays are bounded and deterministic; the supervisor supplies
/// jitter. A successful handshake resets the failure count.
pub fn reconnect_delay_ms(failure_count: u32) -> u64 {
    500u64
        .saturating_mul(1u64 << failure_count.min(6))
        .min(30_000)
}
