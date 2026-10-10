//! Session updates that are not text, plans or plain tool calls: usage and session info, tool
//! content chunks, compaction, "action required" and the updates this client does not know. The
//! tool-shaped ones become synthetic tool updates, so they merge and project like any other tool.
use super::*;

/// `usage_update`, or an idle `state_update` that carries usage, as the thread's context usage.
pub(super) fn usage_snapshot(update: &Value) -> Option<Value> {
    match update["sessionUpdate"].as_str()? {
        "usage_update" => {
            let mut usage = json!({"usedTokens":update["used"]});
            if update["size"].as_u64().is_some_and(|size| size > 0) {
                usage["maxTokens"] = update["size"].clone();
            }
            let currency = update["cost"]["currency"].as_str().map(|text| t3_contracts::trim_wire_string(text).chars().take(32).collect::<String>()).filter(|text| !text.is_empty());
            if let (Some(currency), Some(amount)) = (currency, update["cost"].get("amount")) {
                usage["cost"] = json!({"amount":amount,"currency":currency});
            }
            Some(usage)
        }
        "state_update" if update["state"] == "idle" && update["usage"]["totalTokens"].is_number() => Some(json!({"usedTokens":update["usage"]["totalTokens"]})),
        _ => None,
    }
}

/// `session_info_update` as the title and update time the agent reports (null clears one).
pub(super) fn session_info(update: &Value) -> Value {
    let bounded = |value: &Value, limit: usize| -> Option<Value> {
        if value.is_null() && !value.is_string() {
            return None;
        }
        let text: String = t3_contracts::trim_wire_string(value.as_str()?).chars().take(limit).collect();
        (!text.is_empty()).then(|| json!(text))
    };
    let mut metadata = serde_json::Map::new();
    for (key, limit) in [("title", 1_024), ("updatedAt", 128)] {
        if update.get(key).is_some_and(Value::is_null) {
            metadata.insert(key.into(), Value::Null);
        } else if let Some(value) = bounded(&update[key], limit) {
            metadata.insert(key.into(), value);
        }
    }
    Value::Object(metadata)
}

/// Devin's typed extension metadata (`normalizeDevinSessionUpdate`, `normalizeDevinToolCall`): its
/// streaming message id becomes the chunk's `messageId`, and a tool without a useful title takes the
/// name of the inference tool it ran.
pub(super) fn devin_update(value: &mut Value) {
    let meta = value["update"]["_meta"].clone();
    // A subagent's updates travel on the root session, tagged with the agent that sent them: they are
    // that agent's session's.
    if let Some(parent) = meta["cognition.ai/subagent_context"]["parentAgentId"].as_str().filter(|parent| *parent != "root") {
        value["sessionId"] = json!(parent);
    }
    match value["update"]["sessionUpdate"].as_str() {
        Some("agent_message_chunk" | "agent_thought_chunk") => {
            if let Some(id) = meta["cognition.ai/streamingMessageId"].as_str() {
                value["update"]["messageId"] = json!(id);
            }
        }
        Some("tool_call" | "tool_call_update") => {
            let title = value["update"]["title"].as_str().map(str::trim).unwrap_or("");
            if let Some(name) = meta["cognition.ai/inferenceToolName"].as_str() {
                if title.is_empty() || title == "Tool" {
                    value["update"]["title"] = json!(name);
                }
            }
        }
        _ => {}
    }
}

fn text_of_output(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Object(map) if map.get("text").is_some_and(Value::is_string) => map["text"].as_str().unwrap().to_owned(),
        Value::Null => String::new(),
        other => crate::acp_tools::output_text(other).unwrap_or_default(),
    }
}

impl Actor {
    /// Usage and session info update the thread's context usage and native metadata, in or out of a turn.
    pub(super) fn thread_state(&mut self, update: &Value) -> Result<bool, StoreError> {
        let usage = usage_snapshot(update);
        let tag = update["sessionUpdate"].as_str().unwrap_or("");
        if usage.is_none() && tag != "session_info_update" {
            return Ok(false);
        }
        if let Some(usage) = usage {
            self.context_usage = Some(usage);
        } else {
            let info = session_info(update);
            let metadata = self.native_metadata.get_or_insert_with(|| json!({}));
            for (key, value) in info.as_object().unwrap() {
                metadata[key] = value.clone();
            }
        }
        let now = at(Utc::now());
        let session = self.session.setup.session_id.clone();
        commit(&self.store, &self.thread_id, |view| {
            let Some(thread) = view["providerThreads"].as_array().unwrap().iter().find(|thread| thread["providerInstanceId"] == self.instance_id && thread["nativeThreadRef"]["nativeId"] == session) else {
                return Ok(vec![]);
            };
            let mut thread = thread.clone();
            if let Some(usage) = &self.context_usage {
                thread["contextUsage"] = usage.clone();
            }
            if let Some(metadata) = &self.native_metadata {
                let mut merged = thread["nativeMetadata"].as_object().cloned().unwrap_or_default();
                merged.extend(metadata.as_object().unwrap().clone());
                thread["nativeMetadata"] = Value::Object(merged);
            }
            thread["updatedAt"] = json!(now);
            // The original's adapter republishes the thread as it last cached it (active), so a title that
            // arrives after the turn ended leaves a Grok thread active until its next turn.
            if self.flavor.republishes_cached_thread() {
                thread["status"] = json!("active");
            }
            Ok(vec![("provider-thread.updated", thread)])
        })?;
        Ok(true)
    }

