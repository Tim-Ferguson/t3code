//! Antigravity provider instance: executable resolution, version and health, authentication status, models,
//! slash commands and skills. Mirrors `AntigravityProvider.ts`, `AntigravityDriver.ts` and `AntigravitySkills.ts`.
//!
//! The health check never starts the agent (it unpacks about 1 GB per launch): it resolves the installation on
//! disk and reports it installed, with the sign-in unchecked. What only a session can know (the account, the
//! models and commands it offers) reaches the snapshot through `session_started` and the provider registry.
use crate::{
    antigravity_acp::{self as acp, AuthConfig},
    antigravity_install::{AntigravityInstallation, Executable, InstallError},
    provider_manage::{Management, merged_environment},
    provider_process::{ProcessError, ProcessOptions},
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub type AntigravityConfig = t3_contracts::AntigravitySettings;
const SIGN_IN_MESSAGE: &str = "Sign in with Google to use Antigravity.";
const AUTH_UNCHECKED_MESSAGE: &str = "Antigravity is installed. Google account access is not checked yet.";
const MAX_WORKSPACE_SNAPSHOTS: usize = 32;
/// The chat default the bundled model manifest names; the provider-default alias selects it when the account offers it.
pub const MANIFEST_DEFAULT_MODEL: &str = "gemini-3.8-flash-high";
/// The manifest's current models: any other model the account returns is shown folded under the legacy section.
const MANIFEST_CURRENT_MODELS: [&str; 3] = ["gemini-3.8-flash-high", "gemini-3.8-flash-medium", "gemini-3.8-flash-low"];

#[derive(Clone)]
pub struct AntigravityInstance {
    pub instance_id: String,
    pub display_name: String,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub config: AntigravityConfig,
    pub environment: HashMap<String, String>,
    pub management: Management,
}
impl std::fmt::Debug for AntigravityInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The configuration holds an API key.
        formatter.debug_struct("AntigravityInstance").field("instance_id", &self.instance_id).field("enabled", &self.enabled).finish_non_exhaustive()
    }
}
/// What a session holds until it ends: the lease on the release in use and the runtime directory it unpacks into.
#[derive(Default, Clone)]
pub struct Holds(Arc<Mutex<Vec<Box<dyn std::any::Any + Send>>>>);
struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
/// The files of an instance's profile: `GEMINI_HOME` (it holds the Google sign-in, which only the agent reads).
pub struct Profile {
    pub gemini_home: PathBuf,
    pub temp_parent: PathBuf,
    pub browser_command: String,
}
fn missing_installation(error: &InstallError) -> bool {
    let detail = error.detail.to_lowercase();
    error.operation == "resolve" && ["not installed", "missing", "incomplete", "does not publish"].iter().any(|needle| detail.contains(needle))
}
fn non_empty_text(value: &str) -> Option<String> {
    Some(t3_contracts::trim_wire_string(value)).filter(|text| !text.is_empty()).map(str::to_owned)
}
impl AntigravityInstance {
    pub fn auth(&self) -> AuthConfig {
        AuthConfig::from_settings(&self.config)
    }
    /// The environment every Antigravity process of this instance runs in: the server's with the instance's on top.
    pub fn process_environment(&self) -> HashMap<String, String> {
        merged_environment(&self.environment)
    }
    /// The home the agent expands `~` against.
    pub fn user_home(&self) -> PathBuf {
        let environment = self.process_environment();
        let get = |name: &str| environment.get(name).cloned().filter(|value| !value.is_empty());
        let home = if cfg!(windows) { get("USERPROFILE").or_else(|| Some(format!("{}{}", get("HOMEDRIVE")?, get("HOMEPATH")?))) } else { get("HOME") };
        home.map(PathBuf::from).unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default())
    }
    /// The profile directory and the parent of the per-process directories, under the server's state directory.
    pub fn directories(&self) -> Option<(PathBuf, PathBuf)> {
        let state = self.management.options().tools_dir?.parent()?.to_path_buf();
        Some(acp::instance_directories(&state, &self.instance_id))
    }
    pub async fn installation(&self) -> Result<AntigravityInstallation, InstallError> {
        self.management.antigravity_installation().await
    }
    /// The generic ACP instance a session starts through: a local agent whose command is the resolved executable.
    pub fn acp_instance(&self) -> crate::acp_runtime::AcpInstance {
        let config = serde_json::from_value(json!({"enabled":self.enabled,"source":"local","commandPath":self.config.binary_path.as_str(),"commandArgs":[],"authMethodId":self.auth().auth_method})).expect("local ACP settings");
        crate::acp_runtime::AcpInstance { instance_id: self.instance_id.clone(), display_name: self.display_name.clone(), accent_color: self.accent_color.clone(), enabled: self.enabled, config, environment: self.process_environment(), catalog: None, coordinator: Default::default() }
    }
    /// Where pasted uploads are saved: the one directory outside the workspace the agent may read.
    pub fn attachments_dir(&self) -> Option<PathBuf> {
        Some(self.management.options().tools_dir?.parent()?.join("attachments"))
    }
    pub fn continuation_key(&self) -> String {
        format!("antigravity:instance:{}", self.instance_id)
    }

    // ---- the profile and the process ----

    /// Prepares a private profile without reading or copying Google credentials. The agent opens the sign-in page
    /// through `BROWSER`; a command that only reports the URL is proven to work before anything is launched.
    pub async fn prepare_profile(&self) -> Result<Profile, ProcessError> {
        let failure = |detail: &str| ProcessError::Protocol(detail.to_owned());
        let (gemini_home, temp_parent) = self.directories().ok_or_else(|| failure("The Antigravity profile directory is not available in this build."))?;
        let browser_command = acp::browser_command();
        Self::prove_browser_command(&self.auth(), &gemini_home, &browser_command).await?;
        for directory in [&gemini_home, &gemini_home.join("antigravity-acp"), &temp_parent] {
            std::fs::create_dir_all(directory).map_err(|_| failure("The Antigravity profile directory could not be created."))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).map_err(|_| failure("The Antigravity profile directory permissions could not be set."))?;
            }
        }
        // Rewriting on every launch keeps a method, project or location edit in Settings effective.
        std::fs::write(gemini_home.join("antigravity-acp").join("settings.json"), self.auth().profile_settings()).map_err(|_| failure("The Antigravity profile settings could not be written."))?;
        link_user_skills(&gemini_home, &self.user_home());
        Ok(Profile { gemini_home, temp_parent, browser_command })
    }
    async fn prove_browser_command(auth: &AuthConfig, profile: &Path, browser_command: &str) -> Result<(), ProcessError> {
        const PREFLIGHT_URL: &str = "https://example.invalid/t3-antigravity-browser-preflight";
        let failure = || ProcessError::Protocol("Antigravity browser suppression could not be verified.".into());
        // Python substitutes the URL into the command's last argument.
        let words: Vec<String> = shlex_split(browser_command).ok_or_else(failure)?;
        let (program, args) = words.split_first().ok_or_else(failure)?;
        let args: Vec<String> = args.iter().map(|arg| arg.replace("%s", PREFLIGHT_URL)).collect();
        let _ = (auth, profile);
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), tokio::process::Command::new(program).args(&args).stdin(std::process::Stdio::null()).output()).await.map_err(|_| failure())?.map_err(|_| failure())?;
        let expected = format!("{}\"{PREFLIGHT_URL}\"\n", acp::AUTH_BROWSER_MARKER);
        if !output.status.success() || !output.stdout.is_empty() || String::from_utf8_lossy(&output.stderr) != expected {
            return Err(failure());
        }
        Ok(())
    }
    /// The process of one session (or sign-in): the agent with the instance's profile and credential, the harness
    /// beside it, and a runtime directory of its own that goes with `holds`.
    pub async fn spawn_options(&self, cwd: &Path, holds: &Holds, extra_environment: &HashMap<String, String>) -> Result<ProcessOptions, ProcessError> {
        let auth = self.auth();
        if let Some(issue) = auth.config_issue() {
            return Err(ProcessError::Protocol(issue.to_owned()));
        }
        let installation = self.installation().await.map_err(|error| ProcessError::Protocol(error.detail))?;
        let environment = self.process_environment();
        let lease = installation.acquire(self.config.binary_path.as_str(), &environment).await.map_err(|error| ProcessError::Protocol(error.detail))?;
        let profile = self.prepare_profile().await?;
        // Each process unpacks into its own directory that goes with the session, after the process is ended.
        let runtime_temp = profile.temp_parent.join(format!("run-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&runtime_temp).map_err(|_| ProcessError::Protocol("Could not create an Antigravity runtime temp directory.".into()))?;
        let mut base = environment;
        base.extend(extra_environment.clone());
        let options = process_options(&lease.executable, &profile, cwd, &base, &auth, &runtime_temp);
        let mut held = holds.0.lock().unwrap();
        held.push(Box::new(lease));
        held.push(Box::new(RemoveOnDrop(runtime_temp)));
        Ok(options)
    }

    // ---- the snapshot ----

    fn base_snapshot(&self) -> Value {
        let mut snapshot = json!({
            "instanceId":self.instance_id,"driver":"antigravity","displayName":self.display_name,"enabled":self.enabled,
            "installed":false,"version":null,"status":"warning","auth":{"status":"unknown","type":self.auth().auth_method},
            "checkedAt":Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),"availability":"available","models":[],"slashCommands":[],"skills":[],
            "setup":{"canAuthenticate":true,"canInstall":true},"supportsConversationRollback":false,"supportsTextGeneration":false,"showInteractionModeToggle":false,
            "workspaceSnapshots":[],"continuation":{"groupKey":self.continuation_key()}
        });
        if let Some(accent) = &self.accent_color {
            snapshot["accentColor"] = json!(accent);
        }
        snapshot
    }
    pub async fn discover(&self, cwd: &Path) -> Result<Value, ProcessError> {
        let mut snapshot = self.base_snapshot();
        let finish = |snapshot: Value| validate(snapshot);
        if !self.enabled {
            snapshot["status"] = json!("disabled");
            snapshot["message"] = json!("Antigravity is disabled in T3 Code settings.");
            return finish(snapshot);
        }
        if let Some(issue) = self.auth().config_issue() {
            snapshot["installed"] = json!(true);
            snapshot["status"] = json!("error");
            snapshot["message"] = json!(issue);
            return finish(snapshot);
        }
        let resolved = match self.installation().await {
            Ok(installation) => installation.resolve(self.config.binary_path.as_str(), &self.process_environment()),
            Err(error) => Err(error),
        };
        match resolved {
            Err(error) => {
                snapshot["installed"] = json!(!missing_installation(&error));
                snapshot["status"] = json!("error");
                snapshot["message"] = json!(non_empty_text(&error.detail).unwrap_or_else(|| "Antigravity could not complete its local health check.".into()));
                finish(snapshot)
            }
            Ok(executable) => {
                snapshot["installed"] = json!(true);
                snapshot["version"] = json!(executable.version.clone().unwrap_or_else(|| "unknown".into()));
                snapshot["message"] = json!(AUTH_UNCHECKED_MESSAGE);
                snapshot["skills"] = json!(discover_skills(cwd, &self.user_home()).unwrap_or_default());
                finish(snapshot)
            }
        }
    }
}
fn validate(snapshot: Value) -> Result<Value, ProcessError> {
    let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}
