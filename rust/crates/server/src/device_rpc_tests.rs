//! Real device service, owned helper process and authenticated Effect transport.
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
    tokio::time::timeout(std::time::Duration::from_secs(30),async {
        let root=tempfile::tempdir().unwrap();let fixture=crate::device_service::tests::fixture(root.path()).await;
        let store=Store::memory().unwrap();let auth=AuthService::new(store.clone(),[44;32],"device_service_fixture".into(),"loopback-browser".into()).unwrap();
        let grant=|scopes|auth.issue_session("fixture","bearer-access-token",scopes,json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let reader=grant(vec![AuthEnvironmentScope::OrchestrationRead]);
        let writer=grant(vec![AuthEnvironmentScope::OrchestrationRead,AuthEnvironmentScope::OrchestrationOperate,AuthEnvironmentScope::SettingsWrite]);
        let state=ApiState{store,auth,environment:json!({"environmentId":"device-owner-fixture","label":"Device service fixture","platform":{"os":"darwin","arch":"arm64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),config:None,settings:Some(fixture.settings.clone()),device_hosts:None,devices:Some(fixture.service.clone()),background:None,cors_origins:None,assets:None,providers:None,execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None};
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
