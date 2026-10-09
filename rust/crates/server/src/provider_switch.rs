//! Switching a thread to another provider instance (`ProviderSwitchService`,
//! `ProviderSessionTransitionPolicy`): which of the thread's provider sessions the switch releases, whether
//! the target continues the native conversation of the current instance or takes the conversation over as
//! a handoff, and the provider thread a message to another provider is queued on.
//!
//! The provider registry that answers "is the target enabled" and "do these two instances share a native
//! conversation" is reached through the store, since the queue is advanced by services that hold only the
//! store. `ExecutionService` registers it when it starts.
use crate::{persistence::Store, provider_registry::ProviderRegistry};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Mutex};

static REGISTRIES: Mutex<Option<HashMap<usize, ProviderRegistry>>> = Mutex::new(None);

/// Makes the registry answer for every service over this store.
pub(crate) fn register(store: &Store, providers: &ProviderRegistry) {
    REGISTRIES.lock().unwrap().get_or_insert_with(HashMap::new).insert(store.identity(), providers.clone());
}
pub(crate) fn unregister(store: &Store) {
    if let Some(map) = REGISTRIES.lock().unwrap().as_mut() {
        map.remove(&store.identity());
    }
}
pub(crate) fn registry(store: &Store) -> Option<ProviderRegistry> {
    REGISTRIES.lock().unwrap().as_ref().and_then(|map| map.get(&store.identity()).cloned())
}

fn rows<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map_or(&[], Vec::as_slice)
}
pub(crate) fn live(session: &Value) -> bool {
    !matches!(session["status"].as_str(), Some("stopped" | "error"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    /// The target continues the native conversation: only the session being replaced is released.
    RestartAndResume,
    /// The target takes the conversation over as a handoff: every other instance's sessions are released.
    CreateWithHandoff,
}

#[derive(Debug)]
pub(crate) struct Plan {
    pub instance_changed: bool,
    pub transition: Transition,
    /// Live sessions the switch releases.
    pub release: Vec<String>,
}

/// `ProviderSwitchService.plan` for a selection that may change the instance. A target the registry does
/// not have enabled is refused.
pub(crate) fn plan(registry: Option<&ProviderRegistry>, projection: &Value, target: &Value) -> Result<Plan, String> {
    let current = &projection["thread"]["modelSelection"];
    let instance_changed = current["instanceId"] != target["instanceId"];
    if let Some(registry) = registry {
        let (target_instance, enabled) = (target["instanceId"].as_str().unwrap_or(""), |instance: &str| registry.snapshots().iter().any(|snapshot| snapshot["instanceId"] == instance && snapshot["enabled"] == true));
        if !enabled(target_instance) || registry.driver(target_instance).is_err() {
            return Err("The target provider instance is unavailable.".into());
        }
    }
    let mut sessions: Vec<&Value> = rows(projection, "providerSessions").iter().filter(|session| session["providerInstanceId"] == current["instanceId"]).collect();
    sessions.sort_by(|left, right| right["updatedAt"].as_str().cmp(&left["updatedAt"].as_str()));
    let current_session = sessions.iter().copied().find(|session| live(session));
    let current_thread = rows(projection, "providerThreads").iter().find(|thread| thread["id"] == projection["thread"]["activeProviderThreadId"] && thread["providerInstanceId"] == current["instanceId"] && !thread["nativeThreadRef"].is_null());
    let continues = instance_changed
        && (current_session.is_some() || current_thread.is_some())
        && registry.is_some_and(|registry| registry.continues_across(current["instanceId"].as_str().unwrap_or(""), target["instanceId"].as_str().unwrap_or("")));
    let transition = if continues { Transition::RestartAndResume } else { Transition::CreateWithHandoff };
    let release = rows(projection, "providerSessions")
        .iter()
        .filter(|session| live(session))
        .filter(|session| match transition {
            Transition::RestartAndResume => Some(&session["id"]) == current_session.map(|session| &session["id"]),
            Transition::CreateWithHandoff => instance_changed && session["providerInstanceId"] != target["instanceId"],
        })
        .filter_map(|session| session["id"].as_str().map(str::to_owned))
        .collect();
    Ok(Plan { instance_changed, transition, release })
}

/// `rootProviderThreadsForProvider`: the thread's own provider threads on this instance, newest run first.
pub(crate) fn root_provider_threads<'a>(projection: &'a Value, instance: &Value) -> Vec<&'a Value> {
    let mut threads: Vec<&Value> = rows(projection, "providerThreads").iter().filter(|thread| thread["providerInstanceId"] == *instance && thread["appThreadId"] == projection["thread"]["id"] && thread["ownerNodeId"].is_null()).collect();
    threads.sort_by(|left, right| right["lastRunOrdinal"].as_u64().unwrap_or(0).cmp(&left["lastRunOrdinal"].as_u64().unwrap_or(0)).then_with(|| right["updatedAt"].as_str().cmp(&left["updatedAt"].as_str())));
    threads
}

