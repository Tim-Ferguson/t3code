//! Pi coding agent provider actor: owns one `pi --mode rpc` process per thread, starts turns from outbox
//! effects, turns its event stream into persisted events (via `pi_projection`), answers extension dialogs from
//! persisted responses, and stops on request. Mirrors `PiAdapterV2.ts`; structure follows `claude_adapter`.
//!
//! The session files are Pi's own (`~/.pi/agent/sessions`), and the session file path is the durable native
//! thread, so a thread started in T3 Code can be resumed from the Pi TUI and the other way around. The
//! process runs with no `--no-*` flags of T3's, so the user's extensions, skills, prompt templates, context
//! files, settings, custom models and auth load as they do in the TUI. `agent_settled` is the only terminal
//! signal: `agent_end` closes one low-level run, and compaction retries, auto-retries and queued continuations
//! may still follow it, so a turn stays open until Pi reports the session settled and is confirmed idle.
use crate::{
    codex_runtime::{ActorLifetime, Work, commit, fail_run_with, find, settle_queue, terminal_captured, terminal_with, Retain},
    execution::at,
    persistence::{Effect, Store, StoreError},
    pi_commands,
    pi_instance::{INHERIT_MODEL_SLUG, PiInstance},
    pi_launch::{self, LaunchOptions},
    pi_projection::{self as projection, Compaction, Projector, Retry, StreamKind, non_negative},
    pi_protocol::{Event, Launch, PiProcess, RpcError},
    provider_registry::ProviderRegistry,
    provider_turn_context::TurnContext,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const DRIVER: &str = "pi";
/// `STREAM_FLUSH_MS`.
const STREAM_FLUSH: Duration = Duration::from_millis(50);
/// `PI_REQUEST_TIMEOUT_MS`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Session lifecycle hooks reload extensions, MCP servers and language servers.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);
const SKILL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const SETTLE_PROBE_MAX_ATTEMPTS: u64 = 3;
const SETTLE_PROBE_RETRY: Duration = Duration::from_millis(100);
const UNSOLICITED_ACTIVITY_ERROR: &str = "Pi started agent work outside an active T3 turn. The session was stopped to prevent invisible tool execution.";

fn thread_ref(id: &str) -> Value {
    projection::native_ref(id)
}
fn error(message: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidCommand(message.to_string())
}
fn rpc(error: RpcError) -> String {
    error.to_string()
}
fn view(store: &Store, thread_id: &str) -> Result<Value, StoreError> {
    crate::codex_runtime::projection(store, thread_id)
}
fn workspace(store: &Store, thread_id: &str) -> Result<std::path::PathBuf, String> {
    let view = view(store, thread_id).map_err(|e| e.to_string())?;
    let project = store.projection("project", view["thread"]["projectId"].as_str().unwrap_or("")).map_err(|e| e.to_string())?.ok_or("Project not found.")?;
    let cwd = view["thread"]["worktreePath"].as_str().or_else(|| project["workspaceRoot"].as_str()).ok_or("Workspace root missing.")?;
    let cwd = std::fs::canonicalize(cwd).map_err(|e| e.to_string())?;
    if !cwd.is_dir() {
        return Err("Workspace root is not a directory.".into());
    }
    Ok(cwd)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    TurnStart,
    Steer,
}
/// An extension dialog Pi is blocked on.
struct PendingPrompt {
    native_request: String,
    method: String,
    approval_key: String,
    prompt: projection::Prompt,
}
/// Where the turn sits in Pi's session tree (`PiTurnTreeRefs`): its first user entry, and the new head.
#[derive(Clone, Default)]
struct TreeRefs {
    turn_start_entry: Option<String>,
    leaf: Option<String>,
}
/// The live process and what this actor knows of the session it is on.
struct Live {
    process: Arc<PiProcess>,
    events: mpsc::UnboundedReceiver<Event>,
    /// Everything that makes a running process unable to serve the next turn: launch arguments, mode, credential.
    launch_key: String,
    skills: Arc<Mutex<Option<HashSet<String>>>>,
    /// `providerThreadLoadKey` of the thread the process hosts: the native session, selection and policy.
    loaded: Option<String>,
    registration_attempted: bool,
    /// The session file the process is on.
    native_thread: Option<String>,
    last_native: Option<String>,
    applied_model: Option<String>,
    applied_thinking: Option<String>,
    applied_name: Option<String>,
    /// Pi's own configured defaults, from the first `get_state`, so selecting "Pi default" again can restore them.
    baseline_model: Option<(String, String)>,
    baseline_thinking: Option<String>,
    context_window: Option<u64>,
    windows: HashMap<String, u64>,
    models_discovered: bool,
    /// The session tree's leaf as of the last turn boundary, so a turn's user entry is found relative to it.
    last_leaf: Option<String>,
    /// A `get_entries` capture failed, so the cursor no longer bounds a single turn.
    leaf_stale: bool,
    /// Prompt acknowledgements carry no id: their send order and owner, so a late one cannot touch the next turn.
    prompt_responses: VecDeque<(String, PromptKind)>,
    compact_responses: VecDeque<(String, PromptKind)>,
    /// Extension failures raised outside a turn are attached to the next one.
    early_errors: Vec<Value>,
    stop_requested: bool,
    unsolicited: bool,
}
struct Turn {
    projector: Projector,
    interrupted: bool,
    /// Whether any agent activity was seen: a command-only prompt never starts a run and never settles by itself.
    saw_agent_activity: bool,
    command_only: bool,
    /// Pi reports context as unknown right after compaction; its estimate keeps the meter.
    compaction_after_tokens: Option<u64>,
    live_used_tokens: Option<u64>,
    /// Invalidates idle snapshots when new work starts after a settle probe.
    probe_generation: u64,
    /// An extension may start compaction right after `agent_settled`.
    settle_when_idle: bool,
    saw_compaction: bool,
    /// RPC compact is in flight; Pi's abort does not cancel it.
    manual_compact: bool,
    compaction: Option<Compaction>,
    retry: Option<Retry>,
    failure: Option<Value>,
    /// The tree refs read just before Stop terminated Pi, when no read is possible later.
    stop_refs: Option<Option<TreeRefs>>,
}
/// What a record asks the actor to do beyond writing events.
enum Action {
    Finalize { read_usage: bool },
    Terminate,
    Probe { settle_after_activity: bool, attempt: u64, delay: bool },
    SessionEnded,
}
struct Actor {
    store: Store,
    thread_id: String,
    session_id: String,
    instance: PiInstance,
    cwd: std::path::PathBuf,
    session: Value,
    live: Option<Live>,
    turn: Option<Turn>,
    prompts: HashMap<String, PendingPrompt>,
    /// Confirmations the user accepted for the rest of the session, by title and message.
    session_approvals: HashSet<String>,
    mcp: Option<crate::mcp_sessions::ProviderSessionConfig>,
    detached: bool,
}

impl Actor {
    async fn connect(store: Store, providers: &ProviderRegistry, thread_id: &str, mcp: Option<&crate::provider_mcp::CredentialLease>) -> Result<Self, String> {
        let view = view(&store, thread_id).map_err(|e| e.to_string())?;
        let instance_id = view["thread"]["modelSelection"]["instanceId"].as_str().ok_or("Thread has no provider instance.")?;
        let instance = providers.pi(instance_id).map_err(|e| e.to_string())?;
        let cwd = workspace(&store, thread_id)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let capabilities: Value = serde_json::from_str(include_str!("pi-capabilities.json")).unwrap();
        let session = json!({"id":session_id,"driver":DRIVER,"providerInstanceId":instance_id,"status":"ready","cwd":cwd,"model":view["thread"]["modelSelection"]["model"],"capabilities":capabilities,"createdAt":now,"updatedAt":now,"lastError":null});
        commit(&store, thread_id, |_| Ok(vec![("provider-session.attached", session.clone())])).map_err(|e| e.to_string())?;
        Ok(Self { store, thread_id: thread_id.into(), session_id, instance, cwd, session, live: None, turn: None, prompts: HashMap::new(), session_approvals: HashSet::new(), mcp: mcp.map(|lease| lease.config.clone()), detached: false })
    }

    async fn effect(&mut self, effect: &Effect) -> Result<(), String> {
        match effect.request["type"].as_str() {
            Some("provider-turn.start") => self.start_turn(effect.request["runId"].as_str().ok_or("Run id missing.")?).await,
            Some("provider-turn.interrupt") => self.interrupt(effect.request["runId"].as_str()).await,
            Some("provider-turn.steer") => self.steer(effect).await,
            Some("runtime-request.respond") => self.respond(effect).await,
            Some("provider-session.detach") => self.detach(effect).await,
            Some("provider-turn.restart") => Err("Pi steers a running turn natively and does not restart it.".into()),
            Some("provider-thread.rollback") => self.rollback(effect).await,
            // Pi runs nothing between prompts: there is no background work for a Stop to end.
            Some("background-work.stop") => Ok(()),
            _ => Err("Unsupported Pi provider effect.".into()),
        }
    }

    fn live(&self) -> Result<&Live, String> {
        self.live.as_ref().ok_or_else(|| "The Pi process has ended.".to_owned())
    }
    fn live_mut(&mut self) -> Result<&mut Live, String> {
        self.live.as_mut().ok_or_else(|| "The Pi process has ended.".to_owned())
    }

    // ---- process and session ----

    /// The process serving this thread: reused while what it was launched with still holds, else replaced.
    async fn ensure_process(&mut self, runtime_mode: &str) -> Result<(), String> {
        let launch_args = pi_launch::resolve_launch_args(self.instance.config.launch_args.as_str())?;
        let cache = self.store.state_dir().map(|dir| dir.join("caches")).unwrap_or_else(|| std::env::temp_dir().join("t3-pi-caches"));
        // The extension owns both the optional MCP bridge and Pi's permission hook, so it is written even for a
        // session without an MCP credential: Supervised never silently degrades to unrestricted tool execution.
        let extension = pi_launch::materialize_extension(&cache).map_err(|cause| format!("Could not write the Pi extension: {cause}"))?;
        let extension = extension.to_string_lossy().into_owned();
        let mcp = self.mcp.as_ref().map(|mcp| (mcp.endpoint.as_str(), mcp.authorization_header.as_str()));
        let plan = pi_launch::build_rpc_launch(&LaunchOptions { launch_args: &launch_args, mcp, extension_path: Some(&extension), ephemeral: false, runtime_mode: Some(runtime_mode) });
        let launch_key = json!({"args":plan.args,"env":plan.env.iter().collect::<std::collections::BTreeMap<_, _>>(),"cwd":self.cwd,"binary":self.instance.binary_path()}).to_string();
        if self.live.as_ref().is_some_and(|live| live.launch_key == launch_key) {
            return Ok(());
        }
        self.end_process().await?;
        let mut env = self.instance.process_environment();
        env.extend(plan.env);
        let (process, events) = PiProcess::spawn(&Launch { binary: self.instance.binary_path(), args: plan.args, cwd: self.cwd.clone(), env, remove_env: plan.remove_env })?;
        let process = Arc::new(process);
        // Skill discovery can invoke extension code and so raise a blocking dialog: it runs beside session setup and
        // never holds it. A failed lookup is retried once at first use.
        let skills: Arc<Mutex<Option<HashSet<String>>>> = Default::default();
        tokio::spawn({
            let (process, skills) = (process.clone(), skills.clone());
            async move {
                if let Ok(data) = process.request(json!({"type":"get_commands"}), SKILL_DISCOVERY_TIMEOUT).await {
                    *skills.lock().unwrap() = Some(skill_names(&data));
                }
            }
        });
        self.live = Some(Live {
            process, events, launch_key, skills, loaded: None, registration_attempted: false, native_thread: None, last_native: None, applied_model: None, applied_thinking: None, applied_name: None, baseline_model: None, baseline_thinking: None, context_window: None, windows: HashMap::new(), models_discovered: false, last_leaf: None, leaf_stale: false,
            prompt_responses: VecDeque::new(), compact_responses: VecDeque::new(), early_errors: vec![], stop_requested: false, unsolicited: false,
        });
        Ok(())
    }
    /// Stops the process and forgets what depended on it.
    async fn end_process(&mut self) -> Result<(), String> {
        if let Some(live) = self.live.take() {
            live.process.terminate().await;
            self.cancel_prompts().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    fn cancel_prompts(&mut self) -> Result<(), StoreError> {
        let pending: Vec<PendingPrompt> = self.prompts.drain().map(|(_, prompt)| prompt).collect();
        if pending.is_empty() {
            return Ok(());
        }
        let now = at(Utc::now());
        if let Some(live) = &self.live {
            for prompt in &pending {
                let _ = live.process.send(&json!({"type":"extension_ui_response","id":prompt.native_request,"cancelled":true}));
            }
        }
        let events: Vec<_> = pending.iter().flat_map(|prompt| prompt.prompt.cancelled(&now)).collect();
        commit(&self.store, &self.thread_id, |_| Ok(events))
    }

    /// A request that does not wait on an event pump. Events the process writes meanwhile queue behind it.
    async fn ask(&self, record: Value, timeout: Duration) -> Result<Value, RpcError> {
        match &self.live {
            Some(live) => live.process.request(record, timeout).await,
            None => Err(RpcError::Closed("The Pi process has ended.".into())),
        }
    }
    /// A request made while the session is being set up: events keep flowing meanwhile, so a dialog an
    /// extension raises from a session hook is shown instead of deadlocking the setup.
    async fn call(&mut self, record: Value, timeout: Duration) -> Result<Value, RpcError> {
        let process = self.live.as_ref().ok_or_else(|| RpcError::Closed("The Pi process has ended.".into()))?.process.clone();
        let mut request = Box::pin(process.request(record, timeout));
        loop {
            let next = tokio::select! {
                result = &mut request => Ok(result),
                event = async { match self.live.as_mut() { Some(live) => live.events.recv().await, None => std::future::pending().await } } => Err(event),
            };
            match next {
                Ok(result) => return result,
                Err(event) => {
                    let actions = self.on_event(event).map_err(|cause| RpcError::Closed(cause.to_string()))?;
                    Box::pin(self.act(actions)).await.map_err(|cause| RpcError::Closed(cause.to_string()))?;
                }
            }
        }
    }
    /// A session lifecycle request. A local timeout does not cancel Pi's hook, so the process is retired before a
    /// fallback can race its eventual switch.
    async fn lifecycle(&mut self, record: Value) -> Result<Value, String> {
        let operation = record["type"].as_str().unwrap_or("").to_owned();
        match self.call(record, SESSION_TIMEOUT).await {
            Ok(data) => Ok(data),
            Err(cause) => {
                tracing::warn!(operation, error_tag = if cause.is_timeout() { "PiRpcTimeoutError" } else { "PiRpcError" }, "Pi session lifecycle request failed");
                if cause.is_timeout() {
                    if let Some(live) = &self.live {
                        live.process.terminate().await;
                    }
                }
                Err(rpc(cause))
            }
        }
    }

    /// `registerThread`: bind the process to the native session, starting it fresh or resuming the saved one, and
    /// learn Pi's defaults. Returns the session file, the durable native thread.
    async fn register(&mut self, resume: Option<&str>) -> Result<String, String> {
        let needs_new = {
            let live = self.live_mut()?;
            let needs_new = resume.is_none() && live.registration_attempted;
            live.registration_attempted = true;
            if resume.is_some() || needs_new {
                live.last_native = resume.map(str::to_owned).or_else(|| live.last_native.take());
                // Even a failed lifecycle operation can change Pi's native session: the old binding and model defaults must not stay usable.
                live.loaded = None;
                live.native_thread = None;
                live.applied_model = None;
                live.applied_thinking = None;
                live.applied_name = None;
                live.baseline_model = None;
                live.baseline_thinking = None;
                live.context_window = None;
            }
            needs_new
        };
        if resume.is_some() || needs_new {
            let result = match resume {
                Some(path) => self.lifecycle(json!({"type":"switch_session","sessionPath":path})).await?,
                None => self.lifecycle(json!({"type":"new_session"})).await?,
            };
            if result["cancelled"] == true {
                return Err("A Pi extension cancelled the session switch".into());
            }
        }
        let state = self.call(json!({"type":"get_state"}), REQUEST_TIMEOUT).await.map_err(rpc)?;
        if !self.live()?.models_discovered {
            let models = self.call(json!({"type":"get_available_models"}), REQUEST_TIMEOUT).await.ok();
            if let Some(models) = models.as_ref().and_then(|data| data["models"].as_array()) {
                let live = self.live_mut()?;
                for model in models {
                    remember_window(live, model);
                }
                live.models_discovered = true;
            }
        }
        let native = {
            let live = self.live_mut()?;
            live.context_window = remember_window(live, &state["model"]);
            // Each baseline is captured independently, and only while nothing has been applied yet, so a `get_state` that
            // arrives after our own selection cannot record that selection as Pi's default.
            if live.baseline_model.is_none() && live.applied_model.is_none() {
                if let (Some(provider), Some(id)) = (state["model"]["provider"].as_str(), state["model"]["id"].as_str()) {
                    live.baseline_model = Some((provider.to_owned(), id.to_owned()));
                }
            }
            if live.baseline_thinking.is_none() && live.applied_thinking.is_none() {
                live.baseline_thinking = state["thinkingLevel"].as_str().map(str::to_owned);
            }
            // switch_session takes a file path, not Pi's display session UUID.
            let native = state["sessionFile"].as_str().ok_or("get_state returned no persisted sessionFile")?.to_owned();
            if needs_new && live.last_native.as_deref() == Some(native.as_str()) {
                return Err("Pi did not create a distinct session file".into());
            }
            live.last_native = Some(native.clone());
            live.native_thread = Some(native.clone());
            native
        };
        // Baseline the leaf so the first turn's user entry can be located with a `since` cursor instead of a full scan.
        let entries = self.call(json!({"type":"get_entries"}), REQUEST_TIMEOUT).await.ok();
        let live = self.live_mut()?;
        live.last_leaf = entries.as_ref().and_then(|entries| entries["leafId"].as_str()).map(str::to_owned);
        // A successful baseline makes the cursor trustworthy again. An empty tree is a success with no leaf.
        live.leaf_stale = entries.is_none();
        Ok(native)
    }

    /// `applySelection`: the model and thinking level the run asks for.
    async fn apply_selection(&mut self, selection: &Value) -> Result<(), String> {
        let thinking = selection["options"].as_array().into_iter().flatten().find(|option| option["id"] == "thinking").and_then(|option| option["value"].as_str()).map(str::to_owned);
        let model = selection["model"].as_str().unwrap_or("").to_owned();
        if model == INHERIT_MODEL_SLUG {
            // Returning to "Pi default" after an explicit pick replays the captured baseline: Pi has no "unset" command.
            let (applied, baseline) = { let live = self.live()?; (live.applied_model.is_some(), live.baseline_model.clone()) };
            if let (true, Some((provider, id))) = (applied, baseline) {
                let restored = self.call(json!({"type":"set_model","provider":provider,"modelId":id}), REQUEST_TIMEOUT).await.map_err(rpc)?;
                let live = self.live_mut()?;
                live.context_window = remember_window(live, &restored);
                live.applied_model = None;
                self.set_session_model(INHERIT_MODEL_SLUG)?;
            }
            // The "Pi default" model advertises no thinking choices of its own, so an unqualified return also restores
            // the configured level instead of silently keeping the effort a previous pick applied.
            let (applied, baseline) = { let live = self.live()?; (live.applied_thinking.clone(), live.baseline_thinking.clone()) };
            if let (None, Some(applied), Some(baseline)) = (&thinking, applied, baseline) {
                if applied != baseline {
                    self.call(json!({"type":"set_thinking_level","level":baseline}), REQUEST_TIMEOUT).await.map_err(rpc)?;
                    self.live_mut()?.applied_thinking = None;
                }
            }
        } else if self.live()?.applied_model.as_deref() != Some(model.as_str()) {
            let (provider, id) = pi_commands::parse_model_slug(&model).ok_or_else(|| format!("Pi model '{model}' must use provider/model format"))?;
            let selected = self.call(json!({"type":"set_model","provider":provider,"modelId":id}), REQUEST_TIMEOUT).await.map_err(rpc)?;
            let live = self.live_mut()?;
            live.context_window = remember_window(live, &selected);
            live.applied_model = Some(model.clone());
            self.set_session_model(&model)?;
        }
        if let Some(thinking) = thinking.filter(|level| pi_commands::is_thinking_level(level) && self.live.as_ref().is_some_and(|live| live.applied_thinking.as_deref() != Some(level.as_str()))) {
            self.call(json!({"type":"set_thinking_level","level":thinking}), REQUEST_TIMEOUT).await.map_err(rpc)?;
            self.live_mut()?.applied_thinking = Some(thinking);
        }
        Ok(())
    }
    fn set_session_model(&mut self, model: &str) -> Result<(), String> {
        self.session["model"] = json!(model);
        self.session["updatedAt"] = json!(at(Utc::now()));
        let session = self.session.clone();
        commit(&self.store, &self.thread_id, |_| Ok(vec![("provider-session.updated", session)])).map_err(|e| e.to_string())
    }

    /// `resolvePromptPayload`: the text with `$skill` references hoisted to Pi's `/skill:name` position, the
    /// message's images as Pi image inputs, and a note for every other attachment.
    async fn prompt_payload(&mut self, message: &Value, inline: Option<&crate::claude_fork::Inline>) -> Result<(String, Vec<Value>), String> {
        use base64::Engine;
        let mut text = crate::attachment_prompt::provider_text(&self.store, message);
        if let Some(inline) = inline {
            text = crate::claude_fork::with_context(&inline.context, &text);
        }
        // Provider discovery and the live session are separate Pi processes. A failed session-local lookup is retried once at first use.
        if text.contains('$') && self.live()?.skills.lock().unwrap().is_none() {
            let found = self.ask(json!({"type":"get_commands"}), SKILL_DISCOVERY_TIMEOUT).await.map(|data| skill_names(&data)).unwrap_or_default();
            *self.live()?.skills.lock().unwrap() = Some(found);
        }
        let names = self.live()?.skills.lock().unwrap().clone();
        let expanded = match &names {
            Some(names) => pi_commands::expand_skill_reference(&text, names),
            None => text,
        };
        let (mut images, mut notes) = (vec![], vec![]);
        let attachments_dir = self.store.state_dir().map(|dir| dir.join("attachments"));
        for attachment in message["attachments"].as_array().into_iter().flatten() {
            let Some(path) = attachments_dir.as_deref().zip(crate::attachment_store::AttachmentRef::from_value(attachment)).and_then(|(dir, reference)| crate::attachment_store::resolve_attachment_path(dir, &reference)) else { continue };
            let mime = attachment["mimeType"].as_str().unwrap_or_default();
            if mime.starts_with("image/") {
                let bytes = tokio::fs::read(&path).await.map_err(|cause| format!("Failed to read attachment '{}': {cause}", attachment["id"].as_str().unwrap_or_default()))?;
                images.push(json!({"type":"image","data":base64::engine::general_purpose::STANDARD.encode(bytes),"mimeType":mime}));
            } else {
                notes.push(format!("[Attachment saved at {}]", path.display()));
            }
        }
        Ok((if notes.is_empty() { expanded } else { format!("{expanded}\n\n{}", notes.join("\n")) }, images))
    }

    // ---- turns ----

    async fn start_turn(&mut self, run_id: &str) -> Result<(), String> {
        if self.detached {
            self.reattach()?;
        }
        if self.turn.is_some() {
            return Err("A Pi turn is still active.".into());
        }
        // A process that ended while the thread was idle is learned of before it is reused.
        self.drain_events().await.map_err(|e| e.to_string())?;
        let view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let run = find(&view, "runs", &json!(run_id)).map_err(|e| e.to_string())?;
        if run["status"] != "starting" {
            return Err("Target run is no longer starting.".into());
        }
        if run["providerInstanceId"] != self.session["providerInstanceId"] {
            return Err("Native provider handoff is not yet available.".into());
        }
        let selection = run["modelSelection"].clone();
        let provider_thread = find(&view, "providerThreads", &run["providerThreadId"]).map_err(|e| e.to_string())?.clone();
        let saved = provider_thread["nativeThreadRef"]["nativeId"].as_str().map(str::to_owned);
        let message = find(&view, "messages", &run["userMessageId"]).map_err(|e| e.to_string())?.clone();
        let thread = &view["thread"];
        let runtime_mode = thread["runtimeMode"].as_str().unwrap_or("approval-required").to_owned();
        // What the run's handoffs say goes ahead of the user's message, within the budget.
        let inline = crate::claude_fork::plan_inline(&view, run, message["text"].as_str().unwrap_or(""), &message["attachments"])?;
        self.ensure_process(&runtime_mode).await?;
        // `providerThreadLoadKey`: the thread is registered with the process when it is not the one it hosts, or when the
        // selection or policy changed. A rollback moves the native session, so the next turn resumes it.
        let load_key = json!({"thread":saved,"selection":selection,"mode":runtime_mode,"cwd":self.cwd}).to_string();
        let native = if self.live()?.loaded.as_deref() == Some(load_key.as_str()) && saved.is_some() {
            saved.clone().unwrap()
        } else {
            let native = self.register(saved.as_deref()).await?;
            let key = json!({"thread":native,"selection":selection,"mode":runtime_mode,"cwd":self.cwd}).to_string();
            self.live_mut()?.loaded = Some(key);
            native
        };
        self.apply_selection(&selection).await?;
        // Mirror the thread title into Pi's session name so the session stays identifiable in Pi's own /resume listing.
        // Best effort: naming never blocks a turn.
        let title = thread["title"].as_str().unwrap_or("").to_owned();
        if self.live()?.applied_name.as_deref() != Some(title.as_str()) && self.call(json!({"type":"set_session_name","name":title}), REQUEST_TIMEOUT).await.is_ok() {
            self.live_mut()?.applied_name = Some(title);
        }
        let continuation = crate::provider_continuation::is_continuation(&message);
        if continuation {
            return Err("Pi has no background work to continue.".into());
        }
        let compact = pi_commands::parse_compact_command(message["text"].as_str().unwrap_or(""));
        // Resolved before the turn is installed: a failure here (an unreadable attachment) must not leave a turn set.
        let payload = match &compact {
            Some(_) => None,
            None => Some(self.prompt_payload(&message, inline.as_ref()).await?),
        };
        let provider_turn_id = uuid::Uuid::new_v4().to_string();
        let native_turn_id = format!("{}:attempt:{}", run["providerThreadId"].as_str().unwrap_or(""), run["activeAttemptId"].as_str().unwrap_or(""));
        let mut captured = TurnContext::capture(&view, run_id).map_err(|e| e.to_string())?;
        let now = at(Utc::now());
        let session_id = self.session_id.clone();
        self.session["status"] = json!("ready");
        self.session["lastError"] = Value::Null;
        self.session["updatedAt"] = json!(now);
        let session = self.session.clone();
        commit(&self.store, &self.thread_id, |view| {
            if !captured.is_current(view) {
                return Err(error("Native start belongs to a replaced attempt."));
            }
            let (mut run, attempt) = captured.rows(view)?;
            if run["status"] != "starting" {
                return Err(error("Run was interrupted before the Pi turn started."));
            }
            let mut attempt = attempt.clone();
            let mut node = find(view, "nodes", &captured.root_node_id)?.clone();
            let mut thread = find(view, "providerThreads", &captured.provider_thread_id)?.clone();
            run["status"] = json!("running");
            run["startedAt"] = json!(now);
            attempt["status"] = json!("running");
            attempt["startedAt"] = json!(now);
            attempt["providerTurnId"] = json!(provider_turn_id);
            node["status"] = json!("running");
            node["startedAt"] = json!(now);
            node["providerTurnId"] = json!(provider_turn_id);
            thread["driver"] = json!(DRIVER);
            thread["providerSessionId"] = json!(session_id);
            thread["nativeThreadRef"] = thread_ref(&native);
            thread["status"] = json!("active");
            thread["lastRunOrdinal"] = run["ordinal"].clone();
            thread["updatedAt"] = json!(now);
            let turn = json!({"id":provider_turn_id,"providerThreadId":run["providerThreadId"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":{"driver":DRIVER,"nativeId":native_turn_id,"strength":"weak"},"ordinal":run["ordinal"],"status":"running","startedAt":now,"completedAt":null});
            Ok(vec![("provider-session.updated", session.clone()), ("provider-thread.updated", thread), ("provider-turn.updated", turn), ("run.updated", run), ("run-attempt.updated", attempt), ("node.updated", node)])
        })
        .map_err(|e| e.to_string())?;
        captured.provider_turn_id = json!(provider_turn_id);
        let view = view_after(&self.store, &self.thread_id)?;
        let projector = Projector::new(&self.thread_id, captured, &view, &provider_turn_id).map_err(|e| e.to_string())?;
        let command_only = compact.is_some() || payload.as_ref().is_some_and(|(text, _)| text.trim_start().starts_with('/'));
        self.turn = Some(Turn {
            projector, interrupted: false, saw_agent_activity: false, command_only, compaction_after_tokens: None, live_used_tokens: None, probe_generation: 0, settle_when_idle: false, saw_compaction: false, manual_compact: compact.is_some(),
            compaction: None, retry: None, failure: None, stop_refs: None,
        });
        // Written after the running state is durable: Pi answers immediately.
        if let Some(inline) = &inline {
            inline.mark(&self.store, &self.thread_id, "pending")?;
        }
        let sent = match (&compact, &payload) {
            (Some(instructions), _) => {
                let record = compact_record(instructions.as_deref());
                self.live_mut()?.compact_responses.push_back((provider_turn_id.clone(), PromptKind::TurnStart));
                self.live()?.process.send(&record)
            }
            (None, Some((text, images))) => {
                let mut record = json!({"type":"prompt","message":text});
                if !images.is_empty() {
                    record["images"] = json!(images);
                }
                self.live_mut()?.prompt_responses.push_back((provider_turn_id.clone(), PromptKind::TurnStart));
                self.live()?.process.send(&record)
            }
            (None, None) => Ok(()),
        };
        if let Err(cause) = sent {
            self.turn = None;
            return Err(rpc(cause));
        }
        if let Some(inline) = &inline {
            inline.mark(&self.store, &self.thread_id, "inline")?;
        }
        // Extension failures raised before the turn are attached to it.
        let early: Vec<Value> = std::mem::take(&mut self.live_mut()?.early_errors);
        for event in early {
            self.extension_error(&event).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn steer(&mut self, effect: &Effect) -> Result<(), String> {
        let view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let message = find(&view, "messages", &effect.request["messageId"]).map_err(|e| e.to_string())?.clone();
        if !crate::provider_turn_context::steering_delivery_is_current(&view, &message) || effect.request["providerSessionId"] != self.session_id {
            return Ok(());
        }
        let turn = find(&view, "providerTurns", &effect.request["providerTurnId"]).map_err(|e| e.to_string())?;
        if turn["status"] == "completed" {
            return crate::execution::follow_up_completed_steer(&self.store, effect).map(|_| ()).map_err(|e| e.to_string());
        }
        let active = self.turn.as_ref().is_some_and(|turn| turn.projector.provider_turn == effect.request["providerTurnId"].as_str().unwrap_or_default());
        if !active || self.live.is_none() {
            return Err(format!("Pi turn {} is not active", effect.request["providerTurnId"].as_str().unwrap_or("")));
        }
        let provider_turn = effect.request["providerTurnId"].as_str().unwrap_or_default().to_owned();
        let compact = pi_commands::parse_compact_command(message["text"].as_str().unwrap_or(""));
        let payload = match &compact {
            Some(_) => None,
            None => Some(self.prompt_payload(&message, None).await?),
        };
        // `prompt` with a steering behavior is atomic on Pi's side: it queues during an active run and starts a new one if settlement won
        // the race. A direct `steer` sent after Pi became idle would stay queued forever. `/compact` is not a prompt: Pi's compact RPC aborts first.
        if self.turn.is_none() {
            return Err(format!("Pi turn {provider_turn} is not active"));
        }
        match (&compact, &payload) {
            (Some(instructions), _) => {
                self.turn.as_mut().unwrap().manual_compact = true;
                self.live()?.process.send(&compact_record(instructions.as_deref())).map_err(rpc)?;
                self.live_mut()?.compact_responses.push_back((provider_turn, PromptKind::Steer));
            }
            (None, Some((text, images))) => {
                let mut record = json!({"type":"prompt","message":text,"streamingBehavior":"steer"});
                if !images.is_empty() {
                    record["images"] = json!(images);
                }
                self.live()?.process.send(&record).map_err(rpc)?;
                self.live_mut()?.prompt_responses.push_back((provider_turn, PromptKind::Steer));
            }
            (None, None) => {}
        }
        self.turn.as_mut().unwrap().probe_generation += 1;
        Ok(())
    }

    /// Stop. Pi's abort does not cancel manual compaction and cannot be trusted to land on a wedged process, so a
    /// Stop retires the process after aborting; the next turn resumes the native session in a fresh one.
    async fn interrupt(&mut self, run_id: Option<&str>) -> Result<(), String> {
        let Some(turn) = self.turn.as_mut() else {
            // Pi runs nothing between prompts: nothing of a settled turn is left to stop.
            return Ok(());
        };
        if Some(turn.projector.ctx.run_id.as_str()) != run_id {
            return Err("Target run is no longer active.".into());
        }
        turn.interrupted = true;
        let settle_when_idle = turn.settle_when_idle;
        self.live_mut()?.stop_requested = true;
        if !settle_when_idle {
            let _ = self.ask(json!({"type":"abort"}), PROBE_TIMEOUT).await;
        }
        // Terminating fails every later request, so the stopped turn's tree refs are read first: rolling back past this
        // turn forks at its user entry.
        if self.turn.as_ref().is_some_and(|turn| turn.stop_refs.is_none()) {
            let refs = self.capture_tree_refs(PROBE_TIMEOUT).await;
            self.turn.as_mut().unwrap().stop_refs = Some(refs);
        }
        if let Some(live) = &self.live {
            live.process.terminate().await;
        }
        // Everything the process wrote before it exited is queued: settle it before the next effect can look at the dead process.
        self.drain_events().await.map_err(|e| e.to_string())
    }

    async fn detach(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id || self.detached {
            return Ok(());
        }
        if let Some(turn) = self.turn.as_mut() {
            turn.interrupted = true;
            if let Some(live) = self.live.as_mut() {
                live.stop_requested = true;
            }
            self.turn.as_mut().unwrap().stop_refs.get_or_insert(None);
            if let Some(live) = &self.live {
                live.process.terminate().await;
            }
            self.drain_events().await.map_err(|e| e.to_string())?;
            if self.turn.is_some() {
                self.finalize(false).await.map_err(|e| e.to_string())?;
            }
        }
        self.end_process().await?;
        self.detached = true;
        Ok(())
    }
    fn reattach(&mut self) -> Result<(), String> {
        let cwd = workspace(&self.store, &self.thread_id)?;
        let view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let mut session = self.session.clone();
        session["cwd"] = json!(cwd);
        session["status"] = json!("ready");
        session["model"] = view["thread"]["modelSelection"]["model"].clone();
        session["updatedAt"] = json!(at(Utc::now()));
        commit(&self.store, &self.thread_id, |_| Ok(vec![("provider-session.attached", session.clone())])).map_err(|e| e.to_string())?;
        self.session = session;
        self.cwd = cwd;
        self.detached = false;
        Ok(())
    }

    /// `runtime-request.respond`: the persisted decision or answers go back to the blocked extension. The run's
    /// command has already written the request, node and item as resolved.
    async fn respond(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id {
            return Err("Runtime callback belongs to a different provider session.".into());
        }
        let request_id = effect.request["requestId"].as_str().ok_or("Request id missing.")?;
        let pending = self.prompts.get(request_id).ok_or_else(|| format!("No pending Pi extension request {request_id}"))?;
        let mut response = projection::ui_response(&pending.method, &pending.native_request, effect.request["decision"].as_str(), &effect.request["answers"]);
        response["type"] = json!("extension_ui_response");
        response["id"] = json!(pending.native_request);
        self.live()?.process.send(&response).map_err(rpc)?;
        // Dropped only once Pi has the answer, so a failed send leaves the request retryable.
        let pending = self.prompts.remove(request_id).unwrap();
        if pending.method == "confirm" && effect.request["decision"] == "acceptForSession" {
            self.session_approvals.insert(pending.approval_key);
        }
        Ok(())
    }

    /// `rollbackThread`: Pi's `fork` re-roots the session before a user message, and replaces the session file,
    /// so the rolled-back thread continues in a new file.
    async fn rollback(&mut self, effect: &Effect) -> Result<(), String> {
        if self.turn.is_some() {
            return Err("Cannot roll back while a Pi turn is active".into());
        }
        self.drain_events().await.map_err(|e| e.to_string())?;
        let prepared = crate::checkpoint_rollback::prepare(&self.store, effect)?;
        let provider_thread = if prepared.needs_provider() {
            let mut thread = prepared.provider_thread.clone();
            // `fork(entryId)` re-roots the active branch before that user message: the first user entry of the earliest turn discarded.
            let Some(entry) = rollback_fork_entry(&prepared)? else {
                return crate::checkpoint_rollback::finish(&self.store, effect, prepared, thread).await;
            };
            let native = thread["nativeThreadRef"]["nativeId"].as_str().ok_or_else(|| format!("Provider thread {} is missing a native Pi session id.", thread["id"].as_str().unwrap_or("")))?.to_owned();
            let view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
            let mode = view["thread"]["runtimeMode"].as_str().unwrap_or("approval-required").to_owned();
            self.ensure_process(&mode).await?;
            if self.live()?.native_thread.as_deref() != Some(native.as_str()) {
                self.register(Some(&native)).await?;
            }
            let forked = self.lifecycle(json!({"type":"fork","entryId":entry})).await?;
            if forked["cancelled"] == true {
                return Err("A Pi extension cancelled the session fork".into());
            }
            // Pi's fork replaces the session file, including for rollback; its new identity is read before anything can fail or restart.
            let state = match self.ask(json!({"type":"get_state"}), REQUEST_TIMEOUT).await {
                Ok(state) => state,
                Err(cause) => {
                    self.live_mut()?.native_thread = None;
                    return Err(rpc(cause));
                }
            };
            let Some(file) = state["sessionFile"].as_str().map(str::to_owned) else {
                self.live_mut()?.native_thread = None;
                return Err("Pi fork did not return a persisted session file".into());
            };
            {
                let live = self.live_mut()?;
                live.last_native = Some(file.clone());
                live.native_thread = Some(file.clone());
                live.applied_model = None;
                live.applied_thinking = None;
                live.applied_name = None;
            }
            let entries = self.ask(json!({"type":"get_entries"}), REQUEST_TIMEOUT).await.ok();
            let leaf = entries.as_ref().and_then(|entries| entries["leafId"].as_str()).map(str::to_owned);
            let live = self.live_mut()?;
            live.last_leaf = leaf.clone();
            // The fork re-baselined the tree, so the cursor is trustworthy again unless this listing itself failed.
            live.leaf_stale = entries.is_none();
            thread["nativeThreadRef"] = thread_ref(&file);
            thread["nativeConversationHeadRef"] = leaf.as_deref().map_or(Value::Null, thread_ref);
            thread["status"] = json!("idle");
            thread["updatedAt"] = json!(at(Utc::now()));
            thread
        } else {
            prepared.provider_thread.clone()
        };
        crate::checkpoint_rollback::finish(&self.store, effect, prepared, provider_thread).await
    }

    // ---- the event stream ----

    /// One event from the process. What it changes is written here; what needs the process again is returned.
    fn on_event(&mut self, event: Option<Event>) -> Result<Vec<Action>, StoreError> {
        match event {
            Some(Event::Record(record)) => self.on_record(&record),
            Some(Event::Closed(exit)) => {
                // Transport death finalizes any live turn. A Stop retires the process on purpose and is not a failure.
                let mut actions = vec![];
                if let Some(turn) = self.turn.as_mut() {
                    turn.failure = if turn.interrupted { None } else { Some(crate::provider_failure::make(&json!({"cause":{"message":exit.describe()},"message":"Pi process exited unexpectedly.","class":"transport_error"}))) };
                    actions.push(Action::Finalize { read_usage: false });
                }
                actions.push(Action::SessionEnded);
                Ok(actions)
            }
            None => Ok(vec![Action::SessionEnded]),
        }
    }

    fn on_record(&mut self, record: &Value) -> Result<Vec<Action>, StoreError> {
        let kind = record["type"].as_str().unwrap_or_default();
        match kind {
            "agent_start" => {
                let Some(turn) = self.turn.as_mut() else {
                    // Pi extensions can trigger an agent run after the owning turn settled. With no first-class provider-initiated run, that
                    // runtime is stopped before it can run tools without a timeline owner.
                    if let Some(live) = self.live.as_mut() {
                        live.unsolicited = true;
                    }
                    self.update_session("error", Some(UNSOLICITED_ACTIVITY_ERROR))?;
                    return Ok(vec![Action::Terminate]);
                };
                turn.saw_agent_activity = true;
                turn.probe_generation += 1;
            }
            "message_start" => {
                if let Some(turn) = self.turn.as_mut().filter(|_| record["message"]["role"] == "assistant") {
                    turn.saw_agent_activity = true;
                    turn.projector.next_message();
                }
            }
            "message_update" => return self.message_update(record),
            "message_end" => {
                let Some(turn) = self.turn.as_mut() else { return Ok(vec![]) };
                let message = &record["message"];
                if message["role"] != "assistant" {
                    return Ok(vec![]);
                }
                let events = turn.projector.complete_open(&Value::Null);
                if message["stopReason"] == "error" && turn.failure.is_none() {
                    turn.failure = Some(crate::provider_failure::make(&json!({"message":message["errorMessage"].as_str().unwrap_or("Pi reported a model error."),"class":"provider_error"})));
                }
                self.commit_events(events)?;
            }
            "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                let phase = match kind {
                    "tool_execution_start" => "start",
                    "tool_execution_update" => "update",
                    _ => "end",
                };
                let Some(turn) = self.turn.as_mut() else { return Ok(vec![]) };
                if phase == "start" {
                    turn.saw_agent_activity = true;
                }
                let known = record["toolCallId"].as_str().is_some_and(|id| turn.projector.knows(id));
                let interrupted = turn.interrupted;
                let view = if known { Value::Null } else { view(&self.store, &self.thread_id)? };
                let events = self.turn.as_mut().unwrap().projector.tool(&view, record, phase, interrupted);
                self.commit_events(events)?;
            }
            "compaction_start" => {
                let Some(turn) = self.turn.as_mut() else { return Ok(vec![]) };
                turn.probe_generation += 1;
                turn.saw_compaction = true;
                let view = view(&self.store, &self.thread_id)?;
                let mut events = vec![];
                if let Some(previous) = turn.compaction.take() {
                    events.extend(turn.projector.compaction(&view, &previous, "cancelled", None, None, None));
                }
                let compaction = turn.projector.new_compaction();
                events.extend(turn.projector.compaction(&view, &compaction, "running", None, None, None));
                turn.compaction = Some(compaction);
                self.commit_events(events)?;
            }
            "compaction_end" => return self.compaction_end(record),
            "auto_retry_start" => return self.auto_retry_start(record),
            "auto_retry_end" => return self.auto_retry_end(record),
            "extension_ui_request" => return self.extension_ui_request(record),
            "extension_error" => {
                if self.turn.is_none() {
                    if let Some(live) = self.live.as_mut() {
                        live.early_errors.push(record.clone());
                    }
                } else {
                    self.extension_error(record)?;
                }
            }
            "agent_settled" => {
                let Some(turn) = self.turn.as_mut() else { return Ok(vec![]) };
                if turn.interrupted {
                    return Ok(vec![Action::Finalize { read_usage: true }]);
                }
                turn.settle_when_idle = true;
                turn.probe_generation += 1;
                return Ok(vec![Action::Probe { settle_after_activity: true, attempt: 1, delay: false }]);
            }
            "response" => return self.response(record),
            "t3.settle_probe" => return self.settle_probe(record),
            _ => {}
        }
        Ok(vec![])
    }

    fn message_update(&mut self, record: &Value) -> Result<Vec<Action>, StoreError> {
        let Some(turn) = self.turn.as_mut() else { return Ok(vec![]) };
        turn.saw_agent_activity = true;
        let _ = turn;
        // Pi attaches the current message's cumulative usage to every streaming update (0.84.2+). It is written only when
        // the total changes, so the meter moves live without a burst of no-op updates.
        let usage = &record["usage"];
        let window = self.live.as_ref().and_then(|live| live.context_window).filter(|window| *window > 0);
        let turn = self.turn.as_mut().unwrap();
        let mut events = vec![];
        if let (Some(used), Some(window)) = (non_negative(usage, "totalTokens").filter(|used| *used > 0 && Some(*used) != turn.live_used_tokens), window) {
            turn.live_used_tokens = Some(used);
            let mut token_usage = json!({"usedTokens":used,"maxTokens":window,"updatedAt":at(Utc::now())});
            for (key, field) in [("inputTokens", "input"), ("cachedInputTokens", "cacheRead"), ("outputTokens", "output")] {
                if let Some(value) = non_negative(usage, field) {
                    token_usage[key] = json!(value);
                }
            }
            let view = view(&self.store, &self.thread_id)?;
            let mut row = find(&view, "providerTurns", &json!(self.turn.as_ref().unwrap().projector.provider_turn))?.clone();
            row["tokenUsage"] = token_usage;
            events.push(("provider-turn.updated", row));
        }
        let turn = self.turn.as_mut().unwrap();
        let delta = &record["assistantMessageEvent"];
        let index = delta["contentIndex"].as_f64().map_or(0, |index| index as u64);
        let (stream_kind, ending) = match delta["type"].as_str() {
            Some("text_delta") => (Some(StreamKind::Text), false),
            Some("thinking_delta") => (Some(StreamKind::Reasoning), false),
            Some("text_end") => (Some(StreamKind::Text), true),
            Some("thinking_end") => (Some(StreamKind::Reasoning), true),
            _ => (None, false),
        };
        if let Some(kind) = stream_kind {
            let known = turn.projector.has_stream(kind, index);
            let view = if known { Value::Null } else { view(&self.store, &self.thread_id)? };
            let turn = self.turn.as_mut().unwrap();
            match delta["type"].as_str() {
                Some("text_delta" | "thinking_delta") => turn.projector.delta(&view, kind, index, delta["delta"].as_str().unwrap_or("")),
                _ => {
                    let _ = ending;
                    let final_text = delta["content"].as_str().or_else(|| delta["thinking"].as_str());
                    events.extend(turn.projector.end(&view, kind, index, final_text));
                }
            }
        }
        self.commit_events(events)?;
        Ok(vec![])
    }

    fn compaction_end(&mut self, record: &Value) -> Result<Vec<Action>, StoreError> {
        if self.turn.is_none() {
            return Ok(vec![]);
        }
        let view = view(&self.store, &self.thread_id)?;
        let turn = self.turn.as_mut().unwrap();
        let compaction = turn.compaction.take().unwrap_or_else(|| turn.projector.new_compaction());
        let result = &record["result"];
        let mut actions = vec![];
        let settle = |turn: &Turn| (turn.settle_when_idle || !turn.saw_agent_activity).then_some(Action::Probe { settle_after_activity: turn.settle_when_idle, attempt: 1, delay: false });
        let events = if result.is_null() {
            if record["aborted"] == true {
                let events = turn.projector.compaction(&view, &compaction, "cancelled", None, None, None);
                actions.extend(settle(turn));
                events
            } else {
                let message = record["errorMessage"].as_str().unwrap_or("Pi context compaction failed.");
                let events = turn.projector.compaction(&view, &compaction, "failed", Some(&message.chars().take(1000).collect::<String>()), None, None);
                actions.extend(settle(turn));
                events
            }
        } else {
            // An overflow can surface as a model error before Pi compacts and retries the turn. That failure is cleared only when
            // Pi confirms compaction will retry; a successful non-retrying compaction must not erase an exhausted provider retry.
            if record["willRetry"] == true {
                turn.failure = None;
            }
            turn.compaction_after_tokens = non_negative(result, "estimatedTokensAfter");
            let events = turn.projector.compaction(&view, &compaction, "completed", result["summary"].as_str(), non_negative(result, "tokensBefore"), non_negative(result, "estimatedTokensAfter"));
            actions.extend(settle(turn));
            events
        };
        self.commit_events(events)?;
        Ok(actions)
    }

    fn auto_retry_start(&mut self, record: &Value) -> Result<Vec<Action>, StoreError> {
        if self.turn.is_none() {
            return Ok(vec![]);
        }
        let view = view(&self.store, &self.thread_id)?;
        let turn = self.turn.as_mut().unwrap();
        let attempt = record["attempt"].as_f64().map_or(1, |attempt| attempt.trunc().max(1.0) as u64);
        let max_attempts = record["maxAttempts"].as_f64().map_or(attempt, |max| max.trunc() as u64).max(attempt);
        let delay = record["delayMs"].as_f64().map_or(0, |delay| delay.trunc().max(0.0) as u64);
        let failure = crate::provider_failure::make(&json!({"message":record["errorMessage"].as_str().unwrap_or("Pi provider request failed."),"class":"provider_error","retryable":true}));
        let (started_at, ordinal) = match &turn.retry {
            Some(current) => (current.started_at.clone(), current.ordinal),
            None => (at(Utc::now()), turn.projector.failure_ordinal(&view)),
        };
        let retry = Retry { attempt, max_attempts, delay_ms: Some(delay), failure, started_at, ordinal };
        let events = turn.projector.retry_item(&retry, "running");
        turn.retry = Some(retry);
        self.commit_events(events)?;
        Ok(vec![])
    }
    fn auto_retry_end(&mut self, record: &Value) -> Result<Vec<Action>, StoreError> {
        if self.turn.is_none() {
            return Ok(vec![]);
        }
        let view = view(&self.store, &self.thread_id)?;
        let turn = self.turn.as_mut().unwrap();
        if record["success"] == true {
            // The retry recovered. Pi emits the erroring `message_end` before retrying, so leaving that failure in place would
            // terminalize a successful turn as failed.
            let mut events = vec![];
            if let Some(mut current) = turn.retry.take() {
                current.attempt = record["attempt"].as_f64().map_or(current.attempt, |attempt| attempt.trunc().max(1.0) as u64);
                events = turn.projector.retry_item(&current, "completed");
            }
            turn.failure = None;
            return self.commit_events(events).map(|_| vec![]);
        }
        let failure = crate::provider_failure::make(&json!({"message":record["finalError"].as_str().unwrap_or("Pi auto-retry failed."),"class":"provider_error","retryable":false}));
        let attempt = record["attempt"].as_f64().map_or(1, |attempt| attempt.trunc().max(1.0) as u64);
        let current = turn.retry.clone();
        let retry = Retry {
            attempt,
            max_attempts: current.as_ref().map_or(attempt, |current| current.max_attempts),
            delay_ms: current.as_ref().and_then(|current| current.delay_ms),
            failure: failure.clone(),
            started_at: current.as_ref().map_or_else(|| at(Utc::now()), |current| current.started_at.clone()),
            ordinal: current.as_ref().map_or_else(|| turn.projector.failure_ordinal(&view), |current| current.ordinal),
        };
        turn.failure = Some(failure);
        let events = turn.projector.retry_item(&retry, "failed");
        turn.retry = Some(retry);
        self.commit_events(events)?;
        Ok(vec![])
    }

    /// `handleExtensionUiRequest`. Dialog methods become runtime requests (`confirm` an approval, the others a question);
    /// answers travel back as `extension_ui_response`. `notify` becomes a completed activity item. Terminal decoration
    /// (status, widget, title, editor text) has no matching T3 surface and is ignored.
    fn extension_ui_request(&mut self, event: &Value) -> Result<Vec<Action>, StoreError> {
        let method = event["method"].as_str().unwrap_or_default();
        let native = event["id"].as_str();
        if method == "notify" {
            let message = event["message"].as_str().unwrap_or("");
            if self.turn.is_some() && !message.is_empty() {
                let view = view(&self.store, &self.thread_id)?;
                let events = self.turn.as_mut().unwrap().projector.notify(&view, message, event["notifyType"].as_str().unwrap_or("info"));
                self.commit_events(events)?;
            }
            return Ok(vec![]);
        }
        if !matches!(method, "select" | "confirm" | "input" | "editor") {
            tracing::debug!(method, "Ignoring pi extension UI update.");
            return Ok(vec![]);
        }
        let Some(native) = native else { return Ok(vec![]) };
        let title = event["title"].as_str().unwrap_or("");
        let approval_key = format!("{}:{title}{}", title.encode_utf16().count(), event["message"].as_str().unwrap_or(""));
        if method == "confirm" && self.session_approvals.contains(&approval_key) {
            if let Some(live) = &self.live {
                let _ = live.process.send(&json!({"type":"extension_ui_response","id":native,"confirmed":true}));
            }
            return Ok(vec![]);
        }
        let Some(turn) = self.turn.as_mut() else {
            // A dialog outside a turn has no run to be shown on: Pi is told it was cancelled rather than left blocked.
            tracing::warn!(method, "A Pi extension dialog arrived outside a turn and was cancelled.");
            if let Some(live) = &self.live {
                let _ = live.process.send(&json!({"type":"extension_ui_response","id":native,"cancelled":true}));
            }
            return Ok(vec![]);
        };
        let view = view(&self.store, &self.thread_id)?;
        let title = if title.is_empty() { method } else { title };
        let confirm = method == "confirm";
        let prompt = turn.projector.request(&view, native, !confirm, title, event["message"].as_str().unwrap_or(title), "command", if confirm { vec![] } else { vec![projection::question(native, method, title, event)] }, &self.session_id);
        let events = prompt.events.clone();
        self.prompts.insert(prompt.request_id.clone(), PendingPrompt { native_request: native.to_owned(), method: method.to_owned(), approval_key, prompt });
        self.commit_events(events)?;
        Ok(vec![])
    }
    /// `emitExtensionError`: a failed notice on the turn; the turn goes on.
    fn extension_error(&mut self, event: &Value) -> Result<(), StoreError> {
        let Some(turn) = self.turn.as_mut() else { return Ok(()) };
        let name = projection::extension_display_name(event["extensionPath"].as_str());
        let during = event["event"].as_str().map(|during| format!(" during {during}")).unwrap_or_default();
        let detail = event["error"].as_str().map(str::trim).filter(|detail| !detail.is_empty());
        let message = [Some(format!("{name} failed{during}.")), detail.map(|detail| detail.chars().take(2000).collect())].into_iter().flatten().collect::<Vec<String>>().join("\n\n");
        let failure = crate::provider_failure::make(&json!({"message":message,"class":"provider_error","retryable":false}));
        let view = view(&self.store, &self.thread_id)?;
        let events = turn.projector.extension_error(&view, &name, failure);
        self.commit_events(events)
    }

    /// The deferred, id-less answer to a fire-and-forget `prompt`, `steer` or `compact`.
    fn response(&mut self, record: &Value) -> Result<Vec<Action>, StoreError> {
        let command = record["command"].as_str().unwrap_or_default();
        let success = record["success"].as_bool();
        let turn_id = self.turn.as_ref().map(|turn| turn.projector.provider_turn.clone());
        if command == "compact" {
            let pending = self.live.as_mut().and_then(|live| live.compact_responses.pop_front());
            let owned = pending.as_ref().is_some_and(|(owner, _)| Some(owner) == turn_id.as_ref());
            if owned {
                self.turn.as_mut().unwrap().manual_compact = false;
            }
            let kind = pending.as_ref().map(|(_, kind)| *kind);
            let Some(turn) = self.turn.as_mut().filter(|_| owned) else { return Ok(vec![]) };
            if success == Some(true) {
                if kind == Some(PromptKind::TurnStart) && turn.command_only && !turn.saw_agent_activity {
                    return Ok(vec![Action::Probe { settle_after_activity: false, attempt: 1, delay: false }]);
                }
                return Ok(vec![]);
            }
            if success != Some(false) || turn.compaction.is_some() {
                return Ok(vec![]);
            }
            if kind == Some(PromptKind::Steer) {
                tracing::warn!(error_length = record["error"].as_str().map(str::len), "Pi rejected a compact steer.");
                return Ok(vec![]);
            }
            if !turn.saw_compaction {
                turn.failure = Some(crate::provider_failure::make(&json!({"message":record["error"].as_str().unwrap_or("Pi compact failed."),"class":"provider_error"})));
                return Ok(vec![Action::Finalize { read_usage: true }]);
            }
            if !turn.saw_agent_activity {
                return Ok(vec![Action::Probe { settle_after_activity: false, attempt: 1, delay: false }]);
            }
            return Ok(vec![]);
        }
        let pending = if command == "prompt" { self.live.as_mut().and_then(|live| live.prompt_responses.pop_front()) } else { None };
        let owned = pending.as_ref().is_some_and(|(owner, _)| Some(owner) == turn_id.as_ref());
        let kind = pending.as_ref().map(|(_, kind)| *kind);
        if success == Some(true) {
            // Command-only prompts (pure extension slash commands) never start an agent run and never emit `agent_settled`, so
            // idleness is probed. The probe result is queued behind any events Pi wrote before answering, which keeps the check stream-ordered.
            if kind == Some(PromptKind::TurnStart) && owned {
                if let Some(turn) = self.turn.as_ref().filter(|turn| turn.command_only && !turn.saw_agent_activity) {
                    let _ = turn;
                    return Ok(vec![Action::Probe { settle_after_activity: false, attempt: 1, delay: false }]);
                }
            }
            return Ok(vec![]);
        }
        if success != Some(false) {
            return Ok(vec![]);
        }
        if command == "steer" || kind == Some(PromptKind::Steer) {
            // A rejected steer means that one message was refused. The turn it was aimed at still runs on Pi, so ending it here
            // would report a failure while output keeps streaming.
            tracing::warn!(error_length = record["error"].as_str().map(str::len), "Pi rejected a steer message.");
            return Ok(vec![]);
        }
        let failed = (command == "prompt" && kind == Some(PromptKind::TurnStart) && owned) || command == "parse";
        if let (true, Some(turn)) = (failed, self.turn.as_mut()) {
            turn.failure = Some(crate::provider_failure::make(&json!({"message":record["error"].as_str().unwrap_or("Pi rejected the prompt."),"class":"provider_error"})));
            return Ok(vec![Action::Finalize { read_usage: true }]);
        }
        Ok(vec![])
    }

    /// An idle check made after `agent_settled`, or after a command-only prompt was acknowledged.
    fn settle_probe(&mut self, record: &Value) -> Result<Vec<Action>, StoreError> {
        let settle_after_activity = record["settleAfterAgentActivity"] == true;
        let attempt = record["attempt"].as_f64().map_or(1, |attempt| attempt.trunc().max(1.0) as u64);
        let Some(turn) = self.turn.as_mut() else { return Ok(vec![]) };
        // New work increments the generation before this can be consumed, so only a current snapshot may settle.
        if turn.projector.provider_turn != record["providerTurnId"].as_str().unwrap_or_default() || Some(turn.probe_generation) != record["settleProbeGeneration"].as_u64() || (!settle_after_activity && turn.saw_agent_activity) || turn.compaction.is_some() {
            return Ok(vec![]);
        }
        if record["probeFailed"] == true {
            if !settle_after_activity {
                return Ok(vec![Action::Finalize { read_usage: true }]);
            }
            if attempt < SETTLE_PROBE_MAX_ATTEMPTS {
                return Ok(vec![Action::Probe { settle_after_activity: true, attempt: attempt + 1, delay: true }]);
            }
            // The session could not be confirmed idle: the process is retired and the turn ends with it.
            if let Some(live) = self.live.as_mut() {
                live.stop_requested = true;
            }
            return Ok(vec![Action::Terminate]);
        }
        let data = &record["data"];
        if data["isStreaming"] != true && data["isCompacting"] != true && non_negative(data, "pendingMessageCount").unwrap_or(0) == 0 {
            turn.settle_when_idle = false;
            return Ok(vec![Action::Finalize { read_usage: true }]);
        }
        Ok(vec![])
    }

    async fn act(&mut self, actions: Vec<Action>) -> Result<(), StoreError> {
        for action in actions {
            match action {
                Action::Finalize { read_usage } => Box::pin(self.finalize(read_usage)).await?,
                Action::Terminate => {
                    if let Some(live) = &self.live {
                        live.process.terminate().await;
                    }
                }
                Action::Probe { settle_after_activity, attempt, delay } => {
                    let (Some(live), Some(turn)) = (&self.live, &self.turn) else { continue };
                    tokio::spawn(probe(live.process.clone(), turn.projector.provider_turn.clone(), turn.probe_generation, settle_after_activity, attempt, delay));
                }
                Action::SessionEnded => {
                    let Some(live) = self.live.take() else { continue };
                    self.cancel_prompts()?;
                    if live.unsolicited {
                        self.update_session("error", Some(UNSOLICITED_ACTIVITY_ERROR))?;
                    } else if live.stop_requested {
                        self.update_session("stopped", None)?;
                    } else {
                        let interrupted = self.turn.as_ref().is_some_and(|turn| turn.interrupted);
                        self.update_session("error", Some(if interrupted { "Pi process was stopped." } else { "Pi process exited unexpectedly." }))?;
                    }
                }
            }
        }
        Ok(())
    }
    /// `captureTurnTreeRefs`: this turn's first user entry and the new leaf of Pi's session tree. The user-entry id becomes the provider
    /// turn's native ref (the point `fork` rolls back to); the leaf becomes the conversation head. Bookkeeping only: failures degrade
    /// to the synthetic refs.
    async fn capture_tree_refs(&mut self, timeout: Duration) -> Option<TreeRefs> {
        let (stale, cursor) = self.live.as_ref().map(|live| (live.leaf_stale, live.last_leaf.clone()))?;
        let cursor = if stale { None } else { cursor };
        let mut request = json!({"type":"get_entries"});
        if let Some(cursor) = &cursor {
            request["since"] = json!(cursor);
        }
        let Ok(data) = self.ask(request, timeout).await else {
            // Pi may have advanced past the cursor while this failed, so it can no longer be trusted to bound a single turn.
            if let Some(live) = self.live.as_mut() {
                live.leaf_stale = true;
            }
            return None;
        };
        let leaf = data["leafId"].as_str().map(str::to_owned);
        let live = self.live.as_mut()?;
        if let Some(leaf) = &leaf {
            live.last_leaf = Some(leaf.clone());
        }
        live.leaf_stale = false;
        // Without a trustworthy cursor this window spans more than one turn, so its first user entry belongs to an earlier turn:
        // the cursor is re-synced and the turn-start ref skipped rather than pointing rollback too far back.
        let first_user_entry = if stale {
            None
        } else {
            data["entries"].as_array().into_iter().flatten().find(|entry| entry["type"] == "message" && entry["message"]["role"] == "user").and_then(|entry| entry["id"].as_str()).map(str::to_owned)
        };
        Some(TreeRefs { turn_start_entry: first_user_entry, leaf })
    }

    /// `finalizeTurn`: the turn ends. Open items complete, the session tree and context usage are read, and the run settles.
    async fn finalize(&mut self, read_usage: bool) -> Result<(), StoreError> {
        let Some(mut turn) = self.turn.take() else { return Ok(()) };
        let view = view(&self.store, &self.thread_id)?;
        let completed_at = at(Utc::now());
        let mut events = turn.projector.complete_open(&view);
        if let Some(compaction) = turn.compaction.take() {
            let status = if turn.interrupted { "cancelled" } else if turn.failure.is_none() { "completed" } else { "failed" };
            events.extend(turn.projector.compaction(&view, &compaction, status, None, None, None));
        }
        if let Some(retry) = turn.retry.clone() {
            if turn.interrupted {
                events.extend(turn.projector.retry_item(&retry, "interrupted"));
                turn.retry = None;
            } else if turn.failure.is_none() {
                events.extend(turn.projector.retry_item(&retry, "completed"));
                turn.retry = None;
            }
        }
        // A failure that ends a retried turn is that retry's own item, so the turn carries one error and not two.
        let failed_retry = turn.retry.clone().filter(|_| !turn.interrupted && turn.failure.is_some());
        if let (Some(retry), Some(failure)) = (&failed_retry, &turn.failure) {
            events.extend(turn.projector.retry_item(&Retry { failure: failure.clone(), ..retry.clone() }, "failed"));
        }
        for (_, prompt) in self.prompts.drain() {
            if let Some(live) = &self.live {
                let _ = live.process.send(&json!({"type":"extension_ui_response","id":prompt.native_request,"cancelled":true}));
            }
            events.extend(prompt.prompt.cancelled(&completed_at));
        }
        let refs = match turn.stop_refs.take() {
            Some(refs) => refs,
            None => self.capture_tree_refs(REQUEST_TIMEOUT).await,
        };
        let token_usage = if read_usage { self.read_token_usage(turn.compaction_after_tokens).await } else { None };
        let failure = if turn.interrupted { None } else { turn.failure.clone() };
        let ctx = turn.projector.ctx.clone();
        let provider_turn = turn.projector.provider_turn.clone();
        let session_id = self.session_id.clone();
        let (store, thread_id) = (self.store.clone(), self.thread_id.clone());
        // The process may have ended before the turn did: the session status then belongs to `SessionEnded`.
        let alive = self.live.is_some();
        let mut session = self.session.clone();
        if alive {
            session["status"] = json!(if failure.is_some() { "error" } else { "ready" });
            session["lastError"] = failure.as_ref().map_or(Value::Null, |failure| failure["message"].clone());
            session["updatedAt"] = json!(completed_at);
            self.session = session.clone();
        }
        commit(&store, &thread_id, |view| {
            let mut out = events;
            if let Ok(row) = find(view, "providerTurns", &json!(provider_turn)) {
                let mut row = row.clone();
                if let Some(entry) = refs.as_ref().and_then(|refs| refs.turn_start_entry.as_ref()) {
                    row["nativeTurnRef"] = thread_ref(entry);
                }
                if let Some(usage) = &token_usage {
                    row["tokenUsage"] = usage.clone();
                }
                out.push(("provider-turn.updated", row));
            }
            if let (Ok(row), Some(leaf)) = (find(view, "providerThreads", &ctx.provider_thread_id), refs.as_ref().and_then(|refs| refs.leaf.as_ref())) {
                let mut row = row.clone();
                row["nativeConversationHeadRef"] = thread_ref(leaf);
                row["providerSessionId"] = json!(session_id);
                out.push(("provider-thread.updated", row));
            }
            if alive {
                out.push(("provider-session.updated", session));
            }
            Ok(out)
        })?;
        match failure {
            Some(_) if failed_retry.is_some() => {
                terminal_with(&store, &thread_id, Some(&ctx.run_id), "failed", None, Some(&ctx), Retain::Nothing)?;
                settle_queue(&store, &thread_id, Some(&ctx.run_id))?;
            }
            Some(failure) => {
                fail_run_with(&store, &thread_id, Some(&ctx.run_id), Some(&ctx), &failure, Retain::Nothing)?;
            }
            None => {
                terminal_with(&store, &thread_id, Some(&ctx.run_id), if turn.interrupted { "interrupted" } else { "completed" }, None, Some(&ctx), Retain::Nothing)?;
                settle_queue(&store, &thread_id, None)?;
            }
        }
        Ok(())
    }

    /// `readTokenUsage`: Pi reports context usage only through `get_session_stats`. It is secondary telemetry: the request is bounded and a
    /// Pi without stats leaves the turn without a report, which keeps the meter on the last turn that had one.
    async fn read_token_usage(&self, fallback_used: Option<u64>) -> Option<Value> {
        let stats = self.ask(json!({"type":"get_session_stats"}), PROBE_TIMEOUT).await.ok()?;
        let context = &stats["contextUsage"];
        let max = non_negative(context, "contextWindow")?;
        let used = non_negative(context, "tokens").or(fallback_used)?;
        if max == 0 {
            return None;
        }
        let mut usage = json!({"usedTokens":used,"maxTokens":max,"updatedAt":at(Utc::now())});
        for (key, field) in [("inputTokens", "input"), ("cachedInputTokens", "cacheRead"), ("outputTokens", "output")] {
            if let Some(value) = non_negative(&stats["tokens"], field) {
                usage[key] = json!(value);
            }
        }
        Some(usage)
    }

    fn commit_events(&self, events: projection::Events) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        commit(&self.store, &self.thread_id, |_| Ok(events))
    }
    fn update_session(&mut self, status: &str, last_error: Option<&str>) -> Result<(), StoreError> {
        self.session["status"] = json!(status);
        self.session["lastError"] = last_error.map_or(Value::Null, |error| json!(error));
        self.session["updatedAt"] = json!(at(Utc::now()));
        let session = self.session.clone();
        commit(&self.store, &self.thread_id, |_| Ok(vec![("provider-session.updated", session)]))
    }
    fn flush(&mut self) -> Result<(), StoreError> {
        let Some(turn) = self.turn.as_mut().filter(|turn| turn.projector.has_pending()) else { return Ok(()) };
        let events = turn.projector.flush(&Value::Null);
        self.commit_events(events)
    }
    /// Process every event already received from a process that has been shut down.
    async fn drain_events(&mut self) -> Result<(), StoreError> {
        while let Some(event) = self.live.as_mut().and_then(|live| live.events.try_recv().ok()) {
            let actions = self.on_event(Some(event))?;
            Box::pin(self.act(actions)).await?;
        }
        Ok(())
    }
}

/// `scheduleSettleProbe`: a `get_state` whose answer is queued behind the events Pi has already written, so the
/// check is stream-ordered. A failed probe still reaches the actor: dropping it would leave a command-only turn open forever.
async fn probe(process: Arc<PiProcess>, provider_turn: String, generation: u64, settle_after_activity: bool, attempt: u64, delay: bool) {
    if delay {
        tokio::time::sleep(SETTLE_PROBE_RETRY).await;
    }
    let mut record = json!({"type":"t3.settle_probe","providerTurnId":provider_turn,"settleAfterAgentActivity":settle_after_activity,"settleProbeGeneration":generation,"attempt":attempt});
    match process.request(json!({"type":"get_state"}), PROBE_TIMEOUT).await {
        Ok(data) => record["data"] = data,
        Err(_) => record["probeFailed"] = json!(true),
    }
    process.inject(record);
}
fn compact_record(instructions: Option<&str>) -> Value {
    match instructions {
        Some(instructions) => json!({"type":"compact","customInstructions":instructions}),
        None => json!({"type":"compact"}),
    }
}
fn skill_names(data: &Value) -> HashSet<String> {
    pi_commands::parse_discovered_commands(data).1.iter().filter_map(|skill| skill["name"].as_str().map(str::to_owned)).collect()
}
/// `rememberModelContextWindow`.
fn remember_window(live: &mut Live, model: &Value) -> Option<u64> {
    let (provider, id, capacity) = (model["provider"].as_str()?, model["id"].as_str()?, non_negative(model, "contextWindow").filter(|capacity| *capacity > 0)?);
    live.windows.insert(format!("{provider}/{id}"), capacity);
    Some(capacity)
}
fn view_after(store: &Store, thread_id: &str) -> Result<Value, String> {
    view(store, thread_id).map_err(|e| e.to_string())
}

/// `piRollbackForkEntry`: the session-tree entry `fork` re-roots at. `None` when no turns follow the target (the conversation is already
/// there); an error when the boundary turn has no captured entry (only the refs read at a turn boundary are strong).
fn rollback_fork_entry(prepared: &crate::checkpoint_rollback::Prepared) -> Result<Option<String>, String> {
    let boundary = if prepared.target["type"] == "thread_start" { 0 } else { prepared.target["providerTurn"]["ordinal"].as_u64().unwrap_or(0) };
    let mut discarded: Vec<&Value> = prepared.provider_thread_turns.iter().filter(|turn| turn["ordinal"].as_u64().unwrap_or(0) > boundary).collect();
    discarded.sort_by_key(|turn| turn["ordinal"].as_u64());
    let Some(first) = discarded.first() else { return Ok(None) };
    let reference = &first["nativeTurnRef"];
    if reference["strength"] != "strong" || reference["nativeId"].as_str().is_none() {
        return Err("Pi rollback target has no captured session-tree entry".into());
    }
    Ok(reference["nativeId"].as_str().map(str::to_owned))
}

pub(crate) async fn actor(
    store: Store,
    providers: ProviderRegistry,
    thread_id: String,
    mut work: mpsc::Receiver<Work>,
    mut stopped: watch::Receiver<bool>,
    mut canceled: watch::Receiver<Option<String>>,
    _lifetime: ActorLifetime,
    mcp: Option<crate::provider_mcp::CredentialLease>,
) {
    let mut runtime: Option<Actor> = None;
    let mut user_cancelled = false;
    let mut flush_due: Option<tokio::time::Instant> = None;
    loop {
        if *stopped.borrow() {
            break;
        }
        // The next process event is received only while a process is live.
        let next = async {
            match runtime.as_mut().and_then(|actor| actor.live.as_mut()) {
                Some(live) => live.events.recv().await,
                None => std::future::pending().await,
            }
        };
        let flush = async {
            match flush_due {
                Some(due) => tokio::time::sleep_until(due).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = stopped.changed() => break,
            incoming = work.recv() => {
                let Some(incoming) = incoming else { break };
                let starting = incoming.effect.request["type"] == "provider-turn.start";
                let run_id = incoming.effect.request["runId"].as_str();
                let result = tokio::select! {
                    result = async {
                        if starting {
                            if let Some(mcp) = mcp.as_ref() { mcp.touch(); }
                        }
                        if runtime.is_none() && matches!(incoming.effect.request["type"].as_str(), Some("background-work.stop")) {
                            return Ok(());
                        }
                        if runtime.is_none() {
                            runtime = Some(Actor::connect(store.clone(), &providers, &thread_id, mcp.as_ref()).await?);
                        }
                        Box::pin(runtime.as_mut().unwrap().effect(&incoming.effect)).await
                    } => result,
                    _ = stopped.changed() => break,
                    _ = async { loop { if canceled.borrow_and_update().as_deref() == run_id { break; } if canceled.changed().await.is_err() { std::future::pending::<()>().await; } } }, if starting => {
                        user_cancelled = true;
                        Err("The run was interrupted during provider startup.".to_owned())
                    }
                };
                let close = starting && result.is_err();
                if user_cancelled || close { work.close(); }
                let _ = incoming.complete.send(result);
                if user_cancelled || close { break; }
                flush_due = runtime.as_ref().and_then(|actor| actor.turn.as_ref()).filter(|turn| turn.projector.has_pending()).map(|_| tokio::time::Instant::now() + STREAM_FLUSH);
            }
            _ = flush => {
                flush_due = None;
                if let Some(actor) = runtime.as_mut() {
                    if let Err(cause) = actor.flush() { tracing::error!(%cause, "Pi streamed text could not be persisted"); }
                }
            }
            event = next => {
                let actor = runtime.as_mut().unwrap();
                let outcome = async {
                    let actions = actor.on_event(event)?;
                    Box::pin(actor.act(actions)).await
                }.await;
                if let Err(cause) = outcome {
                    tracing::error!(%cause, "Pi provider event could not be persisted");
                    let ctx = actor.turn.as_ref().map(|turn| turn.projector.ctx.clone());
                    let failure = crate::provider_failure::make(&json!({"message":cause.to_string(),"class":"transport_error","retryable":false}));
                    let _ = fail_run_with(&actor.store, &actor.thread_id, ctx.as_ref().map(|c| c.run_id.as_str()), ctx.as_ref(), &failure, Retain::Nothing);
                    work.close();
                    break;
                }
                flush_due = actor.turn.as_ref().filter(|turn| turn.projector.has_pending()).map(|_| flush_due.unwrap_or_else(|| tokio::time::Instant::now() + STREAM_FLUSH));
            }
        }
    }
    work.close();
    if let Some(mut actor) = runtime {
        let ending = if *stopped.borrow() || user_cancelled { "interrupted" } else { "failed" };
        let ctx = actor.turn.as_ref().map(|turn| turn.projector.ctx.clone());
        let _ = terminal_captured(&actor.store, &actor.thread_id, ctx.as_ref().map(|c| c.run_id.as_str()), ending, None, ctx.as_ref());
        let session_id = actor.session_id.clone();
        let _ = commit(&actor.store, &actor.thread_id, |view| {
            let Ok(session) = find(view, "providerSessions", &json!(session_id)) else { return Ok(vec![]) };
            let mut session = session.clone();
            session["status"] = json!(if ending == "interrupted" { "stopped" } else { "error" });
            session["updatedAt"] = json!(at(Utc::now()));
            if ending == "failed" {
                session["lastError"] = json!("Provider process or event continuity ended.");
            }
            Ok(vec![("provider-session.updated", session)])
        });
        if let Some(live) = actor.live.take() {
            live.process.terminate().await;
        }
    }
    drop(mcp);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_skill_reference_needs_a_listed_skill() {
        let names = skill_names(&json!({"commands":[{"name":"skill:deploy","source":"skill"},{"name":"review"}]}));
        assert_eq!(names, HashSet::from(["deploy".to_owned()]));
        assert_eq!(projection::content_text(&json!([{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}])), "ab");
    }
}
