//! Real socket admission, activity identity, and scoped cleanup for background policy.
use crate::{
    auth::AuthService,
    background_policy::BackgroundPolicy,
    desktop_telemetry::{Clock, DesktopTelemetryOptions, DesktopTelemetryReceiver},
    persistence::Store,
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
    transport::{ApiState, router},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::*;
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
async fn next(socket: &mut Socket) -> Value {
    let message = socket.next().await.unwrap().unwrap();
    let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    serde_json::from_value::<RpcServerMessage>(value.clone()).unwrap();
    value
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
async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    send(socket, id, tag, payload).await;
    let response = next(socket).await;
    assert_eq!(response["requestId"], id);
    response
}
async fn ack(socket: &mut Socket, id: u64) {
    socket
        .send(Message::Text(
            json!({"_tag":"Ack","requestId":id}).to_string().into(),
        ))
        .await
        .unwrap();
}
fn report() -> Value {
    json!({"clientId":"shared-client","clientKind":"web","visible":true,"focused":true,"recentlyInteracted":false,"scopes":[{"type":"diagnostics"}],"observedAt":"2000-01-01T00:00:00.000Z","sessionId":"spoofed-session","rpcClientId":9000})
}
#[tokio::test]
async fn background_socket_trusts_session_and_connection_enforces_power_scope_and_cleans_up_own_leases()
 {
    tokio::time::timeout(std::time::Duration::from_secs(15),async {
        let directory=tempfile::tempdir().unwrap();let secrets=ServerSecretStore::open(directory.path().join("secrets")).unwrap();
        let mut options=SettingsOptions::file(directory.path().join("settings.json"),secrets);options.watch=false;
        let settings=SettingsService::start(options).await.unwrap();
        let clock:Clock=std::sync::Arc::new(||1791417600000);
        let mut options=DesktopTelemetryOptions::unavailable("web");options.clock=clock.clone();
        let desktop=DesktopTelemetryReceiver::new(options).await;
        let policy=BackgroundPolicy::start(settings.clone(),desktop.clone(),clock).await.unwrap();
        let store=Store::memory().unwrap();let auth=AuthService::new(store.clone(),[62;32],"fixture_background_session".into(),"loopback-browser".into()).unwrap();
        let (session,reader)=auth.issue_session("fixture","bearer-access-token",vec![AuthEnvironmentScope::OrchestrationRead],json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap();
        let maintainer=auth.issue_session("fixture","bearer-access-token",vec![AuthEnvironmentScope::EnvironmentMaintain],json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let state=ApiState{store,auth,environment:json!({"environmentId":"background-fixture","label":"Background fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
            config:None,settings:Some(settings.clone()),cors_origins:None,assets:None,providers:None,execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None,background:Some(policy.clone()),device_hosts:None};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
        let(stop,stopped)=tokio::sync::oneshot::channel();let mut tasks=tokio::task::JoinSet::new();
        tasks.spawn(async move {axum::serve(listener,router(state)).with_graceful_shutdown(async {let _=stopped.await;}).await.unwrap();});
        let mut first=connect(address,&reader).await;let mut sibling=connect(address,&reader).await;let mut observer=connect(address,&reader).await;
        assert_eq!(rpc(&mut first,1,"server.reportClientActivity",report()).await["exit"]["_tag"],"Success");
        assert_eq!(rpc(&mut sibling,1,"server.reportClientActivity",report()).await["exit"]["_tag"],"Success");
        let response=rpc(&mut sibling,2,"server.getBackgroundPolicy",json!(false)).await;
        let snapshot:BackgroundPolicySnapshot=serde_json::from_value(response["exit"]["value"].clone()).unwrap();
        assert_eq!(snapshot.leases.len(),2);
        assert_eq!(snapshot.leases[0].session_id.as_str(),session.session_id);
        assert_eq!(snapshot.leases[1].session_id.as_str(),session.session_id);
        assert_ne!(snapshot.leases[0].rpc_client_id,snapshot.leases[1].rpc_client_id);
        assert!(snapshot.leases.iter().all(|lease|lease.rpc_client_id.0!=9000));
        assert!(snapshot.leases.iter().all(|lease|lease.updated_at.timestamp_millis()==1791417600000));
        let sibling_id=snapshot.leases[1].rpc_client_id;
        let malformed=rpc(&mut first,3,"server.reportClientActivity",json!({"clientId":"bad"})).await;
        assert_eq!(malformed["exit"]["cause"][0]["_tag"],"Die");
        let power=json!({"source":"electron-main","idle":"false","idleSeconds":0,"locked":"true","suspended":false,"onBattery":"false","lowPowerMode":"false","thermalState":"nominal","stale":false,"updatedAt":"2026-10-08T00:00:00.000Z"});
        let denied=rpc(&mut first,4,"server.reportHostPowerState",power.clone()).await;
        assert_eq!(denied["exit"]["cause"][0]["error"]["requiredPermission"],"environment:maintain");
        assert_eq!(policy.snapshot().await.host_power.source,HostPowerSource::Unknown);
        send(&mut observer,10,"subscribeBackgroundPolicy",json!({})).await;
        let initial=next(&mut observer).await;assert_eq!(initial["_tag"],"Chunk");
        let initial:BackgroundPolicySnapshot=serde_json::from_value(initial["values"][0].clone()).unwrap();assert_eq!(initial.leases.len(),2);
        ack(&mut observer,10).await;
        first.close(None).await.unwrap();
        let removed=next(&mut observer).await;let removed:BackgroundPolicySnapshot=serde_json::from_value(removed["values"][0].clone()).unwrap();
        assert_eq!(removed.leases.len(),1);assert_eq!(removed.leases[0].rpc_client_id,sibling_id);
        ack(&mut observer,10).await;
        let mut maintenance=connect(address,&maintainer).await;
        assert_eq!(rpc(&mut maintenance,1,"server.reportHostPowerState",power).await["exit"]["_tag"],"Success");
        let powered=next(&mut observer).await;let powered:BackgroundPolicySnapshot=serde_json::from_value(powered["values"][0].clone()).unwrap();
        assert_eq!(powered.host_power.locked,BackgroundBooleanState::True);assert!(!powered.should_run_opportunistic_work);
        ack(&mut observer,10).await;
        sibling.close(None).await.unwrap();let removed=next(&mut observer).await;
        assert!(serde_json::from_value::<BackgroundPolicySnapshot>(removed["values"][0].clone()).unwrap().leases.is_empty());
        // Hold this chunk's Ack: service stop must terminate the stream anyway.
        policy.shutdown().await;
        let exit=next(&mut observer).await;assert_eq!(exit["_tag"],"Exit");assert_eq!(exit["exit"]["_tag"],"Success");
        observer.send(Message::Text(json!({"_tag":"Ping"}).to_string().into())).await.unwrap();assert_eq!(next(&mut observer).await["_tag"],"Pong");
        observer.close(None).await.unwrap();maintenance.close(None).await.unwrap();stop.send(()).unwrap();while let Some(result)=tasks.join_next().await {result.unwrap();}
        desktop.shutdown().await;settings.shutdown().await;
    }).await.expect("owned background socket integration did not reach its milestones");
}
