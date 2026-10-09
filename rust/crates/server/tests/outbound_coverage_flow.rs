//! Replies the rest of the suite never reads, driven over a real socket so that the transport's reply check (`rpc_response.rs`, on
//! for every test) validates them against the original's schemas: a thread's projection, its stream, the archived shell and a
//! method nothing else reaches. Nothing outside the temporary directory is touched.
mod rpc_support;
use chrono::Utc;
use rpc_support::{Socket, failure, harness, next, rpc, send, success};
use serde_json::{Value, json};
use t3_server::{project::ProjectService, thread::ThreadService};

/// One chunk of a stream, acknowledged, then the stream is ended by the client.
async fn first_chunk(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    send(socket, json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})).await;
    let chunk = loop {
        let frame = next(socket).await;
        if frame["_tag"] == "Chunk" && frame["requestId"] == id {
            break frame;
        }
        assert!(!(frame["_tag"] == "Exit" && frame["requestId"] == id), "{tag} ended without a chunk: {frame}");
    };
    send(socket, json!({"_tag":"Ack","requestId":id})).await;
    send(socket, json!({"_tag":"Interrupt","requestId":id})).await;
    chunk
}

#[tokio::test]
async fn a_threads_documents_and_streams_are_checked_against_their_schemas() {
    let harness = harness().await;
    let now = Utc::now();
    let workspace = harness.base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(harness.store.clone())
        .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), now)
        .unwrap();
    let receipt = ThreadService::new(harness.store.clone())
        .dispatch(&json!({"type":"thread.create","commandId":"create","threadId":"thread","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture"},"runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null}), now)
        .unwrap();
    assert_eq!(receipt.status, "accepted");
    let command = json!({"type":"message.dispatch","commandId":"prepare","threadId":"thread","messageId":"message","text":"Prepared turn","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"defer_start","workspaceStrategy":{"type":"root"}}});
    harness
        .store
        .dispatch(
            "prepare",
            "thread",
            "thread",
            "message.dispatch",
            now,
            |tx| t3_server::execution::plan_message(&command, &t3_server::persistence::read_projection(tx, "thread", "thread")?.unwrap(), now),
            t3_server::thread::reduce,
        )
        .unwrap();

    let mut socket = harness.connect(&harness.operator).await;
    let projection = rpc(&mut socket, 1, "orchestration.getThreadProjection", json!({"threadId":"thread"})).await;
    let items = success(&projection)["turnItems"].clone();
    assert!(items.as_array().is_some_and(|items| !items.is_empty()), "{projection}");
    // The check is not vacuous on a document this large: the same projection with a field gone, or with a status the schema lacks, is refused.
    let exit_of = |value: Value| t3_contracts::RpcServerMessage::Exit { request_id: "1".into(), exit: serde_json::from_value(json!({"_tag":"Success","value":value})).unwrap() };
    let projection_value = success(&projection).clone();
    assert_eq!(t3_server::rpc_response::violations("orchestration.getThreadProjection", &exit_of(projection_value.clone())), vec![]);
    let mut without = projection_value.clone();
    without.as_object_mut().unwrap().remove("updatedAt");
    assert_eq!(t3_server::rpc_response::violations("orchestration.getThreadProjection", &exit_of(without))[0].message, "Missing key");
    let mut spoiled = projection_value;
    spoiled["thread"]["runtimeMode"] = json!("sideways");
    assert!(!t3_server::rpc_response::violations("orchestration.getThreadProjection", &exit_of(spoiled)).is_empty());
    let missing = rpc(&mut socket, 2, "orchestration.getThreadProjection", json!({"threadId":"nobody"})).await;
    assert_eq!(failure(&missing)["_tag"], "OrchestrationV2GetThreadProjectionError");
    let item_id = items[0]["id"].as_str().unwrap().to_owned();
    let item = rpc(&mut socket, 3, "orchestration.getTurnItem", json!({"threadId":"thread","itemId":item_id})).await;
    assert!(success(&item).get("item").is_some(), "{item}");
    let absent = rpc(&mut socket, 4, "orchestration.getTurnItem", json!({"threadId":"thread","itemId":"no-such-item"})).await;
    assert_eq!(success(&absent)["item"], Value::Null);

    let snapshot = first_chunk(&mut socket, 5, "orchestration.subscribeThread", json!({"threadId":"thread"})).await;
    assert_eq!(snapshot["values"][0]["kind"], "snapshot");
    let bounded = first_chunk(&mut socket, 6, "orchestration.subscribeThread", json!({"threadId":"thread","acceptBoundedSnapshot":true})).await;
    assert_eq!(bounded["values"][0]["kind"], "snapshot");
    let archived = first_chunk(&mut socket, 7, "orchestration.subscribeArchivedShell", json!({})).await;
    assert_eq!(archived["values"][0]["kind"], "snapshot");

    // Methods no other flow reaches. The operator's session may not hold their scope, and a refusal is a reply the check reads too.
    let relay = rpc(&mut socket, 8, "cloud.getRelayClientStatus", json!({})).await;
    assert!(relay["_tag"] == "Success" || failure(&relay)["_tag"] == "EnvironmentAuthorizationError", "{relay}");
    drop(socket);
    harness.finish().await;
}
