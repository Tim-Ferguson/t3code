//! Provider refresh and one-click updates: the `server.refreshProviders` handler of ws.ts and the
//! ProviderMaintenanceRunner. The registry owns the snapshots; this owns update state, the cached
//! ownership/latest-version lookups and the one place an update command is run.
use crate::{
    provider_maintenance::{self as maintenance, Capabilities, Definition, Platform},
    provider_process::ProcessError,
    provider_registry::ProviderRegistry,
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use t3_contracts::{ServerProviderUpdateErrorTag, ServerProviderUpdateInput, ServerRefreshProvidersInput};

const OUTPUT_MAX_BYTES: usize = 10_000;
const LATEST_VERSION_TTL_MS: i64 = 60 * 60 * 1000;
const CAPABILITIES_TTL: Duration = Duration::from_secs(60 * 60);
const LATEST_VERSION_TIMEOUT: Duration = Duration::from_secs(4);

/// Injection points. The server binary uses the defaults; tests point the registry URL at a loopback
/// server and the update tools at a temporary directory.
#[derive(Clone)]
pub struct Options {
    /// Where a package's latest version is looked up (`<url>/<package>/latest`).
    pub registry_url: String,
    /// Searched instead of the process `PATH` for update tools such as `npm`, and given to them.
    pub path: Option<String>,
    pub update_timeout: Duration,
    /// Managed provider tools live under here (`tools/codex`).
    pub tools_dir: Option<PathBuf>,
    pub http: reqwest::Client,
    /// Code-level substitute for the official Codex release table (`Some(None)`: none for this platform).
    /// The server binary never sets it; it lifts the official-host rule for whatever it names.
    pub codex_release: Option<Option<crate::provider_install::ReleaseAsset>>,
    /// Searched instead of the process `PATH` for a `codex` already installed.
    pub codex_path: Option<String>,
    /// Code-level substitute for the pinned Antigravity release (`Some(None)`: none for this platform). The server
    /// binary never sets it; it lifts the official-host rule for whatever it names.
    pub antigravity_release: Option<Option<crate::antigravity_install::ReleaseAsset>>,
    /// How long one ACP session or provider management request may take.
    pub acp_timeout: Duration,
}
impl Options {
    fn new(tools_dir: Option<PathBuf>) -> Self {
        Self { registry_url: "https://registry.npmjs.org".into(), path: None, update_timeout: Duration::from_secs(5 * 60), tools_dir, http: reqwest::Client::new(), codex_release: None, codex_path: None, antigravity_release: None, acp_timeout: Duration::from_secs(60) }
    }
}

/// What a provider instance's executable is, as far as update ownership is concerned.
#[derive(Clone)]
pub struct Target {
    pub instance_id: String,
    pub driver: &'static str,
    pub definition: Definition,
    pub binary: String,
    pub env: HashMap<String, String>,
}

#[derive(Default)]
struct Inner {
    options: RwLock<Option<Options>>,
    update_states: Mutex<HashMap<String, Value>>,
    running: Mutex<HashSet<String>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    capabilities: Mutex<HashMap<String, (std::time::Instant, String, Capabilities)>>,
    latest: Mutex<HashMap<String, (i64, Option<String>)>>,
    pub(crate) install: crate::provider_install::Slot,
    pub(crate) antigravity: tokio::sync::OnceCell<crate::antigravity_install::AntigravityInstallation>,
}
#[derive(Clone, Default)]
pub struct Management(Arc<Inner>);

pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
fn update_state(status: &str, started_at: Option<&str>, finished_at: Option<&str>, message: Option<&str>, output: Option<String>) -> Value {
    json!({"status":status,"startedAt":started_at,"finishedAt":finished_at,"message":message,"output":output})
}
/// `ServerProviderUpdateError` as it crosses the wire.
pub fn update_error(provider: &str, reason: &str) -> Value {
    serde_json::to_value(t3_contracts::ServerProviderUpdateError {
        tag: ServerProviderUpdateErrorTag::ServerProviderUpdateError,
        provider: provider.parse().unwrap_or_else(|_| "unknown".parse().unwrap()),
        reason: reason.parse().unwrap_or_else(|_| "Update failed.".parse().unwrap()),
    })
    .expect("typed update error")
}
fn expand_home(value: &str, env: &HashMap<String, String>) -> String {
    let value = value.trim();
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        if let Some(home) = env.get("HOME").cloned().or_else(|| std::env::var("HOME").ok()).or_else(|| std::env::var("USERPROFILE").ok()) {
            return PathBuf::from(home).join(value.get(2..).unwrap_or("")).to_string_lossy().into_owned();
        }
    }
    value.to_owned()
}
/// `mergeProviderInstanceEnvironment`: the server's environment with the instance's variables on top.
pub fn merged_environment(instance: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for (name, value) in instance {
        // Child processes do not apply shell expansion, so these two paths are expanded here.
        let value = if name == "CODEX_HOME" || name == "CLAUDE_CONFIG_DIR" { expand_home(value, &env) } else { value.clone() };
        env.insert(name.clone(), value);
    }
    env
}
/// Codex's shared home: the configured one, else `~/.codex`.
fn codex_shared_home(home_path: &str, env: &HashMap<String, String>) -> String {
    let home = if home_path.trim().is_empty() { expand_home("~/.codex", env) } else { expand_home(home_path, env) };
    std::path::absolute(&home).map(|path| path.to_string_lossy().into_owned()).unwrap_or(home)
}
pub fn codex_target(instance: &crate::codex::CodexInstance) -> Target {
    let env = merged_environment(&instance.environment);
    let shared_home = codex_shared_home(instance.config.home_path.as_str(), &env);
    Target { instance_id: instance.instance_id.clone(), driver: "codex", definition: maintenance::codex_definition(&shared_home), binary: expand_home(instance.config.binary_path.as_str(), &env), env }
}
/// Grok installs under `GROK_HOME`, so its updater runs with the instance's environment.
pub fn grok_target(instance: &crate::grok_instance::GrokInstance) -> Target {
    let env = merged_environment(&instance.environment);
    let own = instance.environment.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
    Target { instance_id: instance.instance_id.clone(), driver: "grok", definition: maintenance::grok_definition(own), binary: expand_home(&instance.binary_path().to_string_lossy(), &env), env }
}
/// Antigravity updates by installing a new pinned release, not by an updater of its own, so nothing here can update it.
pub fn antigravity_target(instance: &crate::antigravity_instance::AntigravityInstance) -> Target {
    Target { instance_id: instance.instance_id.clone(), driver: "antigravity", definition: maintenance::manual_definition("antigravity"), binary: String::new(), env: HashMap::new() }
}
pub fn claude_target(instance: &crate::claude_instance::ClaudeInstance) -> Target {
    let env = merged_environment(&instance.environment);
    Target { instance_id: instance.instance_id.clone(), driver: "claudeAgent", definition: maintenance::claude_definition(), binary: expand_home(instance.config.binary_path.as_str(), &env), env }
}

