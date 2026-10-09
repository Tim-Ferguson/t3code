//! The four attachment and asset socket methods. Each decodes its payload, calls one
//! `AttachmentService` method on a blocking worker, and maps the failure to its wire `_tag`.
use crate::{
    asset_access::AssetError, asset_workspace::IssueContext, attachment_service::AttachmentService, persistence::Store,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use t3_contracts::{
    AssetCreateUrlInput, AssetResource, AttachmentCreateUploadUrlInput, AttachmentDeleteInput,
    PersistChatAttachmentsInput, PersistChatAttachmentsResult,
};

pub const METHODS: [&str; 4] = [
    "attachments.createUploadUrl",
    "attachments.delete",
    "assets.createUrl",
    "assets.persistChatAttachments",
];

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload)
        .map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}
fn encode<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("typed attachment result")
}

/// What `ws.ts` resolves for a resource before `issueAssetUrl`: the workspace a thread works in
/// (its worktree, else its project's root), a project's saved icon, and whether its clone has not
/// landed. Kinds that name everything themselves need none of it.
fn asset_context(store: &Store, resource: &AssetResource) -> Result<IssueContext, AssetError> {
    let wire = || serde_json::to_value(resource).expect("resources serialize");
    let refused = |tag: &'static str, cause: Option<Value>| AssetError::Refused { tag, resource: wire(), cause };
    let thread_workspace = |thread_id: &str| -> Result<Option<String>, AssetError> {
        let resolution = |message: String| refused("AssetWorkspaceContextResolutionError", Some(crate::asset_workspace::defect("Error", message)));
        let thread = store.projection("thread", thread_id).map_err(|error| resolution(error.to_string()))?.ok_or_else(|| resolution("Thread not found.".into()))?;
        let project_id = thread["thread"]["projectId"].as_str().unwrap_or_default().to_owned();
        let project = store.projection("project", &project_id).map_err(|error| resolution(error.to_string()))?.filter(|project| project["deletedAt"].is_null());
        Ok(project.map(|project| {
            thread["thread"]["worktreePath"].as_str().filter(|path| !path.is_empty()).map(str::to_owned).unwrap_or_else(|| project["workspaceRoot"].as_str().unwrap_or_default().to_owned())
        }))
    };
    let mut context = IssueContext::default();
    match resource {
        // An absolute media path can be linked from a thread on another environment.
        AssetResource::MediaFile { path, .. } if path.0.as_str().starts_with('/') => {}
        AssetResource::WorkspaceFile { thread_id, .. } | AssetResource::MediaFile { thread_id, .. } => {
            context.workspace_root = Some(thread_workspace(thread_id.as_str())?.ok_or_else(|| refused("AssetWorkspaceContextNotFoundError", None))?);
        }
        AssetResource::DraftWorkspaceFile { cwd, .. } => context.workspace_root = Some(cwd.0.as_str().to_owned()),
        AssetResource::ProjectFavicon { cwd, .. } => {
            let resolution = |message: String| refused("AssetWorkspaceContextResolutionError", Some(crate::asset_workspace::defect("Error", message)));
            let project = store
                .projections("project")
                .map_err(|error| resolution(error.to_string()))?
                .into_iter()
                .find(|project| project["deletedAt"].is_null() && project["workspaceRoot"] == cwd.0.as_str())
                .ok_or_else(|| refused("AssetWorkspaceContextNotFoundError", None))?;
            context.project_favicon_path = project["faviconPath"].as_str().filter(|path| !path.is_empty()).map(str::to_owned);
            // A cloned project exists before its files do; clients ask again when the clone lands.
            context.project_checkout_pending = project["projectId"].as_str().and_then(|id| crate::project_clone::of(store).get(id)).is_some_and(|clone| clone.phase != t3_contracts::ProjectClonePhase::Done && clone.destination_path.as_str() == project["workspaceRoot"].as_str().unwrap_or_default());
        }
        _ => {}
    }
    Ok(context)
}

pub async fn command(store: &Store, tag: &str, payload: Value) -> Result<Value, Value> {
    let service = AttachmentService::for_store(store).ok_or_else(|| {
        json!({"_tag":"NativeServiceUnavailableError","message":"Attachment storage is not configured."})
    })?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let blocking = |work: Box<dyn FnOnce() -> Result<Value, Value> + Send>| async move {
        tokio::task::spawn_blocking(work)
            .await
            .unwrap_or_else(|error| {
                Err(json!({"_tag":"NativeServiceError","message":error.to_string()}))
            })
    };
    match tag {
        "attachments.createUploadUrl" => {
            let input: AttachmentCreateUploadUrlInput = decode(payload)?;
            blocking(Box::new(move || {
                service
                    .issue_upload_url(&input, now_ms)
                    .map(|issued| encode(&issued))
                    .map_err(|error| json!({"_tag":"AttachmentUploadSigningKeyError","cause":{"name":"Error","message":error.0}}))
            }))
            .await
        }
        "attachments.delete" => {
            let input: AttachmentDeleteInput = decode(payload)?;
            blocking(Box::new(move || {
                service.delete_pending(input.attachment_id.0.as_str());
                Ok(Value::Null)
            }))
            .await
        }
        "assets.createUrl" => {
            let input: AssetCreateUrlInput = decode(payload)?;
            let store = store.clone();
            blocking(Box::new(move || {
                let context = asset_context(&store, &input.resource).map_err(|error| error.wire())?;
                service
                    .issue_asset_url(&input.resource, &context, Some(&store), now_ms)
                    .map_err(|error| error.wire())
            }))
            .await
        }
        "assets.persistChatAttachments" => {
            let input: PersistChatAttachmentsInput = decode(payload)?;
            blocking(Box::new(move || {
                let attachments = service
                    .persist_chat_attachments(&input)
                    .map_err(|error| error.wire())?;
                let result: PersistChatAttachmentsResult =
                    serde_json::from_value(json!({"attachments":attachments})).map_err(
                        |error| json!({"_tag":"NativeServiceError","message":error.to_string()}),
                    )?;
                Ok(encode(&result))
            }))
            .await
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}
