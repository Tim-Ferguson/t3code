//! Source-backed Codex account/model/skill discovery over app-server.
use crate::claude_usage_limits::make_unavailable_usage_limits;
use crate::provider_process::{ProcessError, ProcessEvent, ProcessOptions, ProviderProcess};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

pub type CodexConfig = t3_contracts::CodexSettings;

#[derive(Clone, Debug)]
pub struct CodexInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: CodexConfig,
    pub environment: HashMap<String, String>,
}
pub struct CodexConnection {
    pub process: ProviderProcess,
    pub events: tokio::sync::broadcast::Receiver<ProcessEvent>,
    pub initialize: Value,
}
impl CodexInstance {
    pub fn process_options(&self, cwd: &Path) -> Result<ProcessOptions, ProcessError> {
        let (binary, environment, launch_args) = self.launch()?;
        let mut args = vec!["app-server".into()];
        args.extend(tokenize_cli_args(&launch_args));
        Ok(ProcessOptions {
            binary,
            args,
            cwd: cwd.into(),
            environment,
        })
    }
    /// What every way of starting this instance's CLI shares: the binary, the environment (the
    /// instance's own, with `CODEX_HOME` when it has a home) and the configured launch arguments
    /// (`T3CODE_CODEX_LAUNCH_ARGS` wins over the setting).
    pub(crate) fn launch(&self) -> Result<(PathBuf, HashMap<String, String>, String), ProcessError> {
        if self.is_managed() {
            // A managed instance starts through its signed-in runtime (`codex_managed::resolve`), which is asynchronous.
            return Err(ProcessError::Protocol("Managed Codex starts only through its signed-in runtime.".into()));
        }
        let mut environment = self.environment.clone();
        let launch_args = environment
            .get("T3CODE_CODEX_LAUNCH_ARGS")
            .cloned()
            .or_else(|| std::env::var("T3CODE_CODEX_LAUNCH_ARGS").ok())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.config.launch_args.to_string());
        // The shadow home, when the instance has one, is `CODEX_HOME`: its credentials stay its own and the rest is the shared home's.
        let layout = crate::codex_home::resolve(self.config.home_path.as_str(), self.config.shadow_home_path.as_str(), &environment);
        crate::codex_home::materialize(&layout).map_err(ProcessError::Protocol)?;
        if let Some(effective) = &layout.effective {
            environment.insert("CODEX_HOME".into(), effective.to_string_lossy().into_owned());
        }
        let binary = expand_home(self.config.binary_path.as_str(), &environment)?;
        Ok((binary, environment, launch_args))
    }
    /// A managed-setup instance (`setupMode` other than `existing`).
    pub fn is_managed(&self) -> bool {
        self.config.setup_mode.is_some_and(|mode| mode != t3_contracts::CodexSetupMode::Existing)
    }
    /// `resolveCodexHomeLayout`'s shared home: the configured home, else `~/.codex`, made absolute.
    fn shared_home(&self) -> PathBuf {
        crate::codex_home::resolve(self.config.home_path.as_str(), self.config.shadow_home_path.as_str(), &self.environment).shared
    }
    pub async fn connect(&self, cwd: &Path) -> Result<CodexConnection, ProcessError> {
        let process = ProviderProcess::spawn(self.process_options(cwd)?)?;
        Self::initialize_process(process).await
    }
    pub(crate) async fn initialize_process(
        process: ProviderProcess,
    ) -> Result<CodexConnection, ProcessError> {
        let events = process.subscribe();
        let initialize = process
            .request("initialize", initialize_params(), Duration::from_secs(10))
            .await?;
        process.notify("initialized", None).await?;
        Ok(CodexConnection {
            process,
            events,
            initialize,
        })
    }
    pub async fn discover(&self, cwd: &Path) -> Result<Value, ProcessError> {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut snapshot = json!({"instanceId":self.instance_id,"driver":"codex","displayName":self.display_name,"enabled":self.enabled,"installed":false,"version":null,"status":"warning","auth":{"status":"unknown"},"checkedAt":now,"availability":"available","models":append_custom_models(vec![],&self.config.custom_models)?,"slashCommands":[],"skills":[],"showInteractionModeToggle":true,"reportsContextWindow":true});
        // Instances that share a Codex home share its sessions, so a thread continues one native conversation
        // across them (`codexContinuationIdentity`); an auth overlay keeps the home it overlays.
        snapshot["continuation"] = json!({"groupKey":format!("codex:home:{}", self.shared_home().to_string_lossy())});
        if let Some(accent) = &self.accent_color {
            snapshot["accentColor"] = json!(accent);
        }
        if !self.enabled {
            snapshot["status"] = json!("disabled");
            snapshot["message"] = json!("Codex is disabled in T3 Code settings.");
            return validate_snapshot(snapshot);
        }
        // One total probe budget, matching the original provider status timeout.
        match tokio::time::timeout(Duration::from_secs(10), self.probe(cwd, &now)).await {
            Ok(Ok(probe)) => {
                snapshot["installed"] = json!(true);
                snapshot["version"] = probe["version"].clone();
                snapshot["status"] = probe["status"].clone();
                snapshot["auth"] = probe["auth"].clone();
                snapshot["models"] = probe["models"].clone();
                snapshot["skills"] = probe["skills"].clone();
                snapshot["slashCommands"] = json!([{"name":"compact","description":"Summarize the conversation and reduce context usage"}]);
                if !probe["usageLimits"].is_null() {
                    snapshot["usageLimits"] = probe["usageLimits"].clone();
                }
                if let Some(message) = probe.get("message") {
                    snapshot["message"] = message.clone();
                }
            }
            Ok(Err(error)) => {
                snapshot["installed"] = json!(!matches!(error, ProcessError::Io(_)));
                snapshot["status"] = json!("error");
                snapshot["message"] =
                    json!(format!("Codex app-server provider probe failed: {error}."));
            }
            Err(_) => {
                snapshot["installed"] = json!(true);
                snapshot["status"] = json!("error");
                snapshot["message"] =
                    json!("Timed out while checking Codex app-server provider status.");
            }
        }
        validate_snapshot(snapshot)
    }
    async fn probe(&self, cwd: &Path, checked_at: &str) -> Result<Value, ProcessError> {
        self.probe_process(self.process_options(cwd)?, cwd, checked_at, false).await
    }
    /// The probe of a process started as `options` says. `managed` is a ChatGPT-token runtime, whose usage is the account's and not the
    /// CLI's: it is not read (`skipNativeUsage`) and the snapshot carries none.
    pub(crate) async fn probe_process(&self, options: ProcessOptions, cwd: &Path, checked_at: &str, managed: bool) -> Result<Value, ProcessError> {
        let CodexConnection {
            process,
            mut events,
            initialize,
        } = Self::initialize_process(ProviderProcess::spawn(options)?).await?;
        let request_process = process.clone();
        // Probes cannot grant command execution, file writes or interactive
        // requests. Respond explicitly instead of leaving provider calls pending.
        let requests = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let ProcessEvent::Request { id, method, .. } = event {
                    let _ = request_process
                        .respond(
                            id,
                            Err(ProcessError::Remote {
                                code: -32601,
                                message: format!("Method not available during discovery: {method}"),
                                data: Value::Null,
                            }),
                        )
                        .await;
                }
            }
        });
        struct Abort(tokio::task::JoinHandle<()>);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _requests = Abort(requests);
        let account = process
            .request("account/read", json!({}), Duration::from_secs(10))
            .await?;
        let version = initialize["userAgent"]
            .as_str()
            .and_then(|agent| agent.split_once('/'))
            .and_then(|(_, suffix)| suffix.split_whitespace().next());
        let auth = account_auth(&account);
        if account["account"].is_null() && account["requiresOpenaiAuth"] == true {
            // The original reads no usage here, and an unread usage is a failed probe.
            return Ok(
                json!({"version":version,"status":"error","auth":{"status":"unauthenticated"},"message":"Codex CLI is not authenticated. Run `codex login` and try again.","models":append_custom_models(vec![],&self.config.custom_models)?,"skills":[],"usageLimits":if managed { Value::Null } else { make_unavailable_usage_limits(checked_at,"probeFailed",None) }}),
            );
        }
        // Usage is an enrichment: a failure or a slow answer costs this probe its usage, not the account and models.
        let rate_limits = async {
            if managed {
                return Ok::<_, ProcessError>(Err(String::new()));
            }
            Ok::<_, ProcessError>(match process.request("account/rateLimits/read", Value::Null, RATE_LIMITS_PROBE_TIMEOUT).await {
                Ok(response) => Ok(response),
                Err(error) => Err(rate_limits_failure_message(&error)),
            })
        };
        let (models, skills, rate_limits) = tokio::try_join!(
            read_models(&process),
            process.request(
                "skills/list",
                json!({"cwds":[cwd]}),
                Duration::from_secs(10)
            ),
            rate_limits
        )?;
        let usage_limits = if managed {
            Value::Null
        } else if account["account"]["type"] == "apiKey" {
            make_unavailable_usage_limits(checked_at, "unsupported", None)
        } else {
            match rate_limits {
                Ok(response) => crate::codex_usage_limits::rate_limits_to_limits(&response["rateLimits"], Some(&response["rateLimitsByLimitId"]), Some(&response["rateLimitResetCredits"]), checked_at),
                Err(message) => make_unavailable_usage_limits(checked_at, "probeFailed", Some(&message)),
            }
        };
        let mut models = append_custom_models(models, &self.config.custom_models)?;
        apply_preferred_default(&mut models);
        Ok(
            json!({"version":version,"status":"ready","auth":auth,"models":models,"skills":parse_skills(&skills,cwd)?,"usageLimits":usage_limits}),
        )
    }
}

