//! ACP registry session, provider and logout management through the real WebSocket transport, against a
//! scripted ACP v2 agent. Each management call starts that agent afresh; its log shows what it was asked.
#![cfg(unix)]
mod provider_manage_support;
use chrono::Utc;
use provider_manage_support::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use t3_server::{project::ProjectService, thread::ThreadService};

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/acp-session-manager.py");

fn agent(base: &Path, name: &str, scenario: &str) -> Value {
    json!({"driver":"acpRegistry","enabled":true,"environment":[{"name":"MANAGE_SCENARIO","value":scenario}],
           "config":{"source":"local","commandPath":AGENT,"commandArgs":[base.join(format!("{name}.log"))]}})
}
async fn fixture() -> Harness {
    let harness = start(
        |base| {
            json!({"agent":agent(base, "agent", "normal"),"nodelete":agent(base, "nodelete", "no-delete"),"noproviders":agent(base, "noproviders", "no-providers"),
                   "nosession":agent(base, "nosession", "no-session"),"noauth":agent(base, "noauth", "no-auth"),"listerror":agent(base, "listerror", "list-error"),
                   "deleteerror":agent(base, "deleteerror", "delete-error"),"autherror":agent(base, "autherror", "auth-error"),"hold":agent(base, "hold", "hold")})
        },
        |_, _| {},
    )
    .await;
    let root = harness.base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    ProjectService::new(harness.store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":root}), Utc::now()).unwrap();
    harness
}
fn log(harness: &Harness, name: &str) -> Vec<Value> {
    std::fs::read_to_string(harness.base.join(format!("{name}.log"))).map(|text| text.lines().map(|line| serde_json::from_str(line).unwrap()).collect()).unwrap_or_default()
}
fn requests(harness: &Harness, name: &str, method: &str) -> Vec<Value> {
    log(harness, name).into_iter().filter(|entry| entry["method"] == method && entry["client"] == "t3-code-session-manager").collect()
}
fn operation_error(exit: &Value, reason: &str) -> String {
    let error = failure(exit);
    assert_eq!((error["_tag"].as_str(), error["reason"].as_str()), (Some("AcpRegistryOperationError"), Some(reason)), "{exit}");
    assert!(error.get("cause").is_none(), "an agent's own words never reach the wire: {exit}");
    error["message"].as_str().unwrap().to_owned()
}
fn thread_id(instance: &str, session: &str) -> String {
    format!("thread:provider:acpRegistry:provider-instance:{instance}:native-thread:{session}")
}
fn root(harness: &Harness) -> PathBuf {
    harness.base.join("project")
}
fn make_thread(harness: &Harness, id: &str) {
    ThreadService::new(harness.store.clone())
        .dispatch(&json!({"type":"thread.create","commandId":format!("create-{id}"),"threadId":id,"projectId":"project","title":"Existing","modelSelection":{"instanceId":"agent","model":"m"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}), Utc::now())
        .unwrap();
}
fn target(instance: &str) -> Value {
    json!({"instanceId":instance,"projectId":"project"})
}
fn with(mut base: Value, extra: Value) -> Value {
    for (key, value) in extra.as_object().unwrap() {
        base[key] = value.clone();
    }
    base
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_sessions_normalizes_pages_marks_imports_and_reports_failures_by_reason() {
    let harness = fixture().await;
    let mut reader = harness.connect(&harness.reader).await;
    let page = rpc(&mut reader, 1, "server.listAcpRegistrySessions", target("agent")).await;
    let page = success(&page);
    assert_eq!(page["sessions"].as_array().unwrap().len(), 1, "empty ids and unusable directories are dropped: {page}");
    let first = &page["sessions"][0];
    assert_eq!((first["sessionId"].clone(), first["title"].clone(), first["updatedAt"].clone()), (json!("s-1"), json!("First session"), json!("2026-01-01T00:00:00Z")));
    assert_eq!(first["additionalDirectories"], json!(["/extra"]));
    assert_eq!(first["cwd"], json!(root(&harness)));
    assert_eq!((page["nextCursor"].clone(), page["canLoad"].clone(), page["canResume"].clone(), page["canDelete"].clone()), (json!("page2"), json!(true), json!(true), json!(true)));
    let asked = requests(&harness, "agent", "session/list");
    assert_eq!(asked[0]["params"], json!({"cwd":root(&harness)}));

    let next = rpc(&mut reader, 2, "server.listAcpRegistrySessions", with(target("agent"), json!({"cursor":"page2"}))).await;
    assert_eq!(success(&next)["sessions"][0]["sessionId"], "s-2");
    assert_eq!(success(&next)["nextCursor"], Value::Null);
    assert_eq!(requests(&harness, "agent", "session/list")[1]["params"], json!({"cwd":root(&harness),"cursor":"page2"}));

    // A session that already has a thread says which.
    make_thread(&harness, &thread_id("agent", "s-1"));
    let marked = rpc(&mut reader, 3, "server.listAcpRegistrySessions", target("agent")).await;
    assert_eq!(success(&marked)["sessions"][0]["importedThreadId"], json!(thread_id("agent", "s-1")));

    // The things that can go wrong, each with its own reason and none with the agent's own words.
    assert_eq!(operation_error(&rpc(&mut reader, 4, "server.listAcpRegistrySessions", target("ghost")).await, "instance_not_found"), "Provider instance ghost was not found.");
    assert_eq!(operation_error(&rpc(&mut reader, 5, "server.listAcpRegistrySessions", json!({"instanceId":"agent","projectId":"nope"})).await, "project_not_found"), "Project nope was not found.");
    assert_eq!(operation_error(&rpc(&mut reader, 6, "server.listAcpRegistrySessions", target("codex")).await, "instance_not_found"), "Provider instance codex was not found.");
    let refused = rpc(&mut reader, 7, "server.listAcpRegistrySessions", target("listerror")).await;
    let message = operation_error(&refused, "authentication_failed");
    assert_eq!(message, "The ACP agent could not complete authentication.");
    assert!(!refused.to_string().contains("sk-secret-value"));
    let silent = rpc(&mut reader, 8, "server.listAcpRegistrySessions", target("nosession")).await;
    assert_eq!(operation_error(&silent, "session_list_unsupported"), "The ACP agent does not advertise session listing.");
    assert!(defect(&rpc(&mut reader, 9, "server.listAcpRegistrySessions", json!({"instanceId":"agent"})).await).as_str().is_some());

    // An agent that never answers is stopped when the budget runs out.
    harness.registry.management().configure(|options| options.acp_timeout = std::time::Duration::from_millis(400));
    let held = rpc(&mut reader, 10, "server.listAcpRegistrySessions", target("hold")).await;
    assert_eq!(operation_error(&held, "probe_failed"), "The ACP session list request timed out.");
    drop(reader);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn importing_a_session_checks_what_the_agent_can_do_and_never_duplicates_a_thread() {
    let harness = fixture().await;
    let mut operator = harness.connect(&harness.operator).await;
    let import = |instance: &str, session: &str| with(target(instance), json!({"sessionId":session,"title":"Imported"}));

    // Already imported: the existing thread answers, nothing is created.
    make_thread(&harness, &thread_id("agent", "s-1"));
    let existing = rpc(&mut operator, 1, "server.importAcpRegistrySession", import("agent", "s-1")).await;
    assert_eq!(success(&existing), &json!({"threadId":thread_id("agent", "s-1"),"imported":false}));

    // An agent that cannot load or resume native sessions is refused before anything is created.
    let cannot = rpc(&mut operator, 2, "server.importAcpRegistrySession", import("nosession", "s-1")).await;
    assert_eq!(operation_error(&cannot, "session_resume_unsupported"), "The ACP agent cannot load or resume native sessions.");
    assert_eq!(operation_error(&rpc(&mut operator, 3, "server.importAcpRegistrySession", import("ghost", "s-1")).await, "instance_not_found"), "Provider instance ghost was not found.");
    assert_eq!(operation_error(&rpc(&mut operator, 4, "server.importAcpRegistrySession", json!({"instanceId":"agent","projectId":"nope","sessionId":"s-1"})).await, "project_not_found"), "Project nope was not found.");

    // A new session becomes a thread that owns the native-session reference, as an active provider thread
    // that is not loaded yet; importing it again answers with the thread it made.
    let fresh = rpc(&mut operator, 5, "server.importAcpRegistrySession", import("agent", "s-2")).await;
    assert_eq!(success(&fresh), &json!({"threadId":thread_id("agent", "s-2"),"imported":true}));
    let thread = harness.store.projection("thread", &thread_id("agent", "s-2")).unwrap().unwrap();
    let provider = &thread["providerThreads"][0];
    assert_eq!(thread["thread"]["title"], "Imported");
    assert_eq!(provider["id"], format!("provider-thread:provider:acpRegistry:provider-instance:agent:native-thread:s-2"));
    assert_eq!(thread["thread"]["activeProviderThreadId"], provider["id"]);
    assert_eq!((provider["status"].clone(), provider["providerSessionId"].clone(), provider["firstRunOrdinal"].clone()), (json!("not_loaded"), Value::Null, Value::Null));
    assert_eq!(provider["nativeThreadRef"], json!({"driver":"acpRegistry","nativeId":"s-2","strength":"strong"}));
    assert_eq!(provider["nativeMetadata"], json!({"itemIdentityVersion":2,"title":"Imported"}));
    let again = rpc(&mut operator, 6, "server.importAcpRegistrySession", import("agent", "s-2")).await;
    assert_eq!(success(&again), &json!({"threadId":thread_id("agent", "s-2"),"imported":false}));
    drop(operator);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_native_session_needs_the_capability_and_never_removes_an_imported_threads_session() {
    let harness = fixture().await;
    let mut operator = harness.connect(&harness.operator).await;
    let delete = |instance: &str, session: &str| with(target(instance), json!({"sessionId":session}));
    let deleted = rpc(&mut operator, 1, "server.deleteAcpRegistrySession", delete("agent", "s-2")).await;
    assert_eq!(success(&deleted), &json!({"deleted":true}));
    assert_eq!(requests(&harness, "agent", "session/delete")[0]["params"], json!({"sessionId":"s-2"}));
    assert_eq!(operation_error(&rpc(&mut operator, 2, "server.deleteAcpRegistrySession", delete("nodelete", "s-2")).await, "session_delete_unsupported"), "The ACP agent does not advertise session deletion.");
    assert!(requests(&harness, "nodelete", "session/delete").is_empty());

    make_thread(&harness, &thread_id("agent", "s-1"));
    let imported = rpc(&mut operator, 3, "server.deleteAcpRegistrySession", delete("agent", "s-1")).await;
    assert_eq!(operation_error(&imported, "session_delete_failed"), "Delete the imported T3 thread before deleting its native ACP session.");
    assert_eq!(requests(&harness, "agent", "session/delete").len(), 1);

    let broken = rpc(&mut operator, 4, "server.deleteAcpRegistrySession", delete("deleteerror", "s-2")).await;
    assert_eq!(operation_error(&broken, "session_delete_failed"), "Could not delete the ACP session.");
    assert!(!broken.to_string().contains("/home/user/.secret"));

    // Operating threads is enough to import and delete sessions; reading is not.
    let mut reader = harness.connect(&harness.reader).await;
    let denied = rpc(&mut reader, 1, "server.deleteAcpRegistrySession", delete("agent", "s-2")).await;
    assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
    assert_eq!(failure(&denied)["requiredPermission"], "orchestration:operate");
    let denied = rpc(&mut reader, 2, "server.importAcpRegistrySession", delete("agent", "s-2")).await;
    assert_eq!(failure(&denied)["requiredPermission"], "orchestration:operate");
    assert_eq!(requests(&harness, "agent", "session/delete").len(), 1);
    drop((operator, reader));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_configuration_is_listed_validated_applied_and_republished() {
    let harness = fixture().await;
    let mut manager = harness.connect(&harness.manager).await;
    let before = provider(&Value::from(harness.registry.snapshots()), "agent")["checkedAt"].clone();
    let listed = rpc(&mut manager, 1, "server.listAcpRegistryProviders", target("agent")).await;
    assert_eq!(success(&listed), &json!({"providers":[
        {"providerId":"openai","supported":["openai-compat","anthropic"],"required":false,"current":{"apiType":"openai-compat","baseUrl":"https://api.example.test/v1"}},
        {"providerId":"core","supported":["native"],"required":true,"current":null}]}));
    assert_eq!(operation_error(&rpc(&mut manager, 2, "server.listAcpRegistryProviders", target("noproviders")).await, "providers_unsupported"), "The ACP agent does not advertise provider configuration.");

    let set = |payload: Value| with(target("agent"), payload);
    let configured = rpc(&mut manager, 3, "server.setAcpRegistryProvider", set(json!({"providerId":"openai","apiType":"anthropic","baseUrl":"https://api.example.test/anthropic","headers":{"x-team":"secret-team"}}))).await;
    assert_eq!(success(&configured), &json!({"configured":true}));
    assert_eq!(requests(&harness, "agent", "providers/set")[0]["params"], json!({"providerId":"openai","apiType":"anthropic","baseUrl":"https://api.example.test/anthropic","headers":{"x-team":"secret-team"}}));
    // The instance is re-probed so every client's provider list reflects the change.
    assert_ne!(provider(&Value::from(harness.registry.snapshots()), "agent")["checkedAt"], before);

    assert_eq!(operation_error(&rpc(&mut manager, 4, "server.setAcpRegistryProvider", set(json!({"providerId":"openai","apiType":"grpc","baseUrl":"https://x.test"}))).await, "provider_configuration_failed"), "Provider openai does not support grpc.");
    assert_eq!(operation_error(&rpc(&mut manager, 5, "server.setAcpRegistryProvider", set(json!({"providerId":"nope","apiType":"anthropic","baseUrl":"https://x.test"}))).await, "provider_configuration_failed"), "Provider nope does not support anthropic.");
    let many: serde_json::Map<String, Value> = (0..33).map(|index| (format!("h{index}"), json!("v"))).collect();
    assert_eq!(operation_error(&rpc(&mut manager, 6, "server.setAcpRegistryProvider", set(json!({"providerId":"openai","apiType":"anthropic","baseUrl":"https://x.test","headers":many}))).await, "provider_configuration_failed"), "ACP provider configuration accepts at most 32 headers.");
    assert_eq!(requests(&harness, "agent", "providers/set").len(), 1);

    let disabled = rpc(&mut manager, 7, "server.disableAcpRegistryProvider", with(target("agent"), json!({"providerId":"openai"}))).await;
    assert_eq!(success(&disabled), &json!({"disabled":true}));
    assert_eq!(requests(&harness, "agent", "providers/disable")[0]["params"], json!({"providerId":"openai"}));
    assert_eq!(operation_error(&rpc(&mut manager, 8, "server.disableAcpRegistryProvider", with(target("agent"), json!({"providerId":"core"}))).await, "provider_configuration_failed"), "Provider core is required and cannot be disabled.");
    assert_eq!(operation_error(&rpc(&mut manager, 9, "server.disableAcpRegistryProvider", with(target("agent"), json!({"providerId":"ghost"}))).await, "provider_configuration_failed"), "Provider ghost was not advertised by the ACP agent.");
    assert_eq!(requests(&harness, "agent", "providers/disable").len(), 1);

    // Listing providers is reading; changing them is managing.
    let mut reader = harness.connect(&harness.reader).await;
    assert_eq!(success(&rpc(&mut reader, 1, "server.listAcpRegistryProviders", target("agent")).await)["providers"].as_array().unwrap().len(), 2);
    let mut operator = harness.connect(&harness.operator).await;
    for (id, tag, payload) in [(1, "server.setAcpRegistryProvider", set(json!({"providerId":"openai","apiType":"anthropic","baseUrl":"https://x.test"}))), (2, "server.disableAcpRegistryProvider", with(target("agent"), json!({"providerId":"openai"}))), (3, "server.logoutAcpRegistry", json!({"instanceId":"agent"}))] {
        let denied = rpc(&mut operator, id, tag, payload).await;
        assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(failure(&denied)["requiredPermission"], "providers:manage", "{tag}");
    }
    assert_eq!(requests(&harness, "agent", "providers/set").len(), 1);
    drop((manager, reader, operator));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn logging_out_asks_the_agent_and_refuses_what_it_does_not_advertise() {
    let harness = fixture().await;
    let mut manager = harness.connect(&harness.manager).await;
    let out = rpc(&mut manager, 1, "server.logoutAcpRegistry", json!({"instanceId":"agent"})).await;
    assert_eq!(success(&out), &json!({"loggedOut":true}));
    assert_eq!(log(&harness, "agent").iter().filter(|entry| entry["method"] == "auth/logout").count(), 1);
    assert_eq!(operation_error(&rpc(&mut manager, 2, "server.logoutAcpRegistry", json!({"instanceId":"noauth"})).await, "logout_unsupported"), "The ACP agent does not advertise logout.");
    assert!(log(&harness, "noauth").iter().all(|entry| entry["method"] != "auth/logout"));
    let refused = rpc(&mut manager, 3, "server.logoutAcpRegistry", json!({"instanceId":"autherror"})).await;
    assert_eq!(operation_error(&refused, "logout_failed"), "Could not sign out of the ACP agent.");
    assert!(!refused.to_string().contains("Fixture logout refused"));
    assert_eq!(operation_error(&rpc(&mut manager, 4, "server.logoutAcpRegistry", json!({"instanceId":"ghost"})).await, "instance_not_found"), "Provider instance ghost was not found.");
    assert!(defect(&rpc(&mut manager, 5, "server.logoutAcpRegistry", json!({})).await).as_str().is_some());
    drop(manager);
    harness.finish().await;
}
