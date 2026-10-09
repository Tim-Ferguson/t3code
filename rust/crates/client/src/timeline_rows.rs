//! How a thread's turn items become timeline rows: tool activity grouped into a work log, settled
//! turns and superseded attempts folded behind one line, the live activity row of a running turn.
//! Port of `deriveMessagesTimelineRows` (`MessagesTimeline.logic.ts`), the work entries of
//! `session-logic.ts` and `packages/client-runtime/src/work-log/presentation.ts`.
//! T3 Code's own tools are named and summarised by `t3_tools`, and a shell command is read by
//! `command_label`. Left out, and named where it would sit: published HTML pages and MCP apps.
use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;

fn pattern(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("timeline pattern"))
}

/// `formatDuration` (`orchestrationTiming.ts`).
pub fn format_duration(ms: f64) -> String {
    if !ms.is_finite() || ms < 0.0 {
        return "0ms".into();
    }
    let round = |value: f64| (value + 0.5).floor();
    if ms < 1_000.0 {
        return format!("{}ms", round(ms).max(1.0));
    }
    if ms < 10_000.0 {
        let tenths = round(ms / 100.0) / 10.0;
        return if tenths >= 10.0 { "10s".into() } else { format!("{tenths:.1}s") };
    }
    if ms < 60_000.0 {
        return format!("{}s", round(ms / 1_000.0));
    }
    let total = round(ms / 1_000.0) as u64;
    let (hours, minutes, seconds) = (total / 3_600, total % 3_600 / 60, total % 60);
    [(hours, "h"), (minutes, "m"), (seconds, "s")].into_iter().filter(|(count, _)| *count > 0).map(|(count, unit)| format!("{count}{unit}")).collect::<Vec<_>>().join(" ")
}

// ---- work entries -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Thinking,
    Tool,
    Info,
    Error,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    InProgress,
    Completed,
    Failed,
    Stopped,
}
/// A site, an installed app or a themed logo a tool shows beside its name. Only the fields a row
/// may use are kept, and web addresses only when they are http(s) or data URLs.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolIcon {
    Website { page_url: String, favicon: Option<String>, favicon_dark: Option<String> },
    NativeApp { name: String },
    ThemedLogo { logo: String, logo_dark: Option<String> },
}
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSource {
    pub key: String,
    pub name: String,
    pub kind: String,
}

/// One tool call, thought or notice as the work log sees it (`WorkLogEntry`).
#[derive(Debug, Clone, PartialEq)]
pub struct Work {
    pub run_id: Option<String>,
    pub label: String,
    pub tone: Tone,
    pub item_type: String,
    pub status: Status,
    pub command: Option<String>,
    pub changed_files: Vec<String>,
    pub detail: Option<String>,
    pub tool_title: Option<String>,
    pub source_activity_kind: Option<&'static str>,
    pub tool_surface: Option<String>,
    pub tool_icon: Option<ToolIcon>,
    pub tool_source: Option<ToolSource>,
    pub viewed_image_path: Option<String>,
    pub tool_name: Option<String>,
    data_kind: Option<String>,
    read_paths: Vec<String>,
    search_label: Option<String>,
    display_failure: bool,
    command_failure: bool,
    retrying: bool,
    /// Nested under another item (a subagent's or a tool's own error), so it does not fail the run.
    nested: bool,
    dynamic_title: Option<String>,
    /// The call, when it is one of T3 Code's own tools (`resolveT3McpToolDefinition`).
    t3: Option<T3Call>,
}
/// What a T3 tool call keeps for its words and its group summary.
#[derive(Debug, Clone, PartialEq)]
struct T3Call {
    tool: &'static crate::t3_tools::Tool,
    /// `toolData`, where the row finds the call's arguments (`arguments`, `input` or `rawInput`).
    data: Value,
    input: Value,
    output: Value,
    output_failed: bool,
}

fn trimmed(value: &Value) -> Option<String> {
    value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}
fn first_line_trim(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const PATH_KEYS: [&str; 8] = ["path", "filePath", "file_path", "relativePath", "filename", "fileName", "newPath", "oldPath"];
fn collect_paths(value: &Value, paths: &mut Vec<String>, depth: usize) {
    if depth > 4 || paths.len() >= 8 {
        return;
    }
    if let Value::Array(list) = value {
        for entry in list {
            collect_paths(entry, paths, depth + 1);
            if paths.len() >= 8 {
                return;
            }
        }
        return;
    }
    let Value::Object(record) = value else { return };
    for key in PATH_KEYS {
        if let Some(candidate) = record.get(key).and_then(trimmed) {
            if paths.contains(&candidate) {
                continue;
            }
            paths.push(candidate);
            if paths.len() >= 8 {
                return;
            }
        }
    }
    for nested in ["locations", "item", "input", "result", "rawInput", "data", "changes"] {
        if let Some(inner) = record.get(nested) {
            collect_paths(inner, paths, depth + 1);
            if paths.len() >= 8 {
                return;
            }
        }
    }
}
/// `collectToolFilePaths`: structured paths from tool input, never file-body text.
pub fn tool_file_paths(data: &Value) -> Vec<String> {
    let mut paths = vec![];
    collect_paths(data, &mut paths, 0);
    paths
}

/// `formatWorkspaceRelativePath` with no workspace root: forward slashes, and a drive written
/// `/C:/x` shown as `C:/x`.
fn display_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' { path[1..].to_owned() } else { path }
}

fn first_string(record: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| trimmed(&record[*key]))
}
fn record_has_keys(value: &Value) -> bool {
    value.as_object().is_some_and(|record| !record.is_empty())
}
/// `formatSearchToolLabel`.
pub fn search_tool_label(data: &Value) -> Option<String> {
    let input = [&data["rawInput"], &data["input"], &data["item"]["input"]].into_iter().find(|record| record_has_keys(record)).unwrap_or(data);
    let query = first_string(input, &["pattern", "query", "searchTerm", "regex", "grep", "needle"]);
    let glob = first_string(input, &["glob", "globPattern", "glob_pattern", "include", "filePattern", "file_pattern"]);
    let target = first_string(input, &["path", "target_directory", "targetDirectory", "directory", "cwd", "root"]).and_then(|value| value.split(['\\', '/']).rev().find(|part| !part.is_empty() && *part != ".").map(str::to_owned));
    match (query, glob, target) {
        (Some(query), _, Some(target)) => Some(format!("Searched {query} in {target}")),
        (None, Some(glob), Some(target)) => Some(format!("Searched files {glob} in {target}")),
        (None, Some(glob), None) => Some(format!("Searched files {glob}")),
        (Some(query), _, None) => Some(format!("Searched {query}")),
        (None, None, Some(target)) => Some(format!("Searched in {target}")),
        _ => None,
    }
}
/// `formatReadToolLabel`.
pub fn read_tool_label(path: &str, extra: usize) -> String {
    let suffix = if extra > 0 { format!(" +{extra} more") } else { String::new() };
    let path = path.trim();
    if path.is_empty() { format!("Read file{suffix}") } else { format!("Read {path}{suffix}") }
}
/// `dynamicToolTitle`: a CUA call's own title, or the skill a Claude `Skill` call loads.
pub fn dynamic_tool_title(tool_name: Option<&str>, input: &Value) -> Option<String> {
    match tool_name {
        Some("cua_repl.js") => trimmed(&input["title"]),
        Some("Skill") => trimmed(&input["skill"]).map(|name| format!("Skill: {name}")),
        _ => None,
    }
}
/// `toolOutputIndicatesFailure`.
pub fn output_indicates_failure(text: &str) -> bool {
    static A: OnceLock<Regex> = OnceLock::new();
    static B: OnceLock<Regex> = OnceLock::new();
    static C: OnceLock<Regex> = OnceLock::new();
    static D: OnceLock<Regex> = OnceLock::new();
    static E: OnceLock<Regex> = OnceLock::new();
    static F: OnceLock<Regex> = OnceLock::new();
    static G: OnceLock<Regex> = OnceLock::new();
    static H: OnceLock<Regex> = OnceLock::new();
    pattern(&A, r"(?i)file not found|no files found|enoent|no such file|commandnotfoundexception|command not found|is not recognized as the name of a cmdlet|a parameter cannot be found that matches parameter name").is_match(text)
        || (pattern(&B, r"(?i)cannot find path").is_match(text) && pattern(&C, r"(?i)because it does not exist").is_match(text))
        || (pattern(&D, r"(?i)is not recognized").is_match(text) && pattern(&E, r"(?i)the term '").is_match(text))
        || pattern(&F, r"(?i)<exited with exit code\s+[1-9]\d*\s*>").is_match(text)
        || pattern(&G, r"(?i)exit(?:ed)? with exit code\s+[1-9]\d*").is_match(text)
        || pattern(&H, r"(?i)exit code\s*[:\s]\s*[1-9]\d*\b").is_match(text)
}

fn safe_image_url(value: &Value) -> Option<String> {
    let raw = trimmed(value).filter(|raw| raw.len() <= 4096)?;
    let lower = raw.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("data:")).then_some(raw)
}
fn safe_page_url(value: &Value) -> Option<String> {
    let raw = trimmed(value).filter(|raw| raw.len() <= 4096)?;
    let lower = raw.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then_some(raw)
}
fn tool_icon(value: &Value) -> Option<ToolIcon> {
    match value["_tag"].as_str()? {
        "website" => Some(ToolIcon::Website { page_url: safe_page_url(&value["pageUrl"])?, favicon: safe_image_url(&value["faviconUrl"]), favicon_dark: safe_image_url(&value["faviconUrlDark"]) }),
        "native-app" => {
            let app = &value["app"];
            match app["_tag"].as_str()? {
                "display-name" => trimmed(&app["displayName"]).filter(|name| name.len() <= 160).map(|name| ToolIcon::NativeApp { name }),
                "app-id" => trimmed(&app["appId"]).filter(|id| id.len() <= 512 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))).map(|name| ToolIcon::NativeApp { name }),
                _ => None,
            }
        }
        "themed-logo" => Some(ToolIcon::ThemedLogo { logo: safe_image_url(&value["logoUrl"])?, logo_dark: safe_image_url(&value["logoUrlDark"]) }),
        _ => None,
    }
}
/// `workEntryViewedImagePath` for the image a tool row shows when it is opened: the path the call
/// viewed, when it is one line and names an image.
pub fn viewed_image_path(item: &Value) -> Option<String> {
    trimmed(&item["viewedImagePath"]).filter(|path| path.len() <= 4096 && !path.contains(['\r', '\n']) && is_image_path(path))
}
fn is_image_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    [".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"].iter().any(|extension| lower.ends_with(extension))
}

