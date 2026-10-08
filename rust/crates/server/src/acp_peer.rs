//! ACP peer over the same bounded, owned subprocess transport as Codex.
use crate::provider_process::{ProcessError, ProcessEvent, ProcessOptions, ProviderProcess};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_acp::{AcpError, Peer, PeerEvent, RequestId, RpcError};
use tokio::sync::{broadcast, watch};

struct Inner {
    process: ProviderProcess,
    events: broadcast::Sender<PeerEvent>,
    first_receiver: Mutex<Option<broadcast::Receiver<PeerEvent>>>,
    terminal: watch::Sender<Option<AcpError>>,
    forwarding: tokio::task::JoinHandle<()>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.forwarding.abort();
    }
}
#[derive(Clone)]
pub struct ProcessPeer(Arc<Inner>);
impl ProcessPeer {
    pub fn spawn(options: ProcessOptions) -> Result<Self, AcpError> {
        Self::spawn_with_options(options, t3_acp::transport::ProtocolOptions::default())
    }
    pub fn spawn_with_options(
        options: ProcessOptions,
        protocol_options: t3_acp::transport::ProtocolOptions,
    ) -> Result<Self, AcpError> {
        let process = ProviderProcess::spawn_json_rpc_with_options(options, protocol_options)
            .map_err(error)?;
        // Register before initialization; the forwarder retains no process clone,
        // so releasing the peer also releases and reaps the owned child.
        let mut incoming = process.subscribe();
        let (events, first_receiver) = broadcast::channel(1024);
        let outgoing = events.clone();
        let (terminal, _) = watch::channel(None);
        let terminal_writer = terminal.clone();
        let forwarding = tokio::spawn(async move {
            loop {
                let event = match incoming.recv().await {
                    Ok(ProcessEvent::IngressBarrier { acknowledgement }) => {
                        PeerEvent::IngressBarrier { acknowledgement }
                    }
                    Ok(ProcessEvent::Notification { method, params }) => {
                        PeerEvent::Notification { method, params }
                    }
                    Ok(ProcessEvent::Request { id, method, params }) => {
                        match serde_json::from_value(id) {
                            Ok(id) => PeerEvent::Request { id, method, params },
                            Err(cause) => PeerEvent::Closed(AcpError::Transport(cause.to_string())),
                        }
                    }
                    Ok(ProcessEvent::Closed(cause)) => PeerEvent::Closed(error(cause)),
                    Err(cause) => PeerEvent::Closed(AcpError::Transport(format!(
                        "ACP process event continuity was lost: {cause}"
                    ))),
                };
                let closed = matches!(event, PeerEvent::Closed(_));
                if let PeerEvent::Closed(error) = &event {
                    terminal_writer.send_replace(Some(error.clone()));
                }
                let _ = outgoing.send(event);
                if closed {
                    break;
                }
            }
        });
        Ok(Self(Arc::new(Inner {
            process,
            events,
            first_receiver: Mutex::new(Some(first_receiver)),
            terminal,
            forwarding,
        })))
    }
}
fn error(cause: ProcessError) -> AcpError {
    match cause {
        ProcessError::Acp(error) => error.as_ref().clone(),
        ProcessError::Remote {
            code,
            message,
            data,
        } => AcpError::Request(RpcError {
            code,
            message,
            data: Some(data),
        }),
        ProcessError::JsonRpcRemote {
            method,
            request_id,
            code,
            message,
            data,
        } => AcpError::ResponseError {
            method,
            request_id: serde_json::from_value(request_id)
                .expect("validated numeric outgoing request id"),
            error: RpcError {
                code,
                message,
                data,
            },
        },
        ProcessError::EffectCause {
            method,
            request_id,
            cause,
        } => AcpError::ResponseCause {
            method,
            request_id: serde_json::from_value(request_id)
                .expect("validated numeric outgoing request id"),
            cause,
        },
        ProcessError::Timeout { method } => AcpError::Timeout { method },
        ProcessError::Exited { code, pid, message } => AcpError::ProcessExited {
            code: code.map(i64::from),
            pid,
            message,
        },
        ProcessError::Closed(_) => AcpError::Closed,
        cause => AcpError::Transport(cause.to_string()),
    }
}
impl Peer for ProcessPeer {
    fn external_failure(&self) -> BoxFuture<'_, AcpError> {
        Box::pin(async { error(self.0.process.external_failure().await) })
    }
    fn enable_ordered_ingress(&self) {
        self.0.process.enable_ordered_ingress();
    }
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, AcpError>> {
        Box::pin(async move {
            self.0
                .process
                .request(method, params, timeout)
                .await
                .map_err(error)
        })
    }
    fn request_core<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, AcpError>> {
        Box::pin(async move {
            self.0
                .process
                .request_core(method, params, timeout)
                .await
                .map_err(error)
        })
    }
    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            self.0
                .process
                .notify(method, Some(params))
                .await
                .map_err(error)
        })
    }
    fn respond<'a>(
        &'a self,
        id: RequestId,
        result: Result<Value, RpcError>,
    ) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            let id = serde_json::to_value(id).expect("JSON-RPC id serializes");
            match result {
                Ok(value) => self.0.process.respond(id, Ok(value)).await.map_err(error),
                Err(failure) => {
                    // Effect ACP uses the Cause/Fail wrapper even for ordinary
                    // protocol errors. Keep explicit data:null inside the Fail.
                    let cause = t3_acp::EffectCause::new(vec![t3_acp::EffectCauseReason::Fail {
                        error: serde_json::to_value(failure).expect("RPC error serializes"),
                    }]);
                    self.0.process.respond_cause(id, cause).await.map_err(error)
                }
            }
        })
    }
    fn respond_cause<'a>(
        &'a self,
        id: RequestId,
        cause: t3_acp::EffectCause,
    ) -> BoxFuture<'a, Result<(), AcpError>> {
        Box::pin(async move {
            self.0
                .process
                .respond_cause(
                    serde_json::to_value(id).expect("JSON-RPC id serializes"),
                    cause,
                )
                .await
                .map_err(error)
        })
    }
    fn subscribe(&self) -> broadcast::Receiver<PeerEvent> {
        if let Some(receiver) = self.0.first_receiver.lock().unwrap().take() {
            return receiver;
        }
        let terminal = self.0.terminal.borrow();
        if let Some(error) = terminal.as_ref() {
            let (sender, receiver) = broadcast::channel(1);
            let _ = sender.send(PeerEvent::Closed(error.clone()));
            receiver
        } else {
            self.0.events.subscribe()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;
    #[tokio::test]
    async fn standard_rpc_errors_preserve_omitted_and_explicit_null_data_from_actual_child() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("standard-errors.py");
        std::fs::write(&path,"import sys,json\nfor data in [{},{'data':None}]:\n request=json.loads(sys.stdin.readline())\n error={'code':-32000,'message':'fixture detail',**data}\n print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':error}),flush=True)\n").unwrap();
        let peer = ProcessPeer::spawn(ProcessOptions {
            binary: "python3".into(),
            args: vec![path.to_string_lossy().into()],
            cwd: directory.path().into(),
            environment: Default::default(),
        })
        .unwrap();
        for expected in [None, Some(Value::Null)] {
            let AcpError::ResponseError {
                method,
                request_id,
                error,
            } = peer
                .request("probe", Value::Null, Duration::from_secs(3))
                .await
                .unwrap_err()
            else {
                panic!("not a protocol request error")
            };
            assert_eq!(method, "probe");
            assert!(matches!(request_id, RequestId::Number(_)));
            assert_eq!(error.code, -32000);
            assert_eq!(error.data, expected);
            let encoded = serde_json::to_value(error).unwrap();
            assert_eq!(encoded.get("data").cloned(), expected);
        }
    }
    #[tokio::test]
    async fn process_peer_roundtrips_effect_defect_causes_with_original_correlation_and_write_ack()
    {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("effect-cause.py");
        std::fs::write(
            &path,
            r#"import sys,json
request=json.loads(sys.stdin.readline())
assert request['jsonrpc']=='2.0' and 'headers' not in request
print(json.dumps({'jsonrpc':'2.0','id':'callback-0','method':'initialize','params':{}}),flush=True)
reply=json.loads(sys.stdin.readline())
assert reply['jsonrpc']=='2.0' and reply['id']=='callback-0'
error=reply['error']
assert error['_tag']=='Cause' and error['code']==0
assert error['data']==[{'_tag':'Die','defect':{'name':'Error','message':'fixture defect'}}]
assert json.loads(error['message'])==error['data']
print(json.dumps({'jsonrpc':'2.0','id':7,'method':'x/fail','params':{}}),flush=True)
failure=json.loads(sys.stdin.readline())
assert failure['id']==7 and type(failure['id']) is int
assert failure['error']=={'_tag':'Cause','code':-32000,'message':'custom auth','data':[{'_tag':'Fail','error':{'code':-32000,'message':'custom auth','data':None}}]}
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':error}),flush=True)
"#,
        )
        .unwrap();
        let peer = ProcessPeer::spawn(ProcessOptions {
            binary: "python3".into(),
            args: vec![path.to_string_lossy().into()],
            cwd: directory.path().into(),
            environment: Default::default(),
        })
        .unwrap();
        let mut events = peer.subscribe();
        let caller = peer.clone();
        let pending = tokio::spawn(async move {
            caller
                .request("initialize", serde_json::json!({}), Duration::from_secs(3))
                .await
        });
        let PeerEvent::Request { id, .. } =
            tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("missing callback")
        };
        let cause = t3_acp::EffectCause::defect(
            serde_json::json!({"name":"Error","message":"fixture defect"}),
        );
        let mut invalid = cause.clone();
        invalid.code = 9_007_199_254_740_992;
        assert!(peer.respond_cause(id.clone(), invalid).await.is_err());
        peer.respond_cause(id, cause.clone()).await.unwrap();
        let PeerEvent::Request { id, .. } =
            tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("missing extension callback")
        };
        peer.respond(
            id,
            Err(RpcError {
                code: -32000,
                message: "custom auth".into(),
                data: Some(Value::Null),
            }),
        )
        .await
        .unwrap();
        let error = pending.await.unwrap().unwrap_err();
        let AcpError::ResponseCause {
            method,
            request_id,
            cause: received,
        } = error
        else {
            panic!("cause marker was flattened")
        };
        assert_eq!(method, "initialize");
        assert_eq!(
            serde_json::to_value(request_id).unwrap(),
            serde_json::json!(1)
        );
        assert_eq!(
            serde_json::to_value(received).unwrap(),
            serde_json::to_value(cause).unwrap()
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::Closed(AcpError::ProcessExited { code: Some(0), .. })
        ));
    }
    #[tokio::test]
    async fn process_peer_preserves_callback_id_types_and_exit_information() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("acp.py");
        std::fs::write(&path, "import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':{}}),flush=True)\nfor id in [0,'0','$t3:jsonrpc:number:0']:\n print(json.dumps({'jsonrpc':'2.0','id':id,'method':'approve','params':{}}),flush=True)\n a=json.loads(sys.stdin.readline())\n assert type(a['id']) is type(id) and a['id']==id\n assert a['result']['accepted']\nsys.exit(7)\n").unwrap();
        let peer = ProcessPeer::spawn(ProcessOptions {
            binary: "python3".into(),
            args: vec![path.to_string_lossy().into()],
            cwd: directory.path().into(),
            environment: Default::default(),
        })
        .unwrap();
        let mut events = peer.subscribe();
        peer.request("initialize", json!({}), Duration::from_secs(2))
            .await
            .unwrap();
        for expected in [json!(0), json!("0"), json!("$t3:jsonrpc:number:0")] {
            let PeerEvent::Request { id, .. } =
                tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("missing callback")
            };
            assert_eq!(serde_json::to_value(&id).unwrap(), expected);
            peer.respond(id, Ok(json!({"accepted":true})))
                .await
                .unwrap();
        }
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::Closed(AcpError::ProcessExited {
                code: Some(7),
                pid: Some(_),
                ..
            })
        ));
    }
    #[tokio::test]
    async fn startup_notifications_and_exit_are_retained_before_client_subscribes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("immediate.py");
        std::fs::write(&path,"import sys,json\nprint(json.dumps({'jsonrpc':'2.0','method':'startup','params':{'ready':True}}),flush=True)\nsys.exit(6)\n").unwrap();
        let peer = ProcessPeer::spawn(ProcessOptions {
            binary: "python3".into(),
            args: vec![path.to_string_lossy().into()],
            cwd: directory.path().into(),
            environment: Default::default(),
        })
        .unwrap();
        // Wait for the actual forwarder terminal milestone without consuming
        // its first public event receiver. This forces both startup windows.
        let mut terminal = peer.0.terminal.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if terminal.borrow().is_some() {
                    break;
                }
                terminal.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let mut first = peer.subscribe();
        assert!(
            matches!(first.recv().await.unwrap(),PeerEvent::Notification{method,params}if method=="startup"&&params["ready"]==true)
        );
        assert!(matches!(
            first.recv().await.unwrap(),
            PeerEvent::Closed(AcpError::ProcessExited { code: Some(6), .. })
        ));
        let mut late = peer.subscribe();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), late.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::Closed(AcpError::ProcessExited { code: Some(6), .. })
        ));
        assert!(matches!(
            peer.notify("late", json!({})).await,
            Err(AcpError::ProcessExited { code: Some(6), .. })
        ));
    }
    fn ordered_fixture(body: &str) -> (tempfile::TempDir, ProcessPeer, t3_acp::Client) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ordered-acp.py");
        std::fs::write(&path, body).unwrap();
        let peer = ProcessPeer::spawn(ProcessOptions {
            binary: "python3".into(),
            args: vec![path.to_string_lossy().into()],
            cwd: directory.path().into(),
            environment: Default::default(),
        })
        .unwrap();
        let client = t3_acp::Client::new(Arc::new(peer.clone()), Duration::from_secs(10));
        (directory, peer, client)
    }
    async fn held_notification_barrier(
        events: &mut broadcast::Receiver<ProcessEvent>,
    ) -> t3_acp::IngressAcknowledgement {
        tokio::time::timeout(Duration::from_secs(3),async {
            loop {
                if matches!(events.recv().await.unwrap(),ProcessEvent::Notification{method,..} if method=="x/gate") {
                    let ProcessEvent::IngressBarrier{acknowledgement}=events.recv().await.unwrap() else { panic!("missing ordered notification acknowledgement"); };
                    return acknowledgement;
                }
            }
        }).await.unwrap()
    }
    #[tokio::test]
    async fn ordered_actual_child_holds_mixed_request_reply_and_exit_after_notification() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::{mpsc, oneshot};
        for next in ["request", "response", "termination"] {
            let script = format!(
                r#"import sys,json
request=json.loads(sys.stdin.readline())
note={{'jsonrpc':'2.0','method':'x/gate','params':{{}}}}
next='{next}'
frames=[note]
if next=='request': frames.append({{'jsonrpc':'2.0','id':7,'method':'x/callback','params':{{}}}})
if next=='response': frames.append({{'jsonrpc':'2.0','id':request['id'],'result':{{'ok':True}}}})
print('\n'.join(json.dumps(frame) for frame in frames),flush=True)
if next=='termination': sys.exit(7)
if next=='request':
 reply=json.loads(sys.stdin.readline())
 assert reply['id']==7 and reply['result']=={{'ok':True}}
 print(json.dumps({{'jsonrpc':'2.0','id':request['id'],'result':{{'ok':True}}}}),flush=True)
"#
            );
            let (_directory, peer, client) = ordered_fixture(&script);
            let mut process_events = peer.0.process.subscribe();
            let (entered, mut handled) = mpsc::unbounded_channel();
            let (release, held) = oneshot::channel();
            let held = Arc::new(Mutex::new(Some(held)));
            client.handle_unknown_notification(Arc::new(move |_| {
                let entered = entered.clone();
                let held = held.lock().unwrap().take().unwrap();
                Box::pin(async move {
                    entered.send(()).unwrap();
                    held.await.unwrap();
                    Ok(())
                })
            }));
            let called = Arc::new(AtomicUsize::new(0));
            let observed = called.clone();
            client.handle_request(
                "x/callback",
                Arc::new(move |_, _| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(json!({"ok":true})) })
                }),
            );
            let caller = client.clone();
            let pending =
                tokio::spawn(async move { caller.raw_request("x/start", json!({})).await });
            tokio::time::timeout(Duration::from_secs(3), handled.recv())
                .await
                .unwrap()
                .unwrap();
            let barrier = held_notification_barrier(&mut process_events).await;
            {
                let acknowledged = barrier.wait();
                tokio::pin!(acknowledged);
                assert!(futures_util::poll!(acknowledged.as_mut()).is_pending());
            }
            assert!(!pending.is_finished());
            assert_eq!(called.load(Ordering::SeqCst), 0);
            release.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(3), pending)
                .await
                .unwrap()
                .unwrap();
            if next == "termination" {
                assert!(
                    matches!(result, Err(AcpError::ProcessExited { code: Some(7), .. })),
                    "{result:?}"
                );
            } else {
                assert_eq!(result.unwrap(), json!({"ok":true}));
            }
            assert_eq!(
                called.load(Ordering::SeqCst),
                usize::from(next == "request")
            );
        }
    }
    #[tokio::test]
    async fn ordered_actual_child_backpressures_more_than_broadcast_capacity_without_lag() {
        use tokio::sync::{mpsc, oneshot};
        let (_directory, peer, client) = ordered_fixture(
            r#"import sys,json
request=json.loads(sys.stdin.readline())
for index in range(2049):
 print(json.dumps({'jsonrpc':'2.0','method':'x/gate','params':{'index':index}}),flush=True)
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'count':2048}}),flush=True)
"#,
        );
        let mut process_events = peer.0.process.subscribe();
        let (seen, mut values) = mpsc::unbounded_channel();
        let (release, held) = oneshot::channel();
        let held = Arc::new(Mutex::new(Some(held)));
        client.handle_unknown_notification(Arc::new(move |value| {
            let seen = seen.clone();
            let held = held.lock().unwrap().take();
            Box::pin(async move {
                seen.send(value["index"].as_u64().unwrap()).unwrap();
                if let Some(held) = held {
                    held.await.unwrap();
                }
                Ok(())
            })
        }));
        let caller = client.clone();
        let pending = tokio::spawn(async move { caller.raw_request("x/start", json!({})).await });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), values.recv())
                .await
                .unwrap(),
            Some(0)
        );
        let barrier = held_notification_barrier(&mut process_events).await;
        {
            let acknowledged = barrier.wait();
            tokio::pin!(acknowledged);
            assert!(futures_util::poll!(acknowledged.as_mut()).is_pending());
        }
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            for index in 1..=2048 {
                assert_eq!(values.recv().await, Some(index));
            }
            assert_eq!(pending.await.unwrap().unwrap(), json!({"count":2048}));
        })
        .await
        .unwrap();
        let raw = client.notifications();
        for index in 2017..=2048 {
            let Some(t3_acp::IncomingNotification::ExtNotification { params, .. }) = raw.try_recv()
            else {
                panic!("missing retained notification");
            };
            assert_eq!(params["index"], index);
        }
        assert!(raw.try_recv().is_none());
    }
    #[tokio::test]
    async fn ordered_actual_child_preserves_pre_admitted_core_callback_concurrency() {
        use tokio::sync::{mpsc, oneshot};
        let (_directory, _peer, client) = ordered_fixture(
            r#"import sys,json
request=json.loads(sys.stdin.readline())
frames=[{'jsonrpc':'2.0','id':7,'method':'fs/read_text_file','params':{'sessionId':'s','path':'/test'}},{'jsonrpc':'2.0','method':'x/gate','params':{}}]
print('\n'.join(json.dumps(frame) for frame in frames),flush=True)
reply=json.loads(sys.stdin.readline())
assert reply['id']==7 and reply['result']=={'content':'done'}
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'done':True}}),flush=True)
"#,
        );
        let (callback_entered, mut callbacks) = mpsc::unbounded_channel();
        let (finish_callback, callback_gate) = oneshot::channel();
        let callback_gate = Arc::new(Mutex::new(Some(callback_gate)));
        client.handle_request(
            "fs/read_text_file",
            Arc::new(move |_, _| {
                let callback_entered = callback_entered.clone();
                let held = callback_gate.lock().unwrap().take().unwrap();
                Box::pin(async move {
                    callback_entered.send(()).unwrap();
                    held.await.unwrap();
                    Ok(json!({"content":"done"}))
                })
            }),
        );
        let (notification_entered, mut notifications) = mpsc::unbounded_channel();
        let (finish_notification, notification_gate) = oneshot::channel();
        let notification_gate = Arc::new(Mutex::new(Some(notification_gate)));
        client.handle_unknown_notification(Arc::new(move |_| {
            let notification_entered = notification_entered.clone();
            let held = notification_gate.lock().unwrap().take().unwrap();
            Box::pin(async move {
                notification_entered.send(()).unwrap();
                held.await.unwrap();
                Ok(())
            })
        }));
        let mut events = client.subscribe();
        let caller = client.clone();
        let pending = tokio::spawn(async move { caller.raw_request("x/start", json!({})).await });
        tokio::time::timeout(Duration::from_secs(3), callbacks.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        finish_callback.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let t3_acp::ClientEvent::ResponseAcknowledged { request_id } =
                    events.recv().await.unwrap()
                {
                    assert_eq!(request_id, "$t3:jsonrpc:number:7");
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(!pending.is_finished());
        finish_notification.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            json!({"done":true})
        );
    }
    #[tokio::test]
    async fn dropping_ordered_connection_cancels_ack_wait_and_reaps_owned_child() {
        use tokio::sync::{mpsc, oneshot};
        let (_directory, peer, client) = ordered_fixture(
            r#"import sys,json,threading
request=json.loads(sys.stdin.readline())
print(json.dumps({'jsonrpc':'2.0','method':'x/gate','params':{}}),flush=True)
threading.Event().wait()
"#,
        );
        let mut process_events = peer.0.process.subscribe();
        let (entered, mut handled) = mpsc::unbounded_channel();
        let (_release, held) = oneshot::channel::<()>();
        let held = Arc::new(Mutex::new(Some(held)));
        client.handle_unknown_notification(Arc::new(move |_| {
            let entered = entered.clone();
            let held = held.lock().unwrap().take().unwrap();
            Box::pin(async move {
                entered.send(()).unwrap();
                let _ = held.await;
                Ok(())
            })
        }));
        let caller = client.clone();
        let pending = tokio::spawn(async move { caller.raw_request("x/start", json!({})).await });
        tokio::time::timeout(Duration::from_secs(3), handled.recv())
            .await
            .unwrap()
            .unwrap();
        let _unreleased_barrier = held_notification_barrier(&mut process_events).await;
        client.shutdown();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), pending)
                .await
                .unwrap()
                .unwrap(),
            Err(AcpError::Closed)
        ));
        drop(client);
        drop(peer);
        // Closed is published only after the owner kills and waits for its child.
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), process_events.recv())
                .await
                .unwrap()
                .unwrap(),
            ProcessEvent::Closed(ProcessError::Closed(_))
        ));
    }
    #[tokio::test]
    async fn typed_core_and_raw_extension_requests_have_original_disjoint_wire_ids() {
        let (_directory, _peer, client) = ordered_fixture(
            r#"import sys,json
for expected in [4294967296,1,4294967297,2]:
 request=json.loads(sys.stdin.readline())
 assert request['id']==expected,request
 result={'protocolVersion':2,'info':{'name':'fixture','version':'1'}} if request['method']=='initialize' else {}
 print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#,
        );
        client
            .initialize(json!({"protocolVersion":2,"clientInfo":{"name":"test","version":"1"}}))
            .await
            .unwrap();
        client.raw_request("x/first", json!({})).await.unwrap();
        client
            .call(t3_acp::AgentMethod::Logout, json!({}))
            .await
            .unwrap();
        client.raw_request("x/second", json!({})).await.unwrap();
    }
    #[tokio::test]
    async fn actual_writer_failure_escapes_held_incoming_observer_and_fails_pending_before_termination_hook()
     {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("held-observer.py");
        std::fs::write(
            &path,
            r#"import sys,json,os,threading
request=json.loads(sys.stdin.readline())
os.close(0)
print(json.dumps({'jsonrpc':'2.0','id':7,'method':'x/held','params':{}}),flush=True)
threading.Event().wait()
"#,
        )
        .unwrap();
        let peer = ProcessPeer::spawn(ProcessOptions {
            binary: "python3".into(),
            args: vec![path.to_string_lossy().into()],
            cwd: directory.path().into(),
            environment: Default::default(),
        })
        .unwrap();
        let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
        let (terminated, mut terminations) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let gate = Arc::new(Mutex::new(Some(gate)));
        let client = t3_acp::Client::with_options(
            Arc::new(peer.clone()),
            Duration::from_secs(10),
            t3_acp::ClientOptions {
                on_incoming_request: Some(Arc::new(move |_, _| {
                    let entered = entered.clone();
                    let gate = gate.lock().unwrap().take().unwrap();
                    Box::pin(async move {
                        entered.send(()).unwrap();
                        let _ = gate.await;
                    })
                })),
                on_termination: Some(Arc::new(move |error| {
                    let terminated = terminated.clone();
                    Box::pin(async move {
                        terminated.send(error).unwrap();
                    })
                })),
                ..Default::default()
            },
        );
        let caller = client.clone();
        let pending = tokio::spawn(async move { caller.raw_request("x/pending", json!({})).await });
        tokio::time::timeout(Duration::from_secs(3), entries.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            client
                .raw_notify("x/trigger-broken-writer", json!({}))
                .await
                .is_err()
        );
        let failure = tokio::time::timeout(Duration::from_secs(3), terminations.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(failure, AcpError::Failure(failure) if matches!(failure.as_ref(),t3_acp::errors::Failure::Transport(_)))
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(3), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            release.send(()).is_err(),
            "held observer must be canceled without release"
        );
        assert!(terminations.try_recv().is_err());
        let mut closed = peer.0.terminal.subscribe();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if closed.borrow().is_some() {
                    break;
                }
                closed.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(client.raw_request("x/future", json!({})).await.is_err());
    }
    fn option_fixture(
        body: &str,
        options: t3_acp::transport::ProtocolOptions,
    ) -> (tempfile::TempDir, ProcessPeer) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("option-acp.py");
        std::fs::write(&path, body).unwrap();
        let peer = ProcessPeer::spawn_with_options(
            ProcessOptions {
                binary: "python3".into(),
                args: vec![path.to_string_lossy().into()],
                cwd: directory.path().into(),
                environment: Default::default(),
            },
            options,
        )
        .unwrap();
        (directory, peer)
    }
    fn strip_startup_notice() -> t3_acp::transport::StdoutTransform {
        Arc::new(|stream| {
            let fragmented = stream.flat_map(|item| {
                futures_util::stream::iter(match item {
                    Ok(bytes) => bytes
                        .into_iter()
                        .map(|byte| Ok(vec![byte]))
                        .collect::<Vec<_>>(),
                    Err(error) => vec![Err(error)],
                })
            });
            let mut startup = true;
            let mut banner = Vec::new();
            Box::pin(fragmented.filter_map(move |item| {
                let output = match item {
                    Ok(bytes) if startup => {
                        banner.extend_from_slice(&bytes);
                        if banner.last() == Some(&b'\n') {
                            assert_eq!(banner, b"startup notice\n");
                            startup = false;
                        }
                        None
                    }
                    value => Some(value),
                };
                futures_util::future::ready(output)
            }))
        })
    }
    #[tokio::test]
    async fn startup_stdout_transform_filters_notice_and_preserves_fragmented_unicode_before_logging_and_decode()
     {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let recorder = logs.clone();
        let (_directory, peer) = option_fixture(
            r#"import sys,json
print('startup notice',flush=True)
for method in ['initialize','x/echo']:
 request=json.loads(sys.stdin.readline())
 assert request['method']==method and 'headers' not in request,request
 result={'protocolVersion':2,'info':{'name':'fixture','version':'1'}} if method=='initialize' else request['params']
 print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result},ensure_ascii=False),flush=True)
