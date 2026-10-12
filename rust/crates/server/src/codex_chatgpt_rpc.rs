//! `provider.chatgpt.reconnect-profile`, `provider.chatgpt.import-profile` and `provider.chatgpt.handoff.subscribe` (`ws.ts`,
//! `ProviderAuthService.ts`, `CodexChatGptHandoff.ts`): moving a ChatGPT connection of managed Codex between environments. A primary that
//! owns a browser signs in on behalf of a remote environment (the handoff, whose tokens exist only in memory) and sends the profile it made
//! there; the destination keeps it with `import-profile` and later asks the primary for the registration to sign in again with.
use crate::{
    codex_chatgpt_auth::{ChatGptAuth, MemorySecrets, Options},
    persistence::Store,
    provider_auth_flow::AuthFlow,
    server_secret_store::ServerSecretStore,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use t3_contracts::*;
use tokio::sync::mpsc;

fn setup_error(instance: &str, operation: &str, detail: &str) -> Value {
    json!({"_tag":"ProviderSetupError","instanceId":instance,"operation":operation,"detail":detail})
}
fn wire(error: ProviderSetupError) -> Value {
    serde_json::to_value(error).expect("typed provider setup error")
}
fn decode<T: serde::de::DeserializeOwned>(payload: Value) -> Result<T, Value> {
    serde_json::from_value(payload).map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))
}

/// What the environment needs to sign in: where its secrets are and who it is. The settings decide whether the instance is a managed Codex.
pub struct Environment {
    pub environment_id: String,
    pub default_return_url: Option<String>,
    /// Overrides OpenAI's endpoints, for a test that serves its own issuer.
    pub discovery_url: Option<String>,
    pub resource: Option<String>,
    /// Where the account's model catalog is read (`https://api.openai.com/v1/models`); a test serves its own.
    pub models_url: Option<String>,
    pub http: reqwest::Client,
}

impl Environment {
    fn auth(&self, store: &Store, instance: &str, operation: &str, profile: Option<ChatGptReconnectProfile>) -> Result<ChatGptAuth, Value> {
        let state_dir = store.state_dir().ok_or_else(|| json!({"_tag":"NativeServiceUnavailableError","message":"ChatGPT sign-in needs a state directory."}))?;
        self.auth_in(&state_dir, instance, operation, profile)
    }
    /// The sign-in of `instance`, its secrets under `state_dir`. Equal instances share the lock that keeps a refresh to one at a time.
    pub fn auth_in(&self, state_dir: &std::path::Path, instance: &str, operation: &str, profile: Option<ChatGptReconnectProfile>) -> Result<ChatGptAuth, Value> {
        let secrets = ServerSecretStore::open(state_dir.join("secrets")).map_err(|_| setup_error(instance, operation, "Could not read the saved ChatGPT connection."))?;
        ChatGptAuth::new(Options { instance_id: instance.to_owned(), environment_id: self.environment_id.clone(), discovery_url: self.discovery_url.clone(), resource: self.resource.clone(), default_return_url: self.default_return_url.clone(), reconnect_profile: profile, secrets: Arc::new(secrets), http: self.http.clone(), clock: Arc::new(|| chrono::Utc::now().timestamp_millis()) }).map_err(wire)
    }
}

/// `provider.chatgpt.reconnect-profile`: the registration for a method, or `null` when there is none to reuse.
pub fn reconnect_profile(environment: &Environment, store: &Store, payload: Value) -> Result<Value, Value> {
    let input: ChatGptReconnectProfileInput = decode(payload)?;
    let auth = environment.auth(store, input.instance_id.as_str(), "export", None)?;
    Ok(serde_json::to_value(auth.reconnect_profile(&input.method_id).map_err(wire)?).expect("a typed profile"))
}

/// `provider.chatgpt.import-profile` on a server with no sign-in service of its own for the instance: validates the profile against the
/// issuer and keeps it, through a flow that has no sessions to end. The answer is the sign-in state a finished import leaves.
pub async fn import_profile(environment: &Environment, store: &Store, payload: Value) -> Result<Value, Value> {
    let input: ChatGptImportProfileInput = decode(payload)?;
    let instance = input.instance_id.as_str();
    let auth = environment.auth(store, instance, "import", None)?;
    let flow = AuthFlow::new(instance.to_owned(), Arc::new(auth.clone()), None, Duration::from_secs(300), ProviderCredentialOwner::T3, true);
    let state = auth.import_profile(&flow, input.profile, Box::pin(async { Ok(()) })).await;
    flow.shutdown().await;
    Ok(serde_json::to_value(state.map_err(wire)?).expect("typed sign-in state"))
}

