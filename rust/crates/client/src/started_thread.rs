//! Source ChatView.logic started-session model transition eligibility.
use t3_contracts::{ModelSelection, ProviderInstanceId, ServerProvider};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelChangeBlock {
    pub title: &'static str,
    pub description: &'static str,
}
pub fn model_change_block(
    providers: &[ServerProvider],
    has_started_session: bool,
    supports_handoff: bool,
    current: &ModelSelection,
    current_provider_instance: Option<&ProviderInstanceId>,
    next: &ModelSelection,
) -> Option<ModelChangeBlock> {
    if !has_started_session {
        return None;
    }
    let current_instance = current_provider_instance.unwrap_or(&current.instance_id);
    if current_instance == &next.instance_id && current.model == next.model {
        return None;
    }
    if current_instance != &next.instance_id {
        return (!supports_handoff).then_some(ModelChangeBlock {
            title: "Start a new chat to switch providers",
            description: "This thread does not support switching providers after it has started.",
        });
    }
    if [current_instance, &next.instance_id]
        .into_iter()
        .any(|instance| {
            providers
                .iter()
                .find(|provider| &provider.instance_id == instance)
                .is_some_and(|provider| {
                    provider.requires_new_thread_for_model_change.flatten() == Some(true)
                })
        })
    {
        return Some(ModelChangeBlock {
            title: "Start a new chat to change models",
            description: "This provider does not allow switching models after a conversation has started.",
        });
    }
    None
}

/// Source threadWorkflows.resolveThreadProviderSession / threadSupportsProviderHandoff.
/// The projection has already passed the shared typed boundary before reaching state.
pub fn supports_provider_handoff(projection: &serde_json::Value) -> bool {
    use serde_json::Value;
    fn rows<'a>(p: &'a Value, key: &str) -> &'a [Value] {
        p[key].as_array().map(Vec::as_slice).unwrap_or_default()
    }
    let active = rows(projection, "runs").iter().rev().find(|run| {
        matches!(
            run["status"].as_str(),
            Some("preparing" | "starting" | "running" | "waiting")
        )
    });
    let thread = &projection["thread"];
    let provider_thread_id = active
        .and_then(|run| run["providerThreadId"].as_str())
        .or_else(|| thread["activeProviderThreadId"].as_str());
    let attached = provider_thread_id
        .and_then(|id| {
            rows(projection, "providerThreads")
                .iter()
                .find(|row| row["id"] == id)
        })
        .or_else(|| {
            rows(projection, "providerThreads").iter().find(|row| {
                row["appThreadId"] == thread["id"] && row["providerSessionId"].as_str().is_some()
            })
        });
    let session_id = attached.and_then(|row| row["providerSessionId"].as_str());
    let session = if let Some(id) = session_id {
        rows(projection, "providerSessions")
            .iter()
            .find(|row| row["id"] == id)
    } else {
        rows(projection, "providerSessions")
            .iter()
            .rev()
            .find(|row| !matches!(row["status"].as_str(), Some("stopped" | "error")))
    };
    if let Some(session) = session {
        return session["capabilities"]["sessions"]["supportsProviderSwitchingViaHandoff"] == true;
    }
    if active.is_some() {
        return false;
    }
    if thread["historyOrigin"] == "v1_import" || rows(projection, "runs").is_empty() {
        return true;
    }
    rows(projection, "providerThreads").iter().any(|row| {
        row["id"] == thread["activeProviderThreadId"]
            && row["appThreadId"] == thread["id"]
            && row["providerInstanceId"] == thread["modelSelection"]["instanceId"]
            && !row["nativeThreadRef"].is_null()
    })
}

/// Source threadExecution.deriveReportedModelSelection; metadata from another
/// provider thread or configured instance must never influence the picker.
pub fn reported_model_selection(projection: &serde_json::Value) -> Option<ModelSelection> {
    projection["providerThreads"]
        .as_array()?
        .iter()
        .find(|candidate| {
            candidate["id"] == projection["thread"]["activeProviderThreadId"]
                && candidate["providerInstanceId"]
                    == projection["thread"]["modelSelection"]["instanceId"]
        })
        .and_then(|candidate| {
            serde_json::from_value(candidate["nativeMetadata"]["modelSelection"].clone()).ok()
        })
}
