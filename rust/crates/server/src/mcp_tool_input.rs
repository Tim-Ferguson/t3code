//! The original tool catalog and raw admission of tool input against it.
//!
//! The catalog is written by `tools/generate_mcp_tool_catalog.mjs` from the original toolkits, so
//! descriptions and schemas are the ones the model reads there. Admission follows the JSON schema
//! the catalog carries: a property that is not required accepts a value or none, never an explicit
//! null (the original decodes the raw schema, not its JSON codec), and properties the schema does
//! not name are dropped before any handler sees them. Four thread tools keep their exact
//! Effect messages in `mcp_control_parameters.rs`; the rest report the same shape of issue.
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

/// Tools served by the device service rather than the orchestrator handlers.
pub fn device_group(group: &str) -> bool {
    group.starts_with("device")
}

/// Every original tool as `{group, tool}`, in the original's registration order by toolkit.
pub fn entries() -> &'static [Value] {
    static ENTRIES: OnceLock<Vec<Value>> = OnceLock::new();
    ENTRIES.get_or_init(|| serde_json::from_str(include_str!("mcp_tool_catalog.json")).expect("source-generated tool catalog"))
}

pub fn schema(name: &str) -> Option<&'static Value> {
    entries().iter().find(|entry| entry["tool"]["name"] == name).map(|entry| &entry["tool"]["inputSchema"])
}

fn types(schema: &Value) -> String {
    if let Some(any) = schema["anyOf"].as_array() {
        return any.iter().map(types).collect::<Vec<_>>().join(" | ");
    }
    if let Some(values) = schema["enum"].as_array() {
        return values.iter().map(|value| value.to_string()).collect::<Vec<_>>().join(" | ");
    }
    match schema["type"].as_str() {
        Some("integer") => "number".into(),
        Some("array") => "array".into(),
        Some(other) => other.into(),
        None => "unknown".into(),
    }
}

fn utf16(text: &str) -> usize {
    text.encode_utf16().count()
}

fn at(path: &[String]) -> String {
    path.iter().map(|part| if part.parse::<usize>().is_ok() { format!("[{part}]") } else { format!("[\"{part}\"]") }).collect()
}

fn issue(errors: &mut Vec<String>, path: &[String], message: String) {
    errors.push(if path.is_empty() { message } else { format!("{message}\n  at {}", at(path)) });
}

