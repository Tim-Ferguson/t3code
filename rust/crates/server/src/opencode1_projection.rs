//! OpenCode 1.x protocol facts that need no I/O: the permission ruleset a runtime mode becomes, the question form and
//! its answers, token usage by message ownership, tool parts as timeline items, and message ids. Text, request and
//! compaction rows are `opencode_projection`'s (shared with 2.x); what is here mirrors `OpenCodeAdapterV2.ts` and
//! `opencodeRuntime.ts`. The tool-name classification is `OpenCodeToolItems.ts`, which both generations share.
use crate::{
    execution::at,
    opencode_projection::{self as shared, Events, Projector, tool_item},
    persistence::StoreError,
};
use chrono::{TimeZone, Utc};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

// ---- permission rules ----

const ALWAYS_ALLOWED: [&str; 8] = ["question", "read", "glob", "grep", "lsp", "todowrite", "task", "skill"];
const RESTRICTED: [&str; 7] = ["bash", "edit", "webfetch", "websearch", "codesearch", "external_directory", "doom_loop"];

fn rule(permission: &str, pattern: &str, action: &str) -> Value {
    json!({"permission":permission,"pattern":pattern,"action":action})
}
/// `openCodePermissionRules`: OpenCode has no sandbox, so the ruleset is the boundary. Reads and planning are safe by
/// default, edits are allowed only by `auto-accept-edits`, and shell, network and outside-directory access ask. Full
/// access allows everything. T3 hands a provider only the runtime mode, so the policy's own approval and sandbox forms
/// (which only a scripted override sets) do not arise.
pub(crate) fn permission_rules(runtime_mode: &str) -> Vec<Value> {
    if runtime_mode == "full-access" {
        return vec![rule("*", "*", "allow")];
    }
    // A task session inherits only its parent's deny rules, so denies come first and the asks and allows override them.
    let mut rules = vec![rule("*", "*", "deny")];
    rules.extend(RESTRICTED.iter().map(|permission| rule(permission, "*", "deny")));
    rules.push(rule("*", "*", "ask"));
    rules.extend(RESTRICTED.iter().map(|permission| rule(permission, "*", "ask")));
    rules.extend(ALWAYS_ALLOWED.iter().map(|permission| rule(permission, "*", "allow")));
    rules.extend([rule("read", "*.env", "ask"), rule("read", "*.env.*", "ask"), rule("read", "*.env.example", "allow")]);
    if runtime_mode == "auto-accept-edits" {
        rules.push(rule("edit", "*", "allow"));
    }
    rules
}
/// The rules that keep T3's MCP servers to their own thread (tool names are `<server>_<tool>`), after the mode's: the
/// last matching rule decides.
pub(crate) fn mcp_rules(server: Option<&str>) -> Vec<Value> {
    match server {
        None => vec![],
        Some(name) => vec![rule("t3-code-*", "*", "deny"), rule(&format!("{name}_*"), "*", "allow")],
    }
}
/// `openCodeChildPermissionRules`: a task session inherits only the parent's deny and outside-directory rules and
/// then adds its agent's restrictions (no nested tasks). The complete parent policy is installed instead, keeping only
/// what the child's agent added.
pub(crate) fn child_permission_rules(parent: &[Value], native_child: &[Value]) -> Vec<Value> {
    let inherited: Vec<&Value> = parent.iter().filter(|rule| rule["permission"] == "external_directory" || rule["action"] == "deny").collect();
    let mut rules = parent.to_vec();
    rules.extend(native_child.iter().filter(|rule| !inherited.iter().any(|inherited| inherited["permission"] == rule["permission"] && inherited["pattern"] == rule["pattern"] && inherited["action"] == rule["action"])).cloned());
    rules
}

/// `toOpenCodePermissionReply`.
pub(crate) fn permission_reply(decision: &str) -> &'static str {
    match decision {
        "accept" => "once",
        "acceptForSession" | "acceptAlways" => "always",
        _ => "reject",
    }
}

// ---- questions ----

