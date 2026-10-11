//! One-shot text generation through Antigravity (port of `textGeneration/AntigravityTextGeneration.ts`): a short-lived ACP
//! session of the instance's own Google login in an empty temporary directory, with no MCP servers and no client
//! tools. The prompt is wrapped in an instruction not to use tools; anything the agent does besides writing text (a tool
//! call, a permission or user-input request, a file written into the directory, output beyond the limit) fails the
//! request, and so does a model the account does not offer. The session's conversation files are removed afterwards.
//!
//! Not ported: the original registers the helper process with the sign-in flow (`withProcess`) so that a sign-out stops
//! it before credentials are cleared. This port's sessions are not registered either, so a sign-out leaves a running
//! helper to finish or time out.
use crate::{
    antigravity_acp as acp,
    antigravity_instance::{AntigravityInstance, Holds, initialize_request},
    text_generation::{Request, TextGenerationError, utf16_len},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_acp::{AcpError, RpcError};

const MAX_OUTPUT_CHARS: usize = 128_000;
const CONFIGURATION_LIMIT_BYTES: u64 = 64_000;

/// `isAntigravityTextGenerationAvailable`: global hooks and MCP servers can run before a helper could deny a tool
/// request, so a profile that configures any (or whose configuration cannot be read as empty) cannot write text.
pub fn available(profile: &Path) -> std::io::Result<bool> {
    for (name, key) in [("hooks.json", "hooks"), ("mcp_config.json", "mcpServers")] {
        let path = profile.join("config").join(name);
        if !path.try_exists()? {
            continue;
        }
        let info = std::fs::metadata(&path)?;
        if !info.is_file() || info.len() > CONFIGURATION_LIMIT_BYTES {
            return Ok(false);
        }
        // Anything unreadable, not a JSON object, or holding entries is not empty.
        let empty = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|configuration| {
                let configuration = configuration.as_object()?.clone();
                let section = match configuration.get(key) {
                    Some(section) if !section.is_null() => section.clone(),
                    _ => Value::Object(configuration),
                };
                Some(section.as_object()?.is_empty())
            })
            .unwrap_or(false);
        if !empty {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The first reason the helper must not go on, and a signal for whoever waits on it.
#[derive(Clone)]
struct Rejection(Arc<tokio::sync::watch::Sender<Option<String>>>);
impl Default for Rejection {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(None).0))
    }
}
impl Rejection {
    fn reject(&self, detail: &str) {
        self.0.send_if_modified(|slot| {
            let first = slot.is_none();
            if first {
                *slot = Some(detail.into());
            }
            first
        });
    }
    fn detail(&self) -> Option<String> {
        self.0.borrow().clone()
    }
    async fn raised(&self) -> String {
        let mut receiver = self.0.subscribe();
        let detail = receiver.wait_for(Option::is_some).await.ok().and_then(|detail| detail.clone());
        match detail {
            Some(detail) => detail,
            None => std::future::pending().await,
        }
    }
}

