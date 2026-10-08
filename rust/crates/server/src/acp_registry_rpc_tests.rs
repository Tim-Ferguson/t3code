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
async fn rpc(socket: &mut Socket, id: u32, tag: &str, payload: Value) -> Value {
    socket
        .send(Message::Text(
            json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    let wire: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
    serde_json::from_value::<t3_contracts::RpcServerMessage>(wire.clone()).unwrap();
    assert_eq!(wire["requestId"], id);
    wire
}
async fn prepare(socket: &mut Socket, id: u32, payload: Value) -> Value {
    rpc(socket, id, "server.prepareAcpRegistryAgent", payload).await
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
            config:None,settings:None,device_hosts:None,devices:None,background:None,cors_origins:None,assets:None,providers:Some(providers.clone()),execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None,
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

#[tokio::test]
async fn registry_search_and_uninstall_effect_socket_preserve_scope_decode_reference_and_reservation_rules()
 {
    tokio::time::timeout(std::time::Duration::from_secs(20),async {
        let directory=tempfile::tempdir().unwrap();
        let (catalog,config)=fixture_catalog(directory.path(),"normal");
        let settings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"registry-agent":{"driver":"acpRegistry","enabled":true,"config":config}}})).unwrap();
        let providers=ProviderRegistry::discover_with_catalog(&settings,directory.path(),Some(catalog)).await.unwrap();
        let secrets=crate::server_secret_store::ServerSecretStore::open(directory.path().join("secrets")).unwrap();
        let mut options=crate::server_settings::SettingsOptions::file(directory.path().join("settings.json"),secrets);
        options.watch=false;
        let service=crate::server_settings::SettingsService::start(options).await.unwrap();
        let store=Store::memory().unwrap();
        let auth=AuthService::new(store.clone(),[52;32],"fixture_registry_control".into(),"loopback-browser".into()).unwrap();
        let token=|scopes|auth.issue_session("fixture","bearer-access-token",scopes,json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let reader_token=token(vec![AuthEnvironmentScope::OrchestrationRead]);
        let manager_token=token(vec![AuthEnvironmentScope::OrchestrationRead,AuthEnvironmentScope::ProvidersManage]);
        let state=ApiState{store,auth,environment:json!({"environmentId":"registry-control","label":"Registry fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),config:None,settings:Some(service.clone()),device_hosts:None,devices:None,background:None,cors_origins:None,assets:None,providers:Some(providers),execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
        let (stop,stopped)=tokio::sync::oneshot::channel();let mut tasks=tokio::task::JoinSet::new();
        tasks.spawn(async move{axum::serve(listener,router(state)).with_graceful_shutdown(async{let _=stopped.await;}).await.unwrap();});
        let mut reader=connect(address,&reader_token).await;
        let denied=rpc(&mut reader,1,"server.uninstallAcpRegistryManagedBinary",json!({"agentId":"devin"})).await;
        assert_eq!(denied["exit"]["cause"][0]["_tag"],"Fail");assert_eq!(denied["exit"]["cause"][0]["error"]["requiredPermission"],"providers:manage");
        for (id,payload) in [(2,json!([])),(3,json!({"query":4})),(4,json!({"query":"a".repeat(121)}))] {
            let malformed=rpc(&mut reader,id,"server.searchAcpRegistry",payload).await;
            assert_eq!(malformed["exit"]["cause"][0]["_tag"],"Die");assert!(malformed["exit"]["cause"][0]["defect"].is_string());
        }
        assert!(!directory.path().join("cache/acp-registry/registry.json").exists(),"decode and denied calls must not fetch or mutate");assert!(!directory.path().join("tools").exists());
        let searched=rpc(&mut reader,5,"server.searchAcpRegistry",json!({"query":"fixture"})).await;
        assert_eq!(searched["exit"]["_tag"],"Success");
        let found:t3_contracts::AcpRegistrySearchResult=serde_json::from_value(searched["exit"]["value"].clone()).unwrap();
        assert_eq!(found.agents.0.len(),1);assert_eq!(found.agents.0[0].id.as_str(),"devin");assert_eq!(found.agents.0[0].integrity,t3_contracts::AcpRegistryIntegrity::Sha256);
        assert!(!directory.path().join("tools").exists(),"authorized search does not prepare binaries");
        let mut manager=connect(address,&manager_token).await;
        for (id,payload) in [(6,json!([])),(7,json!({"agentId":"../escape"}))] {
            let malformed=rpc(&mut manager,id,"server.uninstallAcpRegistryManagedBinary",payload).await;
            assert_eq!(malformed["exit"]["cause"][0]["_tag"],"Die");assert!(malformed["exit"]["cause"][0]["defect"].is_string());
        }
        let absent=rpc(&mut manager,8,"server.uninstallAcpRegistryManagedBinary",json!({"agentId":"devin"})).await;
        assert_eq!(absent["exit"]["value"],json!({"agentId":"devin","removed":false}));
        let prepared=prepare(&mut manager,9,json!({"agentId":"devin"})).await;assert_eq!(prepared["exit"]["_tag"],"Success");
        let protected=rpc(&mut manager,10,"server.uninstallAcpRegistryManagedBinary",json!({"agentId":"devin"})).await;
        assert_eq!(protected["exit"]["value"]["removed"],false);assert!(directory.path().join("tools/devin").is_dir());
        service.update(serde_json::from_value(json!({"providerInstances":{"ref":{"driver":"acpRegistry","enabled":false,"config":{"agentId":"devin"}}}})).unwrap()).await.unwrap();
        let referenced=rpc(&mut manager,11,"server.uninstallAcpRegistryManagedBinary",json!({"agentId":"devin"})).await;
        assert_eq!(referenced["exit"]["value"]["removed"],false);assert!(directory.path().join("tools/devin").is_dir());
        service.update_provider_instance(t3_contracts::ProviderInstanceMutation::Remove{instance_id:"ref".parse().unwrap()},serde_json::from_value(json!({})).unwrap()).await.unwrap();
        let removed=rpc(&mut manager,12,"server.uninstallAcpRegistryManagedBinary",json!({"agentId":"devin"})).await;
        assert_eq!(removed["exit"]["_tag"],"Success");let result:t3_contracts::AcpRegistryManagedBinaryUninstallResult=serde_json::from_value(removed["exit"]["value"].clone()).unwrap();assert!(result.removed);assert!(!directory.path().join("tools/devin").exists());
        reader.close(None).await.unwrap();manager.close(None).await.unwrap();stop.send(()).unwrap();tasks.join_next().await.unwrap().unwrap();service.shutdown().await;
    }).await.expect("Registry search/uninstall Effect socket milestones stalled");
}

#[tokio::test]
async fn registry_url_consent_socket_gates_and_validates_before_settling_real_provider_callback() {
    tokio::time::timeout(std::time::Duration::from_secs(20),async{
        let directory=tempfile::tempdir().unwrap();
        let fixture=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-coordinator-provider.py");
        let settings=serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},
            "local-agent":{"driver":"acpRegistry","enabled":true,"config":{"source":"local","commandPath":"python3",
                "commandArgs":[fixture,"state",directory.path().join("starts")]}}
        }})).unwrap();
        let providers=ProviderRegistry::discover(&settings,directory.path()).await.unwrap();
        let mut pending=providers.coordinator().subscribe_url_action("local-agent");
        let provider=providers.acp("local-agent").unwrap();
        let cwd=directory.path().to_owned();
        let mut sessions=tokio::task::JoinSet::new();
        sessions.spawn(async move{provider.connect(&cwd,None,false).await});
        let action=pending.recv().await.unwrap().unwrap();
        assert_eq!(action.elicitation_id.0.as_str(),"login-0");
        let store=Store::memory().unwrap();
        let auth=AuthService::new(store.clone(),[42;32],"fixture_registry_session".into(),"loopback-browser".into()).unwrap();
        let token=|scopes|auth.issue_session("fixture","bearer-access-token",scopes,json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let reader=token(vec![AuthEnvironmentScope::OrchestrationRead]);
        let manager=token(vec![AuthEnvironmentScope::ProvidersManage]);
        let state=ApiState{
            store,auth,environment:json!({"environmentId":"registry-fixture","label":"Registry fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
            config:None,settings:None,device_hosts:None,devices:None,background:None,cors_origins:None,assets:None,providers:Some(providers.clone()),execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None,
        };
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let (stop,stopped)=tokio::sync::oneshot::channel();
        let mut tasks=tokio::task::JoinSet::new();
        tasks.spawn(async move{axum::serve(listener,router(state)).with_graceful_shutdown(async{let _=stopped.await;}).await.unwrap();});
        let mut reader=connect(address,&reader).await;
        let input=json!({"instanceId":"local-agent","elicitationId":"login-0"});
        let denied=rpc(&mut reader,1,"server.acceptAcpRegistryUrlAuth",input.clone()).await;
        assert_eq!(denied["exit"]["cause"][0]["_tag"],"Fail");
        assert_eq!(denied["exit"]["cause"][0]["error"]["requiredPermission"],"providers:manage");
        assert!(providers.coordinator().url_action("local-agent").is_some());
        let mut manager=connect(address,&manager).await;
        for (id,payload) in [(2,json!([])),(3,json!({"instanceId":"local-agent","elicitationId":""}))]{
            let invalid=rpc(&mut manager,id,"server.acceptAcpRegistryUrlAuth",payload).await;
            assert_eq!(invalid["exit"]["cause"][0]["_tag"],"Die");
            assert!(invalid["exit"]["cause"][0]["defect"].as_str().is_some_and(|value|!value.is_empty()));
            assert!(providers.coordinator().url_action("local-agent").is_some());
        }
        let wrong=rpc(&mut manager,4,"server.acceptAcpRegistryUrlAuth",json!({"instanceId":"local-agent","elicitationId":"wrong"})).await;
        assert_eq!(wrong["exit"]["value"],json!({"accepted":false}));
        let accepted=rpc(&mut manager,5,"server.acceptAcpRegistryUrlAuth",input.clone()).await;
        let accepted:t3_contracts::AcpRegistryAcceptUrlAuthResult=serde_json::from_value(accepted["exit"]["value"].clone()).unwrap();
        assert!(accepted.accepted);
        let session=sessions.join_next().await.unwrap().unwrap().unwrap();
        assert!(pending.recv().await.unwrap().is_none());
        assert_eq!(session.setup.session_id,"coordinator-session");
        let repeated=rpc(&mut manager,6,"server.acceptAcpRegistryUrlAuth",input).await;
        assert_eq!(repeated["exit"]["value"],json!({"accepted":false}));
        session.shutdown().await;
        reader.close(None).await.unwrap();
        manager.close(None).await.unwrap();
        let _=stop.send(());
        tasks.join_next().await.unwrap().unwrap();
    }).await.unwrap();
}
