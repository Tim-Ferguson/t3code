//! T3 Code's own MCP tools as the timeline words them: which tool a call is whatever the provider
//! called it, the present and past tense of its row, and how a group of its calls reads. Port of
//! `t3McpToolPresentation.ts`, `t3ToolSummary.ts` and the pull request number of
//! `parseChangeRequestUrl`. The inventory (`t3_mcp_tools.json`) is written from the original's
//! table by `rust/tools/generate_client_t3_tools_fixtures.mjs`.
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::sync::{LazyLock, OnceLock};

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub id: String,
    pub display_name: String,
    /// Action, running, completed and the object the tool acts on.
    pub labels: [String; 4],
    pub icon: String,
    pub summary_action: String,
}
static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| serde_json::from_str(include_str!("t3_mcp_tools.json")).expect("t3 tool inventory"));

fn lookup(id: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.id == id)
}
fn pattern(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("t3 tool pattern"))
}

/// `resolveT3McpToolName`: the bare tool name behind any provider's spelling of the server prefix.
fn resolve_name(value: &str) -> Option<String> {
    static TAIL: OnceLock<Regex> = OnceLock::new();
    static MCP: OnceLock<Regex> = OnceLock::new();
    static NAMESPACE: OnceLock<Regex> = OnceLock::new();
    static PREFIXED: OnceLock<Regex> = OnceLock::new();
    static OPENCODE: OnceLock<Regex> = OnceLock::new();
    let label = pattern(&TAIL, r"(?i)\s+(?:complete|completed)\s*$").replace(value, "").trim().to_owned();
    if let Some(found) = pattern(&MCP, r"(?is)^mcp__(.+?)__(.+)$").captures(&label) {
        let server = found[1].to_lowercase();
        return matches!(server.as_str(), "t3-code" | "t3_code" | "t3code").then(|| found[2].to_owned());
    }
    if let Some(found) = pattern(&NAMESPACE, r"(?is)^(?:t3-code|t3_code|t3code)(?:[.:/]|\s*·\s*)(.+)$").captures(&label) {
        return Some(found[1].to_owned());
    }
    let candidate = pattern(&PREFIXED, r"(?is)^(?:mcp[-_]{1,2})?t3[-_ ]?code(?:__|[-_.:/ ])(.+)$").captures(&label).map_or(label.clone(), |found| found[1].to_owned());
    if lookup(&candidate).is_some() {
        return Some(candidate);
    }
    // OpenCode 2 registers one server per thread, `t3-code-<thread>`, joined to the tool with `_`: the longest known tool that ends the label.
    if !pattern(&OPENCODE, r"(?i)^t3-code-").is_match(&label) {
        return None;
    }
    TOOLS.iter().filter(|tool| label.ends_with(&format!("_{}", tool.id))).max_by_key(|tool| tool.id.len()).map(|tool| tool.id.clone())
}
/// `resolveT3McpToolDefinition`.
pub fn resolve(tool_name: Option<&str>) -> Option<&'static Tool> {
    lookup(&resolve_name(tool_name?)?)
}

/// The group action a pull request tool has of its own (`ToolGroupAction`).
pub fn pull_request_action(tool: &Tool) -> Option<&'static str> {
    ["link-pr", "unlink-pr", "list-prs", "watch-pr", "unwatch-pr"].into_iter().find(|action| *action == tool.summary_action)
}

