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
    reply: Option<Reply>,
}
struct Reply {
    sender: oneshot::Sender<(Result<Value, AcpError>, t3_acp::IngressAcknowledgement)>,
    ingress: broadcast::Sender<PeerEvent>,
}
impl Reply {
    fn send(self, result: Result<Value, AcpError>) -> Result<(), Result<Value, AcpError>> {
        let acknowledgement = t3_acp::IngressAcknowledgement::new();
        let _ = self.ingress.send(PeerEvent::IngressBarrier {
            acknowledgement: acknowledgement.clone(),
        });
        self.sender
            .send((result, acknowledgement))
            .map_err(|(result, _)| result)
    }
    fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}
struct Response {
    id: RequestId,
    result: Result<Value, RpcError>,
    cause: Option<t3_acp::EffectCause>,
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
                    reply: Some(Reply {
                        sender,
                        ingress: self.events.clone(),
                    }),
                })
                .unwrap();
            let (result, acknowledgement) = receiver.await.map_err(|_| AcpError::Closed)?;
            acknowledgement.wait().await;
            result
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
            self.responses
                .send(Response {
                    id,
                    result,
                    cause: None,
                })
                .unwrap();
            if let Some(gate) = gate {
                gate.await.map_err(|_| AcpError::Closed)?
            } else {
                Ok(())
            }
        })
    }
    fn respond_cause<'a>(
        &'a self,
        id: RequestId,
        cause: t3_acp::EffectCause,
    ) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            self.responses
                .send(Response {
                    id,
                    result: Err(RpcError::internal()),
                    cause: Some(cause),
                })
                .unwrap();
            Ok(())
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
        Self::with_options(t3_acp::ClientOptions::default())
    }
    fn with_options(options: t3_acp::ClientOptions) -> Self {
        let (events, _) = broadcast::channel(1024);
        let (calls, call_rx) = mpsc::unbounded_channel();
        let (responses, response_rx) = mpsc::unbounded_channel();
        let peer = Arc::new(FakePeer {
            events,
            calls,
            responses,
            response_gate: Mutex::new(None),
        });
        let client = Client::with_options(peer.clone(), Duration::from_secs(10), options);
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
#[tokio::test]
async fn response_cause_normalization_matches_actual_original_client_failures() {
    for line in include_str!("fixtures/client-response-errors.jsonl").lines() {
        let fixture: Value = serde_json::from_str(line).unwrap();
        let mut h = Harness::new();
        let core = fixture["core"] == true;
        let client = h.client.clone();
        let pending = tokio::spawn(async move {
            if core {
                client
                    .initialize(
                        json!({"protocolVersion":2,"clientInfo":{"name":"test","version":"1"}}),
                    )
                    .await
            } else {
                client.extension_request("x/test", json!({})).await
            }
        });
        let call = h.call().await;
        let cause: t3_acp::EffectCause = serde_json::from_value(fixture["error"].clone()).unwrap();
        let id: RequestId = serde_json::from_value(fixture["requestId"].clone()).unwrap();
        call.reply
            .unwrap()
            .send(Err(AcpError::ResponseCause {
                method: call.method,
                request_id: id,
                cause,
            }))
            .unwrap();
        let error = pending.await.unwrap().unwrap_err();
        let observed = match error {
            AcpError::Failure(failure) => {
                let t3_acp::errors::Failure::Request(error) = failure.as_ref() else {
                    panic!("wrong category {failure}")
                };
                let mut result = json!({"tag":"AcpRequestError","message":error.error_message,"code":error.code});
                let d = &error.diagnostics;
                if let Some(method) = &d.method {
                    result["method"] = json!(method);
                }
                if let Some(id) = &d.request_id {
                    result["requestId"] = json!(id);
                }
                if let Some(operation) = d.operation {
                    result["operation"] = json!(operation);
                }
                if let Some(data) = &error.data {
                    result["data"] = data.clone();
                }
                result["causeShape"] = json!(match &d.cause {
                    Some(t3_acp::errors::FailureCause::Value(value)) if value.is_array() => "array",
                    Some(t3_acp::errors::FailureCause::Value(value))
                        if value.get("code").is_some() =>
                        "protocol-error",
                    _ => "other",
                });
                result
            }
            AcpError::ResponseDefect {
                cause,
                decode_error,
                ..
            } => {
                let message = if let Some(error) = decode_error {
                    format!("SchemaError({})", error.issue.formatted())
                } else {
                    cause
                        .data
                        .iter()
                        .find_map(|reason| {
                            if let t3_acp::EffectCauseReason::Die { defect } = reason {
                                defect.as_str().map(str::to_owned)
                            } else {
                                None
                            }
                        })
                        .expect("defect")
                };
                json!({"tag":"Defect","message":message,"causeShape":"defect"})
            }
            other => panic!("wrong error category {other:?}"),
        };
        assert_eq!(observed, fixture["output"], "source fixture {fixture}");
    }
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
    assert!(
        matches!(pending.await.unwrap(), Err(AcpError::Failure(ref error)) if matches!(error.as_ref(), t3_acp::errors::Failure::ProtocolParse(_)))
    );
    assert!(matches!(
        h.client.raw_notify("x/test", json!({})).await,
        Err(AcpError::Failure(ref error)) if matches!(error.as_ref(), t3_acp::errors::Failure::ProtocolParse(_))
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
    let cause = h
        .response()
        .await
        .cause
        .expect("malformed core request uses Effect defect");
    assert!(matches!(
        &cause.data[..],
        [t3_acp::EffectCauseReason::Die { .. }]
    ));
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
    let stream = h.client.notifications();
    for index in 32..64 {
        let Some(t3_acp::IncomingNotification::SessionUpdate { params, .. }) = stream.try_recv()
        else {
            panic!("missing raw update");
        };
        assert_eq!(params["update"]["content"]["text"], index.to_string());
    }
    assert!(stream.try_recv().is_none());
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
async fn deferred_notification_handlers_hold_later_requests_chunks_and_idle_in_wire_order() {
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
    assert!(h.responses.try_recv().is_err());
    assert!(!prompt_result.is_finished());
    assert_eq!(*values.lock().unwrap(), vec![json!("one")]);
    release.send(()).unwrap();
    assert_eq!(h.response().await.result.unwrap()["ready"], true);
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
    assert!(h.responses.try_recv().is_err());
    assert_eq!(*values.lock().unwrap(), vec![json!("old")]);
    release.send(()).unwrap();
    registration.await.unwrap();
    h.response().await;
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
            matches!(result, Err(AcpError::Failure(ref error)) if matches!(error.as_ref(), t3_acp::errors::Failure::ProtocolParse(_)))
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

#[tokio::test]
async fn raw_stream_slides_at_32_and_consumers_share_one_queue() {
    let h = Harness::new();
    let (handled, mut milestones) = mpsc::unbounded_channel();
    h.client
        .handle_unknown_notification(Arc::new(move |params| {
            let handled = handled.clone();
            Box::pin(async move {
                handled.send(params["index"].as_u64().unwrap()).unwrap();
                Ok(())
            })
        }));
    for index in 0..64 {
        h.event(PeerEvent::Notification {
            method: "x/performance".into(),
            params: json!({"index":index}),
        });
    }
    for index in 0..64 {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), milestones.recv())
                .await
                .unwrap(),
            Some(index)
        );
    }
    let first = h.client.notifications();
    let second = h.client.notifications();
    for index in 32..64 {
        let stream = if index % 2 == 0 { &first } else { &second };
        assert_eq!(
            stream.try_recv(),
            Some(t3_acp::IncomingNotification::ExtNotification {
                method: "x/performance".into(),
                params: json!({"index":index})
            })
        );
    }
    assert_eq!(first.try_recv(), None);
    assert_eq!(second.try_recv(), None);
    // A cancelled waiter leaves the following offer available to another reader.
    {
        let pending = first.recv();
        tokio::pin!(pending);
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
    }
    h.event(PeerEvent::Notification {
        method: "x/performance".into(),
        params: json!({"index":64}),
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), milestones.recv())
            .await
            .unwrap(),
        Some(64)
    );
    assert_eq!(
        second.recv().await,
        t3_acp::IncomingNotification::ExtNotification {
            method: "x/performance".into(),
            params: json!({"index":64})
        }
    );
}

#[tokio::test]
async fn raw_notification_is_offered_before_handler_finishes_and_retains_alias_method() {
    let h = Harness::new();
    let (entered, mut started) = mpsc::unbounded_channel();
    let (release, gate) = oneshot::channel();
    let gate = Arc::new(Mutex::new(Some(gate)));
    h.client
        .handle_notification(
            "elicitation/complete",
            Arc::new(move |value| {
                let entered = entered.clone();
                let gate = gate.lock().unwrap().take();
                Box::pin(async move {
                    entered.send(value).unwrap();
                    if let Some(gate) = gate {
                        gate.await.unwrap();
                    }
                    Ok(())
                })
            }),
        )
        .await;
    let params = json!({"elicitationId":"e"});
    h.event(PeerEvent::Notification {
        method: "session/elicitation/complete".into(),
        params: params.clone(),
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), started.recv())
            .await
            .unwrap(),
        Some(params.clone())
    );
    assert_eq!(
        h.client.notifications().try_recv(),
        Some(t3_acp::IncomingNotification::ElicitationComplete {
            method: "session/elicitation/complete".into(),
            params
        })
    );
    release.send(()).unwrap();
}

