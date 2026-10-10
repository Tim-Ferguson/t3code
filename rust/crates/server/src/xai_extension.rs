//! Grok's xAI extensions to ACP as pure functions over JSON: `XAiAcpExtension.ts`. Tool calls are the
//! adapter's parsed tool state (`acp_tools::ToolState::wire`), notifications and requests are the wire payloads.
// The background-task, wake and plan-file decoders are verified against the original ahead of the adapter
// code that will call them (deferred finalize and wake continuation are not in the Grok layer yet).
#![allow(dead_code)]
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

pub const RATE_LIMITED_CODE: i64 = -32003;
pub const TASK_COMPLETED_PROMPT_ID_PREFIX: &str = "task-completed-";
/// The notification methods that carry turn completion and `subagent_finished` alike.
pub const SESSION_NOTIFICATION_METHODS: [&str; 3] = ["x.ai/session_notification", "_x.ai/session_notification", "_x.ai/session/update"];
pub const PROMPT_COMPLETE_METHODS: [&str; 2] = ["x.ai/session/prompt_complete", "_x.ai/session/prompt_complete"];
pub const TASK_LIFECYCLE_METHODS: [(&str, &str); 4] = [("x.ai/task_backgrounded", "running"), ("_x.ai/task_backgrounded", "running"), ("x.ai/task_completed", "completed"), ("_x.ai/task_completed", "completed")];
pub const ASK_USER_QUESTION_METHODS: [&str; 2] = ["x.ai/ask_user_question", "_x.ai/ask_user_question"];
pub const EXIT_PLAN_MODE_METHODS: [&str; 2] = ["x.ai/exit_plan_mode", "_x.ai/exit_plan_mode"];
pub const EMPTY_PLAN_MARKDOWN: &str = "# No plan written yet\n\n(The agent exited plan mode without writing a plan.)";
const UUID: &str = "[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}";

fn rx(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}
static UUID_ONLY: LazyLock<Regex> = LazyLock::new(|| rx(&format!("(?i)^{UUID}$")));
static AGENT_ID: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r"(?i)(?:^|\n)\s*Agent ID:\s*({UUID})\b")));
static SUBAGENT_ID: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r"(?i)(?:^|\n)\s*subagent_id:\s*({UUID})\b")));
static TASK_HEADER: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r"(?i)===\s*Task\s+({UUID})\s*===")));
static SUBAGENT_ID_ANY: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r"(?i)subagent_id:\s*{UUID}")));
static MONITOR_ACK: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)monitor started\s*\(\s*task\s+"));
static MONITOR_ACK_ID: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r"(?i)monitor started\s*\(\s*task\s+({UUID})\b")));
static AGENT_ID_LINES: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r"(?i)(?:^|\n)\s*Agent ID:\s*{UUID}[^\n]*(?:\n|$)")));
static SUBAGENT_ID_LINES: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r"(?i)(?:^|\n)\s*subagent_id:\s*{UUID}[^\n]*(?:\n|$)")));
static MONITOR_EVENT: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r#"(?i)<monitor-event\s+task_id=["']?({UUID})["']?\s*>\s*([\s\S]*?)\s*</monitor-event>"#)));
static MONITOR_BATCH: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r#"(?i)<monitor\s[^>]*task_id=["']?({UUID})["']?[^>]*>\s*([\s\S]*?)\s*</monitor>"#)));
static MONITOR_ENDED: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r#"(?i)Monitor\s+["']?({UUID})["']?\s+ended"#)));
static SUBAGENT_END: LazyLock<Regex> = LazyLock::new(|| rx(&format!(r#"(?i)Background subagent\s+["']?({UUID})["']?"#)));
static EXITED_ZERO: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)exited\s*\(\s*code\s*0\s*\)"));
static ENDED_CLEANLY: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)ended cleanly"));
static FAILED_WORDS: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)exited|failed|error|signal"));
static COMPLETED_SUCCESSFULLY: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)completed successfully"));
static ERROR_WORDS: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)\b(failed|errored|error|crashed|cancelled|canceled|interrupted|timed out)\b"));
static DONE_WORDS: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)\b(completed|finished|ended)\b"));
static STATUS_FAILED: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)Status:\s*failed"));
static EXIT_CODE: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)Exit Code:\s*(-?\d+)"));
static STATUS_COMPLETED: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)Status:\s*(completed|success)"));
static STATUS_RUNNING: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)Status:\s*(running|pending)"));
static OUTPUT_MARKER: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)=== Output ===\s*([\s\S]*)$"));
static SUBAGENT_META: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)<subagent_meta>[\s\S]*?</subagent_meta>"));
static SUBAGENT_RESULT: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)<subagent_result>[\s\S]*?</subagent_result>"));
static SYSTEM_REMINDER: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)</?system-reminder>"));
static MONITOR_PREFIX: LazyLock<Regex> = LazyLock::new(|| rx(r"^\[monitor\]\s*"));

/// JavaScript `String.prototype.trim` whitespace.
fn trimmed(value: &str) -> &str {
    t3_contracts::trim_wire_string(value)
}
fn non_empty(value: &Value) -> Option<String> {
    value.as_str().map(trimmed).filter(|text| !text.is_empty()).map(str::to_owned)
}
fn non_empty_str(value: Option<&str>) -> Option<String> {
    value.map(trimmed).filter(|text| !text.is_empty()).map(str::to_owned)
}
fn record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}
fn uuid_text(value: &Value) -> Option<String> {
    non_empty(value).filter(|text| UUID_ONLY.is_match(text))
}
fn title_key(tool: &Value) -> String {
    tool["title"].as_str().unwrap_or("").trim().to_lowercase()
}
fn variant(raw_input: &Value) -> Option<String> {
    non_empty(&raw_input["variant"]).map(|text| text.to_lowercase())
}
fn decode_byte_text(value: &Value) -> Option<String> {
    match value {
        Value::String(_) => non_empty(value),
        Value::Array(bytes) if !bytes.is_empty() && bytes.iter().all(Value::is_number) => {
            let bytes: Vec<u8> = bytes.iter().map(|byte| (byte.as_f64().unwrap_or(0.0).trunc() as i64).rem_euclid(256) as u8).collect();
            non_empty_str(Some(&String::from_utf8_lossy(&bytes)))
        }
        _ => None,
    }
}