/// `RATE_LIMITS_PROBE_TIMEOUT_MS`.
const RATE_LIMITS_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// `codexRateLimitsFailureMessage`: a bounded reason for the Limits view; the raw error is not sent to clients.
fn rate_limits_failure_message(error: &ProcessError) -> String {
    match error {
        ProcessError::Remote { code, .. } | ProcessError::JsonRpcRemote { code, .. } => format!("Codex could not read usage (JSON-RPC {code})."),
        ProcessError::Io(_) => "Codex could not be started to read usage.".into(),
        ProcessError::Exited { .. } | ProcessError::Closed(_) => "Codex exited before it could report usage.".into(),
        _ => "Codex did not answer the usage request.".into(),
    }
}

fn validate_snapshot(value: Value) -> Result<Value, ProcessError> {
    let typed: t3_contracts::ServerProvider =
        serde_json::from_value(value).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}
/// The client capabilities of the original adapter: `turn/diff/updated`
/// is opted out because checkpoints, not Codex's running diff, drive the diff views. `extensions` declares
/// MCP Apps support (SEP-1724), so servers attach their UI resources and Codex reports them on tool call items.
pub fn initialize_params() -> Value {
    json!({"clientInfo":{"name":"T3 Code","title":"T3 Code","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true,"optOutNotificationMethods":["turn/diff/updated"],"extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":["text/html;profile=mcp-app"]}}}})
}
async fn read_models(process: &ProviderProcess) -> Result<Vec<Value>, ProcessError> {
    let mut models = vec![];
    let mut cursor = None;
    let mut seen = HashSet::new();
    loop {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor":cursor}))
            .unwrap_or(json!({}));
        let response = process
            .request("model/list", params, Duration::from_secs(10))
            .await?;
        let data = response["data"]
            .as_array()
            .ok_or_else(|| ProcessError::Protocol("model/list data must be an array".into()))?;
        for model in data {
            models.push(map_model(model)?);
        }
        cursor = response["nextCursor"]
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .map(String::from);
        match &cursor {
            Some(cursor) if !seen.insert(cursor.clone()) => {
                return Err(ProcessError::Protocol(
                    "model/list repeated a pagination cursor".into(),
                ));
            }
            Some(_) if models.len() > 10_000 => {
                return Err(ProcessError::Protocol(
                    "model/list exceeded catalog budget".into(),
                ));
            }
            Some(_) => {}
            None => break,
        }
    }
    Ok(models)
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProcessError> {
    value[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProcessError::Protocol(format!("{key} must be a nonempty string")))
}
pub fn map_model(model: &Value) -> Result<Value, ProcessError> {
    let slug = text(model, "model")?;
    let name = format_model_name(text(model, "displayName")?);
    let default_reasoning = if family(slug) == "gpt-6-astra" {
        Some("medium")
    } else {
        model["defaultReasoningEffort"].as_str()
    };
    let mut descriptors = vec![];
    let efforts = model["supportedReasoningEfforts"]
        .as_array()
        .ok_or_else(|| {
            ProcessError::Protocol("supportedReasoningEfforts must be an array".into())
        })?;
    let mut options = vec![];
    for effort in efforts {
        let id = text(effort, "reasoningEffort")?;
        let label = match id {
            "none" => "None",
            "minimal" => "Minimal",
            "low" => "Low",
            "medium" => "Medium",
            "high" => "High",
            "xhigh" => "Extra High",
            "max" => "Max",
            "ultra" => "Ultra",
            id => id,
        };
        let mut option = json!({"id":id,"label":label});
        if Some(id) == default_reasoning {
            option["isDefault"] = json!(true);
        }
        options.push(option);
    }
    if !options.is_empty() {
        let current = options
            .iter()
            .find(|option| option["isDefault"] == true)
            .map(|option| option["id"].clone());
        let mut descriptor =
            json!({"id":"reasoningEffort","label":"Reasoning","type":"select","options":options});
        if let Some(current) = current {
            descriptor["currentValue"] = current;
        }
        descriptors.push(descriptor);
    }
    let tiers=model["serviceTiers"].as_array().filter(|tiers|!tiers.is_empty()).cloned().unwrap_or_else(||model["additionalSpeedTiers"].as_array().map(|tiers|tiers.iter().map(|tier|json!({"id":tier,"name":if tier=="fast" {json!("Fast")} else {tier.clone()},"description":""})).collect()).unwrap_or_default());
    if !tiers.is_empty() {
        let default = model["defaultServiceTier"]
            .as_str()
            .filter(|id| tiers.iter().any(|tier| tier["id"] == *id))
            .unwrap_or("default");
        let mut options = vec![json!({"id":"default","label":"Standard"})];
        if default == "default" {
            options[0]["isDefault"] = json!(true);
        }
        for tier in tiers {
            let id = text(&tier, "id")?;
            let mut option = json!({"id":id,"label":text(&tier,"name")?});
            let description = if id == "ultrafast" {
                Some("Even faster, more expensive")
            } else {
                tier["description"].as_str().filter(|text| !text.is_empty())
            };
            if let Some(description) = description {
                option["description"] = json!(description);
            }
            if id == default {
                option["isDefault"] = json!(true);
            }
            options.push(option);
        }
        descriptors.push(json!({"id":"serviceTier","label":"Service Tier","type":"select","options":options,"currentValue":default}));
    }
    let mut result = json!({"slug":slug,"name":name,"isCustom":false,"capabilities":{"optionDescriptors":descriptors}});
    if model["isDefault"] == true {
        result["isDefault"] = json!(true);
    }
    let typed: t3_contracts::ServerProviderModel = serde_json::from_value(result)
        .map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}
fn family(model: &str) -> &str {
    if model.starts_with("openai.gpt-") {
        model.trim_start_matches("openai.")
    } else {
        model
    }
}
fn apply_preferred_default(models: &mut [Value]) {
    let preferred = ["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra"]
        .iter()
        .find_map(|preferred| {
            models
                .iter()
                .find(|model| {
                    model["isCustom"] != true
                        && model["slug"]
                            .as_str()
                            .is_some_and(|slug| family(slug) == *preferred)
                })
                .map(|model| model["slug"].clone())
        });
    if let Some(preferred) = preferred {
        for model in models {
            if model["slug"] == preferred {
                model["isDefault"] = json!(true);
            } else {
                model.as_object_mut().unwrap().remove("isDefault");
            }
        }
    }
}
fn format_model_name(name: &str) -> String {
    let mut name = name.to_owned();
    if name
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("gpt"))
    {
        name.replace_range(..3, "GPT");
    }
    let mut result = String::new();
    let mut after_dash = false;
    for character in name.chars() {
        result.push(if after_dash && character.is_ascii_lowercase() {
            character.to_ascii_uppercase()
        } else {
            character
        });
        after_dash = character == '-';
    }
    result
}
fn append_custom_models<T: Serialize>(
    mut models: Vec<Value>,
    custom: &[T],
) -> Result<Vec<Value>, ProcessError> {
    let fallback = models
        .iter()
        .find_map(|model| model.get("capabilities"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut seen = models
        .iter()
        .filter_map(|model| model["slug"].as_str().map(String::from))
        .collect::<HashSet<_>>();
    for entry in custom {
        let model = serde_json::to_value(entry)
            .map_err(|error| ProcessError::Protocol(error.to_string()))?;
        let slug = model
            .as_str()
            .or_else(|| model["slug"].as_str())
            .ok_or_else(|| ProcessError::Protocol("invalid custom model".into()))?
            .trim();
        if slug.is_empty() {
            continue;
        }
        if seen.insert(slug.into()) {
            let value = json!({"slug":slug,"name":model["name"].as_str().filter(|name|!name.trim().is_empty()).unwrap_or(slug),"isCustom":true,"capabilities":model.get("capabilities").filter(|value|!value.is_null()).unwrap_or(&fallback)});
            let typed: t3_contracts::ServerProviderModel = serde_json::from_value(value)
                .map_err(|error| ProcessError::Protocol(error.to_string()))?;
            models.push(serde_json::to_value(typed).unwrap());
        }
    }
    Ok(models)
}
fn parse_skills(response: &Value, cwd: &Path) -> Result<Vec<Value>, ProcessError> {
    let entries = response["data"]
        .as_array()
        .ok_or_else(|| ProcessError::Protocol("skills/list data must be an array".into()))?;
    let cwd = cwd.to_string_lossy();
    let matching = entries.iter().find(|entry| entry["cwd"] == cwd.as_ref());
    let entries = matching
        .map(|entry| vec![entry])
        .unwrap_or_else(|| entries.iter().collect());
    let mut output = vec![];
    for entry in entries {
        for skill in entry["skills"]
            .as_array()
            .ok_or_else(|| ProcessError::Protocol("skills/list skills must be an array".into()))?
        {
            let mut value = json!({"name":text(skill,"name")?,"path":text(skill,"path")?,"enabled":skill["enabled"]});
            for field in ["description", "scope"] {
                if let Some(text) = skill[field].as_str().filter(|text| !text.is_empty()) {
                    value[field] = json!(text);
                }
            }
            if let Some(text) = skill["interface"]["displayName"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                value["displayName"] = json!(text);
            }
            if let Some(text) = skill["shortDescription"]
                .as_str()
                .or_else(|| skill["interface"]["shortDescription"].as_str())
                .filter(|text| !text.is_empty())
            {
                value["shortDescription"] = json!(text);
            }
            let typed: t3_contracts::ServerProviderSkill = serde_json::from_value(value)
                .map_err(|error| ProcessError::Protocol(error.to_string()))?;
            output.push(serde_json::to_value(typed).unwrap());
        }
    }
    Ok(output)
}
fn account_auth(account: &Value) -> Value {
    let account = &account["account"];
    if account.is_null() {
        return json!({"status":"unknown"});
    }
    let mut auth = json!({"status":"authenticated"});
    if let Some(kind) = account["type"].as_str() {
        auth["type"] = json!(kind);
        let label = match kind {
            "apiKey" => Some("OpenAI API Key"),
            "amazonBedrock" => Some("Amazon Bedrock"),
            "chatgpt" => plan_label(account["planType"].as_str()),
            _ => None,
        };
        if let Some(label) = label {
            auth["label"] = json!(label);
        }
        if kind == "chatgpt" {
            if let Some(email) = account["email"].as_str() {
                auth["email"] = json!(email);
            }
        }
    }
    auth
}
pub(crate) fn plan_label(plan: Option<&str>) -> Option<&'static str> {
    match plan {
        Some("free") => Some("ChatGPT Free Subscription"),
        Some("go") => Some("ChatGPT Go Subscription"),
        Some("plus") => Some("ChatGPT Plus Subscription"),
        Some("pro") => Some("ChatGPT Pro 20x Subscription"),
        Some("prolite") => Some("ChatGPT Pro 5x Subscription"),
        Some("promax") => Some("ChatGPT Pro Max Subscription"),
        Some("team") => Some("ChatGPT Team Subscription"),
        Some("self_serve_business_prolite" | "self_serve_business_usage_based" | "business") => {
            Some("ChatGPT Business Subscription")
        }
        Some(
            "ent26" | "enterprise_cbp_automation" | "enterprise_cbp_usage_based" | "enterprise",
        ) => Some("ChatGPT Enterprise Subscription"),
        Some("edu" | "edu_plus" | "edu_pro") => Some("ChatGPT Edu Subscription"),
        Some("unknown") => Some("ChatGPT Subscription"),
        _ => None,
    }
}
fn expand_home(
    value: &str,
    environment: &HashMap<String, String>,
) -> Result<PathBuf, ProcessError> {
    let value = value.trim();
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        let home = environment
            .get("HOME")
            .cloned()
            .or_else(|| std::env::var("HOME").ok())
            .or_else(|| std::env::var("USERPROFILE").ok())
            .ok_or_else(|| ProcessError::Protocol("home directory is unavailable".into()))?;
        Ok(PathBuf::from(home).join(value.get(2..).unwrap_or("")))
    } else {
        Ok(PathBuf::from(value))
    }
}
/// Exact quote/backslash rules used by shared/cliArgs.ts; never invokes a shell.
pub fn tokenize_cli_args(value: &str) -> Vec<String> {
    let mut tokens = vec![];
    let mut current = String::new();
    let mut quote = None;
    let mut quoted = false;
    let mut characters = value.trim().chars().peekable();
    while let Some(character) = characters.next() {
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
                quoted = true;
            } else if character == '\\'
                && delimiter == '"'
                && characters
                    .peek()
                    .is_some_and(|next| ['"', '\\', '$', '`'].contains(next))
            {
                current.push(characters.next().unwrap());
            } else {
                current.push(character);
            }
        } else if character == '\'' || character == '"' {
            quote = Some(character);
            quoted = true;
        } else if character.is_whitespace() {
            if !current.is_empty() || quoted {
                tokens.push(std::mem::take(&mut current));
                quoted = false;
            }
        } else if character == '\\' && characters.peek().is_some_and(|next| next.is_whitespace()) {
            current.push(characters.next().unwrap());
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() || quoted {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use crate::test_support::HANG_BOUND;
    use super::*;
    #[tokio::test]
    async fn reusable_fixture_emits_tool_execution_only_after_approval_acceptance() {
        for decision in ["accept", "decline", "cancel"] {
            let directory = tempfile::tempdir().unwrap();
            let binary =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
            let instance = CodexInstance {
                instance_id: "codex".into(),
                display_name: "Codex".into(),
                accent_color: None,
                enabled: true,
                config: serde_json::from_value(json!({"binaryPath":binary,"homePath":directory.path().join("codex-home")})).unwrap(),
                environment: HashMap::new(),
            };
            let mut connection = instance.connect(directory.path()).await.unwrap();
            let thread = connection
                .process
                .request(
                    "thread/start",
                    json!({"cwd":directory.path(),"model":"fixture-model"}),
                    HANG_BOUND,
                )
                .await
                .unwrap();
            connection.process.request("turn/start",json!({"threadId":thread["thread"]["id"],"input":[{"type":"text","text":"[approval]"}]}),HANG_BOUND).await.unwrap();
            let id = loop {
                match tokio::time::timeout(HANG_BOUND, connection.events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                {
                    ProcessEvent::Request { id, method, .. } => {
                        assert_eq!(method, "item/commandExecution/requestApproval");
                        break id;
                    }
                    ProcessEvent::Notification { method, .. } => assert!(
                        method != "item/commandExecution/outputDelta" && method != "item/completed"
                    ),
                    event => panic!("unexpected event {event:?}"),
                }
            };
            connection
                .process
                .respond(id, Ok(json!({"decision":decision})))
                .await
                .unwrap();
            let mut saw_output = false;
            let mut successful_execution = false;
            loop {
                match tokio::time::timeout(HANG_BOUND, connection.events.recv())
                    .await
                    .unwrap()
                    .unwrap()
                {
                    ProcessEvent::Notification { method, params }
                        if method == "item/commandExecution/outputDelta" =>
                    {
                        saw_output = !params["delta"].as_str().unwrap().is_empty()
                    }
                    ProcessEvent::Notification { method, params }
                        if method == "item/completed"
                            && params["item"]["type"] == "commandExecution" =>
                    {
                        successful_execution = params["item"]["exitCode"] == 0
                    }
                    ProcessEvent::Notification { method, params } if method == "turn/completed" => {
                        assert_eq!(
                            params["turn"]["status"],
                            if decision == "cancel" {
                                "interrupted"
                            } else {
                                "completed"
                            }
                        );
                        break;
                    }
                    ProcessEvent::Notification { .. } => {}
                    event => panic!("unexpected event {event:?}"),
                }
            }
            assert_eq!(saw_output, decision == "accept");
            assert_eq!(successful_execution, decision == "accept");
        }
    }
    #[test]
    fn catalog_traits_preserve_provider_options_and_native_default_preference() {
        let mut models=vec![map_model(&json!({"model":"gpt-5.6-sol","displayName":"gpt-5.6-sol","isDefault":true,"defaultReasoningEffort":"low","supportedReasoningEfforts":[{"reasoningEffort":"low"}],"additionalSpeedTiers":["fast"]})).unwrap(),map_model(&json!({"model":"openai.gpt-6-astra","displayName":"gpt-6-astra","defaultReasoningEffort":"low","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"medium"}],"serviceTiers":[{"id":"ultrafast","name":"Ultra fast","description":"long description"}],"defaultServiceTier":"ultrafast"})).unwrap()];
        apply_preferred_default(&mut models);
        assert!(models[0].get("isDefault").is_none());
        assert_eq!(models[1]["isDefault"], true);
        assert_eq!(models[1]["name"], "GPT-6-Astra");
        assert_eq!(
            models[1]["capabilities"]["optionDescriptors"][0]["currentValue"],
            "medium"
        );
        assert_eq!(
            models[1]["capabilities"]["optionDescriptors"][1]["options"][1]["description"],
            "Even faster, more expensive"
        );
        let custom = append_custom_models(
            models,
            &[
                json!("custom"),
                json!({"slug":"gpt-5.6-sol"}),
                json!({"slug":"custom"}),
            ],
        )
        .unwrap();
        assert_eq!(custom.len(), 3);
        assert_eq!(custom[2]["capabilities"], custom[0]["capabilities"]);
    }
    #[test]
    fn launch_argument_tokenization_preserves_quoted_config_and_literal_shell_text() {
        assert_eq!(
            tokenize_cli_args(
                "-c 'key=two words' --flag=\"a\\\"b\" '' C:\\Users\\name $HOME $(echo hi)"
            ),
            vec![
                "-c",
                "key=two words",
                "--flag=a\"b",
                "",
                "C:\\Users\\name",
                "$HOME",
                "$(echo",
                "hi)"
            ]
        );
    }
    #[tokio::test]
    async fn requests_arriving_during_initialize_remain_available_to_the_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("provider");
        std::fs::write(&script,"#!/usr/bin/env python3\nimport sys,json\nfor line in sys.stdin:\n r=json.loads(line)\n if r.get('method')=='initialize':\n  print(json.dumps({'id':r['id'],'result':{'userAgent':'codex/0.156.1'}}),flush=True)\n  print(json.dumps({'id':'boot-request','method':'approval','params':{'command':'fixture'}}),flush=True)\n elif r.get('id')=='boot-request':\n  print(json.dumps({'method':'boot-answered','params':r['result']}),flush=True)\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let instance = CodexInstance {
            instance_id: "codex".into(),
            display_name: "Codex".into(),
            accent_color: None,
            enabled: true,
            config: serde_json::from_value(json!({"binaryPath":script,"homePath":directory.path().join("codex-home")})).unwrap(),
            environment: HashMap::new(),
        };
        let mut connection = instance.connect(directory.path()).await.unwrap();
        let event = tokio::time::timeout(HANG_BOUND, connection.events.recv())
            .await
            .unwrap()
            .unwrap();
        let ProcessEvent::Request { id, method, .. } = event else {
            panic!("startup request was dropped")
        };
        assert_eq!(method, "approval");
        connection
            .process
            .respond(id, Ok(json!({"decision":"accept"})))
            .await
            .unwrap();
        assert!(
            matches!(tokio::time::timeout(HANG_BOUND,connection.events.recv()).await.unwrap().unwrap(),ProcessEvent::Notification{method,params} if method=="boot-answered"&&params["decision"]=="accept")
        );
    }
    #[tokio::test]
    async fn fake_app_server_discovers_account_paginated_models_and_workspace_skills() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("provider");
        std::fs::write(&script,"#!/usr/bin/env python3\nimport sys,json,os\ninitialized=False\nfor line in sys.stdin:\n r=json.loads(line); method=r.get('method'); p=r.get('params') or {}\n if method=='initialized': initialized=True; continue\n if method=='initialize': result={'userAgent':'codex/0.156.1 native'}\n elif not initialized: sys.exit(5)\n elif method=='account/read': result={'account':{'type':'chatgpt','planType':'plus','email':'fixture@example.test'},'requiresOpenaiAuth':True}\n elif method=='model/list':\n  slug='gpt-6-astra' if p.get('cursor')=='next' else 'gpt-5.6-sol'\n  result={'data':[{'model':slug,'displayName':slug,'isDefault':slug=='gpt-5.6-sol','defaultReasoningEffort':'low','supportedReasoningEfforts':[{'reasoningEffort':'low'},{'reasoningEffort':'medium'}]}],'nextCursor':None if p.get('cursor') else 'next'}\n elif method=='skills/list': result={'data':[{'cwd':p['cwds'][0],'skills':[{'name':'fixture','path':os.environ['CODEX_HOME']+'/SKILL.md','enabled':True,'interface':{'displayName':'Fixture Skill','shortDescription':'Test'}}]}]}\n elif method=='account/rateLimits/read': result={'rateLimits':{'limitId':'other','primary':{'usedPercent':1}},'rateLimitsByLimitId':{'codex':{'limitId':'codex','planType':'plus','primary':{'usedPercent':42.5,'windowDurationMins':300,'resetsAt':1800003600},'secondary':{'usedPercent':10,'windowDurationMins':10080}}},'rateLimitResetCredits':{'availableCount':2,'credits':[{'status':'available','expiresAt':1800900000}]}}\n else: sys.exit(6)\n print(json.dumps({'id':r['id'],'result':result}),flush=True)\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let instance = CodexInstance {
            instance_id: "codex_work".into(),
            display_name: "Work Codex".into(),
            accent_color: None,
            enabled: true,
            config: CodexConfig {
                binary_path: script.to_string_lossy().parse().unwrap(),
                home_path: "~/fixture-home".parse().unwrap(),
                ..Default::default()
            },
            environment: HashMap::from([(
                "HOME".into(),
                directory.path().to_string_lossy().into_owned(),
            )]),
        };
        let snapshot = instance.discover(directory.path()).await.unwrap();
        assert_eq!(snapshot["instanceId"], "codex_work");
        assert_eq!(snapshot["driver"], "codex");
        assert_eq!(snapshot["status"], "ready");
        assert_eq!(snapshot["auth"]["label"], "ChatGPT Plus Subscription");
        assert_eq!(snapshot["version"], "0.156.1");
        assert_eq!(snapshot["models"].as_array().unwrap().len(), 2);
        assert_eq!(snapshot["models"][1]["isDefault"], true);
        assert_eq!(snapshot["skills"][0]["displayName"], "Fixture Skill");
        // `account/rateLimits/read`: the `codex` bucket is the main allowance, not the legacy snapshot that names another limit.
        let limits = &snapshot["usageLimits"];
        assert_eq!(limits["checkedAt"], snapshot["checkedAt"]);
        assert_eq!(limits["windows"], json!([{"id":"primary","kind":"session","label":"Session","usedPercent":42.5,"windowDurationMins":300,"resetsAt":"2027-01-15T09:00:00.000Z"},{"id":"secondary","kind":"weekly","label":"Weekly","usedPercent":10,"windowDurationMins":10080}]));
        assert_eq!(limits["resetCredits"], json!({"availableCount":2,"nextExpiresAt":"2027-01-25T18:00:00.000Z"}));
        assert_eq!(rate_limits_failure_message(&ProcessError::Remote { code: -32601, message: String::new(), data: Value::Null }), "Codex could not read usage (JSON-RPC -32601).");
        assert_eq!(rate_limits_failure_message(&ProcessError::Timeout { method: "account/rateLimits/read".into() }), "Codex did not answer the usage request.");
        assert_eq!(
            snapshot["skills"][0]["path"],
            directory
                .path()
                .join("fixture-home/SKILL.md")
                .to_string_lossy()
                .as_ref()
        );
    }
}
