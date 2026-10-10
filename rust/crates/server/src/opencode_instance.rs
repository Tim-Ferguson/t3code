//! OpenCode provider instance: executable resolution, version and server probe, authentication status, the model
//! list with options, agents, skills and commands, usage limits and the update target. Mirrors
//! `OpenCodeDriver.ts`, `OpenCodeProvider.ts`, `opencodeVersionProbe.ts`, `opencodeRuntime.ts` (inventory) and
//! `openCodeUsageLimits.ts`. A 1.x install is driven over the SDK's routes and a 2.x one over `/api/*`; the
//! probe says which.
use crate::{
    opencode_api::Api,
    opencode_server::{Connection, Generation, Probed, ServerError, ServerOwner, SpawnConfig, MINIMUM_VERSION, probe_binary, probe_server, version_at_least},
    provider_process::ProcessError,
    scheduled_tasks::SchedulerClock,
};
use chrono::{SecondsFormat, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub type OpenCodeConfig = t3_contracts::OpenCodeSettings;
const DRIVER: &str = "opencode";
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_POLL: Duration = Duration::from_millis(250);
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(20);
const SCAN_TIMEOUT: Duration = Duration::from_secs(10);
/// The supported runtime modes of a 2.x session: every mode maps onto session permission rules.
const V2_RUNTIME_MODES: [&str; 4] = ["approval-required", "auto-accept-edits", "auto", "full-access"];

#[derive(Clone)]
pub struct OpenCodeInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: OpenCodeConfig,
    pub environment: HashMap<String, String>,
    runtime: Arc<Mutex<Runtime>>,
}
/// What the instance learned about its install: the last successful probe, and the server it owns.
#[derive(Default)]
struct Runtime {
    probed: Option<Probed>,
    owner: Option<ServerOwner>,
}
impl std::fmt::Debug for OpenCodeInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OpenCodeInstance").field("instance_id", &self.instance_id).finish_non_exhaustive()
    }
}

impl OpenCodeInstance {
    pub fn new(instance_id: String, display_name: String, accent_color: Option<String>, enabled: bool, config: OpenCodeConfig, environment: HashMap<String, String>) -> Self {
        Self { instance_id, display_name, accent_color, enabled, config, environment, runtime: Default::default() }
    }
    /// The configured binary with a leading `~` expanded.
    pub fn binary_path(&self) -> PathBuf {
        let configured = self.config.binary_path.as_str();
        let configured = if configured.is_empty() { "opencode" } else { configured };
        let home = self.environment.get("HOME").cloned().or_else(|| std::env::var("HOME").ok());
        match (configured.strip_prefix("~/").or((configured == "~").then_some("")), home) {
            (Some(rest), Some(home)) => Path::new(&home).join(rest),
            _ => PathBuf::from(configured),
        }
    }
    /// A configured server: the instance runs nothing of its own.
    pub fn server_url(&self) -> Option<&str> {
        Some(self.config.server_url.as_str()).filter(|url| !url.trim().is_empty())
    }
    /// The server's environment on top of the instance's.
    pub fn process_environment(&self) -> HashMap<String, String> {
        crate::provider_manage::merged_environment(&self.environment)
    }
    pub fn custom_models(&self) -> Vec<Value> {
        serde_json::to_value(&self.config.custom_models).ok().and_then(|models| models.as_array().cloned()).unwrap_or_default()
    }
    /// The last successful probe, without probing.
    pub fn last_probe(&self) -> Option<Probed> {
        self.runtime.lock().unwrap().probed.clone()
    }
    /// The generation and version of the install, remembered after the first success. A failed probe is not remembered.
    pub async fn probe(&self, fresh: bool) -> Result<Probed, ServerError> {
        if !fresh {
            if let Some(known) = self.last_probe() {
                return Ok(known);
            }
        }
        let found = match self.server_url() {
            Some(url) => probe_server(url, self.config.server_password.as_str()).await?,
            None => probe_binary(&self.binary_path(), &self.process_environment()).await?,
        };
        let mut runtime = self.runtime.lock().unwrap();
        // An install replaced by the other major version is spawned under its own protocol.
        if runtime.probed.as_ref().is_none_or(|known| known.generation != found.generation) {
            runtime.owner = None;
        }
        runtime.probed = Some(found.clone());
        Ok(found)
    }
    fn owner(&self, generation: Generation) -> ServerOwner {
        let mut runtime = self.runtime.lock().unwrap();
        runtime
            .owner
            .get_or_insert_with(|| {
                let password = Some(self.config.server_password.as_str().to_owned()).filter(|password| !password.is_empty());
                ServerOwner::new(SpawnConfig { binary: self.binary_path(), environment: self.process_environment(), generation, password })
            })
            .clone()
    }
    /// A verified connection to this instance's server: the configured one, or the one T3 Code owns (started first
    /// when none runs). The connection keeps a spawned server from being stopped for as long as it is held.
    pub async fn connect(&self, directory: &Path, clock: Arc<dyn SchedulerClock>) -> Result<Connection, ServerError> {
        let probed = self.probe(false).await?;
        match self.server_url() {
            Some(url) => Connection::external(url, self.config.server_password.as_str(), probed.generation).await,
            None => self.owner(probed.generation).lease(directory, clock).await,
        }
    }

