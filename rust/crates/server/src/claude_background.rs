//! The Claude adapter's view of work that outlives a turn: the roster of background tasks the CLI
//! reports (`pendingBackgroundTasks` on the provider thread), what makes a finished task wake the
//! root, the frames the CLI produces while no turn runs, and the reports a wake names. Mirrors the
//! roster, wake-buffer and wake-report state of `ClaudeAdapterV2.ts`'s session.
//!
//! Only `local_bash` tasks are opaque to the app (a background command, or a monitor when a Monitor
//! call started it); subagents are projected as subagent rows instead (`claude_subagents`).
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const KNOWN_OPAQUE_CAP: usize = 64;

/// A task type the app does not project further.
pub(crate) fn is_opaque_task(message: &Value) -> bool {
    message["task_type"] == "local_bash"
}
pub(crate) fn is_roster_snapshot(message: &Value) -> bool {
    message["type"] == "system" && message["subtype"] == "background_tasks_changed"
}
/// Claude opens every turn it runs with a root `init` frame. Outside a prompt's turn that is a wake.
pub(crate) fn is_turn_start(message: &Value) -> bool {
    message["type"] == "system" && message["subtype"] == "init"
}
fn outcome(status: &Value) -> &'static str {
    match status.as_str() {
        Some("completed") => "completed",
        Some("stopped") => "cancelled",
        _ => "failed",
    }
}

#[derive(Default)]
pub(crate) struct Background {
    /// The provider thread's roster, in the order tasks were listed.
    roster: IndexMap<String, Value>,
    /// Tasks whose end may wake the root. Consumed when their notification is buffered.
    wake_eligible: HashSet<String>,
    /// Tasks whose buffered notification a continuation will classify as opaque.
    tombstones: HashSet<String>,
    /// The last roster entry of each opaque task: an empty roster can land before the task's end.
    last_known: IndexMap<String, Value>,
    /// Monitor calls waiting for their task, and the monitor tasks running (by task id, the call).
    monitor_calls: HashSet<String>,
    monitor_tasks: HashMap<String, String>,
    /// Frames the CLI produced while no turn ran, and the wake offered for them.
    pub(crate) buffer: Vec<Value>,
    pub(crate) detail: Option<String>,
    pub(crate) requested: bool,
    /// Work that ended and no wake has named yet, with the user turn it ended in.
    reports: IndexMap<String, (Value, u64)>,
    user_turns: u64,
    /// Subagents Claude started in the background: only their ends wake the root.
    pub(crate) backgrounded: HashSet<String>,
}

impl Background {
    pub(crate) fn roster(&self) -> Vec<Value> {
        self.roster.values().cloned().collect()
    }
    pub(crate) fn has_task(&self, task_id: &str) -> bool {
        self.roster.contains_key(task_id)
    }
    pub(crate) fn is_wake_eligible(&self, task_id: &str) -> bool {
        self.wake_eligible.contains(task_id)
    }
    /// An opaque task by its roster, its wake eligibility or the tombstone its idle notification left.
    pub(crate) fn is_known_opaque(&self, task_id: &str) -> bool {
        self.roster.contains_key(task_id) || self.wake_eligible.contains(task_id) || self.tombstones.contains(task_id)
    }
    pub(crate) fn consume_eligibility(&mut self, task_id: &str) {
        self.wake_eligible.remove(task_id);
        self.tombstones.insert(task_id.to_owned());
    }
    pub(crate) fn clear_tombstone(&mut self, task_id: &str) {
        self.tombstones.remove(task_id);
    }
    pub(crate) fn has_tombstone(&self, task_id: &str) -> bool {
        self.tombstones.contains(task_id)
    }