fn status_of(item: &Value) -> Status {
    match item["status"].as_str() {
        Some("pending" | "running" | "waiting") => Status::InProgress,
        Some("completed") => Status::Completed,
        Some("failed") => Status::Failed,
        Some("cancelled" | "interrupted") => Status::Stopped,
        _ => Status::Idle,
    }
}

/// `providerErrorPresentation`.
pub fn error_label(item: &Value) -> (String, String) {
    let class = item["failure"]["class"].as_str().unwrap_or_default();
    let message = item["failure"]["message"].as_str().unwrap_or_default();
    let Some(retry) = item.get("retry").filter(|retry| retry.is_object()) else {
        let label = if class == "usage_limit" { "Usage limit reached".to_owned() } else { trimmed(&item["title"]).unwrap_or_else(|| "Provider error".into()) };
        return (label, message.to_owned());
    };
    let attempt = retry["attempt"].as_u64().unwrap_or(0);
    let progress = match retry["maxAttempts"].as_u64() {
        None => attempt.to_string(),
        Some(max) => format!("{attempt}/{max}"),
    };
    let status = item["status"].as_str().unwrap_or_default();
    let label = match status {
        "running" => format!("Retrying provider ({progress})"),
        "completed" => format!("Provider recovered ({progress} retries)"),
        "failed" => format!("{} after {progress} retries", if class == "usage_limit" { "Usage limit reached" } else { "Provider error" }),
        _ => format!("Provider retry stopped ({progress})"),
    };
    let delay = retry["retryDelayMs"].as_f64().filter(|delay| status == "running" && *delay > 0.0).map(|delay| {
        if delay < 1_000.0 {
            format!(" Retrying in {delay}ms.")
        } else {
            let seconds = format!("{:.1}", delay / 1_000.0);
            format!(" Retrying in {}s.", seconds.strip_suffix(".0").unwrap_or(&seconds))
        }
    });
    (label, format!("{message}{}", delay.unwrap_or_default()))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

impl Work {
    /// `projectedWorkEntry`, for an item the timeline treats as tool or notice activity.
    pub fn from_item(item: &Value) -> Work {
        let kind = item["type"].as_str().unwrap_or_default();
        let title = trimmed(&item["title"]);
        let tone = match kind {
            "error" => Tone::Info,
            "reasoning" => Tone::Thinking,
            "command_execution" | "file_change" | "file_search" | "web_search" | "dynamic_tool" | "subagent" | "thread_created" | "user_input_request" | "approval_request" => Tone::Tool,
            _ => Tone::Info,
        };
        let status = status_of(item);
        let tool_name = if kind == "dynamic_tool" { item["toolName"].as_str().map(str::to_owned) } else { None };
        let mut work = Work {
            run_id: item["runId"].as_str().map(str::to_owned),
            label: String::new(),
            tone,
            item_type: kind.to_owned(),
            status,
            command: None,
            changed_files: vec![],
            detail: None,
            tool_title: None,
            source_activity_kind: None,
            tool_surface: item["toolSurface"].as_str().filter(|surface| matches!(*surface, "browser" | "computer")).map(str::to_owned),
            tool_icon: tool_icon(&item["toolIcon"]),
            tool_source: {
                let source = &item["toolSource"];
                match (trimmed(&source["key"]).filter(|key| key.len() <= 512), trimmed(&source["name"]).filter(|name| name.len() <= 160), source["kind"].as_str()) {
                    (Some(key), Some(name), Some(kind @ ("browser" | "computer" | "integration"))) => Some(ToolSource { key, name, kind: kind.to_owned() }),
                    _ => None,
                }
            },
            viewed_image_path: trimmed(&item["viewedImagePath"]).filter(|path| path.len() <= 4096 && !path.contains(['\r', '\n']) && is_image_path(path)),
            tool_name: tool_name.clone(),
            data_kind: if kind == "dynamic_tool" { None } else { item["kind"].as_str().map(|kind| kind.trim().to_lowercase()).filter(|kind| !kind.is_empty()) },
            read_paths: vec![],
            search_label: None,
            display_failure: false,
            command_failure: false,
            retrying: kind == "error" && item.get("retry").is_some_and(Value::is_object),
            nested: item["parentItemId"].is_string(),
            dynamic_title: if kind == "dynamic_tool" { dynamic_tool_title(tool_name.as_deref(), &item["input"]) } else { None },
            t3: None,
        };
        let tool_data = if kind == "dynamic_tool" { serde_json::json!({"input": item["input"], "output": item["output"]}) } else { item.clone() };
        match kind {
            "thread_created" => work.label = "Created thread".into(),
            "compaction" => {
                work.label = crate::timeline_items::compaction_label(item);
                work.source_activity_kind = Some("context-compaction");
                work.detail = item["summary"].as_str().filter(|summary| !summary.is_empty()).map(str::to_owned);
            }
            "reasoning" => {
                work.label = title.unwrap_or_else(|| "Thinking".into());
                work.detail = item["text"].as_str().filter(|text| !text.is_empty()).map(str::to_owned);
            }
            "command_execution" => {
                work.label = title.clone().unwrap_or_else(|| "Ran command".into());
                work.command = item["input"].as_str().map(str::to_owned);
                work.tool_title = Some(title.unwrap_or_else(|| "Command".into()));
            }
            "file_change" => {
                let changes = item["changes"].as_array();
                work.label = title.clone().unwrap_or_else(|| match changes {
                    Some(changes) if changes.len() > 1 => format!("Changed {} files", changes.len()),
                    _ => format!("Changed {}", item["fileName"].as_str().unwrap_or_default()),
                });
                work.changed_files = match changes {
                    Some(changes) => changes.iter().filter_map(|change| change["path"].as_str().map(str::to_owned)).collect(),
                    None => vec![item["fileName"].as_str().unwrap_or_default().to_owned()],
                };
                work.tool_title = Some(title.unwrap_or_else(|| "File change".into()));
            }
            "file_search" => {
                work.label = title.clone().or_else(|| search_tool_label(item)).unwrap_or_else(|| "Searched files".into());
                work.detail = item["pattern"].as_str().filter(|pattern| !pattern.is_empty()).map(str::to_owned);
                work.tool_title = Some(title.unwrap_or_else(|| "File search".into()));
            }
            "web_search" => {
                work.label = title.clone().unwrap_or_else(|| "Searched the web".into());
                let patterns: Vec<&str> = item["patterns"].as_array().map(|list| list.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                work.detail = (!patterns.is_empty()).then(|| patterns.join(", "));
                work.tool_title = Some(title.unwrap_or_else(|| "Web search".into()));
            }
            "system_notice" => {
                work.label = item["message"].as_str().unwrap_or_default().to_owned();
                work.source_activity_kind = Some("runtime.warning");
            }
            "error" => {
                let (label, detail) = error_label(item);
                work.label = label;
                work.detail = Some(detail).filter(|detail| !detail.is_empty());
                let usage_limit = item["failure"]["class"] == "usage_limit";
                work.source_activity_kind = if usage_limit && item["status"] != "completed" {
                    Some("runtime.warning")
                } else if !work.retrying {
                    Some("runtime.error")
                } else {
                    None
                };
            }
            "dynamic_tool" => {
                let classified = classify(None, None, work.data_kind.as_deref(), tool_name.as_deref());
                let read_path = tool_file_paths(&serde_json::json!({"input": item["input"]})).into_iter().next();
                work.label = title.clone().unwrap_or_else(|| match classified {
                    Action::Read => read_tool_label(read_path.as_deref().unwrap_or_default(), 0),
                    Action::Search => search_tool_label(&serde_json::json!({"input": item["input"]})).or_else(|| tool_name.clone()).unwrap_or_else(|| "Tool call".into()),
                    _ => tool_name.clone().unwrap_or_else(|| "Tool call".into()),
                });
                work.tool_title = Some(title.or_else(|| tool_name.clone()).unwrap_or_else(|| "Tool".into()));
            }
            "approval_request" => {
                work.label = title.unwrap_or_else(|| "Approval requested".into());
                work.detail = item["prompt"].as_str().or_else(|| item["requestKind"].as_str()).filter(|text| !text.is_empty()).map(str::to_owned);
            }
            "user_input_request" => {
                work.label = title.unwrap_or_else(|| if item["questionAnswer"].is_object() { "Answered questions".into() } else { "Input requested".into() });
            }
            "notification" => work.label = item["summary"].as_str().unwrap_or_default().to_owned(),
            other => work.label = title.unwrap_or_else(|| other.replace('_', " ")),
        }
        // `workEntryToolName`: structured identity wins, including when it names a foreign server.
        let identity = if kind == "dynamic_tool" && tool_name.as_deref().is_some_and(|name| !name.is_empty()) {
            tool_name.clone()
        } else if let (Some(server), Some(tool)) = (tool_data["server"].as_str(), tool_data["tool"].as_str()) {
            Some(format!("{server}.{tool}"))
        } else if let Some(name) = tool_data["toolName"].as_str() {
            Some(name.to_owned())
        } else if crate::t3_tools::resolve(work.tool_title.as_deref()).is_some() {
            work.tool_title.clone()
        } else {
            Some(work.label.clone())
        };
        if let Some(tool) = crate::t3_tools::resolve(identity.as_deref()) {
            let first = |keys: &[&str]| keys.iter().map(|key| &tool_data[*key]).find(|value| !value.is_null()).cloned().unwrap_or(Value::Null);
            let (input, output) = if kind == "dynamic_tool" { (item["input"].clone(), item["output"].clone()) } else { (first(&["arguments", "input", "rawInput"]), first(&["output", "result", "rawOutput", "content"])) };
            let output_failed = crate::t3_tools::result_indicates_failure(&output);
            work.t3 = Some(T3Call { tool, data: tool_data.clone(), input, output, output_failed });
        }
        // What `workEntryReadRawPaths` and `workEntryDisplayLabel` read from the call's data.
        work.read_paths = if !work.changed_files.is_empty() {
            work.changed_files.clone()
        } else {
            let structured = if kind == "dynamic_tool" { tool_file_paths(&serde_json::json!({"input": item["input"]})) } else { vec![] };
            if structured.is_empty() { tool_file_paths(&tool_data) } else { structured }
        }
        .iter()
        .map(|path| display_path(path))
        .collect();
        work.search_label = search_tool_label(&tool_data);
        // `workEntryIndicatesToolFailure`: the call's own outcome first, then what its output says.
        let tool_like = work.presentation_tool_like();
        let mut command_failure = false;
        if tool_like && kind == "command_execution" {
            command_failure = item["outputIndicatesFailure"] == true || item["exitCode"].as_i64().is_some_and(|code| code != 0) || item["output"].as_str().is_some_and(|output| output_indicates_failure(&output.chars().take(32_768).collect::<String>()));
        }
        let detail_failure = work.detail.as_deref().is_some_and(|detail| !detail.is_empty() && output_indicates_failure(detail));
        let t3_failure = work.t3.as_ref().is_some_and(|call| call.output_failed);
        work.command_failure = command_failure;
        work.display_failure = work.tone == Tone::Error || work.status == Status::Failed || (tool_like && (t3_failure || command_failure || detail_failure));
        work
    }

    /// `resolveWorkEntryToolPresentation`: the words, icon and pull request action of a T3 tool call,
    /// at `status` (a result that reads as a failure makes it failed whatever the provider said).
    pub fn t3_words(&self, status: Status) -> Option<crate::t3_tools::Words> {
        let call = self.t3.as_ref()?;
        let lifecycle = if call.output_failed {
            "failed"
        } else {
            match status {
                Status::InProgress => "inProgress",
                Status::Completed => "completed",
                Status::Failed => "failed",
                Status::Stopped => "stopped",
                Status::Idle => "idle",
            }
        };
        Some(crate::t3_tools::words(call.tool, Some(lifecycle), &call.data))
    }
    /// `workLogEntryIsToolLike` of `session-logic.ts`.
    pub fn is_tool_like(&self) -> bool {
        matches!(self.tone, Tone::Tool | Tone::Thinking | Tone::Error) || self.command.is_some()
    }
    /// The variant in `presentation.ts`, which also counts the tool item types.
    fn presentation_tool_like(&self) -> bool {
        matches!(self.tone, Tone::Tool | Tone::Thinking | Tone::Error) || self.command.as_deref().is_some_and(|command| !command.trim().is_empty()) || matches!(self.item_type.as_str(), "command_execution" | "file_change" | "web_search")
    }
    /// `workEntryDisplayIndicatesToolFailure`.
    pub fn display_failure(&self) -> bool {
        self.display_failure
    }
    /// `workEntryIndicatesToolFailure`, which also reads the command text.
    pub fn indicates_failure(&self) -> bool {
        self.display_failure || (self.presentation_tool_like() && self.command.as_deref().is_some_and(|command| !command.is_empty() && output_indicates_failure(&[self.detail.as_deref().unwrap_or_default(), command].iter().filter(|part| !part.is_empty()).copied().collect::<Vec<_>>().join("\n"))))
    }
    /// `workEntryIndicatesToolSuccess` of `session-logic.ts`.
    pub fn indicates_success(&self) -> bool {
        if !self.is_tool_like() || self.indicates_failure() || (self.tone == Tone::Thinking && self.item_type != "reasoning") {
            return false;
        }
        !matches!(self.status, Status::Failed | Status::InProgress | Status::Stopped | Status::Idle)
    }
    /// Tool-like with neither a clear success nor a failure.
    pub fn is_neutral(&self) -> bool {
        self.is_tool_like() && !self.indicates_failure() && !self.indicates_success()
    }
    fn failed_top_level_error(&self) -> bool {
        self.item_type == "error" && self.status == Status::Failed && !self.nested
    }
}

// ---- tool classification and summaries ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Command,
    FileChange,
    Search,
    Read,
    Other,
}
/// `classifyToolActivity`.
fn classify(item_type: Option<&str>, request_kind: Option<&str>, kind: Option<&str>, tool_name: Option<&str>) -> Action {
    static NAME: OnceLock<Regex> = OnceLock::new();
    static STRIP: OnceLock<Regex> = OnceLock::new();
    let token = tool_name.map(str::trim).filter(|name| !name.is_empty() && !pattern(&NAME, r"__|[./]").is_match(name)).map(|name| pattern(&STRIP, r"[_\s-]").replace_all(name, "").to_lowercase());
    let token = token.as_deref();
    match item_type {
        Some("command_execution") => return Action::Command,
        Some("file_change") => return Action::FileChange,
        Some("web_search") => return Action::Search,
        _ => {}
    }
    if request_kind == Some("command") || kind == Some("execute") {
        return Action::Command;
    }
    if request_kind == Some("file-change") || matches!(kind, Some("edit" | "move" | "delete" | "write")) {
        return Action::FileChange;
    }
    if kind == Some("search") || matches!(token, Some("find" | "grep" | "glob" | "rg" | "ls")) {
        return Action::Search;
    }
    if request_kind == Some("file-read") || kind == Some("read") {
        return Action::Read;
    }
    if matches!(token, Some("terminal" | "bash" | "shell")) {
        return Action::Command;
    }
    if matches!(token, Some("read" | "readfile")) {
        return Action::Read;
    }
    Action::Other
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupAction {
    Read,
    Edit,
    Command,
    ThreadCreate,
    Browser,
    Device,
    CodeSearch,
    Search,
    Other,
    Update,
    LinkPr,
    UnlinkPr,
    WatchPr,
    UnwatchPr,
    ListPrs,
}
fn local_code_search(work: &Work) -> bool {
    static GREP: OnceLock<Regex> = OnceLock::new();
    work.item_type == "file_search" || (work.item_type == "web_search" && pattern(&GREP, r"(?i)\bgrep\b").is_match(&normalize_compact_label(work.tool_title.as_deref().unwrap_or(&work.label))))
}
/// `normalizeCompactToolLabel`: a trailing "complete" or "completed" dropped.
pub fn normalize_compact_label(value: &str) -> String {
    static TAIL: OnceLock<Regex> = OnceLock::new();
    pattern(&TAIL, r"(?i)\s+(?:complete|completed)\s*$").replace(value, "").trim().to_owned()
}
/// `toolGroupAction`.
pub fn group_action(work: &Work) -> GroupAction {
    if work.item_type == "thread_created" {
        return GroupAction::ThreadCreate;
    }
    if let Some(words) = work.t3_words(work.status) {
        match words.action {
            Some("link-pr") => return GroupAction::LinkPr,
            Some("unlink-pr") => return GroupAction::UnlinkPr,
            Some("watch-pr") => return GroupAction::WatchPr,
            Some("unwatch-pr") => return GroupAction::UnwatchPr,
            Some("list-prs") => return GroupAction::ListPrs,
            _ => {}
        }
        match words.icon {
            "browser" => return GroupAction::Browser,
            "device" => return GroupAction::Device,
            _ => {}
        }
    }
    if work.viewed_image_path.is_some() {
        return GroupAction::Read;
    }
    if matches!(work.item_type.as_str(), "approval_request" | "user_input_request") {
        return if work.is_tool_like() { GroupAction::Other } else { GroupAction::Update };
    }
    let item_type = match work.item_type.as_str() {
        kind @ ("command_execution" | "file_change" | "web_search") => Some(kind),
        _ => None,
    };
    let classified = classify(item_type, None, work.data_kind.as_deref(), work.tool_name.as_deref());
    if classified == Action::Read {
        return GroupAction::Read;
    }
    if classified == Action::FileChange || work.item_type == "file_change" {
        return GroupAction::Edit;
    }
    if classified == Action::Command || work.item_type == "command_execution" || work.command.is_some() {
        return GroupAction::Command;
    }
    if classified == Action::Search {
        return if work.item_type == "web_search" && !local_code_search(work) { GroupAction::Search } else { GroupAction::CodeSearch };
    }
    if local_code_search(work) {
        return GroupAction::CodeSearch;
    }
    if work.item_type == "web_search" {
        return GroupAction::Search;
    }
    if !work.changed_files.is_empty() {
        return GroupAction::Edit;
    }
    if work.is_tool_like() { GroupAction::Other } else { GroupAction::Update }
}

fn action_count(action: GroupAction, entries: &[&Work]) -> usize {
    if action != GroupAction::Edit {
        return entries.len();
    }
    let mut files = HashSet::new();
    let mut without = 0;
    for entry in entries {
        if entry.changed_files.is_empty() {
            without += 1;
        } else {
            files.extend(entry.changed_files.iter().cloned());
        }
    }
    files.len() + without
}
fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}
fn action_label(action: GroupAction, count: usize) -> String {
    match action {
        GroupAction::Read => format!("Read {}", plural(count, "file", "files")),
        GroupAction::Edit => format!("Changed {}", plural(count, "file", "files")),
        GroupAction::Command => format!("Ran {}", plural(count, "command", "commands")),
        GroupAction::ThreadCreate => format!("Created {}", plural(count, "thread", "threads")),
        GroupAction::Device => format!("Used device controls {}", plural(count, "time", "times")),
        GroupAction::Browser => format!("Used browser {}", plural(count, "time", "times")),
        GroupAction::Search => format!("Searched the web {}", plural(count, "time", "times")),
        GroupAction::CodeSearch => format!("Searched code {}", plural(count, "time", "times")),
        GroupAction::Other => format!("Used {}", plural(count, "tool", "tools")),
        GroupAction::Update => format!("Received {}", plural(count, "update", "updates")),
        GroupAction::LinkPr => format!("Linked {}", plural(count, "pull request", "pull requests")),
        GroupAction::UnlinkPr => format!("Unlinked {}", plural(count, "pull request", "pull requests")),
        GroupAction::WatchPr => format!("Watching {}", plural(count, "pull request", "pull requests")),
        GroupAction::UnwatchPr => format!("Stopped watching {}", plural(count, "pull request", "pull requests")),
        GroupAction::ListPrs => if count == 1 { "Checked linked pull requests".to_owned() } else { format!("Checked linked pull requests {count} times") },
    }
}
fn action_priority(action: GroupAction) -> u8 {
    match action {
        GroupAction::Command | GroupAction::Edit | GroupAction::ThreadCreate => 0,
        GroupAction::Other | GroupAction::Update => 2,
        _ => 1,
    }
}
/// The name a group action goes by in the original, for the actions a T3 tool's summary also names.
fn group_action_name(action: GroupAction) -> Option<&'static str> {
    Some(match action {
        GroupAction::Browser => "browser",
        GroupAction::Device => "device",
        GroupAction::LinkPr => "link-pr",
        GroupAction::UnlinkPr => "unlink-pr",
        GroupAction::WatchPr => "watch-pr",
        GroupAction::UnwatchPr => "unwatch-pr",
        GroupAction::ListPrs => "list-prs",
        _ => return None,
    })
}
/// `summarizeToolGroup`: at most two action categories, every other call counted in a remainder.
pub fn summarize_group(entries: &[&Work]) -> (String, bool) {
    let tools: Vec<&Work> = entries.iter().copied().filter(|entry| entry.item_type != "reasoning").collect();
    if !entries.is_empty() && tools.is_empty() {
        return (if entries.len() == 1 { "Thought".into() } else { format!("Thought (×{})", entries.len()) }, false);
    }
    // A group is a T3 tool's summary action when it is one of T3 Code's own tools, else the call's action.
    #[derive(PartialEq)]
    enum Key {
        Action(GroupAction),
        T3(&'static str),
    }
    let mut groups: Vec<(Key, GroupAction, Vec<&Work>)> = vec![];
    let mut sources: Vec<&ToolSource> = vec![];
    for entry in &tools {
        let t3_action = entry.t3.as_ref().map(|call| call.tool.summary_action.as_str());
        if let (Some(source), None) = (&entry.tool_source, t3_action) {
            // A later call from the same source replaces the earlier one's description in place.
            match sources.iter().position(|known| known.key == source.key) {
                Some(position) => sources[position] = source,
                None => sources.push(source),
            }
            continue;
        }
        let action = group_action(entry);
        // The original keys a group by the name of the action, and a browser or pull request tool's summary action is also its group action.
        let key = match t3_action {
            Some(name) => Key::T3(name),
            None => Key::Action(action),
        };
        let same = |known: &Key| match (known, &key) {
            (Key::T3(a), Key::T3(b)) => a == b,
            (Key::Action(a), Key::Action(b)) => a == b,
            (Key::T3(name), Key::Action(action)) | (Key::Action(action), Key::T3(name)) => group_action_name(*action) == Some(*name),
        };
        match groups.iter_mut().find(|(known, ..)| same(known)) {
            Some((_, _, members)) => members.push(entry),
            None => groups.push((key, action, vec![entry])),
        }
    }
    let summaries: Vec<(usize, usize, u8, String, usize)> = groups
        .iter()
        .enumerate()
        .map(|(index, (key, action, members))| match key {
            Key::T3(name) => {
                let calls: Vec<crate::t3_tools::Call> = members
                    .iter()
                    .map(|member| {
                        let call = member.t3.as_ref().expect("a T3 group holds T3 calls");
                        let outcome = if member.status == Status::Failed || member.tone == Tone::Error { "failed" } else if member.status == Status::Completed { "completed" } else { "unfinished" };
                        crate::t3_tools::Call { input: &call.input, output: &call.output, outcome }
                    })
                    .collect();
                let (label, failed) = crate::t3_tools::summarize_calls(name, &calls);
                (index, members.len(), crate::t3_tools::action_priority(name), label, failed)
            }
            Key::Action(_) => (index, members.len(), action_priority(*action), action_label(*action, action_count(*action, members)), members.iter().filter(|member| member.display_failure()).count()),
        })
        .collect();
    let mut selected: Vec<_> = summaries.iter().collect();
    selected.sort_by_key(|(index, _, priority, _, _)| (*priority, *index));
    selected.truncate(2);
    selected.sort_by_key(|(index, ..)| *index);
    let mut labels: Vec<String> = selected.iter().map(|(_, _, _, label, _)| label.clone()).collect();
    if !sources.is_empty() {
        let names: Vec<&str> = sources.iter().map(|source| source.name.as_str()).collect();
        let formatted = match names.as_slice() {
            [one] => (*one).to_owned(),
            [a, b] => format!("{a} and {b}"),
            [head @ .., last] => format!("{}, and {last}", head.join(", ")),
            [] => String::new(),
        };
        let integrations = sources.iter().all(|source| source.kind == "integration");
        labels.insert(0, format!("Used {formatted}{}", if integrations { if sources.len() == 1 { " integration" } else { " integrations" } } else { "" }));
    }
    let sourced = tools.iter().filter(|entry| entry.tool_source.is_some() && entry.t3.is_none()).count();
    let remaining = tools.len() as isize - sourced as isize - selected.iter().map(|(_, count, ..)| *count as isize).sum::<isize>();
    if remaining > 0 {
        labels.push(format!("Performed {remaining} other {}", if remaining == 1 { "action" } else { "actions" }));
    }
    let sentences: Vec<String> = labels.iter().enumerate().map(|(index, label)| if index == 0 { label.clone() } else { let mut chars = label.chars(); chars.next().map_or_else(String::new, |first| first.to_lowercase().chain(chars).collect()) }).collect();
    let summary = match sentences.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [head @ .., last] => format!("{}, and {last}", head.join(", ")),
    };
    (summary, summaries.iter().any(|(_, _, _, _, failed)| *failed > 0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    /// Every call is one of T3 Code's pull request tools.
    PullRequest,
    Action(GroupAction),
    Reasoning,
    DynamicTool,
    AgentTool,
    ToneTool,
    Other,
    Mixed,
}
/// `toolGroupSummaryKind`: which icon a group's summary line carries.
pub fn summary_kind(entries: &[&Work]) -> SummaryKind {
    let tools: Vec<&Work> = entries.iter().copied().filter(|entry| entry.item_type != "reasoning").collect();
    if !entries.is_empty() && tools.is_empty() {
        return SummaryKind::Reasoning;
    }
    if !tools.is_empty() && tools.iter().all(|entry| entry.t3_words(entry.status).is_some_and(|words| words.icon == "pull-request")) {
        return SummaryKind::PullRequest;
    }
    let actions: HashSet<GroupAction> = tools.iter().map(|entry| group_action(entry)).collect();
    if actions.len() != 1 {
        return SummaryKind::Mixed;
    }
    let action = *actions.iter().next().expect("one action");
    if action != GroupAction::Other {
        return SummaryKind::Action(action);
    }
    let kinds: HashSet<u8> = tools
        .iter()
        .map(|entry| match (entry.item_type.as_str(), entry.tone) {
            ("dynamic_tool", _) => 0,
            ("subagent", _) | (_, Tone::Thinking) => 1,
            (_, Tone::Tool) => 2,
            _ => 3,
        })
        .collect();
    match kinds.iter().next().filter(|_| kinds.len() == 1) {
        Some(0) => SummaryKind::DynamicTool,
        Some(1) => SummaryKind::AgentTool,
        Some(2) => SummaryKind::ToneTool,
        Some(_) => SummaryKind::Other,
        None => SummaryKind::Mixed,
    }
}

/// `commandDisplayText`: a plain `sh -c` wrapper removed and control characters made visible.
pub fn command_display_text(command: &str) -> String {
    crate::command_label::display_text(command)
}
/// `commandProgramName`: the program a command runs, if it names one.
pub fn command_program_name(command: &str) -> Option<String> {
    crate::command_label::program_name(command)
}

/// `workEntryDisplayLabel`.
pub fn display_label(work: &Work) -> String {
    if work.item_type == "system_notice" {
        return work.label.clone();
    }
    if work.item_type == "reasoning" || work.tone == Tone::Thinking {
        return work.detail.as_deref().map(first_line_trim).filter(|thought| !thought.is_empty()).unwrap_or_else(|| work.label.clone());
    }
    if let Some(words) = work.t3_words(work.status) {
        return words.display_name;
    }
    if let Some(command) = work.command.as_deref().filter(|command| !command.trim().is_empty()) {
        return command_display_text(command);
    }
    let action = group_action(work);
    if matches!(action, GroupAction::CodeSearch | GroupAction::Search) {
        let label = if work.item_type == "file_search" { Some(work.label.clone()).filter(|label| !label.is_empty()) } else { work.search_label.clone() };
        if let Some(label) = label {
            return label;
        }
    }
    if action == GroupAction::Read {
        if let Some(first) = work.read_paths.first() {
            return read_tool_label(first, work.read_paths.len() - 1);
        }
    }
    if let Some(title) = dynamic_title(work) {
        return title;
    }
    let detail = work.detail.as_deref().map(str::trim).filter(|detail| !detail.is_empty());
    let search_output = matches!(action, GroupAction::CodeSearch | GroupAction::Search) && detail.is_some_and(|detail| detail.contains(['\r', '\n']));
    if let Some(detail) = detail {
        if !work.retrying && action != GroupAction::Read && !search_output {
            return detail.to_owned();
        }
    }
    if let Some(first) = work.changed_files.first() {
        return if work.changed_files.len() == 1 { first.clone() } else { format!("{first} +{} more", work.changed_files.len() - 1) };
    }
    if action == GroupAction::Read && work.viewed_image_path.is_none() {
        return "Read file".into();
    }
    capitalize(&normalize_compact_label(work.tool_title.as_deref().filter(|title| !title.is_empty()).unwrap_or(&work.label)))
}
fn dynamic_title(work: &Work) -> Option<String> {
    work.dynamic_title.clone()
}
/// `liveWorkEntryLabel`: present tense while the call runs, past once it has finished.
pub fn live_label(work: &Work, active: bool) -> String {
    let status = match work.status {
        Status::Failed | Status::Stopped | Status::Idle => work.status,
        Status::InProgress => Status::InProgress,
        Status::Completed if active => Status::InProgress,
        Status::Completed => Status::Completed,
    };
    if work.item_type == "reasoning" {
        return work.detail.as_deref().map(first_line_trim).filter(|thought| !thought.is_empty()).unwrap_or_else(|| if status == Status::InProgress { "Thinking".into() } else { "Thought".into() });
    }
    if let Some(words) = work.t3_words(status) {
        return words.display_name;
    }
    if let Some(command) = work.command.as_deref().map(str::trim).filter(|command| !command.is_empty()) {
        let verb = match status {
            Status::InProgress => "Running",
            Status::Failed => "Failed",
            Status::Stopped => "Stopped",
            _ => "Ran",
        };
        return format!("{verb} {}", command_program_name(command).unwrap_or_else(|| "command".into()));
    }
    display_label(work)
}
/// `singleToolCallLabel`: the heading of a lone call.
pub fn single_call_label(work: &Work) -> String {
    if work.item_type == "reasoning" {
        return work.detail.as_deref().map(first_line_trim).filter(|thought| !thought.is_empty()).unwrap_or_else(|| "Thought".into());
    }
    if let Some(words) = work.t3_words(work.status) {
        return words.display_name;
    }
    if let Some(title) = dynamic_title(work) {
        return title;
    }
    if work.item_type == "web_search" {
        return work.tool_title.clone().unwrap_or_else(|| "Web search".into());
    }
    display_label(work)
}

/// `workEntryIsVisibleInGroup`.
pub fn visible_in_group(work: &Work, expanded_entry: bool) -> bool {
    if work.item_type == "reasoning" {
        return work.detail.as_deref().is_some_and(|detail| !detail.trim().is_empty());
    }
    (expanded_entry && work.status == Status::InProgress) || work.status == Status::Stopped || !work.is_neutral()
}

// ---- timeline entries ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    pub id: String,
    pub run_id: String,
    pub status: String,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Message { role_user: bool, run_id: Option<String>, streaming: bool, id: String, updated_at: String, steer: bool },
    /// A plan or other row that is never grouped or folded.
    Standalone { run_id: Option<String> },
    Work(Work),
    /// A fork, handoff, interrupt marker, secret request or subagent.
    Event { item_type: String, run_id: Option<String>, provider_turn_id: Option<String>, status: String },
}
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Index into the thread's `visibleTurnItems`.
    pub row: usize,
    pub id: String,
    pub created_at: String,
    pub attempt: Option<Attempt>,
    pub kind: Kind,
}
impl Entry {
    fn work(&self) -> Option<&Work> {
        if let Kind::Work(work) = &self.kind { Some(work) } else { None }
    }
    fn is_work(&self, test: impl Fn(&Work) -> bool) -> bool {
        self.work().is_some_and(test)
    }
    /// `timelineEntryRunId`: the run an assistant message, plan or work entry belongs to.
    fn response_run(&self) -> Option<&str> {
        match &self.kind {
            Kind::Message { role_user: false, run_id, .. } => run_id.as_deref(),
            Kind::Work(work) => work.run_id.as_deref(),
            Kind::Standalone { run_id } => run_id.as_deref(),
            _ => None,
        }
    }
}

