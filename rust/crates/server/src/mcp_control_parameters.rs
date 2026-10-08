//! Raw tool-schema admission, before invocation access or JSON codec decoding.
use serde_json::{Map, Value, json};
use t3_contracts::trim_wire_string;
#[derive(Clone, Copy)]
enum Kind {
    Text { trim: bool, max: Option<usize> },
    Bool,
    Int { min: f64, max: Option<f64> },
    Number,
    View,
    Statuses,
}
use Kind::*;
const ID: Kind = Text {
    trim: true,
    max: None,
};
const REASON: Kind = Text {
    trim: false,
    max: Some(2000),
};
const KEY: Kind = Text {
    trim: true,
    max: Some(256),
};
const NONNEGATIVE: Kind = Int {
    min: 0.0,
    max: None,
};
const LIMIT: Kind = Int {
    min: 1.0,
    max: Some(100.0),
};
const RUN_STATUSES: &[&str] = &[
    "preparing",
    "queued",
    "starting",
    "running",
    "waiting",
    "completed",
    "interrupted",
    "failed",
    "cancelled",
    "rolled_back",
];
pub(super) const NAMES: &[&str] = &[
    "t3_thread_list",
    "t3_thread_read",
    "t3_thread_wait",
    "t3_thread_interrupt",
];
fn fields(name: &str) -> &'static [(&'static str, Kind)] {
    match name {
        "t3_thread_list" => &[
            ("projectId", ID),
            ("statuses", Statuses),
            ("titleContains", KEY),
            ("settled", Bool),
            ("snoozed", Bool),
            ("includeSubagents", Bool),
            ("cursor", NONNEGATIVE),
            ("limit", LIMIT),
        ],
        "t3_thread_read" => &[
            ("threadId", ID),
            ("itemId", ID),
            ("textOffset", NONNEGATIVE),
            ("view", View),
            ("afterPosition", NONNEGATIVE),
            ("limit", LIMIT),
            (
                "runLimit",
                Int {
                    min: 1.0,
                    max: Some(50.0),
                },
            ),
            (
                "maxCharsPerItem",
                Int {
                    min: 1.0,
                    max: Some(50000.0),
                },
            ),
        ],
        "t3_thread_wait" => &[("threadId", ID), ("runId", ID), ("timeoutMs", Number)],
        "t3_thread_interrupt" => &[
            ("threadId", ID),
            ("runId", ID),
            ("reason", REASON),
            ("clientRequestId", KEY),
        ],
        _ => unreachable!(),
    }
}
fn expected(kind: Kind) -> &'static str {
    match kind {
        Text { .. } => "string",
        Bool => "boolean",
        Int { .. } | Number => "number",
        View => "\"messages\" | \"activity\"",
        Statuses => "array",
    }
}
fn check(kind: Kind, value: &Value, optional: bool) -> Result<Value, Vec<String>> {
    let mismatch = || {
        vec![format!(
            "Expected {}{}",
            expected(kind),
            if optional { " | undefined" } else { "" }
        )]
    };
    let mut errors = Vec::new();
    let result = match kind {
        Text { trim, max } => {
            let Some(text) = value.as_str() else {
                return Err(mismatch());
            };
            let text = if trim { trim_wire_string(text) } else { text };
            if trim && text.is_empty() {
                errors.push("Expected a non-blank string".into());
            }
            if max.is_some_and(|max| text.encode_utf16().count() > max) {
                errors.push(format!(
                    "Expected a value with a length of at most {}",
                    max.unwrap()
                ));
            }
            json!(text)
        }
        Bool if value.is_boolean() => value.clone(),
        Bool => return Err(mismatch()),
        Number if value.is_number() => value.clone(),
        Number => return Err(mismatch()),
        Int { min, max } => {
            let Some(number) = value.as_f64() else {
                return Err(mismatch());
            };
            if number.fract() != 0.0 || number.abs() > 9007199254740991.0 {
                errors.push("Expected an integer".into());
            }
            if number < min {
                errors.push(format!("Expected a value greater than or equal to {min}"));
            }
            if max.is_some_and(|max| number > max) {
                errors.push(format!(
                    "Expected a value less than or equal to {}",
                    max.unwrap()
                ));
            }
            if errors.is_empty() {
                json!(number as u64)
            } else {
                value.clone()
            }
        }
        View if value == "messages" || value == "activity" => value.clone(),
        View => {
            return Err(if value.is_string() {
                vec!["Expected \"messages\" | \"activity\"".into()]
            } else {
                mismatch()
            });
        }
        Statuses => {
            let Some(values) = value.as_array() else {
                return Err(mismatch());
            };
            if values.len() > 10 {
                return Err(vec!["Expected a value with a length of at most 10".into()]);
            }
            for (index, value) in values.iter().enumerate() {
                if value != "idle"
                    && !value
                        .as_str()
                        .is_some_and(|text| RUN_STATUSES.contains(&text))
                {
                    let literals = RUN_STATUSES
                        .iter()
                        .map(|value| format!("\"{value}\""))
                        .collect::<Vec<_>>()
                        .join(" | ");
                    errors.push(format!("Expected {literals}\n  at [INDEX:{index}]"));
                }
            }
            value.clone()
        }
    };
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}
pub(super) fn decode(name: &str, input: Value) -> Result<Value, String> {
    let input = if input.is_null() { json!({}) } else { input };
    let prefix = format!("Invalid parameters for tool '{name}': ");
    let Some(object) = input.as_object() else {
        return Err(format!("{prefix}Expected object"));
    };
    let mut decoded = Map::new();
    let mut errors = Vec::new();
    for &(key, kind) in fields(name) {
        let optional = key != "threadId";
        match object.get(key) {
            None if !optional => errors.push(format!("Missing key\n  at [\"{key}\"]")),
            None => {}
            Some(value) => match check(kind, value, optional) {
                Ok(value) => {
                    decoded.insert(key.into(), value);
                }
                Err(issues) => {
                    for issue in issues {
                        if let Some((issue, index)) = issue.split_once("\n  at [INDEX:") {
                            errors.push(format!("{issue}\n  at [\"{key}\"][{}", index));
                        } else {
                            errors.push(format!("{issue}\n  at [\"{key}\"]"));
                        }
                    }
                }
            },
        }
    }
    if errors.is_empty() {
        Ok(Value::Object(decoded))
    } else {
        Err(format!("{prefix}{}", errors.join("\n")))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_registered_thread_tool_raw_parameters() {
        let mut count = 0;
        let mut failures = Vec::new();
        for line in include_str!("../tests/fixtures/mcp-control-parameters.jsonl")
            .split('\n')
            .filter(|line| !line.is_empty())
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let result = decode(row["name"].as_str().unwrap(), row["input"].clone());
            match result {
                Ok(decoded) if row["accepted"] == true && decoded == row["decoded"] => {}
                Err(error) if row["accepted"] == false && error == row["error"] => {}
                result => failures.push(format!("{count}: {row}; actual {result:?}")),
            }
            count += 1;
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert_eq!(count, 427);
    }
}
