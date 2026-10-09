//! Native event identity is captured at admission, before a restart command can
//! advance the durable run's active attempt and root.
use crate::{codex_runtime::find, persistence::StoreError};
use serde_json::{Value, json};

/// A delegated delivery is valid only while its parent cohort still owns this
/// exact generation and message. Delivery rechecks this after durable planning.
pub(crate) fn steering_delivery_is_current(view: &Value, message: &Value) -> bool {
    let Some(owner) = message.get("delegatedCompletion") else {
        return true;
    };
    view["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|run| run["id"] == owner["parentRunId"])
        .is_some_and(|run| {
            let cohort = &run["delegatedCompletion"];
            cohort["disposition"] == "open"
                && cohort["delivery"]["messageId"] == message["id"]
                && cohort["delivery"]["generation"] == owner["generation"]
                && cohort["delivery"]["taskIds"]
                    .as_array()
                    .is_some_and(|tasks| !tasks.is_empty())
        })
}

#[derive(Clone, Debug)]
pub(crate) struct TurnContext {
    pub(crate) run_id: String,
    pub(crate) attempt_id: Value,
    pub(crate) root_node_id: Value,
    pub(crate) provider_thread_id: Value,
    pub(crate) provider_turn_id: Value,
    pub(crate) provider_instance_id: Value,
}
impl TurnContext {
    pub(crate) fn capture(view: &Value, run_id: &str) -> Result<Self, StoreError> {
        let run = find(view, "runs", &json!(run_id))?;
        let attempt = find(view, "attempts", &run["activeAttemptId"])?;
        Ok(Self {
            run_id: run_id.into(),
            attempt_id: attempt["id"].clone(),
            root_node_id: run["rootNodeId"].clone(),
            provider_thread_id: run["providerThreadId"].clone(),
            provider_turn_id: attempt["providerTurnId"].clone(),
            provider_instance_id: run["providerInstanceId"].clone(),
        })
    }
    pub(crate) fn is_current(&self, view: &Value) -> bool {
        view["runs"].as_array().into_iter().flatten().any(|run| {
            run["id"] == self.run_id
                && run["activeAttemptId"] == self.attempt_id
                && run["rootNodeId"] == self.root_node_id
        })
    }
    /// Preserve current projection data while overriding execution identity:
    /// restart planners may already have published the next active attempt.
    pub(crate) fn rows<'a>(&self, view: &'a Value) -> Result<(Value, &'a Value), StoreError> {
        let mut run = find(view, "runs", &json!(self.run_id))?.clone();
        run["activeAttemptId"] = self.attempt_id.clone();
        run["rootNodeId"] = self.root_node_id.clone();
        run["providerThreadId"] = self.provider_thread_id.clone();
        run["providerInstanceId"] = self.provider_instance_id.clone();
        let attempt = find(view, "attempts", &self.attempt_id)?;
        Ok((run, attempt))
    }
    pub(crate) fn owns_node(&self, node: &Value) -> bool {
        node["id"] == self.root_node_id || node["rootNodeId"] == self.root_node_id
    }
    pub(crate) fn owns_item(&self, view: &Value, item: &Value) -> bool {
        item["runId"] == self.run_id
            && ((!self.provider_turn_id.is_null()
                && item["providerTurnId"] == self.provider_turn_id)
                || view["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|node| node["id"] == item["nodeId"] && self.owns_node(node)))
    }
}