    fn previous_output(&self, id: &str) -> String {
        self.tools.get(id).map(|tool| text_of_output(&tool.wire["data"]["rawOutput"])).unwrap_or_default()
    }

    /// The updates that become tool items. Returns false for an update this does not handle.
    pub(super) fn other_update(&mut self, update: &Value) -> Result<bool, StoreError> {
        match update["sessionUpdate"].as_str() {
            Some("tool_call_content_chunk") => {
                let content = &update["content"];
                let text = match content["type"].as_str() {
                    Some("content") => crate::acp_model::content_display_text(&content["content"]),
                    Some("diff") => content["patch"]["text"].as_str().or_else(|| content["newText"].as_str()).map(ToOwned::to_owned),
                    _ => None,
                };
                if let (Some(text), Some(id)) = (text, update["toolCallId"].as_str()) {
                    let output = format!("{}{text}", self.previous_output(id));
                    self.tool(&json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"in_progress","rawOutput":{"type":"Text","text":output}}))?;
                }
            }
            Some("compaction_update") => {
                let summary = update["summary"].as_array().into_iter().flatten().filter_map(crate::acp_model::content_display_text).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n");
                let status = match update["status"].as_str() {
                    Some("completed") => "completed",
                    Some("failed" | "cancelled") => "failed",
                    _ => "in_progress",
                };
                let output = update["error"].as_str().filter(|text| !text.is_empty()).map(ToOwned::to_owned).unwrap_or(summary);
                let mut tool = json!({"sessionUpdate":"tool_call","toolCallId":format!("acp-compaction:{}",update["compactionId"].as_str().unwrap_or("")),"title":"Compact context","kind":"think","status":status});
                if !output.is_empty() {
                    tool["rawOutput"] = json!(output);
                }
                self.tool(&tool)?;
            }
            Some("compaction_summary_chunk") => {
                if let Some(text) = crate::acp_model::content_display_text(&update["content"]) {
                    let id = format!("acp-compaction:{}", update["compactionId"].as_str().unwrap_or(""));
                    let output = format!("{}{text}", self.previous_output(&id));
                    self.tool(&json!({"sessionUpdate":"tool_call","toolCallId":id,"title":"Compact context","kind":"think","status":"in_progress","rawOutput":{"type":"Text","text":output}}))?;
                }
            }
            Some("state_update") => {
                let id = format!("{}:requires-action", self.native_turn());
                if update["state"] == "requires_action" {
                    self.tool_with(&json!({"sessionUpdate":"tool_call","toolCallId":id,"kind":"other","title":"Action required","rawInput":{"state":"requires_action"}}), Some("requiresAction"))?;
                } else if self.tools.get(&id).is_some_and(|tool| tool.wire["status"] == "requiresAction") {
                    self.tool(&json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"completed"}))?;
                }
            }
            Some("_t3_unknown") => {
                let kind: String = update["originalSessionUpdate"].as_str().map(|text| text.chars().take(128).collect()).filter(|text: &String| !text.is_empty()).unwrap_or_else(|| "unknown".into());
                self.tool(&json!({"sessionUpdate":"tool_call","toolCallId":format!("{}:unsupported:{kind}",self.native_turn()),"kind":"other","title":format!("Unsupported ACP update: {kind}"),"status":"completed"}))?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn native_turn(&self) -> String {
        format!("{}:turn:{}", self.session.setup.session_id, self.turn_ordinal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn usage_and_session_info_follow_the_source_bounds() {
        assert_eq!(usage_snapshot(&json!({"sessionUpdate":"usage_update","used":10,"size":0})), Some(json!({"usedTokens":10})));
        assert_eq!(usage_snapshot(&json!({"sessionUpdate":"usage_update","used":10,"size":200,"cost":{"amount":1.5,"currency":" USD "}})), Some(json!({"usedTokens":10,"maxTokens":200,"cost":{"amount":1.5,"currency":"USD"}})));
        assert_eq!(usage_snapshot(&json!({"sessionUpdate":"usage_update","used":10,"size":5,"cost":{"amount":1,"currency":"  "}})), Some(json!({"usedTokens":10,"maxTokens":5})));
        assert_eq!(usage_snapshot(&json!({"sessionUpdate":"state_update","state":"idle","usage":{"totalTokens":7}})), Some(json!({"usedTokens":7})));
        assert_eq!(usage_snapshot(&json!({"sessionUpdate":"state_update","state":"running","usage":{"totalTokens":7}})), None);
        assert_eq!(session_info(&json!({"title":"  Fix it  ","updatedAt":null})), json!({"title":"Fix it","updatedAt":null}));
        assert_eq!(session_info(&json!({"title":"   "})), json!({}));
    }
    #[test]
    fn devin_metadata_names_streaming_messages_and_untitled_tools() {
        let mut chunk = json!({"update":{"sessionUpdate":"agent_message_chunk","content":{},"_meta":{"cognition.ai/streamingMessageId":"m7"}}});
        devin_update(&mut chunk);
        assert_eq!(chunk["update"]["messageId"], "m7");
        let mut tool = json!({"update":{"sessionUpdate":"tool_call","toolCallId":"t","title":"Tool","_meta":{"cognition.ai/inferenceToolName":"grep"}}});
        devin_update(&mut tool);
        assert_eq!(tool["update"]["title"], "grep");
        let mut titled = json!({"update":{"sessionUpdate":"tool_call","toolCallId":"t","title":"Read a","_meta":{"cognition.ai/inferenceToolName":"grep"}}});
        devin_update(&mut titled);
        assert_eq!(titled["update"]["title"], "Read a");
    }
}
