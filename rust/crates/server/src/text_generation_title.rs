//! The `thread-title.generate` effect (port of `ThreadTitleRegenerationService.ts`): a model names a
//! thread after its first message, or renames it from its history when asked to.
//!
//! The thread carries `titleRegeneration: { requestId, startedAt }` for as long as a title is being
//! written, and the effect only completes the request that is current. Generation can fail or find
//! nothing worth saying and the thread is never worse for it: it keeps the title it has (the first
//! message's own words, which the client seeded) and the marker is cleared. A first title is tried
//! up to three times, two seconds and then four seconds apart; a requested rename is tried once.
use crate::{
    execution::{at, event},
    persistence::{Decision, Effect, Event, NewEffect, Store, read_projection},
    provider_registry::ProviderRegistry,
    text_generation_context::{ContextMessage, format_thread_title_context},
    text_generation_service::{TextGenerationService, TitleInput},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

/// The effect that writes a title: `initial` for a thread's first message, `regenerate` for a rename
/// from its history (`pendingThreadTitleGenerationEffect`).
pub(crate) fn generate_effect(command_id: &str, thread_id: &str, kind: Value, available_at: String) -> NewEffect {
    NewEffect {
        id: format!("effect:{command_id}:thread-title.generate"),
        command_id: command_id.into(),
        thread_id: thread_id.into(),
        request: json!({"type":"thread-title.generate","kind":kind}),
        available_at,
    }
}

/// `isNativeMaintenanceCommand`: `/compact` and `/logout` are not what a thread is about.
fn is_maintenance(text: &str, attachments: &Value) -> bool {
    attachments.as_array().is_none_or(|attachments| attachments.is_empty()) && matches!(crate::text_generation::js_trim(text).to_lowercase().as_str(), "/compact" | "/logout")
}

/// A thread's title being written for its first real message: the thread as it stands once the title
/// is seeded, the event that says so, and the effect that follows.
pub(crate) struct FirstTitle {
    /// The projection the rest of the message is planned against.
    pub projection: Value,
    event: Event,
    effect: NewEffect,
}

impl FirstTitle {
    pub(crate) fn attach(self, decision: Decision) -> Decision {
        match decision {
            Decision::Accepted { mut events, mut effects } => {
                events.insert(0, self.event);
                effects.push(self.effect);
                Decision::Accepted { events, effects }
            }
            other => other,
        }
    }
}

/// Whether `command` is the message that gives its thread a title: the first message of a thread when
/// the client sent a seed, or the first real message after nothing but maintenance commands.
pub(crate) fn first_message_title(command: &Value, projection: &Value, reuse_message: bool, now: DateTime<Utc>) -> Option<FirstTitle> {
    if reuse_message || is_maintenance(command["text"].as_str().unwrap_or(""), &command["attachments"]) {
        return None;
    }
    let messages = projection["messages"].as_array()?;
    let seed = command.get("titleSeed").and_then(Value::as_str);
    let users: Vec<&Value> = messages.iter().filter(|message| message["role"] == "user").collect();
    let only_maintenance = !users.is_empty() && users.iter().all(|message| is_maintenance(message["text"].as_str().unwrap_or(""), &message["attachments"]));
    if !((seed.is_some() && messages.is_empty()) || (command["createdBy"] == "user" && only_maintenance)) {
        return None;
    }
    let (thread_id, command_id) = (command["threadId"].as_str()?, command["commandId"].as_str()?);
    let mut thread = projection["thread"].clone();
    if let Some(seed) = seed {
        thread["title"] = json!(seed);
    }
    thread["titleRegeneration"] = json!({"requestId":command_id,"startedAt":at(now)});
    thread["updatedAt"] = json!(at(now));
    let thread = serde_json::to_value(serde_json::from_value::<t3_contracts::AppThread>(thread).ok()?).ok()?;
    let mut adjusted = projection.clone();
    adjusted["thread"] = thread.clone();
    Some(FirstTitle {
        projection: adjusted,
        event: event(thread_id, command_id, "thread.metadata-updated", thread, now),
        effect: generate_effect(command_id, thread_id, json!({"type":"initial","messageId":command["messageId"]}), at(now)),
    })
}

enum Outcome {
    /// Another request has taken over, or the thread is gone from under it: nothing to complete.
    Stale,
    Complete(Option<String>),
}

/// What to ask the model about, once the thread has been read.
struct Subject {
    cwd: String,
    message: String,
    attachments: Vec<Value>,
    previous_title: Option<String>,
    project_id: String,
}

/// `Complete(None)` stands in for "there is nothing to title from".
fn read_subject(store: &Store, thread_id: &str, request_id: &str, kind: &Value) -> Result<Result<Subject, Outcome>, String> {
    let initial = kind["type"] == "initial";
    let projection = store.projection("thread", thread_id).map_err(|error| error.to_string())?.ok_or_else(|| format!("Thread {thread_id} does not exist."))?;
    let thread = &projection["thread"];
    if thread["titleRegeneration"]["requestId"] != request_id {
        return Ok(Err(Outcome::Stale));
    }
    let project_id = thread["projectId"].as_str().unwrap_or("").to_string();
    let project = store.projection("project", &project_id).map_err(|error| error.to_string())?;
    let Some(project) = project else { return Ok(Err(Outcome::Complete(None))) };
    let messages = projection["messages"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let streaming = |message: &Value| message["streaming"] == true;
    let (message, attachments) = if initial {
        match messages.iter().find(|message| message["id"] == kind["messageId"] && !streaming(message)) {
            Some(message) => (message["text"].as_str().unwrap_or("").to_string(), message["attachments"].as_array().cloned().unwrap_or_default()),
            None => (String::new(), vec![]),
        }
    } else {
        let history: Vec<ContextMessage> = messages
            .iter()
            .filter(|message| !streaming(message) && matches!(message["role"].as_str(), Some("user" | "assistant")))
            .map(|message| ContextMessage {
                role: message["role"].as_str().unwrap_or(""),
                text: message["text"].as_str().unwrap_or(""),
                attachments: message["attachments"].as_array().map(Vec::as_slice).unwrap_or(&[]),
            })
            .collect();
        let context = format_thread_title_context(&history);
        (context.message, context.attachments)
    };
    if message.is_empty() && attachments.is_empty() {
        return Ok(Err(Outcome::Complete(None)));
    }
    let cwd = thread["worktreePath"].as_str().or_else(|| project["workspaceRoot"].as_str()).unwrap_or("").to_string();
    Ok(Ok(Subject { cwd, message, attachments, previous_title: (!initial).then(|| thread["title"].as_str().unwrap_or("").to_string()), project_id }))
}

async fn attempt(service: &TextGenerationService, store: &Store, thread_id: &str, request_id: &str, kind: &Value) -> Result<Outcome, String> {
    let subject = match read_subject(store, thread_id, request_id, kind)? {
        Ok(subject) => subject,
        Err(outcome) => return Ok(outcome),
    };
    let settings = service.project_settings(&subject.project_id);
    let result = Box::pin(service.thread_title(&TitleInput {
        cwd: &subject.cwd,
        message: &subject.message,
        previous_title: subject.previous_title.as_deref(),
        attachments: &subject.attachments,
        model_selection: settings["textGenerationModelSelection"].clone(),
    }))
    .await
    .map_err(|error| error.message())?;
    let generated = crate::text_generation::js_trim(&result.title);
    let unchanged = subject.previous_title.as_deref().is_some_and(|previous| generated == crate::text_generation::js_trim(previous));
    Ok(Outcome::Complete((generated != "New thread" && !unchanged).then_some(result.title)))
}

/// `thread.title.regeneration.complete`: lands the title (if any) and clears the marker, when the
/// request is still the current one.
fn complete(store: &Store, thread_id: &str, request_id: &str, title: Option<&str>) -> Result<(), String> {
    let now = Utc::now();
    let command_id = format!("{request_id}:title-complete");
    store
        .dispatch(&command_id, "thread", thread_id, "thread.title.regeneration.complete", now, |transaction| {
            let Some(projection) = read_projection(transaction, "thread", thread_id)? else {
                return Ok(Decision::Rejected(json!({"_tag":"OrchestratorDispatchError","commandId":command_id,"commandType":"thread.title.regeneration.complete","cause":format!("Thread {thread_id} does not exist.")})));
            };
            let mut thread = projection["thread"].clone();
            if thread["titleRegeneration"]["requestId"] != request_id {
                return Ok(Decision::Accepted { events: vec![], effects: vec![] });
            }
            if let Some(title) = title {
                thread["title"] = json!(title);
            }
            thread["titleRegeneration"] = Value::Null;
            thread["updatedAt"] = json!(crate::execution::at(now));
            let thread = serde_json::from_value::<t3_contracts::AppThread>(thread).map_err(|error| crate::persistence::StoreError::InvalidCommand(error.to_string()))?;
            let thread = serde_json::to_value(thread).map_err(|error| crate::persistence::StoreError::InvalidCommand(error.to_string()))?;
            Ok(Decision::Accepted { events: vec![event(thread_id, &command_id, "thread.metadata-updated", thread, now)], effects: vec![] })
        }, crate::thread::reduce)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Runs one title request to its end. Failing to write a title is logged and completes the request
/// without one; only failing to record that fails the effect.
pub(crate) async fn execute(store: &Store, providers: &ProviderRegistry, effect: &Effect) -> Result<(), String> {
    let service = TextGenerationService::new(providers.clone(), store.clone());
    execute_with(&service, store, providers, effect).await
}

pub(crate) async fn execute_with(service: &TextGenerationService, store: &Store, providers: &ProviderRegistry, effect: &Effect) -> Result<(), String> {
    let (thread_id, request_id, kind) = (effect.thread_id.as_str(), effect.command_id.as_str(), &effect.request["kind"]);
    let retries = if kind["type"] == "initial" { 2 } else { 0 };
    let mut delay = providers.text_generation_options().retry_delay;
    let mut attempts = 0;
    let outcome = loop {
        match Box::pin(attempt(service, store, thread_id, request_id, kind)).await {
            Ok(outcome) => break outcome,
            Err(error) if attempts < retries => {
                tracing::debug!(thread = %thread_id, %error, "Thread title generation failed; trying again");
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempts += 1;
            }
            Err(error) => {
                tracing::warn!(thread = %thread_id, request = %request_id, %error, "Thread title generation failed");
                break Outcome::Complete(None);
            }
        }
    };
    match outcome {
        Outcome::Stale => Ok(()),
        Outcome::Complete(title) => complete(store, thread_id, request_id, title.as_deref()),
    }
}