/// The provider thread a run queued for `model` waits on: the active one for the same instance, else the
/// newest the instance already has for this thread, else a new one that no native thread backs yet.
pub(crate) fn queue_provider_thread(projection: &Value, active: &Value, model: &Value, driver: &str, unique: &str, now: &str) -> (Value, bool) {
    if active["providerInstanceId"] == model["instanceId"] {
        return (active.clone(), false);
    }
    if let Some(existing) = root_provider_threads(projection, &model["instanceId"]).first() {
        return ((*existing).clone(), false);
    }
    let id = format!("provider-thread:provider:{driver}:pending:{unique}");
    (json!({"id":id,"driver":driver,"providerInstanceId":model["instanceId"],"providerSessionId":null,"appThreadId":projection["thread"]["id"],"ownerNodeId":null,"nativeThreadRef":null,"nativeConversationHeadRef":null,"status":"not_loaded","firstRunOrdinal":null,"lastRunOrdinal":null,"handoffIds":[],"forkedFrom":null,"createdAt":now,"updatedAt":now}), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> Value {
        json!({"thread":{"id":"t","modelSelection":{"instanceId":"a","model":"m"},"activeProviderThreadId":"pt-a"},
            "providerSessions":[{"id":"s-a","providerInstanceId":"a","status":"ready","updatedAt":"2026-01-02T00:00:00.000Z"},{"id":"s-a-old","providerInstanceId":"a","status":"stopped","updatedAt":"2026-01-03T00:00:00.000Z"},{"id":"s-c","providerInstanceId":"c","status":"ready","updatedAt":"2026-01-01T00:00:00.000Z"}],
            "providerThreads":[{"id":"pt-a","providerInstanceId":"a","appThreadId":"t","ownerNodeId":null,"nativeThreadRef":{"nativeId":"n"},"lastRunOrdinal":2,"updatedAt":"2026-01-02T00:00:00.000Z"},
                {"id":"pt-b1","providerInstanceId":"b","appThreadId":"t","ownerNodeId":null,"nativeThreadRef":null,"lastRunOrdinal":1,"updatedAt":"2026-01-01T00:00:00.000Z"},
                {"id":"pt-b2","providerInstanceId":"b","appThreadId":"t","ownerNodeId":null,"nativeThreadRef":null,"lastRunOrdinal":3,"updatedAt":"2026-01-01T00:00:00.000Z"}]})
    }

    #[test]
    fn a_switch_without_a_shared_conversation_releases_every_other_instances_live_sessions() {
        let plan = plan(None, &projection(), &json!({"instanceId":"b","model":"m"})).unwrap();
        assert_eq!((plan.instance_changed, plan.transition, plan.release), (true, Transition::CreateWithHandoff, vec!["s-a".to_owned(), "s-c".to_owned()]));
        let same = super::plan(None, &projection(), &json!({"instanceId":"a","model":"other"})).unwrap();
        assert_eq!((same.instance_changed, same.release.len()), (false, 0));
    }

    #[test]
    fn a_message_queued_for_another_instance_waits_on_that_instances_newest_provider_thread_or_a_new_one() {
        let projection = projection();
        let active = &projection["providerThreads"][0];
        assert_eq!(root_provider_threads(&projection, &json!("b"))[0]["id"], "pt-b2");
        let (thread, created) = queue_provider_thread(&projection, active, &json!({"instanceId":"b","model":"m"}), "codex", "run-4", "now");
        assert_eq!((thread["id"].as_str(), created), (Some("pt-b2"), false));
        let (thread, created) = queue_provider_thread(&projection, active, &json!({"instanceId":"z","model":"m"}), "codex", "run-4", "now");
        assert_eq!((thread["nativeThreadRef"].is_null(), thread["providerInstanceId"].as_str(), created), (true, Some("z"), true));
        let (thread, created) = queue_provider_thread(&projection, active, &json!({"instanceId":"a","model":"m"}), "codex", "run-4", "now");
        assert_eq!((thread["id"].as_str(), created), (Some("pt-a"), false));
    }
}
