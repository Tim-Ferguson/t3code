//! Trusted MCP invocation context and the source tool access gates. Tool input
//! never supplies this context; the session or external-client authenticator does.
use crate::{persistence::Store, thread};
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use t3_contracts::{
    AuthMcpClientAccess, EnvironmentId, ProviderInstanceId, RuntimeMode, ThreadId, ThreadShell,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpCapability {
    Preview,
    Orchestration,
    Worktree,
    Device,
    PullRequests,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCaller {
    pub thread_id: ThreadId,
    pub provider_session_id: String,
    pub provider_instance_id: ProviderInstanceId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCaller {
    pub session_id: String,
    pub label: String,
    pub access: AuthMcpClientAccess,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvocationScope {
    pub environment_id: EnvironmentId,
    pub capabilities: IndexSet<McpCapability>,
    pub issued_at: i64,
    pub request_namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadCaller>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientCaller>,
}

/// These are application failures; storage errors are deliberately reduced to
/// the original generic public message rather than exposing their causes.
#[derive(Debug, Clone, PartialEq)]
pub struct McpFailure(pub Value);
impl std::fmt::Display for McpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(message) = self.0["message"].as_str() {
            return f.write_str(message);
        }
        if self.0["_tag"] == "PreviewAutomationUnavailableError" {
            return f.write_str("MCP credential does not grant the preview capability: browser preview tools are off for this thread. Do not retry them. To check a page, use a headless browser from the shell, such as Playwright, or curl. The user can turn on \"Agent browser access\" in Settings; it applies when the agent session next starts.");
        }
        write!(
            f,
            "MCP credential does not grant the {} capability.",
            self.0["capability"].as_str().unwrap_or("unknown")
        )
    }
}
impl std::error::Error for McpFailure {}
pub fn refusal(code: &str, message: &str) -> McpFailure {
    McpFailure(json!({"_tag":"OrchestratorMcpFailure","code":code,"message":message}))
}
fn unavailable() -> McpFailure {
    refusal(
        "orchestration_error",
        "The operation could not be completed.",
    )
}
impl InvocationScope {
    pub fn client_runtime_ceiling(&self) -> RuntimeMode {
        match self.client.as_ref().map(|client| client.access) {
            None | Some(AuthMcpClientAccess::ReadOnly | AuthMcpClientAccess::ApprovalRequired) => {
                RuntimeMode::ApprovalRequired
            }
            Some(AuthMcpClientAccess::AutoAcceptEdits) => RuntimeMode::AutoAcceptEdits,
            Some(AuthMcpClientAccess::Auto) => RuntimeMode::Auto,
            Some(AuthMcpClientAccess::FullAccess) => RuntimeMode::FullAccess,
        }
    }
    pub fn require_thread(&self, operation: &str) -> Result<&ThreadCaller, McpFailure> {
        self.thread.as_ref().ok_or_else(||refusal("thread_credential_required",&format!("{operation} acts as the calling T3 thread, so it needs an agent running inside T3 Code. This MCP client signed in from outside a thread.")))
    }
    pub fn require_capability(
        &self,
        capability: McpCapability,
        thread_owned: bool,
    ) -> Result<(), McpFailure> {
        if self.capabilities.contains(&capability) && (!thread_owned || self.thread.is_some()) {
            return Ok(());
        }
        let mut fields = json!({"_tag":if capability==McpCapability::Preview {"PreviewAutomationUnavailableError"}else{"McpCapabilityUnavailableError"},"capability":capability,"environmentId":self.environment_id});
        if let Some(thread) = &self.thread {
            fields["threadId"] = json!(thread.thread_id);
            fields["providerSessionId"] = json!(thread.provider_session_id);
            fields["providerInstanceId"] = json!(thread.provider_instance_id);
        }
        Err(McpFailure(fields))
    }
    /// `readsAsCaller`: no live-run check, but an outside client has no devices
    /// or preview tabs belonging to a T3 thread.
    pub fn reads_as_caller(&self) -> Result<&ThreadCaller, McpFailure> {
        self.require_thread("This tool")
    }
    /// `actsAsCaller`: reload the shell on every invocation. A token's lifetime
    /// does not imply the provider still owns the active run.
    pub fn acts_as_caller(&self, store: &Store) -> Result<ThreadShell, McpFailure> {
        let caller = self.require_thread("This tool")?;
        if self
            .client
            .as_ref()
            .is_some_and(|client| client.access == AuthMcpClientAccess::ReadOnly)
        {
            return Err(refusal(
                "capability_denied",
                "This tool changes the environment, and this MCP client was approved for read-only access.",
            ));
        }
        let projection = store
            .projection("thread", caller.thread_id.as_str())
            .map_err(|_| unavailable())?;
        let Some(projection) = projection else {
            return Err(refusal(
                "thread_not_found",
                "The calling thread was not found.",
            ));
        };
        let shell: ThreadShell =
            serde_json::from_value(thread::shell(&projection)).map_err(|_| unavailable())?;
        self.assert_live(&shell)?;
        Ok(shell)
    }
    pub fn assert_live(&self, shell: &ThreadShell) -> Result<(), McpFailure> {
        if shell.deleted_at.is_some() {
            return Err(refusal(
                "thread_not_found",
                "The calling thread was not found.",
            ));
        }
        if shell.archived_at.is_some()
            || shell.active_run_id.is_none()
            || self
                .thread
                .as_ref()
                .is_none_or(|caller| caller.provider_instance_id != shell.provider_instance_id)
        {
            return Err(refusal(
                "parent_not_active",
                "The calling provider no longer owns an active thread run.",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::persistence::write_projection;
    fn scope() -> InvocationScope {
        serde_json::from_value(json!({"environmentId":"environment-1","requestNamespace":"provider-session-1","issuedAt":1,"capabilities":["device"],"thread":{"threadId":"thread:1","providerSessionId":"provider-session-1","providerInstanceId":"codex"}})).unwrap()
    }
    pub(crate) fn fixture() -> (Store, InvocationScope) {
        let store = Store::memory().unwrap();
        let now = "2026-01-01T00:00:00Z".parse().unwrap();
        let project=crate::project::ProjectCommand::from_json(json!({"type":"project.create","commandId":"project-command","projectId":"project:1","title":"Project","workspaceRoot":"/tmp/mcp-project"})).unwrap();
        crate::project::ProjectService::new(store.clone())
            .dispatch(&project, now)
            .unwrap();
        let receipt=thread::ThreadService::new(store.clone()).dispatch(&json!({"type":"thread.create","commandId":"create","threadId":"thread:1","projectId":"project:1","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}),now).unwrap();
        assert_eq!(receipt.status, "accepted");
        (store, scope())
    }
    fn update(store: &Store, apply: impl FnOnce(&mut Value)) {
        let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
        apply(&mut projection);
        store
            .transaction(|tx| write_projection(tx, "thread", "thread:1", &projection))
            .unwrap();
    }
    fn failure(result: Result<ThreadShell, McpFailure>, code: &str) {
        assert_eq!(result.unwrap_err().0["code"], code);
    }
    #[test]
    fn source_context_capabilities_client_ceilings_and_live_caller_oracle() {
        let (store, default) = fixture();
        let baseline: ThreadShell = serde_json::from_value(thread::shell(
            &store.projection("thread", "thread:1").unwrap().unwrap(),
        ))
        .unwrap();
        let mut count = 0;
        for line in include_str!("../tests/fixtures/mcp-invocation.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let actual = match row["type"].as_str().unwrap() {
                "capability" | "threadCapability" => {
                    let scope: InvocationScope =
                        serde_json::from_value(row["scope"].clone()).unwrap();
                    let cap = serde_json::from_value(row["capability"].clone()).unwrap();
                    match scope.require_capability(cap, row["type"] == "threadCapability") {
                        Ok(()) => json!({"ok":true}),
                        Err(error) => json!({"error":error.0}),
                    }
                }
                "thread" => {
                    let scope: InvocationScope =
                        serde_json::from_value(row["scope"].clone()).unwrap();
                    match scope.require_thread("This tool") {
                        Ok(_) => json!({"ok":true}),
                        Err(error) => json!({"error":error.0}),
                    }
                }
                "ceiling" => {
                    let mut scope = default.clone();
                    scope.client = row
                        .get("client")
                        .map(|client| serde_json::from_value(client.clone()).unwrap());
                    json!(scope.client_runtime_ceiling())
                }
                "live" => {
                    let mut shell = baseline.clone();
                    shell.archived_at =
                        serde_json::from_value(row["caller"]["archivedAt"].clone()).unwrap();
                    shell.active_run_id =
                        serde_json::from_value(row["caller"]["activeRunId"].clone()).unwrap();
                    shell.provider_instance_id =
                        serde_json::from_value(row["caller"]["providerInstanceId"].clone())
                            .unwrap();
                    match default.assert_live(&shell) {
                        Ok(()) => json!({"ok":true}),
                        Err(error) => json!({"error":error.0}),
                    }
                }
                other => panic!("Unknown fixture {other}"),
            };
            assert_eq!(actual, row["result"], "source witness {count}: {row}");
            count += 1;
        }
        assert_eq!(count, 526);
    }
    #[test]
    fn reads_do_not_require_live_run_and_every_write_reloads_the_persisted_shell() {
        let (store, scope) = fixture();
        assert!(scope.reads_as_caller().is_ok());
        failure(scope.acts_as_caller(&store), "parent_not_active");
        update(&store, |projection| {
            projection["runs"] = json!([{"id":"run-1","status":"running"}])
        });
        assert!(scope.acts_as_caller(&store).is_ok());
        update(&store, |projection| {
            projection["thread"]["archivedAt"] = json!("2026-01-01T00:00:00Z")
        });
        failure(scope.acts_as_caller(&store), "parent_not_active");
        assert!(scope.reads_as_caller().is_ok());
        update(&store, |projection| {
            projection["thread"]["archivedAt"] = Value::Null;
            projection["thread"]["providerInstanceId"] = json!("different");
        });
        failure(scope.acts_as_caller(&store), "parent_not_active");
        update(&store, |projection| {
            projection["thread"]["deletedAt"] = json!("2026-01-01T00:00:00Z")
        });
        failure(scope.acts_as_caller(&store), "thread_not_found");
        let mut missing = scope.clone();
        missing.thread.as_mut().unwrap().thread_id = "missing".parse().unwrap();
        failure(missing.acts_as_caller(&store), "thread_not_found");
    }
    #[test]
    fn thread_scope_precedes_read_only_and_capability_errors() {
        let (store, mut scope) = fixture();
        scope.client = Some(ClientCaller {
            session_id: "client-1".into(),
            label: "Outside".into(),
            access: AuthMcpClientAccess::ReadOnly,
        });
        scope.thread = None;
        assert_eq!(
            scope.reads_as_caller().unwrap_err().0["code"],
            "thread_credential_required"
        );
        failure(scope.acts_as_caller(&store), "thread_credential_required");
        scope.thread = super::tests::scope().thread;
        failure(scope.acts_as_caller(&store), "capability_denied");
        scope.client.as_mut().unwrap().access = AuthMcpClientAccess::FullAccess;
        failure(scope.acts_as_caller(&store), "parent_not_active");
    }
}
