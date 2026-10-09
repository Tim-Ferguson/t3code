//! Attachments on the answers to a provider's question (`ThreadMessageIntake.ts` and
//! `provider/userInputAttachments.ts`): each question's uploads are claimed into the thread's
//! store, the answer gains a line naming every attached file by its path on this server, and
//! the command that reaches the orchestrator carries the claimed copies. The provider's answer
//! protocol is unchanged: it only ever sees text.
use crate::{
    attachment_claims::{ClaimedDispatchError, claim_pending_attachments, release_claimed_attachments, release_unused_claims, validate_attachment_limits},
    attachment_service::AttachmentService,
    attachment_store::{AttachmentRef, resolve_attachment_path},
    persistence::{Receipt, Store, StoreError},
};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// `Attached <type> "<name>": "<path>"` for each attachment of a question, one per line, added
/// to that question's answer: after an array answer's items, after a text answer on a new
/// paragraph, or as the answer itself.
fn append_paths(answers: &Value, by_question: &Map<String, Value>, directory: &Path) -> Result<Value, ClaimedDispatchError> {
    let mut answers = answers.as_object().cloned().unwrap_or_default();
    for (question, attachments) in by_question {
        let attachments = attachments.as_array().map(Vec::as_slice).unwrap_or(&[]);
        if attachments.is_empty() {
            continue;
        }
        let mut references = vec![];
        for attachment in attachments {
            let name = attachment["name"].as_str().unwrap_or("");
            let path = AttachmentRef::from_value(attachment).and_then(|reference| resolve_attachment_path(directory, &reference)).filter(|path| path.exists());
            let Some(path) = path else {
                return Err(ClaimedDispatchError::Claim(format!("Attachment '{name}' is no longer available. Attach it again.")));
            };
            references.push(format!("Attached {} {}: {}", attachment["type"].as_str().unwrap_or(""), json!(name), json!(path.to_string_lossy())));
        }
        let text = references.join("\n");
        let next = match answers.get(question) {
            Some(Value::Array(items)) => Value::Array(items.iter().cloned().chain([json!(text)]).collect()),
            Some(Value::String(answer)) if !answer.is_empty() => json!(format!("{answer}\n\n{text}")),
            _ => json!(text),
        };
        answers.insert(question.clone(), next);
    }
    Ok(Value::Object(answers))
}

/// The copies claimed for an answer, and the command rewritten to carry them.
fn prepare(directory: &Path, command: &Value, by_question: &Map<String, Value>) -> Result<(Value, Vec<PathBuf>), ClaimedDispatchError> {
    let thread_id = command["threadId"].as_str().unwrap_or("");
    let all: Vec<Value> = by_question.values().filter_map(Value::as_array).flatten().cloned().collect();
    validate_attachment_limits(&all).map_err(|error| ClaimedDispatchError::Claim(error.0))?;
    // One rollback boundary for every question: a failure before dispatch removes every new copy.
    let mut claimed_paths = vec![];
    let mut claimed = Map::new();
    for (question, attachments) in by_question {
        let items = attachments.as_array().map(Vec::as_slice).unwrap_or(&[]);
        match claim_pending_attachments(directory, thread_id, items) {
            Ok(result) => {
                claimed_paths.extend(result.claimed_paths);
                claimed.insert(question.clone(), Value::Array(result.attachments));
            }
            Err(error) => {
                release_claimed_attachments(&claimed_paths);
                return Err(ClaimedDispatchError::Claim(error.0));
            }
        }
    }
    let answers = match append_paths(&command["answers"], &claimed, directory) {
        Ok(answers) => answers,
        Err(error) => {
            release_claimed_attachments(&claimed_paths);
            return Err(error);
        }
    };
    let mut rewritten = command.clone();
    rewritten["answers"] = answers;
    rewritten["attachmentsByQuestionId"] = Value::Object(claimed);
    Ok((rewritten, claimed_paths))
}

/// `runtime-request.respond` with attachments; any other command passes through.
pub fn respond_with_claims(
    files: Option<&AttachmentService>,
    store: &Store,
    command: &Value,
    dispatch: impl FnOnce(&Value) -> Result<Receipt, StoreError>,
) -> Result<Receipt, ClaimedDispatchError> {
    let by_question = command["attachmentsByQuestionId"].as_object().filter(|map| !map.is_empty());
    let (Some(by_question), "runtime-request.respond") = (by_question, command["type"].as_str().unwrap_or("")) else {
        return dispatch(command).map_err(ClaimedDispatchError::Store);
    };
    let files = files.ok_or_else(|| ClaimedDispatchError::Claim("Attachment storage is not configured.".into()))?;
    let (rewritten, claimed_paths) = prepare(files.attachments_dir(), command, by_question)?;
    match dispatch(&rewritten) {
        Err(error) => {
            release_claimed_attachments(&claimed_paths);
            Err(ClaimedDispatchError::Store(error))
        }
        Ok(receipt) if receipt.status == "rejected" => {
            release_claimed_attachments(&claimed_paths);
            Ok(receipt)
        }
        Ok(receipt) => {
            // The resolved item proves which copies the accepted answer referenced; a replayed
            // receipt reports the first attempt's answer, so this attempt's copies go unreferenced.
            // With no resolved item the outcome is ambiguous and everything stays.
            let projection = store.projection("thread", command["threadId"].as_str().unwrap_or("")).ok().flatten();
            let answered: Vec<&Value> = projection
                .iter()
                .flat_map(|projection| projection["turnItems"].as_array().into_iter().flatten())
                .filter(|item| item["type"] == "user_input_request" && item["requestId"] == command["requestId"] && item["status"] != "waiting")
                .collect();
            if !answered.is_empty() {
                let referenced: Vec<Value> = answered
                    .iter()
                    .flat_map(|item| item["questionAnswer"]["attachmentsByQuestionId"].as_object().into_iter().flat_map(|map| map.values().filter_map(Value::as_array).flatten().cloned()))
                    .collect();
                release_unused_claims(files.attachments_dir(), &claimed_paths, &referenced);
            }
            Ok(receipt)
        }
    }
}
