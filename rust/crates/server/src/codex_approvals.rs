//! What Codex asks the user, and how a decision becomes Codex's answer (the request handlers of
//! `CodexAdapterV2.ts` and `CodexMcpElicitation.ts`). Pure: the actor persists the request and
//! sends the response these functions build.
use serde_json::{Map, Value, json};

/// The part of a Codex request the user decides on.
pub(crate) struct Described {
    /// `command`, `file-read`, `file-change` or `mcp-elicitation`.
    pub request_kind: &'static str,
    pub native_item_id: String,
    pub native_request_id: String,
    pub prompt: Option<String>,
    pub app_name: Option<String>,
    pub options: Option<Vec<Value>>,
}

/// The wire shape a decision must answer, kept with the live callback.
#[derive(Clone, Debug)]
pub(crate) struct Callback {
    pub native_id: Value,
    pub method: String,
    pub params: Value,
}

fn text<'a>(value: &'a Value) -> Option<&'a str> {
    value.as_str().map(t3_contracts::trim_wire_string).filter(|text| !text.is_empty())
}

/// The thread a request belongs to: V2 requests name `threadId`, the legacy ones `conversationId`.
pub(crate) fn request_thread(params: &Value) -> Option<&str> {
    params["threadId"].as_str().or_else(|| params["conversationId"].as_str())
}
pub(crate) fn is_supported(method: &str) -> bool {
    matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "mcpServer/elicitation/request"
            | "execCommandApproval"
            | "applyPatchApproval"
    )
}

/// `codexFileChangeApprovalPrompt`: the reason, or the files the change touches, or its root.
pub(crate) fn file_change_prompt(params: &Value) -> Option<String> {
    if let Some(reason) = text(&params["reason"]) {
        return Some(reason.to_owned());
    }
    if let Some(changes) = params["fileChanges"].as_object().filter(|changes| !changes.is_empty()) {
        let mut entries: Vec<(&String, &Value)> = changes.iter().collect();
        entries.sort_by(|left, right| left.0.cmp(right.0));
        let described: Vec<String> = entries
            .iter()
            .take(20)
            .map(|(path, change)| match change["move_path"].as_str().filter(|_| change["type"] == "update") {
                Some(moved) if !moved.is_empty() => format!("{} {path} -> {moved}", change["type"].as_str().unwrap_or("")),
                _ => format!("{} {path}", change["type"].as_str().unwrap_or("")),
            })
            .collect();
        let remaining = entries.len().saturating_sub(described.len());
        return Some(if remaining > 0 { format!("{}\n+{remaining} more", described.join("\n")) } else { described.join("\n") });
    }
    text(&params["grantRoot"]).map(str::to_owned)
}

fn permissions_request_kind(permissions: &Value) -> &'static str {
    if permissions["fileSystem"]["write"].as_array().is_some_and(|paths| !paths.is_empty()) {
        "file-change"
    } else if permissions["fileSystem"]["read"].as_array().is_some_and(|paths| !paths.is_empty()) {
        "file-read"
    } else {
        "command"
    }
}

/// The choices the original's clients offer when a request names none of its own. Naming them
/// keeps the Rust client, which defaults to allow and decline, at the same set.
fn default_options() -> Vec<Value> {
    vec![json!({"decision":"cancel","label":"Cancel"}), json!({"decision":"decline","label":"Decline"}), json!({"decision":"acceptForSession","label":"Always allow this session"}), json!({"decision":"accept","label":"Approve"})]
}