/// The number in a link to a change request on a host the original reads (`parseChangeRequestUrl`).
pub fn change_request_number(target: &str) -> Option<u64> {
    static GITHUB: OnceLock<Regex> = OnceLock::new();
    static FORGEJO: OnceLock<Regex> = OnceLock::new();
    static GITLAB: OnceLock<Regex> = OnceLock::new();
    static BITBUCKET: OnceLock<Regex> = OnceLock::new();
    static AZURE: OnceLock<Regex> = OnceLock::new();
    let url = url::Url::parse(target).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    let path = url.path();
    let host_of = |apex: &str, label: Option<&str>| host == apex || host.ends_with(&format!(".{apex}")) || label.is_some_and(|label| host.split('.').any(|part| part == label));
    let claim = |found: Option<regex::Captures>| -> Option<u64> {
        let found = found?;
        let number: u64 = found[2].parse().ok().filter(|number| *number > 0 && *number <= 9_007_199_254_740_991)?;
        (!found[1].is_empty()).then_some(number)
    };
    if host_of("github.com", Some("github")) {
        if let Some(number) = claim(pattern(&GITHUB, r"^/([^/]+/[^/]+)/pull/(\d+)(?:/|$)").captures(path)) {
            return Some(number);
        }
    }
    if let Some(found) = pattern(&FORGEJO, r"^/([^/]+(?:/[^/]+)+)/pulls/(\d+)(?:/|$)").captures(path) {
        return claim(Some(found));
    }
    if let Some(found) = pattern(&GITLAB, r"^/([^/]+(?:/[^/]+)+)/-/merge_requests/(\d+)(?:/|$)").captures(path) {
        return claim(Some(found));
    }
    if host_of("bitbucket.org", Some("bitbucket")) {
        return claim(pattern(&BITBUCKET, r"^/([^/]+/[^/]+)/pull-requests/(\d+)(?:/|$)").captures(path));
    }
    if host_of("dev.azure.com", None) || host.ends_with(".visualstudio.com") {
        return claim(pattern(&AZURE, r"^/((?:[^/]+/)*_git/[^/]+)/pullrequest/(\d+)(?:/|$)").captures(path));
    }
    None
}

/// `resolveT3McpToolPresentation` of `presentation.ts`: the row's words for one call.
#[derive(Debug, Clone, PartialEq)]
pub struct Words {
    pub display_name: String,
    pub icon: &'static str,
    /// The pull request action, for the group summary and icon.
    pub action: Option<&'static str>,
}
/// `status` is the call's lifecycle: `inProgress`, `completed`, `failed`, `declined`, `stopped`, or none.
pub fn words(tool: &Tool, status: Option<&str>, data: &Value) -> Words {
    let [action, running, completed, detail] = &tool.labels;
    let verb = match status {
        Some("inProgress") => running.clone(),
        Some("completed") => completed.clone(),
        Some("failed") => format!("Failed to {}", action.to_lowercase()),
        Some("declined") => format!("Declined to {}", action.to_lowercase()),
        Some("stopped") => format!("Stopped {}", running.to_lowercase()),
        _ => running.clone(),
    };
    let action_kind = pull_request_action(tool);
    let input = ["arguments", "input", "rawInput"].iter().map(|key| &data[*key]).find(|value| value.is_object());
    let url_number = input.and_then(|input| input["url"].as_str()).and_then(change_request_number);
    let number = url_number.or_else(|| input.and_then(|input| input["number"].as_u64().or_else(|| input["number"].as_f64().filter(|n| n.fract() == 0.0 && *n > 0.0).map(|n| n as u64))));
    let target = match (action_kind, number) {
        (Some(kind), Some(number)) if kind != "list-prs" && number > 0 => format!("PR #{number}"),
        _ => detail.clone(),
    };
    Words {
        display_name: format!("{verb} {target}"),
        icon: match tool.icon.as_str() {
            "browser" => "browser",
            "device" => "device",
            "pull-request" => "pull-request",
            _ => "t3-code",
        },
        action: action_kind,
    }
}

// ---- results and summaries ----------------------------------------------------------------------