fn start_of(row: &Value) -> String {
    let item = &row["item"];
    item["startedAt"].as_str().or_else(|| item["updatedAt"].as_str()).unwrap_or_default().to_owned()
}

/// `deriveTimelineEntriesFromVisibleTurnItems`, for the committed rows.
pub fn entries(items: &[Value], attempts: &[Value], nodes: &[Value]) -> Vec<Entry> {
    let attempt_by_root: HashMap<&str, &Value> = attempts.iter().filter_map(|attempt| Some((attempt["rootNodeId"].as_str()?, attempt))).collect();
    let node_by_id: HashMap<&str, &Value> = nodes.iter().filter_map(|node| Some((node["id"].as_str()?, node))).collect();
    let resolve = |item: &Value| -> Option<Attempt> {
        let (mut node_id, run_id) = (item["nodeId"].as_str()?, item["runId"].as_str()?);
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(node_id) {
                return None;
            }
            if let Some(attempt) = attempt_by_root.get(node_id).filter(|attempt| attempt["runId"] == run_id) {
                return Some(Attempt { id: attempt["id"].as_str()?.to_owned(), run_id: run_id.to_owned(), status: attempt["status"].as_str().unwrap_or_default().to_owned() });
            }
            let node = node_by_id.get(node_id)?;
            if let Some(attempt) = node["rootNodeId"].as_str().and_then(|root| attempt_by_root.get(root)).filter(|attempt| attempt["runId"] == run_id) {
                return Some(Attempt { id: attempt["id"].as_str()?.to_owned(), run_id: run_id.to_owned(), status: attempt["status"].as_str().unwrap_or_default().to_owned() });
            }
            node_id = node["parentNodeId"].as_str()?;
        }
    };
    let answered: HashSet<String> = items.iter().filter_map(|row| row["item"]["questionAnswer"]["requestId"].as_str().filter(|_| row["item"]["type"] == "user_input_request").map(|id| format!("async-answer:{id}"))).collect();
    let mut result = vec![];
    for (index, row) in items.iter().enumerate() {
        let item = &row["item"];
        let kind = item["type"].as_str().unwrap_or_default();
        if kind == "command_execution" && item["input"] == "Preparing workspace" || (kind == "error" && item["status"] == "cancelled" && item["failure"]["code"] == "workspace_preparation_failed") {
            continue;
        }
        // Task progress belongs to the composer and a checkpoint to the changed-files card; neither
        // sits between the conversation's entries, so neither splits a group of tool calls.
        if matches!(kind, "todo_list" | "checkpoint") {
            continue;
        }
        if kind == "user_message" && item["messageId"].as_str().is_some_and(|id| answered.contains(id)) {
            continue;
        }
        let id = item["id"].as_str().unwrap_or_default().to_owned();
        let run_id = item["runId"].as_str().map(str::to_owned);
        let entry_kind = match kind {
            "user_message" | "assistant_message" => Kind::Message {
                role_user: kind == "user_message",
                run_id: run_id.clone(),
                streaming: kind == "assistant_message" && item["streaming"] == true,
                id: item["messageId"].as_str().unwrap_or_default().to_owned(),
                updated_at: item["updatedAt"].as_str().unwrap_or_default().to_owned(),
                steer: kind == "user_message" && matches!(item["inputIntent"].as_str(), Some("steer" | "promoted_queued_to_steer")),
            },
            "proposed_plan" => Kind::Standalone { run_id: run_id.clone() },
            "fork" | "handoff" | "run_interrupt_request" | "run_interrupt_result" | "secret_request" | "subagent" => Kind::Event {
                item_type: kind.to_owned(),
                run_id,
                provider_turn_id: item["providerTurnId"].as_str().map(str::to_owned),
                status: item["status"].as_str().unwrap_or_default().to_owned(),
            },
            _ => Kind::Work(Work::from_item(item)),
        };
        let id = if let Kind::Message { id: message_id, .. } = &entry_kind { message_id.clone() } else { id };
        result.push(Entry { row: index, id, created_at: start_of(row), attempt: resolve(item), kind: entry_kind });
    }
    result
}