    pub async fn discover(&self, cwd: &Path) -> Result<Value, ProcessError> {
        self.discover_scoped(cwd, true).await
    }
    /// `include_usage` is false for a one-workspace scan: usage windows belong to the machine's snapshot, and the
    /// workspace's skills and commands are read instead.
    pub async fn discover_scoped(&self, cwd: &Path, include_usage: bool) -> Result<Value, ProcessError> {
        let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut snapshot = json!({
            "instanceId":self.instance_id,"driver":DRIVER,"displayName":self.display_name,"enabled":self.enabled,
            "installed":false,"version":null,"status":"warning","auth":{"status":"unknown"},"checkedAt":checked_at,
            "availability":"available","models":self.fallback_models(),"slashCommands":[],"skills":[],
            "showInteractionModeToggle":false,
            "continuation":{"groupKey":format!("{DRIVER}:instance:{}", self.instance_id)}
        });
        if let Some(accent) = &self.accent_color {
            snapshot["accentColor"] = json!(accent);
        }
        if !self.enabled {
            snapshot["status"] = json!("disabled");
            snapshot["message"] = json!(if self.server_url().is_some() { "OpenCode is disabled in T3 Code settings. A server URL is configured." } else { "OpenCode is disabled in T3 Code settings." });
            return validate(snapshot);
        }
        let probed = match self.probe(true).await {
            Ok(probed) => probed,
            Err(error) => {
                let (installed, message) = self.describe_failure(&error.0, "version");
                snapshot["installed"] = json!(installed);
                snapshot["status"] = json!("error");
                snapshot["message"] = json!(message);
                return validate(snapshot);
            }
        };
        snapshot["installed"] = json!(true);
        snapshot["version"] = json!(probed.version);
        let clock: Arc<dyn SchedulerClock> = Arc::new(crate::scheduled_tasks::SystemClock);
        if probed.generation == Generation::V2 {
            self.check_v2(&mut snapshot, &probed, cwd, include_usage, clock).await;
        } else {
            self.check_v1(&mut snapshot, &probed, cwd, clock).await;
        }
        if include_usage && snapshot["status"] != "disabled" {
            snapshot["usageLimits"] = read_go_usage_limits(self).await;
        }
        validate(snapshot)
    }
    fn fallback_models(&self) -> Vec<Value> {
        provider_models(vec![], &self.custom_models())
    }

    /// `formatOpenCodeProbeError`: whether the install exists, and the message that explains the failure.
    fn describe_failure(&self, detail: &str, phase: &str) -> (bool, String) {
        let lower = detail.to_lowercase();
        if let Some(url) = self.server_url() {
            let message = if ["401", "403", "unauthorized", "forbidden"].iter().any(|word| lower.contains(word)) {
                "OpenCode server rejected authentication. Check the server URL and password.".to_owned()
            } else if ["econnrefused", "enotfound", "fetch failed", "networkerror", "timed out", "timeout", "socket hang up"].iter().any(|word| lower.contains(word)) {
                // The configured address can carry credentials; it is named without them.
                format!("Couldn't reach the configured OpenCode server at {}. Check that the server is running and the URL is correct.", redacted_url(url))
            } else if detail.trim().is_empty() {
                "Failed to connect to the configured OpenCode server.".to_owned()
            } else {
                detail.to_owned()
            };
            return (true, message);
        }
        if lower.contains("enoent") || lower.contains("notfound") {
            return (false, "OpenCode CLI (`opencode`) is not installed or not on PATH.".into());
        }
        let label = if phase == "inventory" { "Failed to load OpenCode provider inventory" } else { "Failed to execute OpenCode CLI health check" };
        (true, if detail.trim().is_empty() { format!("{label}.") } else { format!("{label}: {detail}") })
    }

