//! Original agent.test.ts scenarios plus v2 replay and handler isolation cases.
//! Wire milestones use channels; timeouts only bound a deadlocked test.
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_acp::{
    AcpError, Agent, ClientEvent, PayloadCodec, Peer, PeerEvent, RequestId, RpcError,
    errors::{Failure, FailureCause, RequestOperation},
    v2,
};
use tokio::sync::{broadcast, mpsc, oneshot};

struct Call {
    method: String,
    params: Value,
    reply: Option<oneshot::Sender<Result<Value, AcpError>>>,
}
struct Reply {
    id: RequestId,
    result: Result<Value, RpcError>,
    cause: Option<t3_acp::EffectCause>,
}
struct MemoryPeer {
    events: broadcast::Sender<PeerEvent>,
    calls: mpsc::UnboundedSender<Call>,
    replies: mpsc::UnboundedSender<Reply>,
}
impl Peer for MemoryPeer {
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
            let (tx, rx) = oneshot::channel();
            self.calls
                .send(Call {
                    method: method.into(),
                    params,
                    reply: Some(tx),
                })
                .map_err(|_| AcpError::Closed)?;
            rx.await.map_err(|_| AcpError::Closed)?
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
                .map_err(|_| AcpError::Closed)
        })
    }
    fn respond<'a>(
        &'a self,
        id: RequestId,
        result: Result<Value, RpcError>,
    ) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            self.replies
                .send(Reply {
                    id,
                    result,
                    cause: None,
                })
                .map_err(|_| AcpError::Closed)
        })
    }
    fn respond_cause<'a>(
        &'a self,
        id: RequestId,
        cause: t3_acp::EffectCause,
    ) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            self.replies
                .send(Reply {
                    id,
                    result: Err(RpcError::internal()),
                    cause: Some(cause),
                })
                .map_err(|_| AcpError::Closed)
        })
    }
}
struct Harness {
    agent: Agent,
    peer: Arc<MemoryPeer>,
    calls: mpsc::UnboundedReceiver<Call>,
    replies: mpsc::UnboundedReceiver<Reply>,
}
impl Harness {
    fn new() -> Self {
        let (events, _) = broadcast::channel(128);
        let (calls, call_rx) = mpsc::unbounded_channel();
        let (replies, reply_rx) = mpsc::unbounded_channel();
        let peer = Arc::new(MemoryPeer {
            events,
            calls,
            replies,
        });
        let agent = Agent::new(peer.clone(), Duration::from_secs(10));
        Self {
            agent,
            peer,
            calls: call_rx,
            replies: reply_rx,
        }
    }
    fn request(&self, id: RequestId, method: &str, params: Value) {
        self.peer
            .events
            .send(PeerEvent::Request {
                id,
                method: method.into(),
                params,
            })
            .unwrap();
    }
    fn notify(&self, method: &str, params: Value) {
        self.peer
            .events
            .send(PeerEvent::Notification {
                method: method.into(),
                params,
            })
            .unwrap();
    }
    async fn reply(&mut self) -> Reply {
        bounded(self.replies.recv()).await.unwrap()
    }
    async fn call(&mut self) -> Call {
        bounded(self.calls.recv()).await.unwrap()
    }
}
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("wire milestone")
}
fn initialize() -> Value {
    json!({"protocolVersion":2,"capabilities":{},"info":{"name":"effect-acp-test","version":"0.0.0"}})
}
fn permission() -> v2::RequestPermissionRequest {
    v2::RequestPermissionRequest::decode(json!({"sessionId":"session-1","title":"Allow mock action","subject":{"type":"tool_call","toolCall":{"toolCallId":"tool-1","title":"Allow mock action"}},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]})).unwrap()
}