// ---- rows ---------------------------------------------------------------------------------------

/// The run the rows are derived for (`TimelineLatestRun`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LatestRun {
    pub id: String,
    pub status: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}
pub struct Input<'a> {
    pub entries: &'a [Entry],
    pub latest_run: Option<&'a LatestRun>,
    pub running_run_id: Option<&'a str>,
    pub expanded_runs: &'a BTreeSet<String>,
    pub expanded_attempts: &'a BTreeSet<String>,
    pub expanded_groups: &'a BTreeSet<String>,
    pub is_working: bool,
    pub runless_work_active: bool,
    pub active_turn_started_at: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// An entry shown as itself: a message, plan, event, or a failure, notice or lone notification.
    Entry { entry: usize },
    /// A lone tool call or thought.
    Single { entry: usize, label: String, continues: bool },
    /// A collapsed or expanded group of tool calls under one summary line.
    Toggle { id: String, group: String, run_id: Option<String>, entries: Vec<usize>, summary: String, kind: SummaryKind, surface: Option<String>, icon: Option<ToolIcon>, has_failure: bool, expanded: bool, continues: bool },
    /// The live line of the running turn: the call in progress, or the latest finished one.
    Live { id: String, group: String, entry: usize, entries: Vec<usize>, label: String, expanded: bool, active: bool, thought: Option<usize>, surface: Option<String>, icon: Option<ToolIcon>, failed: bool, continues: bool },
    /// The members of an expanded group.
    Members { group: String, entries: Vec<usize> },
    Working { since: Option<String> },
    /// "Thinking" when a running turn has no activity to show; a failed last call keeps its group reachable.
    Thinking { group: Option<String>, expanded: bool, entries: Vec<usize>, continues: bool },
    TurnFold { run_id: String, label: String, expanded: bool, created_at: String },
    AttemptFold { run_id: String, attempt_id: String, label: String, expanded: bool, created_at: String },
    Compaction { entry: usize, label: String, active: bool },
    /// Subagents launched together by one provider turn.
    Subagents { entries: Vec<usize> },
    /// A row the derivation leaves out (a to-do list, a checkpoint), shown where it happened.
    Item { row: usize },
    /// A thread the turn created, shown again below the turn's final answer.
    Created { entry: usize },
    /// The meta line of a settled response's final answer (status, copy, time), under it or below the
    /// tool calls that follow it. It ends a run of work, which is what "continues" reads.
    Footer { message: usize },
}