/// Removes the temporary working directory, and the conversation files the session made in the profile.
struct Workspace {
    path: PathBuf,
    profile: PathBuf,
    session: Mutex<Option<String>>,
}
impl Workspace {
    fn create(profile: PathBuf) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("t3-antigravity-text-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&path)?;
        Ok(Self { path, profile, session: Mutex::new(None) })
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(session) = self.session.lock().unwrap().take() {
            crate::antigravity_auth::remove_session_files(&self.profile, &session, &self.path);
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Runs one request. `timeout` bounds the whole helper (`ANTIGRAVITY_TIMEOUT_MS`).
pub async fn run(instance: &AntigravityInstance, request: &Request, timeout: Duration) -> Result<Value, TextGenerationError> {
    let operation = request.operation;
    let failed = |detail: &str| TextGenerationError::new(operation, detail);
    let generic = || failed("Antigravity text generation failed.");
    let (profile, _) = instance.directories().ok_or_else(generic)?;
    // Ignores `cwd`: the helper runs in an empty temporary directory, away from the project.
    let workspace = Workspace::create(profile.clone()).map_err(|_| generic())?;
    let holds = Holds::default();
    let rejection = Rejection::default();
    // The process outlives the timed future, so the files are removed only after it has ended.
    let peer_slot: Arc<Mutex<Option<(crate::acp_peer::ProcessPeer, crate::acp_client_callbacks::Services)>>> = Arc::default();
    let work = helper(instance, request, &workspace, &holds, &rejection, &peer_slot);
    let outcome = tokio::time::timeout(timeout, work).await;
    let started = peer_slot.lock().unwrap().take();
    if let Some((peer, services)) = started {
        services.shutdown().await;
        peer.shutdown().await;
    }
    let raw = match outcome {
        Err(_) => return Err(failed("Antigravity text generation timed out.")),
        Ok(result) => result?,
    };
    let written = std::fs::read_dir(&workspace.path).map(|mut entries| entries.next().is_some()).map_err(|_| generic())?;
    if written {
        return Err(failed("Antigravity wrote files during text generation."));
    }
    if raw.is_empty() {
        return Err(failed("Antigravity returned empty text generation output."));
    }
    serde_json::from_str::<Value>(&crate::text_generation_run::extract_json_object(&raw)).ok().and_then(|answer| request.shape.decode(&answer)).ok_or_else(|| failed("Antigravity returned invalid structured output."))
}

/// The session: the text the agent wrote, or why the request failed.
async fn helper(
    instance: &AntigravityInstance,
    request: &Request,
    workspace: &Workspace,
    holds: &Holds,
    rejection: &Rejection,
    peer_slot: &Arc<Mutex<Option<(crate::acp_peer::ProcessPeer, crate::acp_client_callbacks::Services)>>>,
) -> Result<String, TextGenerationError> {
    use crate::acp_runtime::{SessionEvent, StartHooks};
    let operation = request.operation;
    let failed = |detail: &str| TextGenerationError::new(operation, detail);
    let generic = || failed("Antigravity text generation failed.");
    let profile = &workspace.profile;
    if !available(profile).map_err(|_| generic())? {
        return Err(failed("Antigravity text generation is unavailable for profiles with global hooks or MCP configuration. Select another system model."));
    }
    let process = instance.spawn_options(&workspace.path, holds, &Default::default()).await.map_err(|_| generic())?;
    let peer = crate::acp_peer::ProcessPeer::spawn_with_options(process, crate::antigravity_flavor::refuse_sign_in_options()).map_err(|_| generic())?;
    let acp_instance = instance.acp_instance();
    let services = acp_instance.services_for(&workspace.path, crate::acp_client_callbacks::policy(&json!("approval-required"), &workspace.path));
    *peer_slot.lock().unwrap() = Some((peer.clone(), services.clone()));

    // Every client request that would be a tool or a question is refused, and ends the request.
    let on_reject = rejection.clone();
    let hooks = StartHooks {
        initialize: Some(initialize_request("t3-code-text", false)),
        register: Some(Arc::new(move |context| {
            let rejection = on_reject.clone();
            Box::pin(async move {
                let refuse: t3_acp::client::RequestHandler = Arc::new(move |_, _| {
                    let rejection = rejection.clone();
                    Box::pin(async move {
                        rejection.reject("Antigravity text generation requested a tool or user input.");
                        Err(RpcError { code: -32601, message: "Tools and user input are disabled for text generation.".into(), data: None })
                    })
                });
                for method in ["fs/read_text_file", "fs/write_text_file", "terminal/create", "terminal/output", "terminal/wait_for_exit", "terminal/kill", "terminal/release"] {
                    context.client.handle_request(method, refuse.clone());
                }
                context.client.handle_unknown_request(refuse);
            })
        })),
        authenticate: Some(instance.auth().auth_method),
        prefer_resume: true,
        additional_directories: vec![],
    };
    let mut session = Box::pin(acp_instance.start_peer_flavored(peer, &workspace.path, None, services, None, &hooks)).await.map_err(|cause| {
        tracing::warn!(detail = %cause, "Antigravity text generation could not start a session");
        generic()
    })?;
    let session_id = session.setup.session_id.clone();
    *workspace.session.lock().unwrap() = Some(session_id.clone());
    if !crate::antigravity_auth::is_native_session_id(&session_id) {
        return Err(failed("Antigravity returned an invalid text helper session ID."));
    }

    let mode = serde_json::Map::from_iter([("_t3/session-mode".to_string(), json!("default"))]);
    session.set_options(&mode).await.map_err(|_| generic())?;
    let selection_failed = || failed("Could not select the Antigravity model for text generation.");
    let configuration = serde_json::to_value(&session.setup).map_err(|_| generic())?;
    let selected = acp::resolve_model(&configuration["configOptions"], request.model_selection["model"].as_str(), Some(crate::antigravity_instance::MANIFEST_DEFAULT_MODEL)).map_err(|_| selection_failed())?;
    if let Some(model) = selected {
        session.set_model(&model).await.map_err(|_| selection_failed())?;
    }

    let prompt = [
        "Use only the input below. Do not use tools, read or write files, run commands, or ask questions.",
        "Return only the requested JSON object.",
        "",
        &request.prompt,
    ]
    .join("\n");
    let prompt_request = serde_json::from_value(json!({"sessionId":session_id,"prompt":[{"type":"text","text":prompt}]})).map_err(|_| generic())?;
    let client = session.client.clone();
    let mut prompt_task = tokio::spawn(async move { client.prompt_typed(prompt_request).await });

    let mut output = String::new();
    let mut output_units = 0usize;
    // One update or request of the session. Returns true when the request must end.
    let mut observe = |event: SessionEvent| -> bool {
        match event {
            SessionEvent::Update(update) => {
                let mut notification = serde_json::to_value(update).unwrap_or_default();
                acp::normalize_session_update(&mut notification);
                let kind = notification["update"]["sessionUpdate"].as_str().unwrap_or("");
                if matches!(kind, "tool_call" | "tool_call_update") {
                    rejection.reject("Antigravity attempted tool work during text generation.");
                    return true;
                }
                if notification["sessionId"].as_str() != Some(session_id.as_str()) || kind != "agent_message_chunk" || notification["update"]["content"]["type"] != "text" {
                    return false;
                }
                let text = notification["update"]["content"]["text"].as_str().unwrap_or("");
                let added = utf16_len(text);
                if output_units + added > MAX_OUTPUT_CHARS {
                    rejection.reject("Antigravity text generation exceeded the output limit.");
                    return true;
                }
                output_units += added;
                output.push_str(text);
                false
            }
            SessionEvent::Permission { response, .. } => {
                rejection.reject("Antigravity text generation requested a tool permission or user input.");
                let _ = response.send(serde_json::from_value(json!({"outcome":{"outcome":"cancelled"}})).map_err(|error| AcpError::Transport(error.to_string())));
                true
            }
            SessionEvent::Elicitation { response, .. } => {
                rejection.reject("Antigravity text generation requested user input.");
                let _ = response.send(serde_json::from_value(json!({"action":"decline"})).map_err(|error| AcpError::Transport(error.to_string())));
                true
            }
            SessionEvent::Terminated(_) | SessionEvent::Grok(_) => false,
        }
    };
    let stop_reason = loop {
        tokio::select! {
            detail = rejection.raised() => {
                prompt_task.abort();
                return Err(failed(&detail));
            }
            event = session.events.recv() => match event {
                Some(event) => {
                    if observe(event) {
                        prompt_task.abort();
                        return Err(failed(&rejection.detail().unwrap_or_default()));
                    }
                }
                None => {
                    prompt_task.abort();
                    return Err(generic());
                }
            },
            result = &mut prompt_task => {
                let response = result.map_err(|_| generic())?.map_err(|_| generic())?;
                break response.stop_reason;
            }
        }
    };
    // The prompt settles after the last update has been delivered; read what is still queued.
    while let Ok(event) = session.events.try_recv() {
        if observe(event) {
            return Err(failed(&rejection.detail().unwrap_or_default()));
        }
    }
    if let Some(detail) = rejection.detail() {
        return Err(failed(&detail));
    }
    if stop_reason == "cancelled" {
        return Err(failed("Antigravity text generation was cancelled."));
    }
    Ok(js_trim(&output).to_owned())
}

fn js_trim(text: &str) -> &str {
    crate::text_generation::js_trim(text)
}
