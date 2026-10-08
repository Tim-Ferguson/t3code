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
        let process = ProviderProcess::spawn_json_rpc(options).map_err(error)?;
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
        ProcessError::Remote {
            code,
            message,
            data,
        } => AcpError::Request(RpcError {
            code,
            message,
            data: Some(data),
        }),
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
            let result = result.map_err(|cause| ProcessError::Remote {
                code: cause.code,
                message: cause.message,
                data: cause.data.unwrap_or(Value::Null),
            });
            self.0
                .process
                .respond(
                    serde_json::to_value(id).expect("JSON-RPC id serializes"),
                    result,
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
    use serde_json::json;
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
}