    // --- monitors -----------------------------------------------------------------------------
    /// `trackClaudeMonitorCalls`: a `Monitor` call waits until its task starts or its result ends it.
    pub(crate) fn track_monitor_calls(&mut self, message: &Value) {
        if !matches!(message["type"].as_str(), Some("assistant" | "user")) {
            return;
        }
        let blocks = message["message"]["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let started: Vec<&str> = blocks.iter().filter(|block| block["name"] == "Monitor").filter_map(|block| block["id"].as_str()).collect();
        let returned: Vec<&str> = blocks.iter().filter(|block| block["tool_use_id"].is_string()).filter_map(|block| block["tool_use_id"].as_str()).collect();
        let linked: HashSet<&str> = self.monitor_tasks.values().map(String::as_str).collect();
        let opened: Vec<String> = started.into_iter().filter(|id| !linked.contains(id) && !self.monitor_calls.contains(*id)).map(str::to_owned).collect();
        let closed: Vec<&str> = returned.into_iter().filter(|id| self.monitor_calls.contains(*id)).collect();
        self.monitor_calls.extend(opened);
        for id in closed {
            self.monitor_calls.remove(id);
        }
    }
    /// True when a Monitor call started this task; links the task to that call.
    fn is_monitor_task(&mut self, task_id: &str, tool_use_id: Option<&str>) -> bool {
        if self.monitor_tasks.contains_key(task_id) {
            return true;
        }
        let Some(tool_use_id) = tool_use_id.filter(|id| self.monitor_calls.contains(*id)) else { return false };
        self.monitor_calls.remove(tool_use_id);
        self.monitor_tasks.insert(task_id.to_owned(), tool_use_id.to_owned());
        true
    }

    // --- the roster ---------------------------------------------------------------------------
    fn pending_task(task_id: &str, task_type: Option<&str>, by_monitor: bool, description: Option<&str>) -> Value {
        let kind = if by_monitor {
            "monitor"
        } else {
            match task_type {
                Some("local_bash") => "command",
                _ => "background_task",
            }
        };
        let mut task = json!({"taskId":task_id,"kind":kind});
        if let Some(description) = description.filter(|text| !text.trim().is_empty()) {
            task["description"] = json!(description);
        }
        task
    }
    fn remember(&mut self, task: &Value) {
        if let Some(id) = task["taskId"].as_str() {
            self.last_known.insert(id.to_owned(), task.clone());
            while self.last_known.len() > KNOWN_OPAQUE_CAP {
                self.last_known.shift_remove_index(0);
            }
        }
    }
    /// Applies a roster frame; whether the roster changed. Mirrors `applyBackgroundTaskRosterMessage`.
    pub(crate) fn apply(&mut self, message: &Value) -> bool {
        if is_roster_snapshot(message) {
            let Some(listed) = message["tasks"].as_array() else { return false };
            let listed_ids: HashSet<&str> = listed.iter().filter_map(|entry| entry["task_id"].as_str()).collect();
            self.monitor_tasks.retain(|task_id, _| listed_ids.contains(task_id.as_str()));
            let mut next = IndexMap::new();
            for entry in listed {
                let Some(task_id) = entry["task_id"].as_str().filter(|id| !id.is_empty()) else { continue };
                // Only opaque types are supported; subagent entries stay on the subagent path.
                if entry["task_type"] != "local_bash" {
                    continue;
                }
                let task = Self::pending_task(task_id, entry["task_type"].as_str(), self.monitor_tasks.contains_key(task_id), entry["description"].as_str());
                next.insert(task_id.to_owned(), task);
            }
            for task in next.values().cloned().collect::<Vec<_>>() {
                self.remember(&task);
            }
            // An empty level must not drop wake eligibility: the notification may be in flight.
            let admitted: Vec<String> = next.keys().cloned().collect();
            self.roster = next;
            self.wake_eligible.extend(admitted);
            return true;
        }
        if message["type"] != "system" {
            return false;
        }
        match message["subtype"].as_str() {
            Some("task_started") => {
                // A foreground task blocks its tool call, so it is not background work; one moved to
                // the background later arrives in a roster snapshot.
                if !is_opaque_task(message) || message["is_backgrounded"] == false {
                    return false;
                }
                let task_id = message["task_id"].as_str().unwrap_or_default();
                let by_monitor = self.is_monitor_task(task_id, message["tool_use_id"].as_str());
                let task = Self::pending_task(task_id, message["task_type"].as_str(), by_monitor, message["description"].as_str());
                self.remember(&task);
                self.roster.insert(task_id.to_owned(), task);
                self.wake_eligible.insert(task_id.to_owned());
                true
            }
            Some("task_notification") => {
                let task_id = message["task_id"].as_str().unwrap_or_default();
                self.monitor_tasks.remove(task_id);
                let removed = self.roster.shift_remove(task_id).is_some();
                self.wake_eligible.remove(task_id);
                removed
            }
            _ => false,
        }
    }
    /// The process is gone or the turn failed: the roster, its monitors and its eligibility reset.
    pub(crate) fn reset_process(&mut self) {
        self.roster.clear();
        self.monitor_tasks.clear();
        self.wake_eligible.clear();
        self.tombstones.clear();
    }
    /// A process that ended also loses the idle traffic it left and the wake asked for it.
    pub(crate) fn clear_wake(&mut self) {
        self.reports.clear();
        self.buffer.clear();
        self.detail = None;
        self.requested = false;
    }

    // --- wake reports -------------------------------------------------------------------------
    /// What a finished opaque task reports. Reads the roster, so call it before the notification clears it.
    pub(crate) fn opaque_report(&mut self, message: &Value) -> Value {
        let task_id = message["task_id"].as_str().unwrap_or_default();
        let task = self.roster.get(task_id).or_else(|| self.last_known.get(task_id)).cloned();
        self.last_known.shift_remove(task_id);
        let mut report = json!({"kind":task.as_ref().and_then(|task| task["kind"].as_str()).unwrap_or("command"),"outcome":outcome(&message["status"])});
        if let Some(label) = task.as_ref().and_then(|task| task["description"].as_str()) {
            report["label"] = json!(label);
        }
        report
    }
    pub(crate) fn record_report(&mut self, task_id: &str, report: Value) {
        self.reports.insert(task_id.to_owned(), (report, self.user_turns));
    }
    pub(crate) fn subagent_report(label: Option<&str>, message: &Value, child_thread_id: Option<&str>) -> Value {
        let mut report = json!({"kind":"subagent","outcome":outcome(&message["status"])});
        if let Some(label) = label {
            report["label"] = json!(label);
        }
        if let Some(child) = child_thread_id {
            report["childThreadId"] = json!(child);
        }
        report
    }
    /// A user turn starts: a report survives one user turn and then expires, as a notification Claude
    /// folded into its own turn cannot name a later wake.
    pub(crate) fn start_user_turn(&mut self) {
        self.user_turns += 1;
        let turn = self.user_turns;
        self.reports.retain(|_, (_, at)| *at + 1 >= turn);
    }
    pub(crate) fn take_reports(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.reports).into_values().map(|(report, _)| report).collect()
    }
}
