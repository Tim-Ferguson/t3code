//! Antigravity's sign-in: `AntigravityAuth.ts`, `antigravityCallback.ts`.
//!
//! The Google flow is the agent's own. T3 starts the agent with the instance's private profile and the method the
//! instance is configured for, and shows the user the authorization URL the agent prints. The agent listens on its
//! loopback redirect, exchanges the code for tokens and keeps them in its profile (`acp_token.json`, file storage
//! forced); T3 never receives, stores or refreshes a token, so there is no token endpoint here to point anywhere.
//! A user who is not on the machine pastes the redirect URL instead, and T3 forwards that one request to the
//! agent's loopback, after checking it belongs to the sign-in in progress. Sign-out is the agent's own `logout`.
use crate::{
    acp_peer::ProcessPeer,
    acp_runtime::{AcpSession, StartHooks},
    antigravity_acp::{self as acp, AuthorizationUrl},
    antigravity_flavor::{Antigravity, OnAuthorizationUrl},
    antigravity_instance::{AntigravityInstance, Holds, initialize_request},
    provider_auth_flow::{AuthBackend, AuthContext, AuthFlow, AuthResult, Complete},
    provider_registry::ProviderRegistry,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_acp::AcpError;
use t3_contracts::*;
use tokio::sync::watch;

const FORWARDING_FAILED_MESSAGE: &str = "Could not deliver the sign-in response. Start sign-in again.";
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(90);

/// The loopback the callback is sent to, and how long it may take. Tests aim the agent's redirect at a server of
/// their own; nothing here ever contacts Google.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);

fn failure(instance: &str, operation: &str, detail: &str) -> ProviderSetupError {
    ProviderSetupError { tag: ProviderSetupErrorTag::ProviderSetupError, instance_id: instance.parse().expect("validated instance"), operation: operation.into(), detail: detail.into(), cause: None }
}
/// A directory that goes with the sign-in that made it.
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
/// What a failed sign-in tells the user (`safeAuthFailure`): never the agent's own text.
fn safe_failure(cause: &AcpError, uses_browser: bool) -> String {
    use t3_acp::errors::Failure;
    let request = match cause {
        AcpError::Failure(failure) => match failure.as_ref() {
            Failure::Request(error) => Some((error.code, error.error_message.clone(), error.diagnostics.method.clone())),
            _ => None,
        },
        AcpError::ResponseError { error, method, .. } => Some((error.code, error.message.clone(), Some(method.clone()))),
        AcpError::Request(error) => Some((error.code, error.message.clone(), None)),
        _ => None,
    };
    if let Some((code, message, method)) = request {
        if message.contains("SUBSCRIPTION_REQUIRED") {
            return "Google requires an eligible Antigravity subscription for this account.".into();
        }
        let lower = message.to_lowercase();
        if ["access_denied", "denied access", "cancelled"].iter().any(|needle| lower.contains(needle)) {
            return "Google sign-in was not approved. Start sign-in again.".into();
        }
        if method.as_deref() == Some("session/new") && code == -32603 {
            return "Antigravity authenticated, but could not initialize a session or load models.".into();
        }
        if !uses_browser && code == -32602 {
            return "Antigravity rejected the configured credentials. Check the provider settings.".into();
        }
    }
    if uses_browser { "Google sign-in failed. Start sign-in again.".into() } else { "Antigravity could not authenticate with the configured credentials.".into() }
}
/// `forwardAntigravityCallback`: one GET to the agent's loopback, with no proxy, no redirect, and no response logged.
async fn forward_callback(callback: &url::Url) -> bool {
    let Ok(client) = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(FORWARD_TIMEOUT).build() else { return false };
    client.get(callback.as_str()).send().await.is_ok_and(|response| response.status().is_success())
}
/// `removeAntigravitySessionFiles`: the temporary session a sign-in made is removed once its process has ended. The
/// unique temporary working directory proves which session is ours.
fn remove_session_files(profile: &Path, session_id: &str, cwd: &Path) {
    let native = regex::Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$").unwrap();
    if !native.is_match(session_id) {
        return;
    }
    let directory = profile.join("antigravity-acp");
    let base = directory.join("conversations").join(session_id);
    let Ok(meta) = std::fs::read_to_string(format!("{}.meta", base.display())) else { return };
    let Ok(meta) = serde_json::from_str::<Value>(&meta) else { return };
    if meta["cwd"].as_str() != Some(&cwd.to_string_lossy()) {
        return;
    }
    for suffix in [".db", ".db-wal", ".db-shm", ".db-journal", ".meta"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", base.display()));
    }
    let _ = std::fs::remove_dir_all(directory.join("brain").join(session_id));
}

#[derive(Clone)]
pub struct AntigravityAuth {
    instance: AntigravityInstance,
    registry: ProviderRegistry,
    pending: Arc<Mutex<Option<Pending>>>,
}
struct Pending {
    peer: ProcessPeer,
    services: crate::acp_client_callbacks::Services,
}
impl Pending {
    async fn close(self) {
        self.services.shutdown().await;
        self.peer.shutdown().await;
    }
}
impl AntigravityAuth {
    pub fn new(instance: AntigravityInstance, registry: ProviderRegistry) -> Self {
        Self { instance, registry, pending: Arc::new(Mutex::new(None)) }
    }
    pub fn controller(&self) -> AuthFlow {
        AuthFlow::new(self.instance.instance_id.clone(), Arc::new(self.clone()), Some(self.instance.auth().auth_method), AUTH_TIMEOUT, ProviderCredentialOwner::Provider, false)
    }
    fn fail(&self, operation: &str, detail: &str) -> ProviderSetupError {
        failure(&self.instance.instance_id, operation, detail)
    }
    async fn close_peer(&self) {
        let pending = self.pending.lock().unwrap().take();
        if let Some(pending) = pending {
            pending.close().await;
        }
    }
    /// Starts the agent for sign-in and sets up a session: for a browser method the agent prints the authorization
    /// URL first, and the session starts once the user has finished (in the agent's own browser tab or by pasting
    /// the redirect URL); for the other methods it authenticates with what the instance holds.
    async fn sign_in(&self, method: String, context: AuthContext) -> AuthResult<()> {
        let auth = self.instance.auth();
        let uses_browser = auth.uses_browser();
        if let Some(issue) = auth.config_issue() {
            return Err(self.fail("start", issue));
        }
        let scratch = Scratch(std::env::temp_dir().join(format!("t3-antigravity-setup-{}", uuid::Uuid::new_v4().simple())));
        std::fs::create_dir_all(&scratch.0).map_err(|_| self.fail("start", "Could not create an Antigravity setup workspace."))?;
        let cwd = scratch.0.clone();
        let holds = Holds::default();
        let process = self.instance.spawn_options(&cwd, &holds, &HashMap::new()).await.map_err(|error| self.fail("start", &error.to_string().replace("Provider protocol failed: ", "")))?;
        let profile = process.environment.get("GEMINI_HOME").map(PathBuf::from);
        // The URL the agent prints, and the delivery of a pasted redirect.
        let pending_url: Arc<Mutex<Option<AuthorizationUrl>>> = Arc::new(Mutex::new(None));
        let (forwarding_failed, mut failed) = watch::channel(false);
        let sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let complete: Complete = {
            let (pending_url, sent, context, instance_id) = (pending_url.clone(), sent.clone(), context.clone(), self.instance.instance_id.clone());
            Arc::new(move |callback_url: String| -> BoxFuture<'static, AuthResult<()>> {
                let (pending_url, sent, context, instance_id, forwarding_failed) = (pending_url.clone(), sent.clone(), context.clone(), instance_id.clone(), forwarding_failed.clone());
                Box::pin(async move {
                    let Some(pending) = pending_url.lock().unwrap().clone() else {
                        return Err(failure(&instance_id, "complete", "Wait for the Google sign-in link before you send a redirect URL."));
                    };
                    let callback = acp::validate_callback_url(&pending, &callback_url).map_err(|detail| failure(&instance_id, "complete", detail))?;
                    if sent.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        return Err(failure(&instance_id, "complete", "The sign-in response was already sent. Wait for Google to finish."));
                    }
                    context.verifying();
                    if forward_callback(&callback).await {
                        return Ok(());
                    }
                    // The sign-in cannot go on without its response: it ends here.
                    forwarding_failed.send_replace(true);
                    Err(failure(&instance_id, "complete", FORWARDING_FAILED_MESSAGE))
                })
            })
        };
        let on_url: OnAuthorizationUrl = {
            let (pending_url, context) = (pending_url.clone(), context.clone());
            Arc::new(move |url: String| -> Result<(), AcpError> {
                let request = acp::parse_authorization_url(&url).ok_or_else(|| AcpError::Transport("Antigravity returned an invalid Google sign-in URL.".into()))?;
                let mut slot = pending_url.lock().unwrap();
                if let Some(existing) = slot.as_ref() {
                    return if existing.authorization_url == request.authorization_url { Ok(()) } else { Err(AcpError::Transport("Antigravity started more than one Google sign-in request.".into())) };
                }
                let interaction = serde_json::from_value(json!({"type":"browser","id":context.flow_id,"url":request.authorization_url,"requiresConsent":false,"acceptsCallback":true})).expect("browser interaction");
                *slot = Some(request);
                context.set_interaction(interaction, None, Some(complete.clone()));
                Ok(())
            })
        };
        let peer = ProcessPeer::spawn_with_options(process, Antigravity::sign_in_options(on_url)).map_err(|_| self.fail("start", "Could not start the Antigravity agent."))?;
        let acp_instance = self.instance.acp_instance();
        let services = acp_instance.services_for(&cwd, json!({"mode":"readonly"}));
        *self.pending.lock().unwrap() = Some(Pending { peer: peer.clone(), services: services.clone() });
        let hooks = StartHooks { initialize: Some(initialize_request("t3-code-provider-setup", false)), authenticate: Some(method), ..Default::default() };
        let started: Result<AcpSession, AcpError> = tokio::select! {
            biased;
            _ = failed.wait_for(|failed| *failed) => Err(AcpError::Transport(FORWARDING_FAILED_MESSAGE.into())),
            started = Box::pin(acp_instance.start_peer_flavored(peer, &cwd, None, services, None, &hooks)) => started,
        };
        let session = match started {
            Ok(session) => session,
            Err(AcpError::Transport(detail)) if detail == FORWARDING_FAILED_MESSAGE => return Err(self.fail("start", FORWARDING_FAILED_MESSAGE)),
            Err(cause) => return Err(self.fail("start", &safe_failure(&cause, uses_browser))),
        };
        context.verifying();
        // The account works: the instance's snapshot learns the models and commands it offers.
        let initialize = serde_json::to_value(&session.initialize).unwrap_or_default();
        let started = json!({"version":initialize["agentInfo"]["version"],"setup":serde_json::to_value(&session.setup_response).unwrap_or_default()});
        let session_id = session.setup_response.session_id.to_string();
        let (label, auth_type) = (auth.label(), auth.auth_method.clone());
        self.registry.antigravity_update(&self.instance.instance_id, |_, snapshot, now| crate::antigravity_instance::session_started(snapshot, &started, None, label, &auth_type, now));
        session.shutdown().await;
        self.close_peer().await;
        if let Some(profile) = profile {
            remove_session_files(&profile, &session_id, &cwd);
        }
        drop(holds);
        Ok(())
    }
}
impl AuthBackend for AntigravityAuth {
    fn methods(&self) -> BoxFuture<'static, AuthResult<Vec<ProviderAuthMethod>>> {
        let auth = self.instance.auth();
        let method = serde_json::from_value(json!({"id":auth.auth_method,"name":auth.label(),"description":null,"type":"agent"})).expect("sign-in method");
        Box::pin(async move { Ok(vec![method]) })
    }
    fn authenticate(&self, method: String, context: AuthContext) -> BoxFuture<'static, AuthResult<()>> {
        let backend = self.clone();
        Box::pin(async move { backend.sign_in(method, context).await })
    }
    fn cleanup(&self) -> BoxFuture<'static, ()> {
        let backend = self.clone();
        Box::pin(async move { backend.close_peer().await })
    }
    fn cleanup_methods(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
    fn logout(&self) -> BoxFuture<'static, AuthResult<Option<String>>> {
        let backend = self.clone();
        Box::pin(async move {
            let instance_id = backend.instance.instance_id.clone();
            let result = tokio::time::timeout(LOGOUT_TIMEOUT, async {
                let cwd = std::env::temp_dir().join(format!("t3-antigravity-logout-{}", uuid::Uuid::new_v4().simple()));
                std::fs::create_dir_all(&cwd).map_err(|_| failure(&instance_id, "logout", "Antigravity sign-out failed. Try again."))?;
                let scratch = Scratch(cwd.clone());
                let holds = Holds::default();
                let process = backend.instance.spawn_options(&cwd, &holds, &HashMap::new()).await.map_err(|error| failure(&instance_id, "logout", &error.to_string().replace("Provider protocol failed: ", "")))?;
                let peer = ProcessPeer::spawn(process).map_err(|_| failure(&instance_id, "logout", "Antigravity sign-out failed. Try again."))?;
                let acp_instance = backend.instance.acp_instance();
                let services = acp_instance.services_for(&cwd, json!({"mode":"readonly"}));
                *backend.pending.lock().unwrap() = Some(Pending { peer: peer.clone(), services: services.clone() });
                let initialized = acp_instance.initialize_only(peer, true, services, None, Some(initialize_request("t3-code-provider-setup", false))).await.map_err(|_| failure(&instance_id, "logout", "Antigravity sign-out failed. Try again."))?;
                let raw = serde_json::to_value(&initialized.initialize).unwrap_or_default();
                if raw["agentCapabilities"]["auth"]["logout"].is_null() {
                    initialized.shutdown().await;
                    return Err(failure(&instance_id, "logout", "This Antigravity version does not support sign-out. Update the provider."));
                }
                let logged_out = initialized.client.logout_typed(serde_json::from_value(json!({})).unwrap()).await;
                initialized.shutdown().await;
                drop(scratch);
                logged_out.map_err(|_| failure(&instance_id, "logout", "Antigravity sign-out failed. Try again."))?;
                Ok::<_, ProviderSetupError>(())
            })
            .await
            .unwrap_or_else(|_| Err(failure(&instance_id, "logout", "Antigravity sign-out timed out.")));
            backend.close_peer().await;
            result?;
            backend.registry.antigravity_update(&backend.instance.instance_id, |_, snapshot, now| crate::antigravity_instance::account_cleared(snapshot, now));
            Ok(Some("Signed out of Google.".into()))
        })
    }
}