/// `openCodeQuestionId`.
pub(crate) fn question_id(index: usize, question: &Value) -> String {
    let header: String = question["header"].as_str().unwrap_or("").trim().to_lowercase();
    let mut slug = String::new();
    let mut separated = false;
    for character in header.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_' || character == '-' {
            slug.push(character);
            separated = false;
        } else if !separated {
            slug.push('-');
            separated = true;
        }
    }
    if slug.is_empty() { format!("question-{index}") } else { format!("question-{index}-{slug}") }
}
fn trimmed(value: &Value) -> String {
    value.as_str().unwrap_or("").trim().to_owned()
}
/// `requestQuestions`: a question request as T3's questions.
pub(crate) fn request_questions(request: &Value) -> Value {
    let questions: Vec<Value> = request["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, question)| {
            let header = Some(trimmed(&question["header"])).filter(|header| !header.is_empty()).unwrap_or_else(|| format!("Question {}", index + 1));
            let text = Some(trimmed(&question["question"])).filter(|text| !text.is_empty()).unwrap_or_else(|| header.clone());
            let options: Vec<Value> = question["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|option| {
                    let label = Some(trimmed(&option["label"])).filter(|label| !label.is_empty()).unwrap_or_else(|| "Option".into());
                    let description = Some(trimmed(&option["description"])).filter(|text| !text.is_empty()).unwrap_or_else(|| label.clone());
                    json!({"label":label,"description":description})
                })
                .collect();
            json!({"id":question_id(index, question),"header":header,"question":text,"options":options})
        })
        .collect();
    json!(questions)
}
/// `toOpenCodeQuestionAnswers`: one list of chosen labels per question.
pub(crate) fn question_answers(request: &Value, answers: &Value) -> Value {
    let lists: Vec<Value> = request["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, question)| {
            let raw = [question_id(index, question), question["header"].as_str().unwrap_or("").to_owned(), question["question"].as_str().unwrap_or("").to_owned()].iter().find_map(|key| answers.get(key).filter(|value| !value.is_null()).cloned());
            match raw {
                Some(Value::Array(values)) => json!(values.into_iter().filter(Value::is_string).collect::<Vec<_>>()),
                Some(Value::String(text)) if !text.trim().is_empty() => json!([text]),
                _ => json!([]),
            }
        })
        .collect();
    json!(lists)
}

// ---- token usage ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Owned,
    Other,
    Unknown,
}
/// A turn's token usage. OpenCode reports a step's tokens on its parts, and a part can arrive before its message says
/// whose prompt it answers, so unresolved steps wait for the message. Native removal does not undo usage.
pub(crate) struct Usage {
    parts: HashSet<String>,
    /// The messages that prompted this turn: its own prompt and each steer.
    pub(crate) prompts: HashSet<String>,
    ownership: HashMap<String, Ownership>,
    unresolved: HashMap<String, IndexMap<String, Value>>,
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
    pub(crate) has_subagents: bool,
    /// False once the event stream reconnected: steps may have been missed.
    pub(crate) complete: bool,
}
impl Usage {
    pub(crate) fn new() -> Self {
        Self { parts: HashSet::new(), prompts: HashSet::new(), ownership: HashMap::new(), unresolved: HashMap::new(), input: 0, cached: 0, cache_write: 0, output: 0, reasoning: 0, has_subagents: false, complete: true }
    }
    fn accumulate(&mut self, part: &str, tokens: &Value) {
        if !self.parts.insert(part.to_owned()) {
            return;
        }
        let count = |value: &Value| value.as_u64().unwrap_or(0);
        let (input, output, reasoning) = (count(&tokens["input"]), count(&tokens["output"]), count(&tokens["reasoning"]));
        let (read, write) = (count(&tokens["cache"]["read"]), count(&tokens["cache"]["write"]));
        self.input += input + read + write;
        self.cached += read;
        self.cache_write += write;
        self.output += output + reasoning;
        self.reasoning += reasoning;
    }
    /// An assistant message: whose prompt it answers settles the steps that arrived before it.
    pub(crate) fn on_assistant(&mut self, message: &str, parent: Option<&str>) {
        let ownership = match self.ownership.get(message) {
            Some(prior) if *prior != Ownership::Unknown => *prior,
            _ => match parent {
                None => Ownership::Unknown,
                Some(parent) if self.prompts.contains(parent) => Ownership::Owned,
                Some(_) => Ownership::Other,
            },
        };
        self.ownership.insert(message.to_owned(), ownership);
        if ownership != Ownership::Unknown {
            if ownership == Ownership::Owned {
                for (part, tokens) in self.unresolved.get(message).cloned().unwrap_or_default() {
                    self.accumulate(&part, &tokens);
                }
            }
            self.unresolved.remove(message);
        }
    }
    /// A `step-finish` part. `role_known` is whether the message's role was seen.
    pub(crate) fn on_step(&mut self, message: &str, part: &str, tokens: &Value, role_known: bool) {
        match self.ownership.get(message).copied() {
            Some(Ownership::Owned) => self.accumulate(part, tokens),
            Some(Ownership::Unknown) => {
                self.unresolved.entry(message.to_owned()).or_default().insert(part.to_owned(), tokens.clone());
            }
            None if !role_known => {
                self.unresolved.entry(message.to_owned()).or_default().insert(part.to_owned(), tokens.clone());
            }
            _ => {}
        }
    }
    /// The turn's usage as its provider turn row carries it once the turn ended `status`.
    pub(crate) fn summary(&self, status: &str) -> Value {
        if self.parts.is_empty() {
            return json!({"usageScope":"main_agent","usageStatus":"unavailable","hasSubagents":self.has_subagents});
        }
        let complete = status == "completed" && self.complete && self.unresolved.is_empty();
        json!({"usageScope":"main_agent","usageStatus":if complete { "complete" } else { "partial" },"inputTokens":self.input,"cachedInputTokens":self.cached,"cacheCreationTokens":self.cache_write,"outputTokens":self.output,"reasoningTokens":self.reasoning,"hasSubagents":self.has_subagents})
    }
}

