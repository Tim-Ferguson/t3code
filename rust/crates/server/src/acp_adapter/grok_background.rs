//! Grok's background work in the actor: a turn held open while a command, monitor or subagent it started
//! still runs (`deferFinalizeForBackgroundWork`), the wake turns Grok starts on its own once that work ends,
//! and the continuation runs that deliver them (`enablePostSettleContinuation`). The shared orchestration is
//! `provider_continuation::offer` (the run that reports finished work), the idle clock (the quiet window) and
//! the pending-work derivation (`background_work`): nothing here is Grok-shaped on their side.
use super::*;
use crate::{
    acp_tools::ToolState,
    grok_background::{self as bg, HYDRATION_HOLD_MS, INJECTED_REPORT_HOLD_MS},
    xai_extension as xai,
};

/// What `bufferPostSettleWake` decided about one frame that arrived with no turn running.
pub(super) struct Wake {
    offer: bool,
    stop: bool,
}

fn chunk(update: &Value) -> bool {
    matches!(update["sessionUpdate"].as_str(), Some("agent_message_chunk" | "agent_thought_chunk"))
}

impl Actor {
    pub(super) fn grok(&self) -> bool {
        matches!(self.flavor, crate::acp_flavor::Flavor::Grok(_))
    }
    fn now_ms(&self) -> i64 {
        self.providers.idle_release().clock().now_ms()
    }
    /// `hasDeferredBackgroundWork`: what keeps a settled turn from finishing.
    pub(super) fn has_deferred_work(&self) -> bool {
        let turn = &self.bg.turn;
        !turn.awaiting.is_empty() || !turn.pending_report.is_empty() || bg::blocking_tasks(turn, &self.tools) || self.children.blocks_settlement()
    }
    /// The prompt returned. A Grok turn with background work still running is held open; everything else ends here.
    pub(super) fn settle_prompt(&mut self, result: Result<PromptResponse, AcpError>) -> Result<(), StoreError> {
        if self.grok() && !self.interrupt_requested && self.bg.turn.settled.is_none() {
            if let Ok(response) = &result {
                if self.has_deferred_work() {
                    self.bg.turn.settled = Some(if response.stop_reason == "cancelled" { "cancelled" } else { "completed" });
                    if let Some(instructions) = self.pending_instructions.take() {
                        self.instructions = Some(instructions);
                    }
                    // The agent finished this prompt's reply: background work holds the run open, not the text it sent.
                    return self.close_text_streams();
                }
            }
        }
        self.finish(result)
    }
    /// `rearmDeferredFinalize`: a settled turn with nothing left to wait for starts its quiet window again.
    pub(super) fn rearm(&mut self) {
        if !self.grok() || self.bg.turn.settled.is_none() || self.bg.turn.finalizing || self.active_run.is_none() || self.has_deferred_work() {
            return;
        }
        self.arm_finalize();
    }
    /// `scheduleDeferredFinalize`: the turn settles once it has been quiet for the window.
    fn arm_finalize(&mut self) {
        if self.bg.turn.settled.is_none() || self.bg.turn.finalizing || self.active_run.is_none() || self.interrupt_requested || self.has_deferred_work() {
            return;
        }
        self.bg.turn.finalize_at = Some(self.now_ms() + crate::deferred_finalize::DEBOUNCE_MS);
        self.providers.finish_debounce().arm();
    }
    /// Settles a held turn with the status its prompt returned.
    pub(super) fn finalize_held(&mut self) -> Result<(), StoreError> {
        let status = self.bg.turn.settled.unwrap_or("completed");
        self.bg.turn.finalize_at = None;
        let cancelled = status != "completed" || self.interrupt_requested;
        let response = serde_json::from_value(json!({"stopReason":if cancelled { "cancelled" } else { "end_turn" }})).expect("prompt response");
        self.finish(Ok(response))
    }
    /// The earliest moment a hold or the quiet window ends.
    pub(super) fn next_deadline(&self) -> Option<i64> {
        let turn = &self.bg.turn;
        turn.awaiting.values().chain(turn.pending_report.values()).copied().chain(turn.finalize_at).min()
    }
    /// Whatever a hold or the quiet window waited for has run out of time.
    pub(super) fn timers_due(&mut self) -> Result<(), StoreError> {
        let now = self.now_ms();
        let hydration: Vec<String> = self.bg.turn.awaiting.iter().filter(|(_, at)| **at <= now).map(|(task, _)| task.clone()).collect();
        for task in hydration {
            self.bg.turn.awaiting.shift_remove(&task);
            // End notices keep the tool running until the agent reads its output; complete it so the turn can settle.
            let tool = self.bg.turn.tool_of_task.get(&task).and_then(|call| self.tools.get(call)).filter(|tool| bg::tool_open(tool)).cloned();
            if let Some(mut tool) = tool {
                tool.wire["status"] = json!("completed");
                self.emit_tool_state(tool, None)?;
            }
            self.rearm();
        }
        let reports: Vec<String> = self.bg.turn.pending_report.iter().filter(|(_, at)| **at <= now).map(|(task, _)| task.clone()).collect();
        for task in reports {
            self.bg.turn.pending_report.shift_remove(&task);
            self.rearm();
        }
        if self.bg.turn.finalize_at.is_some_and(|at| at <= now) {
            self.bg.turn.finalize_at = None;
            if self.active_run.is_some() && self.bg.turn.settled.is_some() && !self.interrupt_requested && !self.has_deferred_work() {
                self.finalize_held()?;
            }
        }
        Ok(())
    }