struct Parsed {
    data: Option<Map<String, Value>>,
    failed: bool,
}
fn record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}
/// `readResult`: the structured or JSON envelopes the provider adapters keep.
fn read_result(value: &Value, depth: usize) -> Parsed {
    static ERROR_TAG: OnceLock<Regex> = OnceLock::new();
    if depth > 4 {
        return Parsed { data: None, failed: false };
    }
    match value {
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(inner) => read_result(&inner, depth + 1),
            Err(_) => Parsed { data: None, failed: false },
        },
        Value::Array(blocks) => {
            let (mut data, mut failed) = (None, false);
            for block in blocks {
                let block = record(block);
                let text = block.and_then(|block| present(block.get("text")).or_else(|| block.get("content").and_then(record).and_then(|content| content.get("text"))));
                let inner = text.and_then(|text| record(text).and_then(|text| present(text.get("text")))).or(text);
                let result = read_result(inner.unwrap_or(&Value::Null), depth + 1);
                if data.is_none() {
                    data = result.data;
                }
                failed = failed || result.failed;
            }
            Parsed { data, failed }
        }
        Value::Object(object) => {
            let failed = object.get("isError") == Some(&Value::Bool(true))
                || object.get("is_error") == Some(&Value::Bool(true))
                || object.get("_tag").and_then(Value::as_str).is_some_and(|tag| pattern(&ERROR_TAG, r"(?:Error|Failure)$").is_match(tag))
                || present(object.get("error")).is_some();
            let content = present(object.get("structuredContent")).or_else(|| object.get("content"));
            match content {
                Some(content) => {
                    let result = read_result(content, depth + 1);
                    Parsed { data: result.data, failed: failed || result.failed }
                }
                None => Parsed { data: Some(object.clone()), failed },
            }
        }
        _ => Parsed { data: None, failed: false },
    }
}
fn read_input(value: &Value) -> Option<Map<String, Value>> {
    let input = read_result(value, 0).data?;
    if input.get("toolName").is_some_and(Value::is_string) { input.get("args").and_then(record).cloned() } else { Some(input) }
}
/// `compactDynamicToolOutput(output).taskId`: the id a delegate call returned, if it is a sane one.
pub fn output_task_id(output: &Value) -> Option<String> {
    let id = read_result(output, 0).data?.get("taskId")?.as_str()?.to_owned();
    (id.encode_utf16().count() <= 256 && !id.trim().is_empty()).then_some(id)
}
/// `compactDynamicToolOutput(output).htmlRender` before it is read: the page reference a successful `html_render` call returned
/// (`htmlRenderFromToolItem` gives none for a result that says it failed).
pub fn output_html_render(output: &Value) -> Option<Value> {
    let result = read_result(output, 0);
    if result.failed {
        return None;
    }
    result.data?.get("htmlRender").cloned()
}
/// `t3ToolResultIndicatesFailure`: MCP errors can come back as data from a call the provider completed.
pub fn result_indicates_failure(output: &Value) -> bool {
    read_result(output, 0).failed
}

/// One call as a group summary reads it.
pub struct Call<'a> {
    pub input: &'a Value,
    pub output: &'a Value,
    /// `completed`, `failed` or `unfinished`.
    pub outcome: &'a str,
}
struct Resolved {
    input: Option<Map<String, Value>>,
    output: Option<Map<String, Value>>,
    outcome: &'static str,
}
fn id(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).filter(|text| !text.trim().is_empty()).map(str::to_owned)
}
fn count_entities(ids: &[Option<String>]) -> usize {
    ids.iter().flatten().collect::<BTreeSet<_>>().len() + ids.iter().filter(|id| id.is_none()).count()
}
fn quantity(count: usize, noun: &str, plural: Option<&str>) -> String {
    format!("{count} {}", if count == 1 { noun.to_owned() } else { plural.map_or_else(|| format!("{noun}s"), str::to_owned) })
}
fn q(count: usize, noun: &str) -> String {
    quantity(count, noun, None)
}