// ---- ids ----

/// `makeOpenCodeMessageId`: OpenCode orders messages by id, so a prompt's id is `msg_` + a 48-bit time and counter +
/// 28 random characters.
pub(crate) fn new_message_id() -> String {
    static LAST: std::sync::Mutex<(i64, u64)> = std::sync::Mutex::new((-1, 0));
    let now = Utc::now().timestamp_millis();
    let counter = {
        let mut last = LAST.lock().unwrap();
        if last.0 != now {
            *last = (now, 0);
        }
        last.1 += 1;
        last.1
    };
    let encoded = ((now as u128) * 0x1000 + counter as u128) & ((1u128 << 48) - 1);
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let random: String = (0..28).map(|_| ALPHABET[rand::random::<u32>() as usize % ALPHABET.len()] as char).collect();
    format!("msg_{encoded:012x}{random}")
}

// ---- timestamps ----

/// An epoch-millisecond field of OpenCode as a timestamp.
pub(crate) fn instant(millis: &Value) -> Option<String> {
    millis.as_i64().and_then(|millis| Utc.timestamp_millis_opt(millis).single()).map(at)
}

// ---- tools ----

/// What a tool part's state says, as the node and item statuses.
fn tool_status(part: &Value) -> (&'static str, &'static str) {
    match part["state"]["status"].as_str() {
        Some("running") => ("running", "running"),
        Some("completed") => ("completed", "completed"),
        Some("error") => ("failed", "failed"),
        _ => ("pending", "pending"),
    }
}
fn record_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key).and_then(Value::as_str).filter(|text| !text.trim().is_empty()).map(str::to_owned))
}
/// `toolTitle`: the title a running or completed call reports.
pub(crate) fn tool_title(part: &Value) -> Option<String> {
    matches!(part["state"]["status"].as_str(), Some("running" | "completed")).then(|| part["state"]["title"].as_str().map(str::to_owned)).flatten()
}
/// `toolModel`: `provider/model` from a call's metadata.
pub(crate) fn tool_model(part: &Value) -> Option<String> {
    let metadata = tool_metadata(part)?;
    let model = metadata.get("model")?;
    let provider = record_string(model, &["providerID", "providerId"])?;
    let id = record_string(model, &["modelID", "modelId", "id"])?;
    Some(format!("{provider}/{id}"))
}
/// `taskSessionId`: the session a `task` call runs in.
pub(crate) fn task_session(part: &Value) -> Option<String> {
    record_string(tool_metadata(part)?, &["sessionId", "sessionID"])
}
fn tool_metadata(part: &Value) -> Option<&Value> {
    matches!(part["state"]["status"].as_str(), Some("running" | "completed" | "error")).then(|| &part["state"]["metadata"]).filter(|metadata| metadata.is_object())
}
pub(crate) fn tool_input(part: &Value) -> Value {
    if part["state"]["input"].is_object() { part["state"]["input"].clone() } else { json!({}) }
}
pub(crate) fn tool_output(part: &Value) -> Option<String> {
    match part["state"]["status"].as_str() {
        Some("completed") => part["state"]["output"].as_str().map(str::to_owned),
        Some("error") => part["state"]["error"].as_str().map(str::to_owned),
        _ => None,
    }
}
/// The node and item statuses and times of a tool part.
pub(crate) struct ToolTimes {
    pub(crate) node: &'static str,
    pub(crate) item: &'static str,
    pub(crate) started: String,
    pub(crate) completed: Option<String>,
}
pub(crate) fn tool_times(part: &Value) -> ToolTimes {
    let now = at(Utc::now());
    let (node, item) = tool_status(part);
    let started = if node == "pending" { now.clone() } else { instant(&part["state"]["time"]["start"]).unwrap_or_else(|| now.clone()) };
    let completed = matches!(node, "completed" | "failed").then(|| instant(&part["state"]["time"]["end"]).unwrap_or(now));
    ToolTimes { node, item, started, completed }
}