    // --- tools ---------------------------------------------------------------------------------------
    /// The part of `emitTool` about background work: a tool that starts a task registers it, a poll of a task
    /// hydrates the tool that started it. Whether the call hydrated a registered task (so it is no subagent).
    pub(super) fn tool_hooks(&mut self, next: &ToolState, projected: Option<&str>) -> Result<bool, StoreError> {
        if !self.grok() {
            return Ok(false);
        }
        let id = next.wire["toolCallId"].as_str().unwrap_or_default().to_owned();
        if let Some(task) = xai::background_task_id(&next.wire) {
            self.bg.turn.tool_of_task.insert(task.clone(), id.clone());
            if xai::is_persistent_monitor(&next.wire) {
                self.bg.turn.persistent.insert(task.clone());
            }
            let running = projected.map_or_else(|| bg::tool_open(next), |status| matches!(status, "pending" | "inProgress"));
            self.bg.set_running(&task, running);
            // A task that ended while the prompt was open was reported by the turn itself.
            if self.bg.turn.settled.is_none() && !running {
                self.bg.handled.insert(task.clone());
                self.bg.unreported.remove(&task);
            }
        }
        let mut hydrated = false;
        if projected.is_none() {
            let completions: Vec<xai::Mutation> = xai::background_task_completion(&next.wire).into_iter().chain(xai::killed_background_tasks(&next.wire)).collect();
            for completion in completions {
                self.bg.apply_running(&completion.task_id, completion.status);
                if completion.status != "running" {
                    self.bg.handled.insert(completion.task_id.clone());
                    self.bg.unreported.remove(&completion.task_id);
                    self.bg.turn.awaiting.shift_remove(&completion.task_id);
                }
                let target_call = self.bg.turn.tool_of_task.get(&completion.task_id).cloned();
                // A known task is never a phantom subagent for the same poll.
                hydrated |= target_call.is_some();
                let target = target_call.filter(|call| *call != id).and_then(|call| self.tools.get(&call).cloned());
                if let Some(mut target) = target {
                    target.wire["status"] = json!(match completion.status {
                        "running" => "inProgress",
                        "failed" => "failed",
                        _ => "completed",
                    });
                    if !completion.append_output.is_empty() {
                        let shown = target.output_text_or_empty();
                        // The poll is the real output; replace the boilerplate of an end notice.
                        let text = if bg::is_monitor_end_notice(&shown) || shown.trim().is_empty() { completion.append_output.clone() } else { format!("{shown}{}", completion.append_output) };
                        target.set_output_text(&text);
                    }
                    self.emit_tool_state(target, None)?;
                }
            }
        }
        Ok(hydrated)
    }
    /// `emitTool` for a tool state the adapter builds itself: merged into what is known, then shown.
    pub(super) fn emit_tool_state(&mut self, tool: ToolState, projected: Option<&str>) -> Result<(), StoreError> {
        let ended: Vec<&'static str> = ["assistant", "reasoning"].into_iter().filter(|stream| matches!(self.text_open.get(stream), Some(None))).collect();
        self.close_text(&ended)?;
        let id = tool.wire["toolCallId"].as_str().unwrap_or_default().to_owned();
        let merged = ToolState::merge(self.tools.get(&id), tool);
        let next = self.flavor.normalize_tool(merged);
        self.tools.insert(id.clone(), next.clone());
        let hydrated = self.tool_hooks(&next, projected)?;
        if !hydrated {
            if let Some(subagent) = self.subagent_update(&next.wire) {
                self.emit_subagent(&subagent)?;
                if subagent.suppress_normal_tool {
                    self.rearm();
                    return Ok(());
                }
            }
        }
        self.project_tool_item(&id, &next, projected)?;
        self.rearm();
        Ok(())
    }
    /// `finishRegisteredBackgroundTool`: a structured end of a task is authoritative for the tool that started it.
    /// While the prompt is open the agent reports the end itself.
    fn finish_registered_tool(&mut self, task: &str, status: &'static str, output: Option<&str>) -> Result<(), StoreError> {
        if self.active_run.is_none() || self.bg.turn.settled.is_none() {
            return Ok(());
        }
        let Some(tool) = self.bg.turn.tool_of_task.get(task).and_then(|call| self.tools.get(call)).filter(|tool| bg::tool_open(tool)).cloned() else { return Ok(()) };
        self.bg.turn.awaiting.shift_remove(task);
        let mut finished = tool;
        finished.wire["status"] = json!(status);
        if let Some(output) = output {
            finished.set_output_text(output);
        }
        self.emit_tool_state(finished, Some(status))?;
        self.rearm();
        Ok(())
    }

