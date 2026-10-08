//! ACP effects and callbacks enter the same durable reducer as user commands.
use crate::{
    acp_runtime::{AcpSession, SessionEvent, permission_response},
    codex_runtime::{Work, commit, fail_run, find, projection, terminal},
    execution::at,
    persistence::{Store, StoreError},
    provider_registry::ProviderRegistry,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::HashMap;
use t3_acp::{
    AcpError,
    types::{Optional, PromptRequest, PromptResponse, RequestPermissionRequest},
};
use tokio::sync::{mpsc, oneshot, watch};

enum Activity {
    Event(Option<SessionEvent>),
    Prompt(Result<PromptResponse, AcpError>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution::ExecutionService, launch::ThreadLaunchService, project::ProjectService,
    };
    async fn setup(scenario: &str) -> (tempfile::TempDir, Store, ProviderRegistry) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":directory.path()}),Utc::now()).unwrap();
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-provider.py");
        let mut entry = json!({"driver":"acpRegistry","enabled":true,"config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[fixture,"2",scenario]}});
        if scenario == "hold-runtime-initialize" {
            entry["config"]["commandArgs"]
                .as_array_mut()
                .unwrap()
                .push(json!(directory.path().join("initialized")));
            entry["environment"] = json!([{"name":"FIXTURE_REQUEST_SIGNAL","value":directory.path().join("startup-signal")}]);
        }
        let settings = serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"local-agent":entry}})).unwrap();
        let providers = ProviderRegistry::discover(&settings, directory.path())
            .await
            .unwrap();
        assert_eq!(providers.driver("local-agent").unwrap(), "acpRegistry");
        (directory, store, providers)
    }
    fn launch(store: &Store, providers: &ProviderRegistry, initial: bool) -> String {
        let mut input = json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"local-agent","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}});
        if initial {
            input["initialMessage"] = json!({"text":"initial","attachments":[]});
        }
        ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(input, Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .into()
    }
    fn message(thread: &str, id: &str, text: &str) -> Value {
        json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":format!("message:{id}"),"text":text,"attachments":[],"dispatchMode":{"type":"start_immediately"}})
    }
    async fn milestone(
        store: &Store,
        events: &mut tokio::sync::broadcast::Receiver<Vec<crate::persistence::StoredEvent>>,
        thread: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let view = projection(store, thread).unwrap();
                if predicate(&view) {
                    return view;
                }
                events.recv().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "ACP persisted milestone was not reached: {}",
                projection(store, thread).unwrap()
            )
        })
    }
    fn assistant_text(view: &Value, run: &Value) -> String {
        view["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["runId"] == *run && message["role"] == "assistant")
            .map(|message| message["text"].as_str().unwrap())
            .collect()
    }
    async fn approve(
        execution: &ExecutionService,
        store: &Store,
        events: &mut tokio::sync::broadcast::Receiver<Vec<crate::persistence::StoredEvent>>,
        thread: &str,
        id: &str,
    ) -> Value {
        let view = milestone(store, events, thread, |view| {
            view["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending")
        })
        .await;
        let request = view["runtimeRequests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["status"] == "pending")
            .unwrap();
        assert_eq!(request["nativeRequestRef"]["nativeId"], "command");
        assert_eq!(request["nativeRequestRef"]["strength"], "weak");
        let command = json!({"type":"runtime-request.respond","commandId":id,"threadId":thread,"requestId":request["id"],"decision":"accept"});
        let receipt = execution.dispatch(&command, Utc::now()).unwrap();
        assert_eq!(execution.dispatch(&command, Utc::now()).unwrap(), receipt);
        let ordinal = view["runs"].as_array().unwrap().len() - 1;
        milestone(store, events, thread, |view| {
            view["runs"][ordinal]["status"] == "completed"
        })
        .await
    }
    #[tokio::test]
    async fn managed_registry_binary_installs_and_runs_real_persisted_prompt_approval() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":directory.path()}),Utc::now()).unwrap();
        let (catalog, config) =
            crate::acp_registry_support::fixture_catalog(directory.path(), "normal");
        let settings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"local-agent":{"driver":"acpRegistry","enabled":true,"config":config}}})).unwrap();
        let providers = ProviderRegistry::discover_with_catalog(
            &settings,
            directory.path(),
            Some(catalog.clone()),
        )
        .await
        .unwrap();
        let cold = providers
            .snapshots()
            .iter()
            .find(|row| row["instanceId"] == "local-agent")
            .cloned()
            .unwrap();
        assert_eq!(
            cold["installed"], false,
            "cold discovery does not implicitly prepare a binary"
        );
        assert!(!directory.path().join("tools").exists());
        let existing_registry_clone = providers.clone();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        assert!(
            providers.driver("local-agent").is_err(),
            "unprepared provider must remain unavailable for turn dispatch"
        );
        let prepared = catalog
            .prepare(&serde_json::from_value(json!({"agentId":"devin"})).unwrap())
            .await
            .unwrap();
        assert!(prepared.prepared);
        providers
            .reconfigure(&settings, directory.path())
            .await
            .unwrap();
        assert_eq!(providers.driver("local-agent").unwrap(), "acpRegistry");
        assert_eq!(
            existing_registry_clone.driver("local-agent").unwrap(),
            "acpRegistry",
            "existing runtime clones observe complete reconfiguration"
        );
        let thread = launch(&store, &providers, false);
        let mut events = store.subscribe();
        execution
            .dispatch(&message(&thread, "managed-prompt", "normal"), Utc::now())
            .unwrap();
        let completed = approve(&execution, &store, &mut events, &thread, "managed-approval").await;
        assert_eq!(
            assistant_text(&completed, &completed["runs"][0]["id"]),
            "Hello approved"
        );
        assert_eq!(completed["providerThreads"][0]["driver"], "acpRegistry");
        serde_json::from_value::<t3_contracts::ThreadProjection>(completed).unwrap();
        execution.shutdown().await;
        let refreshed =
            ProviderRegistry::discover_with_catalog(&settings, directory.path(), Some(catalog))
                .await
                .unwrap();
        let ready = refreshed
            .snapshots()
            .iter()
            .find(|row| row["instanceId"] == "local-agent")
            .cloned()
            .unwrap();
        assert_eq!(ready["status"], "ready");
        assert!(
            ready["models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["slug"] == "fixture-model")
        );
    }
    #[tokio::test]
    async fn persisted_acp_initial_prompt_permission_replay_resume_and_interrupt() {
        let (_directory, store, providers) = setup("normal").await;
        let mut events = store.subscribe();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let thread = launch(&store, &providers, true);
        let allocated = projection(&store, &thread).unwrap();
        assert_eq!(allocated["providerThreads"][0]["driver"], "acpRegistry");
        if allocated["runs"][0]["status"] == "preparing" {
            execution.dispatch(&json!({"type":"prepared-run.release","commandId":"release","threadId":thread,"runId":allocated["runs"][0]["id"]}),Utc::now()).unwrap();
        }
        let completed = approve(&execution, &store, &mut events, &thread, "answer-initial").await;
        assert_eq!(
            assistant_text(&completed, &completed["runs"][0]["id"]),
            "Hello approved"
        );
        assert!(
            completed["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["streaming"] == false)
        );
        assert_eq!(
            completed["providerThreads"][0]["nativeThreadRef"]["strength"],
            "strong"
        );
        serde_json::from_value::<t3_contracts::ThreadProjection>(completed).unwrap();
        execution.shutdown().await;
        let execution = ExecutionService::start(store.clone(), providers);
        let command = message(&thread, "resumed", "new turn");
        let receipt = execution.dispatch(&command, Utc::now()).unwrap();
        assert_eq!(execution.dispatch(&command, Utc::now()).unwrap(), receipt);
        let resumed = approve(&execution, &store, &mut events, &thread, "answer-resumed").await;
        assert_eq!(
            assistant_text(&resumed, &resumed["runs"][1]["id"]),
            "Hello approved"
        );
        assert!(
            !serde_json::to_string(&resumed["messages"])
                .unwrap()
                .contains("replayed")
        );
        execution
            .dispatch(&message(&thread, "held", "hold"), Utc::now())
            .unwrap();
        let held = milestone(&store, &mut events, &thread, |view| {
            view["runs"].as_array().unwrap().len() == 3
                && assistant_text(view, &view["runs"][2]["id"]) == "waiting"
        })
        .await;
        execution.dispatch(&json!({"type":"run.interrupt","commandId":"interrupt","threadId":thread,"runId":held["runs"][2]["id"]}),Utc::now()).unwrap();
        milestone(&store, &mut events, &thread, |view| {
            view["runs"][2]["status"] == "interrupted"
        })
        .await;
        execution.shutdown().await;
        assert!(store.acquire_runtime_lease().is_ok());
    }
    #[tokio::test]
    async fn final_prompt_reply_and_immediate_process_exit_preserve_completed_run() {
        let (_directory, store, providers) = setup("exit-after-prompt").await;
        let thread = launch(&store, &providers, false);
        let execution = ExecutionService::start(store.clone(), providers);
        let mut events = store.subscribe();
        execution
            .dispatch(&message(&thread, "prompt", "normal"), Utc::now())
            .unwrap();
        let completed = approve(&execution, &store, &mut events, &thread, "answer").await;
        assert_eq!(
            assistant_text(&completed, &completed["runs"][0]["id"]),
            "Hello approved"
        );
        let ended = milestone(&store, &mut events, &thread, |view| {
            view["providerSessions"][0]["status"] == "error"
        })
        .await;
        assert_eq!(ended["runs"][0]["status"], "completed");
        assert!(
            ended["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["streaming"] == false)
        );
        execution.shutdown().await;
    }
    #[tokio::test]
    async fn plan_updates_keep_identity_steps_and_removal_in_durable_projection() {
        let (_directory, store, providers) = setup("plans").await;
        let thread = launch(&store, &providers, false);
        let execution = ExecutionService::start(store.clone(), providers);
        let mut events = store.subscribe();
        execution
            .dispatch(&message(&thread, "prompt", "normal"), Utc::now())
            .unwrap();
        let pending = milestone(&store, &mut events, &thread, |view| {
            view["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending")
        })
        .await;
        assert_eq!(pending["plans"].as_array().unwrap().len(), 2);
        assert_eq!(pending["plans"][0]["steps"][0]["text"], "Finish");
        assert_eq!(pending["plans"][0]["steps"][0]["status"], "running");
        let id = pending["plans"][0]["id"].clone();
        let done = approve(&execution, &store, &mut events, &thread, "answer").await;
        assert_eq!(done["plans"].as_array().unwrap().len(), 2);
        assert_eq!(done["plans"][0]["id"], id);
        assert_eq!(done["plans"][0]["status"], "completed");
        assert_eq!(done["plans"][0]["steps"][0]["status"], "completed");
        assert_eq!(done["plans"][1]["status"], "superseded");
        assert!(done["turnItems"].as_array().unwrap().iter().any(|item| {
            item["nativeItemRef"]["nativeId"]
                .as_str()
                .is_some_and(|id| id.ends_with(":plan:todo%20plan"))
        }));
        serde_json::from_value::<t3_contracts::ThreadProjection>(done).unwrap();
        execution.shutdown().await;
    }
    #[tokio::test]
    async fn thought_chunks_ignore_nontext_and_preserve_text_whitespace() {
        let (_directory, store, providers) = setup("thoughts").await;
        let thread = launch(&store, &providers, false);
        let execution = ExecutionService::start(store.clone(), providers);
        let mut events = store.subscribe();
        execution
            .dispatch(&message(&thread, "prompt", "normal"), Utc::now())
            .unwrap();
        let done = approve(&execution, &store, &mut events, &thread, "answer").await;
        let thoughts = done["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "reasoning")
            .collect::<Vec<_>>();
        assert_eq!(thoughts.len(), 1);
        assert_eq!(thoughts[0]["text"], " think ");
        assert_eq!(thoughts[0]["streaming"], false);
        serde_json::from_value::<t3_contracts::ThreadProjection>(done).unwrap();
        execution.shutdown().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_during_initialization_reaps_child_before_releasing_database_lease() {
        let (directory, store, providers) = setup("hold-runtime-initialize").await;
        let signal =
            tokio::net::UnixDatagram::bind(directory.path().join("startup-signal")).unwrap();
        let thread = launch(&store, &providers, false);
        let execution = ExecutionService::start(store.clone(), providers);
        execution
            .dispatch(&message(&thread, "held-startup", "normal"), Utc::now())
            .unwrap();
        let mut bytes = [0; 128];
        let length =
            tokio::time::timeout(std::time::Duration::from_secs(5), signal.recv(&mut bytes))
                .await
                .unwrap()
                .unwrap();
        let pid = serde_json::from_slice::<Value>(&bytes[..length]).unwrap()["pid"]
            .as_i64()
            .unwrap() as i32;
        assert_eq!(
            projection(&store, &thread).unwrap()["runs"][0]["status"],
            "starting"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), execution.shutdown())
            .await
            .expect("Shutdown must interrupt initialization and await child reap");
        assert!(store.acquire_runtime_lease().is_ok());
        // This is the exact PID reported by our isolated child. Signal zero
        // checks its absence without sending a signal or discovering processes.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[tokio::test]
    async fn durable_tools_merge_partial_updates_bound_output_and_accept_scalar_read_inputs() {
        let (_directory, store, providers) = setup("tools").await;
        let thread = launch(&store, &providers, false);
        let execution = ExecutionService::start(store.clone(), providers);
        let mut events = store.subscribe();
        execution
            .dispatch(&message(&thread, "prompt", "normal"), Utc::now())
            .unwrap();
        let pending = milestone(&store, &mut events, &thread, |view| {
            view["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending")
        })
        .await;
        let command = pending["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "command_execution")
            .unwrap();
        assert_eq!(command["status"], "running");
        assert_eq!(command["input"], "pwd -P");
        assert_eq!(command["output"], "before");
        assert!(command.get("exitCode").is_none());
        assert_eq!(command["nativeItemRef"]["strength"], "strong");
        for id in ["read-scalar", "read-array"] {
            let item = pending["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["nativeItemRef"]["nativeId"] == id)
                .unwrap();
            assert_eq!(item["input"], json!({"path":"doc.txt"}));
            assert_eq!(item["title"], "Read doc.txt");
            assert_eq!(item["output"], "file body");
        }
        let tool = |id: &str| {
            pending["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["nativeItemRef"]["nativeId"] == id)
                .unwrap()
        };
        assert_eq!(tool("monitor")["type"], "command_execution");
        assert_eq!(tool("monitor")["input"], "monitor cmd");
        assert_eq!(tool("monitor")["output"], "monitor done");
        assert_eq!(tool("monitor")["exitCode"], 0);
        assert_eq!(tool("web-search")["type"], "web_search");
        assert_eq!(tool("web-search")["title"], "Web search: ACP");
        assert_eq!(tool("web-search")["patterns"], json!(["ACP"]));
        assert_eq!(
            tool("web-search")["results"],
            json!([{"url":"https://example.test"}])
        );
        assert_eq!(tool("structured-file")["type"], "file_change");
        assert_eq!(tool("structured-file")["fileName"], "src/file.rs");
        assert_eq!(
            tool("structured-file")["changes"],
            json!([{"path":"src/file.rs","operation":"move","oldPath":"src/old.rs"}])
        );
        assert_eq!(tool("structured-file")["diffStr"], "diff patch");
        let capabilities = &pending["providerSessions"][0]["capabilities"];
        assert_eq!(capabilities["streaming"]["streamsToolOutput"], true);
        assert_eq!(capabilities["tools"]["emitsToolStarted"], true);
        assert_eq!(capabilities["tools"]["emitsToolCompleted"], true);
        assert_eq!(capabilities["tools"]["emitsToolOutput"], true);
        assert_eq!(capabilities["tools"]["supportsMcpTools"], false);
        let item_id = command["id"].clone();
        assert!(
            item_id
                .as_str()
                .unwrap()
                .starts_with("turn-item:provider:acpRegistry:native-item:provider-instance%3A")
        );
        assert_eq!(
            pending["providerThreads"][0]["nativeMetadata"]["itemIdentityVersion"],
            2
        );
        let done = approve(&execution, &store, &mut events, &thread, "answer").await;
        let command = done["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == item_id)
            .unwrap();
        assert_eq!(command["status"], "completed");
        assert_eq!(command["input"], "pwd -P");
        assert_eq!(command["exitCode"], 0);
        assert_eq!(
            command["output"],
            format!("[Earlier output truncated]\n\n{}", "z".repeat(8000))
        );
        assert_eq!(
            done["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["id"] == command["nodeId"])
                .unwrap()["countsForRun"],
            true
        );
        serde_json::from_value::<t3_contracts::ThreadProjection>(done).unwrap();
        execution.shutdown().await;
    }
    #[tokio::test]
    async fn persisted_mcp_tools_recover_identity_brand_outputs_and_retain_titleless_updates() {
        let (_directory, store, providers) = setup("mcp-tools").await;
        let thread = launch(&store, &providers, false);
        let execution = ExecutionService::start(store.clone(), providers);
        let mut events = store.subscribe();
        execution
            .dispatch(&message(&thread, "prompt", "normal"), Utc::now())
            .unwrap();
        let pending = milestone(&store, &mut events, &thread, |view| {
            view["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending")
        })
        .await;
        let tool = |id: &str| {
            pending["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["nativeItemRef"]["nativeId"] == id)
                .unwrap()
        };
        let tagged = tool("mcp-tagged");
        assert_eq!(tagged["type"], "dynamic_tool");
        assert_eq!(tagged["toolName"], "t3-code.delegate_task");
        assert_eq!(tagged["title"], Value::Null);
        assert_eq!(tagged["input"], json!({"task":"fixture"}));
        assert_eq!(tagged["output"], json!({"pending":true}));
        assert!(tagged.get("toolSource").is_none());
        let title = tool("mcp-title");
        assert_eq!(title["toolName"], "t3-code.orchestrator_capabilities");
        assert_eq!(title["title"], Value::Null);
        let weather = tool("mcp-weather");
        assert_eq!(weather["toolName"], "weather.get_weather");
        assert_eq!(weather["title"], "get weather");
        assert_eq!(weather["input"], json!({"city":"Phoenix"}));
        assert_eq!(weather["output"], json!([{"type":"text","text":"sunny"}]));
        assert_eq!(
            weather["toolSource"],
            json!({"key":"mcp:weather","name":"Weather Service","kind":"integration","icon":{"_tag":"themed-logo","logoUrl":"https://example.test/weather.png"}})
        );
        assert_eq!(weather["toolIcon"], weather["toolSource"]["icon"]);
        let tagged_id = tagged["id"].clone();
        let title_id = title["id"].clone();
        let done = approve(&execution, &store, &mut events, &thread, "answer").await;
        let tagged = done["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == tagged_id)
            .unwrap();
        assert_eq!(tagged["status"], "completed");
        assert_eq!(tagged["toolName"], "t3-code.delegate_task");
        assert_eq!(tagged["input"], json!({"task":"fixture"}));
        assert_eq!(tagged["output"], json!({"completed":true}));
        let title = done["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == title_id)
            .unwrap();
        assert_eq!(title["status"], "completed");
        assert_eq!(title["toolName"], "t3-code.orchestrator_capabilities");
        assert_eq!(title["title"], Value::Null);
        assert_eq!(title["input"], json!({"retained":true}));
        assert_eq!(
            title["output"],
            json!({"error":"Denied","result":{"attempt":1}})
        );
        serde_json::from_value::<t3_contracts::ThreadProjection>(done).unwrap();
        execution.shutdown().await;
    }
}
struct Callback {
    request: RequestPermissionRequest,
    response: oneshot::Sender<Result<t3_acp::v2::RequestPermissionResponse, AcpError>>,
    written: oneshot::Receiver<Result<(), AcpError>>,
}
struct Actor {
    cwd: std::path::PathBuf,
    store: Store,
    thread_id: String,
    session_id: String,
    instance_id: String,
    item_identity_v2: bool,
    session: AcpSession,
    active_run: Option<String>,
    callbacks: HashMap<String, Callback>,
    prompt: Option<tokio::task::JoinHandle<Result<PromptResponse, AcpError>>>,
    text_segments: HashMap<&'static str, u64>,
    tools: HashMap<String, crate::acp_tools::ToolState>,
    tool_emission: HashMap<String, (Option<usize>, usize)>,
}
impl Drop for Actor {
    fn drop(&mut self) {
        if let Some(prompt) = self.prompt.take() {
            prompt.abort();
        }
    }
}
fn native_ref(id: &str, strong: bool) -> Value {
    json!({"driver":"acpRegistry","nativeId":id,"strength":if strong{"strong"}else{"weak"}})
}
fn error(error: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidCommand(error.to_string())
}
impl Actor {
    async fn next(&mut self) -> Activity {
        tokio::select! {
            biased;
            event=self.session.events.recv()=>Activity::Event(event),
            result=async{self.prompt.as_mut().unwrap().await},if self.prompt.is_some()=>Activity::Prompt(result.unwrap_or_else(|error|Err(AcpError::Transport(error.to_string())))),
        }
    }
    async fn connect(
        store: Store,
        providers: &ProviderRegistry,
        thread_id: &str,
        pending_peer: &mut Option<crate::acp_peer::ProcessPeer>,
        pending_services: &mut Option<crate::acp_client_callbacks::Services>,
    ) -> Result<Self, StoreError> {
        let view = projection(&store, thread_id)?;
        let instance_id = view["thread"]["modelSelection"]["instanceId"]
            .as_str()
            .unwrap()
            .to_owned();
        let instance = providers.acp(&instance_id).map_err(error)?;
        let project = store
            .projection("project", view["thread"]["projectId"].as_str().unwrap())?
            .ok_or_else(|| error("Project not found."))?;
        let cwd = view["thread"]["worktreePath"]
            .as_str()
            .or_else(|| project["workspaceRoot"].as_str())
            .ok_or_else(|| error("Workspace root is missing."))?;
        let cwd = std::fs::canonicalize(cwd).map_err(error)?;
        if !cwd.is_dir() {
            return Err(error("Workspace root is not a directory."));
        }
        let saved_thread = view["providerThreads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|thread| {
                thread["id"] == view["thread"]["activeProviderThreadId"]
                    && thread["providerInstanceId"] == instance_id
            });
        let saved = saved_thread.and_then(|thread| thread["nativeThreadRef"]["nativeId"].as_str());
        let item_identity_v2 = saved.is_none()
            || saved_thread
                .is_some_and(|thread| thread["nativeMetadata"]["itemIdentityVersion"] == 2);
        let peer = crate::acp_peer::ProcessPeer::spawn(
            instance.resolve_process(&cwd).await.map_err(error)?,
        )
        .map_err(error)?;
        // Retain the process outside this cancellable setup future. The actor
        // shutdown path waits for its reap before releasing the runtime lease.
        *pending_peer = Some(peer.clone());
        let services = instance.services_for(
            &cwd,
            crate::acp_client_callbacks::policy(&view["thread"]["runtimeMode"], &cwd),
        );
        // Keep callbacks outside the cancellable initialize/load future. Their
        // owned terminals must be reaped before runtime ownership is released.
        *pending_services = Some(services.clone());
        let session = instance
            .start_peer_with_services(peer, &cwd, saved, false, services)
            .await
            .map_err(error)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let mut capabilities: Value =
            serde_json::from_str(include_str!("acp-capabilities.json")).unwrap();
        let setup = serde_json::to_value(&session.setup).unwrap();
        // These source capabilities require adapter paths still being ported.
        // A negotiated load operation alone is not a conversation snapshot API.
        for (group, fields) in [
            ("sessions", &["supportsProviderSwitchingViaHandoff"][..]),
            (
                "threads",
                &["canReadThreadSnapshot", "canRollbackThread"][..],
            ),
            (
                "turns",
                &[
                    "supportsSteeringByInterruptRestart",
                    "supportsQueuedMessages",
                ][..],
            ),
            ("planning", &["supportsStructuredQuestions"][..]),
            (
                "context",
                &[
                    "acceptsSyntheticUserContext",
                    "canGenerateSummaries",
                    "canConsumeHandoffSummaries",
                    "supportsDeltaHandoff",
                    "supportsFullThreadHandoff",
                ][..],
            ),
            (
                "checkpointing",
                &[
                    "appCanCheckpointFilesystem",
                    "supportsNestedCheckpointScopes",
                    "providerCanRollbackConversation",
                    "providerRollbackReturnsSnapshot",
                    "providerCanReadConversationSnapshot",
                ][..],
            ),
        ] {
            for field in fields {
                capabilities[group][*field] = json!(false);
            }
        }
        capabilities["sessions"]["supportsModelSwitchInSession"] = json!(
            setup["configOptions"]
                .as_array()
                .is_some_and(|options| options.iter().any(|option| option["category"] == "model"))
        );
        // This bridge does not yet advertise the application MCP toolkit.
        capabilities["tools"]["supportsMcpTools"] = json!(false);
        commit(&store, thread_id, |_| {
            Ok(vec![(
                "provider-session.attached",
                json!({"id":session_id,"driver":"acpRegistry","providerInstanceId":instance_id,"status":"ready","cwd":cwd,"model":view["thread"]["modelSelection"]["model"],"capabilities":capabilities,"createdAt":now,"updatedAt":now,"lastError":null}),
            )])
        })?;
        Ok(Self {
            cwd,
            store,
            thread_id: thread_id.into(),
            session_id,
            instance_id,
            item_identity_v2,
            session,
            active_run: None,
            callbacks: HashMap::new(),
            prompt: None,
            text_segments: HashMap::new(),
            tools: HashMap::new(),
            tool_emission: HashMap::new(),
        })
    }
    async fn effect(&mut self, effect: &crate::persistence::Effect) -> Result<(), StoreError> {
        match effect.request["type"].as_str() {
            Some("provider-turn.start") => {
                self.start_turn(effect.request["runId"].as_str().unwrap())
                    .await
            }
            Some("provider-turn.interrupt") => {
                if self.active_run.as_deref() != effect.request["runId"].as_str() {
                    return Err(error("Target run is no longer active."));
                }
                self.session
                    .client
                    .cancel_typed(
                        serde_json::from_value(json!({"sessionId":self.session.setup.session_id}))
                            .unwrap(),
                    )
                    .await
                    .map_err(error)
            }
            Some("runtime-request.respond") => {
                if effect.request["providerSessionId"] != self.session_id {
                    return Err(error("Runtime callback belongs to another session."));
                }
                let callback = self
                    .callbacks
                    .remove(effect.request["requestId"].as_str().unwrap())
                    .ok_or_else(|| error("The live ACP callback no longer exists."))?;
                let response = permission_response(
                    &callback.request,
                    effect.request["decision"].as_str().unwrap_or("cancel"),
                );
                self.session.services.record_approval(
                    &serde_json::to_value(&callback.request).unwrap(),
                    effect.request["decision"].as_str().unwrap_or("cancel"),
                );
                callback
                    .response
                    .send(response)
                    .map_err(|_| error("The ACP callback was cancelled."))?;
                // Completing the persisted effect means the actual native stdin
                // write was acknowledged, rather than merely releasing a waiter.
                callback
                    .written
                    .await
                    .map_err(|_| error("ACP response acknowledgement was lost."))?
                    .map_err(error)
            }
            _ => Err(error("Unsupported ACP provider effect.")),
        }
    }
    async fn start_turn(&mut self, run_id: &str) -> Result<(), StoreError> {
        // Load/new notifications are historical/bootstrap events. The ordered
        // response barrier guarantees earlier replay is queued before setup
        // returns; consume it before assigning the next turn's active context.
        while let Ok(event) = self.session.events.try_recv() {
            match event {
                SessionEvent::Update(update) => {
                    let value = serde_json::to_value(update).unwrap();
                    if value["update"]["sessionUpdate"] == "config_option_update" {
                        self.session.setup.config_options =
                            serde_json::from_value(value["update"]["configOptions"].clone())
                                .map_err(error)?;
                    }
                }
                SessionEvent::Permission {
                    request,
                    response,
                    written,
                    ..
                } => {
                    let _ = response.send(permission_response(&request, "cancel"));
                    let _ = written.await;
                }
                SessionEvent::Terminated(cause) => return Err(error(cause)),
            }
        }
        let view = projection(&self.store, &self.thread_id)?;
        let run = find(&view, "runs", &json!(run_id))?;
        if run["status"] != "starting" || self.active_run.is_some() {
            return Err(error("Target run is no longer starting."));
        }
        if run["providerInstanceId"] != self.instance_id {
            return Err(error("ACP provider handoff is not yet available."));
        }
        let selection = &run["modelSelection"];
        self.session
            .set_model(selection["model"].as_str().unwrap())
            .await
            .map_err(error)?;
        let mut options = serde_json::Map::new();
        for option in selection["options"].as_array().into_iter().flatten() {
            options.insert(
                option["id"].as_str().unwrap().into(),
                option["value"].clone(),
            );
        }
        self.session.set_options(&options).await.map_err(error)?;
        if view["thread"]["interactionMode"] == "plan" {
            return Err(error("ACP plan-mode selection is not yet available."));
        }
        let text = find(&view, "messages", &run["userMessageId"])?["text"]
            .as_str()
            .unwrap()
            .to_owned();
        let provider_turn_id = uuid::Uuid::new_v4().to_string();
        self.session.services.set_turn(
            crate::acp_client_callbacks::policy(
                &view["thread"]["runtimeMode"],
                std::path::Path::new(
                    view["thread"]["worktreePath"]
                        .as_str()
                        .unwrap_or_else(|| self.cwd.to_str().unwrap()),
                ),
            ),
            provider_turn_id.clone(),
        );
        let native_turn_id = format!("{}:turn:{}", self.session.setup.session_id, run["ordinal"]);
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let mut run = find(view, "runs", &json!(run_id))?.clone();
            if run["status"] != "starting" {
                return Err(error("Run was interrupted before ACP prompt admission."));
            }
            let mut attempt = find(view, "attempts", &run["activeAttemptId"])?.clone();
            let mut node = find(view, "nodes", &run["rootNodeId"])?.clone();
            let mut thread = find(view, "providerThreads", &run["providerThreadId"])?.clone();
            run["status"] = json!("running");
            run["startedAt"] = json!(now);
            attempt["status"] = json!("running");
            attempt["startedAt"] = json!(now);
            attempt["providerTurnId"] = json!(provider_turn_id);
            node["status"] = json!("running");
            node["startedAt"] = json!(now);
            node["providerTurnId"] = json!(provider_turn_id);
            thread["driver"] = json!("acpRegistry");
            thread["providerSessionId"] = json!(self.session_id);
            thread["nativeThreadRef"] = native_ref(&self.session.setup.session_id, true);
            if self.item_identity_v2 {
                if !thread["nativeMetadata"].is_object() {
                    thread["nativeMetadata"] = json!({});
                }
                thread["nativeMetadata"]["itemIdentityVersion"] = json!(2);
            }
            thread["nativeConversationHeadRef"] = native_ref(&native_turn_id, false);
            thread["status"] = json!("active");
            thread["lastRunOrdinal"] = run["ordinal"].clone();
            thread["updatedAt"] = json!(now);
            let turn = json!({"id":provider_turn_id,"providerThreadId":run["providerThreadId"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":native_ref(&native_turn_id,false),"ordinal":run["ordinal"],"status":"running","startedAt":now,"completedAt":null});
            Ok(vec![
                ("provider-thread.updated", thread),
                ("provider-turn.updated", turn),
                ("run.updated", run),
                ("run-attempt.updated", attempt),
                ("node.updated", node),
            ])
        })?;
        self.active_run = Some(run_id.into());
        self.text_segments.clear();
        self.tools.clear();
        self.tool_emission.clear();
        let client = self.session.client.clone();
        let session_id = self.session.setup.session_id.clone();
        self.prompt = Some(tokio::spawn(async move {
            client
                .prompt_typed(PromptRequest {
                    session_id,
                    prompt: vec![
                        serde_json::from_value(json!({"type":"text","text":text})).unwrap(),
                    ],
                    meta: Optional::Missing,
                })
                .await
        }));
        Ok(())
    }
    fn context<'a>(&self, view: &'a Value) -> Result<(&'a Value, &'a Value), StoreError> {
        let run = find(view, "runs", &json!(self.active_run))?;
        Ok((run, find(view, "attempts", &run["activeAttemptId"])?))
    }
    fn base_item(&self, view: &Value, run: &Value, attempt: &Value, id: &str, node: &str) -> Value {
        let now = at(Utc::now());
        json!({"id":id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":null,"ordinal":view["turnItems"].as_array().unwrap().len(),"status":"running","title":null,"startedAt":now,"completedAt":null,"updatedAt":now})
    }
    fn update(
        &mut self,
        notification: t3_acp::types::SessionNotification,
    ) -> Result<(), StoreError> {
        if notification.session_id != self.session.setup.session_id {
            return Err(error("ACP update belongs to another session."));
        }
        if self.active_run.is_none() {
            return Ok(());
        } // Historical load replay is not a new turn.
        let value = serde_json::to_value(notification).unwrap();
        if value["_meta"]["isReplay"] == true {
            return Ok(());
        }
        let update = &value["update"];
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk" | "agent_thought_chunk") => {
                let reasoning = update["sessionUpdate"] == "agent_thought_chunk";
                let Some(text) = (if reasoning {
                    crate::acp_model::thought_delta(&update["content"]).map(ToOwned::to_owned)
                } else {
                    crate::acp_model::content_display_text(&update["content"])
                })
                .filter(|text| !text.is_empty()) else {
                    return Ok(());
                };
                let run_id = self.active_run.as_deref().unwrap();
                let stream = if reasoning { "reasoning" } else { "assistant" };
                let id = format!(
                    "{run_id}:{stream}:{}",
                    self.text_segments.get(stream).copied().unwrap_or(0)
                );
                let node_id = format!("node:{id}");
                let message_id = format!("message:{id}");
                let now = at(Utc::now());
                commit(&self.store, &self.thread_id, |view| {
                    let (run, attempt) = self.context(view)?;
                    let previous = view["turnItems"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|item| item["id"] == id);
                    let mut item = previous
                        .cloned()
                        .unwrap_or_else(|| self.base_item(view, run, attempt, &id, &node_id));
                    item["type"] = json!(if reasoning {
                        "reasoning"
                    } else {
                        "assistant_message"
                    });
                    item["text"] = json!(format!("{}{text}", item["text"].as_str().unwrap_or("")));
                    item["streaming"] = json!(true);
                    item["updatedAt"] = json!(now);
                    let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":if reasoning{"reasoning"}else{"assistant_message"},"status":"running","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":item["startedAt"],"completedAt":null});
                    let mut events = vec![("node.updated", node)];
                    if !reasoning {
                        item["messageId"] = json!(message_id);
                        events.push(("message.updated",json!({"id":message_id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node_id,"role":"assistant","createdBy":"agent","creationSource":"provider","text":item["text"],"attachments":[],"streaming":true,"createdAt":item["startedAt"],"updatedAt":now})));
                    }
                    events.push(("turn-item.updated", item));
                    Ok(events)
                })
            }
            Some("config_option_update") => {
                self.session.setup.config_options =
                    serde_json::from_value(json!(update["configOptions"])).map_err(error)?;
                Ok(())
            }
            Some("plan" | "plan_update" | "plan_removed") => {
                match crate::acp_model::plan_update(update) {
                    Some(payload) => self.plan(&payload),
                    None => Ok(()),
                }
            }
            Some("tool_call" | "tool_call_update") => self.tool(update),
            _ => Ok(()),
        }
    }
    fn tool(&mut self, update: &Value) -> Result<(), StoreError> {
        let Some(parsed) =
            crate::acp_tools::ToolState::parse(update, update["sessionUpdate"] == "tool_call")
        else {
            return Ok(());
        };
        let id = parsed.wire["toolCallId"].as_str().unwrap().to_owned();
        let previous = self.tools.get(&id);
        let next = crate::acp_tools::ToolState::merge(previous, parsed);
        let (last, skipped) = self.tool_emission.get(&id).copied().unwrap_or((None, 0));
        let (emit, skipped) = crate::acp_tools::ToolState::emission(previous, &next, last, skipped);
        self.tool_emission.insert(
            id.clone(),
            (
                if emit {
                    Some(next.progress_length())
                } else {
                    last
                },
                skipped,
            ),
        );
        self.tools.insert(id.clone(), next.clone());
        if !emit {
            return Ok(());
        }
        let native = &next.wire;
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let native_id = format!("{}:tool:{id}", self.session.setup.session_id);
            let (item_id, node_id) = crate::acp_tools::provider_item_ids(
                &self.instance_id,
                &native_id,
                self.item_identity_v2,
            );
            let existing = view["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == item_id);
            let mut item = existing
                .cloned()
                .unwrap_or_else(|| self.base_item(view, run, attempt, &item_id, &node_id));
            let status = match native["status"].as_str() {
                Some("pending") => "pending",
                Some("requiresAction") => "waiting",
                Some("completed") => "completed",
                Some("failed") => "failed",
                _ => "running",
            };
            let complete = matches!(status, "completed" | "failed");
            item["status"] = json!(status);
            item["title"] = native.get("title").cloned().unwrap_or(Value::Null);
            item["nativeItemRef"] = native_ref(&id, true);
            item["updatedAt"] = json!(now);
            item["completedAt"] = if complete { json!(now) } else { Value::Null };
            let data = &native["data"];
            let input = data
                .get("rawInput")
                .filter(|value| !value.is_null())
                .cloned()
                .unwrap_or(json!({}));
            let output = data
                .get("rawOutput")
                .filter(|value| !value.is_null())
                .or_else(|| data.get("content"));
            let changes = crate::acp_tools::structured_changes(data);
            let path = changes
                .first()
                .and_then(|change| change["path"].as_str())
                .or_else(|| {
                    data["locations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find_map(|location| {
                            location["path"]
                                .as_str()
                                .filter(|value| !t3_contracts::trim_wire_string(value).is_empty())
                        })
                        .or_else(|| {
                            ["path", "filePath", "file_path", "url", "query", "pattern"]
                                .iter()
                                .find_map(|key| {
                                    input[key].as_str().filter(|value| {
                                        !t3_contracts::trim_wire_string(value).is_empty()
                                    })
                                })
                        })
                })
                .map(t3_contracts::trim_wire_string);
            for key in [
                "output",
                "exitCode",
                "toolName",
                "input",
                "pattern",
                "patterns",
                "results",
                "fileName",
                "diffStr",
                "changes",
                "toolIcon",
                "toolSource",
            ] {
                item.as_object_mut().unwrap().remove(key);
            }
            let patch = data["content"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|entry| entry["type"] == "diff" && entry["patch"]["text"].is_string())
                .and_then(|entry| entry["patch"]["text"].as_str().map(ToOwned::to_owned))
                .or_else(|| output.and_then(crate::acp_tools::output_text));
            if let Some(identity) = crate::acp_mcp_tools::identity(
                native,
                &self
                    .session
                    .services
                    .embedded_commands(&self.session.setup.session_id, &id),
            ) {
                item["type"] = json!("dynamic_tool");
                item["title"] = Value::Null;
                let output_record = output.filter(|value| value.is_object());
                let metadata_output = output_record
                    .and_then(|value| value.get("result").filter(|value| value.is_object()))
                    .or(output_record);
                let presentation = crate::acp_mcp_tools::presentation(
                    &json!({"serverName":identity["server"],"toolName":identity["tool"],"source":metadata_output.map(|value|value["_meta"]["source"].clone())}),
                );
                for (key, value) in presentation.as_object().unwrap() {
                    item[key] = value.clone();
                }
                item["toolName"] = json!(format!(
                    "{}.{}",
                    identity["server"].as_str().unwrap(),
                    identity["tool"].as_str().unwrap()
                ));
                item["input"] = identity
                    .get("input")
                    .cloned()
                    .or_else(|| {
                        input
                            .get("arguments")
                            .filter(|value| value.is_object())
                            .cloned()
                    })
                    .or_else(|| input.as_object().map(|_| input.clone()))
                    .unwrap_or(json!({}));
                if let Some(output) = output {
                    item["output"] = crate::acp_mcp_tools::output(output);
                }
            } else if !changes.is_empty() {
                item["type"] = json!("file_change");
                item["fileName"] = changes[0]["path"].clone();
                item["changes"] = json!(changes);
                if let Some(patch) = patch {
                    item["diffStr"] = json!(patch);
                }
            } else {
                match native["kind"].as_str() {
                    Some("execute") => {
                        item["type"] = json!("command_execution");
                        item["input"] = native
                            .get("command")
                            .cloned()
                            .or_else(|| {
                                crate::acp_tools::monitor_command(
                                    &input,
                                    &output.cloned().unwrap_or(Value::Null),
                                )
                                .1
                                .map(|value| json!(value))
                            })
                            .or_else(|| native.get("title").cloned())
                            .unwrap_or(json!("Command"));
                        if let Some(output) = output {
                            if let Some(text) = crate::acp_tools::output_text(output) {
                                item["output"] = json!(text);
                            }
                            if let Some(code) =
                                crate::acp_tools::projected_exit_code(status, output)
                            {
                                item["exitCode"] = code;
                            }
                        }
                    }
                    Some("read") => {
                        item["type"] = json!("dynamic_tool");
                        item["toolName"] = json!("Read");
                        item["title"] =
                            json!(path.map(|path| format!("Read {path}")).unwrap_or_else(|| {
                                native["title"].as_str().unwrap_or("Read file").to_owned()
                            }));
                        item["input"] = crate::acp_tools::read_input(&input, path);
                        if let Some(output) = output {
                            item["output"] = output.clone();
                        }
                    }
                    Some("search") => {
                        if let Some(search) = crate::acp_tools::backend_search(
                            &input,
                            &output.cloned().unwrap_or(Value::Null),
                        ) {
                            item["type"] = json!("web_search");
                            item["title"] = json!(crate::acp_tools::backend_title(
                                &data["title"],
                                &native["title"],
                                search["query"].as_str()
                            ));
                            if let Some(query) = search["query"].as_str() {
                                item["patterns"] = json!([query]);
                            }
                            if !search["results"].as_array().unwrap().is_empty() {
                                item["results"] = search["results"].clone();
                            }
                        } else {
                            item["type"] = json!("file_search");
                            item["title"] = crate::acp_tools::search_label(
                                &json!({"rawInput":input,"input":input,"pattern":path}),
                            )
                            .map(|value| json!(value))
                            .unwrap_or_else(|| item["title"].clone());
                            if let Some(path) = path {
                                item["pattern"] = json!(path);
                                let mut result = json!({"fileName":path});
                                if let Some(text) = output.and_then(crate::acp_tools::output_text) {
                                    result["preview"] = json!(text);
                                }
                                item["results"] = json!([result]);
                            }
                        }
                    }
                    Some("fetch") => {
                        item["type"] = json!("web_search");
                        if let Some(path) = path {
                            item["patterns"] = json!([path]);
                            let mut result = json!({"url":path});
                            if let Some(snippet) = crate::acp_tools::output_text(&data["content"])
                                .or_else(|| output.and_then(crate::acp_tools::output_text))
                            {
                                result["snippet"] = json!(snippet);
                            }
                            item["results"] = json!([result]);
                        }
                    }
                    Some("edit" | "delete" | "move") => {
                        item["type"] = json!("file_change");
                        item["fileName"] = json!(path.unwrap_or_else(|| {
                            native["title"].as_str().unwrap_or("File change")
                        }));
                        if let Some(patch) = patch {
                            item["diffStr"] = json!(patch);
                        }
                    }
                    _ => {
                        let (monitor, command) = crate::acp_tools::monitor_command(
                            &input,
                            &output.cloned().unwrap_or(Value::Null),
                        );
                        if monitor {
                            item["type"] = json!("command_execution");
                            item["input"] = native
                                .get("command")
                                .cloned()
                                .or_else(|| command.map(|command| json!(command)))
                                .or_else(|| native.get("title").cloned())
                                .unwrap_or_else(|| {
                                    json!(if input["variant"]
                                        .as_str()
                                        .unwrap_or("")
                                        .trim()
                                        .eq_ignore_ascii_case("monitor")
                                    {
                                        "Monitor"
                                    } else {
                                        "Command"
                                    })
                                });
                            if let Some(output) = output {
                                if let Some(text) = crate::acp_tools::output_text(output) {
                                    item["output"] = json!(text);
                                }
                                if let Some(code) =
                                    crate::acp_tools::projected_exit_code(status, output)
                                {
                                    item["exitCode"] = code;
                                }
                            }
                        } else {
                            item["type"] = json!("dynamic_tool");
                            item["toolName"] = native
                                .get("title")
                                .or_else(|| native.get("kind"))
                                .cloned()
                                .unwrap_or(Value::Null);
                            item["input"] = input;
                            if let Some(output) = output {
                                item["output"] = output.clone();
                            }
                        }
                    }
                }
            }
            let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":"tool_call","status":if status=="pending"{"running"}else{status},"countsForRun":true,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":item["nativeItemRef"],"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":item["startedAt"],"completedAt":item["completedAt"]});
            Ok(vec![("node.updated", node), ("turn-item.updated", item)])
        })
    }
    fn plan(&mut self, update: &Value) -> Result<(), StoreError> {
        self.close_text_streams()?;
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let turn = find(view, "providerTurns", &attempt["providerTurnId"])?;
            let escaped = update["nativePlanId"]
                .as_str()
                .unwrap()
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                        (byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect::<String>();
            let native_id = format!(
                "{}:plan:{escaped}",
                turn["nativeTurnRef"]["nativeId"].as_str().unwrap()
            );
            let id = format!("{}:{native_id}", self.session_id);
            let node_id = format!("node:{id}");
            let existing = view["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == id);
            if update["kind"] == "removed" && existing.is_none() {
                return Ok(vec![]);
            }
            let mut item = existing
                .cloned()
                .unwrap_or_else(|| self.base_item(view, run, attempt, &id, &node_id));
            let plan_id = existing
                .and_then(|item| item["planId"].as_str())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let mut plan = if update["kind"] == "removed" {
                find(view, "plans", &json!(plan_id))?.clone()
            } else {
                json!({"id":plan_id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node_id})
            };
            if update["kind"] == "removed" {
                plan["status"] = json!("superseded");
            } else if update["kind"] == "items" {
                let steps=update["plan"].as_array().unwrap().iter().enumerate().map(|(index,step)|json!({"id":format!("acp-step-{}",index+1),"text":step["step"],"status":match step["status"].as_str(){Some("completed")=>"completed",Some("inProgress")=>"running",_=>"pending"}})).collect::<Vec<_>>();
                let completed =
                    !steps.is_empty() && steps.iter().all(|step| step["status"] == "completed");
                plan["status"] = json!(if completed { "completed" } else { "active" });
                plan["kind"] = json!("todo_list");
                plan["steps"] = json!(steps);
            } else {
                plan["status"] = json!("active");
                plan["kind"] = json!("proposed_plan");
                plan["markdown"] = json!(match update["kind"].as_str() {
                    Some("markdown") => update["markdown"].as_str().unwrap().to_owned(),
                    Some("file") => format!("Plan file: {}", update["uri"].as_str().unwrap()),
                    _ => format!(
                        "[Unsupported ACP plan content: {}]",
                        update["contentType"].as_str().unwrap()
                    ),
                });
            }
            let complete = matches!(plan["status"].as_str(), Some("completed" | "superseded"));
            item["type"] = plan["kind"].clone();
            item["planId"] = json!(plan_id);
            item["nativeItemRef"] = native_ref(&native_id, false);
            item["status"] = json!(if complete { "completed" } else { "running" });
            item["completedAt"] = if complete { json!(now) } else { Value::Null };
            item["updatedAt"] = json!(now);
            if plan["kind"] == "todo_list" {
                item.as_object_mut().unwrap().remove("markdown");
                item.as_object_mut().unwrap().remove("streaming");
                item["steps"] = plan["steps"].clone();
            } else {
                item.as_object_mut().unwrap().remove("steps");
                item["markdown"] = plan["markdown"].clone();
                item["streaming"] = json!(!complete);
            }
            let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":if plan["kind"]=="todo_list"{"todo_list"}else{"plan"},"status":if complete{"completed"}else{"running"},"countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":item["nativeItemRef"],"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":item["startedAt"],"completedAt":item["completedAt"]});
            Ok(vec![
                ("node.updated", node),
                ("plan.updated", plan),
                ("turn-item.updated", item),
            ])
        })
    }
    async fn permission(
        &mut self,
        request: RequestPermissionRequest,
        response: oneshot::Sender<Result<t3_acp::v2::RequestPermissionResponse, AcpError>>,
        written: oneshot::Receiver<Result<(), AcpError>>,
    ) -> Result<(), StoreError> {
        if request.session_id != self.session.setup.session_id || self.active_run.is_none() {
            let _ = response.send(permission_response(&request, "cancel"));
            return Ok(());
        }
        let native = serde_json::to_value(&request).unwrap();
        let kind = native["toolCall"]["kind"].as_str().unwrap_or("other");
        let disposition = self.session.services.permission(&native);
        if disposition == crate::acp_client_policy::Disposition::Deny {
            let _ = response.send(permission_response(&request, "cancel"));
            return written
                .await
                .map_err(|_| error("ACP acknowledgement lost."))?
                .map_err(error);
        }
        let auto = disposition == crate::acp_client_policy::Disposition::Allow;
        if auto {
            let mut result = permission_response(&request, "accept").map_err(error)?;
            if result.as_value()["outcome"]["outcome"] == "cancelled" {
                result = permission_response(&request, "acceptForSession").map_err(error)?;
            }
            response
                .send(Ok(result))
                .map_err(|_| error("ACP callback ended."))?;
            return written
                .await
                .map_err(|_| error("ACP acknowledgement lost."))?
                .map_err(error);
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        self.close_text_streams()?;
        let node_id = format!("node:{request_id}");
        let item_id = format!("item:{request_id}");
        let now = at(Utc::now());
        let request_kind = match kind {
            "read" | "search" | "fetch" => "file-read",
            "edit" | "delete" | "move" => "file-change",
            _ => "command",
        };
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let native_ref = native_ref(&request.tool_call.tool_call_id, false);
            let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":"approval_request","status":"waiting","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":native_ref,"runtimeRequestId":request_id,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
            let row = json!({"id":request_id,"nodeId":node_id,"providerTurnId":attempt["providerTurnId"],"nativeRequestRef":native_ref,"kind":request_kind,"status":"pending","responseCapability":{"type":"live","providerSessionId":self.session_id},"createdAt":now,"resolvedAt":null});
            let mut item = self.base_item(view, run, attempt, &item_id, &node_id);
            item["nativeItemRef"] = native_ref;
            item["status"] = json!("waiting");
            item["type"] = json!("approval_request");
            item["requestId"] = json!(request_id);
            item["requestKind"] = json!(request_kind);
            item["prompt"] = json!(
                native["toolCall"]["title"]
                    .as_str()
                    .unwrap_or("Approve ACP operation?")
            );
            Ok(vec![
                ("node.updated", node),
                ("runtime-request.updated", row),
                ("turn-item.updated", item),
            ])
        })?;
        self.callbacks.insert(
            request_id,
            Callback {
                request,
                response,
                written,
            },
        );
        Ok(())
    }
    fn finish(&mut self, result: Result<PromptResponse, AcpError>) -> Result<(), StoreError> {
        if result.is_ok() {
            self.close_text_streams()?;
        }
        match result {
            Ok(response) => terminal(
                &self.store,
                &self.thread_id,
                self.active_run.as_deref(),
                if response.stop_reason == "cancelled" {
                    "interrupted"
                } else {
                    "completed"
                },
                None,
            )?,
            Err(cause) => fail_run(
                &self.store,
                &self.thread_id,
                self.active_run.as_deref(),
                &cause.to_string(),
            )?,
        };
        self.active_run = None;
        self.session.services.settle();
        self.callbacks.clear();
        Ok(())
    }
    fn close_text_streams(&mut self) -> Result<(), StoreError> {
        if self.active_run.is_none() {
            return Ok(());
        }
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let mut events = vec![];
            for item in
                view["turnItems"].as_array().unwrap().iter().filter(|item| {
                    item["runId"] == json!(self.active_run) && item["streaming"] == true
                })
            {
                let mut item = item.clone();
                item["streaming"] = json!(false);
                item["status"] = json!("completed");
                item["completedAt"] = json!(now);
                item["updatedAt"] = json!(now);
                let mut node = find(view, "nodes", &item["nodeId"])?.clone();
                node["status"] = json!("completed");
                node["completedAt"] = json!(now);
                events.push(("node.updated", node));
                if item["type"] == "assistant_message" {
                    let mut message = find(view, "messages", &item["messageId"])?.clone();
                    message["streaming"] = json!(false);
                    message["updatedAt"] = json!(now);
                    events.push(("message.updated", message));
                }
                events.push(("turn-item.updated", item));
            }
            Ok(events)
        })?;
        for stream in ["assistant", "reasoning"] {
            *self.text_segments.entry(stream).or_default() += 1;
        }
        Ok(())
    }
}
pub(crate) async fn actor(
    store: Store,
    providers: ProviderRegistry,
    thread_id: String,
    mut work: mpsc::Receiver<Work>,
    mut stopped: watch::Receiver<bool>,
    mut canceled: watch::Receiver<Option<String>>,
) {
    let mut runtime: Option<Actor> = None;
    let mut pending_peer = None;
    let mut pending_services = None;
    let mut interrupted = false;
    loop {
        if *stopped.borrow() {
            break;
        }
        tokio::select! {
            _=stopped.changed()=>break,
            incoming=work.recv()=>{
                let Some(incoming)=incoming else{break};let starting=incoming.effect.request["type"]=="provider-turn.start";let run_id=incoming.effect.request["runId"].as_str();
                let result=tokio::select! {
                    result=async{if runtime.is_none(){runtime=Some(Actor::connect(store.clone(),&providers,&thread_id,&mut pending_peer,&mut pending_services).await?);pending_services.take();pending_peer.take();}runtime.as_mut().unwrap().effect(&incoming.effect).await}=>result,
                    _=stopped.changed()=>break,
                    _=async{loop{if canceled.borrow_and_update().as_deref()==run_id{break;}if canceled.changed().await.is_err(){std::future::pending::<()>().await;}}},if starting=>{interrupted=true;Err(error("Run interrupted during ACP startup."))}
                };
                let close=starting&&result.is_err();if interrupted||close{work.close();}let _=incoming.complete.send(result.map_err(|error|error.to_string()));if interrupted||close{break;}
            }
            activity=async{runtime.as_mut().unwrap().next().await},if runtime.is_some()=>{
                let ended=matches!(&activity,Activity::Event(Some(SessionEvent::Terminated(_)))|Activity::Event(None));
                let actor=runtime.as_mut().unwrap();let result=match activity {
                    Activity::Event(Some(SessionEvent::Update(update)))=>actor.update(update),
                    Activity::Event(Some(SessionEvent::Permission{request,response,written,..}))=>tokio::select!{result=actor.permission(request,response,written)=>result,_=stopped.changed()=>break},
                    Activity::Event(Some(SessionEvent::Terminated(cause)))=>{if let Some(prompt)=actor.prompt.take(){actor.finish(prompt.await.unwrap_or_else(|error|Err(AcpError::Transport(error.to_string()))))}else{Err(error(cause))}},Activity::Event(None)=>Err(error("ACP event stream ended.")),
                    Activity::Prompt(result)=>{actor.prompt.take();actor.finish(result)}
                };
                if let Err(cause)=result {work.close();let _=fail_run(&store,&thread_id,actor.active_run.as_deref(),&cause.to_string());break;}
                if ended{work.close();break;}
            }
        }
    }
    work.close();
    if let Some(services) = pending_services {
        services.shutdown().await;
    }
    if let Some(peer) = pending_peer {
        peer.shutdown().await;
    }
    if let Some(mut actor) = runtime {
        let status = if *stopped.borrow() || interrupted {
            "interrupted"
        } else {
            "failed"
        };
        let _ = terminal(
            &store,
            &thread_id,
            actor.active_run.as_deref(),
            status,
            None,
        );
        let _ = commit(&store, &thread_id, |view| {
            let mut session = find(view, "providerSessions", &json!(actor.session_id))?.clone();
            session["status"] = json!(if status == "interrupted" {
                "stopped"
            } else {
                "error"
            });
            session["updatedAt"] = json!(at(Utc::now()));
            Ok(vec![("provider-session.updated", session)])
        });
        if let Some(prompt) = actor.prompt.take() {
            prompt.abort();
            let _ = prompt.await;
        }
        actor.callbacks.clear();
        actor.session.shutdown().await;
    }
}