    // ---- 2.x ----

    async fn check_v2(&self, snapshot: &mut Value, probed: &Probed, cwd: &Path, include_usage: bool, clock: Arc<dyn SchedulerClock>) {
        snapshot["showInteractionModeToggle"] = json!(true);
        snapshot["supportedRuntimeModes"] = json!(V2_RUNTIME_MODES);
        // Compaction does not depend on the model catalog.
        snapshot["slashCommands"] = json!([compact_command()]);
        let connection = match self.connect(cwd, clock).await {
            Ok(connection) => connection,
            Err(error) => {
                // The detail stays in the log: the status says only what could not be done.
                tracing::warn!(detail = %error, "OpenCode 2 server could not be reached");
                snapshot["status"] = json!("error");
                snapshot["message"] = json!("OpenCode could not load its model list.");
                return;
            }
        };
        let models = match load_v2_models(&connection.api, cwd).await {
            Ok(models) => models,
            Err(error) => {
                // The detail can carry a response body; it stays in the log, not the status.
                tracing::warn!(%error, "OpenCode 2 model list failed");
                snapshot["status"] = json!("error");
                snapshot["message"] = json!("OpenCode could not load its model list.");
                return;
            }
        };
        let mut listed: Vec<(String, Value)> = models.iter().map(|model| (str_of(&model["name"]), json!({"slug":format!("{}/{}", str_of(&model["providerID"]), str_of(&model["id"])),"name":str_of(&model["name"]),"isCustom":false,"capabilities":v2_capabilities(model)}))).collect();
        listed.sort_by(|left, right| crate::workspace_entries::collate(&left.0, &right.0));
        snapshot["models"] = json!(provider_models(listed.into_iter().map(|(_, model)| model).collect(), &self.custom_models()));
        let count = models.len();
        if count > 0 {
            snapshot["status"] = json!("ready");
            snapshot["auth"] = json!({"status":"authenticated"});
            snapshot["message"] = json!(format!("OpenCode {} lists {count} model{}.", probed.version, if count == 1 { "" } else { "s" }));
        } else {
            snapshot["status"] = json!("warning");
            snapshot["message"] = json!("OpenCode 2 is running, but it did not list any models yet.");
        }
        if !include_usage {
            // A one-workspace scan adds that directory's skills and commands.
            if let Ok((skills, commands)) = tokio::time::timeout(INVENTORY_TIMEOUT, load_v2_workspace(&connection.api, cwd)).await {
                snapshot["skills"] = json!(skills);
                snapshot["slashCommands"] = json!(commands);
            }
        }
    }
    // ---- 1.x ----