    /// `terminalizeOpenForegroundTools`: a tool the completed turn left open that is no task and no subagent ended with it.
    pub(super) fn terminalize_foreground_tools(&mut self) -> Result<(), StoreError> {
        let mut open: Vec<ToolState> = self.tools.values().filter(|tool| bg::tool_open(tool) && xai::background_task_id(&tool.wire).is_none() && self.subagent_update(&tool.wire).is_none()).cloned().collect();
        open.sort_by(|left, right| left.wire["toolCallId"].as_str().cmp(&right.wire["toolCallId"].as_str()));
        for tool in open {
            self.emit_tool_state(tool, Some("completed"))?;
        }
        Ok(())
    }

    // --- extension events -----------------------------------------------------------------------------
    /// `x.ai/task_backgrounded` and `x.ai/task_completed`.
    pub(super) fn task_lifecycle(&mut self, lifecycle: xai::Lifecycle) -> Result<(), StoreError> {
        // A cancelled subagent's re-run in its child session must not gate the root's wake machinery.
        if self.bg.quarantined || lifecycle.session_id != self.session.setup.session_id {
            return Ok(());
        }
        if lifecycle.status != "running" && !self.bg.handled.contains(&lifecycle.task_id) {
            if let Some(report) = &lifecycle.report {
                self.record_wake_report(&lifecycle.task_id, bg::command_report(report, lifecycle.status));
            }
        }
        self.apply_late_mutation(&lifecycle.session_id, &lifecycle.task_id, lifecycle.status);
        if lifecycle.status != "running" {
            self.finish_registered_tool(&lifecycle.task_id, if lifecycle.status == "failed" { "failed" } else { "completed" }, lifecycle.output.as_deref())?;
        }
        Ok(())
    }
    /// `finishSubagentFromNotice`: Grok's own word that a background subagent ended.
    pub(super) fn subagent_finished(&mut self, notice: xai::SubagentFinished) -> Result<(), StoreError> {
        if self.bg.quarantined || notice.session_id != self.session.setup.session_id {
            return Ok(());
        }
        let known = self.children.by_child_session(&notice.child_session_id).map(|child| {
            let task = child.task();
            (task["status"].clone(), task["title"].as_str().or_else(|| task["prompt"].as_str()).map(str::to_owned), child.child_thread(), task.clone())
        });
        let Some((status, label, child_thread, task)) = known else { return Ok(()) };
        let blocks = matches!(status.as_str(), Some("running" | "pending"));
        if blocks {
            let mut report = json!({"kind":"subagent","outcome":notice.status,"childThreadId":child_thread});
            if let Some(label) = label {
                report["label"] = json!(label);
            }
            self.record_wake_report(&notice.child_session_id, report);
        }
        if self.active_run.is_some() && blocks {
            self.emit_subagent(&xai::SubagentUpdate {
                native_task_id: task["nativeTaskRef"]["nativeId"].as_str().unwrap_or(&notice.child_session_id).to_owned(),
                prompt: task["prompt"].as_str().unwrap_or_default().to_owned(),
                title: task["title"].as_str().map(str::to_owned),
                model: task["model"].as_str().map(str::to_owned),
                status: notice.status,
                child_session_id: Some(notice.child_session_id.clone()),
                result: notice.result.clone(),
                suppress_normal_tool: true,
            })?;
            self.rearm();
        }
        Ok(())
    }
    /// `recordWakeReport`: work that ended after the prompt settled names itself in the next offer. An open
    /// prompt reports the work itself, and no continuation follows.
    fn record_wake_report(&mut self, key: &str, report: Value) {
        if self.active_run.is_some() && self.bg.turn.settled.is_none() {
            return;
        }
        self.bg.record_report(key, report);
    }
    /// `applyLateBackgroundMutation`.
    fn apply_late_mutation(&mut self, session_id: &str, task: &str, status: &str) {
        let already_ended = self.bg.was_ended(task);
        self.bg.apply_running(task, status);
        if status == "running" {
            // A straggler event after the real end starts an internal prompt whose chatter must not wake anything.
            if already_ended {
                self.bg.suppress_monitor_prompt = true;
            }
            return;
        }
        if !already_ended {
            self.bg.suppress_monitor_prompt = false;
        }
        // Ends that land while a turn is streaming belong to it; the turn re-checks once it settles.
        if self.active_run.is_some() {
            if self.bg.turn.settled.is_none() && !self.bg.handled.contains(task) && self.bg.turn.tool_of_task.contains_key(task) {
                self.bg.unreported.insert(task.to_owned());
            }
            return;
        }
        if session_id == self.session.setup.session_id && self.bg.running.is_empty() && (!self.bg.wake.is_empty() || !self.bg.unreported.is_empty()) {
            self.offer_continuation();
        }
    }
    /// `offerContinuationRun`: a run that delivers what finished while no turn ran. One is outstanding at a time.
    fn offer_continuation(&mut self) -> bool {
        if self.bg.quarantined || self.bg.requested {
            return false;
        }
        self.bg.requested = true;
        let reports = self.bg.take_offer_reports();
        let notification = crate::notification::background_work_notification(&reports);
        if let Err(cause) = crate::provider_continuation::offer(&self.store, &self.providers, &self.thread_id, None, notification) {
            tracing::warn!(%cause, thread = %self.thread_id, "Could not offer a Grok continuation run");
            self.bg.requested = false;
            return false;
        }
        true
    }

