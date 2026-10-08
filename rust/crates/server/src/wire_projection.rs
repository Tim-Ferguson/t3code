//! Keep persisted provider output intact while bounding timeline and detail transport.
use serde_json::{Map, Value, json};
use t3_contracts::trim_wire_string;
fn whitespace(c: char) -> bool {
    let mut buffer = [0u8; 4];
    trim_wire_string(c.encode_utf8(&mut buffer)).is_empty()
}
fn transport_prefix(text: &str, limit: usize) -> &str {
    let prefix = prefix(text, limit);
    prefix.strip_suffix('\u{fffd}').unwrap_or(prefix)
}

const DETAIL: usize = 32_768;
const DYNAMIC: usize = 16_384;
const ON_DEMAND: usize = 256 * 1024;
const TRUNCATED: &str = "\n… output truncated for transport";
fn size(value: &Value) -> usize {
    serde_json::to_vec(value).unwrap().len()
}
fn prefix(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
fn truncate(value: &mut Value, limit: usize) {
    if let Some(text) = value.as_str().filter(|text| text.len() > limit) {
        *value = json!(format!("{}{TRUNCATED}", transport_prefix(text, limit)));
    }
}
fn truncate_field(value: &mut Value, key: &str, limit: usize) {
    if let Some(field) = value.get_mut(key) {
        truncate(field, limit);
    }
}
fn has_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !trim_wire_string(s).is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}
fn summarize(value: &Value) -> Value {
    if !value
        .as_str()
        .is_some_and(|text| text.encode_utf16().take(DYNAMIC + 1).count() > DYNAMIC)
        && size(value) <= DYNAMIC
    {
        return value.clone();
    }
    let serialized;
    let text = match value.as_str() {
        Some(text) => text,
        None => {
            serialized = value.to_string();
            &serialized
        }
    };
    let mut normalized = String::new();
    let mut space = false;
    let mut units = 0;
    for character in text.trim_start_matches(whitespace).chars() {
        if character == '\n' {
            break;
        }
        if whitespace(character) {
            space = true;
            continue;
        }
        if space {
            normalized.push(' ');
            units += 1;
        }
        normalized.push(character);
        units += character.len_utf16();
        space = false;
        if units > 160 {
            break;
        }
    }
    let summary = if normalized.is_empty() {
        "Large tool output".into()
    } else if normalized.encode_utf16().count() <= 160 {
        normalized
    } else {
        format!(
            "{}…",
            utf16_prefix(&normalized, 159).trim_end_matches(whitespace)
        )
    };
    json!({"summary":summary,"truncated":true})
}
fn utf16_prefix(text: &str, limit: usize) -> &str {
    let mut units = 0;
    let mut end = 0;
    for (cindex, c) in text.char_indices() {
        if units + c.len_utf16() > limit {
            break;
        }
        units += c.len_utf16();
        end = cindex + c.len_utf8();
    }
    &text[..end]
}
fn bounded(value: &Value) -> Value {
    if let Some(text) = value.as_str() {
        if text.len() <= ON_DEMAND {
            return value.clone();
        }
        return json!(format!("{}{TRUNCATED}", transport_prefix(text, ON_DEMAND)));
    }
    if size(value) <= ON_DEMAND {
        value.clone()
    } else {
        let serialized = value.to_string();
        json!(format!(
            "{}{TRUNCATED}",
            transport_prefix(&serialized, ON_DEMAND)
        ))
    }
}
fn image(block: &Value) -> Option<Value> {
    if block["type"] != "image" {
        return None;
    }
    let (mime, data) = if block["source"].is_object() {
        if block["source"]["type"] != "base64" {
            return None;
        }
        (&block["source"]["media_type"], &block["source"]["data"])
    } else {
        (&block["mimeType"], &block["data"])
    };
    let mime = mime.as_str()?.to_ascii_lowercase();
    if !matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) || !data.is_string()
    {
        return None;
    }
    Some(json!({"type":"image","mimeType":mime}))
}
fn omit_images(output: &Value) -> Value {
    if let Some(array) = output.as_array() {
        return json!(
            array
                .iter()
                .map(|block| image(block).unwrap_or_else(|| block.clone()))
                .collect::<Vec<_>>()
        );
    }
    if let Some(array) = output["content"].as_array() {
        let mut output = output.clone();
        output["content"] = json!(
            array
                .iter()
                .map(|block| image(block).unwrap_or_else(|| block.clone()))
                .collect::<Vec<_>>()
        );
        return output;
    }
    image(output).unwrap_or_else(|| output.clone())
}
fn bounded_output(output: &Value) -> Value {
    let omitted = omit_images(output);
    let bound = bounded(&omitted);
    if !bound.is_string() || omitted.is_string() {
        return bound;
    }
    let blocks = omitted.as_array().or_else(|| omitted["content"].as_array());
    let candidates = blocks
        .map(Vec::as_slice)
        .unwrap_or(std::slice::from_ref(&omitted));
    let images: Vec<Value> = candidates
        .iter()
        .filter(|block| {
            block["type"] == "image"
                && block["mimeType"].as_str().is_some_and(|mime| {
                    matches!(
                        mime.to_ascii_lowercase().as_str(),
                        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
                    )
                })
        })
        .take(8)
        .cloned()
        .collect();
    if images.is_empty() {
        bound
    } else {
        let mut result = vec![json!({"type":"text","text":bound})];
        result.extend(images);
        json!(result)
    }
}
struct Budget {
    bytes: usize,
    nodes: usize,
    exceeded: bool,
}
fn result(value: &Value, budget: &mut Budget, depth: usize) -> (Option<Value>, bool) {
    if depth > 4 || budget.nodes == 0 {
        budget.exceeded = true;
        return (None, false);
    }
    budget.nodes -= 1;
    if let Some(text) = value.as_str() {
        if text.len() > budget.bytes {
            budget.exceeded = true;
            return (None, false);
        }
        budget.bytes -= text.len();
        return serde_json::from_str::<Value>(text)
            .map(|value| result(&value, budget, depth + 1))
            .unwrap_or((None, false));
    }
    if let Some(array) = value.as_array() {
        if array.len() > 32 {
            budget.exceeded = true;
            return (None, false);
        }
        let (mut data, mut failed) = (None, false);
        for block in array {
            let text = block
                .get("text")
                .map(|text| {
                    if text.is_object() {
                        text.get("text").unwrap_or(text)
                    } else {
                        text
                    }
                })
                .unwrap_or(&Value::Null);
            let (nested, error) = result(text, budget, depth + 1);
            if data.is_none() {
                data = nested;
            }
            failed |= error;
            if budget.exceeded {
                break;
            }
        }
        return (data, failed);
    }
    let Some(object) = value.as_object() else {
        return (None, false);
    };
    let failed = value["isError"] == true
        || value["is_error"] == true
        || value["_tag"] == "OrchestratorMcpFailure"
        || object.get("error").is_some_and(|error| !error.is_null());
    if let Some(content) = object
        .get("structuredContent")
        .filter(|value| !value.is_null())
        .or_else(|| object.get("content"))
    {
        let (data, error) = result(content, budget, depth + 1);
        return (data, failed || error);
    }
    (Some(value.clone()), failed)
}
fn id(value: &Value) -> Option<Value> {
    value
        .as_str()
        .filter(|text| text.encode_utf16().count() <= 256 && !trim_wire_string(text).is_empty())
        .map(|_| value.clone())
}
fn html(value: &Value) -> Option<Value> {
    let attachment = value["attachmentId"]
        .as_str()
        .filter(|text| !text.is_empty() && text.encode_utf16().count() <= 256)?;
    let title = value["title"].as_str()?;
    let height = value["height"].as_f64()?;
    let title = utf16_prefix(trim_wire_string(title), 200);
    let mut output = json!({"attachmentId":attachment,"title":if title.is_empty(){"HTML"}else{title},"height":height.round().clamp(80.,2000.) as i64});
    if let Some(heights) = value["heights"]
        .as_array()
        .filter(|array| !array.is_empty() && array.len() <= 24)
    {
        let mut measured = Vec::new();
        for entry in heights {
            let Some(pair) = entry.as_array().filter(|a| a.len() == 2) else {
                return Some(output);
            };
            let Some(width) = pair[0].as_u64().filter(|w| *w >= 1 && *w <= 10000) else {
                return Some(output);
            };
            let Some(height) = pair[1].as_f64() else {
                return Some(output);
            };
            measured.push((width, height.round().clamp(80., 2000.) as i64));
        }
        measured.sort_by_key(|pair| pair.0);
        output["heights"] = json!(measured);
    }
    Some(output)
}
fn domain(value: &str) -> bool {
    if value.len() > 256 {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    let Some((scheme, host)) = lower.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https" | "ws" | "wss") {
        return false;
    }
    let host = host.strip_prefix("*.").unwrap_or(host);
    let host = if let Some((host, port)) = host.split_once(':') {
        if port.is_empty() || port.len() > 5 || !port.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        host
    } else {
        host
    };
    !host.is_empty()
        && host.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}