/// The server a tool belongs to when exactly one of the known MCP servers prefixes its name (`<server>_<tool>`, the
/// server's name with anything outside `[a-zA-Z0-9_-]` replaced by `_`).
pub(crate) fn mcp_server_of<'a>(tool: &str, servers: &'a [String]) -> Option<&'a str> {
    if tool == "code_search" || tool == "apply_patch" {
        return None;
    }
    let sanitize = |name: &str| name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    let matching: Vec<&String> = servers.iter().filter(|name| tool.starts_with(&format!("{}_", sanitize(name)))).collect();
    (matching.len() == 1).then(|| matching[0].as_str())
}

/// A non-subagent tool part: its node and timeline item. `servers` are the MCP servers known to OpenCode.
pub(crate) fn tool_events(projector: &mut Projector, view: &Value, part: &Value, servers: Option<&[String]>) -> Result<Events, StoreError> {
    let tool = part["tool"].as_str().unwrap_or("");
    let native = part["id"].as_str().unwrap_or("");
    let times = tool_times(part);
    let provider_turn = projector.provider_turn(view)?;
    let ordinal = projector.ordinal(view, native);
    let now = at(Utc::now());
    let node = projector.node(native, "tool_call", times.node, &times.started, times.completed.as_deref(), &provider_turn);
    let mut base = projector.base_item(&shared::item_id(native), &json!(shared::node_id(native)), ordinal, times.item, native, &times.started, times.completed.as_deref(), &now, &provider_turn);
    base["title"] = json!(tool_title(part));
    let input = tool_input(part);
    let output = tool_output(part);
    let server = servers.and_then(|servers| mcp_server_of(tool, servers));
    let item = match server {
        Some(server) => {
            let sanitized: String = server.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
            let name = tool.get(sanitized.len() + 1..).unwrap_or(tool);
            let title = tool_title(part).filter(|title| title != tool);
            // T3's own per-thread servers present as the one the tool catalogue knows.
            let known = if server.starts_with("t3-code-") { "t3-code" } else { server };
            let mut presentation = crate::claude_projection::mcp_presentation(&format!("mcp__{known}__{name}"));
            if let Some(title) = title.filter(|_| presentation.get("title").is_some() || presentation.get("toolSource").is_some()) {
                presentation["title"] = json!(title);
            }
            let mut item = base;
            item["type"] = json!("dynamic_tool");
            if let Some(fields) = presentation.as_object() {
                for (key, value) in fields {
                    item[key] = value.clone();
                }
            }
            item["toolName"] = json!(tool);
            item["input"] = input;
            if let Some(output) = output {
                item["output"] = json!(output);
            }
            item
        }
        None => tool_item(base, tool, &input, output.as_deref(), (part["state"]["status"] == "completed").then(|| &part["state"]["metadata"]).filter(|metadata| metadata.is_object())),
    };
    Ok(vec![("node.updated", node), ("turn-item.updated", item)])
}