struct TurnFold {
    run_id: String,
    created_at: String,
    hidden: HashSet<usize>,
    label: String,
}
struct AttemptFold {
    run_id: String,
    attempt_id: String,
    created_at: String,
    hidden: HashSet<usize>,
}

fn starts_response(entry: &Entry) -> bool {
    matches!(&entry.kind, Kind::Message { role_user: true, steer: false, .. }) || entry.is_work(|work| work.item_type == "notification")
}
fn is_persistent_resource(entry: &Entry) -> bool {
    matches!(&entry.kind, Kind::Event { item_type, .. } if matches!(item_type.as_str(), "fork" | "secret_request"))
}
fn iso_ms(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|at| at.timestamp_millis() as f64)
}
fn elapsed(start: &str, end: &str) -> Option<f64> {
    Some((iso_ms(end)? - iso_ms(start)?).max(0.0))
}
fn max_iso(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (None, b) => b.map(str::to_owned),
        (a, None) => a.map(str::to_owned),
        (Some(a), Some(b)) => match (iso_ms(a), iso_ms(b)) {
            (Some(x), Some(y)) => Some(if y > x { b } else { a }.to_owned()),
            (None, _) => Some(b.to_owned()),
            _ => Some(a.to_owned()),
        },
    }
}

fn live_subagent_ids(entries: &[Entry]) -> HashSet<usize> {
    let mut visible = HashSet::new();
    let mut batch: Vec<usize> = vec![];
    let flush = |batch: &mut Vec<usize>, visible: &mut HashSet<usize>| {
        if batch.iter().any(|&index| matches!(&entries[index].kind, Kind::Event { status, .. } if matches!(status.as_str(), "pending" | "running" | "waiting"))) {
            visible.extend(batch.iter().copied());
        }
        batch.clear();
    };
    for (index, entry) in entries.iter().enumerate() {
        let Kind::Event { item_type, run_id, provider_turn_id, .. } = &entry.kind else {
            flush(&mut batch, &mut visible);
            continue;
        };
        if item_type != "subagent" {
            flush(&mut batch, &mut visible);
            continue;
        }
        if let Some(&previous) = batch.last() {
            if let Kind::Event { run_id: previous_run, provider_turn_id: previous_turn, .. } = &entries[previous].kind {
                if previous_run != run_id || previous_turn != provider_turn_id {
                    flush(&mut batch, &mut visible);
                }
            }
        }
        batch.push(index);
    }
    flush(&mut batch, &mut visible);
    visible
}