/// `buildAntigravityAcpSpawnInput`.
fn process_options(executable: &Executable, profile: &Profile, cwd: &Path, base: &HashMap<String, String>, auth: &AuthConfig, runtime_temp: &Path) -> ProcessOptions {
    let mut environment = acp::environment(base, &profile.gemini_home, &profile.browser_command, auth, runtime_temp);
    environment.insert("ANTIGRAVITY_HARNESS_PATH".into(), executable.harness_path.to_string_lossy().into_owned());
    let mut args: Vec<String> = if cfg!(target_os = "linux") { vec!["--uid=".into()] } else { vec![] };
    let executable_path = executable.executable_path.to_string_lossy().into_owned();
    // The agent starts with the instance's environment alone: whatever the server itself was started with that
    // selects another credential is unset before it runs (`env -u`).
    if cfg!(unix) {
        let unset: Vec<String> = base.keys().chain(std::env::vars().map(|(key, _)| key).collect::<Vec<_>>().iter()).filter(|key| acp::is_removed_key(key) && !environment.contains_key(*key)).cloned().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let mut wrapped: Vec<String> = unset.into_iter().flat_map(|key| ["-u".to_owned(), key]).collect();
        wrapped.push(executable_path);
        wrapped.append(&mut args);
        return ProcessOptions { binary: PathBuf::from("/usr/bin/env"), args: wrapped, cwd: cwd.into(), environment };
    }
    ProcessOptions { binary: executable.executable_path.clone(), args, cwd: cwd.into(), environment }
}
/// The agent reads its user-global skills under `GEMINI_HOME`, which T3 points at the private profile. The two skill
/// directories are linked back to the user's `~/.gemini` so global skills load, while MCP servers, hooks and
/// credentials stay isolated. Best effort: a link that cannot be made costs global skills, never the session.
fn link_user_skills(profile: &Path, user_home: &Path) {
    let links = acp::user_skill_directories(profile);
    let targets = acp::user_skill_directories(&user_home.join(".gemini"));
    for (link, target) in links.iter().zip(targets.iter()) {
        let result = (|| -> std::io::Result<()> {
            match std::fs::read_link(link) {
                Ok(existing) => {
                    if link.parent().map(|parent| parent.join(&existing)).as_ref() == Some(target) || &existing == target {
                        return Ok(());
                    }
                    std::fs::remove_file(link)?;
                }
                // A real directory at the link path is the user's own content and is left alone.
                Err(_) if link.exists() => return Ok(()),
                Err(_) => {}
            }
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, link)?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_dir(target, link)?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "Antigravity user skills are not linked into the profile.");
        }
    }
}
/// A shell-style split of the `BROWSER` command, as Python's `shlex.split` reads it: single quotes are literal.
fn shlex_split(command: &str) -> Option<Vec<String>> {
    let mut words = vec![];
    let (mut current, mut started, mut quote) = (String::new(), false, None);
    for character in command.chars() {
        match (quote, character) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(character);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    Some(words)
}
/// `identify`: what a downloaded agent says about itself at `initialize`, run in a throwaway profile.
pub async fn identify(executable: &Executable) -> Result<Value, String> {
    let scratch_directory = std::env::temp_dir().join(format!("t3-antigravity-validate-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&scratch_directory).map_err(|error| error.to_string())?;
    let scratch = RemoveOnDrop(scratch_directory);
    let profile = Profile { gemini_home: scratch.0.clone(), temp_parent: scratch.0.join("tmp"), browser_command: acp::browser_command() };
    std::fs::create_dir_all(&profile.temp_parent).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(profile.gemini_home.join("antigravity-acp")).map_err(|error| error.to_string())?;
    let auth = AuthConfig { auth_method: "oauth-personal".into(), api_key: String::new(), gcp_project: String::new(), gcp_location: String::new() };
    std::fs::write(profile.gemini_home.join("antigravity-acp").join("settings.json"), auth.profile_settings()).map_err(|error| error.to_string())?;
    let base: HashMap<String, String> = std::env::vars().collect();
    let options = process_options(executable, &profile, scratch.0.as_path(), &base, &auth, &profile.temp_parent);
    let environment = options.environment.clone();
    let peer = crate::acp_peer::ProcessPeer::spawn(options).map_err(|error| error.to_string())?;
    let instance = crate::acp_runtime::AcpInstance {
        instance_id: "antigravity-validate".into(),
        display_name: "Antigravity".into(),
        accent_color: None,
        enabled: true,
        config: serde_json::from_value(json!({"enabled":true,"source":"local","commandPath":executable.executable_path,"commandArgs":[]})).map_err(|error| error.to_string())?,
        environment,
        catalog: None,
        coordinator: Default::default(),
    };
    let services = instance.services_for(scratch.0.as_path(), crate::acp_client_callbacks::policy(&json!("approval-required"), scratch.0.as_path()));
    let initialized = Box::pin(instance.initialize_only(peer, true, services, None, Some(initialize_request("t3-code", false)))).await.map_err(|error| error.to_string())?;
    let value = serde_json::to_value(&initialized.initialize).map_err(|error| error.to_string());
    initialized.shutdown().await;
    value
}
/// The `initialize` request the agent gets: file access only when the session serves it.
pub fn initialize_request(client_name: &str, client_file_system: bool) -> t3_acp::types::InitializeRequest {
    use t3_acp::types::{ClientCapabilities, Implementation, InitializeRequest, Optional};
    InitializeRequest {
        client_info: Optional::Value(Implementation { name: client_name.into(), version: "0.0.0".into(), title: Optional::Missing, meta: Optional::Missing }),
        client_capabilities: Some(serde_json::from_value::<ClientCapabilities>(json!({"fs":{"readTextFile":client_file_system,"writeTextFile":client_file_system},"terminal":false})).expect("Antigravity client capabilities")),
        ..Default::default()
    }
}

// ---- what a session tells the snapshot ----

/// `buildAntigravityModelsFromSession`: the models the account offers, the session's current one the default.
pub fn models_from_session(setup: &Value) -> Vec<Value> {
    let config = setup["configOptions"].as_array().and_then(|options| options.iter().find(|option| option["id"] == "model" || option["category"] == "model"));
    let current = match config {
        Some(config) if config["type"] == "select" => config["currentValue"].as_str().map(str::to_owned),
        _ => setup["models"]["currentModelId"].as_str().map(str::to_owned),
    };
    let entries: Vec<(String, String)> = match config {
        Some(config) if config["type"] == "select" => config["options"].as_array().into_iter().flatten().flat_map(|entry| if entry.get("value").is_some() { vec![entry.clone()] } else { entry["options"].as_array().cloned().unwrap_or_default() }).filter_map(|entry| Some((entry["value"].as_str()?.to_owned(), entry["name"].as_str().unwrap_or("").to_owned()))).collect(),
        None => setup["models"]["availableModels"].as_array().into_iter().flatten().filter_map(|model| Some((model["modelId"].as_str()?.to_owned(), model["name"].as_str().unwrap_or("").to_owned()))).collect(),
        _ => vec![],
    };
    let mut seen = std::collections::HashSet::new();
    entries
        .into_iter()
        .filter(|(slug, _)| !t3_contracts::trim_wire_string(slug).is_empty() && seen.insert(slug.clone()))
        .map(|(slug, name)| {
            let mut model = json!({"slug":slug,"name":if t3_contracts::trim_wire_string(&name).is_empty() { slug.clone() } else { name },"isCustom":false,"capabilities":{"optionDescriptors":[]}});
            if Some(&slug) == current.as_ref() {
                model["isDefault"] = json!(true);
                model["aliases"] = json!([acp::DEFAULT_MODEL]);
            }
            model
        })
        .collect()
}
/// `applyModelManifest`: a model the manifest does not name as current is shown folded under the legacy section,
/// and the manifest's chat default takes the default flag (and the provider-default alias) when the account has it.
pub fn classify_models(mut models: Vec<Value>) -> Vec<Value> {
    for model in &mut models {
        let legacy = !MANIFEST_CURRENT_MODELS.contains(&model["slug"].as_str().unwrap_or(""));
        match (legacy, model["isLegacy"] == true) {
            (true, false) => model["isLegacy"] = json!(true),
            (false, true) => drop(model.as_object_mut().map(|object| object.remove("isLegacy"))),
            _ => {}
        }
    }
    let preferred = models.iter().position(|model| model["slug"] == MANIFEST_DEFAULT_MODEL);
    let previous = models.iter().position(|model| model["isDefault"] == true);
    if let (Some(preferred), Some(previous)) = (preferred, previous.filter(|previous| Some(*previous) != preferred.into())) {
        let aliases = models[previous]["aliases"].as_array().cloned().unwrap_or_default();
        let object = models[previous].as_object_mut().unwrap();
        object.remove("isDefault");
        object.remove("aliases");
        models[preferred]["isDefault"] = json!(true);
        let mut merged = models[preferred]["aliases"].as_array().cloned().unwrap_or_default();
        for alias in aliases {
            if !merged.contains(&alias) {
                merged.push(alias);
            }
        }
        if !merged.is_empty() {
            models[preferred]["aliases"] = json!(merged);
        }
    }
    models
}
/// `nativeCommands`.
pub fn native_commands(commands: &Value) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    commands.as_array().into_iter().flatten().filter_map(|command| {
        let name = command["name"].as_str().filter(|name| !t3_contracts::trim_wire_string(name).is_empty())?;
        if !seen.insert(name.to_owned()) {
            return None;
        }
        let mut native = json!({"name":name});
        if let Some(description) = non_empty_text(command["description"].as_str().unwrap_or("")) {
            native["description"] = json!(description);
        }
        if let Some(hint) = command["input"]["hint"].as_str().and_then(non_empty_text) {
            native["input"] = json!({"hint":hint});
        }
        Some(native)
    }).collect()
}
/// `onSessionStarted` of `AntigravityProvider.ts`: a session proves the account works, and tells which models and
/// commands it offers. `snapshot` is the instance's stored snapshot.
pub fn session_started(snapshot: &mut Value, started: &Value, cwd: Option<&str>, label: &str, auth_type: &str, now: &str) {
    snapshot.as_object_mut().map(|object| object.remove("message"));
    snapshot["installed"] = json!(true);
    snapshot["status"] = json!(if snapshot["enabled"] == true { "ready" } else { "disabled" });
    if let Some(version) = started["version"].as_str().and_then(non_empty_text) {
        snapshot["version"] = json!(version);
    }
    snapshot["auth"] = json!({"status":"authenticated","type":auth_type,"label":label});
    snapshot["checkedAt"] = json!(now);
    snapshot["models"] = json!(classify_models(models_from_session(&started["setup"])));
    if let Some(cwd) = cwd {
        let mut all: Vec<Value> = snapshot["workspaceSnapshots"].as_array().cloned().unwrap_or_default();
        let existing = all.iter().find(|entry| entry["cwd"] == cwd).cloned();
        all.retain(|entry| entry["cwd"] != cwd);
        all.push(json!({"cwd":cwd,"checkedAt":now,
            "slashCommands":existing.as_ref().map_or_else(|| snapshot["slashCommands"].clone(), |entry| entry["slashCommands"].clone()),
            "skills":existing.as_ref().map_or_else(|| json!([]), |entry| entry["skills"].clone())}));
        let keep = all.len().saturating_sub(MAX_WORKSPACE_SNAPSHOTS);
        snapshot["workspaceSnapshots"] = json!(all.split_off(keep));
    }
}
/// `onConfigOptionsUpdated`: the models change with the configuration, once the account is known to work.
pub fn models_updated(snapshot: &mut Value, config_options: &Value) {
    if snapshot["auth"]["status"] != "authenticated" {
        return;
    }
    snapshot["models"] = json!(classify_models(models_from_session(&json!({"configOptions":config_options}))));
}
/// `onAvailableCommands`.
pub fn commands_updated(snapshot: &mut Value, commands: &Value, cwd: Option<&str>, now: &str) {
    if snapshot["auth"]["status"] == "unauthenticated" {
        return;
    }
    let native = native_commands(commands);
    snapshot["slashCommands"] = json!(native);
    if let Some(cwd) = cwd {
        let mut all: Vec<Value> = snapshot["workspaceSnapshots"].as_array().cloned().unwrap_or_default();
        let skills = all.iter().find(|entry| entry["cwd"] == cwd).map_or_else(|| json!([]), |entry| entry["skills"].clone());
        all.retain(|entry| entry["cwd"] != cwd);
        all.push(json!({"cwd":cwd,"checkedAt":now,"slashCommands":native,"skills":skills}));
        let keep = all.len().saturating_sub(MAX_WORKSPACE_SNAPSHOTS);
        snapshot["workspaceSnapshots"] = json!(all.split_off(keep));
    }
}
/// `clearAccountMetadata`: a sign-out, or a session that found the account signed out.
pub fn account_cleared(snapshot: &mut Value, now: &str) {
    snapshot["auth"] = json!({"status":"unauthenticated"});
    snapshot["status"] = json!(if snapshot["enabled"] == true { "warning" } else { "disabled" });
    snapshot["message"] = json!(SIGN_IN_MESSAGE);
    snapshot["checkedAt"] = json!(now);
    snapshot["models"] = json!([]);
    snapshot["slashCommands"] = json!([]);
    snapshot["skills"] = json!([]);
    snapshot["workspaceSnapshots"] = json!([]);
    snapshot["supportsTextGeneration"] = json!(false);
}
/// `mergeProviderSnapshot` for Antigravity, and the state the source keeps between health checks: the health check
/// only initializes the agent, so a re-probe reports the account as unchecked. The saved login still works, and the
/// previous snapshot proves it: that account state (with the models, commands and workspaces a session learned) is
/// carried until a session, refresh or sign-out reports something new. A confirmed missing installation, a
/// sign-out, a disabled instance or a changed sign-in method is never overridden.
pub fn carry_saved_account(previous: &Value, next: &mut Value) {
    if next["driver"] != "antigravity" || previous["driver"] != "antigravity" {
        return;
    }
    // `shouldRetainMissingProviderModels`, judged on the probe as it came.
    let retains = next["enabled"] == true
        && next["auth"]["status"] != "unauthenticated"
        && ((next["status"] == "warning" && next["auth"]["status"] == "unknown") || (next["installed"] != true && next["status"] == "warning") || (next["installed"] == true && next["status"] == "error"));
    let carries = next["enabled"] == true
        && next["auth"]["status"] == "unknown"
        && previous["auth"]["status"] == "authenticated"
        && (next["auth"].get("type").is_none_or(Value::is_null) || next["auth"]["type"] == previous["auth"]["type"])
        && (next["installed"] == true || next["status"] == "warning");
    if carries {
        // The pending boot probe (`installed: false`, warning) and a failed probe keep their own status; only a passed
        // health check reads as ready.
        let ready = next["installed"] == true && next["status"] == "warning";
        next["auth"] = previous["auth"].clone();
        if ready {
            next["status"] = json!("ready");
            next.as_object_mut().map(|object| object.remove("message"));
        }
        for key in ["slashCommands", "workspaceSnapshots"] {
            if next[key].as_array().is_none_or(Vec::is_empty) && next["installed"] == true {
                if let Some(kept) = previous.get(key).filter(|kept| kept.as_array().is_some_and(|all| !all.is_empty())) {
                    next[key] = kept.clone();
                }
            }
        }
    }
    // `mergeProviderModels`: a model without option descriptors keeps the ones the previous snapshot had for it, and
    // while the probe cannot discover models the previous ones stay.
    let has_capabilities = |model: &Value| model["capabilities"]["optionDescriptors"].as_array().is_some_and(|all| !all.is_empty());
    let previous_models: Vec<Value> = previous["models"].as_array().cloned().unwrap_or_default();
    let retainable: Vec<Value> = previous_models.iter().filter(|model| model["isCustom"] != true).cloned().collect();
    let next_models: Vec<Value> = next["models"].as_array().cloned().unwrap_or_default();
    if retains && next_models.is_empty() && !retainable.is_empty() {
        next["models"] = json!(retainable);
        return;
    }
    let merged: Vec<Value> = next_models
        .iter()
        .map(|model| match previous_models.iter().find(|previous| previous["slug"] == model["slug"]) {
            Some(previous) if !has_capabilities(model) && has_capabilities(previous) => {
                let mut model = model.clone();
                model["capabilities"] = previous["capabilities"].clone();
                model
            }
            _ => model.clone(),
        })
        .collect();
    next["models"] = json!(if retains { merged.into_iter().chain(retainable.into_iter().filter(|previous| !next_models.iter().any(|model| model["slug"] == previous["slug"]))).collect::<Vec<_>>() } else { merged });
}

/// What the source's provider keeps between health checks beyond its registry merge: a sign-out it knows of stays
/// known, because a health check does not check the account again.
pub fn keep_signed_out(previous: &Value, next: &mut Value) {
    if next["driver"] == "antigravity"
        && previous["driver"] == "antigravity"
        && next["enabled"] == true
        && next["installed"] == true
        && next["auth"]["status"] == "unknown"
        && previous["auth"]["status"] == "unauthenticated"
        && next["status"] == "warning"
    {
        next["auth"] = previous["auth"].clone();
        next["message"] = json!(SIGN_IN_MESSAGE);
    }
}

// ---- skills (AntigravitySkills.ts) ----

const MAX_SKILL_BYTES: u64 = 1_000_000;
const MAX_SCAN_BYTES: u64 = 8_000_000;
const MAX_SCAN_ENTRIES: usize = 10_000;
struct Budget {
    bytes: u64,
    entries: usize,
}
/// The part of a skill's YAML frontmatter the picker reads: `name` and `description` as plain, quoted or block
/// scalars. A value of any other type, or a document that is not a mapping, makes the skill unusable.
fn frontmatter(contents: &str, file_name: &str) -> Option<(String, Option<String>)> {
    let start = contents.find("---")?;
    let end = contents[start + 3..].find("---")? + start + 3;
    let block = contents[start + 3..end].trim();
    let (mut name, mut description): (Option<Option<String>>, Option<Option<String>>) = (None, None);
    let lines: Vec<&str> = block.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        let parsed: Option<Option<String>> = if value.starts_with(['|', '>']) {
            let folded = value.starts_with('>');
            let mut parts = vec![];
            while index < lines.len() && (lines[index].starts_with([' ', '\t']) || lines[index].trim().is_empty()) {
                parts.push(lines[index].trim().to_owned());
                index += 1;
            }
            let text = parts.join(if folded { " " } else { "\n" }).trim().to_owned();
            Some(Some(text))
        } else if value.is_empty() || value == "~" || value == "null" {
            Some(None)
        } else if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2) || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2) {
            Some(Some(value[1..value.len() - 1].to_owned()))
        } else if value.starts_with(['[', '{', '&', '*', '!', '"', '\'']) || value.parse::<f64>().is_ok() || matches!(value, "true" | "false") {
            // Not a string: the original's schema refuses the document.
            if matches!(key.trim(), "name" | "description") { return None } else { continue }
        } else {
            Some(Some(value.to_owned()))
        };
        match key.trim() {
            "name" => name = parsed,
            "description" => description = parsed,
            _ => {}
        }
    }
    let name = name.flatten().filter(|name| !name.is_empty()).unwrap_or_else(|| file_name.get(..file_name.len().saturating_sub(3)).unwrap_or("").to_owned());
    // Native names are not trimmed. Do not rename one to fit the picker contract.
    if name.is_empty() || name != t3_contracts::trim_wire_string(&name) {
        return None;
    }
    let description = description.flatten().map(|text| t3_contracts::trim_wire_string(&text).to_owned()).filter(|text| !text.is_empty());
    Some((name, description))
}
/// The native loader orders child paths by Go's `URL.EscapedPath` encoding.
fn path_sort_key(entry: &str) -> String {
    let mut key = String::new();
    for byte in entry.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:@/".contains(&byte) {
            if matches!(byte, b'!' | b'\'' | b'(' | b')' | b'*') { key.push_str(&format!("%{byte:02X}")) } else { key.push(byte as char) }
        } else {
            key.push_str(&format!("%{byte:02X}"));
        }
    }
    key
}
/// `discoverAntigravitySkills`: the agent's explicit skill roots; the first valid same-name skill wins. Each root
/// loads its own `SKILL.md` or those in its immediate subdirectories. `None` when a read failed or the scan
/// budget ran out, so a workspace snapshot never caches a partial result.
pub fn discover_skills(cwd: &Path, user_home: &Path) -> Option<Vec<Value>> {
    let [config_skills, cli_skills] = acp::user_skill_directories(&user_home.join(".gemini"));
    let roots = [
        (config_skills, "user"),
        (cwd.join(".gemini").join("skills"), "project"),
        (cli_skills, "user"),
        (cwd.join(".agents").join("skills"), "project"),
        (cwd.join(".agent").join("skills"), "project"),
    ];
    let mut budget = Budget { bytes: MAX_SCAN_BYTES, entries: MAX_SCAN_ENTRIES };
    let mut skills: Vec<(String, Value)> = vec![];
    fn scan(directory: &Path, scope: &str, children: bool, budget: &mut Budget, skills: &mut Vec<(String, Value)>) -> Option<()> {
        let info = match std::fs::metadata(directory) {
            Ok(info) => info,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(()),
            Err(_) => return None,
        };
        if !info.is_dir() {
            return Some(());
        }
        let mut entries: Vec<String> = match std::fs::read_dir(directory) {
            Ok(entries) => entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(()),
            Err(_) => return None,
        };
        if entries.len() > budget.entries {
            return None;
        }
        budget.entries -= entries.len();
        entries.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        if let Some(skill_file) = entries.iter().find(|entry| entry.to_lowercase() == "skill.md") {
            if !skill_file.ends_with(".md") {
                return Some(());
            }
            let path = directory.join(skill_file);
            let info = match std::fs::metadata(&path) {
                Ok(info) => info,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(()),
                Err(_) => return None,
            };
            if !info.is_file() {
                return Some(());
            }
            let limit = MAX_SKILL_BYTES.min(budget.bytes);
            if info.len() > limit {
                return None;
            }
            let bytes = std::fs::read(&path).ok()?;
            if bytes.len() as u64 > limit {
                return None;
            }
            budget.bytes -= bytes.len() as u64;
            let Some((name, description)) = frontmatter(&String::from_utf8_lossy(&bytes), skill_file) else { return Some(()) };
            if skills.iter().any(|(existing, _)| *existing == name) {
                return Some(());
            }
            let mut skill = json!({"name":name,"path":path,"scope":scope,"enabled":true});
            if let Some(description) = description {
                skill["description"] = json!(description);
            }
            skills.push((name, skill));
            return Some(());
        }
        if children {
            let mut sorted: Vec<(String, &String)> = entries.iter().map(|entry| (path_sort_key(entry), entry)).collect();
            sorted.sort_by(|left, right| left.0.encode_utf16().cmp(right.0.encode_utf16()));
            for (_, entry) in sorted {
                scan(&directory.join(entry), scope, false, budget, skills)?;
            }
        }
        Some(())
    }
    for (directory, scope) in roots {
        scan(&directory, scope, true, &mut budget, &mut skills)?;
    }
    let mut skills: Vec<Value> = skills.into_iter().map(|(_, skill)| skill).collect();
    // `localeCompare` in the server's locale.
    let preferences: icu_collator::CollatorPreferences = crate::workspace_entries::locale_from_environment().into();
    let collator = icu_collator::Collator::try_new(preferences, Default::default()).expect("compiled ICU collation data");
    skills.sort_by(|left, right| collator.compare(left["name"].as_str().unwrap_or(""), right["name"].as_str().unwrap_or("")));
    Some(skills)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/antigravity-pure.json")).unwrap()
    }
    #[test]
    fn session_models_are_listed_and_classified_as_the_source_does() {
        let fixture = fixture();
        for case in fixture["sessionModels"].as_array().unwrap() {
            let models = models_from_session(&case["setup"]);
            assert_eq!(json!(models), case["models"], "{}", case["setup"]);
            assert_eq!(json!(classify_models(models)), case["classified"], "{}", case["setup"]);
        }
        assert_eq!(fixture["manifestDefault"], MANIFEST_DEFAULT_MODEL);
    }
    #[test]
    fn skills_are_discovered_from_the_agents_roots_as_the_source_does() {
        let fixture = fixture();
        let home = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(home.path()).unwrap();
        for entry in fixture["skills"]["files"].as_array().unwrap() {
            let path = home.join(entry[0].as_str().unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, entry[1].as_str().unwrap()).unwrap();
        }
        let skills = discover_skills(&home.join("project"), &home).unwrap();
        let skills: Vec<Value> = skills.into_iter().map(|mut skill| {
            skill["path"] = json!(skill["path"].as_str().unwrap().replace(home.to_str().unwrap(), "<home>"));
            skill
        }).collect();
        assert_eq!(json!(skills), fixture["skills"]["expected"]);
    }
    #[test]
    fn a_saved_account_is_carried_over_a_health_check_as_the_source_merges_it() {
        for case in fixture()["merges"].as_array().unwrap() {
            let (previous, mut next) = (case["previous"].clone(), case["next"].clone());
            carry_saved_account(&previous, &mut next);
            let result = &case["result"];
            for key in ["auth", "status", "message", "models"] {
                assert_eq!(next.get(key).cloned().unwrap_or(Value::Null), result.get(key).cloned().unwrap_or(Value::Null), "{key} of {case}");
            }
        }
    }
    #[test]
    fn a_session_teaches_the_snapshot_its_account_and_a_sign_out_forgets_it() {
        let mut snapshot = json!({"driver":"antigravity","enabled":true,"installed":true,"status":"warning","auth":{"status":"unknown","type":"oauth-personal"},"message":"unchecked","models":[],"slashCommands":[],"skills":[],"workspaceSnapshots":[]});
        let started = json!({"version":"agy_acp_server_1.3.0","setup":{"configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"gemini-3.8-flash-low","options":[{"value":"gemini-3.8-flash-high","name":"High"},{"value":"gemini-3.8-flash-low","name":"Low"}]}]}});
        session_started(&mut snapshot, &started, Some("/w"), "Google account", "oauth-personal", "2026-10-09T00:00:00.000Z");
        assert_eq!((snapshot["status"].clone(), snapshot["auth"]["status"].clone(), snapshot.get("message").cloned()), (json!("ready"), json!("authenticated"), None));
        assert_eq!(snapshot["version"], "agy_acp_server_1.3.0");
        assert_eq!(snapshot["models"].as_array().unwrap().len(), 2);
        assert_eq!(snapshot["workspaceSnapshots"][0]["cwd"], "/w");
        commands_updated(&mut snapshot, &json!([{"name":"plan","description":" Plan ","input":{"hint":"task"}},{"name":"plan"},{"name":" "}]), Some("/w"), "2026-10-09T00:00:01.000Z");
        assert_eq!(snapshot["slashCommands"], json!([{"name":"plan","description":"Plan","input":{"hint":"task"}}]));
        assert_eq!(snapshot["workspaceSnapshots"][0]["slashCommands"], snapshot["slashCommands"]);
        account_cleared(&mut snapshot, "2026-10-09T00:00:02.000Z");
        assert_eq!((snapshot["status"].clone(), snapshot["auth"].clone(), snapshot["models"].clone()), (json!("warning"), json!({"status":"unauthenticated"}), json!([])));
        // Models do not change for an account that is not known to work.
        models_updated(&mut snapshot, &started["setup"]["configOptions"]);
        assert_eq!(snapshot["models"], json!([]));
    }
}
