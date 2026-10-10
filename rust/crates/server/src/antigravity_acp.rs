//! How T3 launches and talks to Google's Antigravity ACP agent: `AntigravityProtocol.ts`,
//! `AntigravityAcpSupport.ts` and the parts of `antigravityAuthSupport.ts` that shape the process. The
//! functions are pure so they can be checked against the original's (`generate_antigravity_fixtures.mjs`).
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// Shown when the agent asks for a Google sign-in during a normal launch.
pub const SIGN_IN_REQUIRED_MESSAGE: &str = "Sign in to Antigravity in Settings before you continue.";
/// The line the agent prints before the Google authorization URL.
pub const AUTH_STDOUT_PREFIX: &str = "Open the following link to authenticate the ACP server: ";
/// What the `BROWSER` helper writes to stderr before the JSON-encoded URL it was asked to open.
pub const AUTH_BROWSER_MARKER: &str = "__T3_ANTIGRAVITY_AUTH_URL__";
const MAX_AUTHORIZATION_URL_LENGTH: usize = 16_384;
const TOOL_TEXT_LIMIT: usize = 8_000;
const TOOL_TEXT_TRUNCATED: &str = "[Earlier output truncated]\n\n";
const QUESTION_LABEL_LIMIT: usize = 512;
const WARNING_TEXT_LIMIT: usize = 512;
/// The text a prompt of `/compact` sends and the compaction item it leaves.
pub const COMPACT_COMMAND: &str = "/compact";

fn trim(value: &str) -> &str {
    t3_contracts::trim_wire_string(value)
}
/// JavaScript `string.length`.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}
/// `copyBoundedText` of the first/last `count` UTF-16 units.
fn utf16_head(text: &str, count: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(count).collect::<Vec<_>>())
}
fn utf16_tail(text: &str, count: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    String::from_utf16_lossy(&units[units.len().saturating_sub(count)..])
}
/// `boundText`: the last `limit` units behind a truncation marker.
pub fn bound_text(text: &str, limit: usize) -> String {
    if js_len(text) <= limit { text.to_owned() } else { format!("{TOOL_TEXT_TRUNCATED}{}", utf16_tail(text, limit)) }
}
fn ellipsis(text: &str, limit: usize) -> String {
    if js_len(text) > limit { format!("{}...", utf16_head(text, limit - 3)) } else { text.to_owned() }
}

// ---- permission requests: approvals and questions ----