fn failed_runs(entries: &[Entry], latest: Option<&LatestRun>) -> HashSet<String> {
    let mut failed = HashSet::new();
    if let Some(latest) = latest.filter(|latest| latest.status == "failed") {
        failed.insert(latest.id.clone());
    }
    for entry in entries {
        if let Some(run) = entry.work().filter(|work| work.failed_top_level_error()).and_then(|work| work.run_id.as_ref()) {
            failed.insert(run.clone());
        }
    }
    failed
}

/// Rows for a thread: the whole of `deriveMessagesTimelineRows` minus what the module comment lists.
pub fn derive(input: &Input) -> Vec<Row> {
    let settled = settle_superseded_reasoning(input.entries);
    let entries: &[Entry] = &settled;
    let mut rows: Vec<Row> = vec![];

    // Terminal assistant message of each response: the last one before the next user message, per run.
    let mut terminal: HashSet<usize> = HashSet::new();
    {
        let mut last: HashMap<String, usize> = HashMap::new();
        let mut unkeyed = 0;
        for (index, entry) in entries.iter().enumerate() {
            match &entry.kind {
                Kind::Message { role_user: true, .. } => unkeyed += 1,
                Kind::Message { role_user: false, .. } => {
                    last.insert(entry.response_run().map(|run| format!("turn:{run}")).unwrap_or_else(|| format!("unkeyed:{unkeyed}")), index);
                }
                _ => {}
            }
        }
        terminal.extend(last.into_values());
    }
    let unsettled: Option<String> = match input.running_run_id {
        Some(run) => Some(run.to_owned()),
        None => input.latest_run.and_then(|latest| {
            let settled = latest.completed_at.is_some() && !matches!(latest.status.as_str(), "running" | "starting" | "waiting");
            (!settled).then(|| latest.id.clone())
        }),
    };
    let failed = failed_runs(entries, input.latest_run);
    let live_subagents = live_subagent_ids(entries);
    let last_boundary = entries.iter().rposition(starts_response);
    let attempt_folds = superseded_folds(entries, &failed, &live_subagents);
    // Runs of the response that is still open.
    let mut active_runs: HashSet<String> = HashSet::new();
    if let Some(run) = &unsettled {
        active_runs.insert(run.clone());
        if input.is_working {
            let from = last_boundary.map_or(0, |index| index + 1);
            for entry in &entries[from..] {
                if let Some(run) = entry.response_run() {
                    active_runs.insert(run.to_owned());
                }
            }
        }
    }
    let runless_active = input.is_working && input.runless_work_active;
    let mut unfolded = active_runs.clone();
    unfolded.extend(failed.iter().cloned());
    let turn_folds = turn_folds(entries, &terminal, input.latest_run, &unfolded, runless_active, &live_subagents);

    let mut collapsed: HashSet<usize> = HashSet::new();
    for fold in turn_folds.values() {
        if !input.expanded_runs.contains(&fold.run_id) {
            collapsed.extend(fold.hidden.iter().copied());
        }
    }
    let mut collapsed_attempts: HashSet<usize> = HashSet::new();
    for fold in attempt_folds.values() {
        if !input.expanded_attempts.contains(&fold.attempt_id) {
            collapsed_attempts.extend(fold.hidden.iter().copied());
        }
    }
    let is_active_response = |run: &Option<String>| match run {
        None => runless_active,
        Some(run) => active_runs.contains(run),
    };
    let in_active_run = |work: &Work| input.is_working && work.status == Status::InProgress && match &work.run_id {
        None => runless_active,
        Some(run) => unsettled.as_ref() == Some(run),
    };
    let group_id = |index: usize| format!("work-group:{}", entries[index].id);
    let active_header = if input.is_working { last_boundary.map_or(0, |index| index + 1) } else { entries.len() };

    // The trailing work of the running turn becomes one live row.
    let mut active_tool: Vec<usize> = vec![];
    if input.is_working && (unsettled.is_some() || runless_active) {
        let mut tail_attempt: Option<Option<&str>> = None;
        for index in (active_header..entries.len()).rev() {
            let entry = &entries[index];
            let Some(work) = entry.work() else { break };
            if work.tone == Tone::Error || work.source_activity_kind == Some("runtime.error") || matches!(work.item_type.as_str(), "system_notice" | "notification") || !is_active_response(&work.run_id) || work.source_activity_kind == Some("context-compaction") || collapsed.contains(&index) || collapsed_attempts.contains(&index) || turn_folds.contains_key(&index) || attempt_folds.contains_key(&index) {
                break;
            }
            let attempt = entry.attempt.as_ref().map(|attempt| attempt.id.as_str());
            match tail_attempt {
                None => tail_attempt = Some(attempt),
                Some(tail) if tail != attempt => break,
                _ => {}
            }
            active_tool.insert(0, index);
        }
    }
    let visible_active: Vec<usize> = active_tool.iter().copied().filter(|&index| visible_in_group(entries[index].work().expect("work"), true)).collect();
    let work_of = |index: usize| entries[index].work().expect("work entry");
    let anchor = active_tool.first().copied();
    let latest_visible = visible_active.last().copied();
    let latest_running = visible_active.iter().rev().copied().find(|&index| {
        let work = work_of(index);
        work.status == Status::InProgress
    });
    let keeps_live = latest_running.is_some() || latest_visible.is_some_and(|index| work_of(index).indicates_success());
    let latest_failed = latest_running.is_none() && latest_visible.is_some_and(|index| work_of(index).display_failure());
    let thought = visible_active.iter().rev().copied().find(|&index| work_of(index).item_type == "reasoning" && work_of(index).detail.as_deref().is_some_and(|detail| !detail.trim().is_empty()));
    let placement = latest_visible;
    let live_row = match (anchor, latest_visible) {
        (Some(anchor), Some(latest)) if !latest_failed => {
            let shown = latest_running.unwrap_or(latest);
            let group = group_id(anchor);
            let (surface, icon) = group_look(&visible_active.iter().map(|&i| work_of(i)).collect::<Vec<_>>());
            Some(Row::Live {
                id: if keeps_live { "live-activity-row".into() } else { format!("work-live:{}", entries[anchor].id) },
                expanded: input.expanded_groups.contains(&group),
                group,
                entry: shown,
                entries: visible_active.clone(),
                failed: work_of(shown).display_failure(),
                label: live_label(work_of(shown), keeps_live),
                active: keeps_live,
                thought,
                surface,
                icon,
                continues: false,
            })
        }
        _ => None,
    };
    let active_ids: HashSet<usize> = if live_row.is_some() || latest_failed { active_tool.iter().copied().collect() } else { HashSet::new() };
    let mut has_activity = false;
    let mut has_compaction = false;
    let push_working = |rows: &mut Vec<Row>| rows.push(Row::Working { since: input.active_turn_started_at.map(str::to_owned) });
    let mut index = 0;
    while index < entries.len() {
        let entry = &entries[index];
        if input.is_working && index == active_header {
            push_working(&mut rows);
        }
        if Some(index) == placement {
            if let Some(Row::Live { group, entries: members, expanded, active, .. }) = &live_row {
                rows.push(live_row.clone().expect("live row"));
                has_activity |= *active;
                if *expanded {
                    rows.push(Row::Members { group: group.clone(), entries: members.clone() });
                }
            }
        }
        if matches!(&entry.kind, Kind::Event { item_type, .. } if item_type == "run_interrupt_request") {
            index += 1;
            continue;
        }
        if let Some(fold) = turn_folds.get(&index) {
            rows.push(Row::TurnFold { run_id: fold.run_id.clone(), label: fold.label.clone(), expanded: input.expanded_runs.contains(&fold.run_id), created_at: fold.created_at.clone() });
        }
        if collapsed.contains(&index) {
            index += 1;
            continue;
        }
        if let Some(fold) = attempt_folds.get(&index) {
            rows.push(Row::AttemptFold { run_id: fold.run_id.clone(), attempt_id: fold.attempt_id.clone(), label: "Superseded attempt".into(), expanded: input.expanded_attempts.contains(&fold.attempt_id), created_at: fold.created_at.clone() });
        }
        if collapsed_attempts.contains(&index) || active_ids.contains(&index) {
            index += 1;
            continue;
        }
        if let Some(work) = entry.work() {
            if work.source_activity_kind == Some("context-compaction") {
                let active = in_active_run(work);
                has_compaction |= active;
                rows.push(Row::Compaction { entry: index, label: work.label.clone(), active });
                index += 1;
                continue;
            }
            let standalone = |work: &Work| (work.run_id.as_ref().is_some_and(|run| failed.contains(run))) || (work.item_type == "error" && work.status == Status::Failed) || work.tone == Tone::Error || work.source_activity_kind == Some("runtime.error") || matches!(work.item_type.as_str(), "system_notice" | "notification");
            if standalone(work) {
                rows.push(Row::Entry { entry: index });
                index += 1;
                continue;
            }
            let mut grouped = vec![index];
            let mut cursor = index + 1;
            while cursor < entries.len() {
                let next = &entries[cursor];
                let Some(next_work) = next.work() else { break };
                if next_work.source_activity_kind == Some("context-compaction") || next_work.tone == Tone::Error || next_work.source_activity_kind == Some("runtime.error") || matches!(next_work.item_type.as_str(), "system_notice" | "notification") || active_ids.contains(&cursor) || collapsed.contains(&cursor) || collapsed_attempts.contains(&cursor) || turn_folds.contains_key(&cursor) || attempt_folds.contains_key(&cursor) || next_work.run_id != work.run_id || next.attempt.as_ref().map(|a| &a.id) != entry.attempt.as_ref().map(|a| &a.id) {
                    break;
                }
                grouped.push(cursor);
                cursor += 1;
            }
            let visible: Vec<usize> = grouped.iter().copied().filter(|&i| visible_in_group(work_of(i), in_active_run(work_of(i)))).collect();
            if !visible.is_empty() {
                let group = group_id(index);
                let expanded = input.expanded_groups.contains(&group);
                let active_in_progress: Vec<usize> = visible.iter().copied().filter(|&i| in_active_run(work_of(i))).collect();
                if let Some(&latest) = active_in_progress.last() {
                    let (surface, icon) = group_look(&visible.iter().map(|&i| work_of(i)).collect::<Vec<_>>());
                    rows.push(Row::Live { id: format!("work-live:{}", entry.id), group: group.clone(), entry: latest, entries: visible.clone(), label: live_label(work_of(latest), true), expanded, active: true, thought: None, surface, icon, failed: work_of(latest).display_failure(), continues: false });
                    has_activity = true;
                    if expanded {
                        rows.push(Row::Members { group, entries: visible });
                    }
                } else if visible.len() == 1 && work_of(visible[0]).is_tool_like() {
                    let only = work_of(visible[0]);
                    let label = if group_action(only) == GroupAction::Edit { summarize_group(&[only]).0 } else { single_call_label(only) };
                    rows.push(Row::Single { entry: visible[0], label, continues: false });
                } else {
                    let members: Vec<&Work> = visible.iter().map(|&i| work_of(i)).collect();
                    let only = if members.len() == 1 { Some(members[0]) } else { None };
                    let uses_single = only.is_some_and(|only| only.is_tool_like() && group_action(only) != GroupAction::Edit);
                    let summary = match only {
                        Some(only) if uses_single => single_call_label(only),
                        Some(only) if !only.is_tool_like() => only.label.clone(),
                        _ => summarize_group(&members).0,
                    };
                    let (surface, icon) = group_look(&members);
                    let latest_tool = members.iter().rev().find(|work| work.is_tool_like());
                    rows.push(Row::Toggle { id: format!("work-toggle:{}", entry.id), group: group.clone(), run_id: work.run_id.clone(), entries: visible.clone(), summary, kind: summary_kind(&members), surface, icon, has_failure: latest_tool.is_some_and(|work| work.display_failure()), expanded, continues: false });
                    if expanded {
                        rows.push(Row::Members { group, entries: visible });
                    }
                }
            }
            index = cursor;
            continue;
        }
        if let Kind::Event { item_type, run_id, provider_turn_id, .. } = &entry.kind {
            if item_type == "subagent" {
                if let Some(Row::Subagents { entries: batch }) = rows.last_mut() {
                    if let Kind::Event { run_id: previous_run, provider_turn_id: previous_turn, item_type: previous_type, .. } = &entries[*batch.last().expect("batch")].kind {
                        if previous_type == "subagent" && previous_run == run_id && previous_turn == provider_turn_id {
                            batch.push(index);
                            index += 1;
                            continue;
                        }
                    }
                }
                rows.push(Row::Subagents { entries: vec![index] });
                index += 1;
                continue;
            }
        }
        rows.push(Row::Entry { entry: index });
        // Created chats stay below the final answer even when the work that made them folds away.
        if let Kind::Message { role_user: false, run_id: Some(run), .. } = &entry.kind {
            if terminal.contains(&index) && !is_active_response(&Some(run.clone())) {
                for (created, candidate) in entries.iter().enumerate() {
                    if candidate.is_work(|work| work.item_type == "thread_created" && work.run_id.as_ref() == Some(run)) {
                        rows.push(Row::Created { entry: created });
                    }
                }
            }
        }
        index += 1;
    }
    let has_working = rows.iter().any(|row| matches!(row, Row::Working { .. }));
    if input.is_working && !has_working && active_header == entries.len() {
        push_working(&mut rows);
    }
    if input.is_working && !has_compaction && (!has_activity || latest_failed) {
        match anchor.filter(|_| latest_failed) {
            Some(anchor) => {
                let group = group_id(anchor);
                let expanded = input.expanded_groups.contains(&group);
                rows.push(Row::Thinking { group: Some(group.clone()), expanded, entries: visible_active.clone(), continues: false });
                if expanded {
                    rows.push(Row::Members { group, entries: visible_active.clone() });
                }
            }
            None => rows.push(Row::Thinking { group: None, expanded: false, entries: vec![], continues: false }),
        }
    }
    attach_trailing_tool_groups(&mut rows, entries, &terminal, &is_active_response_runs(&active_runs, runless_active));
    // Adjacent work stays one visual list, standalone notices and a group's members included.
    let is_log = |row: &Row| match row {
        Row::Single { .. } | Row::Toggle { .. } | Row::Live { .. } | Row::Thinking { .. } | Row::Members { .. } => true,
        Row::Entry { entry } => entries[*entry].work().is_some(),
        _ => false,
    };
    for index in 0..rows.len().saturating_sub(1) {
        if is_log(&rows[index]) && is_log(&rows[index + 1]) {
            match &mut rows[index] {
                Row::Single { continues, .. } | Row::Toggle { continues, .. } | Row::Live { continues, .. } | Row::Thinking { continues, .. } => *continues = true,
                _ => {}
            }
        }
    }
    rows
}

