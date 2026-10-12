//! Managed Codex: the Codex a user signs in to with ChatGPT, run on the tokens T3 Code keeps (`provider/CodexManagedRuntime.ts`,
//! `provider/Drivers/CodexManagedProvider.ts`, `provider/CodexChatGptModels.ts`).
//!
//! The tokens never reach Codex's own `auth.json`. A session starts the installed `codex` with the access token in its environment and a
//! launch configuration that names a model provider reading it (`resolve`), and the provider list reports what that sign-in gives
//! (`snapshot`). Both read the sign-in through a [`ChatGptHost`], which a server installs next to the settings.
use crate::{
    codex::CodexInstance,
    codex_chatgpt_auth::{ChatGptAuth, REQUIRED_SCOPE},
    codex_chatgpt_rpc::Environment,
    provider_manage::Management,
    provider_process::{ProcessError, ProcessOptions},
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::{Path, PathBuf}, sync::Arc, time::Duration};
use t3_contracts::{ProviderSetupError, ProviderSetupErrorTag};

/// How this server reaches OpenAI and where it keeps the tokens: the environment the settings are served from.
#[derive(Clone)]
pub struct ChatGptHost {
    pub environment: Arc<Environment>,
    pub state_dir: PathBuf,
}

impl ChatGptHost {
    /// The sign-in of `instance` (a handle on the stored tokens).
    pub fn auth(&self, instance: &str, operation: &str) -> Result<ChatGptAuth, ProviderSetupError> {
        self.environment.auth_in(&self.state_dir, instance, operation, None).map_err(|_| setup_error(instance, operation, "Could not read the saved ChatGPT connection."))
    }
}

fn setup_error(instance: &str, operation: &str, detail: &str) -> ProviderSetupError {
    ProviderSetupError { tag: ProviderSetupErrorTag::ProviderSetupError, instance_id: instance.parse().expect("validated instance"), operation: operation.into(), detail: detail.into(), cause: None }
}

/// `managedCodexLaunchArgs`: Codex reads the token from `ACCESS_TOKEN` through a provider of its own, so nothing is written to its auth file.
const LAUNCH_ARGS: [&str; 9] = [
    "model_provider=\"openai_token_sharing\"",
    "model_providers.openai_token_sharing.name=\"OpenAI Token Sharing\"",
    "model_providers.openai_token_sharing.base_url=\"https://api.openai.com/v1\"",
    "model_providers.openai_token_sharing.model_catalog_url=\"https://api.openai.com/v1/models\"",
    "features.api_key_model_discovery=true",
    "model_providers.openai_token_sharing.env_key=\"ACCESS_TOKEN\"",
    "model_providers.openai_token_sharing.wire_api=\"responses\"",
    "model_providers.openai_token_sharing.requires_openai_auth=false",
    "model_providers.openai_token_sharing.supports_websockets=false",
];

/// The launch arguments in the form a setting holds them: each as `-c '<value>'`.
pub fn launch_args() -> String {
    LAUNCH_ARGS.iter().map(|value| format!("-c '{value}'")).collect::<Vec<_>>().join(" ")
}

/// `CodexEffectiveRuntime`: what a session of a managed instance starts.
#[derive(Debug, Clone)]
pub struct Effective {
    pub binary: PathBuf,
    pub environment: HashMap<String, String>,
    pub launch_args: String,
    pub home: PathBuf,
    pub access_token: String,
}

impl Effective {
    pub fn process_options(&self, cwd: &Path) -> ProcessOptions {
        let mut args = vec!["app-server".to_owned()];
        args.extend(crate::codex::tokenize_cli_args(&self.launch_args));
        ProcessOptions { binary: self.binary.clone(), args, cwd: cwd.into(), environment: self.environment.clone() }
    }
}

