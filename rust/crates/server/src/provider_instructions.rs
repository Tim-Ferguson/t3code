//! Source application prompt text. These strings are sent to providers, never
//! interpreted as instructions for the server itself.
use serde_json::{Value, json};
use std::sync::OnceLock;

fn text(key: &str) -> &'static str {
    static DATA: OnceLock<Value> = OnceLock::new();
    DATA.get_or_init(|| serde_json::from_str(include_str!("provider-instructions.json")).unwrap())
        [key]
        .as_str()
        .unwrap()
}
fn single_line(value: &str) -> String {
    let mut output = String::new();
    let mut whitespace = false;
    for c in value.chars() {
        let mut bytes = [0; 4];
        if t3_contracts::trim_wire_string(c.encode_utf8(&mut bytes)).is_empty() {
            whitespace = true;
        } else {
            if whitespace && !output.is_empty() {
                output.push(' ');
            }
            output.push(c);
            whitespace = false;
        }
    }
    output
}
pub(crate) fn runtime(
    harness: &str,
    model: &str,
    model_name: Option<&str>,
    effort: Option<&str>,
) -> String {
    let harness = single_line(harness);
    let model = single_line(model);
    let name = single_line(model_name.unwrap_or(""));
    let effort = single_line(effort.unwrap_or(""));
    let label = if !name.is_empty() && name != model {
        format!("{name} (model slug: {model})")
    } else {
        model.clone()
    };
    let model_info = if !model.is_empty() && model != "auto" && model != "default" {
        format!(", as {label}")
    } else {
        String::new()
    };
    let effort_info = if effort.is_empty() {
        String::new()
    } else {
        format!(" with {effort} reasoning effort")
    };
    format!(
        "<runtime_info>In case you're asked: you are running in T3 Code through the {harness} harness{model_info}{effort_info}. No need to mention this otherwise. You can embed images and videos in your response using Markdown with absolute file paths.</runtime_info>\n\n{}",
        text("runtimeSuffix")
    )
}
pub(crate) fn codex_context(model: &str, effort: &str, browser: bool, device: bool) -> Value {
    let mut context = json!({
        "t3_code_orchestration":{"kind":"application","value":text("orchestration")},
        "t3_code_runtime":{"kind":"application","value":runtime("Codex",model,None,Some(effort))},
    });
    let mut tools = Vec::new();
    if browser {
        tools.push(text("browser"));
    }
    if device {
        tools.push(text("device"));
    }
    if !tools.is_empty() {
        context["t3_code_tools"] = json!({"kind":"application","value":tools.join("\n\n")});
    }
    context
}
pub(crate) fn codex_mode(plan: bool) -> &'static str {
    text(if plan { "codexPlan" } else { "codexDefault" })
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct AcpState {
    pub(crate) plan: bool,
    pub(crate) mcp: bool,
}
pub(crate) fn acp_prompt(prompt: &str, state: AcpState, previous: Option<AcpState>) -> String {
    if t3_contracts::trim_wire_string(prompt).starts_with('/') || previous == Some(state) {
        return prompt.into();
    }
    let mut instructions = vec![text(if state.plan { "acpPlan" } else { "acpDefault" })];
    if state.mcp {
        instructions.push(t3_contracts::trim_wire_string(text("browser")));
        instructions.push(t3_contracts::trim_wire_string(text("orchestration")));
    }
    format!(
        "<t3_code_instructions>\n{}\n</t3_code_instructions>\n\n<user_request>\n{prompt}\n</user_request>",
        instructions.join("\n\n")
    )
}
pub(crate) fn device_environment(
    base: &mut std::collections::HashMap<String, String>,
    extra: Option<&indexmap::IndexMap<String, String>>,
) {
    let Some(extra) = extra else {
        return;
    };
    let path = base.get("PATH").or_else(|| base.get("Path")).cloned();
    for (key, value) in extra {
        if key != "PATH" && key != "PATH_SEPARATOR" {
            base.insert(key.clone(), value.clone());
        }
    }
    if let Some(shim) = extra.get("PATH").filter(|value| !value.is_empty()) {
        let separator = extra
            .get("PATH_SEPARATOR")
            .map(String::as_str)
            .unwrap_or(":");
        let path = path
            .filter(|value| !value.is_empty())
            .map_or_else(|| shim.clone(), |value| format!("{shim}{separator}{value}"));
        base.insert("PATH".into(), path);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_source_instruction_and_optional_device_environment_oracle() {
        let rows: Vec<Value> =
            serde_json::from_str(include_str!("../tests/fixtures/provider-instructions.json"))
                .unwrap();
        assert_eq!(rows.len(), 296);
        for (index, row) in rows.iter().enumerate() {
            let input = &row["input"];
            let output = match row["operation"].as_str().unwrap() {
                "runtime" => json!(runtime(
                    input["harness"].as_str().unwrap(),
                    input["model"].as_str().unwrap(),
                    input["modelName"].as_str(),
                    input["reasoningEffort"].as_str()
                )),
                "codexContext" => codex_context(
                    input["model"].as_str().unwrap(),
                    input["reasoningEffort"].as_str().unwrap(),
                    input["browser"].as_bool().unwrap(),
                    input["device"].as_bool().unwrap(),
                ),
                "acpPrompt" => {
                    let state = |value: &Value| AcpState {
                        plan: value["interactionMode"] == "plan",
                        mcp: value["hasT3Mcp"] == true,
                    };
                    json!(acp_prompt(
                        input["prompt"].as_str().unwrap(),
                        state(&input["state"]),
                        input.get("previousState").map(state)
                    ))
                }
                "deviceEnvironment" => {
                    let mut base = serde_json::from_value(input["base"].clone()).unwrap();
                    let extra: Option<indexmap::IndexMap<String, String>> = input
                        .get("extra")
                        .map(|value| serde_json::from_value(value.clone()).unwrap());
                    device_environment(&mut base, extra.as_ref());
                    serde_json::to_value(base).unwrap()
                }
                _ => panic!("unknown operation"),
            };
            assert_eq!(
                output, row["output"],
                "source case {index}: {}",
                row["operation"]
            );
        }
    }
}