    // --- frames with no turn ----------------------------------------------------------------------------
    /// `bufferPostSettleWake`: what Grok says after the turn settled is kept for a continuation run, which opens
    /// once something completion-like arrives and no task still runs.
    fn buffer_post_settle_wake(&mut self, value: &Value) -> Result<Wake, StoreError> {
        let none = Wake { offer: false, stop: false };
        if self.bg.quarantined {
            return Ok(Wake { offer: false, stop: true });
        }
        if value["sessionId"] != self.session.setup.session_id.as_str() {
            return Ok(none);
        }
        let update = &value["update"];
        if !bg::wake_evidence(update) {
            return Ok(none);
        }
        let mut already_handled = false;
        let tools = bg::normalized_tools(update);
        if !tools.is_empty() {
            for tool in &tools {
                if let Some(task) = xai::background_task_id(&tool.wire) {
                    self.bg.set_running(&task, bg::tool_open(tool));
                }
            }
            for tool in &tools {
                if let crate::acp_flavor::Flavor::Grok(_) = &self.flavor {
                    if let Some(markdown) = self.flavor.proposed_plan(tool, &self.session_environment()).filter(|markdown| !markdown.is_empty()) {
                        self.capture_proposed_plan(&markdown)?;
                    }
                }
                if xai::background_task_id(&tool.wire).is_some_and(|task| self.bg.handled.contains(&task)) {
                    already_handled = true;
                }
                let completions: Vec<xai::Mutation> = xai::background_task_completion(&tool.wire).into_iter().chain(xai::killed_background_tasks(&tool.wire)).collect();
                for completion in completions {
                    already_handled |= self.bg.handled.contains(&completion.task_id);
                    self.bg.apply_running(&completion.task_id, completion.status);
                }
            }
        }
        let running = !self.bg.running.is_empty();
        let chatter = !self.bg.handled.is_empty() && chunk(update);
        let mut buffered = false;
        if !already_handled && !chatter && bg::should_buffer(update, running) {
            self.bg.wake.push(value.clone());
            buffered = true;
        }
        let _ = buffered;
        if !bg::offer_evidence(update) || running {
            return Ok(Wake { offer: false, stop: true });
        }
        if already_handled {
            // Leftover noise of work the turn reported must not pin the thread; a continuation already outstanding drains its own.
            if !self.bg.requested {
                self.bg.wake.clear();
            }
            return Ok(Wake { offer: false, stop: true });
        }
        Ok(Wake { offer: !chatter, stop: true })
    }
    /// A root frame while no turn runs: wake traffic is buffered, and late task ends still count.
    pub(super) fn post_settle(&mut self, value: &Value) -> Result<(), StoreError> {
        if self.bg.quarantined {
            return Ok(());
        }
        let wake = self.buffer_post_settle_wake(value)?;
        if wake.offer {
            self.offer_continuation();
        }
        if wake.stop {
            return Ok(());
        }
        let update = &value["update"];
        if matches!(update["sessionUpdate"].as_str(), Some("user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk")) && update["content"]["type"] == "text" {
            for mutation in xai::background_tool_mutations(update["content"]["text"].as_str().unwrap_or("")) {
                let session = value["sessionId"].as_str().unwrap_or_default().to_owned();
                self.apply_late_mutation(&session, &mutation.task_id, mutation.status);
            }
        }
        Ok(())
    }
    fn session_environment(&self) -> std::collections::HashMap<String, String> {
        match &self.flavor {
            crate::acp_flavor::Flavor::Grok(grok) => grok.instance.process_environment(),
            crate::acp_flavor::Flavor::Registry => Default::default(),
        }
    }
    /// A frame of the agent's own turn about finished work, arriving while the settled root turn is still held for
    /// it: it is the agent's reply, not more of the root. Whether the frame was taken.
    pub(super) fn wake_while_held(&mut self, value: &Value) -> Result<bool, StoreError> {
        if !self.grok() || self.bg.turn.settled.is_none() || self.bg.turn.continuation || !xai::is_task_completed_wake(value) {
            return Ok(false);
        }
        if self.has_deferred_work() {
            self.bg.wake.push(value.clone());
            return Ok(true);
        }
        // Settle the root first so the frame reaches the wake path and opens a continuation run.
        self.finalize_held()?;
        let wake = self.buffer_post_settle_wake(value)?;
        if wake.offer {
            self.offer_continuation();
        }
        Ok(true)
    }

