//! Provider-flavor client callbacks and the source ACP runtime-policy boundary.
use crate::{
    acp_client_policy::{self, Disposition, Grants},
    acp_client_terminals::{Options, Terminals},
};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use t3_acp::{AcpError, Client, RpcError};
#[derive(Clone)]
pub(crate) struct Services {
    policy: Arc<Mutex<Context>>,
    terminals: Option<Terminals>,
    environment: IndexMap<String, String>,
    embedded: Arc<Mutex<IndexMap<(String, String), Vec<String>>>>,
    mcp: Arc<Mutex<Option<crate::acp_mcp_callback::Bridge>>>,
}
struct Context {
    latest: Value,
    turn_key: Option<String>,
    grants: Grants,
}
impl Services {
    pub(crate) fn new(
        registry_agent_id: &str,
        cwd: &Path,
        environment: IndexMap<String, String>,
        policy: Value,
    ) -> Self {
        let terminals = (registry_agent_id == "devin").then(|| {
            let mut options = Options::new(cwd.into());
            options.environment = environment.clone();
            options.shell_commands = true;
            Terminals::new(options)
        });
        Self {
            policy: Arc::new(Mutex::new(Context {
                latest: policy,
                turn_key: None,
                grants: Grants::default(),
            })),
            terminals,
            environment,
            embedded: Arc::new(Mutex::new(IndexMap::new())),
            mcp: Default::default(),
        }
    }
    #[cfg(test)]
    pub(crate) fn owned_terminal(
        &self,
        id: &str,
    ) -> Option<(u32, tokio::sync::watch::Receiver<bool>)> {
        self.terminals.as_ref()?.owned_terminal(id)
    }
    pub(crate) fn has_terminals(&self) -> bool {
        self.terminals.is_some()
    }
    pub(crate) fn set_mcp(&self, bridge: crate::acp_mcp_callback::Bridge) {
        *self.mcp.lock().unwrap() = Some(bridge);
    }
    pub(crate) fn set_turn(&self, policy: Value, turn_key: String) {
        let mut context = self.policy.lock().unwrap();
        context.latest = policy;
        context.turn_key = Some(turn_key);
    }
    pub(crate) fn settle(&self) {
        self.policy.lock().unwrap().turn_key = None;
    }
    pub(crate) fn permission(&self, request: &Value) -> Disposition {
        acp_client_policy::permission(&self.policy.lock().unwrap().latest, request)
    }
    pub(crate) fn record_approval(&self, request: &Value, decision: &str) {
        if !matches!(decision, "accept" | "acceptForSession")
            || request["toolCall"]["kind"] != "execute"
        {
            return;
        }
        let mut context = self.policy.lock().unwrap();
        let Some(turn_key) = context.turn_key.clone() else {
            return;
        };
        context.grants.record(
            "command",
            if decision == "acceptForSession" {
                "session"
            } else {
                "turn"
            },
            &turn_key,
        );
    }
    fn guard_execute(&self) -> Result<(), AcpError> {
        let context = self.policy.lock().unwrap();
        let disposition = acp_client_policy::execute(&context.latest);
        if disposition == Disposition::Allow
            || (disposition == Disposition::Ask
                && context.grants.allows_execute(context.turn_key.as_deref()))
        {
            return Ok(());
        }
        tracing::warn!(
            operation = "terminal/create",
            disposition = disposition.as_str(),
            "ACP client policy denied a client-mediated operation"
        );
        Err(RpcError{code:-32603,message:if disposition==Disposition::Ask{"The active T3 runtime policy requires approval for terminal/create. Request permission with session/request_permission before retrying."}else{"The active T3 runtime policy does not allow terminal/create."}.into(),data:None}.into())
    }
    pub(crate) fn register(&self, client: &Client) {
        let Some(terminals) = &self.terminals else {
            return;
        };
        let service = self.clone();
        let manager = terminals.clone();
        client.handle_create_terminal(Arc::new(move |request, _| {
            let service = service.clone();
            let manager = manager.clone();
            Box::pin(async move {
                service.guard_execute()?;
                let result = manager
                    .create(request.as_value(), &service.environment)
                    .await;
                logged("terminal/create", &result);
                t3_acp::v1::CreateTerminalResponse::decode(result?).map_err(Into::into)
            })
        }));
        let manager = terminals.clone();
        client.handle_terminal_output(Arc::new(move |request, _| {
            let manager = manager.clone();
            Box::pin(async move {
                let result = manager.output(request.as_value());
                logged("terminal/output", &result);
                t3_acp::v1::TerminalOutputResponse::decode(result?).map_err(Into::into)
            })
        }));
        let manager = terminals.clone();
        client.handle_terminal_wait_for_exit(Arc::new(move |request, _| {
            let manager = manager.clone();
            Box::pin(async move {
                let result = manager.wait(request.as_value()).await;
                logged("terminal/wait_for_exit", &result);
                t3_acp::v1::WaitForTerminalExitResponse::decode(result?).map_err(Into::into)
            })
        }));
        let manager = terminals.clone();
        client.handle_terminal_kill(Arc::new(move |request, _| {
            let manager = manager.clone();
            Box::pin(async move {
                let result = manager.kill(request.as_value()).await;
                logged("terminal/kill", &result);
                t3_acp::v1::KillTerminalResponse::decode(result?).map_err(Into::into)
            })
        }));
        let manager = terminals.clone();
        client.handle_terminal_release(Arc::new(move |request, _| {
            let manager = manager.clone();
            Box::pin(async move {
                let result = manager.release(request.as_value()).await;
                logged("terminal/release", &result);
                t3_acp::v1::ReleaseTerminalResponse::decode(result?).map_err(Into::into)
            })
        }));
    }
    pub(crate) fn resolve_update(&self, notification: Value) -> Value {
        let Some(terminals) = &self.terminals else {
            return notification;
        };
        let update = &notification["update"];
        if matches!(
            update["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) {
            let ids = update["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|entry| entry["type"] == "terminal")
                .filter_map(|entry| entry["terminalId"].as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>();
            if !ids.is_empty() {
                let key = (
                    notification["sessionId"].as_str().unwrap().to_owned(),
                    update["toolCallId"].as_str().unwrap().to_owned(),
                );
                let mut embedded = self.embedded.lock().unwrap();
                embedded.shift_remove(&key);
                embedded.insert(key, ids);
                while embedded.len() > 256 {
                    embedded.shift_remove_index(0);
                }
            }
        }
        terminals.resolve_content(&notification)
    }
    pub(crate) fn embedded_commands(&self, session: &str, tool: &str) -> Vec<String> {
        let Some(terminals) = &self.terminals else {
            return Vec::new();
        };
        self.embedded
            .lock()
            .unwrap()
            .get(&(session.into(), tool.into()))
            .into_iter()
            .flatten()
            .filter_map(|id| terminals.command_line(id))
            .collect()
    }
    pub(crate) async fn shutdown(&self) {
        let mcp = self.mcp.lock().unwrap().clone();
        if let Some(mcp) = mcp {
            mcp.dispose().await;
        }
        if let Some(terminals) = &self.terminals {
            terminals.shutdown().await;
        }
    }
}
fn logged(operation: &str, result: &Result<Value, AcpError>) {
    if let Err(error) = result {
        tracing::warn!(operation,detail=%error,"ACP client terminal operation failed");
    }
}
pub(crate) fn policy(runtime_mode: &Value, cwd: &Path) -> Value {
    json!({"runtimeMode":runtime_mode,"cwd":cwd})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_approval_is_scoped_to_source_turn_and_session_policy() {
        let cwd = std::env::current_dir().unwrap();
        let services = Services::new(
            "devin",
            &cwd,
            IndexMap::new(),
            policy(&json!("approval-required"), &cwd),
        );
        assert!(services.guard_execute().is_err());
        services.set_turn(policy(&json!("approval-required"), &cwd), "one".into());
        services.record_approval(&json!({"toolCall":{"kind":"read"}}), "accept");
        assert!(services.guard_execute().is_err());
        services.record_approval(&json!({"toolCall":{"kind":"execute"}}), "accept");
        assert!(services.guard_execute().is_ok());
        services.settle();
        assert!(services.guard_execute().is_err());
        services.set_turn(policy(&json!("approval-required"), &cwd), "two".into());
        assert!(services.guard_execute().is_err());
        services.record_approval(&json!({"toolCall":{"kind":"execute"}}), "acceptForSession");
        services.settle();
        assert!(services.guard_execute().is_ok());
        services.set_turn(
            json!({"runtimeMode":"full-access","cwd":cwd,"sandboxPolicy":{"type":"unknown"}}),
            "three".into(),
        );
        assert!(services.guard_execute().is_err());
    }
    #[test]
    fn only_explicit_resolved_devin_flavor_registers_client_terminals() {
        let cwd = std::env::current_dir().unwrap();
        assert!(
            Services::new(
                "devin",
                &cwd,
                IndexMap::new(),
                policy(&json!("full-access"), &cwd)
            )
            .has_terminals()
        );
        for agent in ["", "generic", "antigravity"] {
            assert!(
                !Services::new(
                    agent,
                    &cwd,
                    IndexMap::new(),
                    policy(&json!("full-access"), &cwd)
                )
                .has_terminals()
            );
        }
    }
}
