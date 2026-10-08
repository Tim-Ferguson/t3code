//! Behavioral cases ported from original client/protocol tests. The fake Peer
//! controls wire arrival and write acknowledgement independently of requests.
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use t3_acp::{
    AcpError, AgentMethod, Client, ClientEvent, Generation, Peer, PeerEvent, RequestId, RpcError,
};
use tokio::sync::{broadcast, mpsc, oneshot};

struct Call {
    method: String,
    params: Value,
    reply: Option<oneshot::Sender<Result<Value, AcpError>>>,
}
struct Response {
    id: RequestId,
    result: Result<Value, RpcError>,
}
struct FakePeer {
    events: broadcast::Sender<PeerEvent>,
    calls: mpsc::UnboundedSender<Call>,
    responses: mpsc::UnboundedSender<Response>,
    response_gate: Mutex<Option<oneshot::Receiver<Result<(), AcpError>>>>,
}
impl Peer for FakePeer {
    fn subscribe(&self) -> broadcast::Receiver<PeerEvent> {
        self.events.subscribe()
    }
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        _: Duration,
    ) -> BoxFuture<'a, Result<Value, AcpError>> {
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            self.calls
                .send(Call {
                    method: method.into(),
                    params,
                    reply: Some(sender),
                })
                .unwrap();
            receiver.await.map_err(|_| AcpError::Closed)?
        })
    }
    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            self.calls
                .send(Call {
                    method: method.into(),
                    params,
                    reply: None,
                })
                .unwrap();
            Ok(())
        })
    }
    fn respond<'a>(
        &'a self,
        id: RequestId,
        result: Result<Value, RpcError>,
    ) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            let gate = self.response_gate.lock().unwrap().take();
            self.responses.send(Response { id, result }).unwrap();
            if let Some(gate) = gate {
                gate.await.map_err(|_| AcpError::Closed)?
            } else {
                Ok(())
            }
        })
    }
}
struct Harness {
    peer: Arc<FakePeer>,
    client: Client,
    calls: mpsc::UnboundedReceiver<Call>,
    responses: mpsc::UnboundedReceiver<Response>,
}
impl Harness {
    fn new() -> Self {
        let (events, _) = broadcast::channel(1024);
        let (calls, call_rx) = mpsc::unbounded_channel();
        let (responses, response_rx) = mpsc::unbounded_channel();
        let peer = Arc::new(FakePeer {
            events,
            calls,
            responses,
            response_gate: Mutex::new(None),
        });
        let client = Client::new(peer.clone(), Duration::from_secs(10));
        Self {
            peer,
            client,
            calls: call_rx,
            responses: response_rx,
        }
    }
    async fn initialize(&mut self, generation: Generation, version: u16) {
        let client = self.client.clone();
        let result = tokio::spawn(async move {
            client.initialize(json!({"protocolVersion":1,"clientCapabilities":{"terminal":true,"fs":{"readTextFile":true}},"clientInfo":{"name":"test","version":"1"}})).await
        });
        let call = self.call().await;
        assert_eq!(call.method, "initialize");
        assert_eq!(call.params["protocolVersion"], 2);
        assert_eq!(call.params["clientCapabilities"]["terminal"], true);
        assert_eq!(call.params["info"]["name"], "test");
        call.reply.unwrap().send(Ok(match generation{Generation::V1=>json!({"protocolVersion":version,"agentInfo":{"name":"legacy","version":"1"},"agentCapabilities":{"loadSession":true}}),Generation::V2=>json!({"protocolVersion":version,"info":{"name":"modern","version":"1"},"capabilities":{"session":{}}})})).unwrap();
        result.await.unwrap().unwrap();
        assert_eq!(self.client.generation(), Some(generation));
    }
    async fn call(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(2), self.calls.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn response(&mut self) -> Response {
        tokio::time::timeout(Duration::from_secs(2), self.responses.recv())
            .await
            .unwrap()
            .unwrap()
    }
    fn event(&self, event: PeerEvent) {
        self.peer.events.send(event).unwrap();
    }
    fn update(&self, mut update: Value) {
        if matches!(
            update["sessionUpdate"].as_str(),
            Some("agent_message_chunk" | "user_message_chunk" | "agent_thought_chunk")
        ) {
            update["messageId"] = json!("message");
        }
        self.event(PeerEvent::Notification {
            method: "session/update".into(),
            params: json!({"sessionId":"s","update":update}),
        });
    }
}
fn prompt() -> Value {
    json!({"sessionId":"s","prompt":[{"type":"text","text":"hello"}]})
}
fn permission() -> Value {
    json!({"sessionId":"s","title":"Allow?","options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}],"subject":{"type":"command","toolCallId":"tool","command":"echo","cwd":"/workspace"}})
}
async fn notification(events: &mut broadcast::Receiver<ClientEvent>) -> t3_acp::Notification {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ClientEvent::Notification(value) = events.recv().await.unwrap() {
                return value;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn v1_response_shape_overrides_version_number_and_preserves_legacy_methods() {
    let mut h = Harness::new();
    h.initialize(Generation::V1, 2).await;
    let client = h.client.clone();
    let auth = tokio::spawn(async move {
        client
            .call(AgentMethod::Authenticate, json!({"methodId":"login"}))
            .await
    });
    let call = h.call().await;
    assert_eq!(call.method, "authenticate");
    call.reply.unwrap().send(Ok(json!({}))).unwrap();
    auth.await.unwrap().unwrap();
    let client = h.client.clone();
    let prompt = tokio::spawn(async move { client.prompt(prompt()).await });
    h.call()
        .await
        .reply
        .unwrap()
        .send(Ok(json!({"stopReason":"end_turn"})))
        .unwrap();
    assert_eq!(prompt.await.unwrap().unwrap()["stopReason"], "end_turn");
    assert!(matches!(
        h.client
            .call(AgentMethod::DeleteSession, json!({"sessionId":"s"}))
            .await,
        Err(AcpError::Request(RpcError { code: -32601, .. }))
    ));
}
#[tokio::test]
async fn v2_prompt_handles_parallel_exact_id_permission_callbacks_before_idle_completion() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let observed = contexts.clone();
    h.client.handle_request(
        "session/request_permission",
        Arc::new(move |params, context| {
            let observed = observed.clone();
            Box::pin(async move {
                assert_eq!(params["toolCall"]["toolCallId"], "tool");
                observed.lock().unwrap().push(context.request_id);
                Ok(json!({"outcome":{"outcome":"selected","optionId":"allow"}}))
            })
        }),
    );
    let client = h.client.clone();
    let pending = tokio::spawn(async move { client.prompt(prompt()).await });
    let call = h.call().await;
    for id in [RequestId::Number(0.into()), RequestId::String("0".into())] {
        h.event(PeerEvent::Request {
            id,
            method: "session/request_permission".into(),
            params: permission(),
        });
    }
    let first = h.response().await;
    let second = h.response().await;
    assert_ne!(first.id, second.id);
    assert_eq!(first.result.unwrap()["outcome"]["optionId"], "allow");
    assert_eq!(second.result.unwrap()["outcome"]["optionId"], "allow");
    call.reply.unwrap().send(Ok(json!({}))).unwrap();
    assert!(!pending.is_finished());
    h.update(json!({"sessionUpdate":"state_update","state":"idle","stopReason":"cancelled","_meta":{"complete":true}}));
    let output = pending.await.unwrap().unwrap();
    assert_eq!(output["stopReason"], "cancelled");
    assert_eq!(output["_meta"]["complete"], true);
    let mut ids = contexts.lock().unwrap().clone();
    ids.sort();
    assert_eq!(ids, ["$t3:jsonrpc:number:0", "0"]);
}
#[tokio::test]
async fn v2_early_idle_waits_for_rpc_ack_and_canceled_prompt_registration_is_removed() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let client = h.client.clone();
    let first = tokio::spawn(async move { client.prompt(prompt()).await });
    let call = h.call().await;
    assert!(matches!(
        h.client.prompt(prompt()).await,
        Err(AcpError::Request(RpcError { code: -32603, .. }))
    ));
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    drop(call);
    let client = h.client.clone();
    let second = tokio::spawn(async move { client.prompt(prompt()).await });
    let call = h.call().await;
    let mut events = h.client.subscribe();
    h.update(json!({"sessionUpdate":"state_update","state":"idle"}));
    notification(&mut events).await;
    assert!(!second.is_finished());
    call.reply.unwrap().send(Ok(json!({}))).unwrap();
    assert_eq!(
        second.await.unwrap().unwrap(),
        json!({"stopReason":"end_turn"})
    );
    h.client.cancel(json!({"sessionId":"s"})).await.unwrap();
    let cancel = h.call().await;
    assert_eq!(cancel.method, "session/cancel");
    assert!(cancel.reply.is_none());
}
#[tokio::test]
async fn buffered_updates_preserve_order_future_payloads_and_isolate_failing_handlers() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let mut events = h.client.subscribe();
    for text in ["one", "two"] {
        h.update(
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}),
        );
    }
    h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"future_content","payload":{"nested":[1,2]}}}));
    for _ in 0..3 {
        notification(&mut events).await;
    }
    let values = Arc::new(Mutex::new(Vec::new()));
    let observed = values.clone();
    h.client
        .handle_notification(
            "session/update",
            Arc::new(move |value| {
                let observed = observed.clone();
                Box::pin(async move {
                    observed.lock().unwrap().push(value);
                    Err(AcpError::Transport("handler failed".into()))
                })
            }),
        )
        .await;
    assert_eq!(
        values.lock().unwrap()[0]["update"]["content"]["text"],
        "one"
    );
    assert_eq!(
        values.lock().unwrap()[1]["update"]["content"]["text"],
        "two"
    );
    assert_eq!(
        values.lock().unwrap()[2]["update"]["content"]["type"],
        "_t3_unknown"
    );
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    h.client
        .handle_notification(
            "session/update",
            Arc::new(move |_| {
                let observed = observed.clone();
                Box::pin(async move {
                    observed.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                })
            }),
        )
        .await;
    h.update(json!({"sessionUpdate":"future_update","custom":{"unchanged":true}}));
    notification(&mut events).await;
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert_eq!(
        values.lock().unwrap()[3]["update"]["raw"]["custom"]["unchanged"],
        true
    );
}
#[tokio::test]
async fn malformed_known_notification_terminates_pending_and_future_calls() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let client = h.client.clone();
    let pending = tokio::spawn(async move { client.prompt(prompt()).await });
    let call = h.call().await;
    call.reply.unwrap().send(Ok(json!({}))).unwrap();
    h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text"}}));
    assert!(matches!(pending.await.unwrap(), Err(AcpError::Schema(_))));
    assert!(matches!(
        h.client.raw_notify("x/test", json!({})).await,
        Err(AcpError::Schema(_))
    ));
}
#[tokio::test]
async fn extension_panics_reply_once_with_internal_error_and_reader_keeps_running() {
    let mut h = Harness::new();
    h.client.handle_request(
        "x/panic",
        Arc::new(|_, _| Box::pin(async { panic!("handler defect") })),
    );
    h.client.handle_unknown_request(Arc::new(|params, context| {
        Box::pin(async move { Ok(json!({"echo":params,"identity":context.request_id})) })
    }));
    h.event(PeerEvent::Request {
        id: RequestId::Number(7.into()),
        method: "x/panic".into(),
        params: json!({}),
    });
    assert_eq!(h.response().await.result.unwrap_err().code, -32603);
    h.event(PeerEvent::Request {
        id: RequestId::String("$t3:jsonrpc:number:7".into()),
        method: "x/echo".into(),
        params: json!({"keep":1}),
    });
    let response = h.response().await;
    assert_eq!(
        response.id,
        RequestId::String("$t3:jsonrpc:number:7".into())
    );
    assert_eq!(
        response.result.unwrap()["identity"],
        "$t3:jsonrpc:string:$t3:jsonrpc:number:7"
    );
    h.event(PeerEvent::Request {
        id: RequestId::Number(8.into()),
        method: "session/request_permission".into(),
        params: permission(),
    });
    assert_eq!(h.response().await.result.unwrap_err().code, -32601);
}
#[tokio::test]
async fn elicitation_requires_mode_fields_and_preserves_flat_and_legacy_response_shapes() {
    let mut h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    for method in ["elicitation/create", "session/elicitation"] {
        let calls = calls.clone();
        h.client.handle_request(method,Arc::new(move|_,_|{let calls=calls.clone();Box::pin(async move{calls.fetch_add(1,Ordering::Relaxed);Ok(json!({"action":"accept","content":{"approved":true},"_meta":{"reply":true}}))})}));
    }
    h.event(PeerEvent::Request {
        id: RequestId::Number(1.into()),
        method: "elicitation/create".into(),
        params: json!({"sessionId":"s","mode":"url","message":"sign in"}),
    });
    assert_eq!(h.response().await.result.unwrap_err().code, -32602);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    for (id, method) in [(2, "elicitation/create"), (3, "session/elicitation")] {
        h.event(PeerEvent::Request{id:RequestId::Number(id.into()),method:method.into(),params:json!({"sessionId":"s","mode":"form","message":"choose","requestedSchema":{"type":"object","properties":{}}})});
        let response = h.response().await.result.unwrap();
        if id == 2 {
            assert_eq!(response["action"], "accept")
        } else {
            assert_eq!(response["action"]["action"], "accept");
            assert!(response["action"].get("_meta").is_none());
        }
        assert_eq!(response["_meta"]["reply"], true);
    }
}
#[tokio::test]
async fn recent_raw_stream_is_bounded_while_late_handler_replays_all_core_updates() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let mut events = h.client.subscribe();
    for index in 0..64 {
        h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":index.to_string()}}));
        notification(&mut events).await;
    }
    assert_eq!(h.client.recent_notifications().len(), 32);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    h.client
        .handle_notification(
            "session/update",
            Arc::new(move |value| {
                let observed = observed.clone();
                Box::pin(async move {
                    observed.lock().unwrap().push(
                        value["update"]["content"]["text"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    );
                    Ok(())
                })
            }),
        )
        .await;
    assert_eq!(
        *seen.lock().unwrap(),
        (0..64).map(|i| i.to_string()).collect::<Vec<_>>()
    );
}
#[tokio::test]
async fn response_acknowledgement_follows_actual_write_and_failure_is_terminal() {
    let mut h = Harness::new();
    h.client.handle_request(
        "x/reply",
        Arc::new(|_, _| Box::pin(async { Ok(json!({"ok":true})) })),
    );
    let (sender, receiver) = oneshot::channel();
    *h.peer.response_gate.lock().unwrap() = Some(receiver);
    let mut events = h.client.subscribe();
    h.event(PeerEvent::Request {
        id: RequestId::String("write".into()),
        method: "x/reply".into(),
        params: json!({}),
    });
    h.response().await;
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    sender.send(Ok(())).unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),ClientEvent::ResponseAcknowledged{request_id} if request_id=="write")
    );
    let (sender, receiver) = oneshot::channel();
    *h.peer.response_gate.lock().unwrap() = Some(receiver);
    h.event(PeerEvent::Request {
        id: RequestId::String("fail".into()),
        method: "x/reply".into(),
        params: json!({}),
    });
    h.response().await;
    sender
        .send(Err(AcpError::Transport("write failed".into())))
        .unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),ClientEvent::ResponseFailed{request_id,..} if request_id=="fail")
    );
    assert!(matches!(
        events.recv().await.unwrap(),
        ClientEvent::Terminated(_)
    ));
    assert!(h.client.raw_request("x/later", json!({})).await.is_err());
}
#[tokio::test]
async fn process_exit_details_fail_waiting_prompt_and_are_retained_for_future_operations() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let client = h.client.clone();
    let pending = tokio::spawn(async move { client.prompt(prompt()).await });
    h.call().await.reply.unwrap().send(Ok(json!({}))).unwrap();
    h.event(PeerEvent::Closed(AcpError::ProcessExited {
        code: Some(17),
        pid: Some(123),
        message: "stderr tail".into(),
    }));
    assert!(matches!(
        pending.await.unwrap(),
        Err(AcpError::ProcessExited {
            code: Some(17),
            pid: Some(123),
            ..
        })
    ));
    assert!(matches!(
        h.client.raw_notify("x/later", json!({})).await,
        Err(AcpError::ProcessExited { code: Some(17), .. })
    ));
}
#[tokio::test]
async fn deferred_notification_handlers_keep_chunks_and_idle_ordered_while_requests_progress() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let client = h.client.clone();
    let prompt_result = tokio::spawn(async move { client.prompt(prompt()).await });
    h.call().await.reply.unwrap().send(Ok(json!({}))).unwrap();
    let (entered, mut milestones) = mpsc::unbounded_channel();
    let (release, held) = oneshot::channel();
    let held = Arc::new(Mutex::new(Some(held)));
    let values = Arc::new(Mutex::new(Vec::new()));
    let seen = values.clone();
    h.client
        .handle_notification(
            "session/update",
            Arc::new(move |value| {
                let held = held.clone();
                let seen = seen.clone();
                let entered = entered.clone();
                Box::pin(async move {
                    seen.lock()
                        .unwrap()
                        .push(value["update"]["content"]["text"].clone());
                    let barrier = held.lock().unwrap().take();
                    if let Some(barrier) = barrier {
                        entered.send(()).unwrap();
                        barrier.await.unwrap();
                    }
                    Ok(())
                })
            }),
        )
        .await;
    h.client.handle_request(
        "x/barrier",
        Arc::new(|_, _| Box::pin(async { Ok(json!({"ready":true})) })),
    );
    let mut events = h.client.subscribe();
    h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"one"}}));
    milestones.recv().await.unwrap();
    h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"two"}}));
    h.update(json!({"sessionUpdate":"state_update","state":"idle"}));
    h.event(PeerEvent::Request {
        id: RequestId::String("barrier".into()),
        method: "x/barrier".into(),
        params: json!({}),
    });
    assert_eq!(h.response().await.result.unwrap()["ready"], true);
    assert!(!prompt_result.is_finished());
    assert_eq!(*values.lock().unwrap(), vec![json!("one")]);
    release.send(()).unwrap();
    for _ in 0..3 {
        notification(&mut events).await;
    }
    assert_eq!(
        prompt_result.await.unwrap().unwrap()["stopReason"],
        "end_turn"
    );
    assert_eq!(
        *values.lock().unwrap(),
        vec![json!("one"), json!("two"), Value::Null]
    );
}
#[tokio::test]
async fn handler_registration_replay_cannot_be_overtaken_by_new_incoming_updates() {
    let mut h = Harness::new();
    h.initialize(Generation::V2, 2).await;
    let mut events = h.client.subscribe();
    h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"old"}}));
    notification(&mut events).await;
    let (entered, mut milestones) = mpsc::unbounded_channel();
    let (release, held) = oneshot::channel();
    let held = Arc::new(Mutex::new(Some(held)));
    let values = Arc::new(Mutex::new(Vec::new()));
    let seen = values.clone();
    let client = h.client.clone();
    let registration = tokio::spawn(async move {
        client
            .handle_notification(
                "session/update",
                Arc::new(move |value| {
                    let held = held.clone();
                    let entered = entered.clone();
                    let seen = seen.clone();
                    Box::pin(async move {
                        seen.lock()
                            .unwrap()
                            .push(value["update"]["content"]["text"].clone());
                        let barrier = held.lock().unwrap().take();
                        if let Some(barrier) = barrier {
                            entered.send(()).unwrap();
                            barrier.await.unwrap();
                        }
                        Ok(())
                    })
                }),
            )
            .await;
    });
    milestones.recv().await.unwrap();
    h.update(
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"new-1"}}),
    );
    h.update(
        json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"new-2"}}),
    );
    h.client.handle_request(
        "x/barrier",
        Arc::new(|_, _| Box::pin(async { Ok(json!({})) })),
    );
    h.event(PeerEvent::Request {
        id: RequestId::String("barrier".into()),
        method: "x/barrier".into(),
        params: json!({}),
    });
    h.response().await;
    assert_eq!(*values.lock().unwrap(), vec![json!("old")]);
    release.send(()).unwrap();
    registration.await.unwrap();
    for _ in 0..2 {
        notification(&mut events).await;
    }
    assert_eq!(
        *values.lock().unwrap(),
        vec![json!("old"), json!("new-1"), json!("new-2")]
    );
}
#[tokio::test]
async fn protocol_failure_and_shutdown_cancel_active_raw_waiters_without_waiting_for_timeout() {
    for shutdown in [false, true] {
        let mut h = Harness::new();
        let client = h.client.clone();
        let pending =
            tokio::spawn(async move { client.initialize(json!({"protocolVersion":2})).await });
        let call = h.call().await;
        let reply = call.reply.unwrap();
        if shutdown {
            h.client.shutdown();
        } else {
            h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text"}}));
        }
        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(if shutdown {
            matches!(result, Err(AcpError::Closed))
        } else {
            matches!(result, Err(AcpError::Schema(_)))
        });
        assert!(reply.is_closed());
    }
    let mut h = Harness::new();
    let client = h.client.clone();
    let pending = tokio::spawn(async move { client.raw_request("x/wait", json!({})).await });
    let call = h.call().await;
    for _ in 0..1100 {
        h.event(PeerEvent::Notification {
            method: "x/update".into(),
            params: json!({}),
        });
    }
    let result = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(AcpError::Transport(_))));
    assert!(call.reply.unwrap().is_closed());
}