/// `summarizeT3ToolCalls`: what a group of one tool's calls did, counting effects that took place
/// apart from calls that failed or never finished. The label and how many calls failed.
pub fn summarize_calls(action: &str, calls: &[Call]) -> (String, usize) {
    let results: Vec<Resolved> = calls
        .iter()
        .map(|call| {
            let result = read_result(call.output, 0);
            Resolved { input: read_input(call.input), output: result.data, outcome: if result.failed { "failed" } else { match call.outcome { "completed" => "completed", "failed" => "failed", _ => "unfinished" } } }
        })
        .collect();
    let completed = results.iter().filter(|call| call.outcome == "completed").count();
    let failed_count = results.iter().filter(|call| call.outcome == "failed").count();
    let selected: Vec<&Resolved> = if completed > 0 { results.iter().filter(|call| call.outcome == "completed").collect() } else { results.iter().collect() };
    let times = q(selected.len(), "time");
    let phrase = |past: &str, infinitive: &str, object: &str| format!("{} {object}", if completed > 0 { past.to_owned() } else { format!("Tried to {infinitive}") });
    let field = |call: &Resolved, side: bool, key: &str| -> Option<Value> { (if side { call.output.as_ref() } else { call.input.as_ref() }).and_then(|map| map.get(key)).cloned() };
    let entity_ids = |key: &str| -> Vec<Option<String>> { selected.iter().map(|call| id(field(call, true, key).as_ref()).or_else(|| id(field(call, false, key).as_ref()))).collect() };
    let project_ids: Vec<Option<String>> = selected.iter().map(|call| id(field(call, true, "id").as_ref()).or_else(|| id(field(call, true, "projectId").as_ref())).or_else(|| id(field(call, false, "projectId").as_ref()))).collect();
    let thread_ids: Vec<Option<String>> = selected
        .iter()
        .map(|call| id(field(call, true, "threadId").as_ref()).or_else(|| id(field(call, true, "thread").and_then(|thread| thread.get("threadId").cloned()).as_ref())).or_else(|| id(field(call, false, "threadId").as_ref())))
        .collect();
    let entities = |key: &str, noun: &str| q(count_entities(&entity_ids(key)), noun);
    let label = match action {
        "thread-send" => {
            let messages = count_entities(&selected.iter().map(|call| id(field(call, true, "messageId").as_ref())).collect::<Vec<_>>());
            let known = thread_ids.iter().all(Option::is_some);
            let threads = thread_ids.iter().collect::<BTreeSet<_>>().len();
            let object = if known {
                if messages == threads && messages > 1 { format!("messages to {}", q(threads, "thread")) } else { format!("{} to {}", q(messages, "message"), q(threads, "thread")) }
            } else {
                q(messages, "message")
            };
            phrase("Sent", "send", &object)
        }
        "thread-create" => {
            let mut created: Vec<String> = vec![];
            let known = completed > 0
                && selected.iter().filter(|call| call.outcome == "completed").all(|call| {
                    let output = call.output.as_ref().map(|map| Value::Object(map.clone()));
                    let threads: Vec<Value> = match output.as_ref().and_then(|output| output.get("threads")) {
                        Some(Value::Array(list)) => list.clone(),
                        _ => vec![output.clone().unwrap_or(Value::Null)],
                    };
                    threads.iter().all(|thread| {
                        if thread.get("status").and_then(Value::as_str) == Some("rolled_back") {
                            return true;
                        }
                        match id(thread.get("threadId")) {
                            Some(thread_id) => {
                                created.push(thread_id);
                                true
                            }
                            None => false,
                        }
                    })
                });
            if known { format!("Created {}", q(created.iter().collect::<BTreeSet<_>>().len(), "thread")) } else { format!("Requested thread creation {times}") }
        }
        "delegate" => phrase("Delegated", "delegate", &entities("taskId", "task")),
        "thread-read" | "thread-wait" => {
            let targets = if thread_ids.iter().all(Option::is_some) { q(thread_ids.iter().collect::<BTreeSet<_>>().len(), "thread") } else { format!("threads {times}") };
            if action == "thread-read" { phrase("Read", "read", &targets) } else { phrase("Waited on", "wait on", &targets) }
        }
        "thread-list" => phrase("Listed", "list", &format!("threads {times}")),
        "thread-interrupt" => phrase("Requested interrupts for", "interrupt", &q(count_entities(&thread_ids), "thread")),
        "task-status" => phrase("Checked", "check", &format!("task status {times}")),
        "task-cancel" => phrase("Requested cancellation of", "cancel", &entities("taskId", "task")),
        "schedule-create" => phrase("Scheduled", "schedule", &entities("scheduledTaskId", "task")),
        "schedule-list" => phrase("Listed", "list", &format!("scheduled tasks {times}")),
        "schedule-update" => phrase("Updated", "update", &entities("scheduledTaskId", "scheduled task")),
        "schedule-delete" => phrase("Requested deletion of", "delete", &entities("scheduledTaskId", "scheduled task")),
        "schedule-run" => phrase("Requested", "request", &q(selected.len(), "scheduled task run")),
        "thread-configuration" => phrase("Checked", "check", &format!("thread configuration {times}")),
        "thread-configure" => phrase("Set", "set", &format!("thread model {times}")),
        "thread-fork" => phrase("Requested", "request", &q(selected.len(), "thread fork")),
        "thread-merge" => phrase("Requested", "request", &q(selected.len(), "context merge")),
        "thread-search" => phrase("Searched", "search", &format!("threads {times}")),
        "thread-transfers" => phrase("Checked", "check", &format!("thread transfers {times}")),
        "thread-organize" => phrase("Organized", "organize", &format!("threads {times}")),
        "thread-update" => phrase("Updated", "update", &q(count_entities(&thread_ids), "thread")),
        "queue-list" => phrase("Listed", "list", &format!("queued messages {times}")),
        "queue-read" => phrase("Read", "read", &entities("queuedRunId", "queued message")),
        "queue-edit" => phrase("Edited", "edit", &entities("queuedRunId", "queued message")),
        "queue-cancel" => phrase("Requested cancellation of", "cancel", &entities("queuedRunId", "queued run")),
        "queue-reorder" => phrase("Reordered", "reorder", &entities("queuedRunId", "queued run")),
        "queue-steer" => phrase("Requested steering with", "steer with", &entities("queuedRunId", "queued message")),
        "question-list" => phrase("Listed", "list", &format!("pending questions {times}")),
        "question-read" => phrase("Read", "read", &entities("requestId", "pending question request")),
        "question-respond" => phrase("Answered", "answer", &entities("requestId", "pending question request")),
        "secret-request" => phrase("Asked for", "ask for", &q(selected.len(), "secret")),
        "worktree-handoff" => phrase("Handed off to", "hand off to", &entities("worktreePath", "worktree")),
        "worktree-list" => phrase("Listed", "list", &format!("workspace branches {times}")),
        "worktree-status" => phrase("Checked", "check", &format!("worktree status {times}")),
        "project-list" => phrase("Listed", "list", &format!("projects {times}")),
        "project-read" => phrase("Read", "read", &q(count_entities(&project_ids), "project")),
        "project-create" => phrase("Registered", "register", &q(count_entities(&project_ids), "project")),
        "project-update" => phrase("Updated", "update", &q(count_entities(&project_ids), "project")),
        "project-delete" => phrase("Deleted", "delete", &q(count_entities(&project_ids), "project")),
        "project-clone" => phrase("Cloned", "clone", &quantity(count_entities(&entity_ids("cwd")), "repository", Some("repositories"))),
        "environment-read" => phrase("Checked", "check", &format!("environment preferences {times}")),
        "environment-update" => phrase("Updated", "update", &format!("environment preferences {times}")),
        "attachment-prepare" => phrase("Prepared", "prepare", &entities("attachmentId", "attachment upload")),
        "attachment-discard" => phrase("Discarded", "discard", &entities("attachmentId", "pending attachment")),
        "attachment-send" => {
            // A message can hold several attachments, and a retry counts by message identity.
            let mut messages: Vec<(String, &Resolved)> = vec![];
            for (index, call) in selected.iter().enumerate() {
                let key = id(field(call, true, "messageId").as_ref()).unwrap_or_else(|| format!("call-{index}"));
                match messages.iter().position(|(known, _)| *known == key) {
                    Some(position) => messages[position].1 = call,
                    None => messages.push((key, call)),
                }
            }
            let attachments = |call: &Resolved| call.input.as_ref().and_then(|input| input.get("attachments")).and_then(Value::as_array).map(Vec::len);
            let total: usize = messages.iter().map(|(_, call)| attachments(call).unwrap_or(0)).sum();
            let known = messages.iter().all(|(_, call)| attachments(call).is_some_and(|len| len > 0));
            let targets = if thread_ids.iter().all(Option::is_some) { format!(" to {}", q(thread_ids.iter().collect::<BTreeSet<_>>().len(), "thread")) } else { String::new() };
            phrase("Sent", "send", &if known { format!("{}{targets}", q(total, "attachment")) } else { format!("attachments{targets} {times}") })
        }
        "link-pr" => phrase("Linked", "link", &q(selected.len(), "pull request")),
        "unlink-pr" => phrase("Unlinked", "unlink", &q(selected.len(), "pull request")),
        "watch-pr" => phrase("Watching", "watch", &q(selected.len(), "pull request")),
        "unwatch-pr" => phrase("Stopped watching", "stop watching", &q(selected.len(), "pull request")),
        "list-prs" => phrase("Checked", "check", &format!("linked pull requests{}", if selected.len() == 1 { String::new() } else { format!(" {times}") })),
        "browser" => phrase("Used", "use", &format!("browser {times}")),
        "device" => phrase("Used", "use", &format!("device controls {times}")),
        "html-preview" => phrase("Previewed", "preview", &q(selected.len(), "HTML page")),
        "html-render" => phrase("Rendered", "render", &q(selected.len(), "HTML page")),
        _ => phrase("Checked", "check", &format!("orchestration capabilities {times}")),
    };
    (label, failed_count)
}

