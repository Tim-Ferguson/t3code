//! Devin's subagents: `extractDevinSubagentUpdate` in `DevinAcp.ts`. Devin names a subagent in the metadata
//! of the tool call that starts or ends it, and its own session id is the agent id.
use crate::xai_extension::SubagentUpdate;
use serde_json::Value;

/// The subagent update a tool call's `cognition.ai/subagent_started` or `_completed` metadata carries.
pub(crate) fn subagent_update(tool: &Value) -> Option<SubagentUpdate> {
    let meta = &tool["data"]["meta"];
    let completed = meta["cognition.ai/subagent_completed"].as_object().map(|_| &meta["cognition.ai/subagent_completed"]);
    let started = meta["cognition.ai/subagent_started"].as_object().map(|_| &meta["cognition.ai/subagent_started"]);
    let data = completed.or(started)?;
    let agent_id = data["agentId"].as_str()?.to_owned();
    let text = |value: Option<&Value>, key: &str| value.and_then(|value| value[key].as_str()).map(str::to_owned);
    Some(SubagentUpdate {
        native_task_id: agent_id.clone(),
        child_session_id: Some(agent_id),
        prompt: text(started, "task").unwrap_or_default(),
        title: text(started, "title"),
        model: text(started, "model"),
        status: match completed { Some(done) if done["success"] == false => "failed", Some(_) => "completed", None => "running" },
        result: text(completed, "summary"),
        suppress_normal_tool: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_metadata_names_a_subagent_as_the_source_does() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/devin-subagent.json")).unwrap();
        for row in rows {
            let found = subagent_update(&row["input"]).map(|update| {
                let mut value = json!({"nativeTaskId":update.native_task_id,"childSessionId":update.child_session_id,"prompt":update.prompt,"title":update.title,"model":update.model,"status":update.status,"result":update.result});
                if update.suppress_normal_tool {
                    value["suppressNormalTool"] = json!(true);
                }
                value
            });
            // The source leaves `suppressNormalTool` unset, which means true.
            let mut expected = row["update"].clone();
            if expected.is_object() {
                expected["suppressNormalTool"] = json!(true);
            }
            assert_eq!(json!(found), expected, "{}", row["input"]);
        }
    }
}
