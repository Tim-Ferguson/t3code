//! `server.discoverSourceControl` and the three `sourceControl.*` socket methods. Each decodes
//! its payload, calls one service method, and maps the failure to its wire `_tag`. The host
//! (environment, caches, clock) is the process's; settings come from the request's server.
//!
//! Tests that need a fake `PATH` or API endpoint put their host in place with `Host::install`.
use crate::{
    source_control_host::Host,
    source_control_registry::SourceControlRegistry,
    source_control_repository::SourceControlRepositoryService,
    transport::ApiState,
    vcs_rpc,
    vcs_service::VcsService,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use t3_contracts::{
    SourceControlCloneRepositoryInput, SourceControlPublishRepositoryInput, SourceControlRepositoryLookupInput,
};

pub const METHODS: [&str; 4] = [
    "server.discoverSourceControl",
    "sourceControl.lookupRepository",
    "sourceControl.cloneRepository",
    "sourceControl.publishRepository",
];

pub(crate) fn host(state: &ApiState) -> Host {
    Host::process_default().with_settings(state.settings.clone())
}

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload).map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}

fn encode<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("typed source control result")
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let registry = SourceControlRegistry::new(host(state));
    match tag {
        "server.discoverSourceControl" => {
            if !payload.is_object() {
                return Err(json!({"_tag":"SchemaDecodeError","message":"Expected an object."}));
            }
            Ok(encode(&Box::pin(registry.discover()).await))
        }
        "sourceControl.lookupRepository" => {
            let input: SourceControlRepositoryLookupInput = decode(payload)?;
            let service = SourceControlRepositoryService::new(registry);
            let result = Box::pin(service.lookup_repository(&input)).await;
            result.map(|info| encode(&info)).map_err(|e| e.wire())
        }
        "sourceControl.cloneRepository" => {
            let input: SourceControlCloneRepositoryInput = decode(payload)?;
            let service = SourceControlRepositoryService::new(registry);
            let result = Box::pin(service.clone_repository(&input)).await;
            result.map(|result| encode(&result)).map_err(|e| e.wire())
        }
        "sourceControl.publishRepository" => {
            let input: SourceControlPublishRepositoryInput = decode(payload)?;
            let service = SourceControlRepositoryService::new(registry);
            let publish_input = input.clone();
            let result = Box::pin(service.publish_repository(&publish_input)).await.map_err(|e| e.wire())?;
            // A new remote changes what the status shows (the host badge, upstream).
            let (status, hooks, cwd) = (
                VcsService::shared().status().clone(),
                vcs_rpc::context(state).hooks(),
                input.cwd.as_str().to_string(),
            );
            tokio::spawn(async move {
                if let Err(error) = status.refresh_status(&cwd, &hooks).await {
                    tracing::warn!(cwd=%cwd,error=%error,"VCS status refresh after publish failed");
                }
            });
            Ok(encode(&result))
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}
