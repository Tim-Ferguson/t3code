//! Claude Code provider actor: owns one `claude` subprocess per thread, starts turns from outbox
//! effects, turns its stream into persisted events (via `claude_projection`), answers permission
//! and user-input control requests from persisted responses, and interrupts on request.
//! Structure follows `codex_runtime::codex_actor` and `acp_adapter::actor`.
use crate::{
    claude_instance::ClaudeInstance,
    claude_launch::{self, LaunchInput, Mcp},
    claude_models,
    claude_projection::{self as projection, Projector},
    claude_protocol::{ClaudeProcess, Event, PermissionResult, QueryOptions, user_message},
    claude_background::{self as background, Background},
    claude_subagents::{Parent, Subagents},
    codex_runtime::{ActorLifetime, Retain, Work, commit, fail_run_captured, fail_run_with, find, settle_queue, terminal_captured, terminal_with},
    execution::at,
    persistence::{Effect, Store, StoreError},
    provider_registry::ProviderRegistry,
    provider_turn_context::TurnContext,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use tokio::sync::{mpsc, watch};

const DRIVER: &str = "claudeAgent";
/// Matches the text coalescing interval of streamed reasoning in the source adapter.
const REASONING_FLUSH: Duration = Duration::from_millis(50);
const SUPPORTED_IMAGES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

fn error(message: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidCommand(message.to_string())
}
fn thread_ref(id: &str) -> Value {
    json!({"driver":DRIVER,"nativeId":id,"strength":"strong"})
}

/// A pending control request the CLI is blocked on.
enum Pending {
    Approval { control_id: String, tool_name: String, input: Value, suggestions: Option<Value>, tool_use_id: String },
    UserInput { control_id: String, input: Value, tool_use_id: String },
    /// The resume prompt dialog: answered with `compact`, `never` or `continue`.
    Dialog { control_id: String, question: String },
}
impl Pending {
    fn control_id(&self) -> &str {
        match self {
            Pending::Approval { control_id, .. } | Pending::UserInput { control_id, .. } | Pending::Dialog { control_id, .. } => control_id,
        }
    }
}
/// The live CLI process and what it was opened for.
/// Whether this CLI process echoes a prompt's uuid on the first frame of the turn answering it
/// ("early") or only on its result. Learned from the first prompt turn; "acknowledged": the CLI
/// confirmed it took a prompt's uuid before any echo, so it echoes, but a resume's own turns can
/// still run ahead of that prompt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EchoMode {
    Unknown,
    Acknowledged,
    Early,
    ResultOnly,
}
struct Live {
    echo_mode: EchoMode,
    process: ClaudeProcess,
    events: mpsc::UnboundedReceiver<Event>,
    key: String,
    native_thread_id: String,
    opened_mode: String,
    mode: String,
}
struct Turn {
    /// The uuid the prompt was offered with; Claude echoes it on the turn answering it. None for a continuation.
    prompt_uuid: Option<String>,
    echo_confirmed: bool,
    /// Claude acknowledged starting the prompt (`command_lifecycle` "started").
    prompt_started: bool,
    /// Root frames seen before the echo; held only when the CLI echoes early.
    gated_before_echo: u32,
    held: Vec<Value>,
    /// The run ingests output the CLI already produced instead of answering a prompt.
    continuation: bool,
    projector: Projector,
    interrupted: bool,
    steered: bool,
    mode_ask_user: bool,
}
struct Actor {
    store: Store,
    thread_id: String,
    session_id: String,
    instance: ClaudeInstance,
    cwd: std::path::PathBuf,
    session: Value,
    live: Option<Live>,
    turn: Option<Turn>,
    pending: HashMap<String, Pending>,
    /// Native sessions this actor created: a later open of the same id must resume it.
    opened: std::collections::HashSet<String>,
    mcp: Option<crate::mcp_sessions::ProviderSessionConfig>,
    detached: bool,
    providers: ProviderRegistry,
    /// The subagents the session has seen. They outlive the turn that launched them.
    subagents: Subagents,
    /// The projector of the turn that settled last: it still writes what a subagent reports while the
    /// root is idle, and it holds the positions of the thread's items.
    settled: Option<Projector>,
    /// Task ids of subagents launched by a subagent (a nested launch reports to its owner).
    nested: std::collections::HashSet<String>,
    /// Non-subagent tasks (`local_bash`) this turn has seen: not subagent rows.
    ignored_tasks: std::collections::HashSet<String>,
    /// The roster of background tasks, and what the CLI produced while no turn ran.
    bg: Background,
    /// The provider thread the latest turn ran on: where a wake is offered and the roster is written.
    route: Option<Value>,
    /// A fork's first launch, until Claude reports the session it forked into.
    fork_wait: Option<crate::claude_fork::ForkWait>,
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

impl Actor {
    async fn connect(store: Store, providers: &ProviderRegistry, thread_id: &str, mcp: Option<&crate::provider_mcp::CredentialLease>) -> Result<Self, String> {
        let view = view(&store, thread_id).map_err(|e| e.to_string())?;
        let instance_id = view["thread"]["modelSelection"]["instanceId"].as_str().ok_or("Thread has no provider instance.")?;
        let instance = providers.claude(instance_id).map_err(|e| e.to_string())?;
        let cwd = workspace(&store, thread_id)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let capabilities: Value = serde_json::from_str(include_str!("claude-capabilities.json")).unwrap();
        let session = json!({"id":session_id,"driver":DRIVER,"providerInstanceId":instance_id,"status":"ready","cwd":cwd,"model":view["thread"]["modelSelection"]["model"],"capabilities":capabilities,"createdAt":now,"updatedAt":now,"lastError":null});
        commit(&store, thread_id, |_| Ok(vec![("provider-session.attached", session.clone())])).map_err(|e| e.to_string())?;
        Ok(Self { store, thread_id: thread_id.into(), session_id, instance, cwd, session, live: None, turn: None, pending: HashMap::new(), opened: Default::default(), mcp: mcp.map(|lease| lease.config.clone()), detached: false, providers: providers.clone(), subagents: Default::default(), settled: None, nested: Default::default(), ignored_tasks: Default::default(), bg: Default::default(), route: None, fork_wait: None })
    }

    async fn effect(&mut self, effect: &Effect) -> Result<(), String> {
        match effect.request["type"].as_str() {
            Some("provider-turn.start") => self.start_turn(effect.request["runId"].as_str().ok_or("Run id missing.")?).await,
            Some("provider-turn.interrupt") => self.interrupt(effect.request["runId"].as_str()).await,
            Some("provider-turn.steer") => self.steer(effect).await,
            Some("runtime-request.respond") => self.respond(effect).await,
            Some("provider-session.detach") => self.detach(effect).await,
            Some("provider-turn.restart") => Err("Claude steers a running turn natively and does not restart it.".into()),
            Some("provider-thread.rollback") => self.rollback(effect).await,
            Some("background-work.stop") => self.stop_background_work().await,
            _ => Err("Unsupported Claude provider effect.".into()),
        }
    }

    /// Everything that makes a running CLI unable to serve the next turn.
    fn query_key(options: &QueryOptions, identity: &str) -> String {
        json!({"permissionMode":options.permission_mode,"allowed":options.allowed_tools,"skip":options.allow_dangerously_skip_permissions,"mcp":options.mcp_servers,"selection":identity,"settings":options.settings}).to_string()
    }