/// Native questions share the permission method, but their choices are not approvals.
pub fn is_user_input_request(request: &Value) -> bool {
    request["toolCall"]["toolCallId"].as_str().is_some_and(|id| id.starts_with("interaction_"))
}
fn option_with_kind<'a>(request: &'a Value, kind: &str) -> Option<&'a Value> {
    request["options"].as_array()?.iter().find(|option| option["kind"] == kind && option["optionId"].as_str().is_some_and(|id| !trim(id).is_empty()))
}
/// The agent marks "Allow Always" on shell and web tools with a prompt injection warning in `_meta`.
fn security_warning(option: &Value) -> Option<String> {
    let warning = option["_meta"].as_object()?.get("agy.security.warning")?.as_object()?;
    let field = |key: &str| match warning.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(text)) => Some(Some(trim(text).to_owned())),
        Some(_) => None,
    };
    let (title, message) = (field("title")?, field("message")?);
    let text = message.filter(|text| !text.is_empty()).or(title).filter(|text| !text.is_empty())?;
    Some(if js_len(&text) > WARNING_TEXT_LIMIT { format!("{}...", utf16_head(&text, WARNING_TEXT_LIMIT - 3)) } else { text })
}
/// `antigravityApprovalOptions`: only the decisions the native request can honor.
pub fn approval_options(request: &Value) -> Vec<Value> {
    if is_user_input_request(request) {
        return vec![];
    }
    let mut options = vec![];
    if option_with_kind(request, "allow_once").is_some() {
        options.push(json!({"decision":"accept","label":"Allow once"}));
    }
    if let Some(always) = option_with_kind(request, "allow_always") {
        let mut option = json!({"decision":"acceptForSession","label":"Allow for this thread"});
        if let Some(warning) = security_warning(always) {
            option["warning"] = json!(warning);
        }
        options.push(option);
    }
    if option_with_kind(request, "reject_once").is_some() {
        options.push(json!({"decision":"decline","label":"Deny"}));
    }
    options.push(json!({"decision":"cancel","label":"Cancel"}));
    options
}
fn question_label(option: &Value) -> String {
    let name = trim(option["name"].as_str().unwrap_or(""));
    let label = if name.is_empty() { option["optionId"].as_str().unwrap_or("") } else { name };
    ellipsis(label, QUESTION_LABEL_LIMIT)
}
/// `extractAntigravityUserInputQuestion`.
pub fn user_input_question(request: &Value) -> Option<Value> {
    let options = request["options"].as_array().filter(|options| !options.is_empty())?;
    if !is_user_input_request(request) {
        return None;
    }
    let mut ids = HashSet::new();
    for option in options {
        let id = option["optionId"].as_str()?;
        if trim(id).is_empty() || !ids.insert(id) {
            return None;
        }
    }
    let title = trim(request["toolCall"]["title"].as_str().unwrap_or(""));
    let question = ellipsis(if title.is_empty() { "Choose an option." } else { title }, TOOL_TEXT_LIMIT);
    Some(json!({
        "id":request["toolCall"]["toolCallId"],"header":"Question","question":question,"multiSelect":false,"allowCustomAnswer":false,
        "options":options.iter().map(|option| json!({"value":option["optionId"],"label":question_label(option),"description":question_label(option)})).collect::<Vec<_>>()
    }))
}
/// `makeAntigravityUserInputResponse`: `None` for an answer that is not one of the choices.
pub fn user_input_response(request: &Value, answers: &Value) -> Option<Value> {
    user_input_question(request)?;
    let answer = &answers[request["toolCall"]["toolCallId"].as_str()?];
    let value = match answer {
        Value::String(value) => value.as_str(),
        Value::Array(values) if values.len() == 1 => values[0].as_str()?,
        _ => return None,
    };
    let options = request["options"].as_array()?;
    if let Some(exact) = options.iter().find(|option| option["optionId"] == value) {
        return Some(json!({"outcome":{"outcome":"selected","optionId":exact["optionId"]}}));
    }
    let matching: Vec<&Value> = options.iter().filter(|option| question_label(option) == value).collect();
    (matching.len() == 1).then(|| json!({"outcome":{"outcome":"selected","optionId":matching[0]["optionId"]}}))
}

// ---- tool payloads ----

struct Budget {
    nodes: i64,
    text: i64,
}
fn is_data_image(text: &str) -> bool {
    text.get(..11).is_some_and(|head| head.eq_ignore_ascii_case("data:image/"))
}
fn sanitize_value(value: &Value, budget: &mut Budget, depth: usize) -> Option<Value> {
    let exhausted = budget.nodes <= 0;
    budget.nodes -= 1;
    if depth > 12 || exhausted {
        return None;
    }
    match value {
        Value::String(text) => {
            if is_data_image(text) || budget.text <= 0 {
                return None;
            }
            let bounded = bound_text(text, TOOL_TEXT_LIMIT.min(budget.text as usize));
            budget.text -= js_len(&bounded) as i64;
            Some(Value::String(bounded))
        }
        Value::Array(entries) => {
            let mut result = vec![];
            for entry in entries {
                if budget.nodes <= 0 {
                    break;
                }
                if let Some(sanitized) = sanitize_value(entry, budget, depth + 1) {
                    result.push(sanitized);
                }
            }
            Some(Value::Array(result))
        }
        Value::Object(object) => {
            let mut result = Map::new();
            for (key, entry) in object {
                if budget.nodes <= 0 {
                    break;
                }
                let image_data = object.get("type").is_some_and(|kind| kind == "image") && matches!(key.as_str(), "data" | "blob");
                let image_blob = key == "blob" && object.get("mimeType").and_then(Value::as_str).is_some_and(|mime| mime.starts_with("image/"));
                let duplicate_output = matches!(key.as_str(), "formatted_output" | "formattedOutput") && entry.is_string() && (object.get("combinedOutput") == Some(entry) || object.get("combined_output") == Some(entry));
                if image_data || image_blob || duplicate_output {
                    continue;
                }
                if let Some(sanitized) = sanitize_value(entry, budget, depth + 1) {
                    result.insert(key.clone(), sanitized);
                }
            }
            Some(Value::Object(result))
        }
        other => Some(other.clone()),
    }
}
/// `sanitizeAntigravityToolPayload`: bounds both retained raw events and display data.
pub fn sanitize_tool_payload(payload: &Value) -> Option<Value> {
    sanitize_value(payload, &mut Budget { nodes: 512, text: 64_000 }, 0)
}
/// `normalizeAntigravitySessionUpdate`: a tool call's text, raw input and output, content and metadata are
/// bounded before anything retains them.
pub fn normalize_session_update(notification: &mut Value) {
    let kind = notification["update"]["sessionUpdate"].as_str().unwrap_or("");
    if !matches!(kind, "tool_call" | "tool_call_update") {
        return;
    }
    let update = &mut notification["update"];
    let mut budget = Budget { nodes: 512, text: 32_000 };
    let content = update.get("content").map(|content| match content.as_array() {
        Some(entries) => entries.iter().filter_map(|entry| sanitize_value(entry, &mut budget, 0)).filter_map(|entry| t3_acp::schema::decode("v1.ToolCallContent", entry).ok()).collect::<Vec<_>>(),
        None => vec![],
    });
    let meta = update.get("_meta").map(|meta| sanitize_tool_payload(meta).filter(Value::is_object).unwrap_or(Value::Null));
    if let Some(title) = update["title"].as_str().map(|title| bound_text(title, TOOL_TEXT_LIMIT)) {
        update["title"] = json!(title);
    }
    for key in ["rawInput", "rawOutput"] {
        if let Some(value) = update.get(key) {
            match sanitize_tool_payload(value) {
                Some(value) => update[key] = value,
                None => {
                    update.as_object_mut().unwrap().remove(key);
                }
            }
        }
    }
    if let Some(content) = content {
        update["content"] = json!(content);
    }
    if let Some(meta) = meta {
        update["_meta"] = meta;
    }
}

