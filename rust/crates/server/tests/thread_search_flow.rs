//! `orchestration.searchThreads` over a real socket against a store whose projects and threads come
//! from real commands. Messages are written into the thread projections, because the provider that
//! would normally produce them is not part of this flow.
mod rpc_support;
use chrono::Utc;
use rpc_support::*;
use serde_json::{Value, json};
use t3_server::{
    persistence::{Store, write_projection},
    project::ProjectService,
    thread::ThreadService,
};

fn create_project(store: &Store, id: &str, root: &std::path::Path) {
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":format!("create-{id}"),"projectId":id,"title":id,"workspaceRoot":root}), Utc::now())
        .unwrap();
}
fn create_thread(store: &Store, project: &str, id: &str) {
    let receipt = ThreadService::new(store.clone())
        .dispatch(&json!({"type":"thread.create","commandId":format!("create-{id}"),"threadId":id,"projectId":project,"title":id,"modelSelection":{"instanceId":"codex","model":"fixture"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}), Utc::now())
        .unwrap();
    assert_eq!(receipt.status, "accepted", "{receipt:?}");
}
fn command(store: &Store, command: Value) {
    let receipt = ThreadService::new(store.clone()).dispatch(&command, Utc::now()).unwrap();
    assert_eq!(receipt.status, "accepted", "{receipt:?}");
}
/// Adds a message to a thread's projection, as a provider turn would.
fn say(store: &Store, thread: &str, id: &str, role: &str, text: &str, minute: u32, streaming: bool) {
    store
        .transaction(|transaction| {
            let mut projection = t3_server::persistence::read_projection(transaction, "thread", thread)?.unwrap();
            let at = format!("2026-09-27T00:{minute:02}:00.000Z");
            projection["messages"].as_array_mut().unwrap().push(json!({"id":id,"threadId":thread,"runId":null,"nodeId":null,"role":role,"text":text,"attachments":[],"streaming":streaming,"createdAt":at,"updatedAt":at}));
            write_projection(transaction, "thread", thread, &projection)
        })
        .unwrap();
}
fn found(exit: &Value) -> Vec<(String, String, String)> {
    success(exit)["matches"].as_array().unwrap().iter().map(|m| (m["threadId"].as_str().unwrap().into(), m["source"].as_str().unwrap().into(), m["snippet"].as_str().unwrap().into())).collect()
}

#[tokio::test]
async fn search_finds_one_finished_message_per_visible_thread_and_refuses_what_the_original_refuses() {
    let h = harness().await;
    let root = h.base.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    create_project(&h.store, "project", &root);
    for thread in ["both", "assistant", "archived", "other"] {
        create_thread(&h.store, "project", thread);
    }
    say(&h.store, "both", "both-assistant", "assistant", "needle from the answer", 3, false);
    say(&h.store, "both", "both-user-old", "user", "older needle question", 1, false);
    say(&h.store, "both", "both-user-new", "user", "newer needle question", 2, false);
    say(&h.store, "assistant", "assistant-only", "assistant", "needle in an answer", 1, false);
    say(&h.store, "assistant", "typing", "user", "needle still typing", 1, true);
    say(&h.store, "assistant", "system", "system", "needle system prompt", 1, false);
    say(&h.store, "archived", "archived", "user", "needle archived", 1, false);
    say(&h.store, "other", "other", "user", "unrelated text", 1, false);
    command(&h.store, json!({"type":"thread.archive","commandId":"archive","threadId":"archived"}));

    let mut reader = h.connect(&h.reader).await;
    let hits = rpc(&mut reader, 1, "orchestration.searchThreads", json!({"query":"  NEEDLE  ","limit":20})).await;
    assert_eq!(
        found(&hits),
        vec![
            ("both".into(), "user".into(), "newer needle question".into()),
            ("assistant".into(), "assistant".into(), "needle in an answer".into()),
        ]
    );
    // The deleted thread is hidden once the delete lands (it can only be issued while empty).
    create_thread(&h.store, "project", "removed");
    command(&h.store, json!({"type":"thread.delete","commandId":"delete","threadId":"removed"}));
    say(&h.store, "removed", "removed-message", "user", "needle removed", 1, false);
    let hits = found(&rpc(&mut reader, 2, "orchestration.searchThreads", json!({"query":"needle"})).await);
    assert_eq!(hits.iter().map(|hit| hit.0.as_str()).collect::<Vec<_>>(), ["both", "assistant"]);

    // Each match carries its project and the message time.
    let exit = rpc(&mut reader, 3, "orchestration.searchThreads", json!({"query":"newer","limit":1})).await;
    assert_eq!(success(&exit)["matches"], json!([{"threadId":"both","projectId":"project","source":"user","snippet":"newer needle question","messageCreatedAt":"2026-09-27T00:02:00.000Z"}]));
    // LIKE wildcards match literally, and a miss is an empty list.
    for query in ["ne%le", "no such text"] {
        assert_eq!(success(&rpc(&mut reader, 4, "orchestration.searchThreads", json!({"query":query})).await), &json!({"matches":[]}));
    }
    // Queries and limits outside the contract die as schema failures.
    for (id, payload) in [(10, json!({"query":"a"})), (11, json!({"query":"   "})), (12, json!({"query":"x".repeat(201)})), (13, json!({"query":"ab","limit":0})), (14, json!({"query":"ab","limit":51})), (15, json!({})), (16, Value::Null)] {
        defect(&rpc(&mut reader, id, "orchestration.searchThreads", payload).await);
    }
    h.finish().await;
}

#[tokio::test]
async fn a_socket_without_orchestration_read_is_refused_and_an_unreadable_match_is_a_search_error() {
    use t3_contracts::AuthEnvironmentScope;
    let h = harness().await;
    // A session holding only settings:write cannot search.
    let auth = t3_server::auth::AuthService::new(h.store.clone(), [7; 32], "t3_session_rpc".into(), "loopback-browser".into()).unwrap();
    let token = auth.issue_session("limited", "bearer-access-token", vec![AuthEnvironmentScope::SettingsWrite], json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let mut limited = h.connect(&token).await;
    let exit = rpc(&mut limited, 1, "orchestration.searchThreads", json!({"query":"needle"})).await;
    let error = failure(&exit);
    assert_eq!(error["_tag"], "EnvironmentAuthorizationError", "{exit}");
    assert_eq!(error["operation"], "orchestration.searchThreads");
    assert_eq!(error["requiredPermission"], "orchestration:read");

    // A message whose text is not a string cannot be decoded as a match.
    let root = h.base.join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    create_project(&h.store, "project", &root);
    create_thread(&h.store, "project", "corrupt");
    say(&h.store, "corrupt", "corrupt", "user", "placeholder", 1, false);
    h.store
        .transaction(|transaction| {
            let mut projection = t3_server::persistence::read_projection(transaction, "thread", "corrupt")?.unwrap();
            projection["messages"][0]["text"] = json!(20260927);
            write_projection(transaction, "thread", "corrupt", &projection)
        })
        .unwrap();
    let mut reader = h.connect(&h.reader).await;
    let exit = rpc(&mut reader, 2, "orchestration.searchThreads", json!({"query":"0260927"})).await;
    let error = failure(&exit);
    assert_eq!(error["_tag"], "OrchestrationSearchThreadsError");
    assert_eq!(error["message"], "Failed to search threads");
    assert_eq!(error["cause"]["message"], "Thread search decode failed.");
    h.finish().await;
}