/// `provider.chatgpt.handoff.subscribe`: the sign-in the primary asked this environment to run, as its states, then the finished profile.
/// Nothing is kept on this environment's disk: the tokens live in memory until the stream ends.
pub fn handoff(environment: &Environment, payload: Value, owner: String) -> Result<mpsc::Receiver<Result<Value, Value>>, Value> {
    let input: ChatGptHandoffInput = decode(payload)?;
    let instance = input.instance_id.as_str().to_owned();
    let auth = ChatGptAuth::new(Options {
        instance_id: instance.clone(),
        environment_id: input.environment_id.as_str().to_owned(),
        discovery_url: environment.discovery_url.clone(),
        resource: environment.resource.clone(),
        default_return_url: Some(input.return_url.0.clone()),
        reconnect_profile: input.profile.clone(),
        secrets: Arc::new(MemorySecrets::default()),
        http: environment.http.clone(),
        clock: Arc::new(|| chrono::Utc::now().timestamp_millis()),
    })
    .map_err(wire)?;
    let flow = AuthFlow::new(instance.clone(), Arc::new(auth.clone()), None, Duration::from_secs(300), ProviderCredentialOwner::T3, true);
    let (events, receiver) = mpsc::channel(8);
    let return_url = input.return_url.0.clone();
    tokio::spawn(async move {
        let mut subscription = flow.subscribe(owner.clone());
        let started = flow.start(owner, Some("chatgpt".into()), Some(return_url), Some(ProviderAuthCallbackMode::Server), crate::provider_auth_flow::RoutedStop::new(Arc::new(|| Box::pin(async { Ok(()) })))).await;
        if let Err(error) = started {
            let _ = events.send(Err(wire(error))).await;
            return;
        }
        while let Some(state) = subscription.recv().await {
            let finished = matches!(state.phase, ProviderAuthPhase::Succeeded | ProviderAuthPhase::Failed | ProviderAuthPhase::Cancelled);
            let item = if state.phase == ProviderAuthPhase::Succeeded {
                match auth.export_profile() {
                    Ok(profile) => Ok(json!({"phase":"finished","profile":profile})),
                    Err(error) => Err(wire(error)),
                }
            } else {
                Ok(json!({"phase":"auth","state":state}))
            };
            let failed = item.is_err();
            if events.send(item).await.is_err() || finished || failed {
                break;
            }
        }
        flow.shutdown().await;
    });
    Ok(receiver)
}

// ---- the server's environment --------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Slot(std::sync::OnceLock<Arc<Environment>>);

/// Installs how this server reaches OpenAI; a server that never installed it answers these methods as unavailable.
pub fn install(store: &Store, environment: Environment) {
    let _ = store.extension(Slot::default).0.set(Arc::new(environment));
}
pub fn current(store: &Store) -> Result<Arc<Environment>, Value> {
    store.extension(Slot::default).0.get().cloned().ok_or_else(|| json!({"_tag":"NativeServiceUnavailableError","message":"ChatGPT sign-in is not configured."}))
}

/// `getController`: the instance must exist and be a managed Codex, the only provider with a ChatGPT sign-in.
pub fn require_managed_codex(settings: &Value, instance: &str, operation: &str) -> Result<(), Value> {
    let unsupported = || setup_error(instance, operation, "This provider does not support sign-in in T3 Code.");
    match settings["providerInstances"].get(instance) {
        Some(entry) if entry["driver"] == "codex" && entry["config"]["setupMode"] == "managed" => Ok(()),
        Some(_) => Err(unsupported()),
        // A default instance has no entry of its own until it is configured; its settings are the legacy `providers.<driver>`.
        None => match settings["providers"].get(instance) {
            Some(config) if instance == "codex" && config["setupMode"] == "managed" => Ok(()),
            Some(_) => Err(unsupported()),
            None => Err(setup_error(instance, operation, "This provider instance is no longer available.")),
        },
    }
}