/// The surface and icon a group's summary line carries (`groupToolSurface`, `groupToolIcon`).
fn group_look(entries: &[&Work]) -> (Option<String>, Option<ToolIcon>) {
    let primary = entries.iter().find(|work| work.tool_source.is_some());
    let key = primary.and_then(|work| work.tool_source.as_ref()).map(|source| source.key.as_str());
    let source_icon = key.and_then(|key| entries.iter().find(|work| work.tool_source.as_ref().is_some_and(|source| source.key == key) && work.tool_icon.is_some()).and_then(|work| work.tool_icon.clone()));
    let surface = primary.and_then(|work| work.tool_surface.clone()).or_else(|| entries.iter().rev().find_map(|work| work.tool_surface.clone()));
    let icon = source_icon.or_else(|| entries.iter().rev().find_map(|work| work.tool_icon.clone()));
    (surface, icon)
}

/// A steer or later activity ends a thought even when the provider never says it finished.
fn settle_superseded_reasoning(entries: &[Entry]) -> std::borrow::Cow<'_, [Entry]> {
    let last = entries.len().saturating_sub(1);
    let stale = |index: usize, entry: &Entry| index != last && entry.is_work(|work| work.item_type == "reasoning" && work.status == Status::InProgress);
    if !entries.iter().enumerate().any(|(index, entry)| stale(index, entry)) {
        return std::borrow::Cow::Borrowed(entries);
    }
    std::borrow::Cow::Owned(entries.iter().enumerate().map(|(index, entry)| {
        let mut entry = entry.clone();
        if stale(index, &entry) {
            if let Kind::Work(work) = &mut entry.kind {
                work.status = Status::Completed;
            }
        }
        entry
    }).collect())
}

fn superseded_folds(entries: &[Entry], unfolded: &HashSet<String>, live_subagents: &HashSet<usize>) -> HashMap<usize, AttemptFold> {
    let mut order: Vec<String> = vec![];
    let mut by_attempt: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(attempt) = entry.attempt.as_ref().filter(|attempt| attempt.status == "superseded" && !unfolded.contains(&attempt.run_id)) else { continue };
        if matches!(&entry.kind, Kind::Message { role_user: true, .. }) || is_persistent_resource(entry) || live_subagents.contains(&index) || entry.is_work(|work| work.item_type == "system_notice") {
            continue;
        }
        if !by_attempt.contains_key(&attempt.id) {
            order.push(attempt.id.clone());
        }
        by_attempt.entry(attempt.id.clone()).or_default().push(index);
    }
    let mut folds = HashMap::new();
    for id in order {
        let members = &by_attempt[&id];
        let first = members[0];
        let attempt = entries[first].attempt.as_ref().expect("attempt");
        folds.insert(first, AttemptFold { run_id: attempt.run_id.clone(), attempt_id: id, created_at: entries[first].created_at.clone(), hidden: members.iter().copied().collect() });
    }
    folds
}

fn fold_run(entry: &Entry, runless: Option<&str>) -> Option<String> {
    if entry.is_work(|work| work.item_type == "system_notice") {
        return None;
    }
    match &entry.kind {
        Kind::Message { role_user: false, run_id, .. } => run_id.clone().or_else(|| runless.map(str::to_owned)),
        Kind::Work(work) => work.run_id.clone().or_else(|| runless.map(str::to_owned)),
        Kind::Event { item_type, run_id, .. } if is_persistent_resource(entry) || item_type == "subagent" => run_id.clone().or_else(|| runless.map(str::to_owned)),
        _ => None,
    }
}

