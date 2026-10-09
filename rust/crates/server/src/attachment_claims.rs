//! Claims staged uploads into a thread's attachment store before a message command reaches
//! the orchestrator (orchestration-v2/AttachmentClaims.ts and ThreadMessageIntake.ts): the
//! staged file is verified and copied under a thread-scoped id, and the attachment refs in
//! the command are rewritten. A copy, not a move: the staged file stays behind as the retry
//! source for a failed attempt, and the periodic pending sweep reclaims it later.
use crate::{
    attachment_service::AttachmentService,
    attachment_store::{self as store, AttachmentRef},
    persistence::{Receipt, Store, StoreError},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use t3_contracts::{ChatAttachment, get_provider_attachment_limit_error};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ClaimError(pub String);

#[derive(Debug)]
pub struct Claimed {
    pub attachments: Vec<Value>,
    pub claimed_paths: Vec<PathBuf>,
}

pub fn validate_attachment_limits(attachments: &[Value]) -> Result<(), ClaimError> {
    let typed = attachments
        .iter()
        .map(|attachment| serde_json::from_value::<ChatAttachment>(attachment.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ClaimError(format!("Invalid attachment: {error}")))?;
    match get_provider_attachment_limit_error(&typed) {
        Some(message) => Err(ClaimError(message.into())),
        None => Ok(()),
    }
}

pub fn attachment_is_pending_upload(attachment: &Value) -> bool {
    attachment["id"]
        .as_str()
        .is_some_and(store::is_pending_attachment_id)
}

/// Removes partial claims, only before dispatch or after proving they were not accepted.
pub fn release_claimed_attachments(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

pub fn claim_pending_attachments(
    attachments_dir: &Path,
    thread_id: &str,
    attachments: &[Value],
) -> Result<Claimed, ClaimError> {
    validate_attachment_limits(attachments)?;
    let ids: HashSet<_> = attachments
        .iter()
        .map(|attachment| &attachment["id"])
        .collect();
    if ids.len() != attachments.len() {
        return Err(ClaimError(
            "Duplicate attachment ids are not allowed.".into(),
        ));
    }
    if !attachments.iter().any(attachment_is_pending_upload) {
        return Ok(Claimed {
            attachments: attachments.to_vec(),
            claimed_paths: vec![],
        });
    }
    let mut claimed_paths = Vec::new();
    let mut claimed = Vec::new();
    for attachment in attachments {
        let result = claim_one(attachments_dir, thread_id, attachment, &mut claimed_paths);
        match result {
            Ok(attachment) => claimed.push(attachment),
            Err(error) => {
                release_claimed_attachments(&claimed_paths);
                return Err(error);
            }
        }
    }
    Ok(Claimed {
        attachments: claimed,
        claimed_paths,
    })
}

fn claim_one(
    attachments_dir: &Path,
    thread_id: &str,
    attachment: &Value,
    claimed_paths: &mut Vec<PathBuf>,
) -> Result<Value, ClaimError> {
    if !attachment_is_pending_upload(attachment) {
        return Ok(attachment.clone());
    }
    let name = attachment["name"].as_str().unwrap_or_default();
    let cannot =
        |reason: &str| ClaimError(format!("Attachment '{name}' cannot be sent: {reason}."));
    let plan = store::plan_attachment_claim(
        attachments_dir,
        thread_id,
        attachment["id"].as_str().unwrap_or_default(),
    )
    .map_err(cannot)?;
    let stored =
        std::fs::metadata(&plan.current_path).map_err(|_| cannot("attachment not found"))?;
    if Some(stored.len()) != attachment["sizeBytes"].as_u64() {
        return Err(cannot("stored size does not match"));
    }
    let mut normalized = attachment.clone();
    normalized["id"] = json!(plan.final_id);
    normalized["mimeType"] = json!(
        attachment["mimeType"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
    );
    let expected = AttachmentRef::from_value(&normalized)
        .and_then(|reference| store::resolve_attachment_path(attachments_dir, &reference));
    if expected.as_deref() != Some(plan.final_path.as_path()) {
        return Err(cannot("attachment type does not match the upload"));
    }
    std::fs::copy(&plan.current_path, &plan.final_path).map_err(|error| {
        tracing::error!(error=%error,"Failed to claim attachment.");
        // A partial copy may exist; the caller's rollback must see it.
        claimed_paths.push(plan.final_path.clone());
        ClaimError(format!(
            "Failed to claim attachment '{name}' for this thread."
        ))
    })?;
    claimed_paths.push(plan.final_path);
    Ok(normalized)
}

/// After an accepted command: drop the copies the accepted message does not reference
/// (a replayed receipt reports the first attempt's attachments).
pub fn release_unused_claims(
    attachments_dir: &Path,
    claimed_paths: &[PathBuf],
    accepted: &[Value],
) {
    if claimed_paths.is_empty() {
        return;
    }
    let retained: HashSet<_> = accepted
        .iter()
        .filter_map(AttachmentRef::from_value)
        .filter_map(|reference| store::resolve_attachment_path(attachments_dir, &reference))
        .collect();
    let unused: Vec<_> = claimed_paths
        .iter()
        .filter(|path| !retained.contains(*path))
        .cloned()
        .collect();
    release_claimed_attachments(&unused);
}

pub enum ClaimedDispatchError {
    Claim(String),
    Store(StoreError),
}
impl ClaimedDispatchError {
    /// A refused claim is an `OrchestrationV2DispatchCommandError`; store failures keep the
    /// transport's own mapping.
    pub fn dispatch_wire(self, command: &Value, store: impl FnOnce(StoreError) -> Value) -> Value {
        match self {
            Self::Claim(message) => {
                json!({"_tag":"OrchestrationV2DispatchCommandError","commandId":command["commandId"],"commandType":command["type"],"message":message,"detail":message})
            }
            Self::Store(error) => store(error),
        }
    }
    pub fn launch_wire(self, input: &Value, store: impl FnOnce(StoreError) -> Value) -> Value {
        match self {
            Self::Claim(message) => {
                json!({"_tag":"OrchestrationV2ThreadLaunchError","commandId":input["commandId"],"projectId":input["projectId"],"message":message})
            }
            Self::Store(error) => store(error),
        }
    }
}

fn message_attachments(projection: &Value, message_id: &Value) -> Option<Vec<Value>> {
    projection["messages"]
        .as_array()?
        .iter()
        .find(|message| message["id"] == *message_id)
        .map(|message| {
            message["attachments"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
}

/// `message.dispatch` with uploads: claim, dispatch the rewritten command, then settle the
/// claimed copies against what the store accepted. Any other command passes through.
pub fn dispatch_with_claims(
    files: Option<&AttachmentService>,
    store: &Store,
    command: &Value,
    dispatch: impl FnOnce(&Value) -> Result<Receipt, StoreError>,
) -> Result<Receipt, ClaimedDispatchError> {
    let attachments = command["attachments"]
        .as_array()
        .filter(|items| !items.is_empty());
    let (Some(attachments), "message.dispatch" | "queued-run.edit") =
        (attachments, command["type"].as_str().unwrap_or_default())
    else {
        return dispatch(command).map_err(ClaimedDispatchError::Store);
    };
    let files = files.ok_or_else(|| {
        ClaimedDispatchError::Claim("Attachment storage is not configured.".into())
    })?;
    let thread_id = command["threadId"].as_str().unwrap_or_default();
    let claimed = claim_pending_attachments(files.attachments_dir(), thread_id, attachments)
        .map_err(|error| ClaimedDispatchError::Claim(error.0))?;
    let mut rewritten = command.clone();
    rewritten["attachments"] = Value::Array(claimed.attachments);
    match dispatch(&rewritten) {
        Err(error) => {
            release_claimed_attachments(&claimed.claimed_paths);
            Err(ClaimedDispatchError::Store(error))
        }
        Ok(receipt) if receipt.status == "rejected" => {
            release_claimed_attachments(&claimed.claimed_paths);
            Ok(receipt)
        }
        Ok(receipt) => {
            // With no readable accepted message the outcome is ambiguous and the copies stay.
            if let Some(accepted) = store
                .projection("thread", thread_id)
                .ok()
                .flatten()
                .and_then(|projection| {
                    // An edit names the queued run; its message is the run's.
                    let message_id = if command["type"] == "queued-run.edit" {
                        projection["runs"].as_array()?.iter().find(|run| run["id"] == command["runId"])?["userMessageId"].clone()
                    } else {
                        command["messageId"].clone()
                    };
                    message_attachments(&projection, &message_id)
                })
            {
                release_unused_claims(files.attachments_dir(), &claimed.claimed_paths, &accepted);
            }
            Ok(receipt)
        }
    }
}

/// `orchestration.launchThread` whose initial message carries uploads.
pub fn launch_with_claims(
    files: Option<&AttachmentService>,
    input: &Value,
    launch: impl FnOnce(&Value) -> Result<Value, StoreError>,
) -> Result<Value, ClaimedDispatchError> {
    let attachments = input["initialMessage"]["attachments"]
        .as_array()
        .filter(|items| !items.is_empty());
    let Some(attachments) = attachments else {
        return launch(input).map_err(ClaimedDispatchError::Store);
    };
    validate_attachment_limits(attachments)
        .map_err(|error| ClaimedDispatchError::Claim(error.0))?;
    if !attachments.iter().any(attachment_is_pending_upload) {
        return launch(input).map_err(ClaimedDispatchError::Store);
    }
    let thread_id = input["threadId"].as_str().ok_or_else(|| {
        ClaimedDispatchError::Claim("Uploaded attachments need a thread id at launch.".into())
    })?;
    let files = files.ok_or_else(|| {
        ClaimedDispatchError::Claim("Attachment storage is not configured.".into())
    })?;
    let claimed = claim_pending_attachments(files.attachments_dir(), thread_id, attachments)
        .map_err(|error| ClaimedDispatchError::Claim(error.0))?;
    let mut rewritten = input.clone();
    rewritten["initialMessage"]["attachments"] = Value::Array(claimed.attachments);
    match launch(&rewritten) {
        Err(error) => {
            release_claimed_attachments(&claimed.claimed_paths);
            Err(ClaimedDispatchError::Store(error))
        }
        Ok(result) => {
            let accepted: Vec<Value> = result["projection"]["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|message| {
                    message["attachments"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            release_unused_claims(files.attachments_dir(), &claimed.claimed_paths, &accepted);
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment_service::testing::service;

    const UUID: &str = "00000000-0000-4000-8000-0000000000aa";
    fn stage(files: &AttachmentService, name: &str, bytes: &[u8]) -> Value {
        let id = store::create_pending_attachment_id(None);
        std::fs::write(files.attachments_dir().join(format!("{id}.png")), bytes).unwrap();
        json!({"type":"image","id":id,"name":name,"mimeType":"image/png","sizeBytes":bytes.len()})
    }
    fn names(files: &AttachmentService, prefix: &str) -> Vec<String> {
        std::fs::read_dir(files.attachments_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(prefix))
            .collect()
    }

    #[test]
    fn existing_files_pass_through_without_new_copies() {
        let (_directory, files) = service();
        let existing: Vec<Value> = (0..100)
            .map(|index| json!({"type":"file","id":format!("existing-file-{index}"),"name":format!("{index}.txt"),"mimeType":"text/plain","sizeBytes":1}))
            .collect();
        let claimed =
            claim_pending_attachments(files.attachments_dir(), "thread-many", &existing).unwrap();
        assert_eq!(claimed.attachments, existing);
        assert!(claimed.claimed_paths.is_empty());
        let too_many: Vec<Value> = existing.iter().cloned().chain([json!({"type":"file","id":"one-more","name":"x","mimeType":"text/plain","sizeBytes":1})]).collect();
        assert!(
            claim_pending_attachments(files.attachments_dir(), "thread-many", &too_many)
                .unwrap_err()
                .0
                .contains("up to 100 files")
        );
    }

    #[test]
    fn claims_a_pending_upload_into_the_thread_store_and_keeps_the_source() {
        let (_directory, files) = service();
        let pending = stage(&files, "screenshot.png", &[1, 2, 3, 4]);
        let claimed = claim_pending_attachments(
            files.attachments_dir(),
            "thread-claims-1",
            &[pending.clone()],
        )
        .unwrap();
        let attachment = &claimed.attachments[0];
        assert!(!attachment_is_pending_upload(attachment));
        assert_eq!(
            store::parse_thread_segment(attachment["id"].as_str().unwrap()).as_deref(),
            Some("thread-claims-1")
        );
        assert_eq!(claimed.claimed_paths.len(), 1);
        assert_eq!(
            std::fs::read(&claimed.claimed_paths[0]).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            names(&files, "pending-").len(),
            1,
            "pending copy stays as the retry source"
        );
        // Still on disk under the path the store will resolve for the rewritten attachment.
        let reference = AttachmentRef::from_value(attachment).unwrap();
        assert_eq!(
            store::resolve_attachment_path(files.attachments_dir(), &reference).unwrap(),
            claimed.claimed_paths[0]
        );
    }

    #[test]
    fn already_claimed_attachments_are_untouched() {
        let (_directory, files) = service();
        let stored = json!({"type":"image","id":format!("thread-claims-2-{UUID}"),"name":"existing.png","mimeType":"image/png","sizeBytes":4});
        let claimed = claim_pending_attachments(
            files.attachments_dir(),
            "thread-claims-2",
            &[stored.clone()],
        )
        .unwrap();
        assert_eq!(claimed.attachments, vec![stored]);
        assert!(claimed.claimed_paths.is_empty());
    }

    #[test]
    fn refuses_missing_mismatched_and_duplicate_uploads_and_rolls_back() {
        let (_directory, files) = service();
        let missing = json!({"type":"image","id":store::create_pending_attachment_id(None),"name":"gone.png","mimeType":"image/png","sizeBytes":4});
        let error =
            claim_pending_attachments(files.attachments_dir(), "thread-claims-3", &[missing])
                .unwrap_err();
        assert_eq!(
            error.0,
            "Attachment 'gone.png' cannot be sent: attachment not found (removed or expired)."
        );

        let good = stage(&files, "one.png", &[1, 2, 3, 4]);
        let mut mismatched = stage(&files, "two.png", &[1, 2, 3, 4]);
        mismatched["sizeBytes"] = json!(999);
        let error = claim_pending_attachments(
            files.attachments_dir(),
            "thread-claims-4",
            &[good.clone(), mismatched],
        )
        .unwrap_err();
        assert!(
            error.0.contains("stored size does not match"),
            "{}",
            error.0
        );
        assert_eq!(
            names(&files, "thread-claims-4-"),
            Vec::<String>::new(),
            "earlier claim rolled back"
        );

        let mut retyped = stage(&files, "three.png", &[1, 2, 3, 4]);
        retyped["mimeType"] = json!("image/jpeg");
        let error =
            claim_pending_attachments(files.attachments_dir(), "thread-claims-5", &[retyped])
                .unwrap_err();
        assert!(
            error
                .0
                .contains("attachment type does not match the upload"),
            "{}",
            error.0
        );

        let pending = stage(&files, "dupe.png", &[5, 5, 5]);
        let before = names(&files, "pending-").len();
        let error = claim_pending_attachments(
            files.attachments_dir(),
            "thread-claims-dupe",
            &[pending.clone(), pending],
        )
        .unwrap_err();
        assert!(error.0.contains("Duplicate attachment ids"));
        assert_eq!(names(&files, "thread-claims-dupe-"), Vec::<String>::new());
        assert_eq!(
            names(&files, "pending-").len(),
            before,
            "pending sources survive for a corrected retry"
        );

        let error = claim_pending_attachments(
            files.attachments_dir(),
            "***",
            &[stage(&files, "x.png", &[1])],
        )
        .unwrap_err();
        assert!(error.0.contains("invalid thread id"), "{}", error.0);
    }

    #[test]
    fn rejects_oversized_image_batches_before_copying() {
        let (_directory, files) = service();
        let pending = stage(&files, "pending.png", &[1]);
        let images: Vec<Value> = (0..9)
            .map(|index| json!({"type":"image","id":format!("existing-image-{index}"),"name":format!("{index}.png"),"mimeType":"image/png","sizeBytes":10*1024*1024}))
            .collect();
        let error = claim_pending_attachments(
            files.attachments_dir(),
            "thread-image-budget",
            &[vec![pending], images].concat(),
        )
        .unwrap_err();
        assert!(error.0.contains("80 MiB"));
        assert_eq!(
            std::fs::read_dir(files.attachments_dir()).unwrap().count(),
            1
        );
    }

    #[test]
    fn unused_copies_are_released_after_acceptance() {
        let (_directory, files) = service();
        let one = stage(&files, "one.png", &[1]);
        let two = stage(&files, "two.png", &[2]);
        let claimed =
            claim_pending_attachments(files.attachments_dir(), "thread-release", &[one, two])
                .unwrap();
        release_unused_claims(
            files.attachments_dir(),
            &claimed.claimed_paths,
            &claimed.attachments[..1],
        );
        assert!(claimed.claimed_paths[0].exists());
        assert!(!claimed.claimed_paths[1].exists());
        release_claimed_attachments(&claimed.claimed_paths);
        assert!(!claimed.claimed_paths[0].exists());
    }
}