"#,
            t3_acp::transport::ProtocolOptions {
                log_incoming: true,
                log_outgoing: true,
                logger: Some(Arc::new(move |event| {
                    let recorder = recorder.clone();
                    Box::pin(async move {
                        recorder.lock().unwrap().push(event);
                    })
                })),
                transform_stdout: Some(strip_startup_notice()),
            },
        );
        let client = t3_acp::Client::new(Arc::new(peer), Duration::from_secs(10));
        client
            .initialize(json!({"protocolVersion":2,"clientInfo":{"name":"test","version":"1"}}))
            .await
            .unwrap();
        let payload = json!({"message":"café 😀"});
        assert_eq!(
            client.raw_request("x/echo", payload.clone()).await.unwrap(),
            payload
        );
        let logs = logs.lock().unwrap();
        assert!(
            logs.iter()
                .all(|event| !event.payload.to_string().contains("startup notice"))
        );
        let incoming: Vec<_> = logs
            .iter()
            .filter(|event| {
                event.direction == t3_acp::transport::LogDirection::Incoming
                    && event.stage == t3_acp::transport::LogStage::Decoded
            })
            .collect();
        assert!(
            incoming
                .iter()
                .any(|event| event.payload.pointer("/0/exit/value/message")
                    == Some(&json!("café 😀")))
        );
        let outgoing: Vec<_> = logs
            .iter()
            .filter(|event| event.direction == t3_acp::transport::LogDirection::Outgoing)
            .collect();
        assert_eq!(outgoing.len(), 4);
        assert_eq!(outgoing[0].stage, t3_acp::transport::LogStage::Decoded);
        assert_eq!(outgoing[0].payload["headers"], json!([]));
        assert_eq!(outgoing[1].stage, t3_acp::transport::LogStage::Raw);
        assert!(
            outgoing[1]
                .payload
                .as_str()
                .unwrap()
                .starts_with("{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":")
        );
        assert_eq!(
            outgoing[3].payload,
            json!(
                "{\"jsonrpc\":\"2.0\",\"method\":\"x/echo\",\"params\":{\"message\":\"café 😀\"},\"id\":1}\n"
            )
        );
    }
    #[tokio::test]
    async fn malformed_transformed_stdout_retains_typed_parse_error_and_private_decode_failure_log()
    {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let recorder = logs.clone();
        let (_directory, peer) = option_fixture(
            "import threading\nprint('startup notice',flush=True)\nprint('{\"secret\":\"private-token\"',flush=True)\nthreading.Event().wait()\n",
            t3_acp::transport::ProtocolOptions {
                log_incoming: true,
                logger: Some(Arc::new(move |event| {
                    let recorder = recorder.clone();
                    Box::pin(async move {
                        recorder.lock().unwrap().push(event);
                    })
                })),
                transform_stdout: Some(strip_startup_notice()),
                ..Default::default()
            },
        );
        let client = t3_acp::Client::new(Arc::new(peer), Duration::from_secs(10));
        let failure = client
            .raw_request("x/pending", json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(failure,AcpError::Failure(failure) if matches!(failure.as_ref(),t3_acp::errors::Failure::ProtocolParse(error) if error.operation==t3_acp::errors::ProtocolParseOperation::DecodeWireMessage))
        );
        let logs = logs.lock().unwrap();
        let safe: Vec<_> = logs
            .iter()
            .filter(|event| event.stage == t3_acp::transport::LogStage::DecodeFailed)
            .collect();
        assert_eq!(safe.len(), 1);
        assert_eq!(safe[0].payload, json!({"operation":"decode-wire-message"}));
        assert!(!safe[0].payload.to_string().contains("private-token"));
        assert!(
            logs.iter()
                .all(|event| !event.payload.to_string().contains("startup notice"))
        );
    }
    #[tokio::test]
    async fn stdout_transform_declared_failure_preserves_error_identity_and_never_logs_rejected_private_input()
     {
        let expected: AcpError = t3_acp::errors::TransportError {
            operation: Some(t3_acp::errors::TransportOperation::ReadInputStream),
            method: None,
            detail: Some("Sign in to the fixture agent.".into()),
            pid: None,
            cause: t3_acp::errors::FailureCause::Value(Value::Null),
        }
        .into();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let recorder = logs.clone();
        let rejected = expected.clone();
        let (terminated, mut terminations) = tokio::sync::mpsc::unbounded_channel();
        let (_directory, peer) = option_fixture(
            "import threading\nprint('sign-in https://example.test/?token=private-secret',flush=True)\nthreading.Event().wait()\n",
            t3_acp::transport::ProtocolOptions {
                log_incoming: true,
                logger: Some(Arc::new(move |event| {
                    let recorder = recorder.clone();
                    Box::pin(async move {
                        recorder.lock().unwrap().push(event);
                    })
                })),
                transform_stdout: Some(Arc::new(move |stream| {
                    let rejected = rejected.clone();
                    Box::pin(stream.map(move |_| Err(rejected.clone())))
                })),
                ..Default::default()
            },
        );
        let client = t3_acp::Client::with_options(
            Arc::new(peer),
            Duration::from_secs(10),
            t3_acp::ClientOptions {
                on_termination: Some(Arc::new(move |error| {
                    let terminated = terminated.clone();
                    Box::pin(async move {
                        terminated.send(error).unwrap();
                    })
                })),
                ..Default::default()
            },
        );
        let failure = client
            .raw_request("x/pending", json!({}))
            .await
            .unwrap_err();
        let termination = tokio::time::timeout(Duration::from_secs(3), terminations.recv())
            .await
            .unwrap()
            .unwrap();
        let (
            AcpError::Failure(expected),
            AcpError::Failure(failure),
            AcpError::Failure(termination),
        ) = (expected, failure, termination)
        else {
            panic!("supplied failure was flattened");
        };
        assert!(Arc::ptr_eq(&expected, &failure));
        assert!(Arc::ptr_eq(&expected, &termination));
        assert!(logs.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn raw_logger_paused_send_is_rejected_after_eof_without_crossing_the_wire() {
        for request in [true, false] {
            let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
            let (terminated, mut terminations) = tokio::sync::mpsc::unbounded_channel();
            let (release, gate) = tokio::sync::oneshot::channel();
            let gate = Arc::new(Mutex::new(Some(gate)));
            let (_directory, peer) = option_fixture(
                "import sys,json\nrequest=json.loads(sys.stdin.readline())\nassert request['method']=='x/finish'\n",
                t3_acp::transport::ProtocolOptions {
                    log_outgoing: true,
                    logger: Some(Arc::new(move |event| {
                        let entered = entered.clone();
                        let gate = gate.clone();
                        Box::pin(async move {
                            if event.stage == t3_acp::transport::LogStage::Raw
                                && event
                                    .payload
                                    .as_str()
                                    .is_some_and(|wire| wire.contains("x/held"))
                            {
                                let gate = gate.lock().unwrap().take().unwrap();
                                entered.send(()).unwrap();
                                let _ = gate.await;
                            }
                        })
                    })),
                    ..Default::default()
                },
            );
            let client = t3_acp::Client::with_options(
                Arc::new(peer.clone()),
                Duration::from_secs(10),
                t3_acp::ClientOptions {
                    on_termination: Some(Arc::new(move |error| {
                        let terminated = terminated.clone();
                        Box::pin(async move {
                            terminated.send(error).unwrap();
                        })
                    })),
                    ..Default::default()
                },
            );
            let caller = client.clone();
            let sending = tokio::spawn(async move {
                if request {
                    caller.raw_request("x/held", json!({})).await.map(|_| ())
                } else {
                    caller.raw_notify("x/held", json!({})).await
                }
            });
            tokio::time::timeout(Duration::from_secs(3), entries.recv())
                .await
                .unwrap()
                .unwrap();
            // This independently admitted message is the only line the child
            // accepts. Any held send that leaked onto stdin would fail its check.
            peer.notify("x/finish", json!({})).await.unwrap();
            let error = tokio::time::timeout(Duration::from_secs(3), terminations.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                error,
                AcpError::ProcessExited { code: Some(0), .. }
            ));
            let _ = release.send(());
            assert!(
                tokio::time::timeout(Duration::from_secs(3), sending)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            assert!(terminations.try_recv().is_err());
        }
    }
    #[tokio::test]
    async fn actual_writer_failure_rejects_current_queued_and_future_responses_with_one_preserved_cause()
     {
        let (_directory, peer) = option_fixture(
            r#"import sys,json,os,threading
request=json.loads(sys.stdin.readline())
os.close(0)
print(json.dumps({'jsonrpc':'2.0','method':'x/ready','params':{}}),flush=True)
threading.Event().wait()
"#,
            Default::default(),
        );
        let mut ingress = peer.subscribe();
        peer.notify("x/start", json!({})).await.unwrap();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(3),ingress.recv()).await.unwrap().unwrap(),PeerEvent::Notification{method,..} if method=="x/ready")
        );
        // Current-thread join admits all three responses before the writer runs.
        let (a, b, c) = tokio::join!(
            peer.respond(RequestId::Number(1.into()), Ok(json!({}))),
            peer.respond(RequestId::Number(2.into()), Ok(json!({}))),
            peer.respond(RequestId::Number(3.into()), Ok(json!({})))
        );
        let errors = [
            a.unwrap_err(),
            b.unwrap_err(),
            c.unwrap_err(),
            peer.respond(RequestId::Number(4.into()), Ok(json!({})))
                .await
                .unwrap_err(),
        ];
        let AcpError::Failure(first) = &errors[0] else {
            panic!("writer failure category lost");
        };
        assert!(
            matches!(first.as_ref(),t3_acp::errors::Failure::Transport(error) if error.detail.as_deref()==Some("Failed to write an outgoing ACP message"))
        );
        for error in &errors[1..] {
            let AcpError::Failure(cause) = error else {
                panic!("queued error category lost");
            };
            assert!(Arc::ptr_eq(first, cause));
        }
    }
}