#[tokio::test]
async fn agent_handles_core_requests_and_parallel_outbound_client_requests() {
    let mut h = Harness::new();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    h.agent.handle_initialize(Arc::new({let contexts=contexts.clone();move|request,context|{assert_eq!(request.as_value(),&initialize());contexts.lock().unwrap().push(context);Box::pin(async{Ok(v2::InitializeResponse::decode(json!({"protocolVersion":2,"capabilities":{},"info":{"name":"mock-agent","version":"0.0.0"}}))?)})}}));
    h.agent.handle_raw(
        "x/test",
        Arc::new({
            let contexts = contexts.clone();
            move |request, context| {
                assert_eq!(request, json!({"hello":"world"}));
                contexts.lock().unwrap().push(context);
                Box::pin(async { Ok(json!({"ok":true})) })
            }
        }),
    );
    let agent = h.agent.clone();
    let pending = tokio::spawn(async move { agent.request_permission(permission()).await });
    let agent = h.agent.clone();
    let extension = tokio::spawn(async move {
        agent
            .connection()
            .raw_request("x/test", json!({"hello":"world"}))
            .await
    });
    let first = h.call().await;
    let second = h.call().await;
    // Both outbound calls remain pending while the agent serves this inbound
    // request. A dispatcher that blocks on its own request would deadlock here.
    h.request(RequestId::Number(2.into()), "initialize", initialize());
    let reply = h.reply().await;
    assert_eq!(reply.id, RequestId::Number(2.into()));
    assert_eq!(reply.result.unwrap()["info"]["name"], "mock-agent");
    assert!(!pending.is_finished());
    assert!(!extension.is_finished());
    for call in [first, second] {
        assert!(call.reply.is_some());
        let value = if call.method == "session/request_permission" {
            assert_eq!(call.params, permission().into_value());
            json!({"outcome":{"outcome":"selected","optionId":"allow"}})
        } else {
            assert_eq!(call.method, "x/test");
            json!({"ok":true})
        };
        call.reply.unwrap().send(Ok(value)).unwrap();
    }
    assert_eq!(
        bounded(pending).await.unwrap().unwrap().as_value()["outcome"]["optionId"],
        "allow"
    );
    assert_eq!(
        bounded(extension).await.unwrap().unwrap(),
        json!({"ok":true})
    );
    h.request(
        RequestId::String("extension-3".into()),
        "x/test",
        json!({"hello":"world"}),
    );
    assert_eq!(h.reply().await.result.unwrap(), json!({"ok":true}));
    let contexts = contexts.lock().unwrap();
    assert_eq!(contexts[0].request_id, "$t3:jsonrpc:number:2");
    assert_eq!(contexts[1].request_id, "extension-3");
}

#[tokio::test]
async fn replay_cursor_selects_load_or_resume_with_source_fallbacks() {
    for (register_load, register_resume) in [(true, true), (true, false), (false, true)] {
        let mut h = Harness::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        if register_load {
            h.agent.handle_load_session(Arc::new({
                let tx = tx.clone();
                move |request, _| {
                    tx.send(("load", request.into_value())).unwrap();
                    Box::pin(async { Ok(v2::ResumeSessionResponse::decode(json!({}))?) })
                }
            }));
        }
        if register_resume {
            h.agent.handle_resume_session(Arc::new({
                let tx = tx.clone();
                move |request, _| {
                    tx.send(("resume", request.into_value())).unwrap();
                    Box::pin(async { Ok(v2::ResumeSessionResponse::decode(json!({}))?) })
                }
            }));
        }
        for (replay, prefer_load) in [
            (Some(json!({"type":"start"})), true),
            (None, false),
            (Some(Value::Null), false),
            (Some(json!({"type":"_cursor","token":"opaque"})), false),
        ] {
            let mut request = json!({"sessionId":"s","cwd":"/project"});
            if let Some(replay) = replay {
                request["replayFrom"] = replay;
            }
            h.request(
                RequestId::Number(0.into()),
                "session/resume",
                request.clone(),
            );
            assert_eq!(h.reply().await.result.unwrap(), json!({}));
            let (handler, received) = bounded(rx.recv()).await.unwrap();
            assert_eq!(received, request);
            assert!(!received.as_object().unwrap().contains_key("mcpServers"));
            assert_eq!(
                handler,
                if prefer_load && register_load || !register_resume {
                    "load"
                } else {
                    "resume"
                }
            );
        }
    }
}

#[tokio::test]
async fn cancel_and_extension_notifications_isolate_failures_and_continue_in_order() {
    let mut h = Harness::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    h.agent
        .handle_cancel(Arc::new({
            let tx = tx.clone();
            move |request| {
                let id = request.as_value()["sessionId"].as_str().unwrap().to_owned();
                tx.send(format!("cancel:{id}")).unwrap();
                Box::pin(async { Err(AcpError::Transport("handler failure".into())) })
            }
        }))
        .await;
    h.agent
        .connection()
        .handle_notification(
            "x/ping",
            Arc::new(move |value| {
                tx.send(format!("ping:{}", value["count"])).unwrap();
                Box::pin(async { Ok(()) })
            }),
        )
        .await;
    h.notify("session/cancel", json!({"sessionId":42})); // invalid handler payload is isolated
    h.notify("session/cancel", json!({"sessionId":"session-1"}));
    h.notify("x/ping", json!({"count":2}));
    assert_eq!(bounded(rx.recv()).await.unwrap(), "cancel:session-1");
    assert_eq!(bounded(rx.recv()).await.unwrap(), "ping:2");
    h.agent.handle_raw(
        "x/alive",
        Arc::new(|_, _| Box::pin(async { Ok(json!({"alive":true})) })),
    );
    h.request(RequestId::String("alive".into()), "x/alive", json!({}));
    assert_eq!(h.reply().await.result.unwrap(), json!({"alive":true}));
}

