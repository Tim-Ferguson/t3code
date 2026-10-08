//! Settings mutation exercises the real resolver and durable owner over Effect RPC.
use crate::{
    auth::AuthService,
    device_host_resolver::{DeviceHostResolver, DeviceHostResolverOptions},
    persistence::Store,
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
    transport::{ApiState, router},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};
use t3_contracts::{AuthEnvironmentScope, RpcServerMessage, ServerSettings};
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
    serde_json::from_value::<RpcServerMessage>(value.clone()).unwrap();
    value
}
fn host(target: &str) -> Value {
    json!({"id":format!("fixture-{target}"),"label":target,"target":target})
}
#[cfg(unix)]
#[tokio::test]
async fn settings_socket_filters_only_self_aliases_and_cancelled_resolution_cannot_mutate_owner() {
    use std::os::unix::fs::PermissionsExt;
    tokio::time::timeout(std::time::Duration::from_secs(15),async {
        let directory=tempfile::tempdir().unwrap();
        let binary=directory.path().join("ssh.py");
        std::fs::write(&binary,"#!/usr/bin/env python3\nimport sys\nt=sys.argv[-1]\nassert '-G' in sys.argv\nprint('hostname '+{'self':'100.65.180.100','loopback':'127.0.1.1','held':'held.test','unresolved':'unresolved.test'}.get(t,'192.0.2.1'))\nprint('port '+('2222' if t=='forwarded' else '22'))\nif t=='proxy':print('proxyjump bastion')\nif t=='failed':sys.exit(3)\n").unwrap();
        std::fs::set_permissions(&binary,std::fs::Permissions::from_mode(0o700)).unwrap();
        let entered=Arc::new(tokio::sync::Notify::new());let signal=entered.clone();
        let resolver=DeviceHostResolver::new(DeviceHostResolverOptions {
            ssh_binary:binary,
            local_addresses:Arc::new(||HashSet::from(["100.65.180.100".into()])),
            lookup:Arc::new(move |hostname| { let signal=signal.clone();Box::pin(async move { if hostname=="held.test" { signal.notify_one();std::future::pending().await } else { vec![] } }) }),
            ..Default::default()
        });
        let path=directory.path().join("settings.json");
        let mut options=SettingsOptions::file(path.clone(),ServerSecretStore::open(directory.path().join("secrets")).unwrap());options.watch=false;
        let settings=SettingsService::start(options).await.unwrap();
        let store=Store::memory().unwrap();let auth=AuthService::new(store.clone(),[78;32],"device_fixture".into(),"loopback-browser".into()).unwrap();
        let grant=|scopes|auth.issue_session("fixture","bearer-access-token",scopes,json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let reader=grant(vec![AuthEnvironmentScope::OrchestrationRead]);
        let writer=grant(vec![AuthEnvironmentScope::OrchestrationRead,AuthEnvironmentScope::SettingsWrite]);
        let state=ApiState {store,auth,environment:json!({"environmentId":"device-fixture","label":"Device fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),config:None,settings:Some(settings.clone()),cors_origins:None,assets:None,providers:None,execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None,background:None,device_hosts:Some(resolver.clone()),devices:None,provider_auth:None};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
        let (stop,stopped)=tokio::sync::oneshot::channel();let mut server=tokio::task::JoinSet::new();
        server.spawn(async move {axum::serve(listener,router(state)).with_graceful_shutdown(async {let _=stopped.await;}).await.unwrap();});
        let mut readonly=connect(address,&reader).await;
        send(&mut readonly,1,"server.updateSettings",json!({"patch":{"deviceHosts":[host("held")]}})).await;
        let denied=next(&mut readonly).await;assert_eq!(denied["exit"]["cause"][0]["error"]["requiredPermission"],"settings:write");
        assert!(settings.snapshot().await.unwrap().device_hosts.0.is_empty());
        let mut socket=connect(address,&writer).await;
        let targets=["self","remote","forwarded","loopback","proxy","unresolved","failed"];
        send(&mut socket,1,"server.updateSettings",json!({"patch":{"deviceHosts":targets.iter().map(|target|host(target)).collect::<Vec<_>>()}})).await;
        let frame=next(&mut socket).await;assert_eq!(frame["exit"]["_tag"],"Success");
        let value=frame["exit"]["value"].clone();let persisted:ServerSettings=serde_json::from_value(value).unwrap();
        let remaining=persisted.device_hosts.0.iter().map(|host|host.target.as_str()).collect::<Vec<_>>();
        assert_eq!(remaining,["remote","forwarded","proxy","unresolved","failed"]);
        let saved=std::fs::read(&path).unwrap();let disk:ServerSettings=serde_json::from_slice(&saved).unwrap();assert_eq!(disk.device_hosts,persisted.device_hosts);
        send(&mut socket,2,"server.updateSettings",json!({"patch":{"deviceHosts":[{"id":"local","label":"Invalid","target":"remote"}]}})).await;
        assert_eq!(next(&mut socket).await["exit"]["cause"][0]["_tag"],"Die");assert_eq!(std::fs::read(&path).unwrap(),saved);
        send(&mut socket,3,"server.updateSettings",json!({"patch":{"deviceHosts":[host("held")]}})).await;
        entered.notified().await;
        socket.send(Message::Text(json!({"_tag":"Ping"}).to_string().into())).await.unwrap();assert_eq!(next(&mut socket).await["_tag"],"Pong");
        socket.send(Message::Text(json!({"_tag":"Interrupt","requestId":3}).to_string().into())).await.unwrap();
        send(&mut socket,4,"server.getSettings",json!({})).await;
        let interrupted=next(&mut socket).await;assert_eq!(interrupted["requestId"],3);assert_eq!(interrupted["exit"]["_tag"],"Failure");assert_eq!(interrupted["exit"]["cause"][0]["_tag"],"Interrupt");
        let frame=next(&mut socket).await;assert_eq!(frame["requestId"],4);assert_eq!(frame["exit"]["value"]["deviceHosts"],serde_json::to_value(&persisted.device_hosts).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(),saved);
        socket.close(None).await.unwrap();readonly.close(None).await.unwrap();stop.send(()).unwrap();server.join_next().await.unwrap().unwrap();
        resolver.shutdown().await;settings.shutdown().await;
    }).await.unwrap();
}