/// The fields of `NativeToolFields`: all optional, and a field of the wrong type fails the whole decode.
#[derive(Default)]
struct Native {
    strings: HashMap<&'static str, String>,
    exit_code: Option<i64>,
    exit_code_snake: Option<i64>,
}
fn decode_native(value: &Value) -> Option<Native> {
    let object = value.as_object()?;
    let mut native = Native::default();
    for key in ["command", "CommandLine", "command_line", "commandLine", "cwd", "Cwd", "WorkingDirectory", "working_dir", "workingDir", "combinedOutput", "combined_output", "imagePath"] {
        match object.get(key) {
            None => {}
            Some(Value::String(text)) => {
                native.strings.insert(key, text.clone());
            }
            Some(_) => return None,
        }
    }
    let int = |key: &str| match object.get(key) {
        None => Some(None),
        Some(Value::Number(number)) => number.as_i64().or_else(|| number.as_f64().filter(|float| float.fract() == 0.0 && float.abs() < 9.0e15).map(|float| float as i64)).map(Some),
        Some(_) => None,
    };
    native.exit_code = int("exitCode")?;
    native.exit_code_snake = int("exit_code")?;
    Some(native)
}
fn local_image_path(image_path: Option<&str>) -> Option<String> {
    let image_path = image_path.filter(|path| js_len(path) <= TOOL_TEXT_LIMIT)?;
    let path = trim(image_path);
    if !crate::asset_workspace::is_workspace_image_preview_path(path) {
        return None;
    }
    if path.len() >= 7 && path[..7].eq_ignore_ascii_case("file://") {
        let url = url::Url::parse(path).ok()?;
        if url.host_str().is_some_and(|host| !host.is_empty() && host != "localhost") {
            return None;
        }
        let pathname = percent_decode(url.path())?;
        let bytes = pathname.as_bytes();
        return Some(if bytes.len() >= 4 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' && bytes[3] == b'/' { pathname[1..].to_owned() } else { pathname });
    }
    let scheme = regex::Regex::new(r"(?i)^[a-z][a-z\d+.-]*:").unwrap().is_match(path);
    let drive = regex::Regex::new(r"(?i)^[a-z]:[\\/]").unwrap().is_match(path);
    if scheme && !drive { None } else { Some(path.to_owned()) }
}
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = text.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}
/// `normalizeAntigravityToolCall`: the native shell fields become the command, working directory, output and
/// exit code the timeline shows.
pub fn normalize_tool_call(tool: &Value) -> Value {
    let input = decode_native(&tool["data"]["rawInput"]);
    let output = decode_native(&tool["data"]["rawOutput"]);
    let pick = |native: &Option<Native>, keys: &[&'static str]| keys.iter().find_map(|key| native.as_ref()?.strings.get(key).cloned());
    let native_command = pick(&input, &["CommandLine", "command_line", "commandLine", "command"]).or_else(|| pick(&output, &["commandLine", "command_line"])).or_else(|| tool["command"].as_str().map(str::to_owned));
    let command = native_command.as_deref().map(trim).filter(|command| !command.is_empty()).map(|command| bound_text(command, TOOL_TEXT_LIMIT));
    let native_cwd = pick(&input, &["Cwd", "WorkingDirectory", "working_dir", "workingDir", "cwd"]).or_else(|| pick(&output, &["workingDir", "working_dir"]));
    let cwd = native_cwd.as_deref().map(trim).filter(|cwd| !cwd.is_empty()).map(|cwd| bound_text(cwd, TOOL_TEXT_LIMIT));
    let aggregated_output = pick(&output, &["combinedOutput", "combined_output"]).map(|text| bound_text(&text, TOOL_TEXT_LIMIT));
    let exit_code = output.as_ref().and_then(|output| output.exit_code.or(output.exit_code_snake));
    let image_path = local_image_path(output.as_ref().and_then(|output| output.strings.get("imagePath")).map(String::as_str));
    let sanitized = sanitize_tool_payload(&tool["data"]);
    let mut data = sanitized.filter(Value::is_object).unwrap_or_else(|| json!({}));
    let kind = tool["kind"].as_str().map(str::to_owned).or_else(|| command.as_ref().map(|_| "execute".to_owned()));
    if let Some(kind) = &kind {
        data["kind"] = json!(kind);
    }
    if let Some(command) = &command {
        data["command"] = json!(command);
    }
    if let Some(cwd) = &cwd {
        data["cwd"] = json!(cwd);
    }
    if let Some(path) = &image_path {
        data["imagePath"] = json!(path);
    }
    if kind.as_deref() == Some("execute") {
        let mut item = data["item"].as_object().cloned().unwrap_or_default();
        if let Some(command) = &command {
            item.insert("command".into(), json!(command));
        }
        if let Some(cwd) = &cwd {
            item.insert("cwd".into(), json!(cwd));
        }
        if let Some(output) = &aggregated_output {
            item.insert("aggregatedOutput".into(), json!(output));
        }
        if let Some(code) = exit_code {
            item.insert("exitCode".into(), json!(code));
        }
        data["item"] = Value::Object(item);
    }
    let mut result = tool.clone();
    if let Some(kind) = kind {
        result["kind"] = json!(kind);
    }
    if let Some(command) = &command {
        result["command"] = json!(command);
    }
    if let Some(title) = tool["title"].as_str() {
        result["title"] = json!(bound_text(title, TOOL_TEXT_LIMIT));
    }
    if let Some(command) = &command {
        result["detail"] = json!(command);
    } else if let Some(detail) = tool["detail"].as_str() {
        result["detail"] = json!(bound_text(detail, TOOL_TEXT_LIMIT));
    }
    result["data"] = data;
    result
}

// ---- subagents ----

/// `classifyAntigravitySubagentToolCall`: ACP 1.1.1 exposes a subagent launch as an ordinary `start_subagent` tool.
pub fn is_subagent_tool_call(tool: &Value) -> bool {
    let kind_ok = tool["kind"].as_str().is_none_or(|kind| kind == "other");
    let title = tool["title"].as_str();
    if !kind_ok || !matches!(title, Some("Running start_subagent" | "Run start_subagent?")) {
        return false;
    }
    tool["data"]["meta"]["is_mcp_tool_call"] != true
}
pub fn subagent_output(tool: &Value) -> Option<String> {
    tool["data"]["rawOutput"].as_str().map(trim).filter(|text| !text.is_empty()).map(|text| bound_text(text, TOOL_TEXT_LIMIT))
}
/// The subagent batch a `start_subagent` call stands for: the timeline shows it as delegated work.
pub fn subagent_update(tool: &Value) -> Option<crate::xai_extension::SubagentUpdate> {
    if !is_subagent_tool_call(tool) {
        return None;
    }
    let status = match tool["status"].as_str() {
        Some("failed") => "failed",
        Some("pending") => "pending",
        _ => "running",
    };
    let output = subagent_output(tool);
    let prompt = output.clone().or_else(|| tool["detail"].as_str().map(str::to_owned)).or_else(|| tool["title"].as_str().map(str::to_owned)).unwrap_or_else(|| "Antigravity subagent batch".into());
    Some(crate::xai_extension::SubagentUpdate {
        native_task_id: tool["toolCallId"].as_str().unwrap_or("").to_owned(),
        prompt,
        title: Some("Antigravity subagent batch".into()),
        model: None,
        status,
        child_session_id: None,
        result: if status == "failed" { output } else { None },
        suppress_normal_tool: true,
    })
}

// ---- session launch ----

/// `antigravityPermissionMode`: the agent's native mode for a runtime mode.
pub fn permission_mode(runtime_mode: &str) -> &'static str {
    match runtime_mode {
        "full-access" => "yolo",
        "auto-accept-edits" => "auto_edit",
        _ => "default",
    }
}
fn model_options(config_options: &Value) -> Vec<Value> {
    let Some(model) = config_options.as_array().and_then(|options| options.iter().find(|option| option["id"] == "model")).filter(|option| option["type"] == "select") else { return vec![] };
    model["options"].as_array().into_iter().flatten().flat_map(|entry| if entry.get("value").is_some() { vec![entry.clone()] } else { entry["options"].as_array().cloned().unwrap_or_default() }).collect()
}
/// `applyAntigravityAcpModelSelection`'s decision: the model to select (`Ok(Some)`), nothing to change
/// (`Ok(None)`), or the account does not offer the model (`Err` with the message).
pub fn resolve_model(config_options: &Value, model: Option<&str>, default_model: Option<&str>) -> Result<Option<String>, String> {
    let current = config_options.as_array().and_then(|options| options.iter().find(|option| option["id"] == "model")).filter(|option| option["type"] == "select").and_then(|option| option["currentValue"].as_str().map(str::to_owned));
    let explicit_model = model.filter(|model| !model.is_empty() && *model != DEFAULT_MODEL);
    let options = model_options(config_options);
    let resolved = match explicit_model {
        Some(model) => Some(model.to_owned()),
        None => default_model.filter(|default| options.iter().any(|option| option["value"] == *default)).map(str::to_owned).or_else(|| current.clone()),
    };
    let Some(resolved) = resolved else { return Ok(None) };
    if explicit_model.is_none() && Some(&resolved) == current.as_ref() {
        return Ok(None);
    }
    if !options.iter().any(|option| option["value"] == resolved.as_str()) {
        return Err(format!("Antigravity model '{resolved}' is unavailable for this Google account. Select an available model."));
    }
    Ok(Some(resolved))
}
/// Keep the official Antigravity session's current model. Never send this ID to ACP.
pub const DEFAULT_MODEL: &str = "antigravity-default";