#[tokio::test]
async fn normalized_update_transform_replaces_large_payload_in_raw_stream_and_handlers() {
    let called = Arc::new(AtomicUsize::new(0));
    let calls = called.clone();
    let replacement = json!({"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Image data omitted."}}});
    let transformed = replacement.clone();
    let mut h = Harness::with_options(t3_acp::ClientOptions {
        transform_session_update: Some(Arc::new(move |value| {
            assert_eq!(
                value["update"]["content"]["data"].as_str().unwrap().len(),
                1_048_576
            );
            calls.fetch_add(1, Ordering::Relaxed);
            transformed.clone()
        })),
    });
    h.initialize(Generation::V1, 1).await;
    let (handled, mut received) = mpsc::unbounded_channel();
    h.client
        .handle_notification(
            "session/update",
            Arc::new(move |value| {
                let handled = handled.clone();
                Box::pin(async move {
                    handled.send(value).unwrap();
                    Ok(())
                })
            }),
        )
        .await;
    h.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"image","data":"A".repeat(1_048_576),"mimeType":"image/png"}}));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap(),
        Some(replacement.clone())
    );
    assert_eq!(
        h.client.notifications().recv().await,
        t3_acp::IncomingNotification::SessionUpdate {
            method: "session/update".into(),
            params: replacement
        }
    );
    assert_eq!(called.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn transform_defect_terminates_pending_and_future_calls_with_private_transport_cause() {
    let mut h = Harness::with_options(t3_acp::ClientOptions {
        transform_session_update: Some(Arc::new(|_| panic!("normalizer bug"))),
    });
    let client = h.client.clone();
    let pending = tokio::spawn(async move { client.raw_request("x/pending", json!({})).await });
    let _call = h.call().await;
    h.update(json!({"sessionUpdate":"plan","entries":[]}));
    let error = tokio::time::timeout(Duration::from_secs(2), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    let AcpError::Failure(failure) = &error else {
        panic!("wrong category {error:?}");
    };
    let t3_acp::errors::Failure::Transport(error) = failure.as_ref() else {
        panic!("wrong category {failure:?}");
    };
    assert_eq!(
        error.operation,
        Some(t3_acp::errors::TransportOperation::ReadInputStream)
    );
    assert_eq!(
        error.to_string(),
        "ACP transport operation read-input-stream failed."
    );
    let t3_acp::errors::FailureCause::Value(cause) = &error.cause else {
        panic!("wrong private cause");
    };
    assert_eq!(cause["message"], "normalizer bug");
    assert!(h.client.raw_notify("x/future", json!({})).await.is_err());
    assert_eq!(h.client.notifications().try_recv(), None);
}

#[tokio::test]
async fn gated_notification_holds_mixed_ingress_like_actual_original_protocol() {
    // Original protocol source probe: notification+request, notification+reply,
    // and notification+EOF all report beforeRelease=false, afterRelease=true.
    for next in ["request", "response", "termination"] {
        let mut h = Harness::new();
        let (entered, mut started) = mpsc::unbounded_channel();
        let (release, held) = oneshot::channel();
        let held = Arc::new(Mutex::new(Some(held)));
        h.client.handle_unknown_notification(Arc::new(move |_| {
            let entered = entered.clone();
            let held = held.lock().unwrap().take();
            Box::pin(async move {
                entered.send(()).unwrap();
                if let Some(held) = held {
                    held.await.unwrap();
                }
                Ok(())
            })
        }));
        h.client.handle_request(
            "x/callback",
            Arc::new(|_, _| Box::pin(async { Ok(json!({"ok":true})) })),
        );
        let pending = if next != "request" {
            let client = h.client.clone();
            Some(tokio::spawn(async move {
                client.raw_request("x/pending", json!({})).await
            }))
        } else {
            None
        };
        let call = if pending.is_some() {
            Some(h.call().await)
        } else {
            None
        };
        h.event(PeerEvent::Notification {
            method: "x/gate".into(),
            params: json!({}),
        });
        match next {
            "request" => h.event(PeerEvent::Request {
                id: RequestId::Number(7.into()),
                method: "x/callback".into(),
                params: json!({}),
            }),
            "response" => call
                .unwrap()
                .reply
                .unwrap()
                .send(Ok(json!({"ok":true})))
                .unwrap(),
            "termination" => {
                // Retain the pending transport reply until the input termination
                // barrier is consumed, as a real ordered transport does.
                let _pending_transport = call;
                h.event(PeerEvent::Closed(AcpError::Closed));
                started.recv().await.unwrap();
                assert!(!pending.as_ref().unwrap().is_finished());
                release.send(()).unwrap();
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(2), pending.unwrap())
                        .await
                        .unwrap()
                        .unwrap(),
                    Err(AcpError::Closed)
                ));
                continue;
            }
            _ => unreachable!(),
        }
        tokio::time::timeout(Duration::from_secs(2), started.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(h.responses.try_recv().is_err());
        if let Some(pending) = &pending {
            assert!(!pending.is_finished());
        }
        release.send(()).unwrap();
        if let Some(pending) = pending {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), pending)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                json!({"ok":true})
            );
        } else {
            assert_eq!(h.response().await.result.unwrap(), json!({"ok":true}));
        }
    }
}

