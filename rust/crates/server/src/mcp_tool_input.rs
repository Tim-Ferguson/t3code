//! The original tool catalog and the decoding of a call's arguments against it.
//!
//! The catalog is written by `tools/generate_mcp_tool_catalog.mjs` from the original toolkits, so
//! descriptions and schemas are the ones the model reads there. Arguments are not decoded from that
//! JSON Schema, which loses what the decoder needs (trimming, refinements, union member
//! descriptors, exact optionality): `decode` runs the Effect Schema AST that
//! `tools/generate_mcp_tool_schemas.mjs` writes (`mcp_tool_decode.rs`) and returns the original's
//! text for a refusal. The catalog's JSON Schema still checks results against the output schemas.
use serde_json::{Map, Value, json};
use std::sync::OnceLock;

#[path = "mcp_tool_decode.rs"]
pub(crate) mod decoder;

/// Tools served by the device service rather than the orchestrator handlers.
pub fn device_group(group: &str) -> bool {
    group.starts_with("device")
}

/// Every original tool as `{group, tool}`, in the original's registration order by toolkit.
pub fn entries() -> &'static [Value] {
    static ENTRIES: OnceLock<Vec<Value>> = OnceLock::new();
    ENTRIES.get_or_init(|| serde_json::from_str(include_str!("mcp_tool_catalog.json")).expect("source-generated tool catalog"))
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
                // An `allOf` branch of a refined number only adds bounds to it.
                for branch in schema["allOf"].as_array().into_iter().flatten() {
                    let mut bounds = branch.clone();
                    bounds["type"] = json!(kind);
                    admit(&bounds, value, path, errors);
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
                // An object schema that names no properties is a record: its entries are the caller's.
                if properties.is_none() {
                    let entry = &schema["additionalProperties"];
                    return Value::Object(object.iter().map(|(key, found)| {
                        path.push(key.clone());
                        let admitted = if entry.is_object() && !entry.as_object().is_some_and(|schema| schema.is_empty()) { admit(entry, found, path, errors) } else { found.clone() };
                        path.pop();
                        (key.clone(), admitted)
                    }).collect());
                }
                let required: Vec<&str> = schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let mut admitted = Map::new();
                for (key, property) in properties.into_iter().flatten() {
                    let optional = !required.contains(&key.as_str());
                    path.push(key.clone());
                    match object.get(key) {
                        None if optional => {}
                        None => issue(errors, path, "Missing key".into()),
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

/// The tool's input as its handler reads it after the original's decoding, or the message that decoding gives.
pub fn decode(name: &str, input: Value) -> Result<Value, String> {
    let schemas = decoder::schemas();
    if !schemas.has(name) {
        return Err(format!("Tool '{name}' not found"));
    }
    // The registration decodes `payload ?? {}`.
    let input = if input.is_null() { json!({}) } else { input };
    schemas.decode(name, &decoder::Js::from_json(&input), true).map(|decoded| decoded.to_json()).map_err(|issue| format!("Invalid parameters for tool '{name}': {}", issue.format()))
}

/// What is wrong with a tool result against the output schema the catalog declares for it.
pub fn output_issues(name: &str, result: &Value) -> Vec<String> {
    let Some(schema) = entries().iter().find(|entry| entry["tool"]["name"] == name).map(|entry| &entry["tool"]["outputSchema"]).filter(|schema| schema.is_object()) else { return vec![] };
    let mut errors = Vec::new();
    admit(schema, result, &mut Vec::new(), &mut errors);
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The control and device catalogs, each generated on its own from the original, are entries of the full one.
    #[test]
    fn the_catalogs_generated_from_the_original_agree() {
        let tool = |name: &str| entries().iter().find(|entry| entry["tool"]["name"] == name).unwrap()["tool"].clone();
        for file in [include_str!("mcp_control_catalog.json"), include_str!("mcp_device_catalog.json")] {
            for expected in serde_json::from_str::<Vec<Value>>(file).unwrap() {
                assert_eq!(tool(expected["name"].as_str().unwrap()), expected);
            }
        }
        let names: Vec<&str> = entries().iter().map(|entry| entry["tool"]["name"].as_str().unwrap()).collect();
        assert_eq!(names.len(), 80);
        assert_eq!(names.iter().collect::<std::collections::HashSet<_>>().len(), 80, "tool names are unique");
        for handled in crate::mcp_control::HANDLED {
            assert!(names.contains(handled), "{handled}");
        }
        // Every tool the model reads carries a description and an input schema.
        for entry in entries() {
            assert!(entry["tool"]["description"].as_str().is_some_and(|text| !text.is_empty()) && entry["tool"]["inputSchema"].is_object(), "{}", entry["tool"]["name"]);
        }
    }

    /// The generator writes a long run of one string as `{"$repeat": [unit, count]}`, and a long array
    /// of one element as `{"$items": [element, count]}`.
    fn expand(value: &Value) -> Value {
        match value {
            Value::Object(object) if object.len() == 1 && object.contains_key("$repeat") => Value::String(object["$repeat"][0].as_str().unwrap().repeat(object["$repeat"][1].as_u64().unwrap() as usize)),
            Value::Object(object) if object.len() == 1 && object.contains_key("$items") => Value::Array(vec![expand(&object["$items"][0]); object["$items"][1].as_u64().unwrap() as usize]),
            Value::Object(object) => Value::Object(object.iter().map(|(key, found)| (key.clone(), expand(found))).collect()),
            Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
            other => other.clone(),
        }
    }

    /// The original's own decoding, run over cases derived from each tool's schema (nested unions
    /// included) by `tools/generate_mcp_tool_parameters.mjs`: the same inputs are accepted, with the
    /// same input handed to the handler, and the rest refused with the same message.
    #[test]
    fn admission_matches_the_originals_decoding_over_generated_cases() {
        let mut mismatches = Vec::new();
        let mut count = 0;
        let mut seen = std::collections::HashSet::new();
        for line in include_str!("../tests/fixtures/mcp-tool-parameters.jsonl").split('\n').filter(|line| !line.is_empty()) {
            let mut row: Value = serde_json::from_str(line).unwrap();
            for key in ["input", "decoded"] {
                row[key] = expand(&row[key]);
            }
            let name = row["name"].as_str().unwrap();
            seen.insert(name.to_owned());
            let actual = decode(name, row["input"].clone());
            count += 1;
            let same = match &actual {
                Ok(admitted) => row["accepted"] == true && *admitted == row["decoded"],
                Err(message) => row["accepted"] == false && Some(message.as_str()) == row["error"].as_str(),
            };
            if !same {
                mismatches.push(format!("{name} {}\n  original: {}\n  port:     {}", row["input"], if row["accepted"] == true { format!("accepts {}", row["decoded"]) } else { format!("refuses {}", row["error"]) }, match &actual { Ok(admitted) => format!("accepts {admitted}"), Err(message) => format!("refuses {message:?}") }));
            }
        }
        // Every tool is covered here, except the four with their own witness below and the three image tools the original registers by hand with default decode options.
        let skipped = ["preview_snapshot", "html_preview", "device_screenshot", "t3_thread_list", "t3_thread_read", "t3_thread_wait", "t3_thread_interrupt"];
        for name in decoder::tool_names() {
            assert!(seen.contains(name) || skipped.contains(&name), "{name} has no generated cases");
        }
        assert!(count > 4000);
        if std::env::var("T3_PRINT_PARAMETER_MISMATCHES").is_ok() {
            eprintln!("{} of {count} differ\n{}", mismatches.len(), mismatches.join("\n"));
        }
        assert!(mismatches.is_empty(), "{} of {count} generated cases differ; first:\n{}", mismatches.len(), mismatches.iter().take(5).cloned().collect::<Vec<_>>().join("\n"));
    }

    /// The schemas hold code the port cannot read as data; each function is ported by source hash.
    #[test]
    fn every_function_the_schemas_hold_is_ported() {
        let mut known: Vec<&str> = crate::mcp_tool_functions::FILTERS.iter().chain(crate::mcp_tool_functions::TRANSFORMS).copied().collect();
        known.sort_unstable();
        let mut held: Vec<&str> = decoder::function_hashes().iter().map(|(hash, _, _)| *hash).collect();
        held.sort_unstable();
        assert_eq!(held, known, "regenerate with tools/generate_mcp_tool_schemas.mjs and port the changed functions");
        assert!(decoder::unrunnable_patterns().is_empty(), "{:?}", decoder::unrunnable_patterns());
    }

    /// The first four thread tools have their own witness, `tools/generate_mcp_control_parameters.mjs`,
    /// which runs them through the toolkit registration with the access layer.
    #[test]
    fn original_registered_thread_tool_raw_parameters() {
        let (mut count, mut failures) = (0, Vec::new());
        for line in include_str!("../tests/fixtures/mcp-control-parameters.jsonl").split('\n').filter(|line| !line.is_empty()) {
            let row: Value = serde_json::from_str(line).unwrap();
            match decode(row["name"].as_str().unwrap(), row["input"].clone()) {
                Ok(decoded) if row["accepted"] == true && decoded == row["decoded"] => {}
                Err(error) if row["accepted"] == false && error == row["error"] => {}
                result => failures.push(format!("{count}: {row}; actual {result:?}")),
            }
            count += 1;
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert_eq!(count, 427);
    }

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