    async fn start_turn(&mut self, run_id: &str) -> Result<(), String> {
        if self.detached {
            self.reattach()?;
        }
        if self.turn.is_some() {
            return Err("A Claude turn is still active.".into());
        }
        let view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let run = find(&view, "runs", &json!(run_id)).map_err(|e| e.to_string())?;
        if run["status"] != "starting" {
            return Err("Target run is no longer starting.".into());
        }
        if run["providerInstanceId"] != self.session["providerInstanceId"] {
            return Err("Native provider handoff is not yet available.".into());
        }
        let selection = run["modelSelection"].clone();
        let provider_thread = find(&view, "providerThreads", &run["providerThreadId"]).map_err(|e| e.to_string())?;
        let saved = provider_thread["nativeThreadRef"]["nativeId"].as_str().map(str::to_owned);
        // A rolled-back run left the conversation, so a session minted after rolling back to the
        // start has no turns to resume.
        let rolled_back: std::collections::HashSet<&str> = view["attempts"].as_array().into_iter().flatten().filter(|attempt| view["runs"].as_array().into_iter().flatten().any(|run| run["id"] == attempt["runId"] && run["status"] == "rolled_back")).filter_map(|attempt| attempt["id"].as_str()).collect();
        let has_turns = view["providerTurns"].as_array().into_iter().flatten().any(|turn| turn["providerThreadId"] == provider_thread["id"] && turn["runAttemptId"].as_str().is_none_or(|attempt| !rolled_back.contains(attempt)));
        // After a rollback the thread names the assistant message the conversation now ends at.
        let resume_session_at = provider_thread["nativeConversationHeadRef"]["nativeId"].as_str().map(str::to_owned);
        // A fork's first launch resumes its source's session as a new one; Claude names it in its `init`.
        let fork_transfer = if saved.is_none() && !provider_thread["forkedFrom"].is_null() { crate::context_transfer::native_fork_transfer(&view, &json!(run_id)).cloned() } else { None };
        let fork_request = fork_transfer.as_ref().map(|transfer| crate::claude_fork::fork_request(&self.store, transfer, provider_thread)).transpose()?;
        let resume_session_at = match &fork_request {
            Some(request) => request.up_to_message.clone(),
            None => resume_session_at,
        };
        let native_id = match &fork_request {
            Some(request) => request.source_session.clone(),
            None => saved.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        };
        // An imported session exists natively before its first turn here; it loads as `not_loaded`.
        let imported = saved.is_some() && provider_thread["status"] == "not_loaded";
        let resume = saved.is_some() && has_turns || self.opened.contains(&native_id) || resume_session_at.is_some() || imported || fork_request.is_some();
        let message = find(&view, "messages", &run["userMessageId"]).map_err(|e| e.to_string())?.clone();
        let thread = &view["thread"];
        let runtime_mode = thread["runtimeMode"].as_str().unwrap_or("approval-required").to_owned();
        let interaction_mode = thread["interactionMode"].as_str().unwrap_or("default").to_owned();
        let compiled = claude_models::compile_selection(&selection);
        let attachments_dir = self.store.state_dir().map(|dir| dir.join("attachments"));
        let options = claude_launch::query_options(LaunchInput {
            binary_path: &self.instance.binary_path().to_string_lossy(),
            launch_args: self.instance.config.launch_args.as_str(),
            auto_compact_window: &serde_json::to_value(&self.instance.config.auto_compact_window).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default(),
            environment: self.instance.process_environment(),
            selection: &selection,
            native_thread_id: &native_id,
            resume,
            resume_session_at: resume_session_at.as_deref(),
            fork_session: fork_request.is_some(),
            cwd: &self.cwd,
            attachments_dir: attachments_dir.as_deref(),
            runtime_mode: &runtime_mode,
            interaction_mode: &interaction_mode,
            mcp: self.mcp.as_ref().map(|mcp| Mcp { endpoint: &mcp.endpoint, authorization_header: &mcp.authorization_header }),
        });
        let key = Self::query_key(&options, &compiled.query_identity);
        // A continuation attaches to what the CLI already produced instead of prompting it again.
        let continuation = crate::provider_continuation::is_continuation(&message);
        // Build the prompt first so a bad attachment fails before any process starts.
        // What the run's handoffs say goes ahead of the user's message, within the budget.
        let inline = if continuation { None } else { crate::claude_fork::plan_inline(&view, run, message["text"].as_str().unwrap_or(""), &message["attachments"])? };
        let content = if continuation {
            Value::Null
        } else {
            let mut prompted = message.clone();
            if let Some(inline) = &inline {
                prompted["text"] = json!(crate::claude_fork::with_context(&inline.context, message["text"].as_str().unwrap_or("")));
            }
            self.prompt_content(&prompted, compiled.prompt_effort.as_deref()).await?
        };
        let reusable = self.live.as_ref().is_some_and(|live| live.key == key && live.native_thread_id == native_id);
        if !reusable && !continuation {
            if let Some(live) = self.live.take() {
                live.process.shutdown().await;
                // What the old process ran, and what it left to report, is not the new one's.
                self.process_ended().map_err(|e| e.to_string())?;
            }
            let (process, events) = ClaudeProcess::spawn_tapped(&options.launch(), crate::provider_event_log::protocol_tap(&self.store, "claudeAgent", "claude-agent-sdk.query", &self.thread_id, &self.session_id))?;
            process.write(&json!({"request_id":format!("t3-init-{}", uuid::Uuid::new_v4().simple()),"type":"control_request","request":options.initialize_request()}))?;
            if fork_request.is_none() {
                self.opened.insert(native_id.clone());
            }
            self.live = Some(Live { echo_mode: EchoMode::Unknown, process, events, key, native_thread_id: native_id.clone(), opened_mode: options.permission_mode.clone(), mode: options.permission_mode.clone() });
        } else if let Some(live) = self.live.as_mut().filter(|live| live.mode != live.opened_mode && !continuation) {
            // Claude can switch its own mode (EnterPlanMode); put the thread's mode back.
            live.process.request(json!({"subtype":"set_permission_mode","mode":live.opened_mode}), Duration::from_secs(10)).await?;
            live.mode = live.opened_mode.clone();
        }
        let ask_user = claude_launch::policy(&runtime_mode, &interaction_mode).ask_user;
        let provider_turn_id = uuid::Uuid::new_v4().to_string();
        let native_turn_id = format!("turn:{}", find(&view, "attempts", &run["activeAttemptId"]).map_err(|e| e.to_string())?["id"].as_str().unwrap_or(""));
        let mut captured = TurnContext::capture(&view, run_id).map_err(|e| e.to_string())?;
        let now = at(Utc::now());
        let session_id = self.session_id.clone();
        commit(&self.store, &self.thread_id, |view| {
            if !captured.is_current(view) {
                return Err(error("Native start belongs to a replaced attempt."));
            }
            let (mut run, attempt) = captured.rows(view)?;
            if run["status"] != "starting" {
                return Err(error("Run was interrupted before the Claude turn started."));
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
            if fork_request.is_none() {
                thread["nativeThreadRef"] = thread_ref(&native_id);
            }
            thread["status"] = json!("active");
            thread["lastRunOrdinal"] = run["ordinal"].clone();
            thread["updatedAt"] = json!(now);
            let turn = json!({"id":provider_turn_id,"providerThreadId":run["providerThreadId"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":{"driver":DRIVER,"nativeId":native_turn_id,"strength":"weak"},"ordinal":run["ordinal"],"status":"running","startedAt":now,"completedAt":null});
            Ok(vec![("provider-thread.updated", thread), ("provider-turn.updated", turn), ("run.updated", run), ("run-attempt.updated", attempt), ("node.updated", node)])
        })
        .map_err(|e| e.to_string())?;
        captured.provider_turn_id = json!(provider_turn_id);
        self.fork_wait = fork_request.zip(fork_transfer).map(|(request, transfer)| crate::claude_fork::ForkWait { source_session: request.source_session, run_id: run_id.to_owned(), transfer_id: transfer["id"].clone(), provider_thread_id: run["providerThreadId"].clone() });
        let view = crate::codex_runtime::projection(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let mut projector = Projector::new(&self.thread_id, captured, &view, selection).map_err(|e| e.to_string())?;
        projector.skip_tools = self.subagents.agent_calls().clone();
        projector.continuation = continuation;
        self.settled = None;
        self.ignored_tasks.clear();
        self.route = Some(run["providerThreadId"].clone());
        let prompt_uuid = (!continuation).then(|| uuid::Uuid::new_v4().to_string());
        self.turn = Some(Turn { prompt_uuid: prompt_uuid.clone(), echo_confirmed: continuation, prompt_started: false, gated_before_echo: 0, held: vec![], continuation, projector, interrupted: false, steered: false, mode_ask_user: ask_user });
        if continuation {
            return self.drain_wake().map_err(|e| e.to_string());
        }
        self.bg.start_user_turn();
        // Written after the running state is durable: the CLI answers immediately.
        let uuid = prompt_uuid.unwrap_or_default();
        // Recorded as pending first: a message the CLI took and then lost is found by the next turn.
        if let Some(inline) = &inline {
            inline.mark(&self.store, &self.thread_id, "pending")?;
        }
        if let Err(cause) = self.live.as_ref().unwrap().process.write(&user_message(content, None, Some(&uuid))) {
            return Err(cause);
        }
        if let Some(inline) = &inline {
            inline.mark(&self.store, &self.thread_id, "inline")?;
        }
        Ok(())
    }

    /// The prompt as message content: the text, or image blocks followed by the text.
    async fn prompt_content(&self, message: &Value, prompt_effort: Option<&str>) -> Result<Value, String> {
        let mut prefixed = message.clone();
        prefixed["text"] = json!(apply_effort_prefix(message["text"].as_str().unwrap_or(""), prompt_effort));
        let text = crate::attachment_prompt::provider_text(&self.store, &prefixed);
        // Re-scanned on every send: skills are added and switched off mid-session. Claude Code
        // expands a skill only from the last text block, and only when `/name` opens it, so a
        // `$skill` chip anywhere in the prompt is split into [leading text, "/name trailing text"].
        let skills = crate::claude_skills::user_invocable_names(&self.instance.discover_skills(Some(&self.cwd)));
        let dispatch = crate::claude_skills::plan_dispatch(&text, &skills);
        let text_block = |text: &str| json!({"type":"text","text":text});
        if message["attachments"].as_array().is_none_or(Vec::is_empty) {
            return Ok(match dispatch {
                Some(dispatch) => Value::Array(dispatch.leading_text.iter().map(|leading| text_block(leading)).chain([text_block(&dispatch.command_text)]).collect()),
                None => json!(text),
            });
        }
        use base64::Engine;
        let mut blocks: Vec<Value> = dispatch.as_ref().and_then(|dispatch| dispatch.leading_text.as_deref()).map(text_block).into_iter().collect();
        for (mime, bytes) in crate::attachment_prompt::native_image_bytes(&self.store, message).await? {
            if !SUPPORTED_IMAGES.contains(&mime.as_str()) {
                return Err(format!("Unsupported Claude image attachment type '{mime}'"));
            }
            blocks.push(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":base64::engine::general_purpose::STANDARD.encode(bytes)}}));
        }
        // Images go before the command block: a text block after them still expands, a command
        // block followed by an image does not.
        match dispatch {
            Some(dispatch) => blocks.push(text_block(&dispatch.command_text)),
            None if !text.is_empty() => blocks.push(text_block(&text)),
            None => {}
        }
        Ok(Value::Array(blocks))
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
        let active = self.turn.as_ref().is_some_and(|turn| turn.projector.context().provider_turn_id == effect.request["providerTurnId"]);
        if !active || self.live.is_none() {
            return Err("Provider turn is not active and cannot be steered.".into());
        }
        let selection = self.turn.as_ref().unwrap().projector_selection();
        let content = self.prompt_content(&message, claude_models::compile_selection(&selection).prompt_effort.as_deref()).await?;
        self.turn.as_mut().unwrap().steered = true;
        self.live.as_ref().unwrap().process.write(&user_message(content, Some("now"), None))
    }

    async fn interrupt(&mut self, run_id: Option<&str>) -> Result<(), String> {
        let Some(turn) = self.turn.as_mut().filter(|turn| Some(turn.projector.context().run_id.as_str()) == run_id) else {
            return Err("Target run is no longer active.".into());
        };
        turn.interrupted = true;
        if let Some(live) = &self.live {
            // The control request asks the model to stop; closing the process is what ends any tool.
            let _ = live.process.request(json!({"subtype":"interrupt"}), Duration::from_secs(5)).await;
            live.process.shutdown().await;
        }
        // Everything the process wrote before exiting is queued; settle it before the next
        // effect can look at the (now dead) process.
        self.drain_events().map_err(|e| e.to_string())

    }

    /// `rollbackThread`: nothing is truncated in place. The live process is closed and the
    /// provider thread records the assistant message the conversation ends at, which the next
    /// open resumes at (`--resume-session-at`). Rolling back to the start mints a fresh session.
    async fn rollback(&mut self, effect: &Effect) -> Result<(), String> {
        if let Some(turn) = &self.turn {
            return Err(format!("Cannot roll back Claude provider thread while provider turn {} is active.", turn.projector.context().provider_turn_id));
        }
        let prepared = crate::checkpoint_rollback::prepare(&self.store, effect)?;
        let provider_thread = if prepared.needs_provider() {
            let native = prepared.provider_thread["nativeThreadRef"]["nativeId"].as_str().ok_or_else(|| format!("Provider thread {} is missing a native Claude session id.", prepared.provider_thread["id"].as_str().unwrap_or("")))?;
            if let Some(live) = self.live.take() {
                live.process.shutdown().await;
                self.process_ended().map_err(|e| e.to_string())?;
            }
            self.pending.clear();
            let rewound = claude_rewind(&prepared)?;
            self.opened.remove(native);
            rewound
        } else {
            prepared.provider_thread.clone()
        };
        crate::checkpoint_rollback::finish(&self.store, effect, prepared, provider_thread).await
    }

    async fn detach(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id || self.detached {
            return Ok(());
        }
        if self.turn.is_some() {
            if let Some(turn) = self.turn.as_mut() {
                turn.interrupted = true;
            }
            if let Some(live) = &self.live {
                let _ = live.process.request(json!({"subtype":"interrupt"}), Duration::from_secs(5)).await;
            }
            self.finish_turn_interrupted().map_err(|e| e.to_string())?;
        }
        if let Some(live) = self.live.take() {
            live.process.shutdown().await;
            self.process_ended().map_err(|e| e.to_string())?;
        }
        self.live = None;
        self.pending.clear();
        self.detached = true;
        Ok(())
    }
    fn reattach(&mut self) -> Result<(), String> {
        let cwd = workspace(&self.store, &self.thread_id)?;
        let view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let mut session = self.session.clone();
        session["cwd"] = json!(cwd);
        session["model"] = view["thread"]["modelSelection"]["model"].clone();
        session["updatedAt"] = json!(at(Utc::now()));
        commit(&self.store, &self.thread_id, |_| Ok(vec![("provider-session.attached", session.clone())])).map_err(|e| e.to_string())?;
        self.session = session;
        self.cwd = cwd;
        self.detached = false;
        Ok(())
    }

    /// `runtime-request.respond`: the persisted decision or answers go back to the blocked CLI.
    async fn respond(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id {
            return Err("Runtime callback belongs to a different provider session.".into());
        }
        let request_id = effect.request["requestId"].as_str().ok_or("Request id missing.")?;
        let pending = self.pending.remove(request_id).ok_or("Live runtime callback no longer exists.")?;
        let live = self.live.as_ref().ok_or("The Claude process has ended.")?;
        let response = match &pending {
            Pending::Approval { tool_name, input, suggestions, tool_use_id, .. } => {
                let decision = effect.request["decision"].as_str().unwrap_or("cancel");
                json!(permission_result(tool_name, decision, input, tool_use_id, suggestions.as_ref()))
            }
            Pending::UserInput { input, tool_use_id, .. } => {
                let answers = effect.request.get("answers").cloned().unwrap_or(json!({}));
                let view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
                let questions = view["turnItems"].as_array().into_iter().flatten().find(|item| item["requestId"] == request_id).map(|item| item["questions"].clone()).unwrap_or(json!([]));
                let answers = keyed_by_question(&answers, &questions);
                json!(PermissionResult::Allow { updated_input: Some(json!({"questions":input["questions"],"answers":projection::sdk_user_input_answers(&answers)})), updated_permissions: None, tool_use_id: Some(tool_use_id.clone()), decision_classification: None })
            }
            Pending::Dialog { question, .. } => {
                let answers = effect.request.get("answers").cloned().unwrap_or(json!({}));
                let selection = answers.get(question.as_str()).map(|answer| match answer {
                    Value::Array(values) => values.first().and_then(Value::as_str).unwrap_or("").to_owned(),
                    other => other.as_str().unwrap_or("").to_owned(),
                });
                let result = match selection.as_deref() {
                    Some("Compact and continue") => "compact",
                    Some(never) if never == crate::claude_adapter::RESUME_NEVER => "never",
                    _ => "continue",
                };
                json!({"behavior":"completed","result":result})
            }
        };
        live.process.respond(pending.control_id(), Ok(response))
    }

    /// One message from the CLI. A prompt offered while Claude has a turn of its own queued (a
    /// background task or subagent finished, a peer message arrived, a resume reports work the previous
    /// process left) is answered only after that turn runs, and the two look alike until each turn's
    /// result. Claude echoes the prompt's uuid on the turn that answers it, so a result for another turn
    /// never settles this one. On a CLI that echoes on the turn's first frame, root output that arrives
    /// before the echo is held: the echo releases it to this turn, and another turn's result sends it to
    /// the wake buffer and so to a continuation run. A CLI that echoes only on the result (or not at
    /// all) is never held.
    fn message(&mut self, message: &Value) -> Result<MessageOutcome, StoreError> {
        if message["type"] == "system" && message["subtype"] == "init" && message["parent_tool_use_id"].is_null() {
            if let Some(outcome) = self.adopt_fork(message)? {
                return Ok(outcome);
            }
        }
        let Some((prompt, echoed, mode)) = self.turn.as_ref().filter(|turn| !turn.echo_confirmed).zip(self.live.as_ref()).and_then(|(turn, live)| Some((turn.prompt_uuid.clone()?, echoed_uuids(message), live.echo_mode))) else { return self.route(message) };
        if echoed.contains(&prompt) {
            if let Some(live) = self.live.as_mut().filter(|live| matches!(live.echo_mode, EchoMode::Unknown | EchoMode::Acknowledged)) {
                live.echo_mode = if message["type"] != "result" && self.turn.as_ref().is_some_and(|turn| turn.gated_before_echo == 0) { EchoMode::Early } else { EchoMode::ResultOnly };
            }
            self.release_held()?;
            return self.route(message);
        }
        if message["type"] == "command_lifecycle" && message["command_uuid"].as_str() == Some(prompt.as_str()) {
            if let Some(live) = self.live.as_mut().filter(|live| live.echo_mode == EchoMode::Unknown) {
                live.echo_mode = EchoMode::Acknowledged;
            }
            let turn = self.turn.as_mut().unwrap();
            match message["state"].as_str() {
                Some("started") => turn.prompt_started = true,
                // Claude finishes a prompt whose uuid its transcript already holds without starting it.
                Some("completed") if !turn.prompt_started => {
                    let failure = crate::provider_failure::make(&json!({"class":"provider_error","message":"Claude acknowledged the message but never started a turn for it."}));
                    self.finish_turn(None, "failed", Some(failure))?;
                    return Ok(MessageOutcome::Finished);
                }
                _ => {}
            }
        }
        let gated = match message["type"].as_str() {
            Some("assistant" | "stream_event" | "user") => message["parent_tool_use_id"].is_null(),
            Some("result") => true,
            _ => false,
        };
        let turn = self.turn.as_mut().unwrap();
        if !gated {
            // Frames the held turn produced through its own tool uses (a subagent it launched, its task
            // lifecycle) follow that turn; anything else keeps its own owner.
            if !turn.held.is_empty() && frame_tool_use_ids(message).iter().any(|id| held_tool_use_ids(&turn.held).contains(id)) {
                turn.held.push(message.clone());
                return Ok(MessageOutcome::Continue);
            }
            return self.route(message);
        }
        turn.gated_before_echo += 1;
        if message["type"] == "result" && result_for_other_turn(message, &prompt, mode) {
            let mut held = std::mem::take(&mut turn.held);
            // The prompt's own turn follows, so echo timing is learned from it alone.
            turn.gated_before_echo = 0;
            // Lifecycle debris, not a turn: any held output stays held for its real owner.
            if message["num_turns"] == 0 {
                turn.held.append(&mut held);
                return Ok(MessageOutcome::Continue);
            }
            if mode == EchoMode::Early {
                // The held turn ran before this prompt's turn, which still follows on the same stream.
                held.push(message.clone());
                for frame in &held {
                    self.buffer_wake(frame)?;
                }
                return Ok(MessageOutcome::Continue);
            }
            // Its output already streamed into this turn, so only its result stays out: settling here
            // would end the turn before its prompt runs. The prompt's own turn ends it.
            if message["origin"]["kind"] == "task-notification" {
                self.bg.take_reports();
            }
            return Ok(MessageOutcome::Continue);
        }
        if mode != EchoMode::Early {
            return self.route(message);
        }
        if message["type"] != "result" {
            turn.held.push(message.clone());
            return Ok(MessageOutcome::Continue);
        }
        // A result from no other turn (an interrupt, a startup failure) settles the prompt's turn.
        self.release_held()?;
        self.route(message)
    }
    /// A fork's first launch reports the session Claude forked into: that is the fork's native thread.
    fn adopt_fork(&mut self, message: &Value) -> Result<Option<MessageOutcome>, StoreError> {
        let Some(wait) = self.fork_wait.take() else { return Ok(None) };
        let session = message["session_id"].as_str().unwrap_or_default();
        if session.is_empty() || session == wait.source_session {
            let failure = crate::provider_failure::make(&json!({"class":"provider_error","message":"Claude did not start a new session for the fork."}));
            self.finish_turn(None, "failed", Some(failure))?;
            return Ok(Some(MessageOutcome::Finished));
        }
        let view = view(&self.store, &self.thread_id)?;
        let instance = self.session["providerInstanceId"].as_str().unwrap_or_default().to_owned();
        let events = crate::claude_fork::adopt(&view, &wait, &instance, session).map_err(error)?;
        commit(&self.store, &self.thread_id, |_| Ok(events))?;
        if let Some(live) = self.live.as_mut() {
            live.native_thread_id = session.to_owned();
        }
        self.opened.insert(session.to_owned());
        Ok(None)
    }
    /// Gives held root output to the pending prompt turn.
    fn release_held(&mut self) -> Result<(), StoreError> {
        let Some(turn) = self.turn.as_mut() else { return Ok(()) };
        turn.echo_confirmed = true;
        for frame in std::mem::take(&mut turn.held) {
            self.route(&frame)?;
        }
        Ok(())
    }
    /// One message for the active turn, once any prompt-echo gate has let it through.
    fn route(&mut self, message: &Value) -> Result<MessageOutcome, StoreError> {
        // Before any routing: a Monitor started during an idle wake reports its task before the drain
        // replays the tool call.
        self.bg.track_monitor_calls(message);
        if message["type"] == "rate_limit_event" {
            return self.rate_limit(message).map(|_| MessageOutcome::Continue);
        }
        if message["type"] == "system" && message["subtype"] == "task_started" && message["spawn_depth"].as_u64().unwrap_or(1) > 1 && !is_opaque_task(message) {
            if let Some(task_id) = message["task_id"].as_str() {
                self.nested.insert(task_id.to_owned());
            }
        }
        let Some(turn) = self.turn.as_mut() else { return self.idle_message(message).map(|_| MessageOutcome::Continue) };
        if message["type"] == "result" && message["parent_tool_use_id"].is_null() {
            // A zero-turn task-notification-origin result is lifecycle debris: it must not finalize a
            // normal turn. A continuation consumes it when draining.
            let task_origin = message["origin"]["kind"] == "task-notification";
            if task_origin && !turn.continuation && message["num_turns"] == 0 {
                return Ok(MessageOutcome::Continue);
            }
            if task_origin && !turn.continuation {
                // This user turn ran the wake itself, so no offer will name its work.
                self.bg.take_reports();
            }
            let turn = self.turn.as_mut().unwrap();
            let Some(terminal) = turn.projector.terminal(message, turn.interrupted, turn.steered) else {
                turn.steered = false;
                return Ok(MessageOutcome::Continue);
            };
            turn.steered = false;
            self.finish_turn(Some(message), terminal.status, terminal.failure)?;
            return Ok(MessageOutcome::Finished);
        }
        if message["type"] == "stream_event" {
            // Reasoning deltas accumulate in memory and are written on the flush timer; only
            // block boundaries need the projection.
            let view = if message["event"]["type"] == "content_block_delta" { Value::Null } else { view(&self.store, &self.thread_id)? };
            let events = turn.projector.plan(&view, message)?;
            return self.commit_events(events).map(|_| MessageOutcome::Continue);
        }
        self.frame(message, false)?;
        Ok(MessageOutcome::Continue)
    }
    /// A frame while no turn runs. A running subagent keeps reporting through the turn that settled
    /// last (`settledTurnForSubagentFrame`); anything else is wake output, kept for the run that
    /// ingests it.
    fn idle_message(&mut self, message: &Value) -> Result<(), StoreError> {
        if self.settled.is_some() && self.settled_takes(message) {
            return self.frame(message, false);
        }
        // A notification is wake evidence while its task is still on the roster; clearing first would drop it.
        if message["type"] == "system" && message["subtype"] == "task_notification" {
            self.buffer_wake(message)?;
            self.apply_roster(message, false)
        } else {
            self.apply_roster(message, false)?;
            self.buffer_wake(message)
        }
    }
    /// Applies a roster frame and writes the roster onto the provider thread when it changed.
    fn apply_roster(&mut self, message: &Value, active: bool) -> Result<(), StoreError> {
        if !self.bg.apply(message) {
            return Ok(());
        }
        self.write_roster(active)
    }
    /// `emitProviderThreadRoster`: between turns, never resurrect an active status from a late roster.
    fn write_roster(&mut self, active: bool) -> Result<(), StoreError> {
        let Some(provider_thread) = self.route.clone() else { return Ok(()) };
        let (roster, session, now) = (self.bg.roster(), self.session_id.clone(), at(Utc::now()));
        commit(&self.store, &self.thread_id, |view| {
            let Ok(row) = find(view, "providerThreads", &provider_thread) else { return Ok(vec![]) };
            let mut row = row.clone();
            row["status"] = json!(if !active { "idle" } else if row["status"] == "idle" { "active" } else { row["status"].as_str().unwrap_or("active") });
            row["providerSessionId"] = json!(session);
            row["pendingBackgroundTasks"] = json!(roster);
            row["updatedAt"] = json!(now);
            Ok(vec![("provider-thread.updated", row)])
        })
    }
    /// `bufferWakeMessage`: output the CLI produced outside a turn. Evidence of a wake is kept, and
    /// once it shows Claude began a turn of its own, a run is offered to ingest it.
    fn buffer_wake(&mut self, message: &Value) -> Result<(), StoreError> {
        let kind = message["type"].as_str().unwrap_or_default();
        let subtype = message["subtype"].as_str().unwrap_or_default();
        let task_id = message["task_id"].as_str().unwrap_or_default();
        let notification = kind == "system" && subtype == "task_notification";
        let started = kind == "system" && subtype == "task_started";
        let pending_task = notification && self.bg.is_wake_eligible(task_id);
        let nested = notification && !pending_task && self.nested.contains(task_id);
        let buffered_start = self.bg.buffer.iter().any(|entry| entry["type"] == "system" && entry["subtype"] == "task_started" && entry["task_id"] == task_id);
        let pending_subagent = notification && !pending_task && !nested && (self.subagents.is_running(task_id) || buffered_start);
        let new_subagent_started = started && !is_opaque_task(message);
        // A `task_started` for a registered subagent that races past settle is a resume: the entry is
        // reopened so the resumed run pins idle and its end counts as wake evidence again.
        let known_subagent_started = started && self.subagents.is_registered(task_id);
        if known_subagent_started {
            self.subagents.preopen(task_id);
        }
        let turn_start = background::is_turn_start(message);
        let evidence = pending_task || pending_subagent || nested || known_subagent_started || new_subagent_started || turn_start || matches!(kind, "assistant" | "user" | "result" | "rate_limit_event");
        if !evidence {
            return Ok(());
        }
        let summary = message["summary"].as_str().filter(|summary| !summary.is_empty()).map(str::to_owned);
        if pending_task {
            let report = self.bg.opaque_report(message);
            self.bg.record_report(task_id, report);
        } else if pending_subagent {
            let registered = self.subagents.task(task_id);
            let label = registered.and_then(|task| task["title"].as_str().map(str::to_owned)).or_else(|| self.bg.buffer.iter().find(|entry| entry["type"] == "system" && entry["subtype"] == "task_started" && entry["task_id"] == task_id).and_then(|entry| entry["description"].as_str().map(str::to_owned)));
            let child = self.subagents.child_thread_of(task_id).map(str::to_owned);
            self.bg.record_report(task_id, Background::subagent_report(label.as_deref(), message, child.as_deref()));
        }
        self.bg.buffer.push(message.clone());
        self.bg.detail = summary.or_else(|| self.bg.detail.take());
        if pending_task {
            self.bg.consume_eligibility(task_id);
        }
        // A terminal task notification can clear the roster without Claude dequeuing it into a model
        // turn: buffer it, but offer nothing until the turn's `init`, or root output, shows Claude began it.
        let has_notification = self.bg.buffer.iter().any(|entry| entry["type"] == "system" && entry["subtype"] == "task_notification");
        let native_opaque_wake_frame = has_notification && matches!(kind, "assistant" | "user") && message["parent_tool_use_id"].is_null();
        if !pending_subagent && !native_opaque_wake_frame && !turn_start && kind != "result" {
            return Ok(());
        }
        let Some(_) = self.route else {
            tracing::warn!("A Claude wake turn has no thread to run on.");
            return Ok(());
        };
        if std::mem::replace(&mut self.bg.requested, true) {
            return Ok(());
        }
        let detail = self.bg.detail.clone();
        let notification = crate::notification::background_work_notification(&self.bg.take_reports());
        if let Err(cause) = crate::provider_continuation::offer(&self.store, &self.providers, &self.thread_id, detail.as_deref(), notification) {
            tracing::warn!(%cause, "Claude wake turn could not be offered");
            self.bg.requested = false;
        }
        Ok(())
    }
    /// A continuation run attaches to the output buffered for it: everything but the result first, the
    /// result last, since a result ends the turn.
    fn drain_wake(&mut self) -> Result<(), StoreError> {
        let drained = std::mem::take(&mut self.bg.buffer);
        self.bg.detail = None;
        self.bg.requested = false;
        // A spurious continuation (the buffer died with a recycled process) settles instead of waiting for a prompt never sent.
        if drained.is_empty() {
            return self.finish_turn(None, "completed", None);
        }
        let (results, others): (Vec<Value>, Vec<Value>) = drained.into_iter().partition(|entry| entry["type"] == "result");
        let has_opaque_notification = others.iter().any(|entry| entry["type"] == "system" && entry["subtype"] == "task_notification" && entry["task_id"].as_str().is_some_and(|id| self.bg.has_tombstone(id)));
        for entry in &others {
            self.message(entry)?;
        }
        if let Some(last) = results.last() {
            self.message(last)?;
            return Ok(());
        }
        // A drained `init` means Claude began the wake turn, so its output may still be on the way.
        let has_native_frame = others.iter().any(|entry| matches!(entry["type"].as_str(), Some("user" | "assistant")) || background::is_turn_start(entry));
        if has_opaque_notification && !has_native_frame {
            self.finish_turn(None, "completed", None)?;
        }
        Ok(())
    }
    /// Stop on a settled run: the background shells belong to the CLI process, so closing it is what
    /// stops them. Its roster, the wake it left and the subagents it ran are no longer anything.
    async fn stop_background_work(&mut self) -> Result<(), String> {
        if self.turn.is_some() {
            return Ok(());
        }
        if let Some(live) = self.live.take() {
            live.process.shutdown().await;
        }
        self.process_ended().map_err(|e| e.to_string())?;
        self.write_roster(false).map_err(|e| e.to_string())
    }
    /// Whether an idle frame belongs to a subagent that is still running (`settledTurnForSubagentFrame`).
    fn settled_takes(&self, message: &Value) -> bool {
        let kind = message["type"].as_str().unwrap_or_default();
        let parent = message["parent_tool_use_id"].as_str();
        let results = projection::tool_results(message);
        // The result of a call already shown as a row only ends that row, so it never waits.
        if kind == "user" && !results.is_empty() && results.len() == message["message"]["content"].as_array().map_or(0, Vec::len) && results.iter().all(|result| result["tool_use_id"].as_str().is_some_and(|id| self.subagents.child_has_tool(id))) {
            return true;
        }
        // A subagent's `task_started` still in the wake buffer registers it only when a continuation
        // drains it, and only that run stores the work that follows. So that subagent's own frames, and
        // the result of the `Agent` call that launched it, wait behind it.
        let starts: Vec<&Value> = self.bg.buffer.iter().filter(|entry| entry["type"] == "system" && entry["subtype"] == "task_started" && !is_opaque_task(entry)).collect();
        let progress = kind == "system" && message["subtype"] == "task_progress";
        let task_id = if progress { message["task_id"].as_str() } else { parent.and_then(|parent| self.subagents.task_of_tool_use(parent)) };
        let returned: Vec<&str> = results.iter().filter_map(|result| result["tool_use_id"].as_str()).collect();
        if starts.iter().any(|start| start["task_id"].as_str() == task_id && task_id.is_some() || start["tool_use_id"].as_str().is_some_and(|id| Some(id) == parent || returned.contains(&id))) {
            return false;
        }
        // An owner named later waits in the hold used during a turn.
        let Some(task_id) = task_id else { return parent.is_some() && matches!(kind, "assistant" | "user") };
        // Without the launching call's id, any result may be its launch.
        if !returned.is_empty() && starts.iter().any(|start| start["tool_use_id"].is_null()) {
            return false;
        }
        matches!(kind, "assistant" | "user" | "system") && self.subagents.is_running(task_id)
    }
    /// The frames that are not the turn's own text: task lifecycle, subagent output, `Agent` calls.
    fn frame(&mut self, message: &Value, replayed: bool) -> Result<(), StoreError> {
        let kind = message["type"].as_str().unwrap_or_default();
        let subtype = message["subtype"].as_str().unwrap_or_default();
        let parent_tool = message["parent_tool_use_id"].as_str();
        let root_active = self.turn.is_some();
        if background::is_roster_snapshot(message) {
            return self.apply_roster(message, root_active);
        }
        if kind == "system" && subtype.starts_with("task_") {
            let task_id = message["task_id"].as_str().unwrap_or_default().to_owned();
            self.with_parent(|parent, subagents, root| subagents.release_before(parent, root, message))?;
            match subtype {
                "task_started" if is_opaque_task(message) => {
                    self.ignored_tasks.insert(task_id);
                    self.apply_roster(message, root_active)?;
                }
                "task_started" => {
                    // A nested subagent's end goes to its owner, so it never wakes the root.
                    if message["is_backgrounded"] == true && !self.nested.contains(&task_id) {
                        self.bg.backgrounded.insert(task_id.clone());
                    }
                    // A task started under a call the turn has open is a resume of a subagent this process never saw.
                    let resumed_by = message["tool_use_id"].as_str().filter(|id| self.turn.as_ref().is_some_and(|turn| turn.projector.has_tool(id)) && !self.subagents.is_registered(&task_id));
                    if let Some(resumed_by) = resumed_by {
                        let launch = self.live.as_ref().and_then(|live| subagent_launch_tool_use(&self.instance.config_dir(), &live.native_thread_id, &task_id));
                        if let Some(launch) = launch.filter(|launch| launch != resumed_by) {
                            let title = message["description"].as_str().unwrap_or_default();
                            self.with_parent(|parent, subagents, root| subagents.recover_resumed(parent, root, &task_id, title, &launch))?;
                        }
                    }
                    self.with_parent(|parent, subagents, root| subagents.task_started(parent, root, message))?;
                }
                "task_progress" if !self.ignored_tasks.contains(&task_id) && !self.bg.has_task(&task_id) => {
                    self.with_parent(|parent, subagents, root| subagents.progress(parent, root, message))?;
                }
                "task_notification" => {
                    // A wake-replay turn has no ignored tasks, so opaque-task tracking (roster, wake
                    // eligibility, or the tombstone an idle notification left) classifies it first.
                    let was_background = self.bg.is_known_opaque(&task_id);
                    // Backgrounded work that ends during a user turn wakes the root after it.
                    if !self.turn.as_ref().is_some_and(|turn| turn.continuation) {
                        if was_background {
                            let report = self.bg.opaque_report(message);
                            self.bg.record_report(&task_id, report);
                        } else if self.bg.backgrounded.contains(&task_id) {
                            let label = self.subagents.task(&task_id).and_then(|task| task["title"].as_str().map(str::to_owned));
                            let child = self.subagents.child_thread_of(&task_id).map(str::to_owned);
                            self.bg.record_report(&task_id, Background::subagent_report(label.as_deref(), message, child.as_deref()));
                        }
                    }
                    // A resume starts the subagent again and says again whether it is backgrounded.
                    self.bg.backgrounded.remove(&task_id);
                    self.apply_roster(message, root_active)?;
                    if !was_background && !self.ignored_tasks.contains(&task_id) {
                        self.with_parent(|parent, subagents, root| subagents.notification(parent, root, message))?;
                    }
                    if was_background {
                        self.bg.clear_tombstone(&task_id);
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if !matches!(kind, "assistant" | "user") {
            // Everything else belongs to the root turn.
            if let Some(turn) = self.turn.as_mut() {
                let view = view(&self.store, &self.thread_id)?;
                let events = turn.projector.plan(&view, message)?;
                let mode = turn.projector.permission_mode.take();
                self.commit_events(events)?;
                if let (Some(mode), Some(live)) = (mode, self.live.as_mut()) {
                    live.mode = mode;
                }
            }
            return Ok(());
        }
        if parent_tool.is_some() {
            self.with_parent(|parent, subagents, root| subagents.child_frame(parent, root, message, replayed))?;
            return Ok(());
        }
        if !root_active {
            return Ok(());
        }
        let turn = self.turn.as_mut().unwrap();
        let view = view(&self.store, &self.thread_id)?;
        // Which results answer a call the turn has open: those are tools', whatever else they name.
        let results = projection::tool_results(message);
        let open: Vec<bool> = results.iter().map(|result| result["tool_use_id"].as_str().is_some_and(|id| turn.projector.has_tool(id))).collect();
        let events = turn.projector.plan(&view, message)?;
        let launches = turn.projector.take_launches();
        let model = turn.projector.selection()["model"].as_str().unwrap_or_default().to_owned();
        self.commit_events(events)?;
        for (id, input) in launches {
            self.subagents.remember_launch(&id, &input, None, &model);
        }
        // The result of an `Agent` call ends its subagent; the result of a call the turn has open is a tool's.
        let structured = (results.len() == 1).then(|| message.get("tool_use_result")).flatten().filter(|value| !value.is_null()).cloned();
        for (result, was_open) in results.iter().zip(open) {
            let Some(id) = result["tool_use_id"].as_str() else { continue };
            if self.subagents.resolve_tool_use(id).is_some() && !was_open {
                self.with_parent(|parent, subagents, root| subagents.agent_result(parent, root, result, structured.as_ref()))?;
            }
        }
        Ok(())
    }
    /// Runs `f` with the turn that writes the parent's rows: the running turn, else the one that
    /// settled last.
    fn with_parent<R>(&mut self, f: impl FnOnce(&Parent, &mut Subagents, &mut Projector) -> Result<R, StoreError>) -> Result<Option<R>, StoreError> {
        let projector = match self.turn.as_mut() {
            Some(turn) => &mut turn.projector,
            None => match self.settled.as_mut() {
                Some(settled) => settled,
                None => return Ok(None),
            },
        };
        let (ctx, selection) = (projector.context().clone(), projector.selection().clone());
        let view = view(&self.store, &self.thread_id)?;
        let parent = Parent { store: &self.store, ctx: &ctx, selection: &selection, app_thread: &view["thread"] };
        f(&parent, &mut self.subagents, projector).map(Some)
    }
    /// A `rate_limit_event` updates the instance's published usage windows, and a rejected window
    /// is announced on the turn.
    fn rate_limit(&mut self, message: &Value) -> Result<(), StoreError> {
        let info = &message["rate_limit_info"];
        if !info.is_object() {
            return Ok(());
        }
        let instance = self.session["providerInstanceId"].as_str().unwrap_or("").to_owned();
        let names = crate::claude_usage_limits::scoped_names(&instance);
        let now = Utc::now();
        if let Some(update) = crate::claude_usage_limits::rate_limit_event_to_update(info, &names) {
            self.providers.apply_usage_limits(&instance, &update, &at(now));
        }
        // A rejected window can open the CLI's wake before the run that records it exists: park it with the wake output.
        let Some(turn) = self.turn.as_mut() else { return self.buffer_wake(message) };
        let view = view(&self.store, &self.thread_id)?;
        let events = turn.projector.rate_limit(&view, info, &names, now.timestamp_millis() as f64)?;
        self.commit_events(events)
    }
    fn commit_events(&self, events: projection::Events) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        commit(&self.store, &self.thread_id, |_| Ok(events))
    }
    fn flush_reasoning(&mut self) -> Result<(), StoreError> {
        let Some(turn) = self.turn.as_mut() else { return Ok(()) };
        let view = view(&self.store, &self.thread_id)?;
        let events = turn.projector.flush_reasoning(&view)?;
        self.commit_events(events)
    }
    fn finish_turn(&mut self, result: Option<&Value>, status: &'static str, failure: Option<Value>) -> Result<(), StoreError> {
        let Some(mut turn) = self.turn.take() else { return Ok(()) };
        let view = view(&self.store, &self.thread_id)?;
        let events = turn.projector.finish(&view, result, status)?;
        commit(&self.store, &self.thread_id, |_| Ok(events))?;
        let ctx = turn.projector.context().clone();
        // A turn that did not complete takes the calls its subagents had open with it.
        if status != "completed" {
            self.subagents.end_open_calls(&self.store, if status == "interrupted" { "interrupted" } else { "failed" })?;
        }
        // A turn that did not complete drops the roster: its monitors and shells never report.
        if status != "completed" {
            self.bg.reset_process();
            self.write_roster(true)?;
        }
        self.settled = (status == "completed").then_some(turn.projector);
        // The conversation head only points past the last turn until the next turn completes.
        if status == "completed" {
            let thread = find(&view, "providerThreads", &ctx.provider_thread_id)?;
            if !thread["nativeConversationHeadRef"].is_null() {
                let mut thread = thread.clone();
                thread["nativeConversationHeadRef"] = Value::Null;
                commit(&self.store, &self.thread_id, |_| Ok(vec![("provider-thread.updated", thread)]))?;
            }
        }
        // Requests still open when the turn ends can no longer be answered; the shared
        // terminal marks them expired or cancelled.
        self.pending.clear();
        if status == "failed" {
            let failure = failure.unwrap_or_else(|| crate::provider_failure::make(&json!({"class":"provider_error"})));
            fail_run_with(&self.store, &self.thread_id, Some(&ctx.run_id), Some(&ctx), &failure, Retain::Nothing)?;
        } else {
            // A subagent a completed turn left running goes on, and reports when it ends.
            terminal_with(&self.store, &self.thread_id, Some(&ctx.run_id), status, None, Some(&ctx), Retain::CommandsOnCompletion)?;
            settle_queue(&self.store, &self.thread_id, None)?;
        }
        Ok(())
    }
    fn finish_turn_interrupted(&mut self) -> Result<(), StoreError> {
        self.finish_turn(None, "interrupted", None)
    }


    /// One event from the CLI. Returns true when the actor cannot continue (a failed run).
    fn handle_event(&mut self, event: Option<Event>) -> Result<bool, StoreError> {
        match event {
            Some(Event::Message(message)) => self.message(&message).map(|_| false),
            Some(Event::Request { request_id, request }) => {
                match self.control_request(request_id.clone(), &request) {
                    Ok(Some(response)) => {
                        if let Some(live) = &self.live {
                            let _ = live.process.respond(&request_id, Ok(response));
                        }
                    }
                    Ok(None) => {}
                    Err(cause) => {
                        if let Some(live) = &self.live {
                            let _ = live.process.respond(&request_id, Err(cause.to_string()));
                        }
                    }
                }
                Ok(false)
            }
            Some(Event::Cancel { request_id }) => self.request_cancelled(&request_id).map(|_| false),
            Some(Event::Closed(exit)) => {
                self.live = None;
                self.process_ended()?;
                let interrupted = self.turn.as_ref().is_some_and(|turn| turn.interrupted);
                if self.turn.is_none() {
                    Ok(false)
                } else if interrupted {
                    self.finish_turn_interrupted().map(|_| false)
                } else {
                    let failure = crate::provider_failure::make(&json!({"message":exit.describe(),"class":"transport_error","retryable":false}));
                    let ctx = self.turn.as_ref().map(|turn| turn.projector.context().clone());
                    self.turn = None;
                    fail_run_captured(&self.store, &self.thread_id, ctx.as_ref().map(|c| c.run_id.as_str()), ctx.as_ref(), &failure).map(|_| true)
                }
            }
            None => {
                self.live = None;
                Ok(false)
            }
        }
    }
    /// The process is gone: the subagents it ran never report again, and the calls they had open end.
    fn process_ended(&mut self) -> Result<(), StoreError> {
        let status = if self.turn.as_ref().is_some_and(|turn| turn.interrupted) { "interrupted" } else { "failed" };
        self.subagents.end_open_calls(&self.store, status)?;
        self.subagents.end_running();
        let had_roster = !self.bg.roster().is_empty();
        self.bg.reset_process();
        self.bg.clear_wake();
        if had_roster {
            self.write_roster(self.turn.is_some())?;
        }
        Ok(())
    }
    /// Process every event already received from a process that has been shut down.
    fn drain_events(&mut self) -> Result<(), StoreError> {
        while let Some(event) = self.live.as_mut().and_then(|live| live.events.try_recv().ok()) {
            self.handle_event(Some(event))?;
        }
        Ok(())
    }
    /// `can_use_tool` and `request_user_dialog` control requests.
    fn control_request(&mut self, control_id: String, request: &Value) -> Result<Option<Value>, StoreError> {
        match request["subtype"].as_str() {
            Some("can_use_tool") => self.permission(control_id, request),
            Some("request_user_dialog") if request["dialog_kind"] == "resume_return" => self.resume_dialog(control_id, request),
            Some("elicitation") => Ok(Some(json!({"action":"decline"}))),
            Some(other) => Err(error(format!("Unsupported control request subtype: {other}"))),
            None => Err(error("Control request has no subtype.")),
        }
    }
    fn permission(&mut self, control_id: String, request: &Value) -> Result<Option<Value>, StoreError> {
        let tool_name = request["tool_name"].as_str().unwrap_or("").to_owned();
        let tool_use_id = request["tool_use_id"].as_str().unwrap_or("").to_owned();
        let input = request["input"].clone();
        if self.turn.is_none() {
            return Ok(Some(json!(PermissionResult::Deny { message: "Claude V2 adapter has no active turn for this tool request.".into(), interrupt: None, tool_use_id: Some(tool_use_id), decision_classification: None })));
        }
        // While root output awaits its prompt echo, its frames are held and may belong to a queued wake
        // turn, so the callback attaches nothing to the pending prompt turn. A request the user must
        // answer cannot wait for the echo, which the held turn is blocked on: the held output goes with it.
        let mut held_for_echo = self.turn.as_ref().is_some_and(|turn| !turn.held.is_empty());
        if held_for_echo && tool_name != "ExitPlanMode" && (tool_name == "AskUserQuestion" || self.turn.as_ref().is_some_and(|turn| turn.mode_ask_user)) {
            self.release_held()?;
            held_for_echo = false;
        }
        let turn = self.turn.as_mut().unwrap();
        let view = view(&self.store, &self.thread_id)?;
        // An `Agent` call is a subagent, not a tool; a tool a subagent runs is already in its thread.
        let mut events = if tool_name == "Agent" {
            let model = turn.projector.selection()["model"].as_str().unwrap_or_default().to_owned();
            turn.projector.skip_tools.insert(tool_use_id.clone());
            self.subagents.remember_launch(&tool_use_id, &input, None, &model);
            vec![]
        } else if held_for_echo || self.subagents.child_has_tool(&tool_use_id) {
            vec![]
        } else {
            turn.projector.ensure_tool(&view, &tool_use_id, &tool_name, &input)?
        };
        if tool_name == "AskUserQuestion" {
            let questions = projection::user_input_questions(&input);
            let (request_id, mut more) = turn.projector.request_artifacts(&view, &tool_use_id, &tool_use_id, "user_input", None, Some(questions), &self.session_id)?;
            events.append(&mut more);
            self.commit_events(events)?;
            self.pending.insert(request_id, Pending::UserInput { control_id, input, tool_use_id });
            return Ok(None);
        }
        if tool_name == "ExitPlanMode" {
            if let Some(markdown) = projection::proposed_plan(&input) {
                events.extend(turn.projector.plan_projection(&view, &tool_use_id, "proposed_plan", None, Some(markdown))?);
            }
            self.commit_events(events)?;
            return Ok(Some(json!(PermissionResult::Deny { message: "The client captured your proposed plan. Stop here and wait for the user's feedback or implementation request in a later turn.".into(), interrupt: None, tool_use_id: Some(tool_use_id), decision_classification: None })));
        }
        if !turn.mode_ask_user {
            self.commit_events(events)?;
            return Ok(Some(json!(PermissionResult::Allow { updated_input: Some(input), updated_permissions: None, tool_use_id: Some(tool_use_id), decision_classification: None })));
        }
        let kind = projection::classify(&tool_name).request_kind;
        let prompt = request["title"].as_str().or(request["description"].as_str()).or(request["decision_reason"].as_str()).map(str::to_owned).unwrap_or_else(|| projection::summarize_request(&tool_name, &input));
        let (request_id, mut more) = turn.projector.request_artifacts(&view, &tool_use_id, &tool_use_id, kind, Some(&prompt), None, &self.session_id)?;
        events.append(&mut more);
        self.commit_events(events)?;
        self.pending.insert(request_id, Pending::Approval { control_id, tool_name, input, suggestions: request.get("permission_suggestions").cloned().filter(|v| !v.is_null()), tool_use_id });
        Ok(None)
    }
    /// The CLI asks whether to compact a long-idle session on resume; it is put to the user as a question.
    fn resume_dialog(&mut self, control_id: String, request: &Value) -> Result<Option<Value>, StoreError> {
        let Some(turn) = self.turn.as_mut() else { return Ok(Some(json!({"behavior":"cancelled"}))) };
        let age = request["payload"]["sessionAgeMinutes"].as_f64().filter(|v| v.is_finite()).map_or(0, |v| v.max(0.0).floor() as u64);
        let tokens = request["payload"]["estimatedTokens"].as_f64().filter(|v| v.is_finite()).map_or(0, |v| v.max(0.0).floor() as u64);
        let question = format!("This session has been idle for {age} minutes and holds about {tokens} tokens. Compact it before continuing?");
        let questions = vec![json!({"id":question,"header":"Resume session","question":question,"options":[
            {"label":"Compact and continue","description":"Resume with a summary and use fewer tokens."},
            {"label":"Keep full history","description":"Resume without changing the conversation."},
            {"label":RESUME_NEVER,"description":"Keep full history and skip future resume prompts."}],"multiSelect":false})];
        let view = view(&self.store, &self.thread_id)?;
        let native = request["tool_use_id"].as_str().map(str::to_owned).unwrap_or_else(|| format!("resume-dialog:{control_id}"));
        let (request_id, events) = turn.projector.request_artifacts(&view, &native, &native, "user_input", None, Some(questions), &self.session_id)?;
        self.commit_events(events)?;
        self.pending.insert(request_id, Pending::Dialog { control_id, question });
        Ok(None)
    }
    /// The CLI withdrew a request (interrupt, or answered elsewhere): record it as cancelled.
    fn request_cancelled(&mut self, control_id: &str) -> Result<(), StoreError> {
        let Some(request_id) = self.pending.iter().find(|(_, pending)| pending.control_id() == control_id).map(|(id, _)| id.clone()) else { return Ok(()) };
        self.pending.remove(&request_id);
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |view| {
            let mut request = find(view, "runtimeRequests", &json!(request_id))?.clone();
            if request["status"] != "pending" {
                return Ok(vec![]);
            }
            request["status"] = json!("cancelled");
            request["resolvedAt"] = json!(now);
            request["responseCapability"] = json!({"type":"not_resumable","reason":"Claude withdrew the request."});
            Ok(vec![("runtime-request.updated", request)])
        })
    }
}
/// The tool call that launched a subagent, which the CLI keeps with the subagent's transcript in its
/// session storage (`<config>/projects/<project>/<session>/subagents/**/agent-<id>.meta.json`).
fn subagent_launch_tool_use(config_dir: &std::path::Path, session: &str, agent: &str) -> Option<String> {
    fn find(directory: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(directory).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find(&path, name) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|file| file == name) {
                return Some(path);
            }
        }
        None
    }
    let name = format!("agent-{agent}.meta.json");
    std::fs::read_dir(config_dir.join("projects")).ok()?.flatten().find_map(|project| {
        let found = find(&project.path().join(session).join("subagents"), &name)?;
        let meta: Value = serde_json::from_str(&std::fs::read_to_string(found).ok()?).ok()?;
        meta["toolUseId"].as_str().map(str::to_owned)
    })
}
/// A `local_bash` task: a background command, which the provider thread's roster lists, not a subagent.
fn is_opaque_task(message: &Value) -> bool {
    background::is_opaque_task(message)
}
/// The prompts a frame says its turn consumed.
fn echoed_uuids(message: &Value) -> Vec<String> {
    if let Some(uuids) = message["user_message_uuids"].as_array() {
        return uuids.iter().filter_map(Value::as_str).map(str::to_owned).collect();
    }
    message["user_message_uuid"].as_str().map(str::to_owned).into_iter().collect()
}
/// The tool use a non-root frame belongs to: a subagent frame's parent, or a task lifecycle frame's call.
fn frame_tool_use_ids(message: &Value) -> Vec<String> {
    let mut ids: Vec<String> = message["parent_tool_use_id"].as_str().map(str::to_owned).into_iter().collect();
    if message["type"] == "system" {
        ids.extend(message["tool_use_id"].as_str().map(str::to_owned));
    }
    ids
}
fn held_tool_use_ids(held: &[Value]) -> Vec<String> {
    held.iter().filter(|frame| frame["parent_tool_use_id"].is_null()).flat_map(|frame| frame["message"]["content"].as_array().into_iter().flatten().filter(|block| block["type"] == "tool_use").filter_map(|block| block["id"].as_str().map(str::to_owned))).collect()
}
/// A result that answers a turn other than the pending prompt's. An echo names the prompts its turn
/// consumed. Without one, only a process known to echo proves the turn foreign by its non-human origin.
fn result_for_other_turn(message: &Value, prompt: &str, mode: EchoMode) -> bool {
    let echoed = echoed_uuids(message);
    if !echoed.is_empty() {
        return !echoed.iter().any(|uuid| uuid == prompt);
    }
    mode != EchoMode::Unknown && message["origin"].is_object() && message["origin"]["kind"] != "human"
}
pub(crate) const RESUME_NEVER: &str = "Keep full history and never ask again";
enum MessageOutcome {
    Continue,
    Finished,
}
impl Turn {
    fn projector_selection(&self) -> Value {
        self.projector.selection().clone()
    }
}

/// `resolveClaudeRollbackResumeSessionAt`: the assistant message the rolled-back conversation
/// ends at, `None` for the start. A turn without a recorded cursor can only be the last one.
fn rollback_resume_session_at(prepared: &crate::checkpoint_rollback::Prepared) -> Result<Option<String>, String> {
    let target = &prepared.target;
    if target["type"] == "thread_start" {
        return Ok(None);
    }
    let turn = &target["providerTurn"];
    if turn["providerThreadId"] != prepared.provider_thread["id"] {
        return Err(format!("Cannot roll back Claude thread {} to provider turn {}: target turn belongs to provider thread {}.", prepared.provider_thread["id"].as_str().unwrap_or(""), turn["id"].as_str().unwrap_or(""), turn["providerThreadId"].as_str().unwrap_or("")));
    }
    let native = &turn["nativeTurnRef"];
    if native["driver"] == DRIVER {
        if let Some(id) = native["nativeId"].as_str().filter(|id| !id.starts_with("turn:")) {
            return Ok(Some(id.to_owned()));
        }
    }
    let later = prepared.provider_thread_turns.iter().any(|candidate| candidate["ordinal"].as_u64() > turn["ordinal"].as_u64() && matches!(candidate["status"].as_str(), Some("completed" | "interrupted" | "failed" | "cancelled")));
    if later {
        return Err(format!("Cannot roll back Claude thread {} to provider turn {}: no SDK assistant message cursor was recorded for that turn.", prepared.provider_thread["id"].as_str().unwrap_or(""), turn["id"].as_str().unwrap_or("")));
    }
    Ok(None)
}
/// `ClaudeAdapterV2.rollbackThread`'s result: the provider thread as the rollback leaves it.
fn claude_rewind(prepared: &crate::checkpoint_rollback::Prepared) -> Result<Value, String> {
    let mut thread = prepared.provider_thread.clone();
    thread["status"] = json!("idle");
    thread["updatedAt"] = json!(at(Utc::now()));
    if prepared.target["type"] == "thread_start" {
        thread["nativeThreadRef"] = thread_ref(&uuid::Uuid::new_v4().to_string());
        thread["nativeConversationHeadRef"] = Value::Null;
        return Ok(thread);
    }
    thread["nativeConversationHeadRef"] = match rollback_resume_session_at(prepared)? {
        Some(cursor) => json!({"driver":DRIVER,"nativeId":cursor,"strength":"weak"}),
        None => Value::Null,
    };
    Ok(thread)
}

/// `permissionResultFromDecision`
pub(crate) fn permission_result(tool_name: &str, decision: &str, input: &Value, tool_use_id: &str, suggestions: Option<&Value>) -> PermissionResult {
    if decision == "accept" || decision == "acceptForSession" {
        let always = decision == "acceptForSession";
        let updated = always.then(|| {
            let updates: Vec<Value> = suggestions.and_then(Value::as_array).into_iter().flatten().map(|suggestion| {
                let mut suggestion = suggestion.clone();
                suggestion["destination"] = json!("session");
                suggestion
            }).collect();
            if updates.is_empty() { json!([{"type":"addRules","rules":[{"toolName":tool_name}],"behavior":"allow","destination":"session"}]) } else { json!(updates) }
        });
        return PermissionResult::Allow { updated_input: Some(input.clone()), updated_permissions: updated, tool_use_id: Some(tool_use_id.into()), decision_classification: Some(if always { "user_permanent" } else { "user_temporary" }) };
    }
    PermissionResult::Deny {
        message: if decision == "cancel" { "User cancelled tool execution." } else { "User declined tool execution." }.into(),
        interrupt: (decision == "cancel").then_some(true),
        tool_use_id: Some(tool_use_id.into()),
        decision_classification: Some("user_reject"),
    }
}
/// Answers arrive keyed by question id; Claude keys them by the question text, which is the id.
fn keyed_by_question(answers: &Value, questions: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (id, answer) in answers.as_object().into_iter().flatten() {
        let key = questions.as_array().into_iter().flatten().find(|question| question["id"] == id.as_str()).and_then(|question| question["question"].as_str()).unwrap_or(id);
        out.insert(key.to_owned(), answer.clone());
    }
    Value::Object(out)
}
/// `applyClaudePromptEffortPrefix`
pub(crate) fn apply_effort_prefix(text: &str, effort: Option<&str>) -> String {
    let trimmed = t3_contracts::trim_wire_string(text);
    if trimmed.is_empty() || effort != Some("ultrathink") {
        return trimmed.to_owned();
    }
    let is_command = trimmed.strip_prefix('/').is_some_and(|rest| {
        let token = rest.split_whitespace().next().unwrap_or("");
        !token.is_empty() && !token.contains('/') && (rest.len() == token.len() || rest[token.len()..].starts_with(char::is_whitespace))
    });
    if is_command || trimmed.starts_with("Ultrathink:") {
        return trimmed.to_owned();
    }
    format!("Ultrathink:\n{trimmed}")
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
    let mut reasoning_due: Option<tokio::time::Instant> = None;
    loop {
        if *stopped.borrow() {
            break;
        }
        // Receive the next CLI event only while a process is live.
        let next = async {
            match runtime.as_mut().and_then(|actor| actor.live.as_mut()) {
                Some(live) => live.events.recv().await,
                None => std::future::pending().await,
            }
        };
        let flush = async {
            match reasoning_due {
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
                        if runtime.is_none() && incoming.effect.request["type"] == "background-work.stop" {
                            return Ok(());
                        }
                        if runtime.is_none() {
                            runtime = Some(Actor::connect(store.clone(), &providers, &thread_id, mcp.as_ref()).await?);
                        }
                        runtime.as_mut().unwrap().effect(&incoming.effect).await
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
                reasoning_due = runtime.as_ref().and_then(|actor| actor.turn.as_ref()).filter(|turn| turn.projector.has_pending_reasoning()).map(|_| tokio::time::Instant::now() + REASONING_FLUSH);
            }
            _ = flush => {
                reasoning_due = None;
                if let Some(actor) = runtime.as_mut() {
                    if let Err(cause) = actor.flush_reasoning() { tracing::error!(%cause, "Claude reasoning could not be persisted"); }
                }
            }
            event = next => {
                let actor = runtime.as_mut().unwrap();
                let outcome = actor.handle_event(event);
                match outcome {
                    Ok(true) => { work.close(); break; }
                    Ok(false) => {}
                    Err(cause) => {
                        tracing::error!(%cause, "Claude provider event could not be persisted");
                        let ctx = actor.turn.as_ref().map(|turn| turn.projector.context().clone());
                        let failure = crate::provider_failure::make(&json!({"message":cause.to_string(),"class":"transport_error","retryable":false}));
                        let _ = fail_run_captured(&actor.store, &actor.thread_id, ctx.as_ref().map(|c| c.run_id.as_str()), ctx.as_ref(), &failure);
                        work.close();
                        break;
                    }
                }
                reasoning_due = actor.turn.as_ref().filter(|turn| turn.projector.has_pending_reasoning()).map(|_| reasoning_due.unwrap_or_else(|| tokio::time::Instant::now() + REASONING_FLUSH));
            }
        }
    }
    work.close();
    if let Some(mut actor) = runtime {
        let ending = if *stopped.borrow() || user_cancelled { "interrupted" } else { "failed" };
        let ctx = actor.turn.as_ref().map(|turn| turn.projector.context().clone());
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
            live.process.shutdown().await;
        }
    }
    drop(mcp);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_results_and_prompt_prefix_match_the_unchanged_source_functions() {
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/claude-adapter-functions.json")).unwrap();
        let mut seen = 0;
        for row in rows.iter().filter(|row| row["operation"] == "permission") {
            let suggestions = row["suggestions"].as_array().map(|_| &row["suggestions"]);
            let got = permission_result("Bash", row["decision"].as_str().unwrap(), &json!({"command":"ls"}), "toolu_1", suggestions);
            assert_eq!(serde_json::to_value(got).unwrap(), row["output"], "{row}");
            seen += 1;
        }
        assert_eq!(seen, 8);
        let rows: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/claude-models.json")).unwrap();
        for row in rows.iter().filter(|row| row["operation"] == "promptEffort") {
            assert_eq!(apply_effort_prefix(row["text"].as_str().unwrap(), row["effort"].as_str()), row["output"].as_str().unwrap(), "{row}");
        }
    }
}