#[tokio::test]
async fn request_defects_reply_once_and_core_payloads_cannot_escape_to_extensions() {
    let mut h = Harness::new();
    h.agent.handle_initialize(Arc::new(|_, _| {
        Box::pin(async { panic!("private handler defect") })
    }));
    h.agent.handle_unknown_request(Arc::new(|_, _| {
        Box::pin(async { Ok(json!({"extension":true})) })
    }));
    h.request(RequestId::Number(7.into()), "initialize", initialize());
    let reply = h.reply().await;
    let cause = reply.cause.expect("core defect uses Effect Cause envelope");
    assert_eq!(cause.code, 0);
    assert!(
        matches!(&cause.data[..],[t3_acp::EffectCauseReason::Die{defect}]if defect["message"]=="private handler defect")
    );
    h.request(
        RequestId::Number(8.into()),
        "initialize",
        json!({"protocolVersion":"invalid"}),
    );
    let cause = h
        .reply()
        .await
        .cause
        .expect("core decode rejection is an Effect defect");
    assert_eq!(
        cause,
        t3_acp::EffectCause::defect(json!(
            "Expected ProtocolVersion\n  at [\"protocolVersion\"]"
        ))
    );
    h.request(RequestId::Number(9.into()), "auth/logout", json!({}));
    assert_eq!(h.reply().await.result.unwrap_err().code, -32601);
    h.request(RequestId::String("future".into()), "x/future", json!({}));
    assert_eq!(h.reply().await.result.unwrap(), json!({"extension":true}));
    assert!(h.replies.try_recv().is_err());
}

#[tokio::test]
async fn extension_payload_and_handler_failures_retain_typed_causes_and_safe_replies() {
    let mut h = Harness::new();
    let mut events = h.agent.connection().subscribe();
    h.agent.handle_extension(
        "x/prompt",
        PayloadCodec::wire(),
        PayloadCodec::wire(),
        Arc::new(|_: v2::PromptRequest, _| {
            Box::pin(async {
                Err::<v2::PromptResponse, _>(AcpError::Transport("private handler cause".into()))
            })
        }),
    );
    let rejected =
        json!({"sessionId":"s","prompt":[{"type":"text","text":{"password":"private"}}]});
    let expected = v2::PromptRequest::decode(rejected.clone())
        .unwrap_err()
        .issue
        .diagnostics();
    h.request(RequestId::Number(0.into()), "x/prompt", rejected.clone());
    let reply = h.reply().await;
    let error = reply.result.unwrap_err();
    assert_eq!(error.code, -32602);
    assert_eq!(error.data, Some(serde_json::to_value(expected).unwrap()));
    assert!(!serde_json::to_string(&error).unwrap().contains("private"));
    let ClientEvent::RequestHandlerFailed {
        request_id,
        error: AcpError::Failure(error),
    } = bounded(events.recv()).await.unwrap()
    else {
        panic!("typed handler error missing")
    };
    assert_eq!(request_id, "$t3:jsonrpc:number:0");
    let Failure::Request(error) = error.as_ref() else {
        panic!()
    };
    let Some(FailureCause::Schema(cause)) = &error.diagnostics.cause else {
        panic!("schema cause missing")
    };
    assert_eq!(cause.cause, rejected);
    // Consume the actual write acknowledgement before the next local failure.
    assert!(matches!(
        bounded(events.recv()).await.unwrap(),
        ClientEvent::ResponseAcknowledged { .. }
    ));
    h.request(
        RequestId::String("0".into()),
        "x/prompt",
        json!({"sessionId":"s","prompt":[]}),
    );
    let error = h.reply().await.result.unwrap_err();
    assert_eq!(error.code, -32603);
    assert_eq!(
        error.message,
        "ACP extension request handler failed for method 'x/prompt'"
    );
    assert!(error.data.is_none());
    let ClientEvent::RequestHandlerFailed {
        error: AcpError::Failure(error),
        ..
    } = bounded(events.recv()).await.unwrap()
    else {
        panic!()
    };
    let Failure::Request(error) = error.as_ref() else {
        panic!()
    };
    assert_eq!(
        error.diagnostics.operation,
        Some(RequestOperation::HandleExtensionRequest)
    );
    assert!(
        matches!(&error.diagnostics.cause,Some(FailureCause::Error(cause))if matches!(cause.as_ref(),AcpError::Transport(message)if message=="private handler cause"))
    );
}

