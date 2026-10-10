//! `subscribeServerLifecycle` over a real socket: a client that connects after startup is given the
//! latest `welcome` and `ready` in order, then every event published after, each numbered.
mod rpc_support;
use rpc_support::*;
use serde_json::{Value, json};

/// The next stream value, acknowledging its chunk.
async fn next_event(socket: &mut Socket, id: u64) -> Value {
    loop {
        let message = next(socket).await;
        assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
        if message["_tag"] == "Chunk" {
            send(socket, json!({"_tag":"Ack","requestId":id})).await;
            return message["values"].clone();
        }
    }
}

#[tokio::test]
async fn a_late_subscriber_sees_welcome_then_ready_and_then_what_is_published_next() {
    let h = harness().await;
    t3_server::server_lifecycle::publish(&h.store, "welcome", json!({"environment":{"environmentId":"e","label":"L","platform":{"os":"linux","arch":"x64"},"serverVersion":"1","orchestrationProtocolVersion":2,"capabilities":{}},"cwd":"/work","projectName":"work"}));
    t3_server::server_lifecycle::publish(&h.store, "ready", json!({"at":"2026-01-01T00:00:00.000Z","environment":{"environmentId":"e","label":"L","platform":{"os":"linux","arch":"x64"},"serverVersion":"1","orchestrationProtocolVersion":2,"capabilities":{}}}));
    let mut socket = h.connect(&h.reader).await;
    send(&mut socket, json!({"_tag":"Request","id":1,"tag":"subscribeServerLifecycle","payload":{},"headers":[]})).await;
    let first = next_event(&mut socket, 1).await;
    assert_eq!(first.as_array().unwrap().iter().map(|event| (event["type"].as_str().unwrap(), event["sequence"].as_u64().unwrap())).collect::<Vec<_>>(), [("welcome", 1), ("ready", 2)]);
    assert_eq!(first[0]["payload"]["projectName"], "work");
    t3_server::server_lifecycle::publish(&h.store, "legacyThreadMigration", json!({"status":"complete","totalThreadCount":3}));
    let live = next_event(&mut socket, 1).await;
    assert_eq!((live[0]["type"].as_str(), live[0]["sequence"].as_u64(), live[0]["payload"]["totalThreadCount"].as_u64()), (Some("legacyThreadMigration"), Some(3), Some(3)));
    h.finish().await;
}