/// What to show for one approval request; `None` for a request this adapter does not ask about.
pub(crate) fn describe(method: &str, params: &Value) -> Option<Described> {
    let id = |value: &Value| value.as_str().map(str::to_owned);
    Some(match method {
        "item/commandExecution/requestApproval" => {
            let item = id(&params["itemId"])?;
            Described { request_kind: "command", native_request_id: id(&params["approvalId"]).unwrap_or_else(|| item.clone()), native_item_id: item, prompt: params["reason"].as_str().or(params["command"].as_str()).map(str::to_owned), app_name: None, options: Some(default_options()) }
        }
        "item/fileChange/requestApproval" => {
            let item = id(&params["itemId"])?;
            Described { request_kind: "file-change", native_request_id: item.clone(), native_item_id: item, prompt: file_change_prompt(params), app_name: None, options: Some(default_options()) }
        }
        "item/permissions/requestApproval" => {
            let item = id(&params["itemId"])?;
            Described { request_kind: permissions_request_kind(&params["permissions"]), native_request_id: item.clone(), native_item_id: item, prompt: params["reason"].as_str().map(str::to_owned), app_name: None, options: Some(default_options()) }
        }
        "execCommandApproval" => {
            let call = id(&params["callId"])?;
            let command = params["command"].as_array().map(|parts| parts.iter().map(|part| part.as_str().unwrap_or("")).collect::<Vec<_>>().join(" "));
            Described { request_kind: "command", native_request_id: id(&params["approvalId"]).unwrap_or_else(|| call.clone()), native_item_id: call, prompt: params["reason"].as_str().map(str::to_owned).or(command), app_name: None, options: Some(default_options()) }
        }
        "applyPatchApproval" => {
            let call = id(&params["callId"])?;
            Described { request_kind: "file-change", native_request_id: call.clone(), native_item_id: call, prompt: file_change_prompt(params), app_name: None, options: Some(default_options()) }
        }
        "mcpServer/elicitation/request" => {
            let request = if params["mode"] == "url" { id(&params["elicitationId"])? } else { format!("mcp-elicitation:{}", params["serverName"].as_str().unwrap_or("")) };
            let (app_name, options) = describe_elicitation(params);
            Described { request_kind: "mcp-elicitation", native_item_id: request.clone(), native_request_id: request, prompt: params["message"].as_str().map(str::to_owned), app_name: Some(app_name), options: Some(options) }
        }
        _ => return None,
    })
}

// --- MCP elicitation -------------------------------------------------------------------------
fn persistence_decision(value: &str) -> Option<&'static str> {
    let normalized = value.to_lowercase();
    if normalized.contains("session") {
        Some("acceptForSession")
    } else if ["always", "permanent", "forever", "persistent"].iter().any(|word| normalized.contains(word)) {
        Some("acceptAlways")
    } else {
        None
    }
}
fn form(params: &Value) -> Option<&Value> {
    (params["mode"] != "url" && params["requestedSchema"].is_object()).then(|| &params["requestedSchema"])
}
fn field_options(field: &Value) -> Vec<(String, Option<String>)> {
    if let Some(one_of) = field["oneOf"].as_array() {
        return one_of.iter().filter_map(|option| Some((option["const"].as_str()?.to_owned(), option["title"].as_str().map(str::to_owned)))).collect();
    }
    field["enum"].as_array().into_iter().flatten().enumerate().filter_map(|(index, value)| Some((value.as_str()?.to_owned(), field["enumNames"][index].as_str().map(str::to_owned)))).collect()
}
fn is_persistence_field(key: &str, field: &Value) -> bool {
    persistence_decision(key).is_some() || key.to_lowercase() == "persist" || persistence_decision(field["title"].as_str().unwrap_or("")).is_some() || persistence_decision(field["description"].as_str().unwrap_or("")).is_some()
}