impl Management {
    pub fn new(tools_dir: Option<PathBuf>) -> Self {
        let management = Self::default();
        *management.0.options.write().unwrap() = Some(Options::new(tools_dir));
        management
    }
    pub fn options(&self) -> Options {
        self.0.options.write().unwrap().get_or_insert_with(|| Options::new(None)).clone()
    }
    /// Adjust the injection points. For the server's own startup and for tests.
    pub fn configure(&self, change: impl FnOnce(&mut Options)) {
        let mut slot = self.0.options.write().unwrap();
        change(slot.get_or_insert_with(|| Options::new(None)));
    }
    pub(crate) fn install_slot(&self) -> &crate::provider_install::Slot {
        &self.0.install
    }
    pub(crate) fn antigravity_slot(&self) -> &tokio::sync::OnceCell<crate::antigravity_install::AntigravityInstallation> {
        &self.0.antigravity
    }

    // ---- update state (volatile; never persisted) ----

    pub fn apply_update_state(&self, snapshot: &mut Value) {
        let id = snapshot["instanceId"].as_str().unwrap_or("").to_owned();
        match self.0.update_states.lock().unwrap().get(&id) {
            Some(state) => snapshot["updateState"] = state.clone(),
            None => {
                snapshot.as_object_mut().map(|object| object.remove("updateState"));
            }
        }
    }
    pub fn set_update_state(&self, instance: &str, state: Option<Value>) {
        let mut states = self.0.update_states.lock().unwrap();
        match state.filter(|state| state["status"] != "idle") {
            Some(state) => states.insert(instance.to_owned(), state),
            None => states.remove(instance),
        };
    }
    /// An instance that no longer exists keeps no update state.
    pub fn retain_update_states(&self, known: &HashSet<String>) {
        self.0.update_states.lock().unwrap().retain(|id, _| known.contains(id));
    }
    pub fn invalidate(&self, instance: &str, package: Option<&str>) {
        self.0.capabilities.lock().unwrap().remove(instance);
        if let Some(package) = package {
            self.0.latest.lock().unwrap().remove(package);
        }
    }

