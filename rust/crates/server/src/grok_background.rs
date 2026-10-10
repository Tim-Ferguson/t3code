//! Grok's background work on the ACP adapter: the tasks known to still run, the traffic Grok produces while
//! no turn does (its wake turns), the reports a wake names, and the holds that keep a settled turn open.
//! Mirrors the session state of `AcpAdapterV2` that its Grok flavour switches on (`deferFinalizeForBackgroundWork`,
//! `enablePostSettleContinuation`). The actor owns one [`Background`]; `acp_adapter/grok_background.rs` is
//! the code that drives it.
use crate::{acp_tools::ToolState, xai_extension as xai};
use indexmap::{IndexMap, IndexSet};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

/// How long a monitor end notice may wait for the agent to read the task's output before the task is
/// completed anyway (`markAwaitingBackgroundHydration`).
pub(crate) const HYDRATION_HOLD_MS: i64 = 60_000;
/// How long a settled turn waits for the report of a monitor that ended (`markPendingInjectedReport`).
pub(crate) const INJECTED_REPORT_HOLD_MS: i64 = 25_000;
/// `endedBackgroundTaskIdLimit`.
const ENDED_LIMIT: usize = 128;

static MONITOR_EVENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<monitor-event\b").unwrap());
static MONITOR_ENDED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)Monitor\s+["']?[0-9a-f-]{8,}["']?\s+ended"#).unwrap());
static MONITOR_END_NOTICE: LazyLock<(Regex, Regex)> = LazyLock::new(|| (Regex::new(r#"(?i)Monitor\s+["']?[0-9a-f-]{8,}"#).unwrap(), Regex::new(r"(?i)ended").unwrap()));

/// `isMonitorEndNoticeText`: boilerplate a monitor's end notice leaves as the tool's output.
pub(crate) fn is_monitor_end_notice(text: &str) -> bool {
    MONITOR_END_NOTICE.0.is_match(text) && MONITOR_END_NOTICE.1.is_match(text)
}

/// What one turn holds open for background work. Reset when a turn starts.
#[derive(Default)]
pub(crate) struct Turn {
    /// Background task id → the tool call that registered it.
    pub tool_of_task: IndexMap<String, String>,
    /// Persistent monitors stream after the turn settles and never hold it.
    pub persistent: HashSet<String>,
    /// Tasks that ended but whose output the agent has not read yet, with when the hold lapses.
    pub awaiting: IndexMap<String, i64>,
    /// Monitors that ended after the prompt settled, whose report is still on its way, with when the hold lapses.
    pub pending_report: IndexMap<String, i64>,
    /// An injected report that arrived before its monitor's end notice.
    pub early_report: bool,
    /// The prompt returned and the turn is held open: the status it will settle with.
    pub settled: Option<&'static str>,
    /// When the quiet window ends and a held turn settles.
    pub finalize_at: Option<i64>,
    /// A run the provider asked for to report finished background work: it attaches to buffered traffic
    /// instead of prompting.
    pub continuation: bool,
    /// The turn is ending; nothing it shows from here on re-arms a quiet window.
    pub finalizing: bool,
}

#[derive(Default)]
pub(crate) struct Background {
    pub turn: Turn,
    /// Tasks known to still run at session level; a continuation run finalizes between a monitor's events.
    pub running: HashSet<String>,
    /// Tasks with a genuine end signal. A straggler event cannot resurrect them.
    ended: IndexSet<String>,
    /// Root traffic Grok produced while no turn ran, until a continuation run drains it.
    pub wake: Vec<Value>,
    /// Work that ended after the prompt settled, keyed by task or child session id, and the keys the offer named.
    pub reports: IndexMap<String, Value>,
    offered: HashSet<String>,
    /// A continuation run was asked for and has not started.
    pub requested: bool,
    /// A user Stop discarded the run: its residue wakes nothing.
    pub quarantined: bool,
    /// Tasks that ended while a root turn was still streaming and nobody reported.
    pub unreported: HashSet<String>,
    /// Tasks the active turn already reported itself.
    pub handled: HashSet<String>,
    /// A monitor event after the real end starts an internal prompt whose output is suppressed.
    pub suppress_monitor_prompt: bool,
}

impl Background {
    /// `setBackgroundTaskRunning`.
    pub(crate) fn set_running(&mut self, task: &str, running: bool) {
        if running {
            if !self.ended.contains(task) {
                self.running.insert(task.to_owned());
            }
        } else {
            self.running.remove(task);
        }
    }
    pub(crate) fn mark_ended(&mut self, task: &str) {
        self.ended.insert(task.to_owned());
        while self.ended.len() > ENDED_LIMIT {
            self.ended.shift_remove_index(0);
        }
        self.set_running(task, false);
    }
    pub(crate) fn was_ended(&self, task: &str) -> bool {
        self.ended.contains(task)
    }
    /// `applyBackgroundTaskMutationRunning`.
    pub(crate) fn apply_running(&mut self, task: &str, status: &str) {
        if status == "running" {
            self.set_running(task, true);
        } else {
            self.mark_ended(task);
        }
    }
    /// A turn starts: what the previous one held, handled and left unreported does not carry over.
    pub(crate) fn start_turn(&mut self) {
        self.turn = Turn::default();
        self.suppress_monitor_prompt = false;
        self.handled.clear();
        self.quarantined = false;
    }
    /// `recordWakeReport`'s store: the work that ended names itself in the next offer.
    pub(crate) fn record_report(&mut self, key: &str, report: Value) {
        self.reports.insert(key.to_owned(), report);
    }
    /// The reports an offer names; the keys are remembered so the turn that delivers it drops exactly those.
    pub(crate) fn take_offer_reports(&mut self) -> Vec<Value> {
        self.offered = self.reports.keys().cloned().collect();
        self.reports.values().cloned().collect()
    }
    /// A user turn supersedes the work reported so far.
    pub(crate) fn clear_reports(&mut self) {
        self.reports.clear();
        self.offered.clear();
    }
    /// The continuation run started: it delivered what its offer named; later work keeps its report.
    pub(crate) fn offer_delivered(&mut self) {
        let offered = std::mem::take(&mut self.offered);
        self.reports.retain(|key, _| !offered.contains(key));
    }
    /// `quarantineStoppedRun`.
    pub(crate) fn quarantine(&mut self) {
        self.quarantined = true;
        self.wake.clear();
        self.reports.clear();
        self.offered.clear();
        self.requested = false;
        self.running.clear();
        self.unreported.clear();
    }
}

/// `acpRootSessionUpdateIngestsOutput`: a root update that carries turn output, not keepalive noise.
fn ingests_output(update: &Value) -> bool {
    let text = |content: &Value| crate::acp_model::content_display_text(content).is_some_and(|text| !text.is_empty());
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk" | "agent_thought_chunk") => text(&update["content"]),
        Some("agent_message" | "agent_thought") => update["content"].as_array().is_some_and(|contents| contents.iter().any(text)),
        Some(kind @ ("tool_call" | "tool_call_update")) => ToolState::parse(update, kind == "tool_call").is_some(),
        Some("plan" | "plan_update" | "plan_removed") => crate::acp_model::plan_update(update).is_some(),
        Some("compaction_update") => true,
        Some("compaction_summary_chunk") => text(&update["content"]),
        _ => false,
    }
}

/// `acpPostSettleWakeEvidence`: post-settle traffic that should be buffered for a continuation to attach.
pub(crate) fn wake_evidence(update: &Value) -> bool {
    if !ingests_output(update) {
        return false;
    }
    if matches!(update["sessionUpdate"].as_str(), Some("user_message_chunk" | "agent_message_chunk")) && update["content"]["type"] == "text" {
        let text = update["content"]["text"].as_str().unwrap_or("");
        if !xai::background_tool_mutations(text).is_empty() {
            return false;
        }
        if update["sessionUpdate"] == "agent_message_chunk" && text.trim().is_empty() {
            return false;
        }
        if MONITOR_EVENT.is_match(text) || MONITOR_ENDED.is_match(text) {
            return false;
        }
    }
    true
}

/// The tool states a tool update carries, as Grok's flavour normalizes them.
pub(crate) fn normalized_tools(update: &Value) -> Vec<ToolState> {
    match update["sessionUpdate"].as_str() {
        Some(kind @ ("tool_call" | "tool_call_update")) => ToolState::parse(update, kind == "tool_call").map(|tool| {
            let mut tool = tool;
            tool.wire = xai::normalize_tool_call(&tool.wire);
            tool
        }).into_iter().collect(),
        _ => vec![],
    }
}

/// `acpPostSettleContinuationOfferEvidence`: only completion-like frames open a run, not each progress chunk.
pub(crate) fn offer_evidence(update: &Value) -> bool {
    if !wake_evidence(update) {
        return false;
    }
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => crate::acp_model::content_display_text(&update["content"]).is_some_and(|text| !text.trim().is_empty()),
        Some("tool_call" | "tool_call_update") => normalized_tools(update).iter().any(|tool| matches!(tool.wire["status"].as_str(), Some("completed" | "failed"))),
        _ => false,
    }
}

/// `acpPostSettleWakeShouldBuffer`: while a task runs its agent chatter is progress, not a result.
pub(crate) fn should_buffer(update: &Value, background_running: bool) -> bool {
    !background_running || !matches!(update["sessionUpdate"].as_str(), Some("agent_message_chunk" | "agent_thought_chunk"))
}

/// A tool that is pending or running (`toolStatus` is `pending` or `running`).
pub(crate) fn tool_open(tool: &ToolState) -> bool {
    matches!(tool.wire["status"].as_str(), Some("pending" | "inProgress"))
}

/// What a command or monitor that ended reports: `{kind, label?, outcome, exitCode?}`.
pub(crate) fn command_report(report: &(&'static str, Option<String>, Option<i64>), outcome: &str) -> Value {
    let mut value = json!({"kind":report.0,"outcome":outcome});
    if let Some(label) = &report.1 {
        value["label"] = json!(label);
    }
    if let Some(code) = report.2 {
        value["exitCode"] = json!(code);
    }
    value
}

/// `acpTurnStartShouldPreserveContinuation`.
pub(crate) fn preserves_continuation(requested: bool, is_continuation: bool, wake_len: usize) -> bool {
    !is_continuation && requested && wake_len > 0
}

/// A map of tool-call id → open flag for every task the turn registered; used by the hold.
pub(crate) fn blocking_tasks(turn: &Turn, tools: &HashMap<String, ToolState>) -> bool {
    turn.tool_of_task.iter().any(|(task, call)| !turn.persistent.contains(task) && tools.get(call).is_some_and(tool_open))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ended_tasks_do_not_come_back_and_the_roster_is_bounded() {
        let mut background = Background::default();
        background.apply_running("a", "running");
        assert!(background.running.contains("a"));
        background.apply_running("a", "completed");
        background.apply_running("a", "running");
        assert!(background.running.is_empty(), "a straggler event cannot resurrect an ended task");
        for index in 0..200 {
            background.mark_ended(&format!("t{index}"));
        }
        assert_eq!(background.ended.len(), ENDED_LIMIT);
        assert!(!background.was_ended("a"));
    }

    #[test]
    fn only_completion_like_frames_offer_a_run() {
        let message = |text: &str| json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}});
        let thought = json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hm"}});
        assert!(wake_evidence(&message("hello")));
        assert!(offer_evidence(&message("hello")));
        assert!(wake_evidence(&thought) && !offer_evidence(&thought), "thoughts buffer but never open a run");
        assert!(!wake_evidence(&message("  ")));
        assert!(!wake_evidence(&message("<monitor-event task_id=\"x\">tick</monitor-event>")));
        assert!(!wake_evidence(&json!({"sessionUpdate":"available_commands_update","availableCommands":[]})));
        assert!(!should_buffer(&message("progress"), true));
        assert!(should_buffer(&message("progress"), false));
        assert!(should_buffer(&json!({"sessionUpdate":"tool_call_update","toolCallId":"t"}), true));
        let finished = json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed"});
        let running = json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"in_progress"});
        assert!(offer_evidence(&finished) && !offer_evidence(&running));
    }

    #[test]
    fn a_stopped_run_discards_what_it_left() {
        let mut background = Background::default();
        background.wake.push(json!({}));
        background.record_report("a", json!({"kind":"command","outcome":"completed"}));
        background.requested = true;
        background.running.insert("x".into());
        background.quarantine();
        assert!(background.wake.is_empty() && background.reports.is_empty() && !background.requested && background.running.is_empty());
        assert!(background.quarantined);
        background.start_turn();
        assert!(!background.quarantined);
    }

    #[test]
    fn an_offer_delivers_only_the_reports_it_named() {
        let mut background = Background::default();
        background.record_report("a", json!({"kind":"command","outcome":"completed"}));
        assert_eq!(background.take_offer_reports().len(), 1);
        background.record_report("b", json!({"kind":"monitor","outcome":"completed"}));
        background.offer_delivered();
        assert_eq!(background.reports.keys().collect::<Vec<_>>(), ["b"]);
    }
}
