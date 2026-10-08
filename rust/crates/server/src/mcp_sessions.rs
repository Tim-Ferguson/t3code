//! Process-owned MCP credentials. Only SHA-256 hashes remain in the registry.
use crate::mcp_invocation::{InvocationScope, McpCapability, ThreadCaller};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use indexmap::IndexSet;
use rand::TryRngCore;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use t3_contracts::{EnvironmentId, ProviderInstanceId, ThreadId};

pub const DEFAULT_LIVENESS_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub struct CredentialRequest {
    pub thread_id: ThreadId,
    pub provider_instance_id: ProviderInstanceId,
    pub browser_tools_available: Option<bool>,
    pub capabilities: Option<IndexSet<McpCapability>>,
}
#[derive(Clone)]
pub struct ProviderSessionConfig {
    pub environment_id: EnvironmentId,
    pub thread_id: ThreadId,
    pub provider_session_id: String,
    pub provider_instance_id: ProviderInstanceId,
    pub endpoint: String,
    pub authorization_header: String,
    pub browser_tools_available: bool,
    pub capabilities: IndexSet<McpCapability>,
}
impl std::fmt::Debug for ProviderSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSessionConfig")
            .field("thread_id", &self.thread_id)
            .field("provider_session_id", &self.provider_session_id)
            .field("provider_instance_id", &self.provider_instance_id)
            .field("endpoint", &self.endpoint)
            .field("authorization_header", &"[redacted]")
            .field("capabilities", &self.capabilities)
            .finish()
    }
}
struct Record {
    scope: InvocationScope,
    last_alive_at: i64,
}
struct Inner {
    environment: EnvironmentId,
    endpoint: String,
    clock: Clock,
    liveness_window_ms: i64,
    records: Mutex<HashMap<[u8; 32], Record>>,
}
#[derive(Clone)]
pub struct McpSessionRegistry(Arc<Inner>);
pub fn endpoint(address: Option<SocketAddr>) -> String {
    match address {
        Some(address) => format!(
            "http://{}:{}/mcp",
            if address.ip().is_unspecified() {
                "127.0.0.1".into()
            } else {
                match address.ip() {
                    std::net::IpAddr::V4(ip) => ip.to_string(),
                    std::net::IpAddr::V6(ip) => format!("[{ip}]"),
                }
            },
            address.port()
        ),
        None => "http://127.0.0.1/mcp".into(),
    }
}
fn hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
impl McpSessionRegistry {
    pub fn new(
        environment: EnvironmentId,
        address: Option<SocketAddr>,
        clock: Clock,
        liveness_window_ms: i64,
    ) -> Self {
        Self(Arc::new(Inner {
            environment,
            endpoint: endpoint(address),
            clock,
            liveness_window_ms,
            records: Mutex::new(HashMap::new()),
        }))
    }
    fn prune(&self, records: &mut HashMap<[u8; 32], Record>, now: i64) {
        records.retain(|_, record| {
            now as i128 - record.last_alive_at as i128 <= self.0.liveness_window_ms as i128
        });
    }
    pub fn issue(
        &self,
        request: CredentialRequest,
    ) -> Result<ProviderSessionConfig, rand::rand_core::OsError> {
        let now = (self.0.clock)();
        let provider_session_id = uuid::Uuid::new_v4().to_string();
        let mut bytes = [0; 32];
        rand::rngs::OsRng.try_fill_bytes(&mut bytes)?;
        let raw_token = URL_SAFE_NO_PAD.encode(bytes);
        let mut capabilities = IndexSet::from([
            McpCapability::Orchestration,
            McpCapability::Worktree,
            McpCapability::PullRequests,
        ]);
        if let Some(explicit) = request.capabilities {
            capabilities.extend(explicit);
        } else if request.browser_tools_available.unwrap_or(true) {
            capabilities.insert(McpCapability::Preview);
        }
        let scope = InvocationScope {
            environment_id: self.0.environment.clone(),
            capabilities: capabilities.clone(),
            issued_at: now,
            request_namespace: provider_session_id.clone(),
            thread: Some(ThreadCaller {
                thread_id: request.thread_id.clone(),
                provider_session_id: provider_session_id.clone(),
                provider_instance_id: request.provider_instance_id.clone(),
            }),
            client: None,
        };
        let mut records = self.0.records.lock().unwrap();
        self.prune(&mut records, now);
        records.insert(
            hash(&raw_token),
            Record {
                scope,
                last_alive_at: now,
            },
        );
        Ok(ProviderSessionConfig {
            environment_id: self.0.environment.clone(),
            thread_id: request.thread_id,
            provider_session_id,
            provider_instance_id: request.provider_instance_id,
            endpoint: self.0.endpoint.clone(),
            authorization_header: format!("Bearer {raw_token}"),
            browser_tools_available: capabilities.contains(&McpCapability::Preview),
            capabilities,
        })
    }
    pub fn resolve(&self, raw_token: &str) -> Option<InvocationScope> {
        if raw_token.is_empty() {
            return None;
        }
        let now = (self.0.clock)();
        let mut records = self.0.records.lock().unwrap();
        self.prune(&mut records, now);
        let record = records.get_mut(&hash(raw_token))?;
        record.last_alive_at = now;
        Some(record.scope.clone())
    }
    pub fn touch(&self, thread: &ThreadId) {
        let now = (self.0.clock)();
        let mut records = self.0.records.lock().unwrap();
        self.prune(&mut records, now);
        for record in records.values_mut() {
            if record
                .scope
                .thread
                .as_ref()
                .is_some_and(|caller| &caller.thread_id == thread)
            {
                record.last_alive_at = now;
            }
        }
    }
    pub fn revoke_provider_session(&self, session: &str) {
        self.0.records.lock().unwrap().retain(|_, record| {
            record
                .scope
                .thread
                .as_ref()
                .is_none_or(|caller| caller.provider_session_id != session)
        });
    }
    pub fn revoke_thread(&self, thread: &ThreadId) {
        self.0.records.lock().unwrap().retain(|_, record| {
            record
                .scope
                .thread
                .as_ref()
                .is_none_or(|caller| &caller.thread_id != thread)
        });
    }
    pub fn revoke_all(&self) {
        self.0.records.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    fn fixture() -> (McpSessionRegistry, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(1000));
        let clock = now.clone();
        (
            McpSessionRegistry::new(
                "environment-1".parse().unwrap(),
                Some("127.0.0.1:43123".parse().unwrap()),
                Arc::new(move || clock.load(Ordering::SeqCst)),
                100,
            ),
            now,
        )
    }
    fn request(thread: &str) -> CredentialRequest {
        CredentialRequest {
            thread_id: thread.parse().unwrap(),
            provider_instance_id: "codex".parse().unwrap(),
            browser_tools_available: None,
            capabilities: None,
        }
    }
    fn token(config: &ProviderSessionConfig) -> &str {
        config.authorization_header.strip_prefix("Bearer ").unwrap()
    }
    #[test]
    fn credentials_are_hash_only_and_revocation_preserves_sibling_threads_and_sessions() {
        let (registry, _) = fixture();
        let first = registry.issue(request("first")).unwrap();
        let sibling = registry.issue(request("first")).unwrap();
        let other = registry.issue(request("other")).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(token(&first)).unwrap().len(), 32);
        assert_ne!(token(&first), token(&sibling));
        assert!(uuid::Uuid::parse_str(&first.provider_session_id).is_ok());
        let records = registry.0.records.lock().unwrap();
        assert!(records.contains_key(&hash(token(&first))));
        assert_eq!(records.len(), 3);
        drop(records);
        assert!(!format!("{first:?}").contains(token(&first)));
        registry.revoke_provider_session(&first.provider_session_id);
        assert!(registry.resolve(token(&first)).is_none());
        assert!(registry.resolve(token(&sibling)).is_some());
        assert!(registry.resolve(token(&other)).is_some());
        registry.revoke_thread(&"first".parse().unwrap());
        assert!(registry.resolve(token(&sibling)).is_none());
        assert!(registry.resolve(token(&other)).is_some());
        registry.revoke_all();
        assert!(registry.resolve(token(&other)).is_none());
        assert!(registry.resolve("").is_none());
        assert!(registry.resolve("unknown").is_none());
    }
    #[test]
    fn capability_order_and_explicit_preview_overrides_match_original() {
        let (registry, _) = fixture();
        let default = registry.issue(request("default")).unwrap();
        assert_eq!(
            default.capabilities.iter().copied().collect::<Vec<_>>(),
            vec![
                McpCapability::Orchestration,
                McpCapability::Worktree,
                McpCapability::PullRequests,
                McpCapability::Preview
            ]
        );
        let mut denied = request("denied");
        denied.browser_tools_available = Some(false);
        assert!(!registry.issue(denied).unwrap().browser_tools_available);
        let mut explicit = request("device");
        explicit.capabilities = Some(IndexSet::from([McpCapability::Device]));
        let issued = registry.issue(explicit).unwrap();
        assert!(!issued.browser_tools_available);
        assert!(issued.capabilities.contains(&McpCapability::Device));
        let mut empty = request("empty");
        empty.capabilities = Some(IndexSet::new());
        assert!(!registry.issue(empty).unwrap().browser_tools_available);
        let mut override_preview = request("override");
        override_preview.browser_tools_available = Some(false);
        override_preview.capabilities = Some(IndexSet::from([McpCapability::Preview]));
        assert!(
            registry
                .issue(override_preview)
                .unwrap()
                .browser_tools_available
        );
    }
    #[test]
    fn inclusive_liveness_resolve_refresh_touch_pruning_and_expired_threads() {
        let (registry, now) = fixture();
        let issued = registry.issue(request("first")).unwrap();
        now.store(1100, Ordering::SeqCst);
        assert!(registry.resolve(token(&issued)).is_some());
        now.store(1200, Ordering::SeqCst);
        assert!(registry.resolve(token(&issued)).is_some());
        now.store(1301, Ordering::SeqCst);
        assert!(registry.resolve(token(&issued)).is_none());
        let live = registry.issue(request("live")).unwrap();
        let dead = registry.issue(request("dead")).unwrap();
        for _ in 0..10 {
            now.fetch_add(99, Ordering::SeqCst);
            registry.touch(&"live".parse().unwrap());
        }
        assert!(registry.resolve(token(&live)).is_some());
        assert!(registry.resolve(token(&dead)).is_none());
        now.fetch_add(101, Ordering::SeqCst);
        registry.touch(&"live".parse().unwrap());
        assert!(
            registry.resolve(token(&live)).is_none(),
            "touch cannot resurrect a pruned credential"
        );
        let expired = registry.issue(request("expired")).unwrap();
        now.fetch_add(101, Ordering::SeqCst);
        registry.issue(request("new")).unwrap();
        assert!(
            !registry
                .0
                .records
                .lock()
                .unwrap()
                .contains_key(&hash(token(&expired)))
        );
    }
    #[test]
    fn original_bound_endpoint_hosts_and_unix_fallback() {
        for (address, expected) in [
            ("100.64.0.40:43123", "http://100.64.0.40:43123/mcp"),
            ("0.0.0.0:43123", "http://127.0.0.1:43123/mcp"),
            ("[::]:43123", "http://127.0.0.1:43123/mcp"),
            ("[::1]:43123", "http://[::1]:43123/mcp"),
            ("127.0.0.1:43123", "http://127.0.0.1:43123/mcp"),
        ] {
            assert_eq!(endpoint(Some(address.parse().unwrap())), expected);
        }
        assert_eq!(endpoint(None), "http://127.0.0.1/mcp");
    }
}
