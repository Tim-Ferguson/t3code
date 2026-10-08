//! Provider-owned selection admission, matching ProviderSelectionTransition.ts.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SelectionTransition {
    ApplyOnNextTurn,
    Reject { reason: String },
}

pub(crate) fn selection_transition(
    driver: &str,
    current: &Value,
    target: &Value,
    capabilities: &Value,
) -> SelectionTransition {
    if matches!(driver, "acpRegistry" | "antigravity")
        && current["model"] != target["model"]
        && capabilities["sessions"]["supportsModelSwitchInSession"] != true
    {
        return SelectionTransition::Reject {
            reason: "The active ACP session does not expose a model-switch capability.".into(),
        };
    }
    // All other original adapters classify complete selections as turn-scoped.
    SelectionTransition::ApplyOnNextTurn
}

/// Match the source's bound provider context. An unloaded native thread keeps
/// its driver identity, but negotiated capability records are not rebound.
pub(crate) fn classify_selection(projection: &Value, target: &Value) -> SelectionTransition {
    let current = &projection["thread"]["modelSelection"];
    let mut sessions: Vec<_> = projection["providerSessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|session| session["providerInstanceId"] == current["instanceId"])
        .collect();
    sessions.sort_by(|left, right| {
        let timestamp = |row: &Value| {
            row["updatedAt"]
                .as_str()
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        };
        timestamp(right)
            .cmp(&timestamp(left))
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    let current_session = sessions
        .iter()
        .copied()
        .find(|session| !matches!(session["status"].as_str(), Some("stopped" | "error")));
    let negotiated = current_session.or_else(|| sessions.first().copied());
    let native_thread = projection["providerThreads"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|thread| {
            thread["id"] == projection["thread"]["activeProviderThreadId"]
                && thread["providerInstanceId"] == current["instanceId"]
                && !thread["nativeThreadRef"].is_null()
        });
    if current_session.is_none() && native_thread.is_none() {
        return SelectionTransition::ApplyOnNextTurn;
    }
    let driver = current_session
        .or(negotiated)
        .or(native_thread)
        .and_then(|record| record["driver"].as_str())
        .unwrap_or("");
    let fallback = serde_json::json!({"sessions":{"supportsModelSwitchInSession":false}});
    let capabilities = negotiated
        .map(|session| &session["capabilities"])
        .unwrap_or(&fallback);
    selection_transition(driver, current, target, capabilities)
}

pub(crate) fn needs_runtime_detach(session: &Value) -> bool {
    !matches!(session["status"].as_str(), Some("stopped" | "error"))
        && session["capabilities"]["sessions"]["supportsRuntimeModeSwitchInSession"] != true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unchanged_provider_selection_and_runtime_detach_source_oracle() {
        let rows: Vec<Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/provider-selection-transitions.json"
        ))
        .unwrap();
        assert_eq!(rows.len(), 325);
        for (index, row) in rows.iter().enumerate() {
            let input = &row["input"];
            let actual = match row["operation"].as_str().unwrap() {
                "selection" => serde_json::to_value(selection_transition(
                    input["driver"].as_str().unwrap(),
                    &input["current"],
                    &input["target"],
                    &input["sessionCapabilities"],
                ))
                .unwrap(),
                "runtimeDetach" => json!(needs_runtime_detach(input)),
                _ => unreachable!(),
            };
            assert_eq!(actual, row["output"], "source witness {index}: {input}");
        }
    }
}