    // --- active-turn frames --------------------------------------------------------------------------------
    /// An injected assistant report is streaming: the monitor it reports is no longer waited for.
    pub(super) fn assistant_chunk_seen(&mut self, starts_segment: bool) {
        if !self.grok() {
            return;
        }
        if !self.bg.turn.pending_report.is_empty() {
            let reported: Vec<String> = self.bg.turn.pending_report.keys().cloned().collect();
            self.bg.turn.pending_report.clear();
            for task in reported {
                self.bg.unreported.remove(&task);
            }
        } else if starts_segment && self.bg.turn.settled.is_some() && !self.bg.unreported.is_empty() {
            // Some agents deliver the report before the end notice; the notice that follows consumes it.
            self.bg.turn.early_report = true;
        }
    }
    /// A synthetic user chunk of an active turn: monitor events and end notices, or a subagent's end reminder.
    pub(super) fn user_chunk(&mut self, update: &Value) -> Result<(), StoreError> {
        if !self.grok() || update["content"]["type"] != "text" {
            return Ok(());
        }
        let text = update["content"]["text"].as_str().unwrap_or("");
        let mutations = xai::background_tool_mutations(text);
        let terminal: Vec<String> = mutations.iter().filter(|mutation| mutation.status != "running").map(|mutation| mutation.task_id.clone()).collect();
        let report_arrived_first = self.bg.turn.early_report && !terminal.is_empty();
        if report_arrived_first {
            self.bg.turn.early_report = false;
            for task in &terminal {
                self.bg.unreported.remove(task);
            }
        }
        for mutation in mutations {
            let previous = self.bg.turn.tool_of_task.get(&mutation.task_id).and_then(|call| self.tools.get(call)).cloned();
            if let Some(mut previous) = previous {
                let mut status = match mutation.status {
                    "running" => "inProgress",
                    "failed" => "failed",
                    _ => "completed",
                };
                // End notices omit the full output: the tool keeps running until the agent reads it.
                if status != "inProgress" {
                    self.hold_for_output(&mutation.task_id);
                    status = "inProgress";
                }
                previous.wire["status"] = json!(status);
                let shown = previous.output_text_or_empty();
                previous.set_output_text(&format!("{shown}{}", mutation.append_output));
                self.emit_tool_state(previous, None)?;
            }
            // After the tool: the hold keeps the row running past an end notice, but the wake gate sees the real status.
            self.bg.apply_running(&mutation.task_id, mutation.status);
            if mutation.status != "running" && !report_arrived_first {
                self.hold_for_report(&mutation.task_id);
            }
        }
        if let Some((child_session, status)) = xai::subagent_end_notice(text) {
            let known = self.children.by_child_session(&child_session).map(|child| child.task().clone());
            if let Some(task) = known.filter(|task| matches!(task["status"].as_str(), Some("running" | "pending"))) {
                // The agent may answer with text only and never poll; the row must not hold the turn open forever.
                self.emit_subagent(&xai::SubagentUpdate {
                    native_task_id: task["nativeTaskRef"]["nativeId"].as_str().unwrap_or(&child_session).to_owned(),
                    prompt: task["prompt"].as_str().unwrap_or_default().to_owned(),
                    title: task["title"].as_str().map(str::to_owned),
                    model: task["model"].as_str().map(str::to_owned),
                    status,
                    child_session_id: Some(child_session.clone()),
                    result: None,
                    suppress_normal_tool: true,
                })?;
            }
        }
        Ok(())
    }
    /// `markAwaitingBackgroundHydration`.
    fn hold_for_output(&mut self, task: &str) {
        if !self.bg.turn.awaiting.contains_key(task) {
            let until = self.now_ms() + HYDRATION_HOLD_MS;
            self.bg.turn.awaiting.insert(task.to_owned(), until);
        }
    }
    /// `markPendingInjectedReport`: only a settled turn has the race.
    fn hold_for_report(&mut self, task: &str) {
        if self.bg.turn.settled.is_some() && !self.bg.turn.pending_report.contains_key(task) {
            let until = self.now_ms() + INJECTED_REPORT_HOLD_MS;
            self.bg.turn.pending_report.insert(task.to_owned(), until);
        }
    }

