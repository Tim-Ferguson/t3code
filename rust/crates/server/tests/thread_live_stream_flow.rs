//! The live end of `orchestration.subscribeThread` keeps the newest running update of each tool call within the window and
//! nothing else (`ThreadLiveEventCoalescer.ts`), over a real socket. Updates are committed together, with no wait between them, so
//! they all reach the stream before its window can close; the test then waits for the frames, never for a time.
mod rpc_support;
use chrono::Utc;
use rpc_support::{harness, next, send};
use serde_json::{Value, json};
use t3_server::{
    persistence::{Decision, Event, Store},
    test_support::HANG_BOUND,
};

const AT: &str = "2026-01-01T00:00:00.000Z";

fn event(kind: &str, payload: Value) -> Event {
    Event { event_id: uuid::Uuid::new_v4().to_string(), aggregate_kind: "thread".into(), aggregate_id: "thread-1".into(), occurred_at: AT.into(), command_id: None, causation_event_id: None, correlation_id: None, event_type: kind.into(), payload, metadata: json!({}) }
}

fn thread_record() -> Value {
    json!({"createdBy":"user","creationSource":"web","id":"thread-1","projectId":"project","title":"Streaming","providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"gpt-5.4"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null,"activeProviderThreadId":"pt","lineage":{"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"thread-1"},"forkedFrom":null,"createdAt":AT,"updatedAt":AT,"archivedAt":null,"settledOverride":null,"settledAt":null,"snoozedUntil":null,"snoozedAt":null,"lastVisitedAt":null,"deletedAt":null})
}

/// A command execution at `ordinal` with `output`, still running or done.
fn call(item: &str, ordinal: u64, output: &str, status: &str) -> Value {
    let done = status != "running";
    json!({"id":item,"threadId":"thread-1","runId":"run:1","nodeId":format!("node:{item}"),"providerThreadId":"pt","providerTurnId":"turn:1","parentItemId":null,"ordinal":ordinal,"type":"command_execution","status":status,"title":null,"updatedAt":AT,"startedAt":AT,"completedAt":if done { json!(AT) } else { Value::Null },"nativeItemRef":{"driver":"codex","nativeId":item,"strength":"strong"},"input":"cargo test","output":output,"exitCode":if done { json!(0) } else { Value::Null }})
}

/// Commits one event of the thread, as a provider adapter does.
fn commit(store: &Store, id: &str, kind: &str, payload: Value) {
    store.dispatch(id, "thread", "thread-1", "test.event", Utc::now(), |_| Ok(Decision::Accepted { events: vec![event(kind, payload)], effects: vec![] }), t3_server::thread::reduce).unwrap();
}

/// The `event` frames of the stream until one carries `until` as its sequence, as `(sequence, item id)`.
async fn frames_until(socket: &mut rpc_support::Socket, until: u64) -> Vec<(u64, String)> {
    let mut frames = vec![];
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let message = next(socket).await;
            assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
            if message["_tag"] != "Chunk" {
                continue;
            }
            send(socket, json!({"_tag":"Ack","requestId":1})).await;
            for value in message["values"].as_array().unwrap() {
                if value["kind"] == "event" {
                    let sequence = value["sequence"].as_u64().unwrap();
                    frames.push((sequence, value["event"]["payload"]["id"].as_str().unwrap_or_default().to_owned()));
                    if sequence == until {
                        return;
                    }
                }
            }
        }
    })
    .await
    .expect("the awaited frame arrived");
    frames
}

#[tokio::test]
async fn a_thread_stream_sends_the_newest_running_update_of_each_call() {
    let h = harness().await;
    commit(&h.store, "created", "thread.created", thread_record());
    let mut socket = h.connect(&h.operator).await;
    send(&mut socket, json!({"_tag":"Request","id":1,"tag":"orchestration.subscribeThread","payload":{"threadId":"thread-1"},"headers":[]})).await;
    // The snapshot first; the thread is as committed so far.
    loop {
        let message = next(&mut socket).await;
        if message["_tag"] == "Chunk" {
            send(&mut socket, json!({"_tag":"Ack","requestId":1})).await;
            assert_eq!(message["values"][0]["kind"], "snapshot");
            break;
        }
    }
    let first = h.store.latest_sequence().unwrap();

    // Two parallel calls report progress, three times each; then the first finishes. Everything is committed before the stream
    // task runs, so its window holds all six updates when the finish arrives and closes it.
    for round in 1..=3u64 {
        commit(&h.store, &format!("a{round}"), "turn-item.updated", call("call-a", 1, &format!("a{round}"), "running"));
        commit(&h.store, &format!("b{round}"), "turn-item.updated", call("call-b", 2, &format!("b{round}"), "running"));
    }
    commit(&h.store, "done", "turn-item.updated", call("call-a", 1, "a-final", "completed"));
    let last = first + 7;
    let frames = frames_until(&mut socket, last).await;
    assert_eq!(frames, vec![(first + 5, "call-a".to_owned()), (first + 6, "call-b".to_owned()), (first + 7, "call-a".to_owned())]);

    // An update alone waits out the window and is then sent (it is the only way it can arrive), still just the newest of the burst.
    for round in 1..=3u64 {
        commit(&h.store, &format!("c{round}"), "turn-item.updated", call("call-c", 3, &format!("c{round}"), "running"));
    }
    let frames = frames_until(&mut socket, last + 3).await;
    assert_eq!(frames, vec![(last + 3, "call-c".to_owned())]);
    h.finish().await;
}