/// `makeCodexManagedRuntime`'s `resolve`: the installed executable, the signed-in tokens (renewed when they are about to lapse), and the
/// home Codex runs in. The failures are the sentences the original shows.
pub async fn resolve(host: &ChatGptHost, management: &Management, instance: &CodexInstance) -> Result<Effective, ProviderSetupError> {
    let id = instance.instance_id.as_str();
    let installation_error = || setup_error(id, "install", "Set up managed Codex before starting a session.");
    let installation = management.codex_installation().await.map_err(|_| installation_error())?;
    let executable = installation.resolve().await.map_err(|_| installation_error())?;
    let credentials = host.auth(id, "access")?.access().await?;
    let layout = crate::codex_home::resolve_managed(&host.state_dir, id, instance.config.home_path.as_str(), instance.config.shadow_home_path.as_str(), &instance.environment);
    crate::codex_home::materialize(&layout).map_err(|detail| setup_error(id, "runtime", &detail))?;
    let home = layout.effective.clone().unwrap_or_else(|| layout.shared.clone());
    std::fs::create_dir_all(&home).map_err(|_| setup_error(id, "runtime", "Could not prepare the managed Codex runtime."))?;
    // TODO(original CodexManagedRuntime.ts): "Ambient CLI overrides cannot redirect a T3-owned token" is enforced on this environment alone;
    // the server's own `OPENAI_API_KEY` and `OPENAI_BASE_URL` are still inherited by the child. The launch configuration names the provider
    // and its base URL, so they have no effect on where the token goes.
    let mut environment = instance.environment.clone();
    environment.insert("ACCESS_TOKEN".into(), credentials.access_token.clone());
    environment.insert("CODEX_HOME".into(), home.to_string_lossy().into_owned());
    for ambient in ["T3CODE_CODEX_LAUNCH_ARGS", "OPENAI_API_KEY", "OPENAI_BASE_URL"] {
        environment.remove(ambient);
    }
    Ok(Effective { binary: executable.path, environment, launch_args: launch_args(), home, access_token: credentials.access_token })
}

// ---- the provider list --------------------------------------------------------------------------------------------------------------

/// `chatGptModels`: the account's choices come from OpenAI; the app-server's own list contributes capability metadata only.
async fn account_models(host: &ChatGptHost, access_token: &str, native: &[Value]) -> Result<Vec<Value>, String> {
    let url = host.environment.models_url.clone().unwrap_or_else(|| "https://api.openai.com/v1/models".into());
    let response = tokio::time::timeout(Duration::from_secs(15), host.environment.http.get(url).bearer_auth(access_token).send()).await.map_err(|_| "timeout".to_owned())?.map_err(|error| error.to_string())?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!("status {}", response.status()));
    }
    let catalog: Value = tokio::time::timeout(Duration::from_secs(15), response.json()).await.map_err(|_| "timeout".to_owned())?.map_err(|error| error.to_string())?;
    let mut models = vec![];
    for model in catalog["models"].as_array().ok_or("catalog")? {
        let (Some(slug), Some(name), Some(visibility)) = (model["slug"].as_str().filter(|slug| !slug.is_empty()), model["display_name"].as_str().filter(|name| !name.is_empty()), model["visibility"].as_str()) else { return Err("catalog".into()) };
        if visibility != "list" {
            continue;
        }
        let known = native.iter().find(|native| native["slug"] == slug);
        let mut entry = known.cloned().unwrap_or_else(|| json!({}));
        entry["capabilities"] = known.map_or(Value::Null, |known| known["capabilities"].clone());
        entry["slug"] = json!(slug);
        entry["name"] = json!(name);
        entry["isCustom"] = json!(false);
        models.push(entry);
    }
    Ok(models)
}

