//! Actual Effect socket proof for native registry preparation and its scope gate.
use crate::{
    acp_registry_support::fixture_catalog,
    auth::AuthService,
    persistence::Store,
    provider_registry::ProviderRegistry,
    transport::{ApiState, router},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;
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
async fn prepare(socket: &mut Socket, id: u32, payload: Value) -> Value {
    socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":"server.prepareAcpRegistryAgent","payload":payload,"headers":[]}).to_string().into())).await.unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    let wire: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
    serde_json::from_value::<t3_contracts::RpcServerMessage>(wire.clone()).unwrap();
    assert_eq!(wire["requestId"], id);
    wire
}
#[tokio::test]
async fn registry_prepare_effect_socket_enforces_manage_scope_and_validates_payload_before_install()
{
    tokio::time::timeout(std::time::Duration::from_secs(20),async {
        let directory=tempfile::tempdir().unwrap();
        let (catalog,config)=fixture_catalog(directory.path(),"normal");
        let settings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"registry-agent":{"driver":"acpRegistry","enabled":true,"config":config}}})).unwrap();
        let providers=ProviderRegistry::discover_with_catalog(&settings,directory.path(),Some(catalog)).await.unwrap();
        let store=Store::memory().unwrap();
        let auth=AuthService::new(store.clone(),[42;32],"fixture_registry_session".into(),"loopback-browser".into()).unwrap();
        let token=|scopes| auth.issue_session("fixture","bearer-access-token",scopes,json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let reader=token(vec![AuthEnvironmentScope::OrchestrationRead]);
        let manager=token(vec![AuthEnvironmentScope::ProvidersManage]);
        let state=ApiState{
            store,auth,environment:json!({"environmentId":"registry-fixture","label":"Registry fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
            config:None,settings:None,background:None,cors_origins:None,assets:None,providers:Some(providers.clone()),execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None,
        };
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let (stop,stopped)=tokio::sync::oneshot::channel();
        // JoinSet aborts only this owned server if an assertion or outer bound fails.
        let mut tasks=tokio::task::JoinSet::new();
        tasks.spawn(async move {axum::serve(listener,router(state)).with_graceful_shutdown(async{let _=stopped.await;}).await.unwrap();});
        let mut reader=connect(address,&reader).await;
        let denied=prepare(&mut reader,1,json!({"agentId":"devin"})).await;
        assert_eq!(denied["exit"]["_tag"],"Failure");
        assert_eq!(denied["exit"]["cause"][0]["_tag"],"Fail");
        assert_eq!(denied["exit"]["cause"][0]["error"]["requiredScope"],"orchestration:operate");
        assert_eq!(denied["exit"]["cause"][0]["error"]["requiredPermission"],"providers:manage");
        assert!(!directory.path().join("tools").exists());
        let mut manager=connect(address,&manager).await;
        let malformed=prepare(&mut manager,2,json!([])).await;
        assert_eq!(malformed["exit"]["cause"][0]["_tag"],"Die");
        assert!(malformed["exit"]["cause"][0]["defect"].as_str().is_some_and(|message| !message.is_empty()),"source schema failures encode a string defect");
        assert!(!directory.path().join("tools").exists());
        let malformed_object=prepare(&mut manager,3,json!({"agentId":"INVALID"})).await;
        assert_eq!(malformed_object["exit"]["cause"][0]["_tag"],"Die");
        assert!(malformed_object["exit"]["cause"][0]["defect"].as_str().is_some_and(|message| !message.is_empty()));
        assert!(!directory.path().join("tools").exists(),"method decoder must run before installing anything");
        let response=prepare(&mut manager,4,json!({"agentId":"devin"})).await;
        assert_eq!(response["exit"]["_tag"],"Success");
        let prepared:t3_contracts::AcpRegistryPrepareResult=serde_json::from_value(response["exit"]["value"].clone()).unwrap();
        assert!(prepared.prepared);assert_eq!(prepared.version.0.as_str(),"1.0.0+fixture");
        assert!(directory.path().join("tools").exists());
        assert!(providers.driver("registry-agent").is_err(),"prepare alone must not invent authenticated discovery");
        providers.reconfigure(&settings,directory.path()).await.unwrap();
        assert_eq!(providers.driver("registry-agent").unwrap(),"acpRegistry");
        assert_eq!(providers.snapshots().iter().find(|row|row["instanceId"]=="registry-agent").unwrap()["status"],"ready");
        reader.close(None).await.unwrap(); manager.close(None).await.unwrap();
        stop.send(()).unwrap();tasks.join_next().await.unwrap().unwrap();
    }).await.expect("Registry Effect socket milestones stalled");
}
