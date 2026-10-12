//! Dispatch for the tools ported after the first group, and the answer for the tools this server has
//! no service for. Every tool of the original is listed to the model, as the original lists them; the
//! ones in `UNAVAILABLE` say so when called, in the shape of the failure the original gives when the
//! capability is missing from the environment.
use crate::{
    mcp_access::{self, Access},
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpCapability, McpFailure, refusal},
};
use serde_json::{Value, json};

type Out = Result<Value, McpFailure>;

/// Tools with a handler behind a service of this server, past the first group (`mcp_control::HANDLED`).
pub(crate) const IMPLEMENTED: &[&str] = &[
    // Threads: search, configuration, queue, pending questions, organization, transfers.
    "t3_thread_search",
    "t3_thread_transfers",
    "t3_thread_configuration",
    "t3_thread_configure",
    "t3_thread_organize",
    "t3_thread_fork",
    "t3_thread_merge_back",
    "t3_queue_list",
    "t3_queue_read",
    "t3_queue_edit",
    "t3_queue_cancel",
    "t3_queue_reorder",
    "t3_queue_promote_to_steer",
    "t3_pending_request_list",
    "t3_pending_request_read",
    "t3_pending_request_respond",
    // Attachments.
    "t3_attachment_prepare_upload",
    "t3_attachment_discard",
    "t3_thread_send_attachments",
    // Environment and projects.
    "t3_environment_read",
    "t3_environment_preferences_update",
    "t3_project_create",
    "t3_project_update",
    "t3_project_delete",
    "t3_project_clone",
    // Pull requests.
    "link_pull_request",
    "unlink_pull_request",
    "list_thread_pull_requests",
    "watch_pull_request",
    "unwatch_pull_request",
    // Worktrees.
    "t3_worktree_list",
    "t3_worktree_status",
    "t3_worktree_handoff",
    // This thread's preview tabs, and closing one.
    "t3_preview_list",
    "t3_preview_close",
    // The browser tools, run by whichever browser host is registered with the broker (`mcp_preview.rs`).
    "preview_dialog",
    "preview_status",
    "preview_open",
    "preview_navigate",
    "preview_resize",
    "preview_set_appearance",
    "preview_snapshot",
    "preview_click",
    "preview_type",
    "preview_hover",
    "preview_select",
    "preview_drag",
    "preview_upload",
    "preview_press",
    "preview_scroll",
    "preview_evaluate",
    "preview_wait_for",
    "preview_recording_start",
    "preview_recording_stop",
];

/// Whether an unavailable tool only reads (`readsAsCaller`) or acts as the calling thread (`actsAsCaller`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Gate {
    Reads,
    Acts,
}

/// What a tool with no service says. Each row is the tool and its gate in the original. The HTML tools fail as the original's HTML renderer
/// does when it has no browser.
pub(crate) const UNAVAILABLE: &[(&str, Gate)] = &[("html_preview", Gate::Reads), ("html_render", Gate::Acts)];

pub(crate) fn unavailable_tool(name: &str) -> Option<&'static (&'static str, Gate)> {
    UNAVAILABLE.iter().find(|(tool, ..)| *tool == name)
}

/// The text the model reads for a failed call. The original declares most tools' failure as a
/// returned value, which the transport writes as the failure's JSON. The preview tools, the pull
/// request tools and the HTML preview use Effect's default failure mode instead, and the transport
/// writes the failure's own `message`; the preview snapshot prefixes it.
pub(crate) fn failure_text(name: &str, error: &McpFailure) -> String {
    match name {
        "html_preview" | "link_pull_request" | "unlink_pull_request" | "list_thread_pull_requests" | "watch_pull_request" | "unwatch_pull_request" => error.to_string(),
        _ if name.starts_with("preview_") => error.to_string(),
        _ => error.0.to_string(),
    }
}