    async fn check_v1(&self, snapshot: &mut Value, probed: &Probed, cwd: &Path, clock: Arc<dyn SchedulerClock>) {
        if !version_at_least(&probed.version, MINIMUM_VERSION) {
            snapshot["status"] = json!("error");
            snapshot["message"] = json!(format!("OpenCode v{} is too old. Upgrade to v{MINIMUM_VERSION} or newer.", probed.version));
            return;
        }
        let inventory = async {
            let connection = self.connect(cwd, clock).await.map_err(|error| error.0)?;
            load_v1_inventory(&connection.api.for_directory(&cwd.to_string_lossy())).await.map_err(|error| error.to_string())
        };
        let inventory = match tokio::time::timeout(INVENTORY_TIMEOUT, inventory).await {
            Err(_) => Err("Timed out waiting for the OpenCode server.".to_owned()),
            Ok(inventory) => inventory,
        };
        let inventory = match inventory {
            Ok(inventory) => inventory,
            Err(detail) => {
                let (installed, message) = self.describe_failure(&detail, "inventory");
                snapshot["installed"] = json!(installed);
                snapshot["status"] = json!("error");
                snapshot["message"] = json!(message);
                return;
            }
        };
        snapshot["models"] = json!(provider_models(flatten_v1_models(&inventory), &self.custom_models()));
        snapshot["skills"] = json!(v1_skills(&inventory["skills"]));
        snapshot["slashCommands"] = json!(v1_commands(&inventory["commands"]));
        let connected = inventory["providerList"]["connected"].as_array().map_or(0, Vec::len);
        let external = self.server_url().is_some();
        snapshot["status"] = json!(if connected > 0 { "ready" } else { "warning" });
        snapshot["auth"] = if connected > 0 { json!({"status":"authenticated","type":"opencode"}) } else { json!({"status":"unknown","type":"opencode"}) };
        snapshot["message"] = json!(if connected > 0 {
            format!("{connected} upstream provider{} connected through {}.", if connected == 1 { "" } else { "s" }, if external { "the configured OpenCode server" } else { "OpenCode" })
        } else if external {
            "Connected to the configured OpenCode server, but it did not report any connected upstream providers.".to_owned()
        } else {
            "OpenCode is available, but it did not report any connected upstream providers.".to_owned()
        });
    }
}

/// A server address without anything that could be a credential.
pub fn redacted_url(url: &str) -> String {
    match url::Url::parse(url.trim()) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.set_query(None);
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => "the configured address".to_owned(),
    }
}
fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or("").to_owned()
}
fn trimmed(value: &Value) -> Option<String> {
    value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned)
}
pub fn compact_command() -> Value {
    json!({"name":"compact","description":"Summarize the conversation and reduce context usage"})
}