// ---- Google sign-in ----

/// A Google sign-in request the agent started: where to send the user, and the loopback it listens on.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthorizationUrl {
    pub authorization_url: String,
    pub redirect_uri: String,
    pub state: String,
}
/// `parseAntigravityAuthorizationUrl`: reads only the public authorization request.
pub fn parse_authorization_url(authorization_url: &str) -> Option<AuthorizationUrl> {
    if js_len(authorization_url) > MAX_AUTHORIZATION_URL_LENGTH || authorization_url.chars().any(char::is_whitespace) {
        return None;
    }
    let url = url::Url::parse(authorization_url).ok()?;
    let all = |name: &str| url.query_pairs().filter(|(key, _)| key == name).map(|(_, value)| value.into_owned()).collect::<Vec<_>>();
    let (states, redirects, types) = (all("state"), all("redirect_uri"), all("response_type"));
    if url.scheme() != "https" || url.host_str() != Some("accounts.google.com") || url.port().is_some() || url.path() != "/o/oauth2/v2/auth" || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return None;
    }
    if states.len() != 1 || redirects.len() != 1 || types != ["code"] {
        return None;
    }
    let (state, redirect_uri) = (&states[0], &redirects[0]);
    if state.is_empty() || js_len(state) > 512 || state.chars().any(char::is_whitespace) {
        return None;
    }
    let pattern = regex::Regex::new(r"^http://127\.0\.0\.1:[1-9][0-9]{0,4}/$").unwrap();
    if !pattern.is_match(redirect_uri) {
        return None;
    }
    let redirect = url::Url::parse(redirect_uri).ok()?;
    if redirect.port().is_none_or(|port| port < 1024) {
        return None;
    }
    Some(AuthorizationUrl { authorization_url: authorization_url.to_owned(), redirect_uri: redirect_uri.clone(), state: state.clone() })
}
/// `validateAntigravityCallbackUrl`: only the callback advertised by this running agent may receive a request.
pub fn validate_callback_url(pending: &AuthorizationUrl, callback_url: &str) -> Result<url::Url, &'static str> {
    if js_len(callback_url) > 16_384 {
        return Err("The sign-in response URL is too long.");
    }
    let callback = url::Url::parse(callback_url).map_err(|_| "Paste the complete redirect URL from the Google sign-in page.")?;
    let expected = url::Url::parse(&pending.redirect_uri).map_err(|_| "This redirect URL does not belong to the current sign-in.")?;
    if callback.scheme() != "http" || callback.host_str() != Some("127.0.0.1") || callback.origin() != expected.origin() || callback.path() != expected.path() || !callback.username().is_empty() || callback.password().is_some() || callback.fragment().is_some() {
        return Err("This redirect URL does not belong to the current sign-in.");
    }
    let all = |name: &str| callback.query_pairs().filter(|(key, _)| key == name).map(|(_, value)| value.into_owned()).collect::<Vec<_>>();
    let states = all("state");
    if states.len() != 1 || states[0] != pending.state {
        return Err("This redirect URL does not belong to the current sign-in.");
    }
    let (codes, errors) = (all("code"), all("error"));
    if !((codes.len() == 1 && !codes[0].is_empty() && errors.is_empty()) || (errors.len() == 1 && !errors[0].is_empty() && codes.is_empty())) {
        return Err("The redirect URL must contain one Google sign-in response.");
    }
    let issuers = all("iss");
    if issuers.len() > 1 || (issuers.len() == 1 && issuers[0] != "https://accounts.google.com") {
        return Err("The redirect URL is not a Google sign-in response.");
    }
    Ok(callback)
}
/// The URL a line of the agent's output asks the user to open, when it is one of the two forms the agent uses.
pub fn authorization_url_in_line(line: &str) -> Option<String> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if let Some(url) = line.strip_prefix(AUTH_STDOUT_PREFIX) {
        return Some(url.to_owned());
    }
    serde_json::from_str::<String>(line.strip_prefix(AUTH_BROWSER_MARKER)?).ok()
}