/// `summaryActionPriority` for the T3 actions: tools that change something lead the summary.
pub fn action_priority(action: &str) -> u8 {
    match action {
        "delegate" | "task-cancel" | "thread-create" | "thread-send" | "thread-interrupt" | "schedule-create" | "schedule-update" | "schedule-delete" | "schedule-run" | "thread-configure" | "thread-fork" | "thread-merge" | "thread-organize" | "thread-update" | "queue-edit" | "queue-cancel" | "queue-reorder" | "queue-steer" | "question-respond" | "worktree-handoff" | "project-create" | "project-update" | "project-delete" | "project-clone" | "environment-update" | "attachment-prepare" | "attachment-discard" | "attachment-send" => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Names, failures, summaries and links agree with the unchanged original on generated inputs.
    #[test]
    fn names_results_summaries_and_links_match_the_original() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/t3-tools.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let input = &row["input"];
            match row["fn"].as_str().unwrap() {
                "resolve" => {
                    let name = input.as_str().unwrap();
                    let tool = resolve(Some(name));
                    let got = json!({"id":tool.map(|tool| tool.id.clone()),"displayName":tool.map(|tool| tool.display_name.clone()),"summaryAction":tool.map(|tool| tool.summary_action.clone())});
                    assert_eq!(got, row["output"], "{name:?}");
                }
                "summarize" => {
                    let calls: Vec<Call> = input["calls"].as_array().unwrap().iter().map(|call| Call { input: &call["input"], output: &call["output"], outcome: call["outcome"].as_str().unwrap() }).collect();
                    let (label, failed) = summarize_calls(input["action"].as_str().unwrap(), &calls);
                    assert_eq!(json!({"label":label,"failedCount":failed}), row["output"], "{input}");
                }
                "failure" => assert_eq!(json!(result_indicates_failure(input)), row["output"], "{input}"),
                "changeRequestNumber" => assert_eq!(json!(change_request_number(input.as_str().unwrap())), row["output"], "{input}"),
                other => panic!("{other}"),
            }
            checked += 1;
        }
        assert!(checked > 4000, "{checked}");
    }

    #[test]
    fn a_pull_request_tool_names_the_pull_request_it_acted_on() {
        let link = resolve(Some("mcp__t3-code__link_pull_request")).unwrap();
        let data = json!({"input":{"url":"https://github.com/o/r/pull/12"}});
        assert_eq!(words(link, Some("completed"), &data).display_name, "Linked PR #12");
        assert_eq!(words(link, Some("failed"), &data).display_name, "Failed to link PR #12");
        assert_eq!(words(link, Some("inProgress"), &json!({})).display_name, "Linking a pull request");
        let preview = resolve(Some("preview_click")).unwrap();
        assert_eq!(words(preview, Some("completed"), &json!({})).icon, "browser");
    }
}