#[tokio::test]
async fn core_request_admitted_before_notification_can_finish_while_reader_is_gated() {
    let mut h = Harness::new();
    let (callback_started, mut callbacks) = mpsc::unbounded_channel();
    let (finish_callback, callback_gate) = oneshot::channel();
    let callback_gate = Arc::new(Mutex::new(Some(callback_gate)));
    h.client.handle_request(
        "fs/read_text_file",
        Arc::new(move |_, _| {
            let started = callback_started.clone();
            let gate = callback_gate.lock().unwrap().take().unwrap();
            Box::pin(async move {
                started.send(()).unwrap();
                gate.await.unwrap();
                Ok(json!({"content":"done"}))
            })
        }),
    );
    let (notification_started, mut notifications) = mpsc::unbounded_channel();
    let (finish_notification, notification_gate) = oneshot::channel();
    let notification_gate = Arc::new(Mutex::new(Some(notification_gate)));
    h.client.handle_unknown_notification(Arc::new(move |_| {
        let started = notification_started.clone();
        let gate = notification_gate.lock().unwrap().take().unwrap();
        Box::pin(async move {
            started.send(()).unwrap();
            gate.await.unwrap();
            Ok(())
        })
    }));
    h.event(PeerEvent::Request {
        id: RequestId::Number(7.into()),
        method: "fs/read_text_file".into(),
        params: json!({"sessionId":"s","path":"/test"}),
    });
    tokio::time::timeout(Duration::from_secs(2), callbacks.recv())
        .await
        .unwrap()
        .unwrap();
    h.event(PeerEvent::Notification {
        method: "x/gate".into(),
        params: json!({}),
    });
    tokio::time::timeout(Duration::from_secs(2), notifications.recv())
        .await
        .unwrap()
        .unwrap();
    finish_callback.send(()).unwrap();
    assert_eq!(
        h.response().await.result.unwrap(),
        json!({"content":"done"})
    );
    finish_notification.send(()).unwrap();
}
