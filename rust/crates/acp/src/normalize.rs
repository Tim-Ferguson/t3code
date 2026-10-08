//! The compatibility transforms in the original effect-acp client.
use serde_json::{Map, Value, json};

fn copy(from: &Value, to: &mut Map<String, Value>, names: &[&str]) {
    for name in names {
        if let Some(v) = from.get(*name) {
            to.insert((*name).into(), v.clone());
        }
    }
}
fn present(value: Option<&Value>) -> bool {
    value.is_some_and(|v| !v.is_null())
}
fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
pub(crate) fn negotiating_initialize(request: Value) -> Value {
    let caps = &request["clientCapabilities"];
    let mut capabilities = Map::new();
    if caps.pointer("/auth/terminal") == Some(&Value::Bool(true)) {
        capabilities.insert("auth".into(), json!({"terminal":{}}));
    }
    if present(caps.get("elicitation")) {
        capabilities.insert("elicitation".into(), caps["elicitation"].clone());
    }
    copy(caps, &mut capabilities, &["_meta"]);
    let mut result = json!({"protocolVersion":2,"info":request.get("clientInfo").filter(|v|!v.is_null()).cloned().unwrap_or_else(||json!({"name":"t3-code","version":"unknown"})),"capabilities":capabilities,"clientCapabilities":request.get("clientCapabilities").cloned().unwrap_or_else(||json!({}))});
    copy(
        &request,
        result.as_object_mut().unwrap(),
        &["clientInfo", "_meta"],
    );
    result
}
pub(crate) fn initialize_response(response: Value) -> Value {
    let capabilities = &response["capabilities"];
    let session = &capabilities["session"];
    let prompt = &session["prompt"];
    let mcp = &session["mcp"];
    let mut result = json!({"protocolVersion":response["protocolVersion"],"agentInfo":response["info"],"agentCapabilities":{"loadSession":!session.is_null(),"promptCapabilities":{"image":present(prompt.get("image")),"audio":present(prompt.get("audio")),"embeddedContext":present(prompt.get("embeddedContext"))},"mcpCapabilities":{"stdio":present(mcp.get("stdio")),"http":present(mcp.get("http")),"acp":present(mcp.get("acp"))}}});
    let agent = result["agentCapabilities"].as_object_mut().unwrap();
    if !session.is_null() {
        let mut s = json!({"list":{},"resume":{},"close":{}});
        for name in ["delete", "fork", "additionalDirectories"] {
            if present(session.get(name)) {
                s[name] = json!({});
            }
        }
        agent.insert("sessionCapabilities".into(), s);
    }
    if present(capabilities.get("providers")) {
        agent.insert("providers".into(), json!({}));
    }
    if let Some(methods) = response.get("authMethods").and_then(Value::as_array) {
        if !methods.is_empty() {
            agent.insert("auth".into(), json!({"logout":{}}));
        }
        let methods = methods
            .iter()
            .filter_map(|method| {
                let mut base = json!({"id":method["methodId"],"name":method["name"]});
                copy(
                    method,
                    base.as_object_mut().unwrap(),
                    &["description", "_meta"],
                );
                match method["type"].as_str() {
                    Some("agent") => {
                        base["type"] = json!("agent");
                        Some(base)
                    }
                    Some("terminal") => {
                        base["type"] = json!("terminal");
                        if let Some(args) = method["args"].as_array() {
                            base["args"] = Value::Array(
                                args.iter().filter(|v| v.is_string()).cloned().collect(),
                            );
                        }
                        if let Some(env) = method["env"].as_array() {
                            base["env"] = Value::Object(
                                env.iter()
                                    .filter_map(|v| {
                                        Some((v["name"].as_str()?.into(), v["value"].clone()))
                                    })
                                    .collect(),
                            );
                        }
                        Some(base)
                    }
                    Some("env_var") => {
                        let vars = method["vars"].as_array()?;
                        if !vars.iter().all(|v| v["name"].is_string()) {
                            return None;
                        }
                        base["type"] = json!("env_var");
                        base["vars"] = Value::Array(
                            vars.iter()
                                .map(|v| {
                                    let mut var = json!({"name":v["name"]});
                                    if v["label"].is_string() {
                                        var["label"] = v["label"].clone();
                                    }
                                    var
                                })
                                .collect(),
                        );
                        if method["link"].is_string() {
                            base["link"] = method["link"].clone();
                        }
                        Some(base)
                    }
                    _ => None,
                }
            })
            .collect();
        result["authMethods"] = Value::Array(methods);
    }
    copy(&response, result.as_object_mut().unwrap(), &["_meta"]);
    result
}
pub(crate) fn config_option(option: Value) -> Option<Value> {
    match option["type"].as_str() {
        Some("select" | "boolean") => {
            let mut result = json!({"type":option["type"],"id":option["configId"],"name":option["name"],"currentValue":option["currentValue"]});
            copy(
                &option,
                result.as_object_mut().unwrap(),
                &["description", "category", "_meta"],
            );
            if option["type"] == "select" {
                result["options"] = option["options"].clone();
            }
            Some(result)
        }
        _ => None,
    }
}
pub(crate) fn setup_response(response: Value) -> Value {
    let mut result = Map::new();
    copy(&response, &mut result, &["sessionId", "_meta"]);
    if let Some(options) = response.get("configOptions").and_then(Value::as_array) {
        result.insert(
            "configOptions".into(),
            Value::Array(options.iter().cloned().filter_map(config_option).collect()),
        );
    }
    Value::Object(result)
}
pub(crate) fn mcp_servers(mut request: Value) -> Value {
    if let Some(servers) = request.get_mut("mcpServers").and_then(Value::as_array_mut) {
        for server in servers {
            if server.get("type").is_none() {
                server["type"] = json!("stdio");
            }
        }
    }
    request
}
pub(crate) fn content(content: Value) -> Value {
    match content["type"].as_str() {
        Some("text" | "image" | "audio" | "resource_link" | "resource") => content,
        _ => json!({"type":"_t3_unknown","originalType":content["type"],"raw":content}),
    }
}
fn tool_content(mut value: Value) -> Value {
    match value["type"].as_str() {
        Some("content") => {
            value["content"] = content(value["content"].take());
            value
        }
        Some("terminal" | "diff") => value,
        _ => json!({"type":"_t3_unknown","originalType":value["type"],"raw":value}),
    }
}
fn tool_update(value: Value) -> Value {
    let mut output = Map::new();
    copy(
        &value,
        &mut output,
        &[
            "toolCallId",
            "name",
            "title",
            "kind",
            "status",
            "locations",
            "rawInput",
            "rawOutput",
            "_meta",
        ],
    );
    if let Some(items) = value.get("content") {
        output.insert(
            "content".into(),
            match items.as_array() {
                Some(items) => Value::Array(items.iter().cloned().map(tool_content).collect()),
                None => items.clone(),
            },
        );
    }
    Value::Object(output)
}
pub(crate) fn notification(mut value: Value, v1: bool) -> Value {
    if v1 {
        if value["update"]["sessionUpdate"] == "available_commands_update" {
            if let Some(commands) = value["update"]["availableCommands"].as_array_mut() {
                for command in commands {
                    let input = command.get("input").cloned();
                    command.as_object_mut().unwrap().remove("input");
                    if let Some(input) = input.filter(|v| !v.is_null()) {
                        command["input"] = json!({"type":"text","hint":input["hint"]});
                    }
                }
            }
        }
        return value;
    }
    let update = value["update"].take();
    let mut result = object(&value);
    let tag = update["sessionUpdate"].as_str().unwrap_or("");
    let normalized = match tag {
        "user_message_chunk" | "agent_message_chunk" | "agent_thought_chunk" => {
            let mut u = update.clone();
            u["content"] = content(update["content"].clone());
            u
        }
        "user_message" | "agent_message" | "agent_thought" => {
            let mut u = update.clone();
            if let Some(contents) = update["content"].as_array() {
                u["content"] = Value::Array(contents.iter().cloned().map(content).collect());
            }
            u
        }
        "tool_call_update" => {
            let mut u = tool_update(update.clone());
            u["sessionUpdate"] = json!(tag);
            u
        }
        "tool_call_content_chunk" => {
            let mut u = update.clone();
            u["content"] = tool_content(update["content"].clone());
            u
        }
        "config_option_update" => {
            let mut u = update.clone();
            u["configOptions"] = Value::Array(
                update["configOptions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .cloned()
                    .filter_map(config_option)
                    .collect(),
            );
            u
        }
        "available_commands_update" => {
            let mut u = update.clone();
            for command in u["availableCommands"].as_array_mut().unwrap() {
                if command.pointer("/input/type").and_then(Value::as_str) != Some("text") {
                    command["input"] = Value::Null;
                }
            }
            u
        }
        "state_update"
        | "terminal_update"
        | "terminal_output_chunk"
        | "plan_update"
        | "plan_removed"
        | "session_info_update"
        | "usage_update"
        | "compaction_update"
        | "compaction_summary_chunk" => update.clone(),
        _ => json!({"sessionUpdate":"_t3_unknown","originalSessionUpdate":tag,"raw":update}),
    };
    result.insert("update".into(), normalized);
    Value::Object(result)
}
pub(crate) fn permission(request: Value, identity: &str) -> Value {
    if request.get("toolCall").is_some() {
        return request;
    }
    let subject = &request["subject"];
    let tool = if subject["type"] == "tool_call" && subject.get("toolCall").is_some() {
        tool_update(subject["toolCall"].clone())
    } else {
        json!({"toolCallId":if subject["type"]=="command"{subject["toolCallId"].as_str().unwrap_or(identity)}else{identity},"title":request["title"],"kind":if subject["type"]=="command"{"execute"}else{"other"}})
    };
    let mut result = object(&request);
    result.insert("toolCall".into(), tool);
    Value::Object(result)
}

#[cfg(test)]
mod source_parity {
    use super::*;
    #[test]
    fn compatibility_transforms_match_actual_original_client_functions() {
        for (index, line) in include_str!("../tests/fixtures/normalization.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let input = row["input"].clone();
            let output = match row["transform"].as_str().unwrap() {
                "toNegotiatingInitializeRequest" => negotiating_initialize(input),
                "normalizeInitializeResponse" => initialize_response(input),
                "normalizeConfigOption" => config_option(input).unwrap_or(Value::Null),
                "normalizeV2SessionSetupResponse" => setup_response(input),
                "normalizeSessionUpdate" => notification(input, false),
                "normalizeV1SessionUpdate" => notification(input, true),
                "normalizePermissionRequest" => {
                    permission(input, row["args"][0]["requestId"].as_str().unwrap())
                }
                "toV2McpServer" => {
                    let request = mcp_servers(json!({"mcpServers":[input]}));
                    request["mcpServers"][0].clone()
                }
                "toV2ResumeRequest" => {
                    let mut request = mcp_servers(input);
                    if let Some(replay) = row["args"].get(0) {
                        request["replayFrom"] = replay.clone();
                    }
                    request
                }
                "toV2ForkRequest" => mcp_servers(input),
                other => panic!("unknown original transform {other}"),
            };
            assert_eq!(
                output, row["output"],
                "source transform case {index}: {}",
                row["transform"]
            );
        }
    }
}
