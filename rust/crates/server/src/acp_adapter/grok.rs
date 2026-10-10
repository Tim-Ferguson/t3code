//! What the actor does with Grok's extension events: a question for the user, and the plan Grok finished.
use super::*;
use crate::{grok_flavor::Event, xai_extension as xai};

impl Actor {
    pub(super) async fn grok_event(&mut self, event: Event) -> Result<(), StoreError> {
        match event {
            Event::PromptSettled => {
                // The prompt's task has its result; the turn ends here, in order with the updates around it.
                match self.prompt.take() {
                    Some(prompt) => self.finish(prompt.await.unwrap_or_else(|error| Err(AcpError::Transport(error.to_string())))),
                    None => Ok(()),
                }
            }
            Event::Question { params, response, written } => self.grok_question(params, response, written),
            Event::ExitPlan { params, response } => {
                // Grok intercepts exit_plan_mode and asks the client to approve. The plan becomes T3's
                // proposed-plan card and the native gate is abandoned, so the turn does not hang.
                let fallback = self.last_proposed_plan();
                self.capture_proposed_plan(&xai::exit_plan_markdown(&params, fallback.as_deref()))?;
                let _ = response.send(Ok(xai::exit_plan_captured_response()));
                Ok(())
            }
        }
    }
    /// The markdown last captured for the active turn, the exit gate's fallback.
    fn last_proposed_plan(&self) -> Option<String> {
        self.proposed_plan.as_ref().filter(|(turn, _, _)| *turn == self.native_turn()).map(|(_, markdown, _)| markdown.clone())
    }
    /// A completed proposed-plan card for the active turn. One plan id per turn, so repeated captures update one
    /// card; identical markdown within a turn is a no-op.
    pub(super) fn capture_proposed_plan(&mut self, markdown: &str) -> Result<(), StoreError> {
        let markdown = t3_contracts::trim_wire_string(markdown).to_owned();
        if self.active_run.is_none() || markdown.is_empty() {
            return Ok(());
        }
        let turn = self.native_turn();
        if self.proposed_plan.as_ref().is_some_and(|(previous_turn, previous, _)| *previous_turn == turn && *previous == markdown) {
            return Ok(());
        }
        self.close_text_streams()?;
        let plan_id = match &self.proposed_plan {
            Some((previous_turn, _, id)) if *previous_turn == turn => id.clone(),
            _ => uuid::Uuid::new_v4().to_string(),
        };
        self.proposed_plan = Some((turn.clone(), markdown.clone(), plan_id.clone()));
        let native_id = format!("{turn}:proposed-plan");
        let (item_id, node_id) = crate::acp_tools::provider_item_ids_for(self.flavor.driver(), &self.instance_id, &native_id, self.item_identity_v2);
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let (run, attempt) = self.context(view)?;
            let existing = view["turnItems"].as_array().unwrap().iter().find(|item| item["id"] == item_id);
            let mut item = existing.cloned().unwrap_or_else(|| self.base_item(view, &run, attempt, &item_id, &node_id));
            let native = native_ref(self.flavor.driver(), &native_id, false);
            let started = item["startedAt"].clone();
            item["type"] = json!("proposed_plan");
            item["planId"] = json!(plan_id);
            item["markdown"] = json!(markdown);
            item["streaming"] = json!(false);
            item["status"] = json!("completed");
            item["nativeItemRef"] = native.clone();
            item["completedAt"] = json!(now);
            item["updatedAt"] = json!(now);
            let node = json!({"id":node_id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":"plan","status":"completed","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":native,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":now});
            let plan = json!({"id":plan_id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node_id,"kind":"proposed_plan","status":"completed","markdown":markdown});
            Ok(vec![("node.updated", node), ("plan.updated", plan), ("turn-item.updated", item)])
        })
    }
    /// `x.ai/ask_user_question`: the questions become a user-input request, answered with Grok's envelope.
    fn grok_question(&mut self, params: Value, response: oneshot::Sender<Result<Value, AcpError>>, written: oneshot::Receiver<Result<(), AcpError>>) -> Result<(), StoreError> {
        if self.active_run.is_none() {
            let _ = response.send(Ok(xai::ask_user_cancelled_response()));
            return Ok(());
        }
        self.close_text_streams()?;
        let (session, tool_call) = xai::ask_user_identity(&params);
        let native_id = format!("{session}:xai-question:{tool_call}");
        // The adapter keeps an id, a header, the question and its options; multi-select is Grok's alone.
        let questions: Vec<Value> = xai::ask_user_questions(&params).into_iter().map(|mut question| { question.as_object_mut().map(|question| question.remove("multiSelect")); question }).collect();
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
            item["questions"] = json!(questions);
            Ok(vec![("node.updated", node), ("runtime-request.updated", row), ("turn-item.updated", item)])
        })?;
        self.callbacks.insert(request_id, Pending::Native { response, written, params });
        Ok(())
    }
}
