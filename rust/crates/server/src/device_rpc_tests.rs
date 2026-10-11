//! Real device service, owned helper process and authenticated Effect transport.
use crate::test_support::HANG_BOUND;
use crate::{
    auth::AuthService,
    persistence::Store,
    transport::{ApiState, router},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::{AuthEnvironmentScope, DeviceServiceState, DeviceSession, RpcServerMessage};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}
async fn send(socket: &mut Socket, id: u64, tag: &str, payload: Value) {
    socket
        .send(Message::Text(
            json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}
async fn next(socket: &mut Socket) -> Value {
    let frame = socket.next().await.unwrap().unwrap();
    let value: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    let _: RpcServerMessage = serde_json::from_value(value.clone()).unwrap();
    value
}
#[tokio::test]
async fn local_device_rpc_preserves_consent_dynamic_scopes_typed_sessions_and_stop_while_ack_held()
{
    tokio::time::timeout(HANG_BOUND,async {
        let root=tempfile::tempdir().unwrap();let fixture=crate::device_service::tests::fixture(root.path()).await;
        let store=Store::memory().unwrap();let auth=AuthService::new(store.clone(),[44;32],"device_service_fixture".into(),"loopback-browser".into()).unwrap();
        let grant=|scopes|auth.issue_session("fixture","bearer-access-token",scopes,json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let reader=grant(vec![AuthEnvironmentScope::OrchestrationRead]);
        let writer=grant(vec![AuthEnvironmentScope::OrchestrationRead,AuthEnvironmentScope::OrchestrationOperate,AuthEnvironmentScope::SettingsWrite]);
        let state=ApiState{store,auth,environment:json!({"environmentId":"device-owner-fixture","label":"Device service fixture","platform":{"os":"darwin","arch":"arm64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),config:None,settings:Some(fixture.settings.clone()),device_hosts:None,devices:Some(fixture.service.clone()),provider_auth:None,background:None,cors_origins:None,assets:None,providers:None,execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
        let (stop,stopped)=tokio::sync::oneshot::channel();let mut server=tokio::task::JoinSet::new();
        server.spawn(async move{axum::serve(listener,router(state)).with_graceful_shutdown(async{let _=stopped.await;}).await.unwrap();});
        let mut read=connect(address,&reader).await;
        for (id,payload) in [(1,json!({"updateTool":"hub"})),(2,json!({"retryHostId":"local"})),(7,json!({"inspectOnly":true,"updateTool":"hub"}))] {
            send(&mut read,id,"device.list",payload).await;
            let denied=next(&mut read).await;assert_eq!(denied["exit"]["cause"][0]["error"]["requiredPermission"],"orchestration:operate");
        }
        // Source inspection bypasses retry admission when updateTool is absent.
        send(&mut read,3,"device.list",json!({"inspectOnly":true,"retryHostId":"local"})).await;
        let value=next(&mut read).await;let snapshot:DeviceServiceState=serde_json::from_value(value["exit"]["value"].clone()).unwrap();assert_eq!(snapshot.host_status,t3_contracts::DeviceHostStatus::Disabled);assert!(value["exit"]["value"]["hosts"][0]["tools"]["hub"]["runningVersion"].is_null());
        send(&mut read,4,"device.configure",json!({"enabled":true})).await;
        assert_eq!(next(&mut read).await["exit"]["cause"][0]["error"]["requiredPermission"],"settings:write");
        send(&mut read,5,"device.open",json!({"threadId":"thread-a","deviceId":"fixture-ios","platform":"ios"})).await;
        assert_eq!(next(&mut read).await["exit"]["cause"][0]["error"]["requiredPermission"],"orchestration:operate");
        assert!(!fixture.settings.snapshot().await.unwrap().enable_device_support);
        let mut write=connect(address,&writer).await;
        send(&mut write,1,"device.configure",json!({"enabled":"bad"})).await;
        assert_eq!(next(&mut write).await["exit"]["cause"][0]["_tag"],"Die");assert!(!fixture.settings.snapshot().await.unwrap().enable_device_support);
        send(&mut read,6,"subscribeDeviceState",json!({})).await;
        let initial=next(&mut read).await;assert_eq!(initial["_tag"],"Chunk");let _:DeviceServiceState=serde_json::from_value(initial["values"][0].clone()).unwrap();
        send(&mut write,2,"device.configure",json!({"enabled":true})).await;let value=next(&mut write).await;let _:DeviceServiceState=serde_json::from_value(value["exit"]["value"].clone()).unwrap();
        let hub_pid=fixture.service.current_readiness(None).unwrap().pid;
        send(&mut read,35,"device.configure",json!({"agentAccessEnabled":true})).await;assert_eq!(next(&mut read).await["exit"]["cause"][0]["error"]["requiredPermission"],"settings:write");assert!(!root.path().join("agent-calls.jsonl").exists());
        send(&mut write,35,"device.configure",json!({"agentAccessEnabled":true})).await;let value=next(&mut write).await;let state:DeviceServiceState=serde_json::from_value(value["exit"]["value"].clone()).unwrap();assert!(state.agent_access_enabled);assert_eq!(fixture.service.agent_readiness_if_supported(None).await.unwrap().unwrap().agent_device.base_url,"http://127.0.0.1:12345");
        send(&mut write,36,"device.configure",json!({"agentAccessEnabled":false})).await;let value=next(&mut write).await;assert_eq!(value["exit"]["value"]["agentAccessEnabled"],false);assert_eq!(fixture.service.current_readiness(None).unwrap().pid,hub_pid);

        read.send(Message::Text(json!({"_tag":"Ping"}).to_string().into())).await.unwrap();assert_eq!(next(&mut read).await["_tag"],"Pong");
        send(&mut write,3,"device.open",json!({"threadId":"thread-a","deviceId":"fixture-ios","platform":"ios"})).await;
        let value=next(&mut write).await;let session:DeviceSession=serde_json::from_value(value["exit"]["value"].clone()).unwrap();assert_eq!(session.thread_id.as_str(),"thread-a");
        assert_eq!(fixture.service.sessions_for_thread(&session.thread_id).len(),1);
        send(&mut read,8,"device.detail",json!({"deviceId":"fixture-ios"})).await;
        let detail=next(&mut read).await;let detail:t3_contracts::DeviceDetail=serde_json::from_value(detail["exit"]["value"].clone()).unwrap();assert_eq!(serde_json::to_value(&detail.settings).unwrap()["appearance"],"light");assert_eq!(serde_json::to_value(&detail.settings).unwrap()["reduceMotion"],true);
        send(&mut read,9,"device.action",json!({"type":"setAppearance","deviceId":"fixture-ios","value":"dark"})).await;
        assert_eq!(next(&mut read).await["exit"]["cause"][0]["error"]["requiredPermission"],"orchestration:operate");
        send(&mut write,4,"device.action",json!({"type":"setAppearance","deviceId":"fixture-ios","value":"dark"})).await;
        let detail=next(&mut write).await;let detail:t3_contracts::DeviceDetail=serde_json::from_value(detail["exit"]["value"].clone()).unwrap();assert_eq!(serde_json::to_value(detail.settings).unwrap()["appearance"],"dark");
        send(&mut write,5,"device.action",json!({"type":"sendPush","deviceId":"fixture-ios","appId":"fixture.app","payload":{"aps":{"alert":"fixture 👋"}}})).await;
        let detail=next(&mut write).await;let _:t3_contracts::DeviceDetail=serde_json::from_value(detail["exit"]["value"].clone()).unwrap();
        let commands:Value=serde_json::from_slice(&tokio::fs::read(root.path().join("commands.json")).await.unwrap()).unwrap();
        let push=commands["calls"].as_array().unwrap().iter().find(|call|call["args"][1]=="push").unwrap();assert_eq!(push["stdin"],"{\"aps\":{\"alert\":\"fixture 👋\"}}");assert_eq!(push["args"],json!(["simctl","push","fixture-ios","fixture.app","-"]));
        tokio::fs::write(root.path().join("commands.json"),b"{\"fail\":true}").await.unwrap();
        send(&mut write,6,"device.action",json!({"type":"setAppearance","deviceId":"fixture-ios","value":"light"})).await;
        let error=next(&mut write).await;let _:t3_contracts::DeviceError=serde_json::from_value(error["exit"]["cause"][0]["error"].clone()).unwrap();assert_eq!(error["exit"]["cause"][0]["error"]["exitCode"],7);
        send(&mut read,10,"device.detail",json!({"deviceId":"fixture-ios"})).await;
        let detail=next(&mut read).await;assert_eq!(detail["exit"]["value"]["settings"],json!({}));
        send(&mut read,11,"device.detail",json!({"deviceId":"missing-device"})).await;
        let missing=next(&mut read).await;assert_eq!(missing["exit"]["cause"][0]["error"]["_tag"],"DeviceNotFoundError");
        send(&mut write,7,"device.action",json!({"type":"shake","deviceId":"fixture-ios"})).await;
        let unsupported=next(&mut write).await;assert_eq!(unsupported["exit"]["cause"][0]["error"]["reason"],"unsupported");

        loop {
            read.send(Message::Text(json!({"_tag":"Ack","requestId":6}).to_string().into())).await.unwrap();
            let event=next(&mut read).await;let snapshot:DeviceServiceState=serde_json::from_value(event["values"][0].clone()).unwrap();
            if snapshot.sessions==vec![session.clone()]{break;}
        }
        // Leave final chunk unacknowledged; shutdown must terminate the stream.
        fixture.service.shutdown().await;
        let exit=next(&mut read).await;assert_eq!(exit["_tag"],"Exit");assert_eq!(exit["exit"]["_tag"],"Success");
        read.send(Message::Text(json!({"_tag":"Ping"}).to_string().into())).await.unwrap();assert_eq!(next(&mut read).await["_tag"],"Pong");
        read.close(None).await.unwrap();write.close(None).await.unwrap();stop.send(()).unwrap();server.join_next().await.unwrap().unwrap();fixture.settings.shutdown().await;
    }).await.unwrap();
}

/// `device.testHost`: a host that resolves to this machine is summarized as the local host without
/// a connection; a remote one fails the way a probe does (this server has no SSH probe), and a
/// session without `settings:write` is refused before anything is resolved.
#[cfg(unix)]
#[tokio::test]
async fn device_test_host_summarizes_the_local_host_and_refuses_what_it_cannot_probe() {
    use crate::device_host_resolver::{DeviceHostResolver, DeviceHostResolverOptions};
    use std::os::unix::fs::PermissionsExt;
    tokio::time::timeout(HANG_BOUND, async {
        let root = tempfile::tempdir().unwrap();
        let fixture = crate::device_service::tests::fixture(root.path()).await;
        // `ssh -G` answers for the alias: `self` is this machine, anything else is elsewhere.
        let ssh = root.path().join("ssh-fake");
        std::fs::write(&ssh, "#!/bin/sh\nfor last; do :; done\ncase \"$last\" in self) echo 'hostname 100.65.180.100';; *) echo 'hostname 192.0.2.1';; esac\necho 'port 22'\n").unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let resolver = DeviceHostResolver::new(DeviceHostResolverOptions { ssh_binary: ssh, local_addresses: std::sync::Arc::new(|| std::collections::HashSet::from(["100.65.180.100".into()])), lookup: std::sync::Arc::new(|_| Box::pin(async { vec![] })), ..Default::default() });
        let store = Store::memory().unwrap();
        let auth = AuthService::new(store.clone(), [45; 32], "device_test_host".into(), "loopback-browser".into()).unwrap();
        let grant = |scopes| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
        let reader = grant(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
        let writer = grant(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SettingsWrite]);
        let state = ApiState { store, auth, environment: json!({"environmentId":"device-test-host","label":"Fixture","platform":{"os":"darwin","arch":"arm64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}), config: None, settings: Some(fixture.settings.clone()), device_hosts: Some(resolver.clone()), devices: Some(fixture.service.clone()), provider_auth: None, background: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None, discovery: None, resource_telemetry: None, host_resources: None };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move { axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap(); });
        let host = |target: &str| json!({"id":format!("host-{target}"),"label":target,"target":target});
        let mut denied = connect(address, &reader).await;
        send(&mut denied, 1, "device.testHost", host("self")).await;
        assert_eq!(next(&mut denied).await["exit"]["cause"][0]["error"]["requiredPermission"], "settings:write");
        let mut socket = connect(address, &writer).await;
        send(&mut socket, 1, "device.testHost", host("self")).await;
        let local = next(&mut socket).await;
        assert_eq!(local["exit"]["_tag"], "Success", "{local}");
        assert_eq!(local["exit"]["value"]["kind"], "local");
        let _: t3_contracts::DeviceHostSummary = serde_json::from_value(local["exit"]["value"].clone()).unwrap();
        send(&mut socket, 2, "device.testHost", host("remote")).await;
        let remote = next(&mut socket).await;
        assert_eq!(remote["exit"]["cause"][0]["error"]["_tag"], "DeviceOperationError");
        assert_eq!((remote["exit"]["cause"][0]["error"]["operation"].as_str(), remote["exit"]["cause"][0]["error"]["reason"].as_str()), (Some("probe host"), Some("request_failed")));
        send(&mut socket, 3, "device.testHost", json!({"id":"x"})).await;
        assert_eq!(next(&mut socket).await["exit"]["cause"][0]["_tag"], "Die");
        socket.close(None).await.unwrap();
        denied.close(None).await.unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
        resolver.shutdown().await;
    })
    .await
    .unwrap();
}
