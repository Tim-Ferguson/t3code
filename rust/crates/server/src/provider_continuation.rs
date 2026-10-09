//! A provider's request for a run it needs to report work that finished outside any turn
//! (`ProviderContinuationRequests` and `ProviderContinuationService` of the original). The adapter
//! has already buffered what the provider produced; the run this starts ingests it, and the message
//! that starts it carries the notification the user reads. Provider-agnostic: any adapter whose
//! provider works between turns can offer one.
use crate::{
    execution::{plan_message_in_transaction},
    persistence::{Store, StoreError, read_projection},
    provider_registry::ProviderRegistry,
};
use chrono::Utc;
use serde_json::{Value, json};

/// The notification a continuation carries when the adapter named no work.
fn generic_notification() -> Value {
    json!({"source":{"kind":"background_task"},"outcome":"updated","summary":"Background activity updated"})
}

/// Whether the message that started a run was an adapter's continuation: the adapter attaches what it
/// buffered instead of sending the text to the provider.
pub(crate) fn is_continuation(message: &Value) -> bool {
    message["createdBy"] == "agent" && message["creationSource"] == "provider"
}

/// Offers a continuation: a queued, agent-created, provider-sourced message with the notification.
/// A run that is active already takes it after that run; an idle thread starts a run at once.
pub(crate) fn offer(store: &Store, providers: &ProviderRegistry, thread_id: &str, detail: Option<&str>, notification: Option<Value>) -> Result<(), StoreError> {
    let message_id = format!("message:{}", uuid::Uuid::new_v4());
    let command_id = format!("provider-continuation:{message_id}");
    let input = json!({
        "type":"message.dispatch","commandId":command_id,"threadId":thread_id,"messageId":message_id,
        "text":detail.unwrap_or("Background task completed."),
        "notification":notification.unwrap_or_else(generic_notification),
        "attachments":[],"dispatchMode":{"type":"queue_after_active"},"createdBy":"agent","creationSource":"provider",
    });
    let decoded: t3_contracts::ProviderCommand = serde_json::from_value(input)?;
    let command = decoded.service_payload()?;
    let now = Utc::now();
    store.dispatch_resolved(
        &command_id,
        "thread",
        "message.dispatch",
        now,
        |_, _| Ok(thread_id.into()),
        |transaction, _| {
            let projection = read_projection(transaction, "thread", thread_id)?.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
            if !projection["thread"]["deletedAt"].is_null() || !projection["thread"]["archivedAt"].is_null() {
                return Err(StoreError::InvalidCommand("Thread is deleted or archived.".into()));
            }
            let driver = providers.driver(projection["thread"]["modelSelection"]["instanceId"].as_str().unwrap_or_default()).map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
            plan_message_in_transaction(&command, &projection, transaction, now, driver)
        },
        crate::thread::reduce,
    )?;
    Ok(())
}
