//! Thread-scoped credentials reserved before opening an external provider.
use crate::{
    mcp_invocation::McpCapability,
    mcp_sessions::{CredentialRequest, McpSessionRegistry, ProviderSessionConfig},
};
use indexmap::IndexSet;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use t3_contracts::{ProviderInstanceId, ThreadId};

struct Slot {
    config: ProviderSessionConfig,
    reservations: usize,
}
struct Inner {
    registry: McpSessionRegistry,
    bridge: PathBuf,
    slots: Mutex<HashMap<ThreadId, Slot>>,
    device_environment: Option<indexmap::IndexMap<String, String>>,
}
#[derive(Clone)]
pub struct ProviderMcpSessions(Arc<Inner>);
pub(crate) struct CredentialLease {
    owner: Arc<Inner>,
    pub(crate) config: ProviderSessionConfig,
}
impl ProviderMcpSessions {
    pub fn new(registry: McpSessionRegistry, bridge_command: PathBuf) -> Self {
        Self::new_with_device_environment(registry, bridge_command, None)
    }
    /// The source accepts an optional already-scoped device CLI environment.
    /// Normal startup leaves it absent; device_open supplies explicit targets.
    pub fn new_with_device_environment(
        registry: McpSessionRegistry,
        bridge_command: PathBuf,
        environment: Option<indexmap::IndexMap<String, String>>,
    ) -> Self {
        Self(Arc::new(Inner {
            registry,
            bridge: bridge_command,
            slots: Default::default(),
            device_environment: environment,
        }))
    }
    pub(crate) fn reserve(
        &self,
        thread: ThreadId,
        instance: ProviderInstanceId,
        browser: bool,
        device: bool,
    ) -> Result<CredentialLease, String> {
        // Validation, reservation and rotation share a lock: no release can
        // revoke the credential between reuse validation and admission.
        let mut slots = self.0.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&thread) {
            let token = slot
                .config
                .authorization_header
                .strip_prefix("Bearer ")
                .unwrap();
            if slot.config.provider_instance_id == instance
                && slot.config.capabilities.contains(&McpCapability::Preview) == browser
                && slot.config.capabilities.contains(&McpCapability::Device) == device
                && self.0.registry.resolve(token).is_some()
            {
                slot.reservations += 1;
                return Ok(CredentialLease {
                    owner: self.0.clone(),
                    config: slot.config.clone(),
                });
            }
        }
        self.0.registry.revoke_thread(&thread);
        let mut capabilities = IndexSet::new();
        if browser {
            capabilities.insert(McpCapability::Preview);
        }
        if device {
            capabilities.insert(McpCapability::Device);
        }
        let mut config = self
            .0
            .registry
            .issue(CredentialRequest {
                thread_id: thread.clone(),
                provider_instance_id: instance,
                browser_tools_available: Some(browser),
                capabilities: Some(capabilities),
            })
            .map_err(|error| format!("Could not issue provider MCP credential: {error}"))?;
        if device {
            config.agent_device_environment = self.0.device_environment.clone();
        }
        slots.insert(
            thread,
            Slot {
                config: config.clone(),
                reservations: 1,
            },
        );
        Ok(CredentialLease {
            owner: self.0.clone(),
            config,
        })
    }
    #[cfg(test)]
    pub(crate) fn config(&self, thread: &str) -> Option<ProviderSessionConfig> {
        self.0
            .slots
            .lock()
            .unwrap()
            .get(&thread.parse().ok()?)
            .map(|slot| slot.config.clone())
    }
}
impl CredentialLease {
    pub(crate) fn apply_acp_environment(&self, environment: &mut HashMap<String, String>) {
        self.apply_device_environment(environment);
        environment.insert("T3_ACP_MCP_ENDPOINT".into(), self.config.endpoint.clone());
        environment.insert(
            "T3_ACP_MCP_AUTHORIZATION".into(),
            self.config.authorization_header.clone(),
        );
        // The source standalone fallback invokes the current executable. This
        // native runner accepts acp-mcp-call directly, without Node or a JS entrypoint.
        environment.insert(
            "T3_ACP_MCP_NODE".into(),
            self.owner.bridge.to_string_lossy().into_owned(),
        );
        environment.remove("T3_ACP_MCP_ENTRYPOINT");
    }
    pub(crate) fn apply_device_environment(&self, environment: &mut HashMap<String, String>) {
        if self.config.agent_device_environment.is_none() {
            return;
        }
        // ProviderProcess inherits the host environment; include inherited PATH
        // in the merge only when the provider supplied neither spelling.
        if !environment.contains_key("PATH") && !environment.contains_key("Path") {
            if let Ok(path) = std::env::var("PATH") {
                environment.insert("PATH".into(), path);
            }
        }
        crate::provider_instructions::device_environment(
            environment,
            self.config.agent_device_environment.as_ref(),
        );
    }
    pub(crate) fn touch(&self) {
        self.owner.registry.touch(&self.config.thread_id);
    }
    pub(crate) fn stdio_server(&self) -> t3_acp::types::McpServer {
        t3_acp::types::McpServer::LegacyStdio {
            name:"t3-code".into(),command:self.owner.bridge.to_string_lossy().into(),
            args:Some(vec!["acp-mcp-bridge".into()]),
            env:Some(vec![
                serde_json::from_value(serde_json::json!({"name":"T3_ACP_MCP_ENDPOINT","value":self.config.endpoint})).expect("literal MCP environment entry"),
                serde_json::from_value(serde_json::json!({"name":"T3_ACP_MCP_AUTHORIZATION","value":self.config.authorization_header})).expect("literal MCP environment entry"),
            ]),meta:t3_acp::types::Optional::Missing,
        }
    }
}
impl Drop for CredentialLease {
    fn drop(&mut self) {
        let mut slots = self.owner.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&self.config.thread_id) {
            if slot.config.provider_session_id == self.config.provider_session_id {
                slot.reservations -= 1;
                if slot.reservations > 0 {
                    return;
                }
                slots.remove(&self.config.thread_id);
            }
        }
        // Releasing an older actor must never clear a replacement's grant.
        self.owner
            .registry
            .revoke_provider_session(&self.config.provider_session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    fn fixture() -> (ProviderMcpSessions, McpSessionRegistry, Arc<AtomicI64>) {
        let now = Arc::new(AtomicI64::new(0));
        let clock = now.clone();
        let registry = McpSessionRegistry::new(
            "environment".parse().unwrap(),
            None,
            Arc::new(move || clock.load(Ordering::SeqCst)),
            100,
        );
        (
            ProviderMcpSessions::new(registry.clone(), PathBuf::from("/t3-server")),
            registry,
            now,
        )
    }
    fn reserve(
        sessions: &ProviderMcpSessions,
        instance: &str,
        browser: bool,
        device: bool,
    ) -> CredentialLease {
        sessions
            .reserve(
                "thread".parse().unwrap(),
                instance.parse().unwrap(),
                browser,
                device,
            )
            .unwrap()
    }
    fn valid(registry: &McpSessionRegistry, lease: &CredentialLease) -> bool {
        registry
            .resolve(
                lease
                    .config
                    .authorization_header
                    .strip_prefix("Bearer ")
                    .unwrap(),
            )
            .is_some()
    }
    #[test]
    fn overlapping_actors_reuse_credentials_until_last_owned_session_is_reaped() {
        let (sessions, registry, _) = fixture();
        let first = reserve(&sessions, "codex", false, false);
        let second = reserve(&sessions, "codex", false, false);
        assert_eq!(
            first.config.provider_session_id,
            second.config.provider_session_id
        );
        assert!(valid(&registry, &first));
        let config = second.config.clone();
        drop(first);
        assert!(valid(&registry, &second));
        drop(second);
        assert!(
            registry
                .resolve(config.authorization_header.strip_prefix("Bearer ").unwrap())
                .is_none()
        );
        assert!(sessions.config("thread").is_none());
    }
    #[test]
    fn capability_and_instance_rotation_preserve_replacements_when_old_actors_finish() {
        let (sessions, registry, _) = fixture();
        let first = reserve(&sessions, "codex", true, false);
        let second = reserve(&sessions, "codex", false, true);
        assert!(!valid(&registry, &first));
        assert!(valid(&registry, &second));
        assert!(!second.config.capabilities.contains(&McpCapability::Preview));
        assert!(second.config.capabilities.contains(&McpCapability::Device));
        drop(first);
        assert!(valid(&registry, &second));
        let replacement = reserve(&sessions, "agent", false, true);
        assert!(!valid(&registry, &second));
        drop(second);
        assert!(valid(&registry, &replacement));
        assert_eq!(
            sessions.config("thread").unwrap().provider_session_id,
            replacement.config.provider_session_id
        );
    }
    #[test]
    fn expired_credentials_rotate_and_real_turn_touches_keep_live_sessions_valid() {
        let (sessions, registry, now) = fixture();
        let first = reserve(&sessions, "codex", false, false);
        now.store(101, Ordering::SeqCst);
        let second = reserve(&sessions, "codex", false, false);
        assert_ne!(
            first.config.provider_session_id,
            second.config.provider_session_id
        );
        drop(first);
        now.store(190, Ordering::SeqCst);
        second.touch();
        now.store(250, Ordering::SeqCst);
        assert!(valid(&registry, &second));
    }
}
