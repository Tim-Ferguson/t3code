//! The `projects.ensureScratch` and `projects.createNew` socket methods. Each decodes its payload,
//! calls one `ManagedProjectFolders` method, and wraps the failure the way the original handler
//! does (`OrchestrationDispatchCommandError`).
//!
//! The original also names `projects.list`, `projects.add` and `projects.remove` in `WS_METHODS`,
//! but declares no RPC for them: clients read projects from the shell snapshot and change them
//! through `projects.mutate`. They are deliberately not handled here.
use crate::{persistence::Store, project_folders::ManagedProjectFolders};
use serde_json::{Value, json};
use t3_contracts::{ProjectCreateNewInput, ProjectCreateNewResult, ProjectEnsureScratchResult};

pub const METHODS: [&str; 2] = ["projects.ensureScratch", "projects.createNew"];

fn decode_failure(message: impl ToString) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message.to_string()})
}

/// `ProjectCreateNewInput`, decoded the way the handler does before anything is made.
pub(crate) fn decode_create_new(payload: Value) -> Result<ProjectCreateNewInput, Value> {
    if !payload.is_object() {
        return Err(decode_failure("Expected an object."));
    }
    serde_json::from_value::<ProjectCreateNewInput>(payload).map_err(decode_failure)
}

pub async fn command(store: &Store, tag: &str, payload: Value) -> Result<Value, Value> {
    // Both payloads are structs: anything that is not an object fails the decode.
    if !payload.is_object() {
        return Err(decode_failure("Expected an object."));
    }
    let create = match tag {
        "projects.createNew" => Some(decode_create_new(payload)?),
        "projects.ensureScratch" => None,
        _ => return Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    };
    let folders = ManagedProjectFolders::for_store(store).ok_or_else(|| {
        json!({"_tag":"NativeServiceUnavailableError","message":"Project storage is not configured."})
    })?;
    // The work outlives a dropped request: a half-made project folder is worse than a finished one.
    tokio::spawn(async move {
        match create {
            None => {
                let project_id = folders.ensure_scratch_project().await;
                project_id.map(|project_id| json!({"projectId":project_id}))
            }
            Some(input) => {
                let name = input.name.0.as_str().to_owned();
                let created = folders.create_named_project(&name).await;
                created.map(|created| {
                    let mut result = json!({"projectId":created.project_id,"workspaceRoot":created.workspace_root});
                    if let Some(error) = created.commit_error.filter(|error| !error.is_empty()) {
                        result["commitError"] = json!(error);
                    }
                    result
                })
            }
        }
    })
    .await
    .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))?
    .map_err(|error| {
        tracing::warn!(error=?error,"managed project folder operation failed");
        error.wire()
    })
    .and_then(|result| {
        // Keep the wire result on the contract: it must decode as the original's success type.
        if tag == "projects.createNew" {
            typed::<ProjectCreateNewResult>(result)
        } else {
            typed::<ProjectEnsureScratchResult>(result)
        }
    })
}

fn typed<T: serde::de::DeserializeOwned + serde::Serialize>(value: Value) -> Result<Value, Value> {
    serde_json::from_value::<T>(value)
        .and_then(|typed| serde_json::to_value(typed))
        .map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))
}
