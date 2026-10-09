//! ACP effects and callbacks enter the same durable reducer as user commands.
use crate::{
    acp_runtime::{AcpSession, SessionEvent, permission_response},
    codex_runtime::{Work, commit, fail_run_captured, fail_run_with, find, projection, terminal_captured},
    execution::at,
    persistence::{Store, StoreError},
    provider_registry::ProviderRegistry,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use t3_acp::{
    AcpError,
    types::{Optional, PromptRequest, PromptResponse, RequestPermissionRequest},
};
use tokio::sync::{mpsc, oneshot, watch};

mod updates;
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
        let settings = serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},"local-agent":entry}})).unwrap();
        let providers = ProviderRegistry::discover(&settings, directory.path())
            .await
            .unwrap();
        assert_eq!(providers.driver("local-agent").unwrap(), "acpRegistry");
        // Isolation guard: a driver that is probed by default (codex, claude, ...) must be
        // disabled here, or discovery would run the developer's real CLI. A new driver fails this.
        let leaked: Vec<_> = providers.snapshots().iter().filter(|row| row["instanceId"] != "local-agent" && row["status"] != "disabled").map(|row| row["instanceId"].clone()).collect();
        assert!(leaked.is_empty(), "test settings leave real providers enabled: {leaked:?}");
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
    async fn credential_stop_captures_admitted_actor_before_its_first_poll_and_prevents_spawn() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (root, store, providers) = setup("normal").await;
            let log = root.path().join("requests");
            let mut settings = serde_json::to_value(providers.settings()).unwrap();
            settings["providerInstances"]["local-agent"]["environment"] =
                json!([{"name":"FIXTURE_REQUEST_LOG","value":log}]);
            providers
                .reconfigure(&serde_json::from_value(settings).unwrap(), root.path())
                .await
                .unwrap();
            let execution = ExecutionService::start(store.clone(), providers.clone());
            let auth = crate::provider_auth_service::ProviderAuthService::new(
                providers.clone(),
                root.path().into(),
                root.path().join("caches"),
                execution.authentication_stop(),
            );
            let thread = launch(&store, &providers, false);
            let mut admitted = execution.pause_next_actor();
            let baseline = std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .filter(|method| *method == "session/new")
                .count();
            execution
                .dispatch(&message(&thread, "pending-auth-stop", "hold"), Utc::now())
                .unwrap();
            let (lifetime, release) = admitted.recv().await.unwrap();
            assert_eq!(lifetime.captured_instance().as_deref(), Some("local-agent"));
            let stopping = auth.clone();
            let logout = tokio::spawn(async move { stopping.logout("local-agent").await });
            lifetime.wait_stopped().await;
            assert!(
                !logout.is_finished(),
                "credential mutation waits actor's owning scope completion"
            );
            release.send(()).unwrap();
            assert!(
                logout.await.unwrap().is_err(),
                "fixture has no logout support"
            );
            let requests = std::fs::read_to_string(&log).unwrap();
            assert_eq!(
                requests
                    .lines()
                    .filter(|method| *method == "session/new")
                    .count(),
                baseline,
                "admitted stopped actor must not create a session"
            );
            assert!(!requests.lines().any(|method| method == "session/prompt"));
            auth.shutdown().await;
            execution.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn registry_auth_logout_reaps_captured_active_actor_after_controller_rebuild() {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let (root, store, providers) = setup("normal").await;
            let signal_path = root.path().join("requests.sock");
            let signal = tokio::net::UnixDatagram::bind(&signal_path).unwrap();
            let mut settings = serde_json::to_value(providers.settings()).unwrap();
            settings["providerInstances"]["local-agent"]["environment"] =
                json!([{"name":"FIXTURE_ALL_REQUEST_SIGNAL","value":signal_path}]);
            let settings: t3_contracts::ServerSettings = serde_json::from_value(settings).unwrap();
            providers.reconfigure(&settings, root.path()).await.unwrap();
            let execution = ExecutionService::start(store.clone(), providers.clone());
            let auth = crate::provider_auth_service::ProviderAuthService::new(
                providers.clone(),
                root.path().into(),
                root.path().join("caches"),
                execution.authentication_stop(),
            );
            let thread = launch(&store, &providers, false);
            let mut events = store.subscribe();
            execution
                .dispatch(&message(&thread, "held-run", "hold"), Utc::now())
                .unwrap();
            milestone(&store, &mut events, &thread, |view| {
                assistant_text(view, &view["runs"][0]["id"]) == "waiting"
            })
            .await;
            let mut buffer = [0; 512];
            let pid = loop {
                let size = signal.recv(&mut buffer).await.unwrap();
                let value: Value = serde_json::from_slice(&buffer[..size]).unwrap();
                if value["method"] == "session/prompt" {
                    break value["pid"].as_i64().unwrap() as i32;
                }
            };
            // Source rebuild closes only controller setup scopes. Existing
            // admitted actors remain owned by their parent and retain the old
            // process instance until an explicit credential operation stops it.
            let mut renamed = serde_json::to_value(&settings).unwrap();
            renamed["providerInstances"]["local-agent"]["displayName"] = json!("Renamed");
            providers
                .reconfigure(&serde_json::from_value(renamed).unwrap(), root.path())
                .await
                .unwrap();
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "controller rebuild must not end parent-owned session"
            );
            // Fixture advertises no logout method. The service must still stop
            // and reap the captured session before reporting that safe error.
            assert!(auth.logout("local-agent").await.is_err());
            let stopped = projection(&store, &thread).unwrap();
            assert_eq!(stopped["runs"][0]["status"], "interrupted");
            assert!(
                stopped["providerSessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|session| session["status"] == "stopped")
            );
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            auth.shutdown().await;
            execution.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn managed_registry_binary_installs_and_runs_real_persisted_prompt_approval() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":directory.path()}),Utc::now()).unwrap();
        let (catalog, config) =
            crate::acp_registry_support::fixture_catalog(directory.path(), "normal");
        let settings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},"local-agent":{"driver":"acpRegistry","enabled":true,"config":config}}})).unwrap();
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
/// A native request the agent is blocked on, answered by `runtime-request.respond`.
enum Pending {
    Permission(Callback),
    /// A form elicitation; only the keys its schema asked for may come back.
    Question {
        keys: Vec<String>,
        response: oneshot::Sender<Result<t3_acp::v2::CreateElicitationResponse, AcpError>>,
        written: oneshot::Receiver<Result<(), AcpError>>,
    },
}
struct Actor {
    cwd: std::path::PathBuf,
    store: Store,
    thread_id: String,
    session_id: String,
    instance_id: String,
    registry_agent_id: String,
    item_identity_v2: bool,
    session: AcpSession,
    active_run: Option<String>,
    active_context: Option<crate::provider_turn_context::TurnContext>,
    callbacks: HashMap<String, Pending>,
    /// Session mode and mode-like options as they were before the last plan turn.
    native_build: Option<crate::acp_interaction::NativeBuild>,
    elicitations: u64,
    /// Whether the user (or a steering restart) asked this turn to stop, as opposed to the agent cancelling it.
    interrupt_requested: bool,
    context_usage: Option<Value>,
    native_metadata: Option<Value>,
    turn_ordinal: u64,
    prompt: Option<tokio::task::JoinHandle<Result<PromptResponse, AcpError>>>,
    text_segments: HashMap<&'static str, u64>,
    /// Streams with an open segment, and the source message id that segment was identified by.
    text_open: HashMap<&'static str, Option<String>>,
    tools: HashMap<String, crate::acp_tools::ToolState>,
    tool_emission: HashMap<String, (Option<usize>, usize)>,
    instructions: Option<crate::provider_instructions::AcpState>,
    pending_instructions: Option<crate::provider_instructions::AcpState>,
    has_mcp: bool,
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
/// Whether the agent refused `session/load` or `session/resume` of a saved session.
fn session_activation_failed(cause: &AcpError) -> bool {
    matches!(cause, AcpError::Failure(failure) if matches!(failure.as_ref(), t3_acp::errors::Failure::Request(request) if matches!(request.diagnostics.method.as_deref(), Some("session/load" | "session/resume"))))
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
        pending_startup: &mut Option<crate::acp_coordinator::ForegroundStartup>,
        pending_access: &mut Option<crate::provider_auth_flow::SessionAccess>,
        lifetime: &crate::codex_runtime::ActorLifetime,
        mcp: Option<&crate::provider_mcp::CredentialLease>,
    ) -> Result<Self, StoreError> {
        let view = projection(&store, thread_id)?;
        let instance_id = lifetime
            .captured_instance()
            .ok_or_else(|| error("Provider instance is unavailable."))?;
        if pending_access.is_none() {
            if let Some(auth) = providers.authentication_service() {
                let lifetime = lifetime.clone();
                *pending_access = Some(
                    auth.admit_session(
                        &instance_id,
                        Arc::new(move || {
                            let lifetime = lifetime.clone();
                            Box::pin(async move { lifetime.stop_and_wait().await })
                        }),
                    )
                    .await
                    .map_err(error)?,
                );
            }
        }
        let instance = providers.acp(&instance_id).map_err(error)?;
        *pending_startup = Some(
            instance
                .coordinator
                .foreground_startup(&instance.startup_key()),
        );
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
        let mut item_identity_v2 = saved.is_none()
            || saved_thread
                .is_some_and(|thread| thread["nativeMetadata"]["itemIdentityVersion"] == 2);
        let mut saved = saved;
        let session = loop {
            let mut process = instance.resolve_process(&cwd).await.map_err(error)?;
            if let Some(mcp) = mcp {
                mcp.apply_acp_environment(&mut process.environment);
            }
            let mut callback_instance = instance.clone();
            callback_instance.environment = process.environment.clone();
            let peer = crate::acp_peer::ProcessPeer::spawn(process).map_err(error)?;
            // Retain the process outside this cancellable setup future. The actor
            // shutdown path waits for its reap before releasing the runtime lease.
            *pending_peer = Some(peer.clone());
            let services = callback_instance.services_for(
                &cwd,
                crate::acp_client_callbacks::policy(&view["thread"]["runtimeMode"], &cwd),
            );
            // Keep callbacks outside the cancellable initialize/load future. Their
            // owned terminals must be reaped before runtime ownership is released.
            *pending_services = Some(services.clone());
            let started = match mcp {
                Some(mcp) => {
                    instance
                        .start_peer_with_mcp(peer, &cwd, saved, services, mcp)
                        .await
                }
                None => {
                    instance
                        .start_peer_with_services(peer, &cwd, saved, false, services)
                        .await
                }
            };
            match started {
                Ok(session) => break session,
                // A saved session the agent will no longer load or resume starts a fresh one in a new
                // process; its items are then identified under this instance (`itemIdentityVersion` 2).
                Err(cause) if saved.is_some() && session_activation_failed(&cause) => {
                    if let Some(services) = pending_services.take() {
                        services.shutdown().await;
                    }
                    if let Some(peer) = pending_peer.take() {
                        peer.shutdown().await;
                    }
                    saved = None;
                    item_identity_v2 = true;
                }
                Err(cause) => return Err(error(cause)),
            }
        };
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let mut capabilities: Value =
            serde_json::from_str(include_str!("acp-capabilities.json")).unwrap();
        let setup = serde_json::to_value(&session.setup).unwrap();
        // These source capabilities require adapter paths still being ported.
        // A negotiated load operation alone is not a conversation snapshot API.
        for (group, fields) in [
            ("sessions", &["supportsProviderSwitchingViaHandoff"][..]),
            ("threads", &["canReadThreadSnapshot"][..]),
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
                    "supportsNestedCheckpointScopes",
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
        capabilities["tools"]["supportsMcpTools"] = json!(mcp.is_some());
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
            registry_agent_id: if instance.config.source
                == t3_contracts::AcpRegistrySettingsSource::Registry
            {
                instance.config.agent_id.to_string()
            } else {
                String::new()
            },
            item_identity_v2,
            session,
            active_run: None,
            active_context: None,
            callbacks: HashMap::new(),
            native_build: None,
            elicitations: 0,
            interrupt_requested: false,
            context_usage: None,
            native_metadata: None,
            turn_ordinal: 0,
            prompt: None,
            text_segments: HashMap::new(),
            text_open: HashMap::new(),
            tools: HashMap::new(),
            tool_emission: HashMap::new(),
            instructions: None,
            pending_instructions: None,
            has_mcp: mcp.is_some(),
        })
    }
    async fn effect(&mut self, effect: &crate::persistence::Effect) -> Result<(), StoreError> {
        match effect.request["type"].as_str() {
            Some("provider-turn.start") => {
                self.start_turn(effect.request["runId"].as_str().unwrap())
                    .await
            }
            Some("provider-turn.restart") => self.restart(effect).await,
            Some("provider-turn.steer") => {
                let view = projection(&self.store, &self.thread_id)?;
                let message = find(&view, "messages", &effect.request["messageId"])?;
                if !crate::provider_turn_context::steering_delivery_is_current(&view, message)
                    || effect.request["providerSessionId"] != self.session_id
                {
                    return Ok(());
                }
                let turn = find(&view, "providerTurns", &effect.request["providerTurnId"])?;
                if turn["status"] == "completed" {
                    crate::execution::follow_up_completed_steer(&self.store, effect)?;
                    return Ok(());
                }
                // ACP advertises interrupt/restart rather than native steering.
                Err(error("ACP does not support active native steering."))
            }
            Some("provider-turn.interrupt") => {
                if self.active_run.as_deref() != effect.request["runId"].as_str() {
                    return Err(error("Target run is no longer active."));
                }
                self.interrupt_requested = true;
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
                let written = match callback {
                    Pending::Permission(callback) => {
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
                        callback.written
                    }
                    Pending::Question { keys, response, written } => {
                        let answered = effect.request.get("answers").filter(|answers| answers.is_object());
                        let reply = match answered {
                            Some(answers) => json!({"action":"accept","content":crate::acp_interaction::elicitation_content(answers, &keys)}),
                            None => json!({"action":"cancel"}),
                        };
                        response
                            .send(serde_json::from_value(reply).map_err(|cause| AcpError::Transport(cause.to_string())))
                            .map_err(|_| error("The ACP callback was cancelled."))?;
                        written
                    }
                };
                // Completing the persisted effect means the actual native stdin
                // write was acknowledged, rather than merely releasing a waiter.
                written
                    .await
                    .map_err(|_| error("ACP response acknowledgement was lost."))?
                    .map_err(error)
            }
            _ => Err(error("Unsupported ACP provider effect.")),
        }
    }
    async fn restart(&mut self, effect: &crate::persistence::Effect) -> Result<(), StoreError> {
        let old = self.active_context.clone().filter(|context| {
            effect.request["providerSessionId"] == self.session_id
                && context.attempt_id == effect.request["interruptedAttemptId"]
                && context.provider_turn_id == effect.request["providerTurnId"]
        });
        if let Some(old) = old {
            self.interrupt_requested = true;
            self.session
                .client
                .cancel_typed(
                    serde_json::from_value(json!({"sessionId":self.session.setup.session_id}))
                        .unwrap(),
                )
                .await
                .map_err(error)?;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while self
                    .active_context
                    .as_ref()
                    .is_some_and(|current| current.provider_turn_id == old.provider_turn_id)
                {
                    match self.next().await {
                        Activity::Event(Some(SessionEvent::Update(update))) => {
                            self.update(update)?
                        }
                        Activity::Event(Some(SessionEvent::Permission {
                            request,
                            response,
                            written,
                            ..
                        })) => self.permission(request, response, written).await?,
                        Activity::Event(Some(SessionEvent::Elicitation { request, response, written })) => self.elicitation(request, response, written)?,
                        Activity::Prompt(result) => {
                            self.prompt.take();
                            self.finish(result)?;
                        }
                        Activity::Event(Some(SessionEvent::Terminated(cause))) => {
                            if let Some(prompt) = self.prompt.take() {
                                self.finish(prompt.await.unwrap_or_else(|error| {
                                    Err(AcpError::Transport(error.to_string()))
                                }))?;
                            }
                            return Err(error(cause));
                        }
                        Activity::Event(None) => {
                            return Err(error("ACP event stream ended during restart."));
                        }
                    }
                }
                Ok(())
            })
            .await
            .map_err(|_| error("Provider turn did not terminalize before restart."))??;
        }
        self.start_turn(
            effect.request["runId"]
                .as_str()
                .ok_or_else(|| error("Restart run is missing."))?,
        )
        .await
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
                SessionEvent::Elicitation { response, .. } => {
                    let _ = response.send(serde_json::from_value(json!({"action":"cancel"})).map_err(|cause| AcpError::Transport(cause.to_string())));
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
            return Err(error("This ACP session serves another provider instance."));
        }
        let selection = &run["modelSelection"];
        // "default" and "auto" mean the agent's own choice, which is never sent (AcpAdapterV2 configureSession).
        if let Some(model) = selection["model"].as_str().filter(|model| !matches!(*model, "" | "auto" | "default")) {
            self.session.set_model(model).await.map_err(error)?;
        }
        let mut options = serde_json::Map::new();
        for option in selection["options"].as_array().into_iter().flatten() {
            options.insert(
                option["id"].as_str().unwrap().into(),
                option["value"].clone(),
            );
        }
        self.session.set_options(&options).await.map_err(error)?;
        let changes = crate::acp_interaction::interaction_changes(&serde_json::to_value(&self.session.setup).unwrap(), view["thread"]["interactionMode"] == "plan", &mut self.native_build);
        self.session.set_options(&changes).await.map_err(error)?;
        let user_message = find(&view, "messages", &run["userMessageId"])?;
        let text = crate::attachment_prompt::provider_text(&self.store, user_message);
        // What the run's handoffs say (a fork, a merge-back, a provider switch) goes ahead of the message.
        let inline = crate::claude_fork::plan_inline(&view, run, user_message["text"].as_str().unwrap_or(""), &user_message["attachments"]).map_err(error)?;
        let text = match &inline {
            Some(inline) => crate::claude_fork::with_context(&inline.context, &text),
            None => text,
        };
        let images = crate::attachment_prompt::acp_image_blocks(&self.store, user_message)
            .await
            .map_err(error)?;
        if !images.is_empty()
            && self.session.initialize.agent_capabilities.as_ref()
                .and_then(|capabilities| capabilities.prompt_capabilities.as_ref())
                .and_then(|capabilities| capabilities.image)
                != Some(true)
        {
            return Err(error("ACP driver did not negotiate image prompt support"));
        }
        let instruction_state = crate::provider_instructions::AcpState {
            plan: view["thread"]["interactionMode"] == "plan",
            mcp: self.has_mcp,
        };
        let prompt_text =
            crate::provider_instructions::acp_prompt(&text, instruction_state, self.instructions);
        let runtime_context = crate::provider_instructions::runtime(
            "acpRegistry",
            selection["model"].as_str().unwrap(),
            None,
            None,
        );
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
        self.turn_ordinal = run["ordinal"].as_u64().unwrap_or(0);
        let now = at(Utc::now());
        let mut captured = crate::provider_turn_context::TurnContext::capture(&view, run_id)?;
        commit(&self.store, &self.thread_id, |view| {
            if !captured.is_current(view) {
                return Err(error("ACP admission belongs to a replaced attempt."));
            }
            let (mut run, attempt) = captured.rows(view)?;
            if run["status"] != "starting" {
                return Err(error("Run was interrupted before ACP prompt admission."));
            }
            let mut attempt = attempt.clone();
            let mut node = find(view, "nodes", &captured.root_node_id)?.clone();
            let mut thread = find(view, "providerThreads", &captured.provider_thread_id)?.clone();
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
        self.pending_instructions = (prompt_text != text).then_some(instruction_state);
        // Recorded as pending first: a prompt the agent took and then lost is found by the next turn.
        if let Some(inline) = &inline {
            inline.mark(&self.store, &self.thread_id, "pending").map_err(error)?;
        }
        self.active_run = Some(run_id.into());
        captured.provider_turn_id = json!(provider_turn_id);
        self.active_context = Some(captured);
        self.text_segments.clear();
        self.text_open.clear();
        self.tools.clear();
        self.tool_emission.clear();
        let client = self.session.client.clone();
        let session_id = self.session.setup.session_id.clone();
        self.prompt = Some(tokio::spawn(async move {
            client
                .prompt_typed(PromptRequest {
                    session_id,
                    prompt: crate::attachment_prompt::acp_prompt_blocks(prompt_text, images, runtime_context)
                        .into_iter()
                        .map(|block| serde_json::from_value(block).unwrap())
                        .collect(),
                    meta: Optional::Missing,
                })
                .await
        }));
        if let Some(inline) = &inline {
            inline.mark(&self.store, &self.thread_id, "inline").map_err(error)?;
        }
        Ok(())
    }
    fn context<'a>(&self, view: &'a Value) -> Result<(Value, &'a Value), StoreError> {
        self.active_context
            .as_ref()
            .ok_or_else(|| error("ACP turn context is missing."))?
            .rows(view)
    }
    /// The provider's items of a run take the next ordinal in the run's hundred (`resolveItemOrdinal`),
    /// so they sort after the run's user message and before the next run's checkpoint.
    fn next_ordinal(&self, view: &Value, run: &Value) -> u64 {
        let base = run["ordinal"].as_u64().unwrap_or(0) * 100;
        let used = view["turnItems"].as_array().unwrap().iter().filter(|item| item["runId"] == run["id"] && item["ordinal"].as_u64().is_some_and(|ordinal| ordinal > base && ordinal < base + 100)).count() as u64;
        base + used + 1
    }
    fn base_item(&self, view: &Value, run: &Value, attempt: &Value, id: &str, node: &str) -> Value {
        let now = at(Utc::now());
        json!({"id":id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":null,"ordinal":self.next_ordinal(view,run),"status":"running","title":null,"startedAt":now,"completedAt":null,"updatedAt":now})
    }
    fn update(
        &mut self,
        notification: t3_acp::types::SessionNotification,
    ) -> Result<(), StoreError> {
        if notification.session_id != self.session.setup.session_id {
            return Err(error("ACP update belongs to another session."));
        }
        let mut value = serde_json::to_value(notification).unwrap();
        if self.registry_agent_id == "devin" {
            updates::devin_update(&mut value);
        }
        if value["_meta"]["isReplay"] == true {
            return Ok(());
        }
        let update = &value["update"];
        // Usage and session info belong to the thread whether or not a turn is running.
        if self.thread_state(update)? {
            return Ok(());
        }
        if self.active_run.is_none() {
            return Ok(());
        } // Historical load replay is not a new turn.
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
                let message_id = update["messageId"].as_str().map(str::trim).filter(|id| !id.is_empty()).map(ToOwned::to_owned);
                self.project_text(reasoning, &text, message_id.as_deref(), false)
            }
            // v2 whole-message updates replace the message they name (`replaceText`).
            Some("agent_message" | "agent_thought") => {
                let Some(contents) = update["content"].as_array() else { return Ok(()) };
                let reasoning = update["sessionUpdate"] == "agent_thought";
                let text = contents.iter().filter_map(crate::acp_model::content_display_text).collect::<Vec<_>>().join("\n");
                let Some(message_id) = update["messageId"].as_str().map(str::trim).filter(|id| !id.is_empty()) else { return Ok(()) };
                self.project_text(reasoning, &text, Some(message_id), true)
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
            _ => self.other_update(update).map(drop),
        }
    }
    fn tool(&mut self, update: &Value) -> Result<(), StoreError> {
        self.tool_with(update, None)
    }
    /// A tool update; `status` overrides the parsed one for states a wire update cannot name (`requiresAction`).
    fn tool_with(&mut self, update: &Value, status: Option<&str>) -> Result<(), StoreError> {
        // An identified message can stream alongside tool updates; the rest end where a tool begins.
        let ended: Vec<&'static str> = ["assistant", "reasoning"].into_iter().filter(|stream| matches!(self.text_open.get(stream), Some(None))).collect();
        self.close_text(&ended)?;
        let Some(mut parsed) =
            crate::acp_tools::ToolState::parse(update, update["sessionUpdate"] == "tool_call")
        else {
            return Ok(());
        };
        if let Some(status) = status {
            parsed.wire["status"] = json!(status);
        }
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
                .unwrap_or_else(|| self.base_item(view, &run, attempt, &item_id, &node_id));
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
            if id.starts_with("acp-compaction:") {
                item["type"] = json!("compaction");
                item["driver"] = json!("acpRegistry");
                item["title"] = native.get("title").cloned().unwrap_or(Value::Null);
                if let Some(summary) = output.and_then(crate::acp_tools::output_text) {
                    item["summary"] = json!(summary);
                }
                if let Some(used) = self.context_usage.as_ref().map(|usage| usage["usedTokens"].clone()) {
                    item["beforeTokenCount"] = used;
                }
            } else if let Some(identity) = crate::acp_mcp_tools::identity(
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
        })?;
        // A finished compaction resets the context the usage described.
        if id.starts_with("acp-compaction:") && self.tools.get(&id).is_some_and(|tool| tool.wire["status"] == "completed") {
            self.context_usage = None;
        }
        Ok(())
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
                .unwrap_or_else(|| self.base_item(view, &run, attempt, &id, &node_id));
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
            // The agent's own reject option when it offers one; cancelled otherwise.
            let _ = response.send(permission_response(&request, "decline"));
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
            let mut item = self.base_item(view, &run, attempt, &item_id, &node_id);
            item["nativeItemRef"] = native_ref;
            item["status"] = json!("waiting");
            item["type"] = json!("approval_request");
            item["requestId"] = json!(request_id);
            item["requestKind"] = json!(request_kind);
            // `parsePermissionRequest`: the command, else the presented title, else the detail.
            let parsed = crate::acp_tools::ToolState::parse(&native["toolCall"], true);
            let prompt = parsed
                .as_ref()
                .and_then(|tool| ["command", "title", "detail"].iter().find_map(|key| tool.wire[*key].as_str()))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("Session {}", request.session_id));
            item["prompt"] = json!(prompt);
            Ok(vec![
                ("node.updated", node),
                ("runtime-request.updated", row),
                ("turn-item.updated", item),
            ])
        })?;
        self.callbacks.insert(
            request_id,
            Pending::Permission(Callback {
                request,
                response,
                written,
            }),
        );
        Ok(())
    }
    /// A form elicitation becomes a user-input request; its answers go back as the elicitation's content.
    fn elicitation(
        &mut self,
        request: Value,
        response: oneshot::Sender<Result<t3_acp::v2::CreateElicitationResponse, AcpError>>,
        written: oneshot::Receiver<Result<(), AcpError>>,
    ) -> Result<(), StoreError> {
        let cancel = |response: oneshot::Sender<Result<t3_acp::v2::CreateElicitationResponse, AcpError>>| {
            let _ = response.send(serde_json::from_value(json!({"action":"cancel"})).map_err(|cause| AcpError::Transport(cause.to_string())));
        };
        if self.active_run.is_none() {
            cancel(response);
            return Ok(());
        }
        self.close_text_streams()?;
        let questions = crate::acp_interaction::elicitation_questions(&request);
        let keys: Vec<String> = request["requestedSchema"]["properties"].as_object().into_iter().flatten().map(|(key, _)| key.clone()).collect();
        let scope = request["sessionId"].as_str().map(ToOwned::to_owned).unwrap_or_else(|| format!("request:{}", request["requestId"].as_str().unwrap_or("")));
        let native_id = format!("{scope}:elicitation:{}", self.elicitations);
        self.elicitations += 1;
        let request_id = uuid::Uuid::new_v4().to_string();
        let node_id = format!("node:{request_id}");
        let item_id = format!("item:{request_id}");
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let native = native_ref(&native_id, false);
            let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":"user_input_request","status":"waiting","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":native,"runtimeRequestId":request_id,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
            let row = json!({"id":request_id,"nodeId":node_id,"providerTurnId":attempt["providerTurnId"],"nativeRequestRef":native,"kind":"user_input","status":"pending","responseCapability":{"type":"live","providerSessionId":self.session_id},"createdAt":now,"resolvedAt":null});
            let mut item = self.base_item(view, &run, attempt, &item_id, &node_id);
            item["nativeItemRef"] = native;
            item["status"] = json!("waiting");
            item["type"] = json!("user_input_request");
            item["requestId"] = json!(request_id);
            item["questions"] = json!(questions);
            Ok(vec![("node.updated", node), ("runtime-request.updated", row), ("turn-item.updated", item)])
        })?;
        self.callbacks.insert(request_id, Pending::Question { keys, response, written });
        Ok(())
    }
    fn finish(&mut self, result: Result<PromptResponse, AcpError>) -> Result<(), StoreError> {
        let pending_instructions = self.pending_instructions.take();
        if result.is_ok() {
            if let Some(instructions) = pending_instructions {
                self.instructions = Some(instructions);
            }
            self.close_text_streams()?;
        }
        match result {
            Ok(response) => terminal_captured(
                &self.store,
                &self.thread_id,
                self.active_run.as_deref(),
                if response.stop_reason == "cancelled" {
                    if self.interrupt_requested { "interrupted" } else { "cancelled" }
                } else {
                    "completed"
                },
                None,
                self.active_context.as_ref(),
            )?,
            // A prompt that fails reports nothing more: a tool it left running is background work,
            // pending until Stop settles it.
            Err(cause) => fail_run_with(
                &self.store,
                &self.thread_id,
                self.active_run.as_deref(),
                self.active_context.as_ref(),
                &crate::provider_failure::acp_prompt(&self.registry_agent_id, &cause),
                crate::codex_runtime::Retain::OnFailure,
            )?,
        };
        self.active_run = None;
        self.active_context = None;
        self.interrupt_requested = false;
        self.session.services.settle();
        self.callbacks.clear();
        crate::codex_runtime::settle_queue(&self.store, &self.thread_id, None)?;
        Ok(())
    }
    fn close_text_streams(&mut self) -> Result<(), StoreError> {
        self.close_text(&["assistant", "reasoning"])
    }
    /// Ends the open segments of these streams; the next text of a stream starts a new item.
    fn close_text(&mut self, streams: &[&'static str]) -> Result<(), StoreError> {
        if self.active_run.is_none() {
            return Ok(());
        }
        let now = at(Utc::now());
        let types: Vec<&str> = streams.iter().map(|stream| if *stream == "reasoning" { "reasoning" } else { "assistant_message" }).collect();
        commit(&self.store, &self.thread_id, |view| {
            let mut events = vec![];
            for item in view["turnItems"].as_array().unwrap().iter().filter(|item| {
                item["runId"] == json!(self.active_run)
                    && item["streaming"] == true
                    && item["type"].as_str().is_some_and(|kind| types.contains(&kind))
                    && self
                        .active_context
                        .as_ref()
                        .is_some_and(|context| context.owns_item(view, item))
            }) {
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
        for stream in streams {
            if self.text_open.remove(stream).is_some() {
                *self.text_segments.entry(stream).or_default() += 1;
            }
        }
        Ok(())
    }
    /// One text chunk (or a whole-message replacement) of the assistant or reasoning stream.
    /// Text of the other stream ends this one, and so does a different source message id.
    fn project_text(&mut self, reasoning: bool, text: &str, message_id: Option<&str>, replace: bool) -> Result<(), StoreError> {
        let stream: &'static str = if reasoning { "reasoning" } else { "assistant" };
        self.close_text(&[if reasoning { "assistant" } else { "reasoning" }])?;
        match self.text_open.get(stream) {
            Some(Some(open)) if message_id.is_some_and(|id| id != open) => self.close_text(&[stream])?,
            Some(None) if message_id.is_some() => {
                // An agent that omits the id on its first chunk keeps that item and takes the first later id.
                self.text_open.insert(stream, message_id.map(ToOwned::to_owned));
            }
            _ => {}
        }
        self.text_open.entry(stream).or_insert_with(|| message_id.map(ToOwned::to_owned));
        // Keyed by provider turn: a steering restart re-prompts within the same run.
        let turn = self.active_context.as_ref().and_then(|context| context.provider_turn_id.as_str()).unwrap_or_else(|| self.active_run.as_deref().unwrap());
        let id = format!("{turn}:{stream}:{}", self.text_segments.get(stream).copied().unwrap_or(0));
        let node_id = format!("node:{id}");
        let message_item_id = format!("message:{id}");
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let previous = view["turnItems"].as_array().unwrap().iter().find(|item| item["id"] == id);
            let mut item = previous.cloned().unwrap_or_else(|| self.base_item(view, &run, attempt, &id, &node_id));
            item["type"] = json!(if reasoning { "reasoning" } else { "assistant_message" });
            item["text"] = if replace { json!(text) } else { json!(format!("{}{text}", item["text"].as_str().unwrap_or(""))) };
            item["streaming"] = json!(true);
            item["updatedAt"] = json!(now);
            let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":if reasoning{"reasoning"}else{"assistant_message"},"status":"running","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":item["startedAt"],"completedAt":null});
            let mut events = vec![("node.updated", node)];
            if !reasoning {
                item["messageId"] = json!(message_item_id);
                events.push(("message.updated",json!({"id":message_item_id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node_id,"role":"assistant","createdBy":"agent","creationSource":"provider","text":item["text"],"attachments":[],"streaming":true,"createdAt":item["startedAt"],"updatedAt":now})));
            }
            events.push(("turn-item.updated", item));
            Ok(events)
        })
    }
}
pub(crate) async fn actor(
    store: Store,
    providers: ProviderRegistry,
    thread_id: String,
    mut work: mpsc::Receiver<Work>,
    mut stopped: watch::Receiver<bool>,
    mut canceled: watch::Receiver<Option<String>>,
    lifetime: crate::codex_runtime::ActorLifetime,
    mcp: Option<crate::provider_mcp::CredentialLease>,
) {
    let mut runtime: Option<Actor> = None;
    let mut pending_peer = None;
    let mut pending_services = None;
    let mut pending_startup = None;
    // This access remains owned by the actor through initialization cancellation
    // and successful sessions, until its callback/provider processes are reaped.
    let mut access = None;
    let mut interrupted = false;
    let mut detached = None;
    loop {
        if *stopped.borrow() {
            break;
        }
        tokio::select! {
            _=stopped.changed()=>break,
            incoming=work.recv()=>{
                let Some(incoming)=incoming else{break};
                if incoming.effect.request["type"] == "provider-session.detach" {
                    if runtime.as_ref().is_some_and(|actor| incoming.effect.request["providerSessionId"] == actor.session_id) {
                        // Exclusive ACP detach closes this process. Acknowledge
                        // only after callbacks, peer, access and grant cleanup.
                        detached = Some(incoming.complete);
                        interrupted = true;
                        break;
                    }
                    let _ = incoming.complete.send(Ok(()));
                    continue;
                }
                if incoming.effect.request["type"] == "provider-thread.rollback" {
                    // ACP has no truncation: the thread gets a fresh native session, so this actor
                    // (bound to the old one) ends once the rollback has committed.
                    if runtime.as_ref().is_some_and(|actor| actor.active_run.is_some()) {
                        let _ = incoming.complete.send(Err("Cannot roll back an ACP thread while a turn is active.".into()));
                        continue;
                    }
                    match crate::checkpoint_rollback::run_acp(&store, &incoming.effect).await {
                        Ok(true) => { work.close(); interrupted = true; detached = Some(incoming.complete); break; }
                        Ok(false) => { let _ = incoming.complete.send(Ok(())); continue; }
                        Err(cause) => { let _ = incoming.complete.send(Err(cause)); continue; }
                    }
                }
                let starting=matches!(incoming.effect.request["type"].as_str(),Some("provider-turn.start"|"provider-turn.restart"));let run_id=incoming.effect.request["runId"].as_str();
                let result=tokio::select! {
                    result=async{if starting {if let Some(mcp)=mcp.as_ref() {mcp.touch();}}if runtime.is_none(){runtime=Some(Actor::connect(store.clone(),&providers,&thread_id,&mut pending_peer,&mut pending_services,&mut pending_startup,&mut access,&lifetime,mcp.as_ref()).await?);pending_services.take();pending_peer.take();pending_startup.take();}runtime.as_mut().unwrap().effect(&incoming.effect).await}=>result,
                    _=stopped.changed()=>break,
                    _=async{loop{if canceled.borrow_and_update().as_deref()==run_id{break;}if canceled.changed().await.is_err(){std::future::pending::<()>().await;}}},if starting=>{interrupted=true;Err(error("Run interrupted during ACP startup."))}
                };
                let close=starting&&result.is_err();if interrupted||close{work.close();}let _=incoming.complete.send(result.map_err(|error|error.to_string()));if interrupted||close{break;}
            }
            activity=async{runtime.as_mut().unwrap().next().await},if runtime.is_some()=>{
                let ended=matches!(&activity,Activity::Event(Some(SessionEvent::Terminated(_)))|Activity::Event(None));
                let actor=runtime.as_mut().unwrap();let result=match activity {
                    Activity::Event(Some(SessionEvent::Update(update)))=>actor.update(update),
                    Activity::Event(Some(SessionEvent::Permission{request,response,written,..}))=>tokio::select!{result=actor.permission(request,response,written)=>result,_=stopped.changed()=>break},Activity::Event(Some(SessionEvent::Elicitation{request,response,written}))=>actor.elicitation(request,response,written),
                    Activity::Event(Some(SessionEvent::Terminated(cause)))=>{if let Some(prompt)=actor.prompt.take(){actor.finish(prompt.await.unwrap_or_else(|error|Err(AcpError::Transport(error.to_string()))))}else{Err(error(cause))}},Activity::Event(None)=>Err(error("ACP event stream ended.")),
                    Activity::Prompt(result)=>{actor.prompt.take();actor.finish(result)}
                };
                if let Err(cause)=result {work.close();let _=fail_run_captured(&store,&thread_id,actor.active_run.as_deref(),actor.active_context.as_ref(),&crate::provider_failure::make(&json!({"message":cause.to_string(),"class":"transport_error","retryable":false})));break;}
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
    // Startup admission outlives canceled setup until callback terminals and
    // the provider process have finished cleanup.
    drop(pending_startup);
    if let Some(mut actor) = runtime {
        let status = if *stopped.borrow() || interrupted {
            "interrupted"
        } else {
            "failed"
        };
        let _ = terminal_captured(
            &store,
            &thread_id,
            actor.active_run.as_deref(),
            status,
            None,
            actor.active_context.as_ref(),
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
        if !interrupted && !*stopped.borrow() && actor.active_run.is_none() && actor.callbacks.is_empty() {
            actor.session.close_session().await;
        }
        actor.callbacks.clear();
        actor.session.shutdown().await;
    }
    drop(access);
    drop(mcp);
    if let Some(detached) = detached {
        let _ = detached.send(Ok(()));
    }
}