/// Checks `value` against `schema`, returning it with unnamed object properties removed.
fn admit(schema: &Value, value: &Value, path: &mut Vec<String>, errors: &mut Vec<String>) -> Value {
    if let Some(any) = schema["anyOf"].as_array() {
        for branch in any {
            let mut trial = Vec::new();
            let admitted = admit(branch, value, &mut path.clone(), &mut trial);
            if trial.is_empty() {
                return admitted;
            }
        }
        issue(errors, path, format!("Expected {}", types(schema)));
        return value.clone();
    }
    let before = errors.len();
    if let Some(values) = schema["enum"].as_array() {
        if !values.contains(value) {
            issue(errors, path, format!("Expected {}", types(schema)));
            return value.clone();
        }
    }
    let mismatch = |errors: &mut Vec<String>, path: &[String]| issue(errors, path, format!("Expected {}", types(schema)));
    let result = match schema["type"].as_str() {
        Some("string") => match value.as_str() {
            Some(text) => {
                if schema["maxLength"].as_u64().is_some_and(|max| utf16(text) as u64 > max) {
                    issue(errors, path, format!("Expected a value with a length of at most {}", schema["maxLength"]));
                }
                if schema["minLength"].as_u64().is_some_and(|min| (utf16(text) as u64) < min) {
                    issue(errors, path, format!("Expected a value with a length of at least {}", schema["minLength"]));
                }
                if let Some(pattern) = schema["pattern"].as_str() {
                    if regex::Regex::new(pattern).is_ok_and(|pattern| !pattern.is_match(text)) {
                        issue(errors, path, format!("Expected a string matching the regex {pattern}"));
                    }
                }
                value.clone()
            }
            None => {
                mismatch(errors, path);
                value.clone()
            }
        },
        Some(kind @ ("integer" | "number")) => match value.as_f64().filter(|_| value.is_number()) {
            Some(number) => {
                if kind == "integer" && (number.fract() != 0.0 || number.abs() > 9007199254740991.0) {
                    issue(errors, path, "Expected an integer".into());
                }
                for (key, text, holds) in [
                    ("minimum", "greater than or equal to", (|n: f64, l: f64| n >= l) as fn(f64, f64) -> bool),
                    ("maximum", "less than or equal to", |n, l| n <= l),
                    ("exclusiveMinimum", "greater than", |n, l| n > l),
                    ("exclusiveMaximum", "less than", |n, l| n < l),
                ] {
                    if let Some(limit) = schema[key].as_f64() {
                        if !holds(number, limit) {
                            issue(errors, path, format!("Expected a value {text} {limit}"));
                        }
                    }
                }
                for branch in schema["allOf"].as_array().into_iter().flatten() {
                    admit(branch, value, path, errors);
                }
                value.clone()
            }
            None => {
                mismatch(errors, path);
                value.clone()
            }
        },
        Some("boolean") => {
            if !value.is_boolean() {
                mismatch(errors, path);
            }
            value.clone()
        }
        Some("null") => {
            if !value.is_null() {
                mismatch(errors, path);
            }
            value.clone()
        }
        Some("array") => match value.as_array() {
            Some(items) => {
                if schema["maxItems"].as_u64().is_some_and(|max| items.len() as u64 > max) {
                    issue(errors, path, format!("Expected a value with a length of at most {}", schema["maxItems"]));
                }
                if schema["minItems"].as_u64().is_some_and(|min| (items.len() as u64) < min) {
                    issue(errors, path, format!("Expected a value with a length of at least {}", schema["minItems"]));
                }
                let element = &schema["items"];
                Value::Array(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            path.push(index.to_string());
                            let admitted = if element.is_object() { admit(element, item, path, errors) } else { item.clone() };
                            path.pop();
                            admitted
                        })
                        .collect(),
                )
            }
            None => {
                mismatch(errors, path);
                value.clone()
            }
        },
        Some("object") => match value.as_object() {
            Some(object) => {
                let properties = schema["properties"].as_object();
                let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let mut admitted = Map::new();
                for (key, property) in properties.into_iter().flatten() {
                    let optional = !required.contains(&key.as_str());
                    path.push(key.clone());
                    match object.get(key) {
                        None if optional => {}
                        None => issue(errors, path, "Missing key".into()),
                        // The raw schema of an optional property names the value, or none: not null.
                        Some(Value::Null) if optional && path.len() == 1 && property["anyOf"].as_array().is_some_and(|any| any.iter().any(|branch| branch["type"] == "null")) => {
                            let named = property["anyOf"].as_array().unwrap().iter().filter(|branch| branch["type"] != "null").map(types).collect::<Vec<_>>().join(" | ");
                            issue(errors, path, format!("Expected {named} | undefined"));
                        }
                        Some(found) => {
                            admitted.insert(key.clone(), admit(property, found, path, errors));
                        }
                    }
                    path.pop();
                }
                Value::Object(admitted)
            }
            None => {
                mismatch(errors, path);
                value.clone()
            }
        },
        _ => value.clone(),
    };
    if errors.len() == before { result } else { value.clone() }
}

/// The tool's input as its handler may read it, or the message the original's decoder gives.
pub fn decode(name: &str, input: Value) -> Result<Value, String> {
    let input = if input.is_null() { json!({}) } else { input };
    let schema = schema(name).ok_or_else(|| format!("Tool '{name}' not found"))?;
    let mut errors = Vec::new();
    let admitted = admit(schema, &input, &mut Vec::new(), &mut errors);
    if errors.is_empty() { Ok(admitted) } else { Err(format!("Invalid parameters for tool '{name}': {}", errors.join("\n"))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_follows_the_catalog_schema() {
        let ok = decode("request_secret", json!({"label":"Token","reason":"Needed","spoofed":1})).unwrap();
        assert_eq!(ok, json!({"label":"Token","reason":"Needed"}));
        assert!(decode("request_secret", json!({"label":"Token"})).unwrap_err().contains("Missing key\n  at [\"reason\"]"));
        assert!(decode("request_secret", json!({"label":"a","reason":"b","timeoutMs":10})).unwrap_err().contains("at [\"timeoutMs\"]"));
        let null = decode("request_secret", json!({"label":"a","reason":"b","placeholder":null})).unwrap_err();
        assert!(null.contains("Expected string | undefined"), "{null}");
        let schedule = json!({"prompt":"p","schedule":{"type":"interval","everyMs":10}});
        assert!(decode("schedule_task", schedule).is_err());
        assert!(decode("schedule_task", json!({"prompt":"p","schedule":{"type":"interval","everyMs":60000}})).is_ok());
    }
}