#[tokio::test]
async fn extension_response_encoding_failures_are_typed_and_extensions_do_not_override_core() {
    let mut h = Harness::new();
    let mut events = h.agent.connection().subscribe();
    let rejected = json!({"protocolVersion":"private invalid response"});
    let cause = v2::InitializeResponse::decode(rejected.clone()).unwrap_err();
    h.agent.handle_extension(
        "x/encode",
        PayloadCodec::<v2::PromptRequest>::wire(),
        PayloadCodec::new(|_| Ok(()), move |()| Err(cause.clone())),
        Arc::new(|_, _| Box::pin(async { Ok(()) })),
    );
    h.request(
        RequestId::Number(7.into()),
        "x/encode",
        json!({"sessionId":"s","prompt":[]}),
    );
    assert_eq!(
        h.reply().await.result.unwrap_err().message,
        "Internal error"
    );
    let ClientEvent::RequestHandlerFailed {
        error: AcpError::Failure(error),
        ..
    } = bounded(events.recv()).await.unwrap()
    else {
        panic!()
    };
    let Failure::Request(error) = error.as_ref() else {
        panic!()
    };
    assert_eq!(
        error.diagnostics.operation,
        Some(RequestOperation::EncodeExtensionResponse)
    );
    let Some(FailureCause::Error(cause)) = &error.diagnostics.cause else {
        panic!()
    };
    let AcpError::Failure(cause) = cause.as_ref() else {
        panic!()
    };
    let Failure::ProtocolParse(cause) = cause.as_ref() else {
        panic!()
    };
    assert_eq!(cause.request_id, Some(RequestId::Number(7.into())));
    assert!(matches!(&cause.cause,FailureCause::Schema(error)if error.cause==rejected));
    h.agent.handle_extension(
        "initialize",
        PayloadCodec::<v2::InitializeRequest>::wire(),
        PayloadCodec::<v2::InitializeResponse>::wire(),
        Arc::new(|_, _| Box::pin(async { panic!("core must not invoke extension registration") })),
    );
    h.agent.handle_initialize(Arc::new(|_, _| {
        Box::pin(async {
            Ok(v2::InitializeResponse::decode(
                json!({"protocolVersion":2,"info":{"name":"core","version":"1"}}),
            )?)
        })
    }));
    h.request(RequestId::Number(8.into()), "initialize", initialize());
    assert_eq!(h.reply().await.result.unwrap()["info"]["name"], "core");
}

#[tokio::test]
async fn agent_raw_response_frames_match_actual_original_agent_oracle() {
    for line in include_str!("fixtures/agent-wire.jsonl").lines() {
        let fixture: Value = serde_json::from_str(line).unwrap();
        let mut h = Harness::new();
        let mode = fixture["mode"].as_str().unwrap().to_owned();
        h.agent.handle_initialize(Arc::new(move |_, _| {
            let mode = mode.clone();
            Box::pin(async move {
                match mode.as_str() {
                    "request-error" => Err(t3_acp::errors::RequestError::auth_required(
                        Some("custom auth"),
                        Some(Value::Null),
                    )
                    .into()),
                    "transport-error" => Err(t3_acp::errors::TransportError {
                        operation: None,
                        method: None,
                        detail: None,
                        pid: None,
                        cause: json!({"private":true}).into(),
                    }
                    .into()),
                    "die" => panic!("handler bug"),
                    _ => Ok(v2::InitializeResponse::decode(
                        json!({"protocolVersion":2,"info":{"name":"mock-agent","version":"1"}}),
                    )?),
                }
            })
        }));
        h.agent.handle_unknown_request(Arc::new(|_, _| {
            Box::pin(async {
                Err(RpcError {
                    code: -32000,
                    message: "custom auth".into(),
                    data: Some(Value::Null),
                })
            })
        }));
        let input = &fixture["input"];
        h.request(
            serde_json::from_value(input["id"].clone()).unwrap(),
            input["method"].as_str().unwrap(),
            input["params"].clone(),
        );
        let reply = h.reply().await;
        let output = if let Some(cause) = reply.cause {
            json!({"jsonrpc":"2.0","id":reply.id,"error":cause})
        } else {
            match reply.result {
                Ok(result) => json!({"jsonrpc":"2.0","id":reply.id,"result":result}),
                Err(error) => {
                    json!({"jsonrpc":"2.0","id":reply.id,"error":t3_acp::EffectCause::new(vec![t3_acp::EffectCauseReason::Fail{error:serde_json::to_value(error).unwrap()}])})
                }
            }
        };
        assert_eq!(output, fixture["output"], "mode {}", fixture["mode"]);
    }
}