/// `xAiToolOutputText`: the text a tool call's raw output or content carries.
pub fn tool_output_text(tool: &Value) -> Option<String> {
    let data = &tool["data"];
    let raw_output = &data["rawOutput"];
    if record(raw_output).is_some() {
        let direct = non_empty(&raw_output["text"]).or_else(|| non_empty(&raw_output["output_for_prompt"])).or_else(|| decode_byte_text(&raw_output["output"]));
        if direct.is_some() {
            return direct;
        }
        let result = if record(&raw_output["Result"]).is_some() { &raw_output["Result"] } else { &raw_output["result"] };
        if record(result).is_some() {
            let nested = non_empty(&result["output"]).or_else(|| non_empty(&result["text"])).or_else(|| decode_byte_text(&result["output"]));
            if nested.is_some() {
                return nested;
            }
        }
    }
    if raw_output.is_string() {
        return non_empty(raw_output);
    }
    let content = data["content"].as_array()?;
    let text = content
        .iter()
        .filter_map(|entry| non_empty(&entry["content"]["text"]).or_else(|| non_empty(&entry["text"])))
        .collect::<Vec<_>>()
        .join("\n");
    let text = trimmed(&text);
    (!text.is_empty()).then(|| text.to_owned())
}
fn first_uuid(text: &str, pattern: &Regex) -> Option<String> {
    pattern.captures(text).map(|captures| captures[1].to_owned())
}
fn child_session_id(output: Option<&str>) -> Option<String> {
    let output = output?;
    first_uuid(output, &AGENT_ID).or_else(|| first_uuid(output, &SUBAGENT_ID)).or_else(|| first_uuid(output, &TASK_HEADER))
}
fn is_async_spawn_ack(output: Option<&str>) -> bool {
    let Some(output) = output else { return false };
    output.to_lowercase().contains("subagent started in background") || (SUBAGENT_ID_ANY.is_match(output) && output.to_lowercase().contains("get_command_or_subagent_output"))
}
fn is_monitor_start_ack(output: Option<&str>) -> bool {
    output.is_some_and(|output| MONITOR_ACK.is_match(output))
}
fn is_spawn_or_task_tool(tool: &Value, raw_input: &Value) -> bool {
    let title = title_key(tool);
    if title == "task" || title == "spawn_subagent" || title.contains("spawn subagent") {
        return true;
    }
    if matches!(variant(raw_input).as_deref(), Some("cursortask" | "task" | "spawn_subagent")) {
        return true;
    }
    non_empty(&raw_input["subagent_type"]).is_some() || non_empty(&raw_input["subagentType"]).is_some()
}
fn is_get_output_tool(tool: &Value, raw_input: &Value) -> bool {
    let title = title_key(tool);
    if title == "get_command_or_subagent_output" || title.contains("get_command_or_subagent_output") || title == "wait_commands_or_subagents" {
        return true;
    }
    if matches!(variant(raw_input).as_deref(), Some("taskoutput" | "task_output")) {
        return true;
    }
    if matches!(non_empty(&tool["data"]["rawOutput"]["type"]).map(|kind| kind.to_lowercase()).as_deref(), Some("taskoutput" | "task_output")) {
        return true;
    }
    title.starts_with("[subagent:") && raw_input["task_ids"].is_array()
}
pub fn is_monitor_tool(tool: &Value) -> bool {
    let title = title_key(tool);
    if title == "monitor" || title.starts_with("monitor ") {
        return true;
    }
    if variant(&tool["data"]["rawInput"]).as_deref() == Some("monitor") {
        return true;
    }
    if non_empty(&tool["data"]["rawOutput"]["type"]).map(|kind| kind.to_lowercase()).as_deref() == Some("monitor") {
        return true;
    }
    is_monitor_start_ack(tool_output_text(tool).as_deref())
}
/// A shell command Grok moved to the background acknowledges with `BackgroundTaskStarted`.
fn background_shell_task_id(tool: &Value) -> Option<String> {
    let raw_output = &tool["data"]["rawOutput"];
    (non_empty(&raw_output["type"]).as_deref() == Some("BackgroundTaskStarted")).then(|| non_empty(&raw_output["task_id"])).flatten()
}
/// `extractXAiMonitorTaskId`: the background task a tool call started.
pub fn background_task_id(tool: &Value) -> Option<String> {
    if let Some(id) = background_shell_task_id(tool) {
        return Some(id);
    }
    if !is_monitor_tool(tool) {
        return None;
    }
    let raw_output = &tool["data"]["rawOutput"];
    if record(raw_output).is_some() && non_empty(&raw_output["type"]).map(|kind| kind.to_lowercase()).as_deref() == Some("monitor") {
        if let Some(structured) = non_empty(&raw_output["taskId"]).or_else(|| non_empty(&raw_output["task_id"])) {
            return Some(structured);
        }
    }
    if let Some(output) = tool_output_text(tool) {
        if let Some(from_ack) = first_uuid(&output, &MONITOR_ACK_ID) {
            return Some(from_ack);
        }
    }
    let raw_input = &tool["data"]["rawInput"];
    non_empty(&raw_input["task_id"]).or_else(|| non_empty(&raw_input["taskId"])).or_else(|| raw_input["task_ids"].as_array().and_then(|ids| ids.first()).and_then(non_empty))
}
/// `isXAiPersistentMonitor`: a persistent monitor does not hold the root turn open.
pub fn is_persistent_monitor(tool: &Value) -> bool {
    if !is_monitor_tool(tool) {
        return false;
    }
    let raw_output = &tool["data"]["rawOutput"];
    if record(raw_output).is_some() && non_empty(&raw_output["type"]).map(|kind| kind.to_lowercase()).as_deref() == Some("monitor") {
        return raw_output["persistent"] == true;
    }
    tool["data"]["rawInput"]["persistent"] == true
}
/// A change to a background task: id, `running`/`completed`/`failed`, and output to append.
#[derive(Clone, Debug, PartialEq)]
pub struct Mutation {
    pub task_id: String,
    pub status: &'static str,
    pub append_output: String,
}
fn task_ids_of(raw_input: &Value, uuid_only: bool) -> Vec<String> {
    let mut ids: Vec<String> = vec![];
    let mut push = |value: &Value| {
        if let Some(id) = if uuid_only { uuid_text(value) } else { non_empty(value) } {
            ids.push(id);
        }
    };
    for entry in raw_input["task_ids"].as_array().into_iter().flatten() {
        push(entry);
    }
    push(&raw_input["task_id"]);
    push(&raw_input["taskId"]);
    ids
}
fn unique(ids: Vec<String>) -> Vec<String> {
    let mut seen = vec![];
    for id in ids {
        if !seen.contains(&id) {
            seen.push(id);
        }
    }
    seen
}
fn result_record(tool: &Value) -> &Value {
    let raw_output = &tool["data"]["rawOutput"];
    if record(&raw_output["Result"]).is_some() { &raw_output["Result"] } else { &raw_output["result"] }
}
/// The identity of the task the get-output envelope reports (not the request list).
fn result_task_id(tool: &Value, output: Option<&str>) -> Option<String> {
    let result = result_record(tool);
    let raw_output = &tool["data"]["rawOutput"];
    let structured = non_empty(&result["task_id"]).or_else(|| non_empty(&result["taskId"])).or_else(|| non_empty(&raw_output["task_id"])).or_else(|| non_empty(&raw_output["taskId"]));
    if let Some(structured) = structured.filter(|id| UUID_ONLY.is_match(id)) {
        return Some(structured);
    }
    first_uuid(output?, &TASK_HEADER)
}
fn result_text(tool: &Value, output: Option<&str>) -> Option<String> {
    let result = result_record(tool);
    let structured = non_empty(&result["output"]).or_else(|| non_empty(&result["text"])).or_else(|| decode_byte_text(&result["output"]));
    if let Some(structured) = structured {
        let without_meta = SUBAGENT_META.replace_all(&structured, "");
        let cleaned = SUBAGENT_RESULT.replace_all(&without_meta, "");
        let cleaned = trimmed(&cleaned);
        return (!cleaned.is_empty()).then(|| cleaned.to_owned());
    }
    let output = output?;
    let body = OUTPUT_MARKER.captures(output).map(|captures| captures[1].to_owned()).unwrap_or_else(|| output.to_owned());
    let body = trimmed(&body);
    (!body.is_empty()).then(|| body.to_owned())
}
fn get_output_status(tool: &Value, output: Option<&str>) -> &'static str {
    if tool["status"] == "failed" {
        return "failed";
    }
    let result = result_record(tool);
    match non_empty(&result["status"]).map(|status| status.to_lowercase()).as_deref() {
        Some("completed" | "success") => return "completed",
        Some("failed" | "error") => return "failed",
        Some("running" | "pending") => return "running",
        _ => {}
    }
    if let Some(code) = result["exit_code"].as_f64() {
        return if code == 0.0 { "completed" } else { "failed" };
    }
    if let Some(output) = output {
        // `Exit Code:\s*-?(?!0\b)\d+`: a code that is not a bare zero.
        let nonzero = EXIT_CODE.captures_iter(output).any(|captures| captures[1].trim_start_matches('-') != "0");
        if STATUS_FAILED.is_match(output) || nonzero {
            return "failed";
        }
        if STATUS_COMPLETED.is_match(output) {
            return "completed";
        }
        if STATUS_RUNNING.is_match(output) {
            return "running";
        }
    }
    // A completed poll only means the RPC returned; without a task-level terminal status the task still runs.
    "running"
}
/// `extractXAiBackgroundTaskCompletion`: what a get-output poll says of the tasks it names.
pub fn background_task_completion(tool: &Value) -> Vec<Mutation> {
    let raw_input = &tool["data"]["rawInput"];
    if !is_get_output_tool(tool, raw_input) {
        return vec![];
    }
    let text = tool_output_text(tool);
    let status = get_output_status(tool, text.as_deref());
    let append_output = result_text(tool, text.as_deref()).unwrap_or_default();
    if let Some(task_id) = result_task_id(tool, text.as_deref()) {
        return vec![Mutation { task_id, status, append_output }];
    }
    unique(task_ids_of(raw_input, true)).into_iter().map(|task_id| Mutation { task_id, status, append_output: append_output.clone() }).collect()
}
/// `extractXAiKilledBackgroundTasks`: a finished kill call ends every task it names.
pub fn killed_background_tasks(tool: &Value) -> Vec<Mutation> {
    let raw_input = &tool["data"]["rawInput"];
    if !title_key(tool).contains("kill_command_or_subagent") && variant(raw_input).as_deref() != Some("kill") {
        return vec![];
    }
    if tool["status"] != "completed" {
        return vec![];
    }
    unique(task_ids_of(raw_input, false)).into_iter().map(|task_id| Mutation { task_id, status: "completed", append_output: String::new() }).collect()
}
/// The `x.ai/task_backgrounded` and `x.ai/task_completed` notifications: one change to a background task.
#[derive(Clone, Debug, PartialEq)]
pub struct Lifecycle {
    pub session_id: String,
    pub task_id: String,
    pub status: &'static str,
    pub output: Option<String>,
    /// `(kind, label, exit code)` of a command or monitor, when Grok said what it is.
    pub report: Option<(&'static str, Option<String>, Option<i64>)>,
}
fn task_report(update: &Value) -> Option<(&'static str, Option<String>, Option<i64>)> {
    let snapshot = &update["task_snapshot"];
    let monitor_description = non_empty(&update["monitor_description"]);
    let display_command = non_empty(&snapshot["display_command"]);
    let is_monitor = snapshot["kind"] == "monitor" || monitor_description.is_some() || display_command.as_deref().is_some_and(|command| command.starts_with("[monitor]"));
    let is_command = snapshot["kind"] == "bash" || !snapshot["command"].is_null() && snapshot.get("command").is_some() || !update["command"].is_null() && update.get("command").is_some();
    if !is_monitor && !is_command {
        return None;
    }
    let label = monitor_description
        .or_else(|| non_empty(&snapshot["description"]))
        .or_else(|| non_empty(&update["description"]))
        .or_else(|| display_command.as_deref().map(|command| MONITOR_PREFIX.replace(command, "").into_owned()))
        .or_else(|| non_empty(&snapshot["command"]))
        .or_else(|| non_empty(&update["command"]));
    if is_monitor {
        return Some(("monitor", label, None));
    }
    Some(("command", label, snapshot["exit_code"].as_f64().map(|code| code as i64)))
}
/// `xAiBackgroundTaskLifecycleMutation` over a notification's `{sessionId, update}`.
pub fn lifecycle_mutation(notification: &Value, status: &str) -> Option<Lifecycle> {
    let update = &notification["update"];
    let task_id = non_empty(&update["task_snapshot"]["task_id"]).or_else(|| non_empty(&update["task_id"])).or_else(|| non_empty(&update["tool_call_id"]))?;
    let exit_code = update["task_snapshot"]["exit_code"].as_f64();
    let status = if status == "completed" && exit_code.is_some_and(|code| code != 0.0) { "failed" } else if status == "completed" { "completed" } else { "running" };
    Some(Lifecycle {
        session_id: notification["sessionId"].as_str().unwrap_or("").to_owned(),
        task_id,
        status,
        output: update["task_snapshot"]["output"].as_str().map(str::to_owned),
        report: task_report(update),
    })
}
/// A background subagent's end, sent on its parent session as `subagent_finished`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentFinished {
    pub session_id: String,
    pub child_session_id: String,
    pub status: &'static str,
    pub result: Option<String>,
}
pub fn subagent_finished_notice(notification: &Value) -> Option<SubagentFinished> {
    let update = &notification["update"];
    let child = non_empty(&update["child_session_id"]);
    if update["sessionUpdate"] != "subagent_finished" {
        return None;
    }
    let child = child?;
    let status = match update["status"].as_str()? {
        "completed" => "completed",
        "failed" => "failed",
        "cancelled" => "cancelled",
        _ => return None,
    };
    let text = if status == "completed" { &update["output"] } else { &update["error"] };
    Some(SubagentFinished { session_id: notification["sessionId"].as_str().unwrap_or("").to_owned(), child_session_id: child, status, result: non_empty(text) })
}
/// A root `turn_completed` for a real prompt id as a prompt-complete payload; `None` for hooks and wake turns.
pub fn prompt_complete_from_session_update(notification: &Value) -> Option<Value> {
    let update = &notification["update"];
    if update["sessionUpdate"] != "turn_completed" {
        return None;
    }
    let prompt_id = non_empty(&update["prompt_id"]).or_else(|| non_empty(&update["promptId"]))?;
    if prompt_id.starts_with(TASK_COMPLETED_PROMPT_ID_PREFIX) {
        return None;
    }
    let mut complete = json!({"sessionId":notification["sessionId"],"promptId":prompt_id});
    if let Some(reason) = non_empty(&update["stop_reason"]).or_else(|| non_empty(&update["stopReason"])) {
        complete["stopReason"] = json!(reason);
    }
    if update.get("agent_result").is_some() {
        complete["agentResult"] = update["agent_result"].clone();
    }
    Some(complete)
}
/// Whether a root frame belongs to a turn Grok started itself after background work ended.
pub fn is_task_completed_wake(notification: &Value) -> bool {
    notification["_meta"]["promptId"].as_str().is_some_and(|id| id.starts_with(TASK_COMPLETED_PROMPT_ID_PREFIX))
}
fn is_generic_title(title: Option<&str>) -> bool {
    matches!(title.unwrap_or("").trim().to_lowercase().as_str(), "" | "tool" | "tool call" | "terminal")
}
/// `resolveXAiAcpToolTitle`: Grok often titles a tool "Tool" while its input says what it is.
pub fn resolve_tool_title(tool: &Value) -> Option<String> {
    if !is_generic_title(tool["title"].as_str()) {
        return non_empty(&tool["title"]);
    }
    let raw_input = &tool["data"]["rawInput"];
    let description = non_empty(&raw_input["description"]);
    let variant = variant(raw_input);
    if let Some(description) = description {
        if variant.as_deref() == Some("monitor") && !description.to_lowercase().starts_with("monitor") {
            return Some(format!("Monitor: {description}"));
        }
        return Some(description);
    }
    match variant.as_deref() {
        Some("monitor") => return Some("Monitor".into()),
        Some("task" | "cursortask" | "spawn_subagent") => return Some("Task".into()),
        Some("taskoutput" | "task_output") => return Some("Task output".into()),
        Some(other) => return Some(other.to_owned()),
        None => {}
    }
    let command = non_empty(&raw_input["command"]).or_else(|| non_empty(&tool["data"]["rawOutput"]["command"]));
    if let Some(command) = command {
        return Some(if command.chars().count() > 80 { format!("{}...", command.chars().take(77).collect::<String>()) } else { command });
    }
    non_empty(&tool["title"])
}
/// `normalizeXAiAcpToolCallState`: generic titles replaced, monitor and background start ACKs kept running,
/// and Bash-shaped results with an exit code made terminal.
pub fn normalize_tool_call(tool: &Value) -> Value {
    let mut tool = tool.clone();
    if let Some(title) = resolve_tool_title(&tool) {
        if tool["title"].as_str() != Some(title.as_str()) {
            tool["title"] = json!(title);
        }
    }
    let raw_output = tool["data"]["rawOutput"].clone();
    let output_type = if record(&raw_output).is_some() { non_empty(&raw_output["type"]).map(|kind| kind.to_lowercase()) } else { None };
    let reported_running = matches!(tool["status"].as_str(), Some("inProgress" | "pending"));
    if output_type.as_deref() == Some("bash") && !reported_running {
        let code = ["exit_code", "exitCode", "code"].iter().map(|key| &raw_output[*key]).find(|value| value.as_f64().is_some_and(|number| number.fract() == 0.0));
        if let Some(code) = code.and_then(Value::as_f64) {
            if code == 0.0 && tool_output_text(&tool).map_or(0, |text| trimmed(&text).len()) == 0 {
                tool["status"] = json!("inProgress");
            } else {
                tool["status"] = json!(if code == 0.0 { "completed" } else { "failed" });
            }
            return tool;
        }
    }
    if background_shell_task_id(&tool).is_some() {
        tool["status"] = json!("inProgress");
        return tool;
    }
    if !is_monitor_tool(&tool) {
        return tool;
    }
    if output_type.as_deref() == Some("monitor") {
        tool["status"] = json!("inProgress");
        return tool;
    }
    if tool["status"] == "completed" && is_monitor_start_ack(tool_output_text(&tool).as_deref()) {
        tool["status"] = json!("inProgress");
    }
    tool
}
/// `extractXAiAcpBackgroundToolMutation`: synthetic monitor traffic in root text chunks.
pub fn background_tool_mutations(text: &str) -> Vec<Mutation> {
    let mut mutations = vec![];
    for captures in MONITOR_EVENT.captures_iter(text).chain(MONITOR_BATCH.captures_iter(text)) {
        let line = trimmed(&captures[2]);
        mutations.push(Mutation { task_id: captures[1].to_owned(), status: "running", append_output: if line.is_empty() { String::new() } else { format!("{line}\n") } });
    }
    for ended in MONITOR_ENDED.captures_iter(text) {
        let whole = ended.get(0).unwrap();
        let after = &text[whole.end()..];
        let tail = after.split('\n').next().unwrap_or("");
        let outcome = format!("{}{tail}", trimmed(whole.as_str()));
        let summary = SYSTEM_REMINDER.replace_all(text, "");
        let summary = trimmed(&summary);
        let status = if EXITED_ZERO.is_match(&outcome) || ENDED_CLEANLY.is_match(&outcome) { "completed" } else if FAILED_WORDS.is_match(&outcome) { "failed" } else { "completed" };
        mutations.push(Mutation { task_id: ended[1].to_owned(), status, append_output: if summary.is_empty() { "Monitor ended.\n".into() } else { format!("{summary}\n") } });
    }
    mutations
}
fn strip_leading_paren_group(text: &str) -> &str {
    let trimmed_start = text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if !trimmed_start.starts_with('(') {
        return trimmed_start;
    }
    let mut depth = 0;
    for (index, character) in trimmed_start.char_indices() {
        if character == '(' {
            depth += 1;
        } else if character == ')' {
            depth -= 1;
            if depth == 0 {
                return trimmed_start[index + 1..].trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
            }
        }
    }
    trimmed_start
}
/// `extractXAiAcpSubagentEndNotice`: the root-session reminder that a background subagent ended.
pub fn subagent_end_notice(text: &str) -> Option<(String, &'static str)> {
    let matched = SUBAGENT_END.captures(text)?;
    let whole = matched.get(0).unwrap();
    let tail = strip_leading_paren_group(&text[whole.end()..]);
    let id = matched[1].to_owned();
    if COMPLETED_SUCCESSFULLY.is_match(tail) {
        return Some((id, "completed"));
    }
    if ERROR_WORDS.is_match(tail) {
        return Some((id, "failed"));
    }
    if DONE_WORDS.is_match(tail) {
        return Some((id, "completed"));
    }
    None
}
/// A subagent launch or poll: what `extractXAiAcpSubagentUpdate` found in a tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct SubagentUpdate {
    pub native_task_id: String,
    pub prompt: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: &'static str,
    pub child_session_id: Option<String>,
    pub result: Option<String>,
    pub suppress_normal_tool: bool,
}
pub fn subagent_update(tool: &Value) -> Option<SubagentUpdate> {
    let raw_input = &tool["data"]["rawInput"];
    let output = tool_output_text(tool);
    let tool_call_id = tool["toolCallId"].as_str().unwrap_or("").to_owned();
    if is_get_output_tool(tool, raw_input) {
        let mut ids = task_ids_of(raw_input, true);
        if let Some(result) = result_task_id(tool, output.as_deref()) {
            ids.push(result);
        }
        let ids = unique(ids);
        let child = result_task_id(tool, output.as_deref()).or_else(|| ids.first().cloned()).or_else(|| child_session_id(output.as_deref()))?;
        return Some(SubagentUpdate { native_task_id: tool_call_id, prompt: String::new(), title: None, model: None, status: get_output_status(tool, output.as_deref()), child_session_id: Some(child), result: result_text(tool, output.as_deref()), suppress_normal_tool: true });
    }
    if !is_spawn_or_task_tool(tool, raw_input) {
        return None;
    }
    let child = child_session_id(output.as_deref());
    let async_ack = is_async_spawn_ack(output.as_deref());
    let legacy = output.as_deref().map(|output| {
        let stripped = AGENT_ID_LINES.replace_all(output, "\n");
        let stripped = SUBAGENT_ID_LINES.replace_all(&stripped, "\n");
        trimmed(&stripped).to_owned()
    }).filter(|text| !text.is_empty());
    let status = if tool["status"] == "failed" { "failed" } else if async_ack { "running" } else if tool["status"] == "completed" { "completed" } else { "running" };
    Some(SubagentUpdate {
        native_task_id: tool_call_id,
        prompt: non_empty(&raw_input["prompt"]).unwrap_or_default(),
        title: non_empty(&raw_input["description"]),
        model: non_empty(&raw_input["model"]),
        status,
        child_session_id: child,
        result: if async_ack { None } else { legacy },
        suppress_normal_tool: true,
    })
}
/// The questions of an `ask_user_question` request, unwrapped from the `{method, params}` envelope some builds send.
pub fn unwrap_params(params: &Value) -> &Value {
    if params["params"].is_object() { &params["params"] } else { params }
}
/// `extractXAiAskUserQuestions`: the user-input questions of the request.
pub fn ask_user_questions(params: &Value) -> Vec<Value> {
    unwrap_params(params)["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|question| {
            let options: Vec<Value> = question["options"].as_array().into_iter().flatten().map(|option| json!({"label":option["label"],"description":option.get("description").filter(|d| !d.is_null()).unwrap_or(&option["label"])})).collect();
            json!({"id":question.get("id").filter(|id| !id.is_null()).unwrap_or(&question["question"]),"header":"Question","question":question["question"],"multiSelect":question["multiSelect"] == true,"options":if options.is_empty(){vec![json!({"label":"OK","description":"Continue"})]}else{options}})
        })
        .collect()
}
/// `(sessionId, toolCallId)` of an `ask_user_question` request.
pub fn ask_user_identity(params: &Value) -> (String, String) {
    let params = unwrap_params(params);
    (params["sessionId"].as_str().unwrap_or("").to_owned(), params["toolCallId"].as_str().unwrap_or("").to_owned())
}
fn answer_values(answer: &Value) -> Vec<String> {
    match answer {
        Value::Array(entries) => entries.iter().filter_map(|entry| entry.as_str().map(trimmed).filter(|text| !text.is_empty()).map(str::to_owned)).collect(),
        Value::String(text) => Some(trimmed(text)).filter(|text| !text.is_empty()).map(|text| vec![text.to_owned()]).unwrap_or_default(),
        _ => vec![],
    }
}
/// `makeXAiAskUserQuestionResponse`: the user's answers as the labels Grok offered, the rest as notes.
pub fn ask_user_response(params: &Value, answers: &Value) -> Value {
    let mut chosen = Map::new();
    let mut annotations = Map::new();
    for question in unwrap_params(params)["questions"].as_array().into_iter().flatten() {
        let key = question.get("id").filter(|id| !id.is_null()).and_then(Value::as_str).or(question["question"].as_str()).unwrap_or("");
        let answer = answers.get(key).filter(|answer| !answer.is_null()).or_else(|| answers.get(question["question"].as_str().unwrap_or("")));
        let values = answer.map(answer_values).unwrap_or_default();
        if values.is_empty() {
            continue;
        }
        let options = question["options"].as_array().cloned().unwrap_or_default();
        let find = |value: &str| options.iter().find(|option| option["label"] == value);
        let selected: Vec<String> = values.iter().filter_map(|value| find(value).and_then(|option| option["label"].as_str()).map(str::to_owned)).collect();
        let notes: Vec<&String> = values.iter().filter(|value| find(value).is_none()).collect();
        let preview = if question["multiSelect"] == true { None } else { values.iter().find_map(|value| find(value).and_then(|option| non_empty(&option["preview"]))) };
        let text = question["question"].as_str().unwrap_or("").to_owned();
        chosen.insert(text.clone(), json!(if selected.is_empty() { vec!["Other".to_owned()] } else { selected }));
        if preview.is_some() || !notes.is_empty() {
            let mut annotation = Map::new();
            if let Some(preview) = preview {
                annotation.insert("preview".into(), json!(preview));
            }
            if !notes.is_empty() {
                annotation.insert("notes".into(), json!(notes.iter().map(|note| note.as_str()).collect::<Vec<_>>().join("\n")));
            }
            annotations.insert(text, Value::Object(annotation));
        }
    }
    let mut response = json!({"outcome":"accepted","answers":chosen});
    if !annotations.is_empty() {
        response["annotations"] = Value::Object(annotations);
    }
    response
}
pub fn ask_user_cancelled_response() -> Value {
    json!({"outcome":"cancelled"})
}
/// `extractXAiExitPlanMarkdown`: the request's plan, else the plan file sniffed this turn, else the empty-state text.
pub fn exit_plan_markdown(params: &Value, fallback: Option<&str>) -> String {
    if let Some(content) = unwrap_params(params)["planContent"].as_str().map(trimmed).filter(|content| !content.is_empty()) {
        return content.to_owned();
    }
    fallback.map(trimmed).filter(|fallback| !fallback.is_empty()).map(str::to_owned).unwrap_or_else(|| EMPTY_PLAN_MARKDOWN.to_owned())
}
/// The abandoning answer for the native plan gate after T3 captured the plan.
pub fn exit_plan_captured_response() -> Value {
    json!({"outcome":"abandoned","feedback":"The client captured your proposed plan. Stop here and wait for the user's feedback or implementation request in a later turn."})
}
fn normalize_fs_path(value: &str) -> String {
    let normalized = trimmed(value).replace('\\', "/");
    normalized.trim_end_matches('/').to_owned()
}
fn session_prefixes(environment: &dyn Fn(&str) -> Option<String>, home: Option<&str>) -> Vec<String> {
    let mut prefixes: Vec<String> = vec![];
    let mut add = |root: &str, nested: bool| {
        let root = normalize_fs_path(root);
        if root.is_empty() {
            return;
        }
        let prefix = if nested { format!("{root}/.grok/sessions/") } else { format!("{root}/sessions/") };
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    };
    add(home.unwrap_or(""), true);
    add("~", true);
    add(&environment("HOME").unwrap_or_default(), true);
    add(&environment("USERPROFILE").unwrap_or_default(), true);
    add("/tmp/mock-home", true);
    let grok_home = environment("GROK_HOME").unwrap_or_default();
    add(&grok_home, false);
    add(&grok_home, true);
    prefixes
}
static CANONICAL_PLAN: LazyLock<Regex> = LazyLock::new(|| rx(r"^(?:/home/[^/]+|/Users/[^/]+|[a-zA-Z]:/Users/[^/]+)/\.grok/sessions/(?:[^/]+/)+plan\.md$"));
static CANONICAL_PLAN_INSENSITIVE: LazyLock<Regex> = LazyLock::new(|| rx(r"(?i)^(?:/home/[^/]+|/Users/[^/]+|[a-zA-Z]:/Users/[^/]+)/\.grok/sessions/(?:[^/]+/)+plan\.md$"));
/// `isGrokPlanMarkdownPath`: Grok's session plan file under a Grok home, never a workspace `plan.md`.
pub fn is_plan_markdown_path(path: Option<&str>, win32: bool, environment: &dyn Fn(&str) -> Option<String>, home: Option<&str>) -> bool {
    let Some(path) = path else { return false };
    let normalized = trimmed(path).replace('\\', "/");
    let haystack = if win32 { normalized.to_lowercase() } else { normalized.clone() };
    if normalized.is_empty() || !haystack.ends_with("/plan.md") || normalized.split('/').any(|segment| segment == "..") {
        return false;
    }
    for prefix in session_prefixes(environment, home) {
        let needle = if win32 { prefix.to_lowercase() } else { prefix };
        if !haystack.starts_with(&needle) {
            continue;
        }
        let rest = &haystack[needle.len()..];
        if rest != "plan.md" && rest.ends_with("plan.md") {
            return true;
        }
    }
    (if win32 { &*CANONICAL_PLAN_INSENSITIVE } else { &*CANONICAL_PLAN }).is_match(&haystack)
}
/// `extractGrokPlanMarkdownFromToolCallData`: the plan a write or edit of the session's `plan.md` carries.
/// `Some("")` when the plan file was written without usable text.
pub fn plan_markdown_from_tool_data(data: &Value, win32: bool, environment: &dyn Fn(&str) -> Option<String>, home: Option<&str>) -> Option<String> {
    if !data.is_object() {
        return None;
    }
    let mut saw_plan_write = false;
    let mut take = |value: Option<&str>, file: Option<&str>| -> Option<String> {
        if !is_plan_markdown_path(file, win32, environment, home) {
            return None;
        }
        let value = value?;
        saw_plan_write = true;
        let value = trimmed(value);
        (!value.is_empty()).then(|| value.to_owned())
    };
    let raw_input = &data["rawInput"];
    if raw_input.is_object() {
        let file = raw_input["file_path"].as_str().or_else(|| raw_input["path"].as_str());
        if let Some(text) = take(raw_input["content"].as_str(), file) {
            return Some(text);
        }
    }
    for block in data["content"].as_array().into_iter().flatten() {
        if !block.is_object() || block["type"] != "diff" {
            continue;
        }
        if let Some(text) = take(block["newText"].as_str(), block["path"].as_str()) {
            return Some(text);
        }
    }
    saw_plan_write.then(String::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_tools::ToolState;
    use std::collections::HashMap;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/grok-pure.json")).unwrap()
    }
    fn mutations(found: &[Mutation]) -> Value {
        json!(found.iter().map(|mutation| json!({"taskId":mutation.task_id,"status":mutation.status,"appendOutput":mutation.append_output})).collect::<Vec<_>>())
    }
    #[test]
    fn tool_calls_of_the_recordings_decode_as_the_source_does() {
        let rows = fixture();
        let mut merged: HashMap<(String, String), ToolState> = HashMap::new();
        for row in rows["tools"].as_array().unwrap() {
            let scenario = row["scenario"].as_str().unwrap().to_owned();
            let update = &row["update"];
            let parsed = ToolState::parse(update, update["sessionUpdate"] == "tool_call").unwrap();
            let key = (scenario, parsed.wire["toolCallId"].as_str().unwrap().to_owned());
            let state = ToolState::merge(merged.get(&key), parsed);
            let expected = &row["expected"];
            let context = format!("{} {}", row["scenario"], row["sequence"]);
            let title = resolve_tool_title(&state.wire);
            assert_eq!(json!(title), expected["title"], "{context}: title");
            let tool = normalize_tool_call(&state.wire);
            assert_eq!(json!({"title":tool.get("title").cloned().unwrap_or(Value::Null),"status":tool.get("status").cloned().unwrap_or(Value::Null)}), expected["normalized"], "{context}: normalized");
            assert_eq!(json!(background_task_id(&tool)), expected["taskId"], "{context}: task id");
            assert_eq!(is_persistent_monitor(&tool), expected["persistent"].as_bool().unwrap(), "{context}: persistent");
            assert_eq!(mutations(&background_task_completion(&tool)), expected["completion"], "{context}: completion");
            assert_eq!(mutations(&killed_background_tasks(&tool)), expected["killed"], "{context}: killed");
            let subagent = subagent_update(&tool).map(|update| {
                let mut value = json!({"nativeTaskId":update.native_task_id,"prompt":update.prompt,"title":update.title,"model":update.model,"status":update.status,"childSessionId":update.child_session_id,"result":update.result});
                if update.suppress_normal_tool {
                    value["suppressNormalTool"] = json!(true);
                }
                value
            });
            assert_eq!(json!(subagent), expected["subagent"], "{context}: subagent");
            // The adapter stores the normalized state, which the next update merges onto.
            let mut stored = state;
            stored.wire = tool;
            merged.insert(key, stored);
        }
    }
    #[test]
    fn text_chunks_decode_background_traffic_as_the_source_does() {
        for row in fixture()["texts"].as_array().unwrap() {
            let text = row["text"].as_str().unwrap();
            assert_eq!(mutations(&background_tool_mutations(text)), row["mutations"], "{text}");
            assert_eq!(json!(subagent_end_notice(text).map(|(id, status)| json!({"childSessionId":id,"status":status}))), row["endNotice"], "{text}");
        }
    }
    #[test]
    fn notifications_decode_lifecycle_completion_and_wake_as_the_source_does() {
        for row in fixture()["notifications"].as_array().unwrap() {
            let (method, params) = (row["method"].as_str().unwrap(), &row["params"]);
            if !row["wake"].is_null() {
                assert_eq!(is_task_completed_wake(params), row["wake"].as_bool().unwrap(), "{params}");
                continue;
            }
            let lifecycle = TASK_LIFECYCLE_METHODS.iter().find(|(name, _)| method.ends_with(name.trim_start_matches('_'))).filter(|_| params["update"].is_object()).and_then(|(_, status)| lifecycle_mutation(params, status)).map(|found| {
                let mut value = json!({"sessionId":found.session_id,"taskId":found.task_id,"status":found.status});
                if let Some(output) = found.output {
                    value["output"] = json!(output);
                }
                if let Some((kind, label, exit)) = found.report {
                    let mut report = json!({"kind":kind});
                    if let Some(label) = label {
                        report["label"] = json!(label);
                    }
                    if let Some(exit) = exit {
                        report["exitCode"] = json!(exit);
                    }
                    value["report"] = report;
                }
                value
            });
            if row["lifecycle"].is_object() || TASK_LIFECYCLE_METHODS.iter().any(|(name, _)| method.ends_with(name.trim_start_matches('_'))) {
                assert_eq!(json!(lifecycle), row["lifecycle"], "{method} {params}");
            }
            let finished = if params["update"].is_object() { subagent_finished_notice(params).map(|found| json!({"sessionId":found.session_id,"childSessionId":found.child_session_id,"status":found.status,"result":found.result})) } else { None };
            assert_eq!(json!(finished), row["finished"], "{params}");
            let complete = if params["update"].is_object() { prompt_complete_from_session_update(params) } else { None };
            assert_eq!(json!(complete), row["promptComplete"], "{params}");
        }
    }
    #[test]
    fn questions_the_plan_gate_and_plan_files_answer_as_the_source_does() {
        let rows = fixture();
        let questions = &rows["questions"];
        assert_eq!(json!(ask_user_questions(&questions["params"])), questions["questions"]);
        let (session, tool) = ask_user_identity(&questions["params"]);
        assert_eq!(json!({"sessionId":session,"toolCallId":tool}), questions["identity"]);
        for case in questions["answers"].as_array().unwrap() {
            assert_eq!(ask_user_response(&questions["params"], &case["answers"]), case["response"], "{}", case["answers"]);
        }
        for case in rows["exitPlan"].as_array().unwrap() {
            assert_eq!(exit_plan_markdown(&case["params"], case["fallback"].as_str()), case["markdown"].as_str().unwrap());
        }
        let environment = |name: &str| rows["host"]["environment"][name].as_str().map(str::to_owned);
        let home = environment("HOME");
        for case in rows["planPaths"].as_array().unwrap() {
            let path = case["path"].as_str();
            // The original also treats the process's own home as a Grok home; these cases name their own.
            assert_eq!(is_plan_markdown_path(path, false, &environment, home.as_deref()), case["linux"].as_bool().unwrap(), "{path:?}");
            assert_eq!(is_plan_markdown_path(path, true, &environment, home.as_deref()), case["win32"].as_bool().unwrap(), "{path:?} on win32");
        }
        for case in rows["planData"].as_array().unwrap() {
            assert_eq!(json!(plan_markdown_from_tool_data(&case["data"], false, &environment, home.as_deref())), case["markdown"], "{}", case["data"]);
        }
    }
    #[test]
    fn launch_and_approval_helpers_match_the_source() {
        use crate::grok_acp;
        let launch = &fixture()["launch"];
        for case in launch["spawnArgs"].as_array().unwrap() {
            assert_eq!(json!(grok_acp::spawn_args(case["mode"].as_str())), case["args"]);
        }
        for case in launch["approvals"].as_array().unwrap() {
            assert_eq!(json!(grok_acp::approval_options(&case["request"])), case["options"], "{}", case["request"]);
        }
        for case in launch["models"].as_array().unwrap() {
            assert_eq!(json!(grok_acp::base_model_id(case["model"].as_str())), case["id"]);
        }
        for case in launch["efforts"].as_array().unwrap() {
            assert_eq!(grok_acp::is_valid_reasoning_effort(case["value"].as_str().unwrap()), case["valid"].as_bool().unwrap(), "{}", case["value"]);
        }
    }
}
