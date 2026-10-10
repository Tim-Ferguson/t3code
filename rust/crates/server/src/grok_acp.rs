//! How T3 launches and talks to `grok agent stdio`: `GrokAcpSupport.ts`.
use serde_json::{Value, json};
use std::collections::HashMap;

pub const API_KEY_ENV: &str = "XAI_API_KEY";
const OAUTH2_REFERRER_ENV: &str = "GROK_OAUTH2_REFERRER";
const OAUTH2_REFERRER: &str = "t3code";
const AUTH_METHOD_API_KEY: &str = "xai.api_key";
const AUTH_METHOD_CACHED_TOKEN: &str = "cached_token";
/// T3's built-in Grok slug: the CLI's product name, not a model id the ACP accepts, so selecting it
/// means "whatever model the Grok session currently runs on".
pub const DEFAULT_MODEL_SLUG: &str = "grok-build";
/// The runtime modes `grok agent` can launch in: ask, its auto classifier, and always-approve.
pub const SUPPORTED_RUNTIME_MODES: [&str; 3] = ["approval-required", "auto", "full-access"];
/// Current Grok treats Ctrl+C cancellation as a barrier against stale background-task wake prompts.
pub const CANCEL_META: fn() -> Value = || json!({"cancelTrigger":"ctrl_c"});
/// Grok's Auto mode asks the client about an action its classifier blocks only when the client declares a
/// type that can show a prompt; `extension` keeps the permission options T3 already maps.
pub const INITIALIZE_META: fn() -> Value = || json!({"clientType":"extension"});
/// Grok's only session-scoped `allow_always` answer: "Yes, allow all edits during this session".
const ALLOW_EDITS_SESSION_OPTION_ID: &str = "allow-edits-session";

/// `grokAcpSpawnArgs`: `--permission-mode` on the argv beats the user's Grok config, so a supervised
/// thread cannot inherit a configured always-approve. A mode Grok does not offer launches asking.
pub fn spawn_args(runtime_mode: Option<&str>) -> Vec<String> {
    let args: &[&str] = match runtime_mode {
        None => &["agent", "stdio"],
        Some("auto") => &["--permission-mode", "auto", "agent", "stdio"],
        Some("full-access") => &["agent", "--always-approve", "stdio"],
        Some(_) => &["--permission-mode", "default", "agent", "stdio"],
    };
    args.iter().map(|arg| (*arg).to_owned()).collect()
}
/// The mode `grok agent` launches in. Grok's permission mode is fixed at launch, and an explicit approval
/// or sandbox override launches it asking, so every mutating prompt reaches T3's policy check.
pub fn launch_runtime_mode<'a>(runtime_mode: &'a str, overridden: bool) -> &'a str {
    if overridden { "approval-required" } else { runtime_mode }
}
/// `buildGrokAcpSpawnInput`'s environment: the instance's, plus the OAuth referrer.
pub fn spawn_environment(environment: &HashMap<String, String>) -> HashMap<String, String> {
    let mut environment = environment.clone();
    environment.insert(OAUTH2_REFERRER_ENV.into(), OAUTH2_REFERRER.into());
    environment
}
/// `resolveGrokAuthMethodId`: the API key when one is set, else the cached login.
pub fn auth_method_id(environment: &HashMap<String, String>) -> &'static str {
    if environment.get(API_KEY_ENV).is_some_and(|key| !t3_contracts::trim_wire_string(key).is_empty()) { AUTH_METHOD_API_KEY } else { AUTH_METHOD_CACHED_TOKEN }
}
/// `resolveGrokAcpBaseModelId`: a model selection as the ACP model id, `grok-build` when empty.
pub fn base_model_id(model: Option<&str>) -> String {
    let trimmed = model.map(t3_contracts::trim_wire_string).filter(|model| !model.is_empty()).unwrap_or(DEFAULT_MODEL_SLUG);
    trimmed.to_owned()
}
/// `isValidGrokReasoningEffortToken`.
pub fn is_valid_reasoning_effort(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else { return false };
    first.is_ascii_alphanumeric() && value.chars().count() <= 32 && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}
/// `grokApprovalOptions`: the approval choices a Grok permission prompt can honor. The session choice appears
/// only where Grok's answer lasts for the session.
pub fn approval_options(request: &Value) -> Vec<Value> {
    let options = request["options"].as_array().cloned().unwrap_or_default();
    let has = |kind: &str, id: Option<&str>| options.iter().any(|option| option["kind"] == kind && id.is_none_or(|id| option["optionId"].as_str().is_some_and(|option| t3_contracts::trim_wire_string(option) == id)));
    let mut choices = vec![json!({"decision":"cancel","label":"Cancel"})];
    if has("reject_once", None) {
        choices.push(json!({"decision":"decline","label":"Decline"}));
    }
    if has("allow_always", Some(ALLOW_EDITS_SESSION_OPTION_ID)) {
        choices.push(json!({"decision":"acceptForSession","label":"Allow all edits this session"}));
    }
    if has("allow_once", None) {
        choices.push(json!({"decision":"accept","label":"Approve"}));
    }
    choices
}
/// A permission request Grok's Auto mode sent is one its classifier refused, so it always goes to the user.
pub fn permission_asks(launch_mode: &str) -> bool {
    launch_mode == "auto"
}
/// The model the session setup says it runs on.
pub fn current_model_id(setup: &Value) -> Option<String> {
    setup["models"]["currentModelId"].as_str().map(t3_contracts::trim_wire_string).filter(|model| !model.is_empty()).map(str::to_owned)
}
