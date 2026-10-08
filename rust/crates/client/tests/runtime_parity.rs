use serde_json::{Value, json};
use t3_client::{
    connection::EnvironmentEndpoint,
    rpc::{RequestKind, RpcEvent, RpcSession},
    shell::ShellState,
    thread::ThreadState,
};
use t3_contracts::{ShellSnapshot, ShellStreamItem};

fn project(identity: Value) -> Value {
    json!({"id":"p","title":"Project","workspaceRoot":"/repo","repositoryIdentity":identity,"defaultModelSelection":null,"scripts":[],"createdAt":"2026-10-07T00:00:00Z","updatedAt":"2026-10-07T00:00:00Z"})
}
fn shell(sequence: u64, identity: Value) -> ShellSnapshot {
    serde_json::from_value(json!({"schemaVersion":2,"snapshotSequence":sequence,"projects":[project(identity)],"threads":[],"archivedThreads":[]})).unwrap()
}
fn projection() -> Value {
    json!({"thread":{"id":"t"},"updatedAt":"old","runs":[],"attempts":[],"messages":[],"turnItems":[],"visibleTurnItems":[],"providerTurns":[]})
}
fn event(sequence: u64, kind: &str, payload: Value) -> Value {
    json!({"kind":"event","sequence":sequence,"event":{"type":kind,"threadId":"t","occurredAt":"new","payload":payload}})
}
fn loaded() -> ThreadState {
    let mut state = ThreadState::default();
    state
        .apply(&json!({"kind":"snapshot","snapshotSequence":5,"projection":projection()}))
        .unwrap();
    state
}

#[test]
fn remote_endpoints_preserve_proxy_and_negotiate_protocol() {
    let endpoint = EnvironmentEndpoint::new("https://server.test/t3/?unrelated=true").unwrap();
    assert_eq!(
        endpoint.http("/api/auth/websocket-ticket").as_str(),
        "https://server.test/t3/api/auth/websocket-ticket"
    );
    let url = endpoint.socket(Some("a+b secret"), "mobile");
    assert_eq!(url.scheme(), "wss");
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "wsTicket" && value == "a+b secret")
    );
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "orchestrationProtocol" && value == "2")
    );
    assert!(EnvironmentEndpoint::new("https://user:secret@server.test").is_err());
}

#[test]
fn stream_acknowledges_late_chunks_after_cancel_without_emitting_values() {
    let mut rpc = RpcSession::default();
    let (id, request) = rpc.request(
        "orchestration.subscribeShell",
        json!({}),
        RequestKind::Stream,
    );
    assert_eq!(request["headers"], json!([]));
    assert_eq!(rpc.cancel(&id).unwrap()["_tag"], "Interrupt");
    let (events, outgoing) = rpc
        .receive(
            &json!({"_tag":"Chunk","requestId":id,"values":[{"kind":"synchronized"}]}).to_string(),
        )
        .unwrap();
    assert!(events.is_empty());
    assert_eq!(outgoing, vec![json!({"_tag":"Ack","requestId":id})]);
}

#[test]
fn rpc_batches_correlate_stream_and_unary_exits() {
    let mut rpc = RpcSession::default();
    let (stream, _) = rpc.request("stream", json!({}), RequestKind::Stream);
    let (unary, _) = rpc.request("unary", json!({}), RequestKind::Unary);
    let (events, ack) = rpc
        .receive(
            &json!([
                {"_tag":"Chunk","requestId":stream,"values":[1,2]},
                {"_tag":"Exit","requestId":unary,"exit":{"_tag":"Success","value":{"sequence":9}}},
                {"_tag":"Pong"}
            ])
            .to_string(),
        )
        .unwrap();
    assert!(
        matches!(&events[0], RpcEvent::Values { method, values, .. } if method == "stream" && values.len() == 2)
    );
    assert!(
        matches!(&events[1], RpcEvent::Complete { method, value, .. } if method == "unary" && value["sequence"] == 9)
    );
    assert_eq!(ack.len(), 1);
    assert_eq!(rpc.disconnect().len(), 1);
    assert!(rpc.disconnect().is_empty());
}

#[test]
fn authoritative_shell_snapshot_can_reset_cursor_but_retains_resolved_identity() {
    let mut state = ShellState::default();
    state.apply(ShellStreamItem::Snapshot {
        snapshot: shell(100, json!({"remote":"origin"})),
        resolved_repository_identity_roots: None,
    });
    state.apply(ShellStreamItem::Snapshot {
        snapshot: shell(2, Value::Null),
        resolved_repository_identity_roots: None,
    });
    let snapshot = state.snapshot.unwrap();
    assert_eq!(snapshot.snapshot_sequence, 2);
    assert_eq!(
        snapshot.projects[0].extra["repositoryIdentity"],
        json!({"remote":"origin"})
    );
}