/// `OpenCodeAdapterV2.ts` `openCodeBoundaryAfterProviderTurn`: fork and revert boundaries are exclusive, so keeping a
/// turn means addressing the next native user message; `None` keeps the head.
pub(crate) fn boundary_after_turn(turns: &[Value], selected: &Value) -> Option<String> {
    let selected = turns.iter().find(|turn| turn["id"] == *selected)?;
    let mut later: Vec<&Value> = turns.iter().filter(|turn| turn["ordinal"].as_u64() > selected["ordinal"].as_u64()).collect();
    later.sort_by_key(|turn| turn["ordinal"].as_u64());
    later.iter().find_map(|turn| turn["nativeTurnRef"]["nativeId"].as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_access_allows_everything_and_the_other_modes_ask() {
        assert_eq!(permission_rules("full-access"), vec![rule("*", "*", "allow")]);
        let supervised = permission_rules("approval-required");
        assert!(supervised.contains(&rule("bash", "*", "ask")) && !supervised.contains(&rule("edit", "*", "allow")));
        assert!(supervised.contains(&rule("read", "*", "allow")));
        assert!(permission_rules("auto-accept-edits").contains(&rule("edit", "*", "allow")));
        // Denies come first, so a task session that inherits only them stays safe until its full policy is installed.
        assert_eq!(supervised[0], rule("*", "*", "deny"));
    }
    #[test]
    fn a_child_keeps_the_parent_policy_and_only_what_its_agent_added() {
        let parent = permission_rules("approval-required");
        let native = vec![rule("*", "*", "deny"), rule("task", "*", "deny")];
        let rules = child_permission_rules(&parent, &native);
        assert_eq!(rules.len(), parent.len() + 1);
        assert_eq!(rules.last().unwrap(), &rule("task", "*", "deny"));
    }
    #[test]
    fn question_ids_and_answers_follow_the_header() {
        let request = json!({"questions":[{"header":"Schema vs Flexibility","question":"Strict?","options":[{"label":"Strict schemas","description":""}]},{"header":" ","question":"Other?","options":[]}]});
        assert_eq!(request_questions(&request)[0]["id"], "question-0-schema-vs-flexibility");
        assert_eq!(request_questions(&request)[0]["options"][0]["description"], "Strict schemas");
        assert_eq!(request_questions(&request)[1]["id"], "question-1");
        let answers = question_answers(&request, &json!({"question-0-schema-vs-flexibility":"Strict schemas","Other?":["a",1,"b"]}));
        assert_eq!(answers, json!([["Strict schemas"],["a","b"]]));
    }
    #[test]
    fn steps_count_once_their_message_is_known_to_answer_this_turn() {
        let mut usage = Usage::new();
        usage.prompts.insert("msg_p".into());
        let tokens = json!({"input":10,"output":2,"reasoning":1,"cache":{"read":5,"write":0}});
        // The step arrives before its message: it waits, and the turn is partial meanwhile.
        usage.on_step("msg_a", "prt_1", &tokens, false);
        assert_eq!(usage.summary("completed")["usageStatus"], "unavailable");
        usage.on_assistant("msg_a", Some("msg_p"));
        usage.on_step("msg_a", "prt_1", &tokens, true);
        let summary = usage.summary("completed");
        assert_eq!((summary["inputTokens"].as_u64(), summary["outputTokens"].as_u64(), summary["usageStatus"].as_str()), (Some(15), Some(3), Some("complete")));
        // Another turn's message is not this turn's usage.
        usage.on_assistant("msg_other", Some("msg_elsewhere"));
        usage.on_step("msg_other", "prt_2", &tokens, true);
        assert_eq!(usage.summary("completed")["inputTokens"], 15);
        assert_eq!(usage.summary("interrupted")["usageStatus"], "partial");
    }
    #[test]
    fn message_ids_ascend_and_are_the_shape_opencode_orders_by() {
        let (first, second) = (new_message_id(), new_message_id());
        assert!(first.starts_with("msg_") && first.len() == 4 + 12 + 28);
        assert!(first < second);
    }
    #[test]
    fn the_boundary_after_a_turn_is_the_next_turns_message() {
        let turns = vec![json!({"id":"a","ordinal":1,"nativeTurnRef":{"nativeId":"msg_1"}}), json!({"id":"b","ordinal":2,"nativeTurnRef":{"nativeId":"msg_2"}})];
        assert_eq!(boundary_after_turn(&turns, &json!("a")).as_deref(), Some("msg_2"));
        assert_eq!(boundary_after_turn(&turns, &json!("b")), None);
    }
    #[test]
    fn a_tool_belongs_to_the_one_server_its_name_starts_with() {
        let servers = vec!["t3-code-abc".to_owned(), "other".to_owned()];
        assert_eq!(mcp_server_of("t3-code-abc_get", &servers), Some("t3-code-abc"));
        assert_eq!(mcp_server_of("bash", &servers), None);
        assert_eq!(mcp_server_of("apply_patch", &["apply".to_owned()]), None);
    }
}
