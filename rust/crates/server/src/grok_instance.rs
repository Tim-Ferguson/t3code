//! Grok provider instance: executable resolution, version and health, authentication status, models,
//! slash commands, skills and usage limits. Mirrors `GrokProvider.ts`, `GrokDriver.ts`, `GrokSkills.ts`
//! and `grokUsageLimits.ts`.
use crate::{acp_runtime::AcpInstance, claude_models, grok_acp, provider_process::{ProcessError, ProcessOptions}};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{collections::HashMap, path::{Path, PathBuf}, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub type GrokConfig = t3_contracts::GrokSettings;
const VERSION_TIMEOUT: Duration = Duration::from_secs(4);
/// `AUTH_PROBE_TIMEOUT_MS`.
const MODELS_TIMEOUT: Duration = Duration::from_secs(10);
/// `initialize` is a single local round trip, so this is generous even on slow machines.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(8);
const SKILLS_TIMEOUT: Duration = Duration::from_secs(4);
const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const COMPACT: fn() -> Value = || json!({"name":"compact","description":"Summarize the conversation and reduce context usage"});

#[derive(Clone, Debug)]
pub struct GrokInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: GrokConfig,
    pub environment: HashMap<String, String>,
}
/// What one CLI invocation printed.
struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}
impl GrokInstance {
    /// The configured binary with a leading `~` expanded; `grok` when the setting is empty.
    pub fn binary_path(&self) -> PathBuf {
        let configured = self.config.binary_path.as_str();
        let configured = if configured.is_empty() { "grok" } else { configured };
        let home = self.environment.get("HOME").cloned().or_else(|| std::env::var("HOME").ok());
        match (configured.strip_prefix("~/").or((configured == "~").then_some("")), home) {
            (Some(rest), Some(home)) => Path::new(&home).join(rest),
            _ => PathBuf::from(configured),
        }
    }
    /// The environment every Grok process of this instance runs with.
    pub fn process_environment(&self) -> HashMap<String, String> {
        self.environment.clone()
    }
    /// The generic ACP instance this driver starts sessions through: a local agent whose command is the
    /// Grok binary. It has its own coordinator, so Grok's models and commands never reach the registry
    /// agents' live-configuration publishing.
    pub fn acp_instance(&self) -> AcpInstance {
        let config = serde_json::from_value(json!({"enabled":self.enabled,"source":"local","commandPath":self.binary_path().to_string_lossy(),"commandArgs":[],"authMethodId":grok_acp::auth_method_id(&self.environment)})).expect("local ACP settings");
        AcpInstance { instance_id: self.instance_id.clone(), display_name: self.display_name.clone(), accent_color: self.accent_color.clone(), enabled: self.enabled, config, environment: self.process_environment(), catalog: None, coordinator: Default::default() }
    }
    /// How `grok agent stdio` launches for a thread's runtime mode (see `grok_acp::spawn_args`).
    pub fn session_process(&self, cwd: &Path, launch_mode: Option<&str>) -> ProcessOptions {
        ProcessOptions { binary: self.binary_path(), args: grok_acp::spawn_args(launch_mode), cwd: cwd.into(), environment: grok_acp::spawn_environment(&self.process_environment()) }
    }
    async fn run(&self, args: &[&str], cwd: Option<&Path>, limit: Duration) -> Result<Option<Output>, std::io::Error> {
        let mut command = Command::new(self.binary_path());
        command.args(args).envs(self.process_environment()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let collect = async {
            let (mut out, mut err) = (String::new(), String::new());
            let _ = tokio::join!(stdout.read_to_string(&mut out), stderr.read_to_string(&mut err));
            let status = child.wait().await?;
            Ok::<_, std::io::Error>(Output { code: status.code(), stdout: out, stderr: err })
        };
        match tokio::time::timeout(limit, collect).await {
            Ok(output) => output.map(Some),
            Err(_) => Ok(None),
        }
    }
    fn custom_models(&self) -> Vec<Value> {
        serde_json::to_value(&self.config.custom_models).ok().and_then(|models| models.as_array().cloned()).unwrap_or_default()
    }
    fn fallback_models(&self) -> Vec<Value> {
        claude_models::provider_models(vec![json!({"slug":grok_acp::DEFAULT_MODEL_SLUG,"name":"Grok Build","isCustom":false,"capabilities":{"optionDescriptors":[]}})], &self.custom_models())
    }
    /// The skills `grok inspect --json` reports for `cwd` (`discoverGrokSkills`); `None` when the probe failed.
    pub async fn discover_skills(&self, cwd: Option<&Path>) -> Option<Vec<Value>> {
        let output = self.run(&["inspect", "--json"], cwd, SKILLS_TIMEOUT).await.ok()??;
        if output.code != Some(0) {
            return None;
        }
        decode_inspect_skills(&output.stdout)
    }
    pub async fn discover(&self, cwd: &Path) -> Result<Value, ProcessError> {
        self.discover_scoped(cwd, true).await
    }
    /// `include_account` is false for a one-workspace scan: the account's usage belongs to the machine snapshot.
    pub async fn discover_scoped(&self, cwd: &Path, include_account: bool) -> Result<Value, ProcessError> {
        let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut snapshot = json!({
            "instanceId":self.instance_id,"driver":"grok","displayName":self.display_name,"enabled":self.enabled,
            "installed":false,"version":null,"status":"warning","auth":{"status":"unknown"},"checkedAt":checked_at,
            "availability":"available","models":self.fallback_models(),"slashCommands":[],"skills":[],
            "supportsConversationRollback":false,"showInteractionModeToggle":false,"supportedRuntimeModes":grok_acp::SUPPORTED_RUNTIME_MODES,
            "continuation":{"groupKey":format!("grok:instance:{}", self.instance_id)}
        });
        if let Some(accent) = &self.accent_color {
            snapshot["accentColor"] = json!(accent);
        }
        if !self.enabled {
            snapshot["status"] = json!("disabled");
            snapshot["message"] = json!("Grok is disabled in T3 Code settings.");
            return validate(snapshot);
        }
        let version = match self.run(&["--version"], None, VERSION_TIMEOUT).await {
            Err(error) => {
                let missing = error.kind() == std::io::ErrorKind::NotFound;
                tracing::warn!(%error, "Grok CLI health check failed.");
                snapshot["installed"] = json!(!missing);
                snapshot["status"] = json!("error");
                snapshot["message"] = json!(if missing { "Grok CLI (`grok`) is not installed or not on PATH." } else { "Failed to execute Grok CLI health check." });
                return validate(snapshot);
            }
            Ok(None) => {
                snapshot["installed"] = json!(true);
                snapshot["status"] = json!("error");
                snapshot["message"] = json!("Grok CLI is installed but timed out while running `grok --version`.");
                return validate(snapshot);
            }
            Ok(Some(output)) => {
                let version = crate::claude_instance::parse_version(&format!("{}\n{}", output.stdout, output.stderr));
                snapshot["installed"] = json!(true);
                snapshot["version"] = json!(version);
                if output.code != Some(0) {
                    tracing::warn!(exit_code = ?output.code, "Grok CLI version probe exited with a non-zero status.");
                    snapshot["status"] = json!("error");
                    snapshot["message"] = json!("Grok CLI is installed but failed to run.");
                    return validate(snapshot);
                }
                version
            }
        };
        let _ = version;
        // `grok models` reports login state and model slugs without starting the agent. Only a clean exit is
        // parsed: a failed invocation prints help or error text that is neither slugs nor a login verdict.
        let models_output = self.run(&["models"], None, MODELS_TIMEOUT).await.ok().flatten().filter(|output| output.code == Some(0));
        if models_output.is_none() {
            tracing::warn!("Grok CLI model listing failed or timed out.");
        }
        let (authenticated, cli_models) = models_output.map(|output| parse_models_output(&format!("{}\n{}", output.stdout, output.stderr))).unwrap_or((None, vec![]));
        let mut auth = if self.environment.get(grok_acp::API_KEY_ENV).is_some_and(|key| !t3_contracts::trim_wire_string(key).is_empty()) {
            json!({"status":"authenticated","type":"api_key","label":"xAI API key"})
        } else {
            match authenticated {
                Some(true) => json!({"status":"authenticated","type":"cached_token","label":"Grok account"}),
                Some(false) => json!({"status":"unauthenticated"}),
                None => json!({"status":"unknown"}),
            }
        };
        let skills = self.discover_skills(Some(cwd)).await.unwrap_or_default();
        let probe = tokio::time::timeout(INITIALIZE_TIMEOUT, self.probe_initialize()).await;
        let (acp_models, slash_commands, acp_failed) = match probe {
            Ok(Ok((models, commands))) => (models, commands, false),
            Ok(Err(error)) => {
                tracing::warn!(%error, "Grok ACP initialize probe failed.");
                (vec![], vec![], true)
            }
            Err(_) => {
                tracing::warn!("Grok ACP initialize probe timed out.");
                (vec![], vec![], true)
            }
        };
        let discovered = if acp_models.is_empty() { cli_models } else { acp_models };
        snapshot["models"] = json!(if discovered.is_empty() { self.fallback_models() } else { claude_models::provider_models(discovered, &self.custom_models()) });
        snapshot["skills"] = json!(skills);
        if auth["status"] == "unauthenticated" {
            snapshot["status"] = json!("error");
            snapshot["auth"] = auth;
            snapshot["message"] = json!("Grok CLI is installed but not logged in. Run `grok login`.");
            return validate(snapshot);
        }
        snapshot["slashCommands"] = json!(if acp_failed { vec![COMPACT()] } else { slash_commands });
        snapshot["status"] = json!(if acp_failed { "warning" } else { "ready" });
        if acp_failed {
            snapshot["message"] = json!("Grok CLI is installed but ACP initialize failed. Model options may be incomplete.");
        }
        // The account and its usage are read once the login is known to work (`readGrokAccount`).
        if include_account && auth["status"] == "authenticated" {
            let (email, usage_limits) = self.read_account(BILLING_URL).await;
            if let Some(email) = email {
                auth["email"] = json!(email);
            }
            snapshot["usageLimits"] = usage_limits;
        }
        snapshot["auth"] = auth;
        validate(snapshot)
    }
    /// Reads model and command metadata from `initialize._meta`. This never calls `authenticate` or
    /// `session/new`, so it cannot open a browser login or boot the workspace's MCP servers.
    async fn probe_initialize(&self) -> Result<(Vec<Value>, Vec<Value>), String> {
        let instance = self.acp_instance();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let process = ProcessOptions { binary: self.binary_path(), args: grok_acp::spawn_args(None), cwd: cwd.clone(), environment: grok_acp::spawn_environment(&self.process_environment()) };
        let peer = crate::acp_peer::ProcessPeer::spawn(process).map_err(|error| error.to_string())?;
        let services = instance.services_for(&cwd, crate::acp_client_callbacks::policy(&json!("approval-required"), &cwd));
        let request = initialize_request("t3-code-provider-probe", true);
        let initialized = Box::pin(instance.initialize_only(peer, true, services, None, Some(request))).await.map_err(|error| error.to_string())?;
        let meta = serde_json::to_value(&initialized.initialize).map_err(|error| error.to_string())?;
        initialized.shutdown().await;
        Ok((models_from_model_state(&meta["_meta"]["modelState"]), slash_commands_from_initialize(&meta["_meta"])))
    }
    /// `readGrokAccount`: the default grok.com login's email and usage limits.
    pub async fn read_account(&self, billing_url: &str) -> (Option<String>, Value) {
        let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let failed = crate::claude_usage_limits::make_unavailable_usage_limits(&checked_at, "probeFailed", Some("Grok could not read usage limits."));
        let environment = self.process_environment();
        let credential = match tokio::time::timeout(ACCOUNT_TIMEOUT, read_credential(&environment)).await {
            Ok(Ok(credential)) => credential,
            _ => return (None, failed),
        };
        let email = credential.as_ref().and_then(|credential| non_empty(&credential["email"]));
        let Some(token) = credential.as_ref().and_then(|credential| non_empty(&credential["key"])) else {
            return (email, crate::claude_usage_limits::make_unavailable_usage_limits(&checked_at, "unsupported", None));
        };
        // A failed quota request still knows which account it asked about.
        let usage = async {
            let response = reqwest::Client::new().get(billing_url).bearer_auth(&token).send().await.ok()?.error_for_status().ok()?;
            let body: Value = response.json().await.ok()?;
            Some(usage_response_to_limits(&body, &checked_at))
        };
        let limits = tokio::time::timeout(ACCOUNT_TIMEOUT, usage).await.ok().flatten().unwrap_or(failed);
        (email, limits)
    }
}
/// The `initialize` request Grok gets: the client types it prompts on, no file system or terminal.
pub fn initialize_request(client_name: &str, discovery: bool) -> t3_acp::types::InitializeRequest {
    use t3_acp::types::{ClientCapabilities, Implementation, InitializeRequest, Optional};
    InitializeRequest {
        client_info: Optional::Value(Implementation { name: client_name.into(), version: if discovery { "0.0.0".into() } else { env!("CARGO_PKG_VERSION").into() }, title: Optional::Missing, meta: Optional::Missing }),
        client_capabilities: Some(serde_json::from_value::<ClientCapabilities>(json!({"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,"elicitation":{"form":{}}})).expect("Grok client capabilities")),
        meta: Optional::Value(grok_acp::INITIALIZE_META().as_object().cloned().unwrap_or_default()),
        ..Default::default()
    }
}
fn validate(snapshot: Value) -> Result<Value, ProcessError> {
    let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}
fn non_empty(value: &Value) -> Option<String> {
    value.as_str().map(t3_contracts::trim_wire_string).filter(|text| !text.is_empty()).map(str::to_owned)
}
/// `parseGrokModelsCliOutput`. The command exits 0 whether or not the user is logged in, so the text is
/// the only signal: a `You are logged in` line, `Not authenticated`, or neither.
pub fn parse_models_output(output: &str) -> (Option<bool>, Vec<Value>) {
    let lower = output.to_lowercase();
    let authenticated = if lower.contains("you are logged in") { Some(true) } else if lower.contains("not authenticated") || lower.contains("not logged in") { Some(false) } else { None };
    let bullet = regex::Regex::new(r"^\s*[*-]\s+(\S+)(.*)$").unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut models = vec![];
    for line in output.split('\n').map(|line| line.trim_end_matches('\r')) {
        let Some(captures) = bullet.captures(line) else { continue };
        let slug = grok_acp::base_model_id(Some(&captures[1]));
        if !seen.insert(slug.clone()) {
            continue;
        }
        let mut model = json!({"slug":slug,"name":display_name(&slug),"isCustom":false,"capabilities":{"optionDescriptors":[]}});
        if captures[2].to_lowercase().contains("(default)") {
            model["isDefault"] = json!(true);
        }
        models.push(model);
    }
    (authenticated, models)
}
fn display_name(slug: &str) -> String {
    slug.split(['-', '_']).map(|part| if part.to_lowercase() == "grok" { "Grok" } else { part }).collect::<Vec<_>>().join(" ")
}
fn text(value: &Value) -> Option<String> {
    non_empty(value)
}
/// `grokReasoningOptionsFromModel` + `buildGrokModelCapabilities`: the reasoning-effort select a model advertises.
pub fn model_capabilities(model: &Value) -> Value {
    let meta = &model["_meta"];
    if !meta.is_object() || meta["supportsReasoningEffort"] == false {
        return json!({"optionDescriptors":[]});
    }
    let current = text(&meta["reasoningEffort"]);
    let mut seen = vec![];
    let mut options: Vec<(String, String, Option<String>, bool)> = vec![];
    for entry in meta["reasoningEfforts"].as_array().into_iter().flatten().filter(|entry| entry.is_object()) {
        let valid = |value: Option<String>| value.filter(|value| grok_acp::is_valid_reasoning_effort(value));
        let Some(value) = valid(text(&entry["value"])).or_else(|| valid(text(&entry["id"]))) else { continue };
        if seen.contains(&value) {
            continue;
        }
        seen.push(value.clone());
        options.push((value.clone(), text(&entry["label"]).unwrap_or(value), text(&entry["description"]), entry["default"] == true || entry["isDefault"] == true));
    }
    let current_value = current.filter(|current| options.iter().any(|option| &option.0 == current));
    let defaults: Vec<&(String, String, Option<String>, bool)> = options.iter().filter(|option| option.3).collect();
    let selected_default = defaults.iter().find(|option| Some(&option.0) == current_value.as_ref()).map(|option| option.0.clone()).or_else(|| defaults.first().map(|option| option.0.clone()));
    if options.is_empty() {
        return json!({"optionDescriptors":[]});
    }
    let rendered: Vec<Value> = options
        .iter()
        .map(|(value, label, description, _)| {
            let mut option = json!({"id":value,"label":label});
            if let Some(description) = description {
                option["description"] = json!(description);
            }
            if Some(value) == selected_default.as_ref() {
                option["isDefault"] = json!(true);
            }
            option
        })
        .collect();
    let mut descriptor = json!({"id":"reasoningEffort","label":"Reasoning","type":"select","options":rendered});
    if let Some(value) = current_value.or(selected_default) {
        descriptor["currentValue"] = json!(value);
    }
    json!({"optionDescriptors":[descriptor]})
}
/// `buildGrokModelsFromSessionModelState`: the models the agent advertises, its current one the default.
pub fn models_from_model_state(state: &Value) -> Vec<Value> {
    let Some(available) = state["availableModels"].as_array().filter(|models| !models.is_empty()) else { return vec![] };
    let current = state["currentModelId"].as_str().map(t3_contracts::trim_wire_string).unwrap_or("");
    let mut seen = std::collections::HashSet::new();
    available
        .iter()
        .filter_map(|model| {
            let slug = grok_acp::base_model_id(model["modelId"].as_str());
            if slug.is_empty() || !seen.insert(slug.clone()) {
                return None;
            }
            let name = model["name"].as_str().map(t3_contracts::trim_wire_string).filter(|name| !name.is_empty()).unwrap_or(&slug).to_owned();
            let mut entry = json!({"slug":slug,"name":name,"isCustom":false,"capabilities":model_capabilities(model)});
            if model["modelId"].as_str().map(t3_contracts::trim_wire_string) == Some(current) {
                entry["isDefault"] = json!(true);
            }
            Some(entry)
        })
        .collect()
}
/// `grokSlashCommandsFromInitialize`: `/compact` first, then the agent's own commands (minus the ones T3 owns).
pub fn slash_commands_from_initialize(meta: &Value) -> Vec<Value> {
    let mut by_name: Vec<(String, Value)> = vec![("compact".into(), COMPACT())];
    for entry in meta["availableCommands"].as_array().into_iter().flatten() {
        let (Some(name), Some(description)) = (entry["name"].as_str(), entry["description"].as_str()) else { continue };
        if !entry["input"].is_null() && entry.get("input").is_some() && entry["input"]["hint"].as_str().is_none() {
            continue;
        }
        let name = t3_contracts::trim_wire_string(name);
        // Permission changes must go through T3, and Grok's /context completes without output.
        if name.is_empty() || matches!(name.to_lowercase().as_str(), "always-approve" | "context") {
            continue;
        }
        let description = t3_contracts::trim_wire_string(description);
        let hint = entry["input"]["hint"].as_str().map(t3_contracts::trim_wire_string).filter(|hint| !hint.is_empty());
        let mut command = json!({"name":name});
        if !description.is_empty() {
            command["description"] = json!(description);
        }
        if let Some(hint) = hint {
            command["input"] = json!({"hint":hint});
        }
        match by_name.iter_mut().find(|(existing, _)| existing == name) {
            Some(existing) => existing.1 = command,
            None => by_name.push((name.to_owned(), command)),
        }
    }
    by_name.into_iter().map(|(_, command)| command).collect()
}
/// `decodeGrokInspectSkills`.
pub fn decode_inspect_skills(stdout: &str) -> Option<Vec<Value>> {
    let parsed: Value = serde_json::from_str(stdout).ok()?;
    let entries = parsed.as_object()?.get("skills")?.as_array()?;
    let mut by_name: Vec<(String, Value)> = vec![];
    for entry in entries.iter().filter(|entry| entry.is_object()) {
        let name = entry["name"].as_str().map(t3_contracts::trim_wire_string).unwrap_or("");
        let source = &entry["source"];
        let path = source["path"].as_str().map(t3_contracts::trim_wire_string).unwrap_or("");
        if name.is_empty() || path.is_empty() {
            continue;
        }
        let scope = source["type"].as_str().map(t3_contracts::trim_wire_string).unwrap_or("");
        let description = entry["description"].as_str().map(t3_contracts::trim_wire_string).unwrap_or("");
        let mut skill = json!({"name":name,"path":path,"enabled":entry["userInvocable"] != false});
        if !scope.is_empty() {
            skill["scope"] = json!(scope);
        }
        if !description.is_empty() {
            skill["description"] = json!(description);
        }
        match by_name.iter_mut().find(|(existing, _)| existing == name) {
            Some(existing) => existing.1 = skill,
            None => by_name.push((name.to_owned(), skill)),
        }
    }
    let mut skills: Vec<Value> = by_name.into_iter().map(|(_, skill)| skill).collect();
    skills.sort_by(|left, right| left["name"].as_str().unwrap_or("").cmp(right["name"].as_str().unwrap_or("")));
    Some(skills)
}
/// `grokUsageResponseToLimits`.
pub fn usage_response_to_limits(response: &Value, checked_at: &str) -> Value {
    let Some(used) = response["config"]["creditUsagePercent"].as_f64().filter(|used| used.is_finite()) else {
        // A billing read that succeeded without a percentage is an account with nothing metered yet.
        return crate::claude_usage_limits::make_usage_limits(checked_at, vec![]);
    };
    let period = &response["config"]["currentPeriod"];
    let kind_name = period["type"].as_str().map(|kind| kind.strip_prefix("USAGE_PERIOD_TYPE_").unwrap_or(kind));
    let (kind, label) = match kind_name {
        Some("WEEKLY") => ("weekly", "Weekly"),
        Some("MONTHLY") => ("monthly", "Monthly"),
        _ => ("other", "Subscription"),
    };
    let clamped = crate::claude_usage_limits::clamp_percent(used);
    let percent = if clamped.fract() == 0.0 { json!(clamped as i64) } else { json!(clamped) };
    let mut window = json!({"id":"subscription","kind":kind,"label":label,"usedPercent":percent});
    if let Some(end) = period["end"].as_str().and_then(|end| chrono::DateTime::parse_from_rfc3339(end).ok()) {
        window["resetsAt"] = json!(end.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Millis, true));
    }
    crate::claude_usage_limits::make_usage_limits(checked_at, vec![window])
}
/// `readGrokCredential`: the grok.com login the CLI uses by default, or none when the CLI is configured to
/// pick another account, endpoint or an API key. Never reads anything but the Grok home.
async fn read_credential(environment: &HashMap<String, String>) -> Result<Option<Value>, std::io::Error> {
    let get = |name: &str| environment.get(name).cloned().or_else(|| std::env::var(name).ok()).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty());
    if get(grok_acp::API_KEY_ENV).is_some() {
        return Ok(None);
    }
    for name in ["GROK_OIDC_ISSUER", "GROK_OIDC_CLIENT_ID", "GROK_OAUTH2_ISSUER", "GROK_OAUTH2_CLIENT_ID", "GROK_OAUTH2_PRINCIPAL_TYPE", "GROK_OAUTH2_PRINCIPAL_ID", "GROK_AUTH_PROVIDER_COMMAND", "GROK_LOCAL_AUTH", "GROK_CLI_CHAT_PROXY_BASE_URL", "GROK_MODELS_BASE_URL", "GROK_CONFIG", "GROK_CONFIG_PATH"] {
        if get(name).is_some() {
            return Ok(None);
        }
    }
    let home = get("GROK_HOME").map(PathBuf::from).or_else(|| get("HOME").or_else(|| get("USERPROFILE")).map(|home| Path::new(&home).join(".grok"))).unwrap_or_else(|| PathBuf::from(".grok"));
    let read = |path: PathBuf| async move {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error),
        }
    };
    let sections = regex::Regex::new(r#"(?m)^\s*(?:\[\[?\s*)?["']?(?:auth|grok_com_config|endpoints)["']?\s*[.\]=]"#).unwrap();
    for path in [home.join("config.toml"), home.join("managed_config.toml"), home.join("requirements.toml"), PathBuf::from("/etc/grok/managed_config.toml"), PathBuf::from("/etc/grok/requirements.toml")] {
        // These sections can change the selected account or endpoint. Leave custom deployments to the CLI.
        if sections.is_match(&read(path).await?) {
            return Ok(None);
        }
    }
    let contents = match get("GROK_AUTH") {
        Some(contents) => contents,
        None => {
            let file = read(home.join("auth.json")).await?;
            if file.is_empty() { "{}".into() } else { file }
        }
    };
    let credentials: Value = serde_json::from_str(&contents).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    // Never pick an arbitrary account from other deployments stored in the same file.
    let credential = ["https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828", "https://accounts.x.ai/sign-in"].iter().find_map(|key| credentials.get(*key).filter(|credential| !credential.is_null())).cloned();
    Ok(credential.filter(|credential| credential["auth_mode"] != "api_key"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/grok-pure.json")).unwrap()
    }
    #[test]
    fn model_listing_and_session_model_state_decode_as_the_source_does() {
        let provider = &fixture()["provider"];
        for case in provider["modelsOutputs"].as_array().unwrap() {
            let (authenticated, models) = parse_models_output(case["output"].as_str().unwrap());
            assert_eq!(json!({"authenticated":authenticated,"models":models}), case["parsed"], "{}", case["output"]);
        }
        assert_eq!(json!(models_from_model_state(&provider["modelState"])), provider["models"]);
        for case in provider["modelStates"].as_array().unwrap() {
            assert_eq!(json!(models_from_model_state(&case["state"])), case["models"], "{}", case["state"]);
        }
    }
    #[test]
    fn slash_commands_and_usage_decode_as_the_source_does() {
        let provider = &fixture()["provider"];
        for case in provider["slashCommands"].as_array().unwrap() {
            assert_eq!(json!(slash_commands_from_initialize(&case["initialize"]["_meta"])), case["commands"], "{}", case["initialize"]);
        }
        for case in provider["usage"].as_array().unwrap() {
            assert_eq!(usage_response_to_limits(&case["response"], "2026-10-09T00:00:00.000Z"), case["limits"], "{}", case["response"]);
        }
    }
    #[test]
    fn inspect_output_becomes_sorted_skills_with_the_last_duplicate_winning() {
        let output = json!({"skills":[
            {"name":"zeta","description":" Z ","source":{"type":"user","path":"/s/zeta/SKILL.md"},"userInvocable":true},
            {"name":"alpha","source":{"type":"project","path":"/s/alpha/SKILL.md"},"userInvocable":false},
            {"name":"alpha","source":{"path":"/other/SKILL.md"}},
            {"name":"nopath","source":{}},{"source":{"path":"/x"}},"bad"
        ]});
        let skills = decode_inspect_skills(&output.to_string()).unwrap();
        assert_eq!(json!(skills), json!([{"name":"alpha","path":"/other/SKILL.md","enabled":true},{"name":"zeta","path":"/s/zeta/SKILL.md","enabled":true,"scope":"user","description":"Z"}]));
        assert!(decode_inspect_skills("not json").is_none());
        assert!(decode_inspect_skills("{\"skills\":1}").is_none());
    }
    #[test]
    fn spawn_environment_adds_only_the_oauth_referrer() {
        let environment = HashMap::from([("XAI_API_KEY".to_owned(), "secret".to_owned())]);
        let spawned = grok_acp::spawn_environment(&environment);
        assert_eq!(spawned.get("GROK_OAUTH2_REFERRER").map(String::as_str), Some("t3code"));
        assert_eq!(spawned.get("XAI_API_KEY").map(String::as_str), Some("secret"));
        assert_eq!(grok_acp::auth_method_id(&environment), "xai.api_key");
        assert_eq!(grok_acp::auth_method_id(&HashMap::new()), "cached_token");
    }
}