/// `titleCaseSlug`.
fn title_case_slug(value: &str) -> String {
    value
        .split(['-', '_', '/'])
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
/// `inferDefaultVariant`.
fn infer_default_variant(provider_id: &str, variants: &[String]) -> Option<String> {
    let has = |wanted: &str| variants.iter().any(|variant| variant == wanted);
    if variants.len() == 1 {
        return variants.first().cloned();
    }
    if provider_id == "anthropic" || provider_id.starts_with("google") {
        return has("high").then(|| "high".to_owned());
    }
    if provider_id == "openai" || provider_id == "opencode" {
        return if has("medium") { Some("medium".into()) } else { has("high").then(|| "high".to_owned()) };
    }
    None
}
fn option_descriptor(id: &str, label: &str, options: Vec<(String, Option<String>)>, default: Option<&str>) -> Value {
    let options: Vec<Value> = options
        .into_iter()
        .map(|(value, name)| {
            let mut option = json!({"id":value,"label":name.unwrap_or_else(|| title_case_slug(&value))});
            if default == Some(value.as_str()) {
                option["isDefault"] = json!(true);
            }
            option
        })
        .collect();
    let mut descriptor = json!({"id":id,"label":label,"type":"select","options":options});
    if let Some(default) = default {
        descriptor["currentValue"] = json!(default);
    }
    descriptor
}
/// `DEFAULT_OPENCODE_MODEL_CAPABILITIES`: what a custom model offers.
fn default_capabilities() -> Value {
    json!({"optionDescriptors":[
        option_descriptor("variant", "Reasoning", ["low", "medium", "high", "xhigh"].map(|id| (id.to_owned(), Some(match id { "xhigh" => "Extra High".to_owned(), other => title_case_slug(other) }))).to_vec(), Some("medium")),
        option_descriptor("agent", "Agent", vec![("build".into(), Some("Build".into())), ("plan".into(), Some("Plan".into()))], Some("build")),
    ]})
}
/// Built-in models followed by the custom ones, which carry the default capabilities.
fn provider_models(builtin: Vec<Value>, custom: &[Value]) -> Vec<Value> {
    let custom: Vec<Value> = custom
        .iter()
        .map(|raw| {
            let mut record = if raw.is_string() { json!({"slug":raw}) } else { raw.clone() };
            if record.is_object() && !record["capabilities"]["optionDescriptors"].is_array() {
                record["capabilities"] = default_capabilities();
            }
            record
        })
        .collect();
    crate::claude_models::provider_models(builtin, &custom)
}

fn v2_capabilities(model: &Value) -> Value {
    let variants: Vec<String> = model["variants"].as_array().into_iter().flatten().filter_map(|variant| variant["id"].as_str().map(str::to_owned)).collect();
    let default = infer_default_variant(model["providerID"].as_str().unwrap_or(""), &variants);
    if variants.is_empty() {
        return json!({"optionDescriptors":[]});
    }
    json!({"optionDescriptors":[option_descriptor("variant", "Reasoning", variants.into_iter().map(|id| (id, None)).collect(), default.as_deref())]})
}
fn location(directory: &Path) -> Vec<(String, String)> {
    vec![("location[directory]".into(), directory.to_string_lossy().into_owned())]
}
/// A server loads its catalog lazily and lists nothing for its first few hundred milliseconds, so an empty list is
/// read again for a while (`makeOpenCode2ModelLoader`).
async fn load_v2_models(api: &Api, directory: &Path) -> Result<Vec<Value>, String> {
    let read = async {
        loop {
            let listed = api.call(Method::GET, "/api/model", &location(directory), None, Some(MODEL_LIST_TIMEOUT)).await.map_err(|error| error.to_string())?;
            let models = listed["data"].as_array().cloned().unwrap_or_default();
            if !models.is_empty() {
                return Ok(models);
            }
            tokio::time::sleep(MODEL_POLL).await;
        }
    };
    match tokio::time::timeout(MODEL_LIST_TIMEOUT, read).await {
        Ok(result) => result,
        Err(_) => Ok(vec![]),
    }
}
/// A workspace's skills and commands as a 2.x server lists them for a directory. A directory it has not served yet
/// is scanned in the background: its first read lists no commands at all, and `command.updated` plus `skill.updated`
/// for the directory end the scan (`loadOpenCode2Workspace`). The stream must be listening before the read starts it.
async fn load_v2_workspace(api: &Api, directory: &Path) -> (Vec<Value>, Vec<Value>) {
    let mut stream = api.subscribe("/api/event").await.ok();
    let query = location(directory);
    let read = || async {
        let (skills, commands) = tokio::join!(api.call(Method::GET, "/api/skill", &query, None, Some(MODEL_LIST_TIMEOUT)), api.call(Method::GET, "/api/command", &query, None, Some(MODEL_LIST_TIMEOUT)));
        (skills.ok().and_then(|list| list["data"].as_array().cloned()).unwrap_or_default(), commands.ok().and_then(|list| list["data"].as_array().cloned()).unwrap_or_default())
    };
    let (mut skills, mut commands) = read().await;
    if commands.is_empty() {
        if let Some(stream) = stream.as_mut() {
            let wanted = directory.to_string_lossy().into_owned();
            let scanned = async {
                let mut pending = vec!["command.updated", "skill.updated"];
                while let Ok(Some(event)) = stream.next().await {
                    if event["location"]["directory"] == wanted.as_str() {
                        pending.retain(|name| event["type"] != *name);
                        if pending.is_empty() {
                            return;
                        }
                    }
                }
            };
            let _ = tokio::time::timeout(SCAN_TIMEOUT, scanned).await;
        }
        (skills, commands) = read().await;
    }
    let mut mapped_skills: Vec<(String, Value)> = skills
        .iter()
        .map(|skill| {
            let id = str_of(&skill["id"]);
            let mut mapped = json!({"name":id,"path":str_of(&skill["path"]),"enabled":true});
            if skill["name"] != skill["id"] {
                mapped["displayName"] = skill["name"].clone();
            }
            if let Some(description) = trimmed(&skill["description"]) {
                mapped["description"] = json!(description);
                mapped["shortDescription"] = json!(description);
            }
            (id, mapped)
        })
        .collect();
    mapped_skills.sort_by(|left, right| crate::workspace_entries::collate(&left.0, &right.0));
    let mut slash = vec![compact_command()];
    let mut names = vec!["compact".to_owned()];
    for command in &commands {
        let Some(name) = trimmed(&command["name"]) else { continue };
        if names.contains(&name) {
            continue;
        }
        names.push(name.clone());
        let mut mapped = json!({"name":name});
        if let Some(description) = trimmed(&command["description"]) {
            mapped["description"] = json!(description);
        }
        slash.push(mapped);
    }
    (mapped_skills.into_iter().map(|(_, skill)| skill).collect(), slash)
}

/// `loadOpenCodeInventory`: the provider list, agents, skills and commands, read together. Agents, skills and
/// commands degrade to empty; the provider list is required.
async fn load_v1_inventory(api: &Api) -> Result<Value, crate::opencode_api::ApiError> {
    let (providers, agents, skills, commands) = tokio::join!(
        api.call(Method::GET, "/provider", &[], None, Some(INVENTORY_TIMEOUT)),
        api.call(Method::GET, "/agent", &[], None, Some(INVENTORY_TIMEOUT)),
        api.call(Method::GET, "/skill", &[], None, Some(INVENTORY_TIMEOUT)),
        api.call(Method::GET, "/command", &[], None, Some(SCAN_TIMEOUT)),
    );
    let providers = providers?;
    if providers.is_null() {
        return Err(crate::opencode_api::ApiError::Decode("OpenCode provider list was empty.".into()));
    }
    Ok(json!({"providerList":providers,"agents":agents.unwrap_or(json!([])),"skills":skills.unwrap_or(json!([])),"commands":commands.unwrap_or(json!([]))}))
}
fn flatten_v1_models(inventory: &Value) -> Vec<Value> {
    let connected: Vec<&str> = inventory["providerList"]["connected"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let agents = inventory["agents"].as_array().cloned().unwrap_or_default();
    let mut models: Vec<(String, Value)> = vec![];
    for provider in inventory["providerList"]["all"].as_array().into_iter().flatten() {
        let provider_id = str_of(&provider["id"]);
        if !connected.contains(&provider_id.as_str()) {
            continue;
        }
        let Some(listed) = provider["models"].as_object() else { continue };
        for model in listed.values() {
            let Some(name) = trimmed(&model["name"]) else { continue };
            let mut entry = json!({"slug":format!("{provider_id}/{}", str_of(&model["id"])),"name":name,"isCustom":false,"capabilities":v1_capabilities(&provider_id, model, &agents)});
            if let Some(sub) = trimmed(&provider["name"]) {
                entry["subProvider"] = json!(sub);
            }
            models.push((name, entry));
        }
    }
    models.sort_by(|left, right| crate::workspace_entries::collate(&left.0, &right.0));
    models.into_iter().map(|(_, model)| model).collect()
}
/// `openCodeCapabilitiesForModel`.
fn v1_capabilities(provider_id: &str, model: &Value, agents: &[Value]) -> Value {
    let raw: Vec<String> = model["variants"].as_object().map(|variants| variants.keys().cloned().collect()).unwrap_or_default();
    // A model that advertises no variants gets the standard levels, so the composer still offers a Reasoning selector.
    let variants = if raw.is_empty() { ["low", "medium", "high", "xhigh"].map(str::to_owned).to_vec() } else { raw };
    let default_variant = infer_default_variant(provider_id, &variants);
    let primary: Vec<&Value> = agents.iter().filter(|agent| agent["hidden"] != true && matches!(agent["mode"].as_str(), Some("primary" | "all"))).collect();
    let default_agent = primary.iter().find(|agent| agent["name"] == "build").or(primary.first()).map(|agent| str_of(&agent["name"]));
    let mut descriptors = vec![];
    if !variants.is_empty() {
        descriptors.push(option_descriptor("variant", "Reasoning", variants.into_iter().map(|id| (id, None)).collect(), default_variant.as_deref()));
    }
    if !primary.is_empty() {
        descriptors.push(option_descriptor("agent", "Agent", primary.iter().map(|agent| (str_of(&agent["name"]), None)).collect(), default_agent.as_deref()));
    }
    json!({"optionDescriptors":descriptors})
}
fn v1_skills(skills: &Value) -> Vec<Value> {
    let mut out: Vec<(String, Value)> = vec![];
    for skill in skills.as_array().into_iter().flatten() {
        let (Some(name), Some(path)) = (trimmed(&skill["name"]), trimmed(&skill["location"])) else { continue };
        let mut mapped = json!({"name":name,"path":path,"enabled":true});
        if let Some(description) = trimmed(&skill["description"]) {
            mapped["description"] = json!(description);
            mapped["shortDescription"] = json!(description);
        }
        out.push((name, mapped));
    }
    out.sort_by(|left, right| crate::workspace_entries::collate(&left.0, &right.0));
    out.into_iter().map(|(_, skill)| skill).collect()
}
fn v1_commands(commands: &Value) -> Vec<Value> {
    let mut slash = vec![compact_command()];
    let mut names = vec!["compact".to_owned()];
    for command in commands.as_array().into_iter().flatten() {
        let Some(name) = trimmed(&command["name"]) else { continue };
        if names.contains(&name) || command["source"] == "skill" {
            continue;
        }
        names.push(name.clone());
        let mut mapped = json!({"name":name});
        if let Some(description) = trimmed(&command["description"]) {
            mapped["description"] = json!(description);
        }
        let hint = command["hints"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(" ");
        if let Some(hint) = Some(hint.trim().to_owned()).filter(|hint| !hint.is_empty()) {
            mapped["input"] = json!({"hint":hint});
        }
        slash.push(mapped);
    }
    slash
}

// ---- usage limits (openCodeUsageLimits.ts) ----

const GO_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
/// `readOpenCodeGoUsageLimits`: an external server owns its credentials, so the host's account is never read for it.
async fn read_go_usage_limits(instance: &OpenCodeInstance) -> Value {
    let checked_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let unsupported = crate::claude_usage_limits::make_unavailable_usage_limits(&checked_at, "unsupported", None);
    if !instance.enabled || instance.server_url().is_some() {
        return unsupported;
    }
    let environment = instance.process_environment();
    let read = async {
        let data_home = environment.get("XDG_DATA_HOME").filter(|home| !home.is_empty()).map(PathBuf::from).or_else(|| environment.get("HOME").or(environment.get("USERPROFILE")).filter(|home| !home.is_empty()).map(|home| Path::new(home).join(".local").join("share")))?;
        let contents = match environment.get("OPENCODE_AUTH_CONTENT").filter(|content| !content.is_empty()) {
            Some(content) => content.clone(),
            None => match tokio::fs::read_to_string(data_home.join("opencode").join("auth.json")).await {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_owned(),
                Err(_) => return Some(Err(())),
            },
        };
        let auth: Value = serde_json::from_str(&contents).ok().filter(Value::is_object)?;
        // Stored API credentials overlay environment ones.
        let stored = auth["opencode-go"].as_object().filter(|entry| entry.get("type") == Some(&json!("api"))).and_then(|entry| entry.get("key")).and_then(Value::as_str);
        let key = stored.or(environment.get("OPENCODE_API_KEY").map(String::as_str)).map(str::trim).filter(|key| !key.is_empty())?.to_owned();
        let url = environment.get("T3CODE_OPENCODE_GO_USAGE_URL").cloned().unwrap_or_else(|| GO_USAGE_URL.to_owned());
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().ok()?;
        let response = client.get(url).bearer_auth(&key).send().await.ok()?;
        // A valid Zen key can exist without a Go subscription.
        if response.status() == 403 {
            return None;
        }
        let body: Value = response.error_for_status().ok()?.json().await.ok()?;
        Some(go_usage_limits(&body, &key, &checked_at).ok_or(()))
    };
    match tokio::time::timeout(Duration::from_secs(5), read).await {
        Err(_) => crate::claude_usage_limits::make_unavailable_usage_limits(&checked_at, "probeFailed", Some("OpenCode Go could not read usage.")),
        Ok(None) => unsupported,
        Ok(Some(Ok(limits))) => limits,
        Ok(Some(Err(()))) => crate::claude_usage_limits::make_unavailable_usage_limits(&checked_at, "probeFailed", Some("OpenCode Go could not read usage.")),
    }
}
/// The Go usage response as three windows, keyed by an unkeyed hash of the credential (the response has no account id).
pub fn go_usage_limits(body: &Value, key: &str, checked_at: &str) -> Option<Value> {
    use sha2::Digest;
    let window = |name: &str, id: &str, kind: &str, label: &str, minutes: Option<u64>| -> Option<Value> {
        let entry = &body["usage"][name];
        let percent = entry["percent"].as_f64()?;
        let resets = chrono::DateTime::parse_from_rfc3339(entry["resetsAt"].as_str()?).ok()?.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut window = json!({"id":id,"kind":kind,"label":label,"usedPercent":crate::claude_usage_limits::clamp_percent(percent),"resetsAt":resets});
        if let Some(minutes) = minutes {
            window["windowDurationMins"] = json!(minutes);
        }
        Some(window)
    };
    let windows = vec![window("rolling", "go_rolling", "session", "Go · Session", Some(5 * 60))?, window("weekly", "go_weekly", "weekly", "Go · Weekly", Some(7 * 24 * 60))?, window("monthly", "go_monthly", "monthly", "Go · Monthly", None)?];
    let digest = sha2::Sha256::digest(format!("opencode-go\0{key}").as_bytes());
    let mut limits = crate::claude_usage_limits::make_usage_limits(checked_at, windows);
    limits["credentialFingerprint"] = json!(digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    Some(limits)
}

/// The update target of an OpenCode install: 1.x ships as `opencode-ai` and 2.x as `@opencode/cli`, and an install
/// is only ever updated within its own package (2.x converts the shared database in place). An unknown version
/// offers no package update.
pub fn update_target(instance: &OpenCodeInstance) -> Option<crate::provider_manage::Target> {
    if !instance.enabled {
        return None;
    }
    let generation = instance.last_probe()?.generation;
    let env = instance.process_environment();
    Some(crate::provider_manage::Target {
        instance_id: instance.instance_id.clone(),
        driver: DRIVER,
        definition: crate::provider_maintenance::opencode_definition(generation == Generation::V2),
        binary: instance.binary_path().to_string_lossy().into_owned(),
        env,
    })
}

fn validate(snapshot: Value) -> Result<Value, ProcessError> {
    let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn variants_default_per_upstream_provider() {
        let levels = ["low", "medium", "high"].map(str::to_owned).to_vec();
        assert_eq!(infer_default_variant("openai", &levels).as_deref(), Some("medium"));
        assert_eq!(infer_default_variant("opencode", &["high".to_owned()]).as_deref(), Some("high"));
        assert_eq!(infer_default_variant("anthropic", &levels).as_deref(), Some("high"));
        assert_eq!(infer_default_variant("google-vertex", &["low".to_owned(), "medium".to_owned()]), None);
        assert_eq!(infer_default_variant("other", &levels), None);
        assert_eq!(title_case_slug("extra-high_x/y"), "Extra High X Y");
    }
    #[test]
    fn a_configured_address_is_named_without_its_credentials() {
        assert_eq!(redacted_url("http://user:secret@10.0.0.5:4096/base?token=abc#frag"), "http://10.0.0.5:4096/base");
        assert_eq!(redacted_url("not a url"), "the configured address");
    }
    #[test]
    fn go_usage_has_three_windows_and_a_fingerprint_that_is_not_the_key() {
        let body = json!({"usage":{"rolling":{"percent":12.5,"resetsAt":"2026-10-09T10:00:00Z"},"weekly":{"percent":130,"resetsAt":"2026-10-12T00:00:00Z"},"monthly":{"percent":3,"resetsAt":"2026-11-01T00:00:00Z"}}});
        let limits = go_usage_limits(&body, "key-123", "2026-10-09T00:00:00.000Z").unwrap();
        let ids: Vec<&str> = limits["windows"].as_array().unwrap().iter().map(|window| window["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["go_rolling", "go_weekly", "go_monthly"]);
        assert_eq!(limits["windows"][1]["usedPercent"], 100.0);
        assert!(!limits.to_string().contains("key-123"));
        assert_eq!(limits["credentialFingerprint"].as_str().unwrap().len(), 64);
    }
    #[test]
    fn custom_models_carry_the_default_reasoning_and_agent_options() {
        let models = provider_models(vec![], &[json!("local/model"), json!({"slug":"x/y","name":"Why"})]);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["capabilities"]["optionDescriptors"][0]["currentValue"], "medium");
        assert_eq!(models[1]["name"], "Why");
        assert_eq!(models[1]["isCustom"], true);
    }
}