// ---- process environment and profile ----

const REMOVED_ENVIRONMENT_KEYS: [&str; 17] = [
    "GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_APPLICATION_CREDENTIALS", "GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_QUOTA_PROJECT",
    "GOOGLE_GENAI_USE_VERTEXAI", "GCLOUD_PROJECT", "CLOUDSDK_CORE_PROJECT", "AGY_ACP_CCPA_PROJECT", "AGY_ACP_ENABLE_OAUTH", "GEMINI_HOME",
    "AGY_ACP_FORCE_FILE_STORAGE", "ANTIGRAVITY_HARNESS_PATH", "BROWSER", "PYTHONUNBUFFERED", "ELECTRON_RUN_AS_NODE",
];
/// The instance's credentials for the non-personal sign-in methods. Empty strings mean "not set".
#[derive(Clone, Debug, PartialEq)]
pub struct AuthConfig {
    pub auth_method: String,
    pub api_key: String,
    pub gcp_project: String,
    pub gcp_location: String,
}
impl AuthConfig {
    pub fn from_settings(settings: &t3_contracts::AntigravitySettings) -> Self {
        let method = serde_json::to_value(&settings.auth_method).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_else(|| "oauth-personal".into());
        Self { auth_method: method, api_key: settings.api_key.as_str().into(), gcp_project: settings.gcp_project.as_str().into(), gcp_location: settings.gcp_location.as_str().into() }
    }
    /// True for the two methods that open a Google sign-in page.
    pub fn uses_browser(&self) -> bool {
        matches!(self.auth_method.as_str(), "oauth-personal" | "oauth-business")
    }
    /// Label shown on the provider card once the method has authenticated.
    pub fn label(&self) -> &'static str {
        match self.auth_method.as_str() {
            "oauth-business" => "Gemini Enterprise",
            "gemini-api-key" => "Gemini API key",
            "agent-platform" => "Agent Platform",
            _ => "Google account",
        }
    }
    /// What is missing before a non-personal method can authenticate, or `None` when the config is complete.
    pub fn config_issue(&self) -> Option<&'static str> {
        match self.auth_method.as_str() {
            "oauth-business" if self.gcp_project.is_empty() || self.gcp_location.is_empty() => Some("Gemini Enterprise needs a GCP project and location in the Antigravity provider settings."),
            "gemini-api-key" if self.api_key.is_empty() => Some("Enter a Gemini API key in the Antigravity provider settings."),
            "agent-platform" if self.api_key.is_empty() && (self.gcp_project.is_empty() || self.gcp_location.is_empty()) => Some("Agent Platform needs an API key, or a GCP project and location, in the Antigravity provider settings."),
            _ => None,
        }
    }
    /// `settings.json` of the agent's profile. `auth.type` names the method so a native logout clears only that
    /// method's credentials. Never holds a credential.
    pub fn profile_settings(&self) -> String {
        let mut gcp = Map::new();
        if !self.gcp_project.is_empty() {
            gcp.insert("project".into(), json!(self.gcp_project));
        }
        if !self.gcp_location.is_empty() {
            gcp.insert("location".into(), json!(self.gcp_location));
        }
        let mut settings = json!({"auth":{"type":self.auth_method}});
        if !gcp.is_empty() {
            settings["gcp"] = Value::Object(gcp);
        }
        format!("{settings}\n")
    }
}
/// The directories an instance owns: the Google profile (`GEMINI_HOME`) and the parent of the per-process
/// directories the agent unpacks into, which sits beside the profile to stay under Windows' path limit.
pub fn instance_directories(state_dir: &std::path::Path, instance_id: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use sha2::{Digest, Sha256};
    let key: String = Sha256::digest(instance_id.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
    (state_dir.join("providers").join("antigravity").join(&key), state_dir.join("antigravity-tmp").join(&key[..12]))
}
/// The environment the agent runs with: the instance's, minus anything that selects another credential, plus the
/// profile and the one credential of the configured method.
pub fn environment(base: &HashMap<String, String>, profile: &std::path::Path, browser_command: &str, auth: &AuthConfig, runtime_temp: &std::path::Path) -> HashMap<String, String> {
    let mut environment: HashMap<String, String> = base.iter().filter(|(key, _)| !REMOVED_ENVIRONMENT_KEYS.contains(&key.to_uppercase().as_str())).map(|(key, value)| (key.clone(), value.clone())).collect();
    match (auth.auth_method.as_str(), auth.api_key.is_empty()) {
        ("gemini-api-key", false) => drop(environment.insert("GEMINI_API_KEY".into(), auth.api_key.clone())),
        ("agent-platform", false) => drop(environment.insert("GOOGLE_API_KEY".into(), auth.api_key.clone())),
        _ => {}
    }
    environment.insert("GEMINI_HOME".into(), profile.to_string_lossy().into_owned());
    environment.insert("AGY_ACP_FORCE_FILE_STORAGE".into(), "1".into());
    environment.insert("BROWSER".into(), browser_command.to_owned());
    environment.insert("PYTHONUNBUFFERED".into(), "1".into());
    let temp = runtime_temp.to_string_lossy().into_owned();
    if cfg!(windows) {
        environment.insert("TEMP".into(), temp.clone());
        environment.insert("TMP".into(), temp);
    } else {
        environment.insert("TMPDIR".into(), temp);
    }
    environment
}
/// The `BROWSER` command: the agent asks it to open the sign-in page, and it reports the URL to T3 on stderr
/// instead of launching anything. Python splits `BROWSER` on the platform path separator, so it has neither a
/// colon nor a semicolon.
pub fn browser_command() -> String {
    format!("/bin/sh -c 'printf \"%b\\\"%b\\\"\\n\" {AUTH_BROWSER_MARKER} \"$1\" >&2' sh %s")
}
/// `antigravityUserSkillDirectories`: the agent's two user-global skill directories under a Gemini home.
pub fn user_skill_directories(gemini_home: &std::path::Path) -> [std::path::PathBuf; 2] {
    [gemini_home.join("config").join("skills"), gemini_home.join("antigravity-cli").join("skills")]
}
/// Whether an error from the agent means Google sign-in is needed.
pub fn is_sign_in_required(code: i64) -> bool {
    code == -32000
}