    // --- continuation runs -----------------------------------------------------------------------------------
    /// Whether the run about to start is one the provider asked for to report finished work.
    pub(super) fn is_continuation_run(&self, run_id: &str) -> bool {
        projection(&self.store, &self.thread_id).ok().is_some_and(|view| {
            find(&view, "runs", &json!(run_id)).ok().and_then(|run| find(&view, "messages", &run["userMessageId"]).ok()).is_some_and(crate::provider_continuation::is_continuation)
        })
    }
    /// A continuation run waits while a test holds the gate shut, taking in what Grok says meanwhile.
    pub(super) async fn wait_attach_gate(&mut self) -> Result<(), StoreError> {
        let gate = self.providers.attach_gate().clone();
        while !gate.is_open() {
            let event = tokio::select! {
                biased;
                event = self.session.events.recv() => Some(event),
                _ = gate.opened() => None,
            };
            match event {
                Some(Some(event)) => self.absorb_between_turns(event).await?,
                Some(None) => return Err(error("ACP event stream ended.")),
                None => {}
            }
        }
        Ok(())
    }
    /// The run the provider asked for starts: it attaches to the traffic Grok produced while no turn ran instead of
    /// prompting, and settles once that traffic has been quiet for the window.
    pub(super) fn attach_continuation(&mut self, was_requested: bool) -> Result<(), StoreError> {
        self.bg.requested = false;
        self.bg.offer_delivered();
        let session = self.session.setup.session_id.clone();
        let drained: Vec<Value> = std::mem::take(&mut self.bg.wake).into_iter().filter(|notification| notification["sessionId"] == session.as_str()).collect();
        let count = drained.len();
        for notification in drained {
            self.update_value(notification)?;
        }
        // Attached traffic counts as a settled prompt: the quiet window completes the run once it has drained.
        self.bg.turn.settled = Some("completed");
        if self.active_run.is_none() {
            return Ok(());
        }
        if count == 0 && !was_requested {
            // Nothing was offered for it: it exists only to show what already ended.
            return self.finalize_held();
        }
        self.arm_finalize();
        Ok(())
    }

    // --- stopping ---------------------------------------------------------------------------------------------
    /// A user Stop discards the run and what it left; the process group goes with the actor.
    pub(super) fn stop_background(&mut self) {
        self.bg.quarantine();
        self.hard_stopped = true;
    }
    /// After a turn settles: work that ended unreported while it streamed opens one continuation, and a stopped or
    /// failed turn leaves no marks behind.
    pub(super) fn after_finalize(&mut self, completed: bool) {
        if !self.grok() {
            return;
        }
        if completed && !self.bg.quarantined && self.bg.running.is_empty() && (!self.bg.wake.is_empty() || !self.bg.unreported.is_empty()) {
            self.offer_continuation();
        }
        if !completed || self.bg.running.is_empty() {
            self.bg.unreported.clear();
        }
        self.bg.turn = Default::default();
    }
}