fn app(value: &Value) -> Option<Value> {
    let mut output = Map::new();
    for field in ["attachmentId", "server", "tool"] {
        output.insert(field.into(), id(&value[field])?);
    }
    let uri = value["resourceUri"]
        .as_str()
        .filter(|uri| uri.starts_with("ui://") && uri.encode_utf16().count() <= 4096)?;
    output.insert("resourceUri".into(), json!(uri));
    let mut csp = Map::new();
    for key in [
        "connectDomains",
        "resourceDomains",
        "frameDomains",
        "baseUriDomains",
    ] {
        if let Some(array) = value["csp"][key].as_array() {
            let domains: Vec<_> = array
                .iter()
                .filter_map(Value::as_str)
                .filter(|text| domain(text))
                .take(32)
                .collect();
            if !domains.is_empty() {
                csp.insert(key.into(), json!(domains));
            }
        }
    }
    if !csp.is_empty() {
        output.insert("csp".into(), Value::Object(csp));
    }
    let mut permissions = Map::new();
    for key in ["camera", "microphone", "geolocation", "clipboardWrite"] {
        if value["permissions"][key].is_object() {
            permissions.insert(key.into(), json!({}));
        }
    }
    if !permissions.is_empty() {
        output.insert("permissions".into(), Value::Object(permissions));
    }
    if value["prefersBorder"].is_boolean() {
        output.insert("prefersBorder".into(), value["prefersBorder"].clone());
    }
    Some(Value::Object(output))
}
fn compact(value: &Value) -> Option<Value> {
    let mut budget = Budget {
        bytes: 16384,
        nodes: 128,
        exceeded: false,
    };
    let (data, failed) = result(value, &mut budget, 0);
    let mut output = Map::new();
    if failed {
        output.insert("isError".into(), json!(true));
    }
    if let Some(data) = data.filter(|_| !budget.exceeded) {
        for key in ["threadId", "messageId", "taskId", "scheduledTaskId"] {
            if let Some(id) = id(&data[key]) {
                output.insert(key.into(), id);
            }
        }
        if data["status"] == "rolled_back" {
            output.insert("status".into(), json!("rolled_back"));
        }
        if let Some(value) = html(&data["htmlRender"]) {
            output.insert("htmlRender".into(), value);
        }
        if let Some(value) = app(&data["t3McpApp"]) {
            output.insert("t3McpApp".into(), value);
        }
        if let Some(id) = id(&data["thread"]["threadId"]) {
            output.insert("thread".into(), json!({"threadId":id}));
        }
        if let Some(array) = data["threads"].as_array() {
            let mut complete = array.len() <= 100;
            let mut threads = vec![];
            if complete {
                for entry in array {
                    let thread = id(&entry["threadId"]);
                    let rolled = entry["status"] == "rolled_back";
                    if !entry.is_object() || (thread.is_none() && !rolled) {
                        complete = false;
                        break;
                    }
                    let mut row = Map::new();
                    if let Some(thread) = thread {
                        row.insert("threadId".into(), thread);
                    }
                    if rolled {
                        row.insert("status".into(), json!("rolled_back"));
                    }
                    threads.push(Value::Object(row));
                }
            }
            if complete {
                output.insert("threads".into(), json!(threads));
            } else {
                output.remove("threadId");
                output.remove("status");
            }
        }
    }
    if serde_json::to_vec(&output).unwrap().len() > 8192 {
        output.remove("threads");
        output.remove("threadId");
        output.remove("status");
    }
    if serde_json::to_vec(&output).unwrap().len() > 8192 {
        if let Some(app) = output.get_mut("t3McpApp").and_then(Value::as_object_mut) {
            app.remove("csp");
        }
    }
    if serde_json::to_vec(&output).unwrap().len() > 8192 {
        output.remove("t3McpApp");
    }
    if output.is_empty() {
        None
    } else {
        Some(Value::Object(output))
    }
}
fn failure(text: &str) -> bool {
    let text = text.to_lowercase();
    if [
        "file not found",
        "no files found",
        "enoent",
        "no such file",
        "commandnotfoundexception",
        "command not found",
        "is not recognized as the name of a cmdlet",
        "a parameter cannot be found that matches parameter name",
    ]
    .iter()
    .any(|needle| text.contains(needle))
        || (text.contains("cannot find path") && text.contains("because it does not exist"))
        || (text.contains("is not recognized") && text.contains("the term '"))
    {
        return true;
    }
    for (index, _) in text.match_indices("exit code") {
        let raw = &text[index + 9..];
        if !raw
            .chars()
            .next()
            .is_some_and(|c| whitespace(c) || c == ':')
        {
            continue;
        }
        let trimmed = raw.trim_start_matches(whitespace);
        let suffix = trimmed
            .strip_prefix(':')
            .unwrap_or(trimmed)
            .trim_start_matches(whitespace);
        let digits: String = suffix.chars().take_while(char::is_ascii_digit).collect();
        if digits.starts_with(|c: char| matches!(c, '1'..='9'))
            && ((raw.chars().next().is_some_and(whitespace)
                && !trimmed.starts_with(':')
                && (text[..index].ends_with("exit with ")
                    || text[..index].ends_with("exited with ")))
                || suffix[digits.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_'))
        {
            return true;
        }
    }
    false
}
pub fn item(item: &Value) -> Value {
    let mut output = item.clone();
    match item["type"].as_str() {
        Some("handoff") => {
            output.as_object_mut().unwrap().remove("summary");
        }
        Some("command_execution") => {
            output.as_object_mut().unwrap().remove("output");
            if item["outputIndicatesFailure"] == true
                || item
                    .get("exitCode")
                    .is_some_and(|code| !code.is_null() && *code != 0)
                || item["output"]
                    .as_str()
                    .is_some_and(|text| failure(utf16_prefix(text, DETAIL)))
            {
                output["outputIndicatesFailure"] = json!(true);
            }
            if item["output"]
                .as_str()
                .is_some_and(|text| !trim_wire_string(text).is_empty())
            {
                output["outputOmitted"] = json!(true);
            }
        }
        Some("file_change") => {
            for key in ["diffStr", "oldStr", "newStr"] {
                output.as_object_mut().unwrap().remove(key);
            }
            if item["status"] == "failed"
                && item["diffStr"]
                    .as_str()
                    .is_some_and(|text| !trim_wire_string(text).is_empty())
            {
                output["diffStr"] = item["diffStr"].clone();
                truncate_field(&mut output, "diffStr", DETAIL);
            }
        }
        Some("subagent") => {
            for key in ["prompt", "progress", "result"] {
                truncate_field(&mut output, key, DETAIL);
            }
        }
        Some("dynamic_tool") => {
            if let Some(input) = item.get("input") {
                output["input"] = summarize(input);
            }
            output.as_object_mut().unwrap().remove("output");
            if let Some(compact) = compact(&item["output"]) {
                output["output"] = compact;
            }
            if has_value(&item["output"]) {
                output["outputOmitted"] = json!(true);
            }
        }
        _ => {}
    }
    output
}
pub fn detail(item: &Value) -> Value {
    let mut output = item.clone();
    match item["type"].as_str() {
        Some("command_execution") => {
            truncate_field(&mut output, "input", ON_DEMAND);
            truncate_field(&mut output, "output", ON_DEMAND);
        }
        Some("dynamic_tool") => {
            if let Some(input) = item.get("input") {
                output["input"] = bounded(input);
            }
            if let Some(value) = item.get("output") {
                output["output"] = bounded_output(value);
            }
        }
        Some("subagent") => {
            for key in ["prompt", "progress", "result"] {
                truncate_field(&mut output, key, ON_DEMAND);
            }
        }
        Some("handoff" | "file_change") => return self::item(item),
        _ => {}
    }
    output
}
pub fn handoff(value: &Value) -> Value {
    let mut value = value.clone();
    let object = value.as_object_mut().unwrap();
    object.remove("history");
    object.remove("delivery");
    object.insert("summaryText".into(), json!(""));
    value
}
pub fn projection(value: &Value) -> Value {
    let mut value = value.clone();
    for field in ["turnItems", "visibleTurnItems", "contextHandoffs"] {
        if let Some(array) = value[field].as_array_mut() {
            for row in array {
                if field == "visibleTurnItems" {
                    row["item"] = item(&row["item"]);
                } else if field == "turnItems" {
                    *row = item(row);
                } else {
                    *row = handoff(row);
                }
            }
        }
    }
    value
}
pub fn domain_event(value: Value) -> Value {
    let mut value = value;
    match value["type"].as_str() {
        Some("turn-item.updated") => value["payload"] = item(&value["payload"]),
        Some("context-handoff.updated") => value["payload"] = handoff(&value["payload"]),
        _ => {}
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    fn expand(value: Value) -> Value {
        match value {
            Value::Object(mut value) => {
                if let Some(repeat) = value.remove("_repeat") {
                    return json!(
                        repeat[0]
                            .as_str()
                            .unwrap()
                            .repeat(repeat[1].as_u64().unwrap() as usize)
                    );
                }
                if let Some(concat) = value.remove("_concat") {
                    return json!(
                        concat
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|part| expand(part.clone()).as_str().unwrap().to_owned())
                            .collect::<String>()
                    );
                }
                Value::Object(
                    value
                        .into_iter()
                        .map(|(key, value)| (key, expand(value)))
                        .collect(),
                )
            }
            Value::Array(value) => Value::Array(value.into_iter().map(expand).collect()),
            _ => value,
        }
    }
    #[test]
    fn source_reference_wire_projection_cases_match() {
        use sha2::{Digest, Sha256};
        fn canonical(value: Value) -> Value {
            match value {
                Value::Object(value) => {
                    let mut fields: Vec<_> = value.into_iter().collect();
                    fields.sort_by(|a, b| a.0.cmp(&b.0));
                    Value::Object(
                        fields
                            .into_iter()
                            .map(|(key, value)| (key, canonical(value)))
                            .collect(),
                    )
                }
                Value::Array(value) => Value::Array(value.into_iter().map(canonical).collect()),
                _ => value,
            }
        }
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../tests/fixtures/wire-projection-parity.json"
        ))
        .unwrap();
        for (index, case) in cases.iter().enumerate() {
            let input = canonical(expand(case["input"].clone()));
            let output = match case["method"].as_str().unwrap() {
                "wire" => item(&input),
                "detail" => detail(&input),
                "handoff" => handoff(&input),
                _ => unreachable!(),
            };
            let hash = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&canonical(output)).unwrap())
            );
            assert_eq!(
                hash,
                case["sha256"].as_str().unwrap(),
                "source fixture {index}: {} {}",
                case["method"],
                case["input"]
            );
        }
    }
    #[test]
    fn command_output_is_omitted_from_timeline_and_utf8_bounded_for_detail() {
        let output = format!("No such file\n{}", "🧰".repeat(100_000));
        let original =
            json!({"type":"command_execution","input":"execute","output":output,"exitCode":0});
        let wire = item(&original);
        assert!(wire.get("output").is_none());
        assert_eq!(wire["outputOmitted"], true);
        assert_eq!(wire["outputIndicatesFailure"], true);
        let detail = detail(&original);
        let text = detail["output"].as_str().unwrap();
        assert!(text.len() <= ON_DEMAND + TRUNCATED.len());
        assert!(text.ends_with(TRUNCATED));
        assert!(!text.contains('\u{fffd}'));
        assert_eq!(original["output"], output);
        assert!(
            item(&json!({"type":"command_execution","input":"echo","output":"   "}))
                .get("outputOmitted")
                .is_none()
        );
    }
    #[test]
    fn dynamic_detail_strips_image_bytes_but_preserves_markers_after_large_text() {
        let output = json!({"content":[{"type":"text","text":"x".repeat(400_000)},{"type":"image","mimeType":"image/png","data":"private screenshot bytes"},{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"private frame"}}]});
        let original = json!({"type":"dynamic_tool","input":"a ".repeat(20_000),"output":output});
        let wire = item(&original);
        assert_eq!(wire["input"]["truncated"], true);
        assert!(wire.get("output").is_none());
        assert_eq!(wire["outputOmitted"], true);
        let detail = detail(&original);
        assert_eq!(detail["output"].as_array().unwrap().len(), 3);
        assert_eq!(
            detail["output"][1],
            json!({"type":"image","mimeType":"image/png"})
        );
        assert_eq!(
            detail["output"][2],
            json!({"type":"image","mimeType":"image/jpeg"})
        );
        assert!(!detail.to_string().contains("private"));
    }
    #[test]
    fn dynamic_summary_keeps_bounded_ids_and_failure_envelopes() {
        let output = json!({"structuredContent":{"threadId":"thread","thread":{"threadId":"nested"},"threads":[{"threadId":"one"},{"status":"rolled_back"}],"htmlRender":{"attachmentId":"page","title":" Page ","height":2500}},"isError":true});
        let summary = compact(&output).unwrap();
        assert_eq!(summary["isError"], true);
        assert_eq!(summary["threadId"], "thread");
        assert_eq!(summary["htmlRender"]["height"], 2000.0);
        assert_eq!(summary["htmlRender"]["title"], "Page");
        let incomplete = compact(
            &json!({"threadId":"misleading","status":"rolled_back","threads":[{"threadId":"valid"},{}]}),
        );
        assert!(incomplete.is_none());
        assert_eq!(
            compact(&json!({"is_error":true,"content":[]})).unwrap(),
            json!({"isError":true})
        );
    }
    #[test]
    fn file_diffs_handoff_payloads_and_subagent_summaries_follow_source_limits() {
        let change = json!({"type":"file_change","status":"completed","diffStr":"patch","oldStr":"old","newStr":"new","path":"file"});
        assert_eq!(
            item(&change),
            json!({"type":"file_change","status":"completed","path":"file"})
        );
        let mut failed = change.clone();
        failed["status"] = json!("failed");
        assert_eq!(item(&failed)["diffStr"], "patch");
        assert!(
            detail(&json!({"type":"handoff","summary":"full context","contextHandoffId":"h"}))
                .get("summary")
                .is_none()
        );
        let projected = projection(
            &json!({"contextHandoffs":[{"id":"h","history":["large"],"delivery":{},"summaryText":"secret"}],"turnItems":[],"visibleTurnItems":[]}),
        );
        assert_eq!(
            projected["contextHandoffs"][0],
            json!({"id":"h","summaryText":""})
        );
        let subagent = json!({"type":"subagent","prompt":"x".repeat(DETAIL+1),"progress":"y".repeat(DETAIL+1),"result":null});
        assert!(
            item(&subagent)["prompt"]
                .as_str()
                .unwrap()
                .ends_with(TRUNCATED)
        );
        assert_eq!(item(&subagent)["result"], Value::Null);
    }
}
