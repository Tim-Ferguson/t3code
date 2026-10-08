//! ACP tool-state parsing, retained output and source presentation rules.
use serde_json::{Value, json};
use std::sync::Arc;

const OUTPUT_LIMIT: usize = 8_000;
const OUTPUT_MARKER: &str = "[Earlier output truncated]\n\n";
const OUTPUT_FIELDS: [&str; 4] = ["content", "stdout", "stderr", "output"];
fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}
/// Source identity v2 scopes fresh ACP items, retaining legacy loaded identities.
pub(crate) fn provider_item_ids(
    instance: &str,
    native: &str,
    version_two: bool,
) -> (String, String) {
    let scoped = if version_two {
        format!("provider-instance:{}:{native}", encode_component(instance))
    } else {
        native.to_owned()
    };
    let encoded = encode_component(&scoped);
    (
        format!("turn-item:provider:acpRegistry:native-item:{encoded}"),
        format!("node:provider:acpRegistry:native-item:{encoded}"),
    )
}
pub(crate) fn structured_changes(data: &Value) -> Vec<Value> {
    data["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["type"] == "diff")
        .flat_map(|entry| entry["changes"].as_array().into_iter().flatten())
        .filter_map(|entry| {
            let operation = trimmed(&entry["operation"])?;
            let path = trimmed(&entry["path"])?;
            let mut result = json!({"operation":operation,"path":path});
            for field in ["oldPath", "fileType", "mimeType"] {
                if let Some(value) = trimmed(&entry[field]) {
                    result[field] = json!(value);
                }
            }
            Some(result)
        })
        .collect()
}
pub(crate) fn backend_search(input: &Value, output: &Value) -> Option<Value> {
    let variant = input["variant"].as_str().unwrap_or("").to_lowercase();
    let action = &output["action"];
    if variant != "xsearch" && variant != "websearch" && action["type"] != "search" {
        return None;
    }
    let args = output["input"]
        .as_str()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .filter(Value::is_object)
        .unwrap_or(Value::Null);
    let args_text = args
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| match value {
            Value::String(value) => Some(format!("{key}: {value}")),
            Value::Number(value) => Some(format!("{key}: {}", t3_acp::js_number(value))),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(", ");
    let query = trimmed(&action["query"])
        .or_else(|| trimmed(&args["query"]))
        .or_else(|| {
            let value = t3_contracts::trim_wire_string(&args_text);
            (!value.is_empty()).then_some(value)
        });
    let mut urls = Vec::new();
    for source in action["sources"].as_array().into_iter().flatten() {
        if let Some(url) = trimmed(&source["url"]) {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    let mut result =
        json!({"results":urls.iter().map(|url|json!({"url":url})).collect::<Vec<_>>()});
    if let Some(query) = query {
        result["query"] = json!(query);
    }
    Some(result)
}
pub(crate) fn backend_title(data_title: &Value, title: &Value, query: Option<&str>) -> String {
    let label = trimmed(data_title).unwrap_or_else(|| title.as_str().unwrap_or("Web search"));
    let before_whitespace = label.trim_end_matches(|character: char| {
        t3_contracts::trim_wire_string(&character.to_string()).is_empty()
    });
    let label = before_whitespace.strip_suffix(':').unwrap_or(label);
    query
        .map(|query| format!("{label}: {query}"))
        .unwrap_or_else(|| label.to_owned())
}
pub(crate) fn monitor_command(input: &Value, output: &Value) -> (bool, Option<String>) {
    let variant = input["variant"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let command = trimmed(&input["command"])
        .or_else(|| trimmed(&output["command"]))
        .map(ToOwned::to_owned);
    let bash = output["type"]
        .as_str()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("bash")
        && ["exitCode", "exit_code", "code"].iter().any(|key| {
            output[*key]
                .as_f64()
                .is_some_and(|value| value.fract() == 0.0)
        });
    (variant == "monitor" || bash, command)
}
fn trimmed(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(t3_contracts::trim_wire_string)
        .filter(|value| !value.is_empty())
}
fn slice(value: &str, start: usize, end: usize) -> String {
    // Current UTF8 strings replace a lone surrogate created by JavaScript's
    // retained-tail slice. Unicode witnesses keep that wire discrepancy explicit;
    // display equivalence is not a claim of full JSON string parity.
    String::from_utf16_lossy(
        &value
            .encode_utf16()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect::<Vec<_>>(),
    )
}
fn length(value: &str) -> usize {
    value.encode_utf16().count()
}
fn bounded_output(value: &str) -> String {
    let size = length(value);
    if size <= OUTPUT_LIMIT {
        value.into()
    } else {
        format!("{OUTPUT_MARKER}{}", slice(value, size - OUTPUT_LIMIT, size))
    }
}
pub fn bound_raw_output(value: &Value) -> Value {
    let mut bounded = value.clone();
    if let Some(object) = bounded.as_object_mut() {
        for field in OUTPUT_FIELDS {
            if let Some(value) = object
                .get(field)
                .and_then(Value::as_str)
                .filter(|value| length(value) > OUTPUT_LIMIT)
            {
                object.insert(field.into(), json!(bounded_output(value)));
            }
        }
    }
    bounded
}
fn content_text(entry: &Value) -> Option<&str> {
    (entry["type"] == "content" && entry["content"]["type"] == "text")
        .then(|| entry["content"]["text"].as_str())
        .flatten()
}
fn byte_text(value: &Value) -> Option<String> {
    let array = value.as_array().filter(|array| !array.is_empty())?;
    let bytes = array
        .iter()
        .map(|value| {
            let value = value
                .as_f64()
                .filter(|value| value.is_finite() && value.fract() == 0.0)?;
            Some(value.rem_euclid(256.0) as u8)
        })
        .collect::<Option<Vec<_>>>()?;
    let decoded = String::from_utf8_lossy(&bytes);
    let decoded = decoded.strip_prefix('\u{FEFF}').unwrap_or(&decoded);
    (!decoded.is_empty()).then(|| decoded.to_owned())
}
pub fn output_text(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.into());
    }
    if let Some(value) = byte_text(value) {
        return Some(value);
    }
    if let Some(array) = value.as_array() {
        let parts = array
            .iter()
            .filter_map(output_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>();
        return (!parts.is_empty()).then(|| parts.join("\n"));
    }
    if !value.is_object() {
        return None;
    }
    for key in [
        "output_for_prompt",
        "stdout",
        "stderr",
        "output",
        "combinedOutput",
        "combined_output",
        "content",
        "text",
        "message",
    ] {
        if let Some(text) = output_text(&value[key]).filter(|text| !text.is_empty()) {
            return Some(text);
        }
    }
    value
        .get("Result")
        .filter(|value| value.is_object())
        .or_else(|| value.get("result").filter(|value| value.is_object()))
        .and_then(output_text)
}
pub fn projected_exit_code(status: &str, output: &Value) -> Option<Value> {
    if !matches!(status, "completed" | "failed") {
        return None;
    }
    ["exitCode", "exit_code", "code"]
        .into_iter()
        .find_map(|key| {
            output
                .get(key)
                .filter(|value| {
                    value
                        .as_f64()
                        .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
                })
                .cloned()
        })
}
pub fn read_input(input: &Value, path: Option<&str>) -> Value {
    let mut input = if input.is_object() {
        input.clone()
    } else {
        json!({})
    };
    if let Some(path) = path {
        if !["path", "filePath", "file_path"]
            .iter()
            .any(|key| input[key] == path)
        {
            input["path"] = json!(path);
        }
    }
    input
}
/// Keep the retained cumulative tail on its original entries. Diff/image entries
/// retain their order; the marker appears once, on the first retained text entry.
pub fn extracted_content(content: &Value) -> (Option<String>, Option<Vec<Value>>) {
    let Some(entries) = content.as_array() else {
        return (None, None);
    };
    let chunks = entries
        .iter()
        .filter_map(content_text)
        .map(t3_contracts::trim_wire_string)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    let joined = chunks.join("\n");
    if length(&joined) <= OUTPUT_LIMIT {
        let entries=entries.iter().map(|entry| match content_text(entry).filter(|text|length(text)>OUTPUT_LIMIT) {
            Some(text)=>{let trimmed=t3_contracts::trim_wire_string(text);json!({"type":"content","content":{"type":"text","text":bounded_output(if trimmed.is_empty(){text}else{trimmed})}})},
            None=>entry.clone(),
        }).collect();
        return ((!joined.is_empty()).then_some(joined), Some(entries));
    }
    let mut offset = 0;
    let mut seen_text = false;
    let ranges = entries
        .iter()
        .map(|entry| {
            let value = content_text(entry)
                .map(t3_contracts::trim_wire_string)
                .filter(|text| !text.is_empty())?;
            if seen_text {
                offset += 1;
            }
            seen_text = true;
            let start = offset;
            offset += length(value);
            Some((start, offset, value))
        })
        .collect::<Vec<_>>();
    let tail_start = offset - OUTPUT_LIMIT;
    let mut marker_pending = true;
    let retained = entries
        .iter()
        .zip(ranges)
        .filter_map(|(entry, range)| {
            if content_text(entry).is_none() {
                return Some(entry.clone());
            }
            let (start, end, value) = range?;
            let overlap = start.max(tail_start);
            if end <= overlap {
                return None;
            }
            let mut piece = slice(value, overlap - start, end - start);
            if marker_pending {
                piece = format!("{OUTPUT_MARKER}{piece}");
                marker_pending = false;
            }
            Some(json!({"type":"content","content":{"type":"text","text":piece}}))
        })
        .collect();
    (Some(bounded_output(&joined)), Some(retained))
}
pub fn sanitize_content(content: &[Value]) -> Vec<Value> {
    content.iter().map(|entry|match entry["type"].as_str() {
        Some("content")=>json!({"type":"content","content":{"type":"text","text":crate::acp_model::content_display_text(&entry["content"]).unwrap_or_default()}}),
        Some("terminal")=>json!({"type":"terminal","terminalId":entry["terminalId"]}),
        Some("diff") if entry.get("changes").is_some()=>{let mut value=json!({"type":"diff","changes":entry["changes"]});if let Some(patch)=entry.get("patch"){value["patch"]=patch.clone();}value},
        Some("diff")=>{let mut value=json!({"type":"diff","path":entry["path"],"newText":entry["newText"]});if let Some(old)=entry.get("oldText"){value["oldText"]=old.clone();}value},
        Some("_t3_unknown")=>{let value=trimmed(&entry["originalType"]).unwrap_or("unknown");json!({"type":"_t3_unknown","originalType":slice(value,0,128),"raw":null})},
        _=>entry.clone(),
    }).collect()
}
fn command_value(value: &Value) -> Option<String> {
    if let Some(value) = trimmed(value) {
        return Some(value.into());
    }
    let values = value
        .as_array()?
        .iter()
        .filter_map(trimmed)
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(" "))
}
fn backtick_command(title: &str) -> Option<String> {
    let (_, tail) = title.split_once('`')?;
    let (value, _) = tail.split_once('`')?;
    let value = t3_contracts::trim_wire_string(value);
    (!value.is_empty()).then(|| value.into())
}
pub fn extract_command(input: &Value, title: Option<&str>, kind: Option<&str>) -> Option<String> {
    if input.is_object() {
        if let Some(command) = command_value(&input["command"]) {
            return Some(command);
        }
        if let Some(executable) = trimmed(&input["executable"]) {
            return Some(match command_value(&input["args"]) {
                Some(args) => format!("{executable} {args}"),
                None => executable.into(),
            });
        }
    }
    (kind == Some("execute"))
        .then(|| title.and_then(backtick_command))
        .flatten()
}
const PATH_KEYS: [&str; 8] = [
    "path",
    "filePath",
    "file_path",
    "relativePath",
    "filename",
    "fileName",
    "newPath",
    "oldPath",
];
const NESTED_KEYS: [&str; 7] = [
    "locations",
    "item",
    "input",
    "result",
    "rawInput",
    "data",
    "changes",
];
pub fn file_paths(data: &Value) -> Vec<String> {
    fn collect(value: &Value, paths: &mut Vec<String>, depth: usize) {
        if depth > 4 || paths.len() >= 8 {
            return;
        }
        if let Some(array) = value.as_array() {
            for value in array {
                collect(value, paths, depth + 1);
                if paths.len() >= 8 {
                    break;
                }
            }
            return;
        }
        if !value.is_object() {
            return;
        }
        for key in PATH_KEYS {
            if let Some(path) = trimmed(&value[key]) {
                if !paths.iter().any(|value| value == path) {
                    paths.push(path.into());
                    if paths.len() >= 8 {
                        return;
                    }
                }
            }
        }
        for key in NESTED_KEYS {
            if let Some(value) = value.get(key) {
                collect(value, paths, depth + 1);
                if paths.len() >= 8 {
                    return;
                }
            }
        }
    }
    let mut paths = vec![];
    collect(data, &mut paths, 0);
    paths
}
fn locations(update: &Value) -> Option<Vec<Value>> {
    if update["locations"].is_null() && update.get("locations").is_some()
        || update["locations"].as_array().is_some_and(Vec::is_empty)
    {
        return Some(vec![]);
    }
    let mut locations = vec![];
    let mut push = |value: Value| {
        if let Some(path) = trimmed(&value["path"]) {
            if !locations
                .iter()
                .any(|location: &Value| location["path"] == path)
            {
                let mut value = value.clone();
                value["path"] = json!(path);
                locations.push(value);
            }
        }
    };
    for location in update["locations"].as_array().into_iter().flatten() {
        push(location.clone());
    }
    for entry in update["content"].as_array().into_iter().flatten() {
        if entry["type"] == "diff" && entry.get("path").is_some() {
            push(json!({"path":entry["path"]}));
        }
    }
    for field in ["rawInput", "rawOutput"] {
        for path in ["path", "filePath", "file_path"] {
            push(json!({"path":update[field][path]}));
        }
    }
    (!locations.is_empty()).then_some(locations)
}
pub fn merge_data(previous: &Value, next: &Value) -> Value {
    if !next.is_object() {
        return previous.clone();
    }
    if !previous.is_object() {
        return next.clone();
    }
    let mut merged = previous.clone();
    for (key, value) in next.as_object().unwrap() {
        merged[key] = value.clone();
    }
    let previous_input = previous["rawInput"].as_object();
    let next_input = next["rawInput"].as_object();
    let input = if next_input.is_some_and(|value| !value.is_empty()) {
        let mut input = previous_input.cloned().unwrap_or_default();
        input.extend(next_input.unwrap().clone());
        input
    } else {
        previous_input.or(next_input).cloned().unwrap_or_default()
    };
    if input.is_empty() {
        merged.as_object_mut().unwrap().remove("rawInput");
    } else {
        merged["rawInput"] = json!(input);
    }
    merged
}

/// Object identity matters to the source output-coalescing predicate. Omitted
/// partial-update fields share their prior Arc; new object payloads do not.
#[derive(Clone)]
pub struct ToolState {
    pub wire: Value,
    content: Option<Arc<Value>>,
    raw_output: Option<Arc<Value>>,
}
impl ToolState {
    pub fn parse(update: &Value, fallback_pending: bool) -> Option<Self> {
        let id = trimmed(&update["toolCallId"])?;
        let title = trimmed(&update["title"]);
        let kind = trimmed(&update["kind"]);
        let command = extract_command(&update["rawInput"], title, kind);
        let (text_content, content) = extracted_content(&update["content"]);
        let normalized_title = title
            .filter(|title| !matches!(title.to_lowercase().as_str(), "terminal" | "tool call"));
        let mut data = json!({"toolCallId":id});
        if let Some(kind) = kind {
            data["kind"] = json!(kind);
        }
        if let Some(title) = title {
            data["title"] = json!(title);
        }
        if let Some(command) = &command {
            data["command"] = json!(command);
        }
        if let Some(input) = update.get("rawInput") {
            data["rawInput"] = input.clone();
        }
        if let Some(output) = update.get("rawOutput") {
            data["rawOutput"] = bound_raw_output(output);
        }
        if update["_meta"].is_object() {
            data["meta"] = update["_meta"].clone();
        }
        if let Some(content) = content {
            data["content"] = json!(sanitize_content(&content));
        }
        if let Some(locations) = locations(update) {
            data["locations"] = json!(locations);
        }
        let mut wire = json!({"toolCallId":id,"data":data});
        if let Some(kind) = kind {
            wire["kind"] = json!(kind);
        }
        let status = match update["status"].as_str() {
            Some("pending") => Some("pending"),
            Some("in_progress" | "inProgress") => Some("inProgress"),
            Some("completed") => Some("completed"),
            Some("failed") => Some("failed"),
            _ => fallback_pending.then_some("pending"),
        };
        if let Some(status) = status {
            wire["status"] = json!(status);
        }
        if title.is_some() || kind.is_some() || command.is_some() || text_content.is_some() {
            let detail = command
                .as_deref()
                .or(normalized_title)
                .or(text_content.as_deref());
            let presentation = presentation(
                crate::acp_model::canonical_item_type(kind.unwrap_or("")),
                title,
                detail,
                &wire["data"],
                title.unwrap_or("Tool"),
            );
            wire["title"] = presentation["summary"].clone();
            if let Some(detail) = presentation.get("detail") {
                wire["detail"] = detail.clone();
            }
        }
        if let Some(command) = command {
            wire["command"] = json!(command);
        }
        let content = wire["data"]
            .get("content")
            .map(|value| Arc::new(value.clone()));
        let raw_output = wire["data"]
            .get("rawOutput")
            .map(|value| Arc::new(value.clone()));
        Some(Self {
            wire,
            content,
            raw_output,
        })
    }
    pub fn merge(previous: Option<&Self>, next: Self) -> Self {
        let Some(previous) = previous else {
            return next;
        };
        let mut wire = next.wire.clone();
        for field in ["kind", "title", "status", "command", "detail"] {
            if wire.get(field).is_none() {
                if let Some(value) = previous.wire.get(field) {
                    wire[field] = value.clone();
                }
            }
        }
        wire["data"] = merge_data(&previous.wire["data"], &next.wire["data"]);
        Self {
            wire,
            content: next.content.or_else(|| previous.content.clone()),
            raw_output: next.raw_output.or_else(|| previous.raw_output.clone()),
        }
    }
    pub fn progress_length(&self) -> usize {
        let content = self.wire["data"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(content_text)
            .map(length)
            .sum::<usize>();
        let output = OUTPUT_FIELDS
            .iter()
            .filter_map(|field| self.wire["data"]["rawOutput"][field].as_str())
            .map(length)
            .sum::<usize>();
        length(self.wire["detail"].as_str().unwrap_or(""))
            .max(content)
            .max(output)
    }
    pub fn emission(
        previous: Option<&Self>,
        next: &Self,
        last_length: Option<usize>,
        skipped: usize,
    ) -> (bool, usize) {
        if matches!(next.wire["status"].as_str(), Some("completed" | "failed")) {
            return (true, 0);
        }
        let Some(previous) = previous else {
            return (true, 0);
        };
        if previous.wire["title"] != next.wire["title"]
            || previous.wire["status"] != next.wire["status"]
        {
            return (true, 0);
        }
        let identical = |left: &Option<Arc<Value>>, right: &Option<Arc<Value>>| match (left, right)
        {
            (None, None) => true,
            (Some(left), Some(right)) => {
                if left.is_array() || left.is_object() {
                    Arc::ptr_eq(left, right)
                } else {
                    left == right
                }
            }
            _ => false,
        };
        if previous.wire["detail"] == next.wire["detail"]
            && identical(&previous.content, &next.content)
            && identical(&previous.raw_output, &next.raw_output)
        {
            return (false, skipped);
        }
        if last_length.is_none_or(|length| next.progress_length().abs_diff(length) >= 256)
            || skipped + 1 >= 10
        {
            (true, 0)
        } else {
            (false, skipped + 1)
        }
    }
}

fn first<'a>(input: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| trimmed(&input[key]))
}
pub fn search_label(data: &Value) -> Option<String> {
    let input = [&data["rawInput"], &data["input"], &data["item"]["input"]]
        .into_iter()
        .find(|input| input.as_object().is_some_and(|object| !object.is_empty()))
        .unwrap_or(data);
    let query = first(
        input,
        &["pattern", "query", "searchTerm", "regex", "grep", "needle"],
    );
    let glob = first(
        input,
        &[
            "glob",
            "globPattern",
            "glob_pattern",
            "include",
            "filePattern",
            "file_pattern",
        ],
    );
    let target = first(
        input,
        &[
            "path",
            "target_directory",
            "targetDirectory",
            "directory",
            "cwd",
            "root",
        ],
    )
    .and_then(|path| {
        path.split(['\\', '/'])
            .rev()
            .find(|part| !part.is_empty() && *part != ".")
    });
    Some(match (query, glob, target) {
        (Some(query), _, Some(target)) => format!("Searched {query} in {target}"),
        (_, Some(glob), Some(target)) => format!("Searched files {glob} in {target}"),
        (_, Some(glob), _) => format!("Searched files {glob}"),
        (Some(query), _, _) => format!("Searched {query}"),
        (_, _, Some(target)) => format!("Searched in {target}"),
        _ => return None,
    })
}
fn equivalent(left: &str, right: &str) -> bool {
    let normalize = |value: &str| {
        let pieces = value
            .split(|c: char| t3_contracts::trim_wire_string(&c.to_string()).is_empty())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        let mut value = pieces.join(" ").to_lowercase();
        for suffix in [" complete", " completed", " started"] {
            if value.ends_with(suffix) {
                value.truncate(value.len() - suffix.len());
                break;
            }
        }
        value
    };
    !left.is_empty() && normalize(left) == normalize(right)
}
pub fn presentation(
    item_type: &str,
    title: Option<&str>,
    detail: Option<&str>,
    data: &Value,
    fallback: &str,
) -> Value {
    let trim = |value: Option<&str>| {
        value
            .map(t3_contracts::trim_wire_string)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    let title = trim(title);
    let mut detail = trim(detail);
    let fallback = trim(Some(fallback)).unwrap_or_else(|| "Tool".into());
    if let Some(value) = &detail {
        if value.ends_with('>') {
            if let Some(start) = value.rfind('<') {
                let suffix = &value[start..];
                if suffix.len() > 24
                    && suffix.get(..23).is_some_and(|prefix| {
                        prefix.eq_ignore_ascii_case("<exited with exit code ")
                    })
                    && suffix[23..suffix.len() - 1]
                        .bytes()
                        .all(|byte| byte.is_ascii_digit())
                {
                    detail = trim(Some(&value[..start]));
                }
            }
        }
    }
    let direct = [
        &data["item"]["command"],
        &data["item"]["input"]["command"],
        &data["item"]["result"]["command"],
        &data["command"],
        &data["rawInput"]["command"],
    ]
    .into_iter()
    .find_map(command_value);
    let command =
        direct.or_else(|| extract_command(&data["rawInput"], title.as_deref(), Some("execute")));
    let paths = file_paths(data);
    let path = paths.first();
    let kind = trimmed(&data["kind"]).unwrap_or("").to_lowercase();
    let name = trimmed(&data["toolName"])
        .or_else(|| trimmed(&data["item"]["tool"]))
        .filter(|name| !name.contains("__") && !name.contains(['.', '/']))
        .unwrap_or("")
        .chars()
        .filter(|character| {
            *character != '_'
                && *character != '-'
                && !t3_contracts::trim_wire_string(&character.to_string()).is_empty()
        })
        .collect::<String>()
        .to_lowercase();
    let action = if item_type == "command_execution" {
        "command"
    } else if item_type == "image_view" {
        "read"
    } else if item_type == "file_change" {
        "change"
    } else if item_type == "web_search" {
        "search"
    } else if kind == "execute" {
        "command"
    } else if matches!(kind.as_str(), "edit" | "move" | "delete" | "write") {
        "change"
    } else if kind == "search" || matches!(name.as_str(), "find" | "grep" | "glob" | "rg" | "ls") {
        "search"
    } else if kind == "read" {
        "read"
    } else if matches!(name.as_str(), "terminal" | "bash" | "shell") {
        "command"
    } else if matches!(name.as_str(), "read" | "readfile") {
        "read"
    } else {
        "other"
    };
    match action {
        "command" => {
            let mut value = json!({"summary":"Ran command"});
            if let Some(command) = command {
                value["detail"] = json!(command);
            }
            value
        }
        "read" => {
            json!({"summary":path.map(|path|format!("Read {path}")).unwrap_or_else(||"Read file".into())})
        }
        "change" => {
            let mut value = json!({"summary":"Changed files"});
            if let Some(path) = path {
                value["detail"] = json!(path);
            }
            value
        }
        "search" => json!({"summary":search_label(data).unwrap_or_else(||"Searched files".into())}),
        _ => {
            let mut value = json!({"summary":title.as_deref().unwrap_or(&fallback)});
            if let Some(detail) = detail.filter(|detail| {
                !equivalent(detail, title.as_deref().unwrap_or(""))
                    && !equivalent(detail, &fallback)
            }) {
                value["detail"] = json!(detail);
            }
            value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_astral_boundaries_preserve_display_and_document_lone_surrogate_wire_gap() {
        let mut gaps = 0;
        for line in include_str!("../tests/fixtures/acp-tool-unicode.jsonl").lines() {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let actual = ToolState::parse(&fixture["input"], true).unwrap().wire;
            assert_eq!(actual, fixture["displayState"]);
            let units = actual["data"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    entry["content"]["text"]
                        .as_str()
                        .unwrap()
                        .encode_utf16()
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            if fixture["wirePreserved"] == true {
                assert_eq!(json!(units), fixture["nativeUnits"]);
            } else {
                assert_ne!(json!(units), fixture["nativeUnits"]);
                gaps += 1;
            }
        }
        assert_eq!(
            gaps, 2,
            "Known scalar-string limits must stay visible rather than be counted as wire parity"
        );
    }
    #[test]
    fn original_tool_state_merging_retained_output_presentation_and_emission() {
        for (index, line) in include_str!("../tests/fixtures/acp-tools.jsonl")
            .lines()
            .enumerate()
        {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let input = &fixture["input"];
            let actual = match fixture["operation"].as_str().unwrap() {
                "parse" => json!(
                    ToolState::parse(input, input["sessionUpdate"] == "tool_call")
                        .map(|state| state.wire)
                ),
                "sequence" => {
                    let mut previous = None;
                    let mut last_length = None;
                    let mut skipped = 0;
                    json!(input.as_array().unwrap().iter().map(|update|{
                        let next=ToolState::merge(previous.as_ref(),ToolState::parse(update,update["sessionUpdate"]=="tool_call").unwrap());
                        let (emit,next_skipped)=ToolState::emission(previous.as_ref(),&next,last_length,skipped);
                        skipped=next_skipped;let progress=next.progress_length();if emit{last_length=Some(progress);}
                        let result=json!({"state":next.wire,"decision":{"emit":emit,"skippedSinceEmit":skipped},"progress":progress});previous=Some(next);result
                    }).collect::<Vec<_>>())
                }
                "backendTitle" => json!(backend_title(
                    &input["dataTitle"],
                    &input["title"],
                    input["query"].as_str()
                )),
                "changes" => json!(structured_changes(input)),
                "backendSearch" => json!(backend_search(&input["input"], &input["output"])),
                "monitor" => {
                    let (project, command) = monitor_command(&input["input"], &input["output"]);
                    json!({"project":project,"command":command})
                }
                "identity" => {
                    let (item, node) = provider_item_ids(
                        input["instanceId"].as_str().unwrap(),
                        input["nativeId"].as_str().unwrap(),
                        input["itemIdentityVersion"] == 2,
                    );
                    json!({"item":item,"node":node})
                }
                "paths" => json!(file_paths(input)),
                "output" => json!(output_text(input)),
                "readInput" => read_input(&input["input"], input["path"].as_str()),
                "exit" => json!(projected_exit_code(
                    input["status"].as_str().unwrap(),
                    &input["output"]
                )),
                "search" => json!(search_label(input)),
                "presentation" => presentation(
                    input["itemType"].as_str().unwrap_or(""),
                    input["title"].as_str(),
                    input["detail"].as_str(),
                    &input["data"],
                    input["fallbackSummary"].as_str().unwrap_or(""),
                ),
                "data" => merge_data(&input["previous"], &input["next"]),
                operation => panic!("Unknown tool oracle operation {operation}"),
            };
            assert_eq!(
                actual, fixture["output"],
                "source tool fixture {index}: {}",
                fixture["operation"]
            );
        }
    }
}