    // ---- capabilities and versions ----

    /// Ownership is re-derived from the executable at most this often, or whenever `fresh`.
    pub async fn capabilities(&self, target: &Target, fresh: bool) -> Capabilities {
        let key = format!("{}\0{}", target.binary, target.definition.native.as_ref().and_then(|native| native.env.as_ref()).map(|env| format!("{env:?}")).unwrap_or_default());
        if !fresh {
            if let Some((at, cached_key, caps)) = self.0.capabilities.lock().unwrap().get(&target.instance_id) {
                if *cached_key == key && at.elapsed() < CAPABILITIES_TTL {
                    return caps.clone();
                }
            }
        }
        if target.driver == "antigravity" {
            return Capabilities::manual("antigravity", None);
        }
        let caps = maintenance::resolve_capabilities(&target.definition, Some(&target.binary), &target.env, Platform::host()).await;
        self.0.capabilities.lock().unwrap().insert(target.instance_id.clone(), (std::time::Instant::now(), key, caps.clone()));
        caps
    }
    async fn fetch_latest(&self, package: &str) -> Option<String> {
        let options = self.options();
        let response = options
            .http
            .get(maintenance::registry_latest_url(&options.registry_url, package))
            .header("accept", "application/json")
            .timeout(LATEST_VERSION_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let payload: Value = response.json().await.ok()?;
        payload["version"].as_str().map(str::trim).filter(|version| !version.is_empty()).map(str::to_owned)
    }
    /// `resolveLatestProviderVersion`: the installer's own answer, else the npm registry's (cached for an hour).
    pub async fn latest_version(&self, caps: &Capabilities) -> Option<String> {
        if let Some(latest) = &caps.latest_version {
            return latest.clone();
        }
        let package = caps.package_name.as_deref()?;
        let now = Utc::now().timestamp_millis();
        if let Some((expires, version)) = self.0.latest.lock().unwrap().get(package) {
            if *expires > now {
                return version.clone();
            }
        }
        let version = self.fetch_latest(package).await;
        self.0.latest.lock().unwrap().insert(package.to_owned(), (now + LATEST_VERSION_TTL_MS, version.clone()));
        version
    }
    /// `enrichProviderSnapshotWithVersionAdvisory`. With `checks` off no registry is asked.
    pub async fn enrich(&self, snapshot: &mut Value, caps: &Capabilities, checks: bool) {
        let version = snapshot["version"].as_str().filter(|version| !version.is_empty()).map(str::to_owned);
        let resolve = checks && snapshot["enabled"] == true && snapshot["installed"] == true && version.is_some();
        snapshot["versionAdvisory"] = if resolve {
            let latest = self.latest_version(caps).await;
            maintenance::version_advisory(caps, version.as_deref(), latest.as_deref(), Some(&now_iso()))
        } else {
            maintenance::version_advisory(caps, version.as_deref(), None, snapshot["checkedAt"].as_str())
        };
    }
    /// `applyProviderCompatibility` with the bundled policies: only an enabled, installed provider is classified.
    pub fn classify(snapshot: &mut Value) {
        let object = snapshot.as_object_mut().expect("provider snapshot");
        object.remove("compatibilityAdvisory");
        if snapshot["enabled"] != true || snapshot["installed"] != true {
            return;
        }
        let driver = snapshot["driver"].as_str().unwrap_or("").to_owned();
        let advisory = maintenance::compatibility_advisory(&driver, snapshot["version"].as_str(), env!("CARGO_PKG_VERSION"));
        let Some(mut advisory) = advisory else {
            return;
        };
        if let Some(latest) = snapshot["versionAdvisory"]["latestVersion"].as_str().filter(|latest| !latest.is_empty()) {
            if let Some(latest) = maintenance::compatibility_advisory(&driver, Some(latest), env!("CARGO_PKG_VERSION")) {
                advisory["latestVersionStatus"] = latest["status"].clone();
            }
        }
        snapshot["compatibilityAdvisory"] = advisory;
    }
    /// Everything a refresh adds to a freshly probed snapshot: version and compatibility advisories and update state.
    pub async fn finish_snapshot(&self, snapshot: &mut Value, target: Option<&Target>, checks: bool) {
        let probed = snapshot.clone();
        if let Some(target) = target {
            let caps = self.capabilities(target, false).await;
            self.enrich(snapshot, &caps, checks).await;
        }
        Self::classify(snapshot);
        self.apply_update_state(snapshot);
        // Whatever clients receive must still be a provider snapshot of the contract.
        match serde_json::from_value::<t3_contracts::ServerProvider>(snapshot.clone()).map(|typed| serde_json::to_value(typed)) {
            Ok(Ok(typed)) => *snapshot = typed,
            _ => {
                tracing::warn!("provider advisories did not fit the snapshot contract and were dropped");
                *snapshot = probed;
                self.apply_update_state(snapshot);
            }
        }
    }

    // ---- refresh ----

    /// `server.refreshProviders`: re-probe one instance, one instance's workspace, or all of them.
    pub async fn refresh(&self, registry: &ProviderRegistry, input: &ServerRefreshProvidersInput) -> Result<Vec<Value>, ProcessError> {
        let instance = input.instance_id.as_ref().map(|id| id.to_string());
        if input.refresh_models == Some(true) {
            // An explicit catalog refresh bypasses this server's caches.
            for (id, package) in registry.maintenance_packages() {
                if instance.as_ref().is_none_or(|wanted| *wanted == id) {
                    self.invalidate(&id, Some(&package));
                }
            }
        }
        match (&instance, &input.cwd) {
            (Some(instance), Some(cwd)) => registry.refresh_workspace(instance, cwd.as_str(), input.fresh == Some(true)).await,
            (Some(instance), None) => registry.refresh_instance(instance).await,
            (None, _) => registry.refresh_all().await,
        }
    }

    // ---- update ----

    /// `server.updateProvider`. Failures of the update itself are reported through the instance's
    /// `updateState`, not as errors; only "cannot update" and "already updating" are errors.
    pub async fn update_provider(&self, registry: &ProviderRegistry, input: &ServerProviderUpdateInput) -> Result<Vec<Value>, Value> {
        let provider = input.provider.to_string();
        let instance = input.instance_id.as_ref().map(|id| id.to_string()).unwrap_or_else(|| provider.clone());
        let target_version = input.target_version.as_ref().map(|version| version.to_string());
        let target = registry.maintenance_target(&instance).filter(|target| target.driver == provider);
        let caps = match &target {
            Some(target) => self.capabilities(target, false).await,
            None => Capabilities::manual(&provider, None),
        };
        let Some(update) = caps.update.clone() else {
            return Err(update_error(&provider, "This provider does not support one-click updates."));
        };
        let target = target.expect("an update implies a live instance");
        let key = format!("instance:{instance}");
        if !self.0.running.lock().unwrap().insert(key.clone()) {
            return Err(update_error(&provider, "An update is already running for this provider."));
        }
        struct Release<'a>(&'a Management, String);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.0.running.lock().unwrap().remove(&self.1);
            }
        }
        let _release = Release(self, key);
        registry.set_update_state(&instance, Some(update_state("queued", None, None, Some("Waiting for another provider update to finish."), None))).await;
        let lock = self.0.locks.lock().unwrap().entry(update.lock_key.clone()).or_default().clone();
        let _held = lock.lock().await;
        let state = Box::pin(self.run_update(registry, &target, &update.lock_key, target_version.as_deref())).await;
        Ok(registry.set_update_state(&instance, Some(state)).await)
    }
    async fn run_update(&self, registry: &ProviderRegistry, target: &Target, lock_key: &str, target_version: Option<&str>) -> Value {
        let started = now_iso();
        registry.set_update_state(&target.instance_id, Some(update_state("running", Some(&started), None, Some("Updating provider."), None))).await;
        let failed = |message: &str, output: Option<String>| update_state("failed", Some(&started), Some(&now_iso()), Some(message), output);
        // The cached capabilities chose the lock; re-derive ownership now so the command that runs
        // matches the executable as it is at click time, not at the last health refresh.
        let fresh = self.capabilities(target, true).await;
        if fresh.update.as_ref().is_none_or(|update| update.lock_key != lock_key) {
            return failed("Provider installation changed. Refresh and try again.", None);
        }
        let candidate = match target_version {
            Some(version) => Some(version.to_owned()),
            None => self.latest_version(&fresh).await,
        };
        let advisory = maintenance::compatibility_advisory(target.driver, candidate.as_deref(), env!("CARGO_PKG_VERSION"));
        let status = advisory.as_ref().and_then(|advisory| advisory["status"].as_str());
        let command = match target_version {
            Some(version) => maintenance::targeted_update(&fresh, version),
            None => fresh.update.clone(),
        };
        let rejected = match target_version {
            Some(version) => command.is_none() || advisory.as_ref().and_then(|advisory| advisory["recommendedVersion"].as_str()) != Some(version) || status != Some("supported"),
            None => matches!(status, Some("broken" | "unsupported")),
        };
        let Some(command) = command.filter(|_| !rejected) else {
            return failed(
                if target_version.is_some() {
                    "This version is no longer recommended or this installer cannot install a specific version. Refresh provider settings."
                } else {
                    "The latest provider version is incompatible with this T3 Code release. Review provider settings."
                },
                None,
            );
        };
        let result = match self.run_command(&command).await {
            Ok(result) => result,
            Err(message) => return failed(&message, None),
        };
        let finished = now_iso();
        if result.timed_out || result.exit_code != Some(0) {
            let message = if result.timed_out {
                "Update timed out.".to_owned()
            } else if let Some(code) = result.exit_code.filter(|code| *code != 0) {
                format!("Update command exited with code {code}.")
            } else {
                "Update command failed.".to_owned()
            };
            return update_state("failed", Some(&started), Some(&finished), Some(&message), result.output());
        }
        // Homebrew's "latest" moves once the upgrade lands; read it again.
        let verified_caps = self.capabilities(target, true).await;
        let verified = Box::pin(self.verify(registry, target, &verified_caps)).await;
        // "Succeeded" needs the provider to still be installed: an installer that exits 0 and leaves the
        // binary missing is not a success. A missing version alone is not held against it.
        let could_not_verify = verified.is_empty()
            || verified.iter().any(|provider| {
                provider["installed"] != true
                    || target_version.is_some_and(|version| provider["version"].as_str().map(|v| v.strip_prefix('v').unwrap_or(v)) != Some(version))
            });
        let still_outdated = target_version.is_none() && verified.iter().any(|provider| provider["versionAdvisory"]["status"] == "behind_latest");
        let (status, message) = if could_not_verify {
            ("unchanged", "Update command completed, but T3 Code could not verify the provider version.")
        } else if still_outdated {
            ("unchanged", "Update command completed, but T3 Code still detects an outdated provider version.")
        } else {
            ("succeeded", "Provider updated.")
        };
        update_state(status, Some(&started), Some(&finished), Some(message), result.output())
    }
    /// Re-probe the instance and re-read its version advisory against what the installer reports now.
    async fn verify(&self, registry: &ProviderRegistry, target: &Target, caps: &Capabilities) -> Vec<Value> {
        let providers = registry.refresh_instance(&target.instance_id).await.unwrap_or_else(|_| registry.snapshots());
        let mut refreshed: Vec<Value> = providers.into_iter().filter(|provider| provider["driver"] == target.driver && provider["instanceId"] == target.instance_id.as_str()).collect();
        for provider in &mut refreshed {
            self.enrich(provider, caps, true).await;
        }
        refreshed
    }
    async fn run_command(&self, action: &maintenance::UpdateAction) -> Result<CommandResult, String> {
        use tokio::io::AsyncReadExt;
        let options = self.options();
        let mut env: HashMap<String, String> = std::env::vars().collect();
        if let Some(path) = &options.path {
            env.insert("PATH".into(), path.clone());
        }
        let platform = Platform::host();
        let executable = if action.executable.contains('/') || action.executable.contains('\\') {
            action.executable.clone()
        } else {
            maintenance::resolve_command_path(&action.executable, &env, platform).unwrap_or_else(|| action.executable.clone())
        };
        let mut command = if platform == Platform::Win32 && (executable.to_ascii_lowercase().ends_with(".cmd") || executable.to_ascii_lowercase().ends_with(".bat")) {
            // Batch shims need a shell. Only plain words are passed through one.
            let plain = |word: &str| !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "_@./:=\\- ".contains(c));
            if !std::iter::once(&executable).chain(&action.args).all(|word| plain(word)) {
                return Err("Update command failed to run.".into());
            }
            let mut command = tokio::process::Command::new("cmd.exe");
            command.args(["/d", "/s", "/c"]).arg(std::iter::once(format!("\"{executable}\"")).chain(action.args.iter().map(|arg| format!("\"{arg}\""))).collect::<Vec<_>>().join(" "));
            command
        } else {
            let mut command = tokio::process::Command::new(&executable);
            command.args(&action.args);
            command
        };
        command.envs(&env);
        if let Some(extra) = &action.env {
            command.envs(extra);
        }
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| format!("Failed to run update command {}.", action.executable))?;
        let (mut stdout, mut stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
        let collect = async {
            // Both streams are drained to the end so a chatty tool cannot block on a full pipe.
            async fn drain(mut stream: impl tokio::io::AsyncRead + Unpin) -> String {
                let (mut kept, mut buffer) = (Vec::new(), [0u8; 4096]);
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            let room = OUTPUT_MAX_BYTES.saturating_sub(kept.len());
                            kept.extend_from_slice(&buffer[..count.min(room)]);
                        }
                    }
                }
                String::from_utf8_lossy(&kept).into_owned()
            }
            let (out, err, status) = tokio::join!(drain(&mut stdout), drain(&mut stderr), child.wait());
            (out, err, status.ok().and_then(|status| status.code()))
        };
        match tokio::time::timeout(options.update_timeout, collect).await {
            Ok((stdout, stderr, exit_code)) => Ok(CommandResult { stdout, stderr, exit_code, timed_out: false }),
            Err(_) => Ok(CommandResult { stdout: String::new(), stderr: String::new(), exit_code: None, timed_out: true }),
        }
    }
}
struct CommandResult {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    timed_out: bool,
}
impl CommandResult {
    /// stderr then stdout, trimmed and bounded, with anything that looks like a credential masked.
    fn output(&self) -> Option<String> {
        let joined = [self.stderr.as_str(), self.stdout.as_str()].into_iter().filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n\n");
        let trimmed = joined.trim();
        (!trimmed.is_empty()).then(|| redact(trimmed).chars().take(OUTPUT_MAX_BYTES).collect())
    }
}
/// Update tools echo configuration on failure. Tokens and passwords in that echo are not for clients.
pub fn redact(text: &str) -> String {
    use regex::Regex;
    use std::sync::OnceLock;
    static ASSIGNMENT: OnceLock<Regex> = OnceLock::new();
    static NPM_TOKEN: OnceLock<Regex> = OnceLock::new();
    let assignment = ASSIGNMENT.get_or_init(|| Regex::new(r"(?i)(_authtoken|_auth|_password|api[_-]?key|token|secret|password|bearer)(\s*[=:]\s*|\s+)([^\s'\x22]+)").unwrap());
    let npm = NPM_TOKEN.get_or_init(|| Regex::new(r"npm_[A-Za-z0-9]{20,}").unwrap());
    let masked = assignment.replace_all(text, "$1$2[redacted]");
    npm.replace_all(&masked, "[redacted]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_masks_credentials_but_keeps_diagnostics() {
        let text = "npm ERR! //registry.npmjs.org/:_authToken=abcDEF123\nERR! token: ghp_secretvalue\nnpm_abcdefghijklmnopqrstuvwxyz0123 failed\nEACCES permission denied";
        let masked = redact(text);
        assert!(!masked.contains("abcDEF123") && !masked.contains("ghp_secretvalue") && !masked.contains("npm_abcdefghij"));
        assert!(masked.contains("EACCES permission denied"));
    }
}
