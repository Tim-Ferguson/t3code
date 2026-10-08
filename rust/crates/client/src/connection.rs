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

    pub fn is_same_origin(&self, origin: &str) -> bool {
        Url::parse(origin).is_ok_and(|origin| self.base.origin() == origin.origin())
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

    pub fn thread_history(&self, thread_id: &str, cursor: &str) -> Url {
        let mut url = self.http("api/orchestration/threads/");
        url.path_segments_mut()
            .expect("http endpoint")
            .pop_if_empty()
            .push(thread_id)
            .push("history");
        url.query_pairs_mut().append_pair("cursor", cursor);
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

/// Original Effect RPC retry policy: exponential 500ms × 1.5, capped at 5s.
pub fn reconnect_delay_ms(failure_count: u32) -> u64 {
    (500.0 * 1.5_f64.powi(failure_count.min(32) as i32))
        .round()
        .min(5_000.0) as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatAction {
    Ping,
    Timeout,
}

/// Effect RPC checks the previous pong on each five-second heartbeat. Regular
/// stream traffic is not a substitute for a pong, and callers own the timer.
#[derive(Debug, Default)]
pub struct Heartbeat {
    awaiting_pong: bool,
}
impl Heartbeat {
    pub fn tick(&mut self) -> HeartbeatAction {
        if self.awaiting_pong {
            HeartbeatAction::Timeout
        } else {
            self.awaiting_pong = true;
            HeartbeatAction::Ping
        }
    }
    pub fn pong(&mut self) {
        self.awaiting_pong = false;
    }
    pub fn reset(&mut self) {
        self.awaiting_pong = false;
    }
}
