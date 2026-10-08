//! Trusted caller and target admission for cooperative MCP tools.
use crate::{
    mcp_invocation::{InvocationScope, McpCapability, McpFailure, refusal},
    persistence::{Store, read_projection},
    thread,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use t3_contracts::{AuthMcpClientAccess, ProviderInteractionMode, RuntimeMode, ThreadShell};
pub const READ_ONLY: &str =
    "This tool changes the environment, and this MCP client was approved for read-only access.";
pub const FULL_ACCESS: &str = "Changing projects or environment settings needs a live full-access/default calling thread or a full-access client.";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallerLimits {
    pub runtime_mode: RuntimeMode,
    pub interaction_mode: ProviderInteractionMode,
}
#[derive(Debug, Clone)]
pub struct Caller {
    pub shell: Option<ThreadShell>,
    pub limits: CallerLimits,
}
#[derive(Debug, Clone, Copy)]
pub enum Access {
    Reads,
    ReadsAsCaller,
    ActsAsCaller,
    Writes,
    WritesThreads,
    WritesEnvironment,
}
pub fn unavailable() -> McpFailure {
    refusal(
        "orchestration_error",
        "The operation could not be completed.",
    )
}
pub fn dispatch_failure(error: &Value) -> McpFailure {
    if matches!(
        error["_tag"].as_str(),
        Some("OrchestratorDispatchError" | "OrchestratorCommandRejectedError")
    ) {
        if let Some(cause) = error["cause"].as_str().filter(|cause| !cause.is_empty()) {
            return refusal(
                "orchestration_error",
                &cause.chars().take(1000).collect::<String>(),
            );
        }
    }
    unavailable()
}
pub fn runtime_rank(mode: RuntimeMode) -> u8 {
    match mode {
        RuntimeMode::ApprovalRequired => 0,
        RuntimeMode::AutoAcceptEdits => 1,
        RuntimeMode::Auto => 2,
        RuntimeMode::FullAccess => 3,
    }
}
pub fn resolve_runtime(
    parent: RuntimeMode,
    requested: Option<RuntimeMode>,
) -> Result<RuntimeMode, McpFailure> {
    let resolved = requested.unwrap_or(parent);
    if runtime_rank(resolved) > runtime_rank(parent) {
        Err(refusal(
            "runtime_mode_escalation_denied",
            &format!(
                "Child runtime mode {} is broader than parent mode {}.",
                json!(resolved).as_str().unwrap(),
                json!(parent).as_str().unwrap()
            ),
        ))
    } else {
        Ok(resolved)
    }
}
pub fn resolve_interaction(
    parent: ProviderInteractionMode,
    requested: Option<ProviderInteractionMode>,
) -> Result<ProviderInteractionMode, McpFailure> {
    let resolved = requested.unwrap_or(parent);
    if parent == ProviderInteractionMode::Plan && resolved == ProviderInteractionMode::Default {
        Err(refusal(
            "interaction_mode_escalation_denied",
            "Child interaction mode default is broader than parent mode plan.",
        ))
    } else {
        Ok(resolved)
    }
}
impl Caller {
    pub fn project_id(&self, requested: Option<&str>) -> Result<String, McpFailure> {
        requested
            .map(str::to_owned)
            .or_else(|| {
                self.shell
                    .as_ref()
                    .map(|shell| shell.project_id.to_string())
            })
            .ok_or_else(|| {
                refusal(
                    "target_required",
                    "Pass projectId: this MCP client is not running inside a T3 thread.",
                )
            })
    }
    pub fn thread_id(&self, requested: Option<&str>) -> Result<String, McpFailure> {
        requested
            .map(str::to_owned)
            .or_else(|| self.shell.as_ref().map(|shell| shell.id.to_string()))
            .ok_or_else(|| {
                refusal(
                    "target_required",
                    "Pass threadId: this MCP client is not running inside a T3 thread.",
                )
            })
    }
    pub fn assert_live(&self, scope: &InvocationScope) -> Result<(), McpFailure> {
        self.shell
            .as_ref()
            .map_or(Ok(()), |shell| scope.assert_live(shell))
    }
    pub fn target_within_limits(&self, target: &ThreadShell) -> Result<(), McpFailure> {
        resolve_runtime(self.limits.runtime_mode, Some(target.runtime_mode))?;
        resolve_interaction(self.limits.interaction_mode, Some(target.interaction_mode))?;
        Ok(())
    }
    pub fn assert_full_access(&self) -> Result<(), McpFailure> {
        if self.limits.runtime_mode == RuntimeMode::FullAccess
            && self.limits.interaction_mode == ProviderInteractionMode::Default
        {
            Ok(())
        } else {
            Err(refusal("capability_denied", FULL_ACCESS))
        }
    }
}
pub fn shell(connection: &Connection, id: &str) -> Result<Option<ThreadShell>, McpFailure> {
    read_projection(connection, "thread", id)
        .map_err(|_| unavailable())?
        .map(|projection| {
            serde_json::from_value(thread::shell(&projection)).map_err(|_| unavailable())
        })
        .transpose()
}
pub fn load_caller(
    connection: &Connection,
    scope: &InvocationScope,
    orchestration: bool,
) -> Result<Caller, McpFailure> {
    if orchestration && !scope.capabilities.contains(&McpCapability::Orchestration) {
        return Err(refusal(
            "capability_denied",
            "This credential cannot control threads.",
        ));
    }
    let caller = match &scope.thread {
        None => None,
        Some(thread) => Some(
            shell(connection, thread.thread_id.as_str())?
                .filter(|shell| shell.deleted_at.is_none())
                .ok_or_else(|| refusal("thread_not_found", "The calling thread was not found."))?,
        ),
    };
    let limits = caller
        .as_ref()
        .map(|shell| CallerLimits {
            runtime_mode: shell.runtime_mode,
            interaction_mode: shell.interaction_mode,
        })
        .unwrap_or(CallerLimits {
            runtime_mode: scope.client_runtime_ceiling(),
            interaction_mode: ProviderInteractionMode::Default,
        });
    Ok(Caller {
        shell: caller,
        limits,
    })
}
pub fn check(
    connection: &Connection,
    scope: &InvocationScope,
    access: Access,
    targets: &[&str],
) -> Result<Caller, McpFailure> {
    if matches!(access, Access::ReadsAsCaller | Access::ActsAsCaller) {
        scope.require_thread("This tool")?;
    }
    if matches!(access, Access::Reads | Access::ReadsAsCaller) {
        return Ok(Caller {
            shell: None,
            limits: CallerLimits {
                runtime_mode: scope.client_runtime_ceiling(),
                interaction_mode: ProviderInteractionMode::Default,
            },
        });
    }
    let writes = matches!(
        access,
        Access::ActsAsCaller | Access::Writes | Access::WritesThreads | Access::WritesEnvironment
    );
    if writes
        && scope
            .client
            .as_ref()
            .is_some_and(|client| client.access == AuthMcpClientAccess::ReadOnly)
    {
        return Err(refusal("capability_denied", READ_ONLY));
    }
    let caller = load_caller(
        connection,
        scope,
        matches!(access, Access::Writes | Access::WritesEnvironment),
    )?;
    if writes {
        caller.assert_live(scope)?;
    }
    if matches!(access, Access::WritesEnvironment) {
        caller.assert_full_access()?;
    }
    if matches!(access, Access::WritesThreads) {
        for id in targets {
            if scope
                .thread
                .as_ref()
                .is_some_and(|thread| thread.thread_id.as_str() == *id)
            {
                continue;
            }
            if let Some(target) =
                shell(connection, id)?.filter(|target| target.deleted_at.is_none())
            {
                caller.target_within_limits(&target)?;
            }
        }
    }
    Ok(caller)
}
/// Source DispatchModeLimit freezes the admitted caller's modes. Receipt replay
/// precedes this guard; the tool's initial admission still runs on every retry.
pub fn recheck_limits(limits: CallerLimits, target: &Value) -> Result<(), McpFailure> {
    let runtime: RuntimeMode =
        serde_json::from_value(target["runtimeMode"].clone()).map_err(|_| unavailable())?;
    let interaction: ProviderInteractionMode =
        serde_json::from_value(target["interactionMode"].clone()).map_err(|_| unavailable())?;
    let code = if runtime_rank(runtime) > runtime_rank(limits.runtime_mode) {
        Some("runtime_mode_escalation_denied")
    } else if limits.interaction_mode == ProviderInteractionMode::Plan
        && interaction == ProviderInteractionMode::Default
    {
        Some("interaction_mode_escalation_denied")
    } else {
        None
    };
    match code {
        None => Ok(()),
        Some(code) => Err(refusal(
            code,
            &format!(
                "Thread {} now runs in {}/{} mode, above this caller's. Its user changed it while this call ran.",
                target["id"].as_str().unwrap_or(""),
                json!(runtime).as_str().unwrap(),
                json!(interaction).as_str().unwrap()
            ),
        )),
    }
}
pub fn check_store(
    store: &Store,
    scope: &InvocationScope,
    access: Access,
    targets: &[&str],
) -> Result<Caller, McpFailure> {
    store
        .read(|connection| Ok(check(connection, scope, access, targets)))
        .map_err(|_| unavailable())?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_limits_reject_late_escalation_but_receipts_replay_before_guard() {
        let (store, mut scope) = crate::mcp_invocation::tests::fixture();
        scope.capabilities.insert(McpCapability::Orchestration);
        let now = "2026-01-01T00:00:00Z".parse().unwrap();
        let mut caller = store.projection("thread", "thread:1").unwrap().unwrap();
        caller["runs"] = json!([{"id":"run:1","status":"running"}]);
        let mut target = caller.clone();
        target["thread"]["id"] = json!("target");
        target["thread"]["lineage"]["rootThreadId"] = json!("target");
        store
            .transaction(|tx| {
                crate::persistence::write_projection(tx, "thread", "thread:1", &caller)?;
                crate::persistence::write_projection(tx, "thread", "target", &target)
            })
            .unwrap();
        let admitted = check_store(&store, &scope, Access::WritesThreads, &["target"])
            .unwrap()
            .limits;
        target["thread"]["runtimeMode"] = json!("full-access");
        store
            .transaction(|tx| crate::persistence::write_projection(tx, "thread", "target", &target))
            .unwrap();
        let command = json!({"type":"thread.metadata.update","commandId":"race","threadId":"target","title":"New title"});
        let refused = std::cell::RefCell::new(None);
        let result = thread::ThreadService::new(store.clone()).dispatch_guarded(
            &command,
            now,
            |_, current| {
                recheck_limits(admitted, &current.unwrap()["thread"]).map_err(|error| {
                    *refused.borrow_mut() = Some(error);
                    crate::persistence::StoreError::InvalidCommand("MCP admission refused".into())
                })
            },
        );
        assert!(result.is_err());
        assert_eq!(
            refused.borrow().as_ref().unwrap().0["code"],
            "runtime_mode_escalation_denied"
        );
        assert!(store.receipt("race").unwrap().is_none());
        assert_eq!(
            store.projection("thread", "target").unwrap().unwrap()["thread"]["title"],
            "Thread"
        );
        // Own-thread shortcut only applies to preflight. Its captured modes
        // still fence a command that waited while the user's modes changed.
        caller["thread"]["runtimeMode"] = json!("full-access");
        assert_eq!(
            recheck_limits(admitted, &caller["thread"]).unwrap_err().0["code"],
            "runtime_mode_escalation_denied"
        );
        target["thread"]["runtimeMode"] = json!("approval-required");
        store
            .transaction(|tx| crate::persistence::write_projection(tx, "thread", "target", &target))
            .unwrap();
        let calls = std::cell::Cell::new(0);
        let service = thread::ThreadService::new(store.clone());
        let receipt = service
            .dispatch_guarded(&command, now, |_, current| {
                calls.set(calls.get() + 1);
                recheck_limits(admitted, &current.unwrap()["thread"])
                    .map_err(|_| crate::persistence::StoreError::InvalidCommand("refused".into()))
            })
            .unwrap();
        target = store.projection("thread", "target").unwrap().unwrap();
        target["thread"]["runtimeMode"] = json!("full-access");
        store
            .transaction(|tx| crate::persistence::write_projection(tx, "thread", "target", &target))
            .unwrap();
        let replay = service
            .dispatch_guarded(&command, now, |_, _| {
                calls.set(calls.get() + 1);
                panic!("source receipt replay must precede late guard")
            })
            .unwrap();
        assert_eq!(receipt, replay);
        assert_eq!(calls.get(), 1);
        // The registered tool must freshly preflight before reaching replay.
        assert_eq!(
            check_store(&store, &scope, Access::WritesThreads, &["target"])
                .unwrap_err()
                .0["code"],
            "runtime_mode_escalation_denied"
        );
    }
    #[test]
    fn original_sealed_access_declarations_oracle() {
        let mut failures = Vec::new();
        let mut count = 0;
        for line in include_str!("../tests/fixtures/mcp-access.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let (store, _) = crate::mcp_invocation::tests::fixture();
            let scope: InvocationScope = serde_json::from_value(row["scope"].clone()).unwrap();
            let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
            projection["thread"]["runtimeMode"] = row["mode"].clone();
            projection["thread"]["interactionMode"] = row["interactionMode"].clone();
            if row["condition"] != "idle" {
                projection["runs"] = json!([{"id":"run:1","status":"running"}]);
            }
            if row["condition"] == "archived" {
                projection["thread"]["archivedAt"] = json!("2026-01-01T00:00:00.000Z");
            }
            if row["condition"] == "deleted" {
                projection["thread"]["deletedAt"] = json!("2026-01-01T00:00:00.000Z");
            }
            if row["condition"] == "switched" {
                projection["thread"]["providerInstanceId"] = json!("different");
            }
            let mut target = projection.clone();
            target["thread"]["id"] = json!("target");
            target["thread"]["runtimeMode"] = json!("full-access");
            target["thread"]["interactionMode"] = json!("default");
            target["thread"]["deletedAt"] = Value::Null;
            store
                .transaction(|tx| {
                    crate::persistence::write_projection(tx, "thread", "thread:1", &projection)?;
                    crate::persistence::write_projection(tx, "thread", "target", &target)?;
                    if row["condition"] == "missing" {
                        tx.execute(
                            "DELETE FROM rust_projections WHERE aggregate_kind='thread' AND aggregate_id='thread:1'",
                            [],
                        )?;
                    }
                    Ok(())
                })
                .unwrap();
            let access = match row["access"].as_str().unwrap() {
                "reads" => Access::Reads,
                "reads_as_caller" => Access::ReadsAsCaller,
                "acts_as_caller" => Access::ActsAsCaller,
                "writes" => Access::Writes,
                "writes_threads" => Access::WritesThreads,
                "writes_environment" => Access::WritesEnvironment,
                _ => unreachable!(),
            };
            let actual =
                match check_store(&store, &scope, access, &[row["targetId"].as_str().unwrap()]) {
                    Ok(_) => json!({"ran":true}),
                    Err(error) => error.0,
                };
            if actual != row["result"] {
                failures.push(format!(
                    "{count}: scope={} access={} target={} expected={} actual={actual}",
                    row["scope"], row["access"], row["targetId"], row["result"]
                ));
            }
            count += 1;
        }
        assert_eq!(count, 1584);
        assert!(
            failures.is_empty(),
            "{} mismatches\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    #[test]
    fn idle_reads_live_writes_and_target_ceiling_precedence() {
        let (store, mut scope) = crate::mcp_invocation::tests::fixture();
        scope.capabilities.insert(McpCapability::Orchestration);
        assert!(check_store(&store, &scope, Access::ReadsAsCaller, &[]).is_ok());
        assert_eq!(
            check_store(&store, &scope, Access::WritesThreads, &["thread:1"])
                .unwrap_err()
                .0["code"],
            "parent_not_active"
        );
        let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
        projection["runs"] = json!([{"id":"run:1","status":"running"}]);
        store
            .transaction(|tx| {
                crate::persistence::write_projection(tx, "thread", "thread:1", &projection)
            })
            .unwrap();
        assert!(check_store(&store, &scope, Access::WritesThreads, &["thread:1"]).is_ok());
        let mut target = projection.clone();
        target["thread"]["id"] = json!("target");
        target["thread"]["runtimeMode"] = json!("full-access");
        store
            .transaction(|tx| crate::persistence::write_projection(tx, "thread", "target", &target))
            .unwrap();
        assert_eq!(
            check_store(&store, &scope, Access::WritesThreads, &["target"])
                .unwrap_err()
                .0["message"],
            "Child runtime mode full-access is broader than parent mode approval-required."
        );
        store.read(|_connection|{assert_eq!(recheck_limits(CallerLimits { runtime_mode:RuntimeMode::ApprovalRequired,interaction_mode:ProviderInteractionMode::Default }, &target["thread"]).unwrap_err().0["message"],"Thread target now runs in full-access/default mode, above this caller's. Its user changed it while this call ran.");Ok(())}).unwrap();
        scope.client = Some(crate::mcp_invocation::ClientCaller {
            session_id: "client".into(),
            label: "Client".into(),
            access: AuthMcpClientAccess::ReadOnly,
        });
        scope.capabilities.clear();
        assert_eq!(
            check_store(&store, &scope, Access::Writes, &[])
                .unwrap_err()
                .0["message"],
            READ_ONLY
        );
        scope.thread = None;
        assert_eq!(
            check_store(&store, &scope, Access::ActsAsCaller, &[])
                .unwrap_err()
                .0["code"],
            "thread_credential_required"
        );
    }
}
