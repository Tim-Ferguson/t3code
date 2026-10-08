//! Actual scoped Effect socket with two independent authenticated owners.
use crate::{
    acp_registry_rpc_tests::{Socket, connect, rpc},
    auth::AuthService,
    persistence::Store,
    provider_auth_service::ProviderAuthService,
    provider_registry::ProviderRegistry,
    transport::{ApiState, router},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use t3_contracts::*;
use tokio_tungstenite::tungstenite::Message;
async fn subscribe(socket: &mut Socket, id: u32) {
    socket.send(Message::Text(json!({"_tag":"Request","id":id,"tag":"provider.auth.subscribe","payload":{"instanceId":"agent"},"headers":[]}).to_string().into())).await.unwrap();
}
async fn next(socket: &mut Socket, id: u32) -> ProviderAuthState {
    let frame = socket.next().await.unwrap().unwrap();
    let wire: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    serde_json::from_value::<RpcServerMessage>(wire.clone()).unwrap();
    assert_eq!(wire["_tag"], "Chunk", "{wire}");
    assert_eq!(wire["requestId"], id);
    socket
        .send(Message::Text(
            json!({"_tag":"Ack","requestId":id}).to_string().into(),
        ))
        .await
        .unwrap();
    serde_json::from_value(wire["values"][0].clone()).unwrap()
}
async fn phase(socket: &mut Socket, id: u32, wanted: ProviderAuthPhase) -> ProviderAuthState {
    loop {
        let state = next(socket, id).await;
        if state.phase == wanted {
            return state;
        }
        assert_ne!(
            state.phase,
            ProviderAuthPhase::Failed,
            "{:?}",
            state.message
        );
    }
}
#[tokio::test]
async fn provider_auth_effect_socket_enforces_scope_owner_privacy_cancel_reconnect_and_logout() {
    tokio::time::timeout(std::time::Duration::from_secs(20),async{
        let root=tempfile::tempdir().unwrap();let log=root.path().join("agent.log");
        let settings:ServerSettings=serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"agent":{"driver":"acpRegistry","enabled":true,"config":{"source":"local","commandPath":"python3","commandArgs":[format!("{}/tests/fixtures/acp-auth-provider.py",env!("CARGO_MANIFEST_DIR")),log,"2"]}}}})).unwrap();
        let registry=ProviderRegistry::discover(&settings,root.path()).await.unwrap();
        let service=ProviderAuthService::new(registry.clone(),root.path().into(),root.path().join("caches"),Arc::new(|_|Box::pin(async{Ok(())})));
        let store=Store::memory().unwrap();let auth=AuthService::new(store.clone(),[47;32],"fixture_auth_socket".into(),"loopback-browser".into()).unwrap();
        let token=|scopes|auth.issue_session("fixture","bearer-access-token",scopes,json!({"deviceType":"unknown"}),Utc::now(),chrono::Duration::hours(1)).unwrap().1;
        let read=token(vec![AuthEnvironmentScope::OrchestrationRead]);let owner=token(vec![AuthEnvironmentScope::ProvidersManage]);let other=token(vec![AuthEnvironmentScope::ProvidersManage]);
        let state=ApiState{store,auth,environment:json!({"environmentId":"auth-fixture","label":"Auth fixture","platform":{"os":"darwin","arch":"arm64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),config:None,settings:None,device_hosts:None,devices:None,background:None,cors_origins:None,assets:None,providers:Some(registry),provider_auth:Some(service.clone()),execution:None,workspace:None,terminals:None,discovery:None,resource_telemetry:None,host_resources:None};
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();let(stop,stopped)=tokio::sync::oneshot::channel();let mut tasks=tokio::task::JoinSet::new();tasks.spawn(async move{axum::serve(listener,router(state)).with_graceful_shutdown(async{let _=stopped.await;}).await.unwrap();});
        let baseline=std::fs::read_to_string(&log).unwrap();let mut reader=connect(address,&read).await;
        for (id,tag) in ["provider.auth.start","provider.auth.respond","provider.auth.complete","provider.auth.cancel","provider.auth.logout","provider.auth.subscribe"].into_iter().enumerate(){
            let denied=rpc(&mut reader,id as u32+1,tag,json!({"instanceId":"agent"})).await;
            assert_eq!(denied["exit"]["cause"][0]["_tag"],"Fail");assert_eq!(denied["exit"]["cause"][0]["error"]["requiredPermission"],"providers:manage");
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap(),baseline,"scope denial cannot start discovery or login");
        let mut commands=connect(address,&owner).await;let mut other_commands=connect(address,&other).await;
        let malformed=rpc(&mut commands,10,"provider.auth.start",json!([])).await;assert_eq!(malformed["exit"]["cause"][0]["_tag"],"Die");
        let malformed=rpc(&mut commands,11,"provider.auth.start",json!({"instanceId":""})).await;assert_eq!(malformed["exit"]["cause"][0]["_tag"],"Die");
        assert_eq!(std::fs::read_to_string(&log).unwrap(),baseline,"decode precedes service work");
        let mut states=connect(address,&owner).await;let mut hidden=connect(address,&other).await;subscribe(&mut states,100).await;subscribe(&mut hidden,100).await;
        loop{let state=next(&mut states,100).await;if let Some(methods)=state.methods{assert_eq!(methods.0.len(),3);break;}}
        let start=rpc(&mut commands,12,"provider.auth.start",json!({"instanceId":"agent","methodId":"agent"})).await;
        assert_eq!(start["exit"]["_tag"],"Success");let flow=start["exit"]["value"]["flowId"].clone();
        let waiting=phase(&mut states,100,ProviderAuthPhase::Waiting).await;assert_eq!(waiting.authorization_url.as_deref(),Some("https://example.test/login"));
        let private=phase(&mut hidden,100,ProviderAuthPhase::Waiting).await;assert!(private.flow_id.is_none()&&private.authorization_url.is_none()&&private.expires_at.is_none());assert_eq!(private.interaction,Some(Some(None)));
        let repeated=rpc(&mut commands,13,"provider.auth.start",json!({"instanceId":"agent","methodId":"agent"})).await;assert_eq!(repeated["exit"]["value"]["flowId"],flow);
        let denied=rpc(&mut other_commands,14,"provider.auth.cancel",json!({"instanceId":"agent","flowId":flow})).await;assert_eq!(denied["exit"]["cause"][0]["error"]["_tag"],"ProviderSetupError");
        let cancelled=rpc(&mut commands,15,"provider.auth.cancel",json!({"instanceId":"agent","flowId":flow})).await;assert_eq!(cancelled["exit"]["value"]["phase"],"cancelled");assert_eq!(cancelled["exit"]["value"]["flowId"],flow);assert!(!std::path::Path::new(&format!("{}.credentials",log.display())).exists());
        // Same session token on a newly connected socket retains ownership.
        commands.close(None).await.unwrap();let mut commands=connect(address,&owner).await;
        let start=rpc(&mut commands,16,"provider.auth.start",json!({"instanceId":"agent","methodId":"agent"})).await;let flow=start["exit"]["value"]["flowId"].clone();
        let waiting=phase(&mut states,100,ProviderAuthPhase::Waiting).await;assert_eq!(serde_json::to_value(&waiting).unwrap()["flowId"],flow);
        let response=json!({"instanceId":"agent","flowId":flow,"interactionId":"fixture-consent","response":{"type":"browser","action":"accept"}});
        let denied=rpc(&mut other_commands,17,"provider.auth.respond",response.clone()).await;assert_eq!(denied["exit"]["_tag"],"Failure");
        let accepted=rpc(&mut commands,18,"provider.auth.respond",response).await;assert_eq!(accepted["exit"]["_tag"],"Success");phase(&mut states,100,ProviderAuthPhase::Succeeded).await;
        assert!(std::path::Path::new(&format!("{}.credentials",log.display())).exists());
        let logout=rpc(&mut commands,19,"provider.auth.logout",json!({"instanceId":"agent"})).await;assert_eq!(logout["exit"]["value"]["phase"],"idle");assert!(!std::path::Path::new(&format!("{}.credentials",log.display())).exists());
        for socket in [&mut reader,&mut commands,&mut other_commands,&mut states,&mut hidden]{socket.close(None).await.unwrap();}
        service.shutdown().await;stop.send(()).unwrap();tasks.join_next().await.unwrap().unwrap();
        for line in std::fs::read_to_string(&log).unwrap().lines(){let row:Value=serde_json::from_str(line).unwrap();assert_eq!(unsafe{libc::kill(row["pid"].as_i64().unwrap() as i32,0)},-1);assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(libc::ESRCH));}
    }).await.expect("Provider auth socket milestones stalled");
}