/// The app an elicitation is about and the approval choices it advertises.
fn describe_elicitation(params: &Value) -> (String, Vec<Value>) {
    let meta = &params["_meta"];
    let first = |candidates: [&Value; 8]| candidates.into_iter().find_map(|value| value.as_str().map(str::to_owned));
    let message_app = params["message"].as_str().and_then(|message| {
        let lower = message.to_lowercase();
        let rest = lower.strip_prefix("allow chatgpt to use ").and_then(|_| message.get("Allow ChatGPT to use ".len()..))?;
        rest.strip_suffix('?').filter(|name| !name.is_empty()).map(str::to_owned)
    });
    let app_name = first([&meta["app_name"], &meta["appName"], &meta["app"], &meta["target"]["app"], &meta["target"]["name"], &meta["tool_params"]["app_name"], &meta["tool_params"]["app"], &Value::Null])
        .or(message_app)
        .or_else(|| first([&meta["connector_name"], &meta["connectorName"], &params["serverName"], &Value::Null, &Value::Null, &Value::Null, &Value::Null, &Value::Null]))
        .unwrap_or_default();
    let mut persistence: Map<String, Value> = Map::new();
    let persist: Vec<&str> = match &meta["persist"] {
        Value::String(value) => vec![value.as_str()],
        Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    for value in persist {
        if let Some(decision) = persistence_decision(value) {
            persistence.insert(decision.into(), json!(""));
        }
    }
    if meta["allowPersistentApproval"] == true {
        persistence.insert("acceptAlways".into(), json!(""));
    }
    if let Some(form) = form(params) {
        for (key, field) in form["properties"].as_object().into_iter().flatten() {
            for (value, label) in field_options(field) {
                if let Some(decision) = persistence_decision(&value) {
                    persistence.insert(decision.into(), json!(label.unwrap_or_default()));
                }
            }
            if field["type"] == "boolean" && is_persistence_field(key, field) {
                persistence.insert("acceptAlways".into(), json!(field["title"].as_str().unwrap_or("")));
            }
        }
    }
    let mut options = vec![json!({"decision":"cancel","label":"Cancel"}), json!({"decision":"decline","label":"Decline"})];
    for (decision, fallback) in [("acceptForSession", "Always allow this session"), ("acceptAlways", "Always allow")] {
        if let Some(label) = persistence.get(decision) {
            if elicitation_response(params, decision)["action"] == "accept" {
                let label = label.as_str().filter(|label| !label.is_empty()).unwrap_or(fallback);
                options.push(json!({"decision":decision,"label":label}));
            }
        }
    }
    options.push(json!({"decision":"accept","label":"Approve"}));
    (app_name, options)
}

/// `toMcpElicitationResponse`: a decision as the MCP elicitation wire response.
pub(crate) fn elicitation_response(params: &Value, decision: &str) -> Value {
    if decision == "decline" || decision == "cancel" {
        return json!({"action":decision});
    }
    if params["mode"] == "url" {
        return json!({"action":"decline"});
    }
    let persist = match decision {
        "acceptForSession" => Some("session"),
        "acceptAlways" => Some("always"),
        _ => None,
    };
    let form = form(params);
    let mut content = Map::new();
    for (key, field) in form.and_then(|form| form["properties"].as_object()).into_iter().flatten() {
        let chosen = field_options(field).into_iter().find(|(value, _)| {
            if persist.is_some() {
                persistence_decision(value) == Some(decision)
            } else {
                let lower = value.to_lowercase();
                ["once", "accept", "approve", "allow"].iter().any(|word| lower.contains(word)) && persistence_decision(value).is_none()
            }
        });
        if let Some((value, _)) = chosen {
            content.insert(key.clone(), json!(value));
        } else if field["type"] == "boolean" && is_persistence_field(key, field) {
            content.insert(key.clone(), json!(decision == "acceptAlways"));
        } else if !field["default"].is_null() {
            content.insert(key.clone(), field["default"].clone());
        }
    }
    if form.is_some_and(|form| form["required"].as_array().is_some_and(|required| required.iter().filter_map(Value::as_str).any(|key| !content.contains_key(key)))) {
        return json!({"action":"decline"});
    }
    let mut response = json!({"action":"accept"});
    if let Some(persist) = persist {
        response["_meta"] = json!({"persist":persist});
    }
    if form.is_some() {
        response["content"] = Value::Object(content);
    }
    response
}

/// Whether an elicitation can be answered with an approval at all; one that cannot is declined
/// instead of being shown to the user.
pub(crate) fn elicitation_is_answerable(params: &Value) -> bool {
    elicitation_response(params, "accept")["action"] == "accept"
}

/// Codex's answer to an approval request for the user's decision.
pub(crate) fn response(callback: &Callback, decision: &str) -> Value {
    let session = decision == "acceptForSession" || decision == "acceptAlways";
    match callback.method.as_str() {
        "item/permissions/requestApproval" => {
            let granted = decision == "accept" || decision == "acceptForSession";
            json!({"permissions":if granted { callback.params["permissions"].clone() } else { json!({}) },"scope":if decision == "acceptForSession" {"session"} else {"turn"}})
        }
        "mcpServer/elicitation/request" => elicitation_response(&callback.params, decision),
        "execCommandApproval" | "applyPatchApproval" => {
            let review = match decision {
                "accept" => json!("approved"),
                _ if session => json!("approved_for_session"),
                "decline" => json!({"denied":{"rejection":"User declined the request."}}),
                _ => json!("abort"),
            };
            json!({"decision":review})
        }
        _ => json!({"decision":if decision == "acceptAlways" {"acceptForSession"} else {decision}}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_change_prompt_prefers_the_reason_then_the_files_then_the_root() {
        assert_eq!(file_change_prompt(&json!({"reason":" write it ","grantRoot":"/r"})).as_deref(), Some("write it"));
        let changes = json!({"fileChanges":{"b.txt":{"type":"add"},"a.txt":{"type":"update","move_path":"c.txt"}},"reason":" "});
        assert_eq!(file_change_prompt(&changes).as_deref(), Some("update a.txt -> c.txt\nadd b.txt"));
        assert_eq!(file_change_prompt(&json!({"grantRoot":"/r"})).as_deref(), Some("/r"));
        assert_eq!(file_change_prompt(&json!({})), None);
    }

    #[test]
    fn a_decision_answers_each_request_in_the_shape_codex_expects() {
        let call = |method: &str, params: Value| Callback { native_id: json!(1), method: method.into(), params };
        let command = call("item/commandExecution/requestApproval", json!({}));
        assert_eq!(response(&command, "acceptAlways"), json!({"decision":"acceptForSession"}));
        assert_eq!(response(&command, "decline"), json!({"decision":"decline"}));
        let permissions = call("item/permissions/requestApproval", json!({"permissions":{"fileSystem":{"write":["/a"]}}}));
        assert_eq!(response(&permissions, "acceptForSession"), json!({"permissions":{"fileSystem":{"write":["/a"]}},"scope":"session"}));
        assert_eq!(response(&permissions, "decline"), json!({"permissions":{},"scope":"turn"}));
        let legacy = call("execCommandApproval", json!({}));
        assert_eq!(response(&legacy, "accept"), json!({"decision":"approved"}));
        assert_eq!(response(&legacy, "acceptForSession"), json!({"decision":"approved_for_session"}));
        assert_eq!(response(&legacy, "cancel"), json!({"decision":"abort"}));
        assert_eq!(response(&legacy, "decline"), json!({"decision":{"denied":{"rejection":"User declined the request."}}}));
    }

    #[test]
    fn an_mcp_elicitation_offers_only_the_persistence_choices_it_can_honour() {
        let params = json!({"serverName":"gh","mode":"form","message":"Allow ChatGPT to use GitHub?","_meta":{"persist":["session","always"]},"requestedSchema":{"properties":{"decision":{"type":"string","oneOf":[{"const":"allow_once","title":"Once"},{"const":"allow_session","title":"This session"},{"const":"allow_always","title":"Always"}]}},"required":["decision"]}});
        let described = describe("mcpServer/elicitation/request", &params).unwrap();
        assert_eq!((described.app_name.as_deref(), described.native_request_id.as_str()), (Some("GitHub"), "mcp-elicitation:gh"));
        let decisions: Vec<&str> = described.options.as_ref().unwrap().iter().map(|option| option["decision"].as_str().unwrap()).collect();
        assert_eq!(decisions, ["cancel", "decline", "acceptForSession", "acceptAlways", "accept"]);
        assert_eq!(elicitation_response(&params, "accept"), json!({"action":"accept","content":{"decision":"allow_once"}}));
        assert_eq!(elicitation_response(&params, "acceptForSession"), json!({"action":"accept","_meta":{"persist":"session"},"content":{"decision":"allow_session"}}));
        // A form that needs input the user cannot give here is declined, not shown.
        let unanswerable = json!({"serverName":"x","mode":"form","message":"?","requestedSchema":{"properties":{"name":{"type":"string"}},"required":["name"]}});
        assert!(!elicitation_is_answerable(&unanswerable));
        assert!(!elicitation_is_answerable(&json!({"serverName":"x","mode":"url","elicitationId":"e","message":"open"})));
    }
}
