//! What the actor does for Antigravity beyond the generic ACP path: a question the agent asks through a permission
//! request (`interaction_*`), and the compaction item a `/compact` turn leaves.
use super::*;

impl Actor {
    pub(super) fn antigravity(&self) -> bool {
        matches!(self.flavor, crate::acp_flavor::Flavor::Antigravity(_))
    }
    /// `runtime.setMode(policyMode)`: nothing is sent when the session already runs in the mode; a mode advertised
    /// as a configuration option is set through it, and one the agent only lists as modes through `session/set_mode`.
    pub(super) async fn set_policy_mode(&mut self, mode: &str) -> Result<(), AcpError> {
        let setup = serde_json::to_value(&self.session.setup).unwrap();
        let state = crate::acp_model::session_mode_state(&setup);
        let option = setup["configOptions"].as_array().and_then(|options| options.iter().find(|option| option["category"] == "mode" && option["type"] == "select"));
        // What the agent last reported: the mode option's value when it has one, else the mode state.
        let current = option.map(|option| option["currentValue"].clone()).or_else(|| state.as_ref().map(|state| state["currentModeId"].clone()));
        if current.is_some_and(|current| current == mode) {
            return Ok(());
        }
        let key = match (option, state) {
            (None, Some(_)) => "_t3/session-mode".to_owned(),
            (Some(option), _) => option["id"].as_str().unwrap_or("mode").to_owned(),
            (None, None) => "mode".to_owned(),
        };
        self.session.set_options(&serde_json::Map::from_iter([(key, json!(mode))])).await
    }
    /// A permission request whose choices are a question: it becomes a user-input request, answered with the
    /// option the user picks. An answer that is not one of the choices cancels the request.
    pub(super) fn permission_question(
        &mut self,
        question: Value,
        request: RequestPermissionRequest,
        response: oneshot::Sender<Result<t3_acp::v2::RequestPermissionResponse, AcpError>>,
        written: oneshot::Receiver<Result<(), AcpError>>,
    ) -> Result<(), StoreError> {
        self.close_text_streams()?;
        let tool_call = request.tool_call.tool_call_id.to_string();
        let native_id = format!("{}:question:{tool_call}", request.session_id);
        // The card shows an id, a header, the question and each choice's label and description.
        let shown = json!({
            "id":question["id"],"header":question["header"],"question":question["question"],"multiSelect":false,
            "options":question["options"].as_array().into_iter().flatten().map(|option| json!({"label":option["label"],"description":option["description"]})).collect::<Vec<_>>()
        });
        let request_id = uuid::Uuid::new_v4().to_string();
        let node_id = format!("node:{request_id}");
        let item_id = format!("item:{request_id}");
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let native = native_ref(self.flavor.driver(), &native_id, false);
            let request_native = native_ref(self.flavor.driver(), &tool_call, false);
            let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":"user_input_request","status":"waiting","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":native,"runtimeRequestId":request_id,"checkpointScopeId":null,"startedAt":now,"completedAt":null});
            let row = json!({"id":request_id,"nodeId":node_id,"providerTurnId":attempt["providerTurnId"],"nativeRequestRef":request_native,"kind":"user_input","status":"pending","responseCapability":{"type":"live","providerSessionId":self.session_id},"createdAt":now,"resolvedAt":null});
            let mut item = self.base_item(view, &run, attempt, &item_id, &node_id);
            item["nativeItemRef"] = native;
            item["status"] = json!("waiting");
            item["type"] = json!("user_input_request");
            item["requestId"] = json!(request_id);
            item["questions"] = json!([shown]);
            Ok(vec![("node.updated", node), ("runtime-request.updated", row), ("turn-item.updated", item)])
        })?;
        self.callbacks.insert(request_id, Pending::PermissionQuestion { request, response, written });
        Ok(())
    }
    /// Batch launches carry no completion signals: when the root turn ends, the ones still running become idle (or take the
    /// turn's own end).
    pub(super) fn settle_open_subagents(&mut self, settled: &'static str) -> Result<(), StoreError> {
        for (task, child_session_id) in self.children.open_tasks() {
            self.emit_subagent(&crate::xai_extension::SubagentUpdate {
                native_task_id: task["nativeTaskRef"]["nativeId"].as_str().or(task["id"].as_str()).unwrap_or_default().to_owned(),
                prompt: task["prompt"].as_str().unwrap_or_default().to_owned(),
                title: task["title"].as_str().map(str::to_owned),
                model: task["model"].as_str().map(str::to_owned),
                status: if settled == "completed" { "idle" } else { settled },
                child_session_id,
                result: None,
                suppress_normal_tool: true,
            })?;
        }
        Ok(())
    }
    /// The item a `/compact` turn leaves once it has completed.
    pub(super) fn emit_compaction(&mut self) -> Result<(), StoreError> {
        let native_id = format!("{}:compaction", self.native_turn());
        let (item_id, _) = crate::acp_tools::provider_item_ids_for(self.flavor.driver(), &self.instance_id, &native_id, self.item_identity_v2);
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let mut item = self.base_item(view, &run, attempt, &item_id, run["rootNodeId"].as_str().unwrap_or_default());
            item["nativeItemRef"] = native_ref(self.flavor.driver(), &native_id, false);
            item["type"] = json!("compaction");
            item["driver"] = json!(self.flavor.driver());
            item["status"] = json!("completed");
            item["title"] = json!("Context compacted");
            item["completedAt"] = json!(now);
            Ok(vec![("turn-item.updated", item)])
        })
    }
}