#[test]
fn enrichment_preserves_structure_and_sequence_and_accepts_resolved_null() {
    let mut state = ShellState::default();
    state.apply(ShellStreamItem::Snapshot {
        snapshot: shell(100, json!({"remote":"origin"})),
        resolved_repository_identity_roots: None,
    });
    let mut refresh = shell(2, Value::Null);
    refresh.projects[0].title = "Different".parse().unwrap();
    state.apply(ShellStreamItem::Snapshot {
        snapshot: refresh,
        resolved_repository_identity_roots: Some(vec!["/repo".into()]),
    });
    let snapshot = state.snapshot.unwrap();
    assert_eq!(snapshot.snapshot_sequence, 100);
    assert_eq!(snapshot.projects[0].title.as_str(), "Project");
    assert!(snapshot.projects[0].extra["repositoryIdentity"].is_null());
}

#[test]
fn stale_shell_deltas_cannot_recreate_removed_projects() {
    let mut state = ShellState::default();
    state.apply(ShellStreamItem::Snapshot {
        snapshot: shell(5, Value::Null),
        resolved_repository_identity_roots: None,
    });
    state.apply(ShellStreamItem::ProjectRemoved {
        sequence: 6,
        project_id: "p".parse().unwrap(),
    });
    assert!(!state.apply(ShellStreamItem::ProjectUpdated {
        sequence: 5,
        project: serde_json::from_value(project(Value::Null)).unwrap()
    }));
    assert!(state.snapshot.unwrap().projects.is_empty());
}

#[test]
fn unknown_thread_events_advance_resume_cursor_and_stale_events_are_ignored() {
    let mut state = loaded();
    assert!(!state.apply(&event(6, "future.event", json!({}))).unwrap());
    assert_eq!(state.sequence, 6);
    assert!(
        !state
            .apply(&event(
                5,
                "message.updated",
                json!({"id":"m","text":"stale"})
            ))
            .unwrap()
    );
    assert!(
        state.projection.unwrap()["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn partial_history_does_not_resurrect_old_rows_but_new_live_turns_are_visible() {
    let mut state = loaded();
    state.has_more_history = true;
    state.latest_local_turn_ordinal = Some(50);
    let item = |id, ordinal| json!({"id":id,"threadId":"t","runId":"r","nodeId":"n","type":"assistant_message","ordinal":ordinal});
    assert!(
        !state
            .apply(&event(6, "turn-item.updated", item("old", 10)))
            .unwrap()
    );
    assert!(
        state
            .apply(&event(7, "turn-item.updated", item("new", 51)))
            .unwrap()
    );
    assert_eq!(
        state.projection.as_ref().unwrap()["visibleTurnItems"][0]["sourceItemId"],
        "new"
    );
    state.merge_history(&json!({"items":[{"sourceItemId":"new","sourceThreadId":"t","position":0,"item":{"text":"stale"}},{"sourceItemId":"old","sourceThreadId":"t","position":0}],"nextCursor":null,"hasMoreHistory":false})).unwrap();
    let rows = &state.projection.unwrap()["visibleTurnItems"];
    assert_eq!(rows[0]["sourceItemId"], "old");
    assert_eq!(rows[1]["item"]["ordinal"], 51);
    assert_eq!(rows[1]["position"], 1);
}

#[test]
fn rolled_back_and_never_sent_queued_turns_leave_transcript() {
    let mut state = loaded();
    for (seq, id, ordinal, intent) in [
        (6, "normal", 1, "new_turn"),
        (7, "queued", 2, "queued_turn"),
    ] {
        state.apply(&event(seq,"turn-item.updated",json!({"id":id,"ordinal":ordinal,"threadId":"t","runId":"r","nodeId":"n","type":"user_message","inputIntent":intent}))).unwrap();
    }
    state
        .apply(&event(
            8,
            "run.updated",
            json!({"id":"r","status":"cancelled"}),
        ))
        .unwrap();
    assert_eq!(
        state.projection.as_ref().unwrap()["visibleTurnItems"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    state
        .apply(&event(
            9,
            "run.updated",
            json!({"id":"r","status":"rolled_back"}),
        ))
        .unwrap();
    assert!(
        state.projection.unwrap()["visibleTurnItems"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn read_markers_do_not_bump_activity_and_provider_usage_survives_sparse_updates() {
    let mut state = loaded();
    state
        .apply(&event(
            6,
            "thread.visited",
            json!({"id":"t","lastVisitedAt":"visited"}),
        ))
        .unwrap();
    assert_eq!(state.projection.as_ref().unwrap()["updatedAt"], "old");
    state
        .apply(&event(
            7,
            "provider-turn.updated",
            json!({"id":"pt","tokenUsage":{"total":10}}),
        ))
        .unwrap();
    state
        .apply(&event(
            8,
            "provider-turn.updated",
            json!({"id":"pt","status":"completed"}),
        ))
        .unwrap();
    assert_eq!(
        state.projection.unwrap()["providerTurns"][0]["tokenUsage"]["total"],
        10
    );
}
