//! ACP presentation at the provider boundary. Binary payloads never enter chat text.
use serde_json::{Value, json};
use t3_contracts::ProviderRuntimeEvent;

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
pub fn thought_delta(content: &Value) -> Option<&str> {
    (content["type"] == "text")
        .then(|| content["text"].as_str())
        .flatten()
        .filter(|text| !text.is_empty())
}
/// Plan payload normalization from the original ACP session-update parser.
pub fn plan_update(update: &Value) -> Option<Value> {
    let steps = |entries: &Value| {
        entries.as_array().into_iter().flatten().enumerate().filter_map(|(index,entry)| {
            let content=entry["content"].as_str()?;
            let content=t3_contracts::trim_wire_string(content);
            Some(json!({"step":if content.is_empty(){format!("Step {}",index+1)}else{content.into()},"status":match text(&entry["status"]){"completed"=>"completed","in_progress"|"inProgress"=>"inProgress",_=>"pending"}}))
        }).collect::<Vec<_>>()
    };
    match text(&update["sessionUpdate"]) {
        "plan" => {
            let plan = steps(&update["entries"]);
            (!plan.is_empty()).then(|| json!({"nativePlanId":"legacy","kind":"items","plan":plan}))
        }
        "plan_update" => {
            let plan = &update["plan"];
            let id = t3_contracts::trim_wire_string(text(&plan["planId"]));
            if id.is_empty() {
                return None;
            }
            Some(match text(&plan["type"]) {
                "items" if plan["entries"].is_array() => {
                    json!({"nativePlanId":id,"kind":"items","plan":steps(&plan["entries"])})
                }
                "markdown" if plan["content"].is_string() => {
                    json!({"nativePlanId":id,"kind":"markdown","markdown":plan["content"]})
                }
                "file" if plan["uri"].is_string() => {
                    json!({"nativePlanId":id,"kind":"file","uri":plan["uri"]})
                }
                _ => json!({"nativePlanId":id,"kind":"unknown","contentType":plan["type"]}),
            })
        }
        "plan_removed" => {
            let id = t3_contracts::trim_wire_string(text(&update["planId"]));
            (!id.is_empty()).then(|| json!({"nativePlanId":id,"kind":"removed"}))
        }
        _ => None,
    }
}
fn bounded(value: &Value, maximum: usize, trim: bool) -> String {
    let value = text(value);
    let value = if trim {
        t3_contracts::trim_wire_string(value)
    } else {
        value
    };
    // JavaScript slice uses UTF-16 code units. An isolated final surrogate is
    // replaced at the Rust string boundary, matching browser display text.
    String::from_utf16_lossy(&value.encode_utf16().take(maximum).collect::<Vec<_>>())
}
fn uri(value: &Value) -> String {
    let value = bounded(value, 4096, true);
    if value.to_ascii_lowercase().starts_with("data:") {
        String::new()
    } else {
        value
    }
}
pub fn content_display_text(content: &Value) -> Option<String> {
    let label = |value: &Value, maximum| bounded(value, maximum, true);
    Some(match text(&content["type"]) {
        "text" => bounded(&content["text"], 65536, false),
        "resource_link" => {
            let title = label(&content["title"], 512);
            let name = label(&content["name"], 512);
            let title = if !title.is_empty() {
                title
            } else if !name.is_empty() {
                name
            } else {
                "resource".into()
            };
            let description = label(&content["description"], 2048);
            let title = if description.is_empty() {
                title
            } else {
                format!("{title}: {description}")
            };
            let uri = uri(&content["uri"]);
            if uri.is_empty() {
                title
            } else {
                format!("{title}\n{uri}")
            }
        }
        "resource" if content["resource"].get("text").is_some() => {
            bounded(&content["resource"]["text"], 65536, false)
        }
        "resource" => {
            let mime = label(&content["resource"]["mimeType"], 256);
            let uri = uri(&content["resource"]["uri"]);
            format!(
                "[ACP binary resource{}{}]",
                if mime.is_empty() {
                    String::new()
                } else {
                    format!(" ({mime})")
                },
                if uri.is_empty() {
                    String::new()
                } else {
                    format!(": {uri}")
                }
            )
        }
        "image" => {
            let mime = label(&content["mimeType"], 256);
            let mime = if mime.is_empty() {
                "unknown type"
            } else {
                &mime
            };
            let uri = uri(&content["uri"]);
            format!(
                "[ACP image ({mime}){}]",
                if uri.is_empty() {
                    String::new()
                } else {
                    format!(": {uri}")
                }
            )
        }
        "audio" => {
            let mime = label(&content["mimeType"], 256);
            format!(
                "[ACP audio ({})]",
                if mime.is_empty() {
                    "unknown type"
                } else {
                    &mime
                }
            )
        }
        "_t3_unknown" => {
            let kind = label(&content["originalType"], 128);
            format!(
                "[Unsupported ACP content: {}]",
                if kind.is_empty() { "unknown" } else { &kind }
            )
        }
        _ => return None,
    })
}
pub fn canonical_item_type(kind: &str) -> &'static str {
    match kind {
        "execute" => "command_execution",
        "edit" | "delete" | "move" => "file_change",
        "search" | "fetch" => "web_search",
        _ => "dynamic_tool_call",
    }
}
pub fn canonical_request_type(kind: &str) -> &'static str {
    match kind {
        "execute" => "exec_command_approval",
        "read" => "file_read_approval",
        "edit" | "delete" | "move" => "file_change_approval",
        _ => "dynamic_tool_call",
    }
}
fn flat_choices(options: &Value) -> Vec<&Value> {
    options
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|candidate| {
            if candidate.get("value").is_some() {
                vec![candidate]
            } else {
                candidate["options"]
                    .as_array()
                    .map(|values| values.iter().collect())
                    .unwrap_or_default()
            }
        })
        .collect()
}
fn opaque(value: &Value, maximum: usize) -> Option<&str> {
    value.as_str().filter(|value| {
        !value.is_empty()
            && *value == t3_contracts::trim_wire_string(value)
            && value.encode_utf16().count() <= maximum
    })
}
fn select_choices(candidates: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut result = vec![];
    for candidate in candidates {
        let Some(id) = opaque(&candidate["value"], 256) else {
            continue;
        };
        if !seen.insert(id.to_owned()) {
            continue;
        }
        let label = bounded(&candidate["name"], 256, true);
        let description = bounded(&candidate["description"], 1024, true);
        let mut choice = json!({"id":id,"label":if label.is_empty(){id}else{&label}});
        if !description.is_empty() {
            choice["description"] = json!(description);
        }
        result.push(choice);
        if result.len() == 64 {
            break;
        }
    }
    result
}
pub fn session_mode_state(setup: &Value) -> Option<Value> {
    let mode_config = setup["configOptions"].as_array().and_then(|options| {
        options
            .iter()
            .find(|option| option["category"] == "mode" && option["type"] == "select")
    });
    let generated=mode_config.map(|option|json!({"currentModeId":option["currentValue"],"availableModes":flat_choices(&option["options"]).iter().map(|value|json!({"id":value["value"],"name":value["name"],"description":value["description"]})).collect::<Vec<_>>()}));
    let modes = setup
        .get("modes")
        .filter(|value| !value.is_null())
        .or(generated.as_ref())?;
    let current = t3_contracts::trim_wire_string(text(&modes["currentModeId"]));
    if current.is_empty() {
        return None;
    }
    let available = modes["availableModes"]
        .as_array()?
        .iter()
        .filter_map(|mode| {
            let id = t3_contracts::trim_wire_string(text(&mode["id"]));
            let name = t3_contracts::trim_wire_string(text(&mode["name"]));
            if id.is_empty() || name.is_empty() {
                return None;
            }
            let mut value = json!({"id":id,"name":name});
            let description = t3_contracts::trim_wire_string(text(&mode["description"]));
            if !description.is_empty() {
                value["description"] = json!(description);
            }
            Some(value)
        })
        .collect::<Vec<_>>();
    (!available.is_empty()).then(|| json!({"currentModeId":current,"availableModes":available}))
}
pub fn option_descriptors(setup: &Value) -> Vec<Value> {
    let mut descriptors = vec![];
    let mut seen = std::collections::HashSet::new();
    let mut mirrored = vec![];
    let mut has_mode = false;
    for option in setup["configOptions"].as_array().into_iter().flatten() {
        if matches!(text(&option["category"]), "model" | "collaboration_mode") {
            continue;
        }
        let Some(id) = opaque(&option["id"], 256) else {
            continue;
        };
        if !seen.insert(id.to_owned()) {
            continue;
        }
        let label = bounded(&option["name"], 256, true);
        let description = bounded(&option["description"], 1024, true);
        let mut descriptor = json!({"id":id,"label":if label.is_empty(){id}else{&label}});
        if !description.is_empty() {
            descriptor["description"] = json!(description);
        }
        if option["type"] == "boolean" {
            if option["category"] == "mode" {
                has_mode = true;
            }
            descriptor["type"] = json!("boolean");
            descriptor["currentValue"] = option["currentValue"].clone();
        } else {
            let choices = select_choices(flat_choices(&option["options"]).into_iter().cloned());
            if choices.is_empty() {
                seen.remove(id);
                continue;
            }
            if option["category"] == "mode" {
                has_mode = true;
            }
            if option["category"] == "thought_level" {
                mirrored.push(
                    choices
                        .iter()
                        .map(|choice| text(&choice["id"]).to_owned())
                        .collect::<std::collections::HashSet<_>>(),
                );
            }
            descriptor["type"] = json!("select");
            if choices
                .iter()
                .any(|choice| choice["id"] == option["currentValue"])
            {
                descriptor["currentValue"] = option["currentValue"].clone();
            }
            descriptor["options"] = json!(choices);
        }
        descriptors.push(descriptor);
        if descriptors.len() == 16 {
            return descriptors;
        }
    }
    if !has_mode {
        if let Some(modes) = session_mode_state(setup) {
            let choices=select_choices(modes["availableModes"].as_array().unwrap().iter().map(|mode|json!({"value":mode["id"],"name":mode["name"],"description":mode["description"]})));
            let duplicate = mirrored.iter().any(|ids| {
                ids.len() == choices.len()
                    && choices
                        .iter()
                        .all(|choice| ids.contains(text(&choice["id"])))
            });
            if choices.len() > 1 && !duplicate && !seen.contains("_t3/session-mode") {
                let mut descriptor = json!({"id":"_t3/session-mode","label":"Mode","description":"Session mode advertised by the ACP agent.","type":"select","options":choices});
                if descriptor["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|choice| choice["id"] == modes["currentModeId"])
                {
                    descriptor["currentValue"] = modes["currentModeId"].clone();
                }
                descriptors.push(descriptor);
            }
        }
    }
    descriptors.truncate(16);
    descriptors
}
/// Source registry normalization, distinct from the presentation catalog's
/// synthetic Default model and user custom models.
pub fn live_configuration(setup: &Value) -> crate::acp_coordinator::LiveConfiguration {
    let options = &setup["configOptions"];
    let current = options
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["category"] == "model" && option["type"] == "select")
        .and_then(|option| opaque(&option["currentValue"], 128));
    let mut seen = std::collections::HashSet::new();
    let mut models = vec![];
    for candidate in options
        .as_array()
        .into_iter()
        .flatten()
        .filter(|option| option["category"] == "model" && option["type"] == "select")
        .flat_map(|option| flat_choices(&option["options"]))
    {
        let Some(id) = opaque(&candidate["value"], 128) else {
            continue;
        };
        if !seen.insert(id.to_owned()) {
            continue;
        }
        let name = bounded(&candidate["name"], 160, true);
        let description = bounded(&candidate["description"], 1024, true);
        models.push(json!({"id":id,"name":if name.is_empty(){id}else{&name},"description":if description.is_empty(){Value::Null}else{json!(description)}}));
        if models.len() == 256 {
            break;
        }
    }
    let current = current.filter(|id| models.iter().any(|model| model["id"] == *id));
    serde_json::from_value(
        json!({"models":models,"currentModelId":current,"configOptions":option_descriptors(setup)}),
    )
    .expect("source-bounded ACP live configuration")
}
pub fn available_commands(commands: &Value) -> crate::acp_coordinator::AvailableCommands {
    let mut seen = std::collections::HashSet::new();
    let mut slash_commands = vec![];
    let mut skills = vec![];
    let mut accepted = 0;
    for command in commands.as_array().into_iter().flatten() {
        let Some(name) = opaque(&command["name"], 128) else {
            continue;
        };
        if !seen.insert(name.to_lowercase()) {
            continue;
        }
        let description = bounded(&command["description"], 1024, true);
        let hint = bounded(&command["input"]["hint"], 1024, true);
        if let Some(name) = name.strip_prefix('$') {
            if name.is_empty() || name != t3_contracts::trim_wire_string(name) {
                continue;
            }
            // encodeURIComponent's exact safe bytes, rather than URL form encoding.
            let encoded = name
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                        (byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect::<String>();
            let mut skill = json!({"name":name,"path":format!("acp://skill/{encoded}"),"scope":"agent","enabled":true});
            if !description.is_empty() {
                skill["description"] = json!(description);
            }
            skills.push(skill);
        } else {
            let mut slash = json!({"name":name});
            if !description.is_empty() {
                slash["description"] = json!(description);
            }
            if !hint.is_empty() {
                slash["input"] = json!({"hint":hint});
            }
            slash_commands.push(slash);
        }
        accepted += 1;
        if accepted == 128 {
            break;
        }
    }
    serde_json::from_value(json!({"slashCommands":slash_commands,"skills":skills}))
        .expect("source-bounded ACP command advertisement")
}
pub fn models_from_live_configuration(
    configuration: &crate::acp_coordinator::LiveConfiguration,
    custom: &[String],
) -> Vec<Value> {
    let capabilities = json!({"optionDescriptors":configuration.config_options});
    let mut seen = std::collections::HashSet::new();
    let mut models = configuration
        .models
        .iter()
        .map(|model| {
            let id = model.id.0.to_string();
            seen.insert(id.clone());
            let mut output =
                json!({"slug":id,"name":model.name,"isCustom":false,"capabilities":capabilities});
            if configuration.current_model_id.as_deref() == Some(id.as_str()) {
                output["isDefault"] = json!(true);
            }
            output
        })
        .collect::<Vec<_>>();
    if models.is_empty() {
        seen.insert("default".into());
        models.push(json!({"slug":"default","name":"Default","isCustom":false,"isDefault":true,"capabilities":capabilities}));
    }
    for id in custom {
        let id = t3_contracts::trim_wire_string(id);
        if !id.is_empty() && seen.insert(id.to_owned()) {
            models.push(json!({"slug":id,"name":id,"isCustom":true,"capabilities":capabilities}));
        }
    }
    models
}
pub fn discovered_models(setup: &Value, custom: &[String]) -> Vec<Value> {
    models_from_live_configuration(&live_configuration(setup), custom)
}
/// The same canonical events consumed by every provider adapter.
pub fn core_event(
    operation: &str,
    input: &Value,
) -> Result<ProviderRuntimeEvent, serde_json::Error> {
    let mut result = input["stamp"].clone();
    let object = result.as_object_mut().expect("event stamp is an object");
    for field in ["provider", "threadId", "turnId"] {
        if let Some(value) = input.get(field) {
            object.insert(field.into(), value.clone());
        }
    }
    let raw =
        || json!({"source":input["source"],"method":input["method"],"payload":input["rawPayload"]});
    match operation {
        "request.opened" => {
            result["type"] = json!(operation);
            result["requestId"] = input["requestId"].clone();
            result["payload"] = json!({"requestType":canonical_request_type(text(&input["permissionRequest"]["kind"])),"detail":input["detail"],"args":input["args"]});
            if let Some(options) = input.get("approvalOptions") {
                result["payload"]["options"] = options.clone();
            }
            result["raw"] = raw();
        }
        "request.resolved" => {
            result["type"] = json!(operation);
            result["requestId"] = input["requestId"].clone();
            result["payload"] = json!({"requestType":canonical_request_type(text(&input["permissionRequest"]["kind"])),"decision":input["decision"]});
        }
        "turn.plan.updated" => {
            result["type"] = json!(operation);
            let payload = &input["payload"];
            result["payload"] = match text(&payload["kind"]) {
                "items" => {
                    let mut value = json!({"plan":payload["plan"]});
                    if let Some(explanation) =
                        payload.get("explanation").filter(|value| !value.is_null())
                    {
                        value["explanation"] = explanation.clone();
                    }
                    value
                }
                "removed" => json!({"plan":[]}),
                kind => {
                    json!({"plan":[{"step":match kind {"markdown"=>text(&payload["markdown"]).to_owned(),"file"=>format!("Plan file: {}",text(&payload["uri"])),_=>format!("[Unsupported ACP plan content: {}]",text(&payload["contentType"]))},"status":"pending"}]})
                }
            };
            result["raw"] = raw();
        }
        "tool" => {
            let tool = &input["toolCall"];
            let status = text(&tool["status"]);
            result["type"] = json!(if matches!(status, "completed" | "failed") {
                "item.completed"
            } else {
                "item.updated"
            });
            result["itemId"] = tool["toolCallId"].clone();
            result["payload"] = json!({"itemType":canonical_item_type(text(&tool["kind"]))});
            if let Some(status) = match status {
                "pending" | "inProgress" => Some("inProgress"),
                "completed" => Some("completed"),
                "failed" => Some("failed"),
                _ => None,
            } {
                result["payload"]["status"] = json!(status);
            }
            for field in ["title", "detail"] {
                if !text(&tool[field]).is_empty() {
                    result["payload"][field] = tool[field].clone();
                }
            }
            if tool["data"]
                .as_object()
                .is_some_and(|value| !value.is_empty())
            {
                result["payload"]["data"] = tool["data"].clone();
            }
            result["raw"] = json!({"source":"acp.jsonrpc","method":"session/update","payload":input["rawPayload"]});
        }
        "assistant" => {
            result["type"] = input["lifecycle"].clone();
            result["itemId"] = input["itemId"].clone();
            result["payload"] = json!({"itemType":"assistant_message","status":if input["lifecycle"]=="item.completed"{"completed"}else{"inProgress"}});
        }
        "content.delta" => {
            result["type"] = json!(operation);
            if !text(&input["itemId"]).is_empty() {
                result["itemId"] = input["itemId"].clone();
            }
            result["payload"] = json!({"streamKind":input.get("streamKind").cloned().unwrap_or(json!("assistant_text")),"delta":input["text"]});
            result["raw"] = json!({"source":"acp.jsonrpc","method":"session/update","payload":input["rawPayload"]});
        }
        _ => unreachable!("known ACP core event"),
    }
    serde_json::from_value(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_original_core_events_and_bounded_content_display() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/acp-model.jsonl").lines() {
            let fixture: Value = serde_json::from_str(line).unwrap();
            let operation = fixture["operation"].as_str().unwrap();
            let actual = match operation {
                "content" => json!(content_display_text(&fixture["input"])),
                "plan" => json!(plan_update(&fixture["input"])),
                "thought" => json!(thought_delta(&fixture["input"])),
                "live-configuration" => json!(live_configuration(&fixture["input"])),
                "commands" => json!(available_commands(&fixture["input"])),
                "catalog" => json!(discovered_models(
                    &fixture["input"]["setup"],
                    &serde_json::from_value::<Vec<String>>(fixture["input"]["custom"].clone())
                        .unwrap()
                )),
                "permission" => serde_json::to_value(
                    crate::acp_runtime::permission_response(
                        &serde_json::from_value(fixture["input"]["request"].clone()).unwrap(),
                        fixture["input"]["decision"].as_str().unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap(),
                _ => {
                    serde_json::to_value(core_event(operation, &fixture["input"]).unwrap()).unwrap()
                }
            };
            assert_eq!(
                actual, fixture["output"],
                "{operation}: {}",
                fixture["input"]
            );
            count += 1;
        }
        assert!(count >= 100);
    }
}
