//! Source-backed presentation rules for persisted tool and activity items.
//! Mirrors client-runtime/work-log/itemDetail.ts; images have separate adapters.
use serde_json::Value;

pub fn detail_revision(item: &Value) -> String {
    if matches!(
        item["status"].as_str(),
        Some("idle" | "pending" | "running" | "waiting")
    ) {
        "live".into()
    } else {
        item["updatedAt"].as_str().unwrap_or_default().to_owned()
    }
}

pub fn needs_detail(item: &Value) -> bool {
    match item["type"].as_str() {
        Some("command_execution") => item["outputOmitted"] == true,
        Some("dynamic_tool") => {
            item["outputOmitted"] == true
                || (item["input"]["truncated"] == true && item["input"]["summary"].is_string())
        }
        _ => false,
    }
}

pub fn output_text(item: &Value) -> Option<String> {
    match item["type"].as_str()? {
        "command_execution" => {
            let output = item["output"].as_str()?;
            if output.trim().is_empty() {
                return None;
            }
            // Legacy Claude bash items persisted structured stdout/stderr JSON.
            if output.trim_start().starts_with("{\"stdout\"") {
                if let Ok(value) = serde_json::from_str::<Value>(output.trim()) {
                    if value["stdout"].is_string()
                        && value["stderr"].is_string()
                        && value["interrupted"].is_boolean()
                    {
                        return nonempty(
                            [
                                value["stdout"].as_str().unwrap(),
                                value["stderr"].as_str().unwrap(),
                            ]
                            .into_iter()
                            .filter(|part| !part.trim().is_empty())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        );
                    }
                }
            }
            Some(output.to_owned())
        }
        "dynamic_tool" if item["outputOmitted"] != true => format_value(&item["output"]),
        "file_search" => nonempty(
            item["results"]
                .as_array()?
                .iter()
                .map(|result| {
                    let mut location = result["fileName"].as_str().unwrap_or_default().to_owned();
                    if let Some(line) = result["line"].as_u64() {
                        location.push_str(&format!(":{line}"));
                    }
                    if let Some(preview) = result["preview"]
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                    {
                        location.push('\n');
                        location.push_str(preview.trim());
                    }
                    location
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "web_search" => nonempty(
            item["results"]
                .as_array()?
                .iter()
                .map(|result| {
                    let title = result["title"]
                        .as_str()
                        .filter(|title| !title.trim().is_empty());
                    let url = result["url"].as_str().unwrap_or_default();
                    let mut parts = vec![title.map(str::trim).unwrap_or(url)];
                    if result["title"]
                        .as_str()
                        .is_some_and(|title| !title.is_empty())
                    {
                        parts.push(url);
                    }
                    if let Some(snippet) = result["snippet"]
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                    {
                        parts.push(snippet.trim());
                    }
                    parts.join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        _ => None,
    }
}

fn nonempty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn text_blocks(value: &Value, depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(values) => {
            let parts: Option<Vec<_>> = values
                .iter()
                .map(|value| text_blocks(value, depth + 1))
                .collect();
            Some(
                parts?
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        Value::Object(object) => {
            match object.get("type").and_then(Value::as_str) {
                Some("text") => {
                    return object
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                Some("image") => {
                    return Some(
                        if matches!(
                            object.get("mimeType").and_then(Value::as_str),
                            Some("image/png" | "image/jpeg" | "image/webp" | "image/gif")
                        ) {
                            ""
                        } else {
                            "[image]"
                        }
                        .into(),
                    );
                }
                Some("resource_link") => {
                    return object.get("uri").and_then(Value::as_str).map(str::to_owned);
                }
                Some("resource") => {
                    if let Some(resource) = object.get("resource") {
                        return resource["text"]
                            .as_str()
                            .or_else(|| resource["uri"].as_str())
                            .map(str::to_owned);
                    }
                }
                _ => {}
            }
            let keys: Vec<_> = object
                .keys()
                .filter(|key| *key != "isError" && *key != "is_error")
                .map(String::as_str)
                .collect();
            if keys == ["content"] {
                return text_blocks(&object["content"], depth + 1);
            }
            if keys.len() == 2 && keys.contains(&"content") && keys.contains(&"structuredContent") {
                return text_blocks(&object["content"], depth + 1)
                    .filter(|text| !text.trim().is_empty());
            }
            None
        }
        _ => None,
    }
}

pub fn format_value(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    if let Some(text) = text_blocks(value, 0) {
        if text.trim().is_empty() {
            return None;
        }
        let trimmed = text.trim();
        if trimmed.starts_with(['[', '{']) {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                return serde_json::to_string_pretty(&value).ok();
            }
            let documents: Result<Vec<Value>, _> = trimmed
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line.trim()))
                .collect();
            if let Ok(documents) = documents {
                return Some(
                    documents
                        .iter()
                        .map(|value| serde_json::to_string_pretty(value).unwrap())
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                );
            }
        }
        return Some(text);
    }
    let text = serde_json::to_string_pretty(value).ok()?;
    (!matches!(text.as_str(), "{}" | "[]")).then_some(text)
}

/// Fetching full tool data must not overwrite newer streaming status/title.
pub fn overlay_detail(projected: &Value, detail: &Value) -> Value {
    let mut item = projected.clone();
    if projected["id"] != detail["id"] || projected["type"] != detail["type"] {
        return item;
    }
    for field in ["input", "output", "outputOmitted"] {
        if let Some(value) = detail.get(field) {
            item[field] = value.clone();
        } else if field == "outputOmitted" {
            if let Some(object) = item.as_object_mut() {
                object.remove(field);
            }
        }
    }
    item
}