/// What a managed-setup instance reports (`CodexManagedProvider`'s `check`): the installation, then the sign-in, then what Codex says
/// when started on the signed-in tokens. `host` is `None` on a server that keeps no ChatGPT sign-in.
pub async fn snapshot(host: Option<&ChatGptHost>, management: &Management, instance: &CodexInstance) -> Result<Value, ProcessError> {
    let id = instance.instance_id.as_str();
    let layout = crate::codex_home::resolve_managed(host.map_or(Path::new(""), |host| host.state_dir.as_path()), id, instance.config.home_path.as_str(), instance.config.shadow_home_path.as_str(), &instance.environment);
    let mut base = json!({"instanceId":id,"driver":"codex","displayName":instance.display_name,"enabled":instance.enabled,"installed":false,"version":null,
        "status":"warning","auth":{"status":"unknown"},"checkedAt":crate::provider_manage::now_iso(),"availability":"available","models":[],"slashCommands":[],"skills":[],
        "showInteractionModeToggle":true,"reportsContextWindow":true,"setup":{"canAuthenticate":true,"canInstall":true},
        "runtimePaths":{"homePath":layout.shared.to_string_lossy(),"shadowHomePath":layout.auth_overlay.then(|| layout.effective.as_ref().map(|path| path.to_string_lossy().into_owned())).flatten()},
        "continuation":{"groupKey":layout.continuation_key}});
    if let Some(accent) = &instance.accent_color {
        base["accentColor"] = json!(accent);
    }
    if !instance.enabled {
        base["status"] = json!("disabled");
        base["message"] = json!("Codex is disabled in T3 Code settings.");
        return validated(base);
    }
    let executable = match management.codex_installation().await {
        Ok(installation) => installation.resolve().await.ok(),
        Err(_) => None,
    };
    let Some(executable) = executable else {
        base["auth"] = json!({"status":"unauthenticated"});
        base["message"] = json!("Set up Codex to get started.");
        return validated(base);
    };
    base["installed"] = json!(true);
    base["version"] = json!(executable.version);
    let saved = host.and_then(|host| host.auth(id, "read").ok()).and_then(|auth| auth.read().ok().flatten());
    let Some(saved) = saved else {
        base["auth"] = json!({"status":"unauthenticated"});
        base["message"] = json!("Sign in with ChatGPT to use Codex.");
        return validated(base);
    };
    let email = saved.email.as_deref().map(str::trim).filter(|email| !email.is_empty()).map(str::to_owned);
    let sharing = saved.scopes.iter().any(|scope| scope == REQUIRED_SCOPE);
    if !sharing {
        base["auth"] = json!({"status":"unauthenticated","label":"ChatGPT"});
        if let Some(email) = &email {
            base["auth"]["email"] = json!(email);
        }
        base["message"] = json!("Signed in with ChatGPT, but token sharing is disabled. Sign in again and enable token sharing, or use another provider.");
        return validated(base);
    }
    let host = host.expect("a saved sign-in came from a host");
    let usage_limits = json!({"checkedAt":base["checkedAt"],"windows":[],"unavailable":{"reason":"unsupported","message":"ChatGPT tracks subscription usage across connected apps. Open Usage settings with the account you connected to Codex."},"externalUsage":{"label":"ChatGPT usage","url":"https://chatgpt.com/#settings/Usage"}});
    let mut managed_auth = json!({"subscriptionSharing":true,"profileId":saved.client_id,"status":"authenticated","type":"chatgpt","label":"ChatGPT"});
    if let Some(email) = &email {
        managed_auth["email"] = json!(email);
    }
    let checked = async {
        let effective = resolve(host, management, instance).await.map_err(|error| error.detail)?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let probe = tokio::time::timeout(Duration::from_secs(10), instance.probe_process(effective.process_options(&cwd), &cwd, base["checkedAt"].as_str().unwrap_or_default(), true)).await.map_err(|_| "probe timeout".to_owned())?.map_err(|error| error.to_string())?;
        let native: Vec<Value> = probe["models"].as_array().cloned().unwrap_or_default();
        let models = account_models(host, &effective.access_token, &native).await?;
        Ok::<_, String>((probe, models))
    };
    match checked.await {
        Ok((probe, models)) => {
            base["status"] = probe["status"].clone();
            base["version"] = if probe["version"].is_null() { base["version"].clone() } else { probe["version"].clone() };
            base["auth"] = managed_auth;
            base["usageLimits"] = usage_limits;
            base["skills"] = probe["skills"].clone();
            // Codex's feedback command sends the thread to OpenAI, not to this account.
            base["slashCommands"] = json!([{"name":"compact","description":"Summarize the conversation and reduce context usage"}]);
            // ChatGPT token sharing does not accept service tiers.
            base["models"] = json!(models.into_iter().map(without_service_tier).collect::<Vec<_>>());
            if let Some(message) = probe.get("message") {
                base["message"] = message.clone();
            }
        }
        Err(_) => {
            base["auth"] = managed_auth;
            base["usageLimits"] = usage_limits;
            base["message"] = json!("Could not check Codex right now. Retry, or reconnect in provider settings.");
        }
    }
    validated(base)
}

fn without_service_tier(mut model: Value) -> Value {
    if let Some(descriptors) = model.get_mut("capabilities").and_then(|capabilities| capabilities.get_mut("optionDescriptors")).and_then(Value::as_array_mut) {
        descriptors.retain(|option| option["id"] != "serviceTier");
    }
    model
}

fn validated(snapshot: Value) -> Result<Value, ProcessError> {
    let typed: t3_contracts::ServerProvider = serde_json::from_value(snapshot).map_err(|error| ProcessError::Protocol(error.to_string()))?;
    serde_json::to_value(typed).map_err(|error| ProcessError::Protocol(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launch_configuration_names_the_token_provider_and_no_other() {
        let args = crate::codex::tokenize_cli_args(&launch_args());
        assert_eq!(args.len(), 18);
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "model_provider=\"openai_token_sharing\"");
        assert!(args.iter().any(|arg| arg == "model_providers.openai_token_sharing.env_key=\"ACCESS_TOKEN\""));
        assert!(args.iter().any(|arg| arg == "model_providers.openai_token_sharing.requires_openai_auth=false"));
    }

    #[test]
    fn service_tiers_are_not_offered_on_shared_tokens() {
        let model = without_service_tier(json!({"slug":"m","capabilities":{"optionDescriptors":[{"id":"serviceTier"},{"id":"reasoningEffort"}]}}));
        assert_eq!(model["capabilities"]["optionDescriptors"], json!([{"id":"reasoningEffort"}]));
        assert_eq!(without_service_tier(json!({"slug":"m","capabilities":null})), json!({"slug":"m","capabilities":null}));
    }
}
