use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub id: String,
    pub kind: String,
    pub detail: Option<String>,
    pub app_name: Option<String>,
    pub options: Vec<Value>,
    pub live: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub struct PendingUserInput {
    pub id: String,
    pub questions: Vec<Value>,
    pub response_capability: String,
    pub dismissible: bool,
}
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingRequests {
    pub approvals: Vec<PendingApproval>,
    pub user_inputs: Vec<PendingUserInput>,
}

/// Joins authoritative request status to timeline display data. Provider auth
/// refresh and dynamic tool calls have dedicated flows and are never approvals.
/// Mirrors packages/client-runtime/src/state/threadRequests.ts.
pub fn pending_requests(projection: &Value) -> PendingRequests {
    let mut result = PendingRequests::default();
    let items = projection["turnItems"].as_array();
    for request in projection["runtimeRequests"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if request["status"] != "pending" {
            continue;
        }
        let Some(id) = request["id"].as_str() else {
            continue;
        };
        let capability = request["responseCapability"]["type"]
            .as_str()
            .unwrap_or("not_resumable");
        let kind = request["kind"].as_str().unwrap_or("");
        if kind == "user_input" {
            let item = items.and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find(|item| item["type"] == "user_input_request" && item["requestId"] == id)
            });
            let Some(item) = item else {
                continue;
            };
            result.user_inputs.push(PendingUserInput {
                id: id.to_owned(),
                questions: item["questions"].as_array().cloned().unwrap_or_default(),
                response_capability: capability.to_owned(),
                dismissible: item["responseMode"] == "message" || capability == "message",
            });
        } else if !matches!(kind, "auth_refresh" | "dynamic_tool_call") {
            let item = items.and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find(|item| item["type"] == "approval_request" && item["requestId"] == id)
            });
            let options = match item.and_then(|item| item.get("options")) {
                Some(Value::Array(options)) => options.clone(),
                _ => vec![
                    json!({"decision":"accept","label":"Allow"}),
                    json!({"decision":"decline","label":"Decline"}),
                ],
            };
            result.approvals.push(PendingApproval {
                id: id.to_owned(),
                kind: kind.to_owned(),
                detail: item
                    .and_then(|item| item["prompt"].as_str())
                    .map(str::to_owned),
                app_name: item
                    .and_then(|item| item["appName"].as_str())
                    .map(str::to_owned),
                options,
                live: capability == "live",
            });
        }
    }
    result
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftAnswer {
    pub selected: Vec<String>,
    pub custom: String,
    pub attachment_count: usize,
    pub attachments_blocked: bool,
}

pub fn resolve_answer(question: &Value, draft: &DraftAnswer) -> Option<Value> {
    if draft.attachments_blocked {
        return None;
    }
    let custom_allowed = question["allowCustomAnswer"] != false;
    let custom = t3_contracts::trim_wire_string(&draft.custom);
    if custom_allowed && !custom.is_empty() {
        return Some(json!(custom));
    }
    let mut selected = Vec::new();
    for value in &draft.selected {
        if !selected.contains(value)
            && question["options"].as_array().is_some_and(|options| {
                options.iter().any(|option| {
                    option["value"]
                        .as_str()
                        .or_else(|| option["label"].as_str())
                        == Some(value)
                })
            })
        {
            selected.push(value.clone());
        }
    }
    if question["multiSelect"] == true {
        if !selected.is_empty() {
            Some(json!(selected))
        } else if custom_allowed && draft.attachment_count > 0 {
            Some(json!(""))
        } else {
            None
        }
    } else {
        selected
            .first()
            .map(|value| json!(value))
            .or_else(|| (custom_allowed && draft.attachment_count > 0).then(|| json!("")))
    }
}

pub fn build_answers(
    questions: &[Value],
    drafts: &std::collections::BTreeMap<String, DraftAnswer>,
) -> Option<Value> {
    let mut answers = serde_json::Map::new();
    for question in questions {
        let id = question["id"].as_str()?;
        let empty = DraftAnswer::default();
        answers.insert(
            id.to_owned(),
            resolve_answer(question, drafts.get(id).unwrap_or(&empty))?,
        );
    }
    Some(Value::Object(answers))
}

pub fn toggle_option(question: &Value, draft: &mut DraftAnswer, value: String) -> String {
    let displaced = t3_contracts::trim_wire_string(&draft.custom).to_owned();
    draft.custom.clear();
    if question["multiSelect"] == true {
        if draft.selected.contains(&value) {
            draft.selected.retain(|selected| selected != &value);
        } else {
            draft.selected.push(value);
        }
    } else {
        draft.selected = vec![value];
    }
    displaced
}

pub fn set_custom_answer(draft: &mut DraftAnswer, value: String) {
    if !t3_contracts::trim_wire_string(&value).is_empty() {
        draft.selected.clear();
    }
    draft.custom = value;
    draft.attachment_count = 0;
    draft.attachments_blocked = false;
}