impl McpControlTools {
    /// The handler of a tool past the first group, or the answer of one with no service.
    pub(crate) async fn extended(&self, scope: &InvocationScope, name: &str, args: Value) -> Out {
        match name {
            "t3_thread_search" => Box::pin(self.thread_search(scope, args)).await,
            "t3_thread_transfers" => self.thread_transfers(scope, args),
            "t3_thread_configuration" => self.thread_configuration(scope, args),
            "t3_thread_configure" => self.thread_configure(scope, args),
            "t3_thread_organize" => self.thread_organize(scope, args),
            "t3_thread_fork" => self.thread_fork(scope, args),
            "t3_thread_merge_back" => self.thread_merge_back(scope, args),
            "t3_queue_list" => self.queue_list(scope, args),
            "t3_queue_read" => self.queue_read(scope, args),
            "t3_queue_edit" | "t3_queue_cancel" | "t3_queue_reorder" | "t3_queue_promote_to_steer" => self.queue_command(scope, name, args),
            "t3_pending_request_list" => self.pending_request_list(scope, args),
            "t3_pending_request_read" => self.pending_request_read(scope, args),
            "t3_pending_request_respond" => self.pending_request_respond(scope, args),
            "t3_attachment_prepare_upload" => Box::pin(self.attachment_prepare_upload(scope, args)).await,
            "t3_attachment_discard" => Box::pin(self.attachment_discard(scope, args)).await,
            "t3_thread_send_attachments" => Box::pin(self.send_attachments(scope, args)).await,
            "t3_environment_read" => Box::pin(self.environment_read(scope)).await,
            "t3_environment_preferences_update" => Box::pin(self.environment_preferences_update(scope, args)).await,
            "t3_project_create" => Box::pin(self.project_create(scope, args)).await,
            "t3_project_update" => Box::pin(self.project_update(scope, args)).await,
            "t3_project_delete" => Box::pin(self.project_delete(scope, args)).await,
            "t3_project_clone" => Box::pin(self.project_clone(scope, args)).await,
            "link_pull_request" | "unlink_pull_request" | "watch_pull_request" | "unwatch_pull_request" => Box::pin(self.pull_request_write(scope, name, args)).await,
            "list_thread_pull_requests" => Box::pin(self.pull_request_list(scope, args)).await,
            "t3_worktree_list" => Box::pin(self.worktree_list(scope, args)).await,
            "t3_worktree_status" => Box::pin(self.worktree_status(scope)).await,
            "t3_worktree_handoff" => Box::pin(self.worktree_handoff(scope, args)).await,
            "t3_preview_list" => self.preview_list(scope, args),
            "t3_preview_close" => Box::pin(self.preview_close(scope, args)).await,
            other if crate::mcp_preview::PREVIEW_TOOLS.contains(&other) => Box::pin(self.preview_tool(scope, other, args)).await,
            other => match unavailable_tool(other) {
                Some((_, gate)) => self.answer_unavailable(scope, other, *gate),
                None => Err(mcp_access::unavailable()),
            },
        }
    }

    /// A tool whose service this server does not have. It runs the original's gate first, so the refusals a caller would meet anyway still
    /// come first, then says what the original says.
    fn answer_unavailable(&self, scope: &InvocationScope, name: &str, gate: Gate) -> Out {
        mcp_access::check_store(&self.store, scope, if gate == Gate::Reads { Access::ReadsAsCaller } else { Access::ActsAsCaller }, &[])?;
        let what = if name == "html_preview" { "render an HTML preview" } else { "publish an HTML render" };
        Err(refusal("orchestration_error", &format!("This T3 server cannot {what}: it has no headless browser. Do not retry. To check a page, use a headless browser from the shell, such as Playwright, or curl.")))
    }
}