fn turn_folds(entries: &[Entry], terminal: &HashSet<usize>, latest: Option<&LatestRun>, unfolded: &HashSet<String>, runless_active: bool, live_subagents: &HashSet<usize>) -> HashMap<usize, TurnFold> {
    let interrupted: HashSet<&str> = entries.iter().filter_map(|entry| match &entry.kind {
        Kind::Event { item_type, run_id: Some(run), .. } if item_type == "run_interrupt_request" || item_type == "run_interrupt_result" => Some(run.as_str()),
        _ => None,
    }).collect();
    struct Group {
        entries: Vec<usize>,
        terminal: Option<usize>,
        streaming: bool,
        start_boundary: Option<String>,
        anchor: usize,
    }
    let mut groups: Vec<(String, Group)> = vec![];
    let mut runless_failed: HashSet<String> = HashSet::new();
    let mut runless_key: Option<String> = None;
    let mut pending: Option<(String, usize)> = None;
    for (index, entry) in entries.iter().enumerate() {
        if starts_response(entry) {
            pending = (index + 1 < entries.len()).then(|| (entry.created_at.clone(), index + 1));
            let boundary_run = match &entry.kind {
                Kind::Work(work) => work.run_id.clone(),
                Kind::Message { run_id, .. } => run_id.clone(),
                _ => None,
            };
            runless_key = boundary_run.is_none().then(|| format!("runless:{}", entry.id));
            continue;
        }
        let Some(run) = fold_run(entry, runless_key.as_deref()) else { continue };
        if Some(&run) == runless_key.as_ref() && failed_item(entry) {
            runless_failed.insert(run.clone());
        }
        let position = match groups.iter().position(|(key, _)| *key == run) {
            Some(position) => position,
            None => {
                let (boundary, anchor) = pending.take().map_or((None, index), |(created, anchor)| (Some(created), anchor));
                groups.push((run.clone(), Group { entries: vec![], terminal: None, streaming: false, start_boundary: boundary, anchor }));
                groups.len() - 1
            }
        };
        let group = &mut groups[position].1;
        group.entries.push(index);
        if let Kind::Message { streaming, .. } = &entry.kind {
            if terminal.contains(&index) {
                group.terminal = Some(index);
            }
            if *streaming {
                group.streaming = true;
            }
        }
    }
    let mut folds = HashMap::new();
    for (run, group) in groups {
        if unfolded.contains(&run) || interrupted.contains(run.as_str()) || runless_failed.contains(&run) || (runless_active && Some(&run) == runless_key.as_ref()) || group.streaming {
            continue;
        }
        let terminal_position = group.terminal.and_then(|terminal| group.entries.iter().position(|&index| index == terminal)).unwrap_or(group.entries.len());
        let mut hidden = HashSet::new();
        for (position, &index) in group.entries.iter().enumerate() {
            if Some(index) == group.terminal {
                continue;
            }
            let entry = &entries[index];
            let compaction = entry.is_work(|work| work.source_activity_kind == Some("context-compaction"));
            let foldable_trailing = entry.is_work(|work| work.status != Status::InProgress && !work.display_failure());
            if !compaction && position > terminal_position && !foldable_trailing {
                continue;
            }
            if is_persistent_resource(entry) || live_subagents.contains(&index) || entry.is_work(|work| work.item_type == "notification") {
                continue;
            }
            hidden.insert(index);
        }
        if hidden.is_empty() {
            continue;
        }
        if !group.entries.iter().any(|index| hidden.contains(index) && !entries[*index].is_work(|work| work.source_activity_kind == Some("context-compaction"))) {
            continue;
        }
        let (Some(&first), Some(&last_index)) = (group.entries.first(), group.entries.last()) else { continue };
        let last = &entries[last_index];
        let interrupted_latest = latest.is_some_and(|latest| latest.id == run && latest.status == "interrupted");
        let last_end = if let Kind::Message { updated_at, .. } = &last.kind { updated_at.clone() } else { last.created_at.clone() };
        let terminal_updated = group.terminal.and_then(|terminal| if let Kind::Message { updated_at, .. } = &entries[terminal].kind { Some(updated_at.as_str()) } else { None });
        let duration_ms = match latest.filter(|latest| latest.id == run && latest.started_at.is_some() && latest.completed_at.is_some()) {
            Some(latest) => elapsed(latest.started_at.as_deref().unwrap_or_default(), latest.completed_at.as_deref().unwrap_or_default()),
            None => elapsed(group.start_boundary.as_deref().unwrap_or(&entries[first].created_at), &max_iso(terminal_updated, Some(&last_end)).unwrap_or_else(|| last_end.clone())),
        };
        let duration = duration_ms.map(format_duration);
        let label = match (interrupted_latest, duration) {
            (true, Some(duration)) => format!("You stopped after {duration}"),
            (true, None) => "You stopped this response".into(),
            (false, Some(duration)) => format!("Worked for {duration}"),
            (false, None) => "Worked".into(),
        };
        folds.insert(group.anchor, TurnFold { run_id: run, created_at: group.start_boundary.clone().unwrap_or_else(|| entries[first].created_at.clone()), hidden, label });
    }
    folds
}
fn failed_item(entry: &Entry) -> bool {
    entry.is_work(|work| work.failed_top_level_error())
}

impl GroupAction {
    pub fn as_str(self) -> &'static str {
        match self {
            GroupAction::Read => "read",
            GroupAction::Edit => "edit",
            GroupAction::Command => "command",
            GroupAction::ThreadCreate => "thread-create",
            GroupAction::Browser => "browser",
            GroupAction::Device => "device",
            GroupAction::CodeSearch => "code-search",
            GroupAction::Search => "search",
            GroupAction::Other => "other",
            GroupAction::Update => "update",
            GroupAction::LinkPr => "link-pr",
            GroupAction::UnlinkPr => "unlink-pr",
            GroupAction::WatchPr => "watch-pr",
            GroupAction::UnwatchPr => "unwatch-pr",
            GroupAction::ListPrs => "list-prs",
        }
    }
}
impl SummaryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SummaryKind::PullRequest => "pull-request",
            SummaryKind::Action(action) => action.as_str(),
            SummaryKind::Reasoning => "reasoning",
            SummaryKind::DynamicTool => "dynamic-tool",
            SummaryKind::AgentTool => "agent-tool",
            SummaryKind::ToneTool => "tone-tool",
            SummaryKind::Other => "other",
            SummaryKind::Mixed => "mixed",
        }
    }
}

/// Puts the rows the derivation leaves out back where they happened: before the first row that
/// comes from a later item, and before the fold line that leads it.
pub fn place_left_out(rows: &mut Vec<Row>, entries: &[Entry], items: &[Value]) {
    let key = |row: &Row| -> Option<usize> {
        match row {
            Row::Entry { entry } | Row::Single { entry, .. } | Row::Compaction { entry, .. } => Some(entries[*entry].row),
            Row::Toggle { entries: members, .. } | Row::Live { entries: members, .. } | Row::Members { entries: members, .. } | Row::Subagents { entries: members } => members.iter().map(|&entry| entries[entry].row).min(),
            Row::Item { row } => Some(*row),
            _ => None,
        }
    };
    for (row, item) in items.iter().enumerate() {
        if !matches!(item["item"]["type"].as_str(), Some("todo_list" | "checkpoint")) {
            continue;
        }
        let mut at = rows.iter().position(|candidate| key(candidate).is_some_and(|key| key > row)).unwrap_or(rows.len());
        while at > 0 && matches!(rows[at - 1], Row::TurnFold { .. } | Row::AttemptFold { .. } | Row::Working { .. } | Row::Thinking { .. }) {
            at -= 1;
        }
        rows.insert(at, Row::Item { row });
    }
}

fn is_active_response_runs(active: &HashSet<String>, runless: bool) -> impl Fn(&Option<String>) -> bool + '_ {
    move |run| match run {
        None => runless,
        Some(run) => active.contains(run),
    }
}

/// The footer of each settled response's final answer (`showAssistantMeta`): right under it, or,
/// when the turn ends with tool calls after its final text, below the tools so text and tools are
/// one visual response (`attachTrailingToolGroupsToAssistant`).
fn attach_trailing_tool_groups(rows: &mut Vec<Row>, entries: &[Entry], terminal: &HashSet<usize>, active: &impl Fn(&Option<String>) -> bool) {
    let mut footers: Vec<(usize, usize)> = vec![];
    for (position, row) in rows.iter().enumerate() {
        let Row::Entry { entry: message } = row else { continue };
        let Kind::Message { role_user: false, run_id, .. } = &entries[*message].kind else { continue };
        if !terminal.contains(message) || active(run_id) {
            continue;
        }
        // A response without a run keeps its footer right under it.
        let Some(run) = run_id else {
            footers.push((position, *message));
            continue;
        };
        let (mut last, mut has_group) = (None, false);
        for (index, candidate) in rows.iter().enumerate().skip(position + 1) {
            match candidate {
                Row::Entry { entry } if matches!(entries[*entry].kind, Kind::Message { .. }) => break,
                Row::Toggle { run_id, .. } if run_id.as_ref() == Some(run) => {
                    has_group = true;
                    last = Some(index);
                }
                Row::Created { entry } if entries[*entry].work().is_some_and(|work| work.run_id.as_ref() == Some(run)) => {
                    has_group = true;
                    last = Some(index);
                }
                Row::Single { entry, .. } | Row::Entry { entry } if entries[*entry].work().is_some_and(|work| work.run_id.as_ref() == Some(run)) => {
                    let work = entries[*entry].work().expect("work");
                    if work.is_tool_like() || work.failed_top_level_error() || work.item_type == "error" && work.status == Status::Failed {
                        has_group = true;
                    }
                    if has_group {
                        last = Some(index);
                    }
                }
                Row::Members { entries: members, .. } if members.iter().any(|&member| entries[member].work().is_some_and(|work| work.run_id.as_ref() == Some(run))) => {
                    if has_group {
                        last = Some(index);
                    }
                }
                Row::Entry { .. } | Row::Single { .. } | Row::Toggle { .. } | Row::Created { .. } | Row::Members { .. } | Row::Live { .. } | Row::Thinking { .. } | Row::Working { .. } | Row::TurnFold { .. } | Row::AttemptFold { .. } | Row::Compaction { .. } | Row::Subagents { .. } | Row::Item { .. } | Row::Footer { .. } => {}
            }
        }
        footers.push((last.unwrap_or(position), *message));
    }
    for (offset, (after, message)) in footers.into_iter().enumerate() {
        rows.insert(after + 1 + offset, Row::Footer { message });
    }
}