/// Identifies this server process, so a client can tell a restart from a change of state.
pub(crate) fn process_epoch() -> String {
    static EPOCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    EPOCH.get_or_init(|| uuid::Uuid::new_v4().to_string()).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_invocation::ClientCaller;
    use std::sync::Arc;
    use t3_contracts::AuthMcpClientAccess;

    /// Every tool of the original that the orchestrator serves is accounted for exactly once, and the
    /// ones answered as unavailable are exactly these, for exactly these reasons.
    #[test]
    fn every_listed_tool_is_handled_or_answered_as_unavailable() {
        let listed: Vec<String> = McpControlTools::catalog().iter().map(|tool| tool["name"].as_str().unwrap().to_owned()).collect();
        assert_eq!(listed.len(), 76);
        for name in &listed {
            let handled = McpControlTools::handled(name);
            let unavailable = unavailable_tool(name).is_some();
            assert!(handled != unavailable, "{name} must be handled or answered as unavailable, exactly one");
        }
        assert_eq!(crate::mcp_control::HANDLED.len() + IMPLEMENTED.len(), 74);
        assert_eq!(IMPLEMENTED.len(), 54);
        // Exactly the HTML tools are answered as unavailable on this server: they need a headless browser it does not have yet.
        let withheld: Vec<&str> = UNAVAILABLE.iter().map(|(name, ..)| *name).collect();
        assert_eq!(withheld, ["html_preview", "html_render"]);
        // Every browser tool has a handler, and the handler module names the same ones.
        let preview: Vec<&str> = IMPLEMENTED.iter().copied().filter(|name| name.starts_with("preview_")).collect();
        assert_eq!(preview.len(), 19);
        assert_eq!(preview, crate::mcp_preview::PREVIEW_TOOLS);
        // Reads and acts follow the original's declaration for the tool that only reads.
        let reads: Vec<&str> = UNAVAILABLE.iter().filter(|(_, gate)| *gate == Gate::Reads).map(|(name, ..)| *name).collect();
        assert_eq!(reads, ["html_preview"]);
    }

    fn tools() -> (McpControlTools, InvocationScope) {
        let (store, mut scope) = crate::mcp_invocation::tests::fixture();
        scope.capabilities.clear();
        (McpControlTools { store, execution: None, clock: Arc::new(chrono::Utc::now), services: Default::default() }, scope)
    }
    async fn text(tools: &McpControlTools, scope: &InvocationScope, name: &str, args: Value) -> (bool, String) {
        let result = tools.call(scope, name, args).await.unwrap();
        (result["isError"] == true, result["content"][0]["text"].as_str().unwrap().to_owned())
    }

    /// A credential without a tool's capability gets the original's refusal for that capability.
    #[tokio::test]
    async fn a_credential_without_the_capability_gets_the_originals_refusal() {
        let (tools, scope) = tools();
        // The writing tools are gated on a live caller first, as the original orders them.
        let (_, not_live) = text(&tools, &scope, "link_pull_request", json!({"url":"https://github.com/acme/web/pull/1"})).await;
        assert_eq!(not_live, "The calling provider no longer owns an active thread run.");
        // Once the caller owns a live run, the capability is what is missing.
        let mut projection = tools.store.projection("thread", "thread:1").unwrap().unwrap();
        projection["runs"] = json!([{"id":"run:1","status":"running"}]);
        tools.store.transaction(|tx| crate::persistence::write_projection(tx, "thread", "thread:1", &projection)).unwrap();
        assert_eq!(text(&tools, &scope, "link_pull_request", json!({"url":"https://github.com/acme/web/pull/1"})).await, (true, "MCP credential does not grant the pull-requests capability.".into()));
        assert_eq!(text(&tools, &scope, "list_thread_pull_requests", json!({})).await, (true, "MCP credential does not grant the pull-requests capability.".into()));
        let (_, worktree) = text(&tools, &scope, "t3_worktree_status", json!({})).await;
        assert_eq!(serde_json::from_str::<Value>(&worktree).unwrap(), json!({"_tag":"WorktreeMcpFailure","code":"capability_denied","message":"This MCP credential does not grant worktree capabilities."}));
        assert_eq!(text(&tools, &scope, "t3_worktree_list", json!({})).await, (true, json!({"_tag":"OrchestratorMcpFailure","code":"capability_denied","message":"This credential cannot inspect worktrees."}).to_string()));
        assert_eq!(text(&tools, &scope, "t3_thread_configuration", json!({})).await.1, json!({"_tag":"OrchestratorMcpFailure","code":"capability_denied","message":"This credential cannot control threads."}).to_string());
        let (_, preview) = text(&tools, &scope, "t3_preview_list", json!({})).await;
        assert_eq!(serde_json::from_str::<Value>(&preview).unwrap()["_tag"], "PreviewAutomationUnavailableError");
        let (failed, click) = text(&tools, &scope, "preview_click", json!({"selector":"b"})).await;
        assert!(failed && click.contains("Do not retry them."), "{click}");
    }

    /// A client approved for read-only access changes nothing, whichever tool it asks; and a client
    /// outside any thread has no thread to act as.
    #[tokio::test]
    async fn read_only_and_threadless_callers_are_refused_before_anything_changes() {
        let (tools, mut scope) = tools();
        scope.capabilities.extend([McpCapability::Orchestration, McpCapability::Worktree, McpCapability::PullRequests, McpCapability::Preview]);
        scope.thread = None;
        scope.client = Some(ClientCaller { session_id: "client".into(), label: "Client".into(), access: AuthMcpClientAccess::ReadOnly });
        let read_only = json!({"_tag":"OrchestratorMcpFailure","code":"capability_denied","message":crate::mcp_access::READ_ONLY}).to_string();
        let writes = [
            ("t3_environment_preferences_update", json!({"enableProviderUpdateChecks":false})),
            ("t3_project_create", json!({"title":"x","workspaceRoot":"/tmp/x"})),
            ("t3_project_update", json!({"projectId":"p","title":"x"})),
            ("t3_project_delete", json!({"projectId":"p","force":true})),
            ("t3_project_clone", json!({"remoteUrl":"/tmp/x","destinationPath":"/tmp/y"})),
            ("t3_thread_organize", json!({"threadId":"thread:1","action":"pin"})),
            ("t3_thread_configure", json!({"threadId":"thread:1","modelSelection":{"instanceId":"codex","model":"m"}})),
            ("t3_thread_fork", json!({"threadId":"thread:1","sourcePoint":{"type":"latest_stable"}})),
            ("t3_queue_edit", json!({"threadId":"thread:1","queuedRunId":"r","text":"x"})),
            ("t3_queue_cancel", json!({"threadId":"thread:1","queuedRunId":"r"})),
            ("t3_queue_promote_to_steer", json!({"threadId":"thread:1","queuedRunId":"r","targetRunId":"t"})),
            ("t3_pending_request_respond", json!({"threadId":"thread:1","requestId":"q","answers":{"a":"b"}})),
            ("t3_attachment_discard", json!({"attachmentId":"pending-00000000-0000-4000-8000-000000000000-txt"})),
            ("t3_thread_send_attachments", json!({"threadId":"thread:1","attachments":[{"type":"file","id":"pending-00000000-0000-4000-8000-000000000000-txt","name":"a.txt","mimeType":"text/plain","sizeBytes":1}]})),
            ("link_pull_request", json!({"threadId":"thread:1","url":"https://github.com/acme/web/pull/1"})),
            ("unwatch_pull_request", json!({"threadId":"thread:1","url":"https://github.com/acme/web/pull/1"})),
        ];
        for (name, args) in writes {
            let (failed, message) = text(&tools, &scope, name, args).await;
            // The pull request tools write the failure's message; the others its JSON.
            let expected = if name.ends_with("pull_request") { crate::mcp_access::READ_ONLY.to_owned() } else { read_only.clone() };
            assert!(failed && message == expected, "{name}: {message}");
        }
        // Tools that act as the calling thread need one.
        for name in ["t3_worktree_handoff", "t3_worktree_status", "t3_preview_close", "preview_click", "html_render"] {
            let args = match name { "t3_worktree_handoff" => json!({"branch":"b"}), "t3_preview_close" => json!({"tabId":"t"}), "preview_click" => json!({"selector":"b"}), "html_render" => json!({"html":"x","title":"x","height":100}), _ => json!({}) };
            let (failed, message) = text(&tools, &scope, name, args).await;
            assert!(failed && (message.contains("thread_credential_required") || message.contains("acts as the calling T3 thread, so it needs an agent running inside T3 Code")), "{name}: {message}");
        }
    }

    /// `thread.pull-request.watch` on the thread service: the watch it records, and what refuses it.
    #[test]
    fn the_watch_command_records_one_watch_per_link_and_refuses_the_threads_that_cannot_watch() {
        let (store, _) = crate::mcp_invocation::tests::fixture();
        let now: chrono::DateTime<chrono::Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
        let service = crate::thread::ThreadService::new(store.clone());
        let watch = |id: &str, watching: bool, link: bool| {
            let mut command = json!({"type":"thread.pull-request.watch","commandId":id,"threadId":"thread:1","host":"github.com","repository":"acme/web","number":7,"watching":watching});
            if link {
                command["link"] = json!({"url":"https://github.com/acme/web/pull/7","source":"agent"});
            }
            service.dispatch(&command, now).unwrap()
        };
        // Stopping a watch that was never started changes nothing.
        let before = store.projection("thread", "thread:1").unwrap().unwrap()["thread"].clone();
        assert_eq!(watch("stop-nothing", false, false).status, "accepted");
        assert_eq!(store.projection("thread", "thread:1").unwrap().unwrap()["thread"], before);
        // Watching an unlinked pull request links it first, and watching twice keeps the first watch.
        assert_eq!(watch("start", true, true).status, "accepted");
        let first = store.projection("thread", "thread:1").unwrap().unwrap()["thread"]["pullRequests"][0].clone();
        assert_eq!((first["source"].as_str(), first["watch"]["wakes"].as_u64(), first["watch"]["passed"].as_bool()), (Some("agent"), Some(0), Some(false)));
        assert_eq!(watch("start-again", true, true).status, "accepted");
        assert_eq!(store.projection("thread", "thread:1").unwrap().unwrap()["thread"]["pullRequests"][0], first);
        assert_eq!(watch("stop", false, false).status, "accepted");
        assert!(store.projection("thread", "thread:1").unwrap().unwrap()["thread"]["pullRequests"][0].get("watch").is_none());
        // A settled thread takes no new watch; a thread Stop reached takes no watch of the agent's.
        let mut projection = store.projection("thread", "thread:1").unwrap().unwrap();
        projection["thread"]["settledOverride"] = json!("settled");
        store.transaction(|tx| crate::persistence::write_projection(tx, "thread", "thread:1", &projection)).unwrap();
        let settled = watch("settled", true, false);
        assert_eq!(settled.status, "rejected");
        assert!(settled.error.unwrap()["cause"].as_str().unwrap().contains("is settled or archived and cannot watch pull requests."));
        projection["thread"]["settledOverride"] = Value::Null;
        projection["runs"] = json!([{"id":"run:1","ordinal":1,"status":"interrupted","startedAt":"2026-01-01T00:00:00.000Z","completedAt":"2026-01-01T00:00:01.000Z"}]);
        projection["turnItems"] = json!([{"id":"stop","type":"run_interrupt_request","runId":"run:1"}]);
        store.transaction(|tx| crate::persistence::write_projection(tx, "thread", "thread:1", &projection)).unwrap();
        let stopped = watch("stopped", true, true);
        assert_eq!(stopped.status, "rejected");
        assert_eq!(stopped.error.unwrap()["cause"], "Thread thread:1 was stopped.");
        assert_eq!(watch("user-watch", true, false).status, "accepted", "a watch of the user's own is not the agent's call");
    }
}
