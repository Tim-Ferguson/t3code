//! OpenCode 1.x provider actor: one per thread, over `opencode serve`'s SDK routes (`opencode1_api`). It borrows the
//! instance's server for as long as it lives, reads the server's event stream, turns it into persisted events, and
//! answers permission asks and questions from persisted responses. Mirrors `OpenCodeAdapterV2.ts`; the 2.x adapter
//! (`opencode_adapter`) is a different protocol with its own model of a turn and shares only the server owner, the
//! transport, the text, tool-item and request rows (`opencode_projection`) and the subagent thread helpers.
//!
//! A turn is one `session.promptAsync` with a message id T3 picks; the session's next `idle` ends it. The idle that
//! belongs to an older prompt can still be in flight, so a turn is admitted only once the server both accepted the
//! prompt and showed its user message (`Admission`). A steer is another prompt into the running turn. Permissions
//! are the session's ruleset, set when it is created. Subagents are `task` tool calls whose child sessions become
//! child threads.
use crate::{
    codex_runtime::{ActorLifetime, Retain, Work, commit, fail_run_with, find, settle_queue, terminal_with},
    execution::at,
    opencode1_api::{Client, is_not_found},
    opencode1_projection as p1,
    opencode_adapter::{Models, Terminal, error, failure, str_of, thread_ref, view, workspace},
    opencode_api::{ApiError, EventStream},
    opencode_instance::OpenCodeInstance,
    opencode_projection::{self as projection, Change, DRIVER, Kind, Projector, RequestBody},
    opencode_server::{Connection, Generation},
    persistence::{Effect, Store, StoreError},
    provider_registry::ProviderRegistry,
    provider_turn_context::TurnContext,
    scheduled_tasks::SchedulerClock,
};
use chrono::Utc;
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const ABORT_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const DESCENDANTS_TIMEOUT: Duration = Duration::from_secs(15);
const MCP_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
const COMMAND_LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// A command is admitted once its user message shows or its request ends; neither within this long fails the turn.
const COMMAND_ADMISSION_MS: i64 = 10_000;
/// OpenCode writes a `server.heartbeat` every 10 seconds, so this long without a byte means the server is stuck.
const SILENT_STREAM_MS: i64 = 45_000;
/// How a request from a session T3 does not know yet is retried while its relation to a thread is being learned.
const ROUTE_RETRIES: u32 = 5;
const RECONCILE_ATTEMPTS: u32 = 40;
const RECONCILE_DELAY_MS: i64 = 250;
/// Sessions up the parent chain that are asked about when a request names an unknown one.
const OWNER_HOPS: usize = 5;

// ---- prompt admission ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Signal {
    Accepted,
    AssistantCompleted,
    Idle,
    UserMessage,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Admit {
    Hold,
    ReconcileIdle,
    Release,
}
/// Whether the server has taken a prompt in, so an idle seen meanwhile is not an older prompt's
/// (`advanceOpenCodePromptAdmission`).
#[derive(Default)]
struct Admission {
    message: Option<String>,
    generation: u64,
    pending: bool,
    accepted: bool,
    observed: bool,
    idle_during: bool,
}
impl Admission {
    fn advance(&mut self, signal: Signal) -> Admit {
        if !self.pending {
            return Admit::Release;
        }
        if signal == Signal::AssistantCompleted {
            self.accepted = true;
            self.observed = true;
            self.pending = false;
            return Admit::Release;
        }
        if signal == Signal::Idle {
            self.idle_during = true;
            return Admit::Hold;
        }
        if signal == Signal::Accepted {
            self.accepted = true;
        }
        if signal == Signal::UserMessage {
            self.observed = true;
        }
        if !self.accepted || !self.observed {
            return Admit::Hold;
        }
        if self.idle_during {
            return Admit::ReconcileIdle;
        }
        self.pending = false;
        Admit::Release
    }
}

// ---- state ----

/// `ActiveOpenCodeTurn`.
struct Turn {
    projector: Projector,
    root: bool,
    user_message: Option<String>,
    admission: Admission,
    interrupted: bool,
    plan: Option<String>,
    usage: p1::Usage,
    /// Text and reasoning parts as they stand, for deltas and for the final write.
    parts: IndexMap<String, Value>,
    by_message: HashMap<String, Vec<String>>,
    /// Parts whose block has not been written final.
    open: Vec<String>,
    tool_names: HashMap<String, String>,
    servers: Option<Vec<String>>,
    started_at: String,
    compacted: bool,
    /// A `/compact` request or a command in flight.
    task: Option<tokio::task::JoinHandle<()>>,
}
/// `OpenCodeThreadState`: a native session T3 follows, the thread's own or a subagent's.
struct Thread {
    provider_thread: Value,
    app_thread: String,
    directory: String,
    turn: Option<Turn>,
    roles: HashMap<String, String>,
    users: Vec<String>,
    /// The `task` call (its part id) this session is the subagent of.
    parent: Option<String>,
    child_turns: u64,
    generation: u64,
    runtime_mode: String,
}
/// `OpenCodeSubagentContext`, keyed by the `task` call's part id.
struct Sub {
    parent_session: String,
    ctx: TurnContext,
    selection: Value,
    started_at: String,
    child: Option<String>,
    model: Option<String>,
    result: Option<String>,
}
struct Pending {
    native: String,
    /// The session that asked, which may be a subagent's.
    session: String,
    /// The thread's own session whose turn shows the request.
    owner: String,
    question: Option<Value>,
}
/// A request waiting for its session's relation to a thread to be known.
struct Route {
    session: String,
    question: bool,
    value: Value,
    tries: u32,
}

/// How a prompt is sent.
enum Submit {
    Prompt,
    Command(Value),
}
enum Frame {
    Event(Value),
    Closed,
    /// The spawned server exited.
    Exited,
    /// A command or compaction request ended.
    Done { session: String, message: Option<String>, generation: u64, result: Result<(), ApiError>, compaction: bool },
    /// A command's admission window ended.
    Admission { session: String, message: String, generation: u64 },
    Route(String),
}
struct Live {
    conn: Connection,
    client: Client,
    frames: mpsc::UnboundedReceiver<Frame>,
    sender: mpsc::UnboundedSender<Frame>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.tasks.drain(..).for_each(|task| task.abort());
    }
}
/// Reads the stream until it ends or goes silent, handing every event to the actor.
fn start_reader(mut stream: EventStream, clock: Arc<dyn SchedulerClock>, sender: mpsc::UnboundedSender<Frame>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let silent = clock.sleep_until(clock.now_ms() + SILENT_STREAM_MS);
            tokio::select! {
                next = stream.next() => match next {
                    Ok(Some(event)) => {
                        if sender.send(Frame::Event(event)).is_err() {
                            return;
                        }
                    }
                    _ => break,
                },
                _ = silent => break,
            }
        }
        let _ = sender.send(Frame::Closed);
    })
}

struct Actor {
    store: Store,
    thread_id: String,
    session_id: String,
    instance: OpenCodeInstance,
    providers: ProviderRegistry,
    clock: Arc<dyn SchedulerClock>,
    cwd: PathBuf,
    session: Value,
    live: Option<Live>,
    /// In the order the sessions were met, which is the order a closing server's sessions are stopped in.
    threads: IndexMap<String, Thread>,
    subs: IndexMap<String, Sub>,
    by_child: HashMap<String, String>,
    /// A session of this runtime that is not a thread's own, to the thread session it runs under.
    owners: HashMap<String, String>,
    /// Sessions OpenCode reports busy.
    busy: HashSet<String>,
    pending: IndexMap<String, Pending>,
    pending_native: HashMap<String, String>,
    settled: HashSet<String>,
    routes: HashMap<String, Route>,
    mcp: Option<crate::mcp_sessions::ProviderSessionConfig>,
    /// The name T3's MCP server is registered under, once it is.
    mcp_name: Option<String>,
    has_connected: bool,
    stream_failure: Option<String>,
    detached: bool,
    /// Prompt messages whose request is a command in flight.
    commands: HashSet<String>,
}

fn parts_text(parts: &[Value]) -> String {
    parts.iter().filter(|part| part["type"] == "text").filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n").trim().to_owned()
}
fn error_message(properties: &Value) -> String {
    let error = &properties["error"];
    if error.is_null() {
        return "OpenCode session failed without an error payload.".into();
    }
    error["data"]["message"].as_str().filter(|message| !message.trim().is_empty()).or_else(|| error["name"].as_str()).unwrap_or("OpenCode session failed.").to_owned()
}
fn is_aborted(properties: &Value) -> bool {
    properties["error"]["name"] == "MessageAbortedError"
}
fn failed_with(cause: &ApiError) -> Terminal {
    Terminal::Failed(failure(&cause.to_string(), "provider_error"))
}
fn file_url(path: &std::path::Path) -> Option<String> {
    url::Url::from_file_path(path).ok().map(|url| url.to_string())
}

impl Actor {
    async fn connect(store: Store, providers: &ProviderRegistry, thread_id: &str, mcp: Option<&crate::provider_mcp::CredentialLease>) -> Result<Self, String> {
        let thread = view(&store, thread_id).map_err(|e| e.to_string())?;
        let instance_id = thread["thread"]["modelSelection"]["instanceId"].as_str().ok_or("Thread has no provider instance.")?;
        let instance = providers.opencode(instance_id).map_err(|e| e.to_string())?;
        let cwd = workspace(&store, thread_id)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let capabilities: Value = serde_json::from_str(include_str!("opencode1-capabilities.json")).unwrap();
        let session = json!({"id":session_id,"driver":DRIVER,"providerInstanceId":instance_id,"status":"ready","cwd":cwd,"model":thread["thread"]["modelSelection"]["model"],"capabilities":capabilities,"createdAt":now,"updatedAt":now,"lastError":null});
        let clock = providers.idle_release().clock();
        let mut actor = Self { store, thread_id: thread_id.into(), session_id, instance, providers: providers.clone(), clock, cwd, session, live: None, threads: IndexMap::new(), subs: IndexMap::new(), by_child: HashMap::new(), owners: HashMap::new(), busy: HashSet::new(), pending: IndexMap::new(), pending_native: HashMap::new(), settled: HashSet::new(), routes: HashMap::new(), mcp: mcp.map(|lease| lease.config.clone()), mcp_name: None, has_connected: false, stream_failure: None, detached: false, commands: HashSet::new() };
        // The session borrows the server for as long as it is open. T3's MCP server is added before the stream is
        // subscribed, and the stream before any session or prompt call, so no event of theirs is missed.
        let connection = actor.instance.connect(&actor.cwd, actor.clock.clone()).await.map_err(|e| e.to_string())?;
        if connection.generation != Generation::V1 {
            return Err("This OpenCode server is not 1.x.".into());
        }
        actor.register_mcp(&connection).await?;
        actor.open_stream(connection).await.map_err(|e| e.to_string())?;
        let session = actor.session.clone();
        commit(&actor.store, thread_id, |_| Ok(vec![("provider-session.attached", session.clone())])).map_err(|e| e.to_string())?;
        Ok(actor)
    }
    /// T3's tools, for a server T3 started: an external one may not reach T3's endpoint. The server is registered under
    /// this thread's own name, and the sessions' rules allow only it, so another thread's tools stay out of reach.
    async fn register_mcp(&mut self, connection: &Connection) -> Result<(), String> {
        let (Some(config), false) = (self.mcp.clone(), connection.external) else { return Ok(()) };
        let name = crate::opencode_adapter::mcp_server_name(&self.thread_id);
        let client = Client::new(&connection.api, &self.cwd.to_string_lossy());
        let body = json!({"type":"remote","url":config.endpoint,"headers":{"Authorization":config.authorization_header},"oauth":false});
        client.mcp_add(&name, &body).await.map_err(|cause| cause.to_string())?;
        self.mcp_name = Some(name);
        Ok(())
    }
    async fn open_stream(&mut self, conn: Connection) -> Result<(), ApiError> {
        let client = Client::new(&conn.api, &self.cwd.to_string_lossy());
        let stream = client.subscribe().await?;
        let (sender, frames) = mpsc::unbounded_channel();
        let mut tasks = vec![start_reader(stream, self.clock.clone(), sender.clone())];
        if let Some(lease) = conn.lease.clone() {
            let exits = sender.clone();
            tasks.push(tokio::spawn(async move {
                lease.exited().await;
                let _ = exits.send(Frame::Exited);
            }));
        }
        self.live = Some(Live { conn, client, frames, sender, tasks });
        Ok(())
    }
    fn client(&self) -> Result<&Client, ApiError> {
        self.live.as_ref().map(|live| &live.client).ok_or_else(|| ApiError::Transport("the OpenCode session is closed".into()))
    }
    fn external(&self) -> bool {
        self.live.as_ref().is_some_and(|live| live.conn.external)
    }
    fn commit_to(&self, thread: &str, events: projection::Events) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        commit(&self.store, thread, |_| Ok(events))
    }

    async fn effect(&mut self, effect: &Effect) -> Result<(), String> {
        match effect.request["type"].as_str() {
            Some("provider-turn.start") => self.start_turn(effect.request["runId"].as_str().ok_or("Run id missing.")?).await,
            Some("provider-turn.interrupt") => self.interrupt(effect.request["runId"].as_str()).await,
            Some("provider-turn.steer") => self.steer(effect).await,
            Some("runtime-request.respond") => self.respond(effect).await,
            Some("provider-session.detach") => self.detach(effect).await,
            Some("provider-thread.rollback") => self.rollback(effect).await,
            Some("background-work.stop") => self.stop_background().await,
            Some("provider-turn.restart") => Err("OpenCode steers a running turn natively and does not restart it.".into()),
            _ => Err("Unsupported OpenCode provider effect.".into()),
        }
    }

    // ---- the thread's session ----

    fn rules(&self, runtime_mode: &str) -> Value {
        let mut rules = p1::permission_rules(runtime_mode);
        rules.extend(p1::mcp_rules(self.mcp_name.as_deref()));
        json!(rules)
    }
    fn register(&mut self, session: &str, provider_thread: Value, runtime_mode: &str) {
        let app_thread = self.thread_id.clone();
        let directory = self.cwd.to_string_lossy().into_owned();
        let parent = self.by_child.get(session).cloned();
        let entry = self.threads.entry(session.to_owned()).or_insert_with(|| Thread { provider_thread: provider_thread.clone(), app_thread, directory, turn: None, roles: HashMap::new(), users: vec![], parent, child_turns: 0, generation: 1, runtime_mode: runtime_mode.to_owned() });
        entry.provider_thread = provider_thread;
        entry.runtime_mode = runtime_mode.to_owned();
    }
    /// Creates the native session for a thread that has none.
    async fn ensure_thread(&mut self, provider_thread: &Value, runtime_mode: &str) -> Result<String, String> {
        let rules = self.rules(runtime_mode);
        let created = self.client().map_err(|e| e.to_string())?.session_create(&rules).await.map_err(|e| e.to_string())?;
        let id = str_of(&created["id"]);
        if id.is_empty() {
            return Err("OpenCode returned no response payload for session.create.".into());
        }
        self.register(&id, provider_thread.clone(), runtime_mode);
        Ok(id)
    }
    /// Loads a thread whose native session exists. The session keeps the rules it was made with, and the thread's mode may
    /// have changed since, so the current mode's rules are written when the session is first loaded here.
    async fn resume_thread(&mut self, session_id: &str, provider_thread: &Value, runtime_mode: &str) -> Result<(), String> {
        let client = self.client().map_err(|e| e.to_string())?.clone();
        client.session_get(session_id).await.map_err(|e| e.to_string())?;
        self.register(session_id, provider_thread.clone(), runtime_mode);
        let rules = self.rules(runtime_mode);
        client.session_update(session_id, &rules).await.map_err(|e| e.to_string())?;
        Ok(())
    }
    /// `forkThread`: OpenCode forks whatever history the source has, cut before the message the first turn after the fork
    /// point prompted with.
    async fn fork_thread(&mut self, provider_thread: &Value, transfer: &Value, run_id: &str, runtime_mode: &str) -> Result<String, String> {
        let source_view = crate::codex_runtime::projection(&self.store, transfer["sourceThreadId"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
        let rows = |field: &str| source_view[field].as_array().cloned().unwrap_or_default();
        let source_thread = rows("providerThreads").into_iter().find(|thread| thread["id"] == provider_thread["forkedFrom"]["providerThreadId"]).ok_or_else(|| format!("Native fork transfer {} has no source provider execution.", str_of(&transfer["id"])))?;
        let source = source_thread["nativeThreadRef"]["nativeId"].as_str().ok_or("The source provider thread has no OpenCode session.")?.to_owned();
        if self.threads.get(&source).is_some_and(|thread| thread.turn.is_some()) {
            return Err(format!("Cannot fork OpenCode thread {} while a turn is active", str_of(&source_thread["id"])));
        }
        let turns: Vec<Value> = rows("providerTurns").into_iter().filter(|turn| turn["providerThreadId"] == source_thread["id"]).collect();
        let before = match provider_thread["forkedFrom"].get("providerTurnId").filter(|id| !id.is_null()) {
            None => None,
            Some(id) => {
                if !turns.iter().any(|turn| turn["id"] == *id) {
                    return Err(format!("OpenCode fork boundary turn {} was not found", str_of(id)));
                }
                p1::boundary_after_turn(&turns, id)
            }
        };
        let client = self.client().map_err(|e| e.to_string())?.clone();
        let forked = client.fork(&source, before.as_deref()).await.map_err(|e| e.to_string())?;
        let id = str_of(&forked["id"]);
        if id.is_empty() {
            return Err("OpenCode returned no response payload for session.fork.".into());
        }
        self.register(&id, provider_thread.clone(), runtime_mode);
        // The fork keeps the source's rules, which are not necessarily this thread's.
        let rules = self.rules(runtime_mode);
        client.session_update(&id, &rules).await.map_err(|e| e.to_string())?;
        let instance = str_of(&self.session["providerInstanceId"]);
        let consumed = crate::context_transfer::consume_native_fork(transfer, &instance, run_id, &thread_ref(&id), Utc::now());
        commit(&self.store, &self.thread_id, |_| Ok(vec![("context-transfer.updated", consumed.clone())])).map_err(|e| e.to_string())?;
        Ok(id)
    }

    // ---- starting a turn ----

    async fn start_turn(&mut self, run_id: &str) -> Result<(), String> {
        if self.detached {
            return Err("The OpenCode session was released; the next effect starts a new one.".into());
        }
        if let Some(message) = self.stream_failure.clone() {
            return Err(message);
        }
        let thread_view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let run = find(&thread_view, "runs", &json!(run_id)).map_err(|e| e.to_string())?.clone();
        if run["status"] != "starting" {
            return Err("Target run is no longer starting.".into());
        }
        if run["providerInstanceId"] != self.session["providerInstanceId"] {
            return Err("Native provider handoff is not yet available.".into());
        }
        let selection = run["modelSelection"].clone();
        let provider_thread = find(&thread_view, "providerThreads", &run["providerThreadId"]).map_err(|e| e.to_string())?.clone();
        let message = find(&thread_view, "messages", &run["userMessageId"]).map_err(|e| e.to_string())?.clone();
        let thread = &thread_view["thread"];
        let runtime_mode = thread["runtimeMode"].as_str().unwrap_or("approval-required").to_owned();
        let interaction_mode = thread["interactionMode"].as_str().unwrap_or("default").to_owned();
        let attempt = find(&thread_view, "attempts", &run["activeAttemptId"]).map_err(|e| e.to_string())?.clone();
        let attempt_id = str_of(&attempt["id"]);

        let saved = provider_thread["nativeThreadRef"]["nativeId"].as_str().map(str::to_owned);
        let fork_transfer = if saved.is_none() && !provider_thread["forkedFrom"].is_null() { crate::context_transfer::native_fork_transfer(&thread_view, &json!(run_id)).cloned() } else { None };
        let session_id = match saved {
            None if fork_transfer.is_some() => self.fork_thread(&provider_thread, fork_transfer.as_ref().unwrap(), run_id, &runtime_mode).await?,
            None => self.ensure_thread(&provider_thread, &runtime_mode).await?,
            Some(id) => {
                if !self.threads.contains_key(&id) {
                    self.resume_thread(&id, &provider_thread, &runtime_mode).await?;
                }
                id
            }
        };
        if self.threads[&session_id].turn.is_some() {
            return Err(format!("OpenCode provider thread {} already has an active turn", str_of(&provider_thread["id"])));
        }
        self.threads.get_mut(&session_id).unwrap().runtime_mode = runtime_mode.clone();
        let text = message["text"].as_str().unwrap_or("").trim().to_owned();
        let bare = message["attachments"].as_array().is_none_or(Vec::is_empty);
        let compaction = bare && text == "/compact";
        let parts = if compaction { vec![] } else { self.prompt_parts(&message)? };
        if !compaction && parts.is_empty() {
            return Err("OpenCode turns require text or at least one valid attachment".into());
        }
        let model = Models::parse_slug(selection["model"].as_str().unwrap_or(""));
        let admission = (!compaction && model.is_some()).then(p1::new_message_id);
        let native_turn = format!("{session_id}:attempt:{attempt_id}");
        let Some((provider_id, model_id)) = model else {
            // A turn T3 will not run still starts and fails, so the refusal is what the user reads.
            self.begin_turn(&session_id, run_id, &selection, &native_turn, None, false).map_err(|e| e.to_string())?;
            let refusal = format!("OpenCode model '{}' must use provider/model format", str_of(&selection["model"]));
            return self.finish_turn(&session_id, Terminal::Failed(failure(&refusal, "validation_error")), false).await.map_err(|e| e.to_string());
        };
        if compaction {
            self.begin_turn(&session_id, run_id, &selection, &native_turn, None, true).map_err(|e| e.to_string())?;
            self.compact(&session_id, &provider_id, &model_id);
            return Ok(());
        }
        let hint = selection["options"].as_array().into_iter().flatten().find(|option| option["id"] == "agent").and_then(|option| option["value"].as_str());
        let agent = hint.map(str::to_owned).or_else(|| (interaction_mode == "plan").then(|| "plan".to_owned()));
        let variant = selection["options"].as_array().into_iter().flatten().find(|option| option["id"] == "variant").and_then(|option| option["value"].as_str().map(str::to_owned));
        let mut system = vec![];
        if self.mcp_name.is_some() {
            system.push(crate::provider_instructions::codex_context("", "", false, false)["t3_code_orchestration"]["value"].as_str().unwrap_or("").to_owned());
        }
        system.push(crate::provider_instructions::runtime("OpenCode", selection["model"].as_str().unwrap_or(""), None, None));
        let system = system.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
        let mut body = json!({"messageID":admission,"model":{"providerID":provider_id,"modelID":model_id},"system":system,"parts":parts});
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        if let Some(variant) = variant {
            body["variant"] = json!(variant);
        }
        // A prompt is sent before the run is shown running, so a run that shows running has been accepted by OpenCode and a
        // Stop that reaches it has a session to stop. A command waits for its generation, so its turn is installed first.
        match self.plan(&body).await {
            Submit::Command(command) => {
                self.begin_turn(&session_id, run_id, &selection, &native_turn, admission.clone(), false).map_err(|e| e.to_string())?;
                self.send_command(&session_id, str_of(&body["messageID"]), command).map_err(|cause| cause.to_string())
            }
            Submit::Prompt => {
                let sent = self.client().map_err(|e| e.to_string())?.prompt_async(&session_id, &body).await;
                if let Err(refused) = self.begin_turn(&session_id, run_id, &selection, &native_turn, admission.clone(), false) {
                    // The run was stopped while the prompt was on its way: nothing shows it, so it is stopped on the server.
                    if sent.is_ok() {
                        if let Ok(client) = self.client() {
                            let _ = client.abort(&session_id, Some(ABORT_TIMEOUT)).await;
                        }
                    }
                    return Err(refused.to_string());
                }
                match sent {
                    Ok(_) => self.admit(&session_id, Signal::Accepted).await.map_err(|e| e.to_string()),
                    Err(cause) => self.finish_turn(&session_id, failed_with(&cause), false).await.map_err(|e| e.to_string()),
                }
            }
        }
    }
    /// The prompt's parts: the text with its attachment notes, then each attachment OpenCode can hand a model as a file.
    fn prompt_parts(&self, message: &Value) -> Result<Vec<Value>, String> {
        let text = crate::attachment_prompt::provider_text(&self.store, message).trim().to_owned();
        let mut parts = if text.is_empty() { vec![] } else { vec![json!({"type":"text","text":text})] };
        let directory = self.store.state_dir().map(|directory| directory.join("attachments"));
        for attachment in message["attachments"].as_array().into_iter().flatten() {
            // Pasted text rides in the prompt.
            if attachment["type"] == "file" && attachment["source"]["_tag"] == "pasted-text" {
                continue;
            }
            let mime = attachment["mimeType"].as_str().unwrap_or("").trim().to_lowercase();
            let native = attachment["sizeBytes"].as_u64().unwrap_or(0) <= 20 * 1024 * 1024 && (matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "application/pdf") || mime.starts_with("text/"));
            if !native {
                continue;
            }
            let path = directory.as_ref().zip(crate::attachment_store::AttachmentRef::from_value(attachment)).and_then(|(directory, reference)| crate::attachment_store::resolve_attachment_path(directory, &reference));
            if let Some(url) = path.as_deref().and_then(file_url) {
                parts.push(json!({"type":"file","mime":attachment["mimeType"],"filename":attachment["name"],"url":url}));
            }
        }
        Ok(parts)
    }
    /// Installs a turn T3 started; every path after it ends the turn with a terminal.
    fn begin_turn(&mut self, session_id: &str, run_id: &str, selection: &Value, native_turn: &str, admission: Option<String>, compaction: bool) -> Result<(), StoreError> {
        let provider_turn_id = uuid::Uuid::new_v4().to_string();
        let mut captured = TurnContext::capture(&view(&self.store, &self.thread_id)?, run_id)?;
        let now = at(Utc::now());
        let session = self.session_id.clone();
        let native = native_turn.to_owned();
        commit(&self.store, &self.thread_id, |view| {
            if !captured.is_current(view) {
                return Err(error("Native start belongs to a replaced attempt."));
            }
            let (mut run, attempt) = captured.rows(view)?;
            if run["status"] != "starting" {
                return Err(error("Run was interrupted before the OpenCode turn started."));
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
            thread["providerSessionId"] = json!(session);
            thread["nativeThreadRef"] = thread_ref(session_id);
            thread["status"] = json!("active");
            if thread["firstRunOrdinal"].is_null() {
                thread["firstRunOrdinal"] = run["ordinal"].clone();
            }
            thread["lastRunOrdinal"] = run["ordinal"].clone();
            thread["updatedAt"] = json!(now);
            let turn = json!({"id":provider_turn_id,"providerThreadId":run["providerThreadId"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":{"driver":DRIVER,"nativeId":native,"strength":"weak"},"ordinal":run["ordinal"],"status":"running","startedAt":now,"completedAt":null});
            Ok(vec![("provider-thread.updated", thread), ("provider-turn.updated", turn), ("run.updated", run), ("run-attempt.updated", attempt), ("node.updated", node)])
        })?;
        captured.provider_turn_id = json!(provider_turn_id);
        let view = view(&self.store, &self.thread_id)?;
        let mut projector = Projector::new(&self.thread_id, captured, &view, selection.clone(), "", None)?;
        projector.stream_paragraphs(self.streams_paragraphs(&view));
        let thread = self.threads.get_mut(session_id).unwrap();
        let mut usage = p1::Usage::new();
        if let Some(message) = &admission {
            usage.prompts.insert(message.clone());
        }
        thread.generation += 1;
        let generation = thread.generation;
        thread.turn = Some(Turn { projector, root: true, user_message: None, admission: Admission { message: admission, generation, pending: !compaction, accepted: compaction, observed: false, idle_during: false }, interrupted: false, plan: None, usage, parts: IndexMap::new(), by_message: HashMap::new(), open: vec![], tool_names: HashMap::new(), servers: None, started_at: now, compacted: false, task: None });
        Ok(())
    }
    /// The thread's project may set its own response streaming mode over the environment's.
    fn streams_paragraphs(&self, view: &Value) -> bool {
        let settings = self.providers.settings();
        let project: Option<t3_contracts::ProjectId> = view["thread"]["projectId"].as_str().and_then(|id| id.parse().ok());
        let mode = project.and_then(|project| settings.project_settings_overrides.get(&project).and_then(|entry| entry.response_streaming_mode)).unwrap_or(settings.response_streaming_mode);
        mode == t3_contracts::ResponseStreamingMode::Paragraph
    }
    /// The session stays `ready` while it lives: a response to a request is accepted only for a ready session, as for
    /// the other adapters. A broken stream marks it `error`.
    fn set_session_status(&mut self, status: &str, last_error: Option<&str>) -> Result<(), StoreError> {
        if status == "running" || status == "waiting" || status == "ready" && last_error.is_none() {
            return Ok(());
        }
        let session_id = self.session_id.clone();
        let now = at(Utc::now());
        self.session["status"] = json!(status);
        self.session["lastError"] = json!(last_error);
        self.session["updatedAt"] = json!(now);
        let row = self.session.clone();
        commit(&self.store, &self.thread_id, |view| Ok(if find(view, "providerSessions", &json!(session_id)).is_ok() { vec![("provider-session.updated", row.clone())] } else { vec![] }))
    }

    /// `submitPrompt`'s choice: `/name args` naming one of the workspace's commands is a command, which waits for the
    /// generation and so runs beside the stream; anything else is `promptAsync`.
    async fn plan(&self, body: &Value) -> Submit {
        let Ok(client) = self.client() else { return Submit::Prompt };
        let text = parts_text(body["parts"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        let Some((name, arguments)) = crate::opencode_adapter::command_of(&text) else { return Submit::Prompt };
        let commands = client.command_list(COMMAND_LIST_TIMEOUT).await.ok().and_then(|list| list.as_array().cloned()).unwrap_or_default();
        if !commands.iter().any(|entry| entry["name"] == name.as_str()) {
            return Submit::Prompt;
        }
        let model = body.get("model").map(|model| format!("{}/{}", str_of(&model["providerID"]), str_of(&model["modelID"])));
        let mut command = json!({"messageID":body["messageID"],"command":name,"arguments":arguments,"parts":body["parts"].as_array().into_iter().flatten().filter(|part| part["type"] == "file").cloned().collect::<Vec<_>>()});
        if let Some(model) = model {
            command["model"] = json!(model);
        }
        for key in ["agent", "variant"] {
            if let Some(value) = body.get(key) {
                command[key] = value.clone();
            }
        }
        Submit::Command(command)
    }
    /// Sends a steer into the running turn. The answer says whether the server has accepted the prompt (a command is
    /// accepted when its user message shows or its request ends).
    async fn submit(&mut self, session_id: &str, body: Value) -> Result<bool, ApiError> {
        match self.plan(&body).await {
            Submit::Command(command) => {
                self.send_command(session_id, str_of(&body["messageID"]), command)?;
                Ok(false)
            }
            Submit::Prompt => {
                self.client()?.prompt_async(session_id, &body).await?;
                Ok(true)
            }
        }
    }
    fn send_command(&mut self, session_id: &str, message: String, command: Value) -> Result<(), ApiError> {
        let client = self.client()?.clone();
        let sender = self.live.as_ref().unwrap().sender.clone();
        let generation = self.threads[session_id].turn.as_ref().map_or(0, |turn| turn.admission.generation);
        self.commands.insert(message.clone());
        let (session, id) = (session_id.to_owned(), message.clone());
        let done = sender.clone();
        let task = tokio::spawn(async move {
            let result = client.command(&session, &command).await.map(|_| ());
            let _ = done.send(Frame::Done { session, message: Some(id), generation, result, compaction: false });
        });
        let (clock, session, window) = (self.clock.clone(), session_id.to_owned(), message);
        let timer = tokio::spawn(async move {
            clock.sleep_until(clock.now_ms() + COMMAND_ADMISSION_MS).await;
            let _ = sender.send(Frame::Admission { session, message: window, generation });
        });
        let turn = self.threads.get_mut(session_id).and_then(|thread| thread.turn.as_mut()).unwrap();
        turn.task = Some(task);
        if let Some(live) = self.live.as_mut() {
            live.tasks.push(timer);
        }
        Ok(())
    }
    /// `session.summarize`: a compaction runs beside the stream too.
    fn compact(&mut self, session_id: &str, provider: &str, model: &str) {
        let (Ok(client), Some(live)) = (self.client().cloned(), self.live.as_ref()) else { return };
        let sender = live.sender.clone();
        let (session, body) = (session_id.to_owned(), json!({"providerID":provider,"modelID":model,"auto":false}));
        let task = tokio::spawn(async move {
            let result = client.summarize(&session, &body).await.map(|_| ());
            let _ = sender.send(Frame::Done { session, message: None, generation: 0, result, compaction: true });
        });
        if let Some(turn) = self.threads.get_mut(session_id).and_then(|thread| thread.turn.as_mut()) {
            turn.task = Some(task);
        }
    }

    // ---- steering, interruption, requests ----

    async fn steer(&mut self, effect: &Effect) -> Result<(), String> {
        let thread_view = view(&self.store, &self.thread_id).map_err(|e| e.to_string())?;
        let message = find(&thread_view, "messages", &effect.request["messageId"]).map_err(|e| e.to_string())?.clone();
        if !crate::provider_turn_context::steering_delivery_is_current(&thread_view, &message) || effect.request["providerSessionId"] != self.session_id {
            return Ok(());
        }
        let provider_turn = find(&thread_view, "providerTurns", &effect.request["providerTurnId"]).map_err(|e| e.to_string())?;
        if provider_turn["status"] == "completed" {
            return crate::execution::follow_up_completed_steer(&self.store, effect).map(|_| ()).map_err(|e| e.to_string());
        }
        let provider_turn_id = effect.request["providerTurnId"].clone();
        let Some(session_id) = self.threads.iter().find(|(_, thread)| thread.turn.as_ref().is_some_and(|turn| turn.projector.context().provider_turn_id == provider_turn_id && !turn.interrupted)).map(|(id, _)| id.clone()) else {
            return Err(format!("OpenCode turn {} is not active", str_of(&provider_turn_id)));
        };
        let selection = self.threads[&session_id].turn.as_ref().unwrap().projector.selection().clone();
        let Some((provider_id, model_id)) = Models::parse_slug(selection["model"].as_str().unwrap_or("")) else {
            return Err(format!("OpenCode model '{}' must use provider/model format", str_of(&selection["model"])));
        };
        let parts = self.prompt_parts(&message)?;
        if parts.is_empty() {
            return Err("OpenCode steering requires text or an attachment".into());
        }
        let id = p1::new_message_id();
        let thread = self.threads.get_mut(&session_id).unwrap();
        thread.generation += 1;
        let generation = thread.generation;
        let turn = thread.turn.as_mut().unwrap();
        turn.admission = Admission { message: Some(id.clone()), generation, pending: true, accepted: false, observed: false, idle_during: false };
        turn.usage.prompts.insert(id.clone());
        let body = json!({"messageID":id,"model":{"providerID":provider_id,"modelID":model_id},"parts":parts});
        let accepted = self.submit(&session_id, body).await.map_err(|cause| cause.to_string())?;
        if accepted && self.threads.get(&session_id).is_some_and(|thread| thread.turn.is_some()) {
            self.admit(&session_id, Signal::Accepted).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    /// A Stop: the turn is marked, its admission and in-flight request are cancelled, the session is aborted and so are
    /// the sessions under it (an abort does not reach a child).
    async fn interrupt(&mut self, run_id: Option<&str>) -> Result<(), String> {
        let Some(session_id) = self.threads.iter().find(|(_, thread)| thread.turn.as_ref().is_some_and(|turn| Some(turn.projector.context().run_id.as_str()) == run_id)).map(|(id, _)| id.clone()) else {
            return Err("Target run is no longer active.".into());
        };
        let (cancelled_request, pending) = {
            let thread = self.threads.get_mut(&session_id).unwrap();
            thread.generation += 1;
            let generation = thread.generation;
            let turn = thread.turn.as_mut().unwrap();
            turn.interrupted = true;
            let pending = turn.admission.pending;
            turn.admission.generation = generation;
            turn.admission.pending = false;
            let cancelled = turn.task.take().inspect(|task| task.abort()).is_some();
            (cancelled, pending)
        };
        // A command request that was cancelled leaves its turn to end on the session's idle; a compaction's ends here.
        let compaction = cancelled_request && self.threads[&session_id].turn.as_ref().is_some_and(|turn| turn.admission.message.is_none());
        let _ = pending;
        let client = self.client().map_err(|e| e.to_string())?.clone();
        if let Err(cause) = client.abort(&session_id, Some(ABORT_TIMEOUT)).await {
            // The turn can settle while the abort is in flight, and aborting an idle session fails. That Stop still
            // succeeded; only a turn that is genuinely still running reports the failure.
            if self.threads.get(&session_id).and_then(|thread| thread.turn.as_ref()).is_some() {
                self.threads.get_mut(&session_id).unwrap().turn.as_mut().unwrap().interrupted = false;
                return Err(cause.to_string());
            }
        }
        if compaction {
            self.finish_turn(&session_id, Terminal::Interrupted, false).await.map_err(|e| e.to_string())?;
        }
        self.abort_descendants(&session_id).await.map_err(|cause| cause.to_string())
    }
    /// Aborts every session under `root`, through any nesting, and reports the first failure (`abortDescendants`).
    async fn abort_descendants(&self, root: &str) -> Result<(), ApiError> {
        let client = self.client()?.clone();
        let walk = async {
            let mut visited: HashSet<String> = HashSet::from([root.to_owned()]);
            let mut queue: Vec<(String, bool)> = vec![(root.to_owned(), false)];
            let mut first: Option<ApiError> = None;
            while let Some((session, abort)) = queue.pop() {
                if abort {
                    if let Err(cause) = client.abort(&session, None).await {
                        if !is_not_found(&cause) {
                            first.get_or_insert(cause);
                        }
                    }
                }
                match client.session_children(&session, None).await {
                    Ok(children) => {
                        for child in children.as_array().into_iter().flatten() {
                            let id = str_of(&child["id"]);
                            if !id.is_empty() && visited.insert(id.clone()) {
                                queue.push((id, true));
                            }
                        }
                    }
                    Err(cause) if is_not_found(&cause) => {}
                    Err(cause) => return Err(first.unwrap_or(cause)),
                }
            }
            first.map_or(Ok(()), Err)
        };
        tokio::time::timeout(DESCENDANTS_TIMEOUT, walk).await.unwrap_or_else(|_| Err(ApiError::Transport("the request timed out".into())))
    }
    /// `runtime-request.respond`: the persisted decision or answers go to OpenCode; the request settles when OpenCode says
    /// it was answered.
    async fn respond(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id {
            return Err("Runtime callback belongs to a different provider session.".into());
        }
        let request_id = effect.request["requestId"].as_str().ok_or("Request id missing.")?.to_owned();
        let Some(entry) = self.pending.get(&request_id) else { return Err(format!("No pending OpenCode request {request_id}")) };
        let native = entry.native.clone();
        let client = self.client().map_err(|e| e.to_string())?.clone();
        if let Some(question) = &entry.question {
            let Some(answers) = effect.request.get("answers").filter(|answers| answers.is_object()) else { return Err(format!("OpenCode question request {request_id} requires answers")) };
            let answers = p1::question_answers(question, answers);
            return client.question_reply(&native, &answers).await.map(|_| ()).map_err(|cause| cause.to_string());
        }
        let Some(decision) = effect.request["decision"].as_str() else { return Err(format!("OpenCode approval request {request_id} requires a decision")) };
        client.permission_reply(&native, p1::permission_reply(decision)).await.map(|_| ()).map_err(|cause| cause.to_string())
    }

    // ---- rollback ----

    /// `rollbackThread`: OpenCode forks the session before the first message of the turn after the one kept, and the
    /// thread continues in the fork. Files stay where they are: T3 restores its own checkpoint when the user asked.
    async fn rollback(&mut self, effect: &Effect) -> Result<(), String> {
        let prepared = crate::checkpoint_rollback::prepare(&self.store, effect)?;
        let provider_thread = if prepared.needs_provider() {
            let session_id = prepared.provider_thread["nativeThreadRef"]["nativeId"].as_str().ok_or_else(|| format!("Provider thread {} is missing a native OpenCode session id.", str_of(&prepared.provider_thread["id"])))?.to_owned();
            if self.threads.get(&session_id).is_some_and(|thread| thread.turn.is_some()) {
                return Err(format!("Cannot roll back OpenCode thread {} while a turn is active", str_of(&prepared.provider_thread["id"])));
            }
            let client = self.client().map_err(|e| e.to_string())?.clone();
            let messages = client.session_messages(&session_id).await.map_err(|e| e.to_string())?;
            let messages = messages.as_array().cloned().unwrap_or_default();
            let boundary = if prepared.target["type"] == "thread_start" {
                messages.iter().find(|entry| entry["info"]["role"] == "user").and_then(|entry| entry["info"]["id"].as_str().map(str::to_owned))
            } else {
                p1::boundary_after_turn(&prepared.provider_thread_turns, &prepared.target["providerTurn"]["id"])
            };
            let mut retained = prepared.provider_thread.clone();
            let mut retained_session = session_id.clone();
            if let Some(boundary) = boundary {
                let index = messages.iter().position(|entry| entry["info"]["id"] == boundary.as_str()).ok_or("The OpenCode rewind boundary is no longer available.")?;
                let fork = client.fork(&session_id, Some(&boundary)).await.map_err(|e| e.to_string())?;
                let fork_id = str_of(&fork["id"]);
                let kept = client.session_messages(&fork_id).await.map_err(|e| e.to_string())?;
                if kept.as_array().map_or(0, Vec::len) != index {
                    return Err("OpenCode did not preserve the requested rewind boundary.".into());
                }
                let mode = self.threads.get(&session_id).map_or_else(|| "approval-required".to_owned(), |thread| thread.runtime_mode.clone());
                let rules = self.rules(&mode);
                client.session_update(&fork_id, &rules).await.map_err(|e| e.to_string())?;
                retained["nativeThreadRef"] = thread_ref(&fork_id);
                self.register(&fork_id, retained.clone(), &mode);
                retained_session = fork_id;
            }
            let mut snapshot = self.snapshot_thread(&retained, &retained_session).await.map_err(|e| e.to_string())?;
            snapshot["nativeConversationHeadRef"] = if prepared.target["type"] == "provider_turn" { prepared.target["providerTurn"]["nativeTurnRef"].clone() } else { Value::Null };
            snapshot
        } else {
            prepared.provider_thread.clone()
        };
        crate::checkpoint_rollback::finish(&self.store, effect, prepared, provider_thread).await
    }
    /// The session's history as a provider thread row: the head is the last user message OpenCode kept. OpenCode keeps a
    /// reverted session's removed messages, so reading stops at its revert marker.
    async fn snapshot_thread(&self, provider_thread: &Value, session_id: &str) -> Result<Value, ApiError> {
        let client = self.client()?;
        let native = client.session_get(session_id).await?;
        let history = client.session_messages(session_id).await?;
        let revert = native["revert"]["messageID"].as_str();
        let kept: Vec<&Value> = history.as_array().into_iter().flatten().take_while(|entry| Some(entry["info"]["id"].as_str().unwrap_or("")) != revert).collect();
        let last_user = kept.iter().rev().find(|entry| entry["info"]["role"] == "user").and_then(|entry| entry["info"]["id"].as_str());
        let mut thread = provider_thread.clone();
        thread["providerSessionId"] = json!(self.session_id);
        thread["nativeConversationHeadRef"] = last_user.map_or(Value::Null, |id| projection::native_ref(id, "weak"));
        thread["status"] = json!("idle");
        thread["updatedAt"] = json!(at(Utc::now()));
        Ok(thread)
    }
    /// Stops whatever still runs under the thread's sessions, as a Stop does for a turn that is gone.
    async fn stop_background(&mut self) -> Result<(), String> {
        let roots: Vec<String> = self.threads.iter().filter(|(_, thread)| thread.parent.is_none()).map(|(id, _)| id.clone()).collect();
        for root in roots {
            self.abort_descendants(&root).await.map_err(|cause| cause.to_string())?;
        }
        Ok(())
    }

    // ---- detaching ----

    async fn detach(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id || self.detached {
            return Ok(());
        }
        let active: Vec<String> = self.threads.iter().filter(|(_, thread)| thread.turn.is_some()).map(|(id, _)| id.clone()).collect();
        for id in active {
            if let Some(turn) = self.threads.get_mut(&id).and_then(|thread| thread.turn.as_mut()) {
                turn.interrupted = true;
                turn.task.take().inspect(|task| task.abort());
            }
            if let Ok(client) = self.client() {
                let _ = client.abort(&id, Some(CLEANUP_TIMEOUT)).await;
            }
            self.finish_turn(&id, Terminal::Interrupted, false).await.map_err(|e| e.to_string())?;
        }
        self.release().await;
        self.detached = true;
        Ok(())
    }
    /// Gives the server back. A server the user runs outlives T3, so what its sessions still run is stopped; one T3
    /// started is shared by the instance's other threads, so only this runtime's busy sessions are. T3's MCP registration
    /// is dropped, and the stream with it.
    async fn release(&mut self) {
        if let Ok(client) = self.client().cloned() {
            let roots: Vec<String> = self.threads.iter().filter(|(_, thread)| thread.parent.is_none()).map(|(id, _)| id.clone()).collect();
            for root in roots {
                if self.external() || self.busy.contains(&root) || self.threads.iter().any(|(id, thread)| thread.parent.is_some() && self.busy.contains(id) && self.owners.get(id) == Some(&root)) {
                    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, client.abort(&root, None)).await;
                    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, self.abort_descendants(&root)).await;
                }
            }
            if let Some(name) = self.mcp_name.take() {
                let _ = client.mcp_disconnect(&name).await;
            }
        }
        self.pending.clear();
        self.pending_native.clear();
        self.live = None;
    }

    // ---- events ----

    async fn handle_frame(&mut self, frame: Option<Frame>) -> Result<bool, StoreError> {
        match frame {
            Some(Frame::Event(event)) => {
                self.handle_event(&event).await?;
                Ok(true)
            }
            Some(Frame::Closed) | None => {
                self.lose_stream("OpenCode event stream ended unexpectedly.").await?;
                Ok(true)
            }
            Some(Frame::Exited) => {
                self.lose_stream("OpenCode server exited unexpectedly.").await?;
                Ok(true)
            }
            Some(Frame::Done { session, message, generation, result, compaction }) => {
                self.on_done(&session, message, generation, result, compaction).await?;
                Ok(true)
            }
            Some(Frame::Admission { session, message, generation }) => {
                self.on_admission_window(&session, &message, generation).await?;
                Ok(true)
            }
            Some(Frame::Route(native)) => {
                self.retry_route(&native).await?;
                Ok(true)
            }
        }
    }
    /// The stream is the only signal an execution ended, and OpenCode 1.x does not resume it: every running turn fails,
    /// and the session is closed so the next turn opens a new one.
    async fn lose_stream(&mut self, message: &str) -> Result<(), StoreError> {
        tracing::warn!("{message}");
        self.busy.clear();
        self.stream_failure = Some(message.to_owned());
        self.set_session_status("error", Some(message))?;
        let ids: Vec<String> = self.threads.iter().filter(|(_, thread)| thread.turn.is_some()).map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.finish_turn(&id, Terminal::Failed(failure(message, "transport_error")), true).await?;
        }
        self.release().await;
        self.detached = true;
        Ok(())
    }
    async fn handle_event(&mut self, event: &Value) -> Result<(), StoreError> {
        let properties = &event["properties"];
        match event["type"].as_str().unwrap_or("") {
            "server.connected" => {
                if self.has_connected {
                    for thread in self.threads.values_mut() {
                        if let Some(turn) = thread.turn.as_mut() {
                            turn.usage.complete = false;
                        }
                    }
                }
                self.has_connected = true;
            }
            "message.updated" => {
                self.on_message_updated(properties).await?;
                self.on_assistant_completed(properties).await?;
            }
            "session.compacted" => {
                let session = str_of(&properties["sessionID"]);
                if self.threads.get(&session).is_some_and(|thread| thread.turn.is_some()) {
                    self.emit_compaction(&session)?;
                }
            }
            "message.part.updated" => self.on_part_updated(&properties["part"]).await?,
            "message.part.delta" => self.on_part_delta(properties)?,
            "todo.updated" => {
                let session = str_of(&properties["sessionID"]);
                if self.threads.get(&session).is_some_and(|thread| thread.turn.is_some()) {
                    self.emit_todos(&session, &properties["todos"])?;
                }
            }
            "permission.asked" => self.route_request(str_of(&properties["id"]), str_of(&properties["sessionID"]), false, properties.clone()).await?,
            "question.asked" => self.route_request(str_of(&properties["id"]), str_of(&properties["sessionID"]), true, properties.clone()).await?,
            "permission.replied" | "question.replied" => {
                let native = str_of(&properties["requestID"]);
                self.remember_settled(&native);
                self.resolve_request(&native, "resolved")?;
            }
            "question.rejected" => {
                let native = str_of(&properties["requestID"]);
                self.remember_settled(&native);
                self.resolve_request(&native, "cancelled")?;
            }
            "session.created" | "session.updated" => {
                let info = &properties["info"];
                let (id, parent) = (str_of(&info["id"]), str_of(&info["parentID"]));
                if !parent.is_empty() && !self.threads.contains_key(&id) {
                    let owner = if self.threads.contains_key(&parent) { Some(parent.clone()) } else { self.owners.get(&parent).cloned() };
                    if let Some(owner) = owner {
                        self.owners.insert(id, owner);
                    }
                }
            }
            "session.deleted" => {
                let id = str_of(&properties["info"]["id"]);
                self.owners.remove(&id);
                self.busy.remove(&id);
            }
            "session.status" => {
                let session = str_of(&properties["sessionID"]);
                match properties["status"]["type"].as_str() {
                    Some("busy" | "retry") => {
                        if self.threads.contains_key(&session) || self.owners.contains_key(&session) {
                            self.busy.insert(session.clone());
                        }
                    }
                    Some("idle") => {
                        self.busy.remove(&session);
                    }
                    _ => {}
                }
                if self.threads.contains_key(&session) && properties["status"]["type"] == "idle" {
                    self.on_idle(&session).await?;
                }
            }
            "session.idle" => {
                let session = str_of(&properties["sessionID"]);
                self.busy.remove(&session);
                self.on_idle(&session).await?;
            }
            "session.error" => self.on_session_error(properties).await?,
            _ => {}
        }
        Ok(())
    }
    fn remember_settled(&mut self, native: &str) {
        if self.settled.len() >= 2_048 {
            self.settled.clear();
        }
        self.settled.insert(native.to_owned());
    }
    /// An idle session ends its turn, unless the prompt it answers is not yet admitted: that idle may be an older prompt's.
    async fn on_idle(&mut self, session: &str) -> Result<(), StoreError> {
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        if turn.admission.pending {
            turn.admission.advance(Signal::Idle);
            return Ok(());
        }
        let end = if turn.interrupted { Terminal::Interrupted } else { Terminal::Completed };
        self.finish_turn(session, end, false).await
    }
    async fn on_session_error(&mut self, properties: &Value) -> Result<(), StoreError> {
        let sessions: Vec<String> = match properties["sessionID"].as_str() {
            None => self.threads.iter().filter(|(_, thread)| thread.turn.is_some()).map(|(id, _)| id.clone()).collect(),
            Some(id) => self.threads.contains_key(id).then(|| id.to_owned()).into_iter().collect(),
        };
        let message = error_message(properties);
        let broken = properties["sessionID"].is_null();
        // An error that names no session breaks them all; one that names a session fails its turn and leaves the session usable.
        if broken && !is_aborted(properties) {
            self.set_session_status("error", Some(&message))?;
        }
        for id in sessions {
            let Some(turn) = self.threads[&id].turn.as_ref() else { continue };
            let end = if turn.interrupted || is_aborted(properties) {
                Terminal::Interrupted
            } else {
                let mut failed = failure(&message, "provider_error");
                if let Some(code) = properties["error"]["name"].as_str() {
                    failed["code"] = json!(code);
                }
                Terminal::Failed(failed)
            };
            self.finish_turn(&id, end, broken).await?;
        }
        Ok(())
    }
    /// A command or compaction request ended.
    async fn on_done(&mut self, session: &str, message: Option<String>, generation: u64, result: Result<(), ApiError>, compaction: bool) -> Result<(), StoreError> {
        if let Some(message) = &message {
            self.commands.remove(message);
        }
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        turn.task = None;
        if compaction {
            return match result {
                Ok(()) => {
                    if !self.threads[session].turn.as_ref().is_some_and(|turn| turn.interrupted) && self.stream_failure.is_none() {
                        self.emit_compaction(session)?;
                    }
                    let end = if self.threads[session].turn.as_ref().is_some_and(|turn| turn.interrupted) { Terminal::Interrupted } else { Terminal::Completed };
                    self.finish_turn(session, end, false).await
                }
                Err(cause) => {
                    let end = if self.threads[session].turn.as_ref().is_some_and(|turn| turn.interrupted) { Terminal::Interrupted } else { failed_with(&cause) };
                    self.finish_turn(session, end, false).await
                }
            };
        }
        if turn.admission.generation != generation || turn.interrupted {
            return Ok(());
        }
        match result {
            Ok(()) => self.admit(session, Signal::Accepted).await,
            Err(cause) => self.finish_turn(session, failed_with(&cause), false).await,
        }
    }
    /// A command not admitted within its window fails the turn.
    async fn on_admission_window(&mut self, session: &str, message: &str, generation: u64) -> Result<(), StoreError> {
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        if !self.commands.contains(message) || turn.admission.generation != generation || !turn.admission.pending || turn.interrupted {
            return Ok(());
        }
        if let Some(task) = turn.task.take() {
            task.abort();
        }
        self.commands.remove(message);
        self.finish_turn(session, Terminal::Failed(failure("OpenCode command admission did not complete within 10 seconds.", "provider_error")), false).await
    }
    /// Feeds an admission signal and runs the idle reconciliation it may ask for.
    async fn admit(&mut self, session: &str, signal: Signal) -> Result<(), StoreError> {
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        if turn.admission.advance(signal) == Admit::ReconcileIdle {
            self.reconcile_admission(session).await?;
        }
        Ok(())
    }
    /// An idle was seen while the prompt was being admitted: the server says whether the session is really idle now.
    async fn reconcile_admission(&mut self, session: &str) -> Result<(), StoreError> {
        let Some(generation) = self.threads.get(session).and_then(|thread| thread.turn.as_ref()).map(|turn| turn.admission.generation) else { return Ok(()) };
        for attempt in 0..RECONCILE_ATTEMPTS {
            let current = |actor: &Actor| actor.threads.get(session).and_then(|thread| thread.turn.as_ref()).is_some_and(|turn| turn.admission.generation == generation && turn.admission.pending && !turn.interrupted);
            if !current(self) {
                return Ok(());
            }
            let status = match self.client() {
                Ok(client) => client.session_status().await.ok().map(|statuses| if statuses.get(session).is_none_or(|status| status["type"] == "idle") { "idle" } else { "busy" }),
                Err(_) => None,
            };
            if !current(self) {
                return Ok(());
            }
            match status {
                Some("idle") => {
                    self.threads.get_mut(session).unwrap().turn.as_mut().unwrap().admission.pending = false;
                    return self.finish_turn(session, Terminal::Completed, false).await;
                }
                Some(_) => {
                    self.threads.get_mut(session).unwrap().turn.as_mut().unwrap().admission.pending = false;
                    return Ok(());
                }
                None => {
                    if attempt + 1 < RECONCILE_ATTEMPTS {
                        self.clock.sleep_until(self.clock.now_ms() + RECONCILE_DELAY_MS).await;
                    }
                }
            }
        }
        Ok(())
    }

    // ---- messages and parts ----

    async fn on_message_updated(&mut self, properties: &Value) -> Result<(), StoreError> {
        let session = str_of(&properties["sessionID"]);
        let info = &properties["info"];
        let (id, role) = (str_of(&info["id"]), str_of(&info["role"]));
        let Some(thread) = self.threads.get_mut(&session) else { return Ok(()) };
        thread.roles.insert(id.clone(), role.clone());
        if role == "assistant" {
            if let Some(turn) = thread.turn.as_mut() {
                turn.usage.on_assistant(&id, info["parentID"].as_str().filter(|parent| !parent.is_empty()));
            }
            return Ok(());
        }
        let receipt = self.commands.contains(&id);
        let thread = self.threads.get_mut(&session).unwrap();
        let new_message = !thread.users.contains(&id);
        if new_message {
            thread.users.push(id.clone());
        }
        if thread.turn.is_none() && thread.parent.is_some() && new_message {
            self.start_child_turn(&session, info).await?;
        }
        let Some(turn) = self.threads.get_mut(&session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        let matches = turn.admission.message.as_ref().is_none_or(|message| *message == id);
        if matches {
            turn.usage.prompts.insert(id.clone());
        }
        let first = matches && turn.user_message.is_none();
        if first {
            turn.user_message = Some(id.clone());
        }
        let pending = matches && turn.admission.pending;
        if first {
            self.write_turn_row(&session, "running", None)?;
        }
        if pending {
            // A command's user message is the receipt of its request.
            if receipt {
                self.commands.remove(&id);
                self.admit(&session, Signal::Accepted).await?;
            }
            self.admit(&session, Signal::UserMessage).await?;
        }
        Ok(())
    }
    /// A completed assistant message is definitive evidence the prompt was admitted, and ends its text parts.
    async fn on_assistant_completed(&mut self, properties: &Value) -> Result<(), StoreError> {
        let info = &properties["info"];
        if info["role"] != "assistant" || info["time"]["completed"].is_null() {
            return Ok(());
        }
        let session = str_of(&properties["sessionID"]);
        let Some(turn) = self.threads.get_mut(&session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        turn.admission.advance(Signal::AssistantCompleted);
        let ids = turn.by_message.get(&str_of(&info["id"])).cloned().unwrap_or_default();
        for id in ids {
            self.emit_text(&session, &id, true)?;
        }
        Ok(())
    }
    async fn on_part_updated(&mut self, part: &Value) -> Result<(), StoreError> {
        let session = str_of(&part["sessionID"]);
        let Some(thread) = self.threads.get_mut(&session) else { return Ok(()) };
        let role_known = thread.roles.contains_key(&str_of(&part["messageID"]));
        let user_text = part["type"] == "text" && thread.roles.get(&str_of(&part["messageID"])).is_some_and(|role| role == "user");
        let Some(turn) = thread.turn.as_mut() else { return Ok(()) };
        if user_text {
            if !turn.root {
                self.project_child_user_part(&session, part)?;
            }
            return Ok(());
        }
        let kind = part["type"].as_str().unwrap_or("");
        if kind == "step-finish" {
            turn.usage.on_step(&str_of(&part["messageID"]), &str_of(&part["id"]), &part["tokens"], role_known);
            return Ok(());
        }
        let id = str_of(&part["id"]);
        if kind == "tool" {
            // Approval routing needs the tool name, without retaining its input and output.
            turn.tool_names.insert(str_of(&part["callID"]), str_of(&part["tool"]));
        } else {
            turn.parts.insert(id.clone(), part.clone());
            turn.by_message.entry(str_of(&part["messageID"])).or_default().push(id.clone());
            if !turn.open.contains(&id) {
                turn.open.push(id.clone());
            }
        }
        match kind {
            "text" | "reasoning" => self.emit_text(&session, &id, false),
            "tool" => self.emit_tool(&session, part).await,
            _ => Ok(()),
        }
    }
    fn on_part_delta(&mut self, properties: &Value) -> Result<(), StoreError> {
        if properties["field"] != "text" {
            return Ok(());
        }
        let session = str_of(&properties["sessionID"]);
        let id = str_of(&properties["partID"]);
        let Some(turn) = self.threads.get_mut(&session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        let Some(part) = turn.parts.get_mut(&id).filter(|part| matches!(part["type"].as_str(), Some("text" | "reasoning"))) else { return Ok(()) };
        let text = format!("{}{}", part["text"].as_str().unwrap_or(""), properties["delta"].as_str().unwrap_or(""));
        part["text"] = json!(text);
        self.emit_text(&session, &id, false)
    }
    /// `emitTextPart`: a text or reasoning part as its block says: running while it streams, written whole when it ends.
    fn emit_text(&mut self, session: &str, id: &str, force: bool) -> Result<(), StoreError> {
        let thread_id = self.threads[session].app_thread.clone();
        let now_ms = self.clock.now_ms();
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        let Some(part) = turn.parts.get(id).cloned() else { return Ok(()) };
        let reasoning = part["type"] == "reasoning";
        if part["type"] == "text" && (part["ignored"] == true || part["synthetic"] == true) {
            return Ok(());
        }
        let text = part["text"].as_str().unwrap_or("").to_owned();
        if text.is_empty() {
            return Ok(());
        }
        let ended = force || !part["time"]["end"].is_null();
        let kind = if reasoning { Kind::Reasoning } else { Kind::Text };
        let change = if ended { Change::Ended(&text) } else { Change::Replace(&text) };
        let Some((native, text, last)) = turn.projector.apply_text_native(id.to_owned(), kind, change, now_ms) else { return Ok(()) };
        if last {
            turn.open.retain(|open| open != id);
        }
        let view = view(&self.store, &thread_id)?;
        let events = self.threads.get_mut(session).unwrap().turn.as_mut().unwrap().projector.write_text(&view, &native, &text, last)?;
        self.commit_to(&thread_id, events)
    }
    async fn emit_tool(&mut self, session: &str, part: &Value) -> Result<(), StoreError> {
        let tool = part["tool"].as_str().unwrap_or("").to_lowercase();
        if tool == "task" {
            return self.emit_subagent(session, part).await;
        }
        // question.asked carries the respondable item; projecting the implementation tool as well would duplicate it.
        if tool == "question" {
            return Ok(());
        }
        let name = str_of(&part["tool"]);
        let native_tool = name == "code_search" || name == "apply_patch";
        if !native_tool && name.contains('_') && self.threads[session].turn.as_ref().is_some_and(|turn| turn.servers.is_none()) {
            if let Ok(client) = self.client().cloned() {
                if let Ok(status) = client.mcp_status(MCP_STATUS_TIMEOUT).await {
                    let servers: Vec<String> = status.as_object().map(|servers| servers.keys().cloned().collect()).unwrap_or_default();
                    if let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) {
                        turn.servers = Some(servers);
                    }
                }
            }
        }
        let thread_id = self.threads[session].app_thread.clone();
        let view = view(&self.store, &thread_id)?;
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        let servers = turn.servers.clone();
        let events = p1::tool_events(&mut turn.projector, &view, part, servers.as_deref())?;
        self.commit_to(&thread_id, events)
    }
    fn emit_compaction(&mut self, session: &str) -> Result<(), StoreError> {
        let thread_id = self.threads[session].app_thread.clone();
        let view = view(&self.store, &thread_id)?;
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        if turn.compacted {
            return Ok(());
        }
        turn.compacted = true;
        let provider_turn = turn.projector.provider_turn(&view)?;
        let native = format!("{}:compaction", str_of(&provider_turn));
        let ordinal = turn.projector.ordinal(&view, &native);
        let now = at(Utc::now());
        let root = turn.projector.context().root_node_id.clone();
        let mut item = turn.projector.base_item(&projection::item_id(&native), &root, ordinal, "completed", &native, &turn.started_at, Some(&now), &now, &provider_turn);
        item["nativeItemRef"] = projection::native_ref(&native, "weak");
        item["type"] = json!("compaction");
        item["driver"] = json!(DRIVER);
        item["title"] = json!("Context compacted");
        self.commit_to(&thread_id, vec![("turn-item.updated", item)])
    }
    /// `emitTodo`: the agent's todo list, rewritten whole by every update.
    fn emit_todos(&mut self, session: &str, todos: &Value) -> Result<(), StoreError> {
        let thread_id = self.threads[session].app_thread.clone();
        let view = view(&self.store, &thread_id)?;
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        let ctx = turn.projector.context().clone();
        let provider_turn = str_of(&turn.projector.provider_turn(&view)?);
        let plan_id = turn.plan.get_or_insert_with(|| format!("plan:provider:{DRIVER}:thread:{}:{}", projection::enc(&thread_id), uuid::Uuid::new_v4())).clone();
        let native = format!("{session}:todo:{provider_turn}");
        let steps: Vec<Value> = todos
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, todo)| {
                let text = todo["content"].as_str().map(str::trim).filter(|text| !text.is_empty()).map_or_else(|| format!("Todo {}", index + 1), str::to_owned);
                let status = match todo["status"].as_str() {
                    Some("completed") => "completed",
                    Some("in_progress") => "running",
                    _ => "pending",
                };
                json!({"id":format!("{native}:{}", index + 1),"text":text,"status":status})
            })
            .collect();
        let done = !steps.is_empty() && steps.iter().all(|step| step["status"] == "completed");
        let now = at(Utc::now());
        let status = if done { "completed" } else { "running" };
        let completed = done.then_some(now.as_str());
        let run = if ctx.run_id.is_empty() { Value::Null } else { json!(ctx.run_id) };
        let ordinal = turn.projector.ordinal(&view, &native);
        let started = turn.started_at.clone();
        let node = turn.projector.node(&native, "todo_list", status, &started, completed, &json!(provider_turn));
        let mut node = node;
        node["nativeItemRef"] = projection::native_ref(&native, "weak");
        let plan = json!({"id":plan_id,"threadId":thread_id,"runId":run,"nodeId":projection::node_id(&native),"status":if done { "completed" } else { "active" },"kind":"todo_list","steps":steps});
        let mut item = turn.projector.base_item(&projection::item_id(&native), &json!(projection::node_id(&native)), ordinal, status, &native, &started, completed, &now, &json!(provider_turn));
        item["nativeItemRef"] = projection::native_ref(&native, "weak");
        item["title"] = json!("Todo list");
        item["type"] = json!("todo_list");
        item["planId"] = json!(plan_id);
        item["steps"] = json!(steps);
        self.commit_to(&thread_id, vec![("node.updated", node), ("plan.updated", plan), ("turn-item.updated", item)])
    }

    // ---- subagents ----

    fn thread_of(&self, session: &str) -> String {
        self.threads.get(session).map_or_else(|| self.thread_id.clone(), |thread| thread.app_thread.clone())
    }
    /// A `task` call: its node, subagent row and item on the calling turn; the first time it names its session, that
    /// session becomes a child thread and gets the thread's complete policy.
    async fn emit_subagent(&mut self, session: &str, part: &Value) -> Result<(), StoreError> {
        let key = str_of(&part["id"]);
        let parent_thread = self.thread_of(session);
        let parent_view = view(&self.store, &parent_thread)?;
        let now = at(Utc::now());
        let times = p1::tool_times(part);
        let input = p1::tool_input(part);
        let prompt = input["prompt"].as_str().map(str::trim).filter(|text| !text.is_empty()).map(|_| str_of(&input["prompt"])).unwrap_or_default();
        let title = p1::tool_title(part).or_else(|| input["description"].as_str().filter(|text| !text.trim().is_empty()).map(str::to_owned));
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        turn.usage.has_subagents = true;
        let ctx = turn.projector.context().clone();
        let selection = turn.projector.selection().clone();
        let ordinal = turn.projector.ordinal(&parent_view, &key);
        self.subs.entry(key.clone()).or_insert_with(|| Sub { parent_session: session.to_owned(), ctx, selection, started_at: times.started.clone(), child: None, model: None, result: None });
        if let Some(model) = p1::tool_model(part) {
            self.subs[&key].model = Some(model);
        }
        if let Some(child) = p1::task_session(part).filter(|_| self.subs[&key].child.is_none()) {
            self.attach_child(session, &key, &child, &prompt, title.as_deref(), ordinal, &parent_view, &now).await?;
        }
        if part["state"]["status"] == "completed" {
            if let Some(output) = p1::tool_output(part) {
                self.subs[&key].result = Some(output);
            }
        }
        self.write_subagent(session, &key, part, &prompt, title.as_deref(), ordinal, &times, &now)
    }
    #[allow(clippy::too_many_arguments)]
    async fn attach_child(&mut self, session: &str, key: &str, child: &str, prompt: &str, title: Option<&str>, ordinal: u64, parent_view: &Value, now: &str) -> Result<(), StoreError> {
        let (ctx, selection, model) = {
            let sub = &mut self.subs[key];
            sub.child = Some(child.to_owned());
            (sub.ctx.clone(), sub.selection.clone(), sub.model.clone())
        };
        self.by_child.insert(child.to_owned(), key.to_owned());
        self.owners.insert(child.to_owned(), session.to_owned());
        let child_thread = format!("thread:provider:{DRIVER}:native-thread:{}", projection::enc(child));
        let provider_thread_id = format!("provider-thread:provider:{DRIVER}:native-thread:{}", projection::enc(child));
        let node = projection::node_id(key);
        let mut child_selection = json!({"instanceId":ctx.provider_instance_id,"model":model.unwrap_or_else(|| str_of(&selection["model"]))});
        if let Some(options) = selection.get("options").filter(|_| false) {
            child_selection["options"] = options.clone();
        }
        let parent_title = parent_view["thread"]["title"].as_str().unwrap_or_default().to_owned();
        let thread_title = crate::subagents::thread_title(&parent_title, &json!(title), prompt, ordinal);
        let app_thread = crate::subagents::child_thread(&parent_view["thread"], &json!({"childThreadId":child_thread,"parentNodeId":node,"activeProviderThreadId":provider_thread_id,"providerInstanceId":ctx.provider_instance_id,"modelSelection":child_selection,"title":thread_title,"createdBy":"agent","creationSource":"provider"}), now);
        crate::subagents::ensure_child_thread(&self.store, &app_thread)?;
        let provider_thread = json!({"id":provider_thread_id,"driver":DRIVER,"providerInstanceId":ctx.provider_instance_id,"providerSessionId":self.session_id,"appThreadId":child_thread,"ownerNodeId":node,"nativeThreadRef":thread_ref(child),"nativeConversationHeadRef":null,"status":"active","firstRunOrdinal":null,"lastRunOrdinal":null,"handoffIds":[],"forkedFrom":null,"pendingBackgroundTasks":[],"contextUsage":null,"nativeMetadata":null,"goal":null,"createdAt":now,"updatedAt":now});
        let parent_thread = self.thread_of(session);
        self.commit_to(&child_thread, vec![("provider-thread.updated", provider_thread.clone())])?;
        self.commit_to(&parent_thread, vec![("provider-thread.updated", provider_thread.clone())])?;
        let mode = self.threads[session].runtime_mode.clone();
        let directory = self.threads[session].directory.clone();
        // OpenCode gives a task session only its parent's deny rules, so the complete policy is installed now.
        if let Ok(client) = self.client().cloned() {
            let installed = async {
                let native = client.session_get(child).await?;
                let parent = {
                    let mut rules = p1::permission_rules(&mode);
                    rules.extend(p1::mcp_rules(self.mcp_name.as_deref()));
                    rules
                };
                let rules = p1::child_permission_rules(&parent, native["permission"].as_array().map(Vec::as_slice).unwrap_or(&[]));
                client.session_update(child, &json!(rules)).await
            };
            if let Err(cause) = installed.await {
                tracing::warn!(%cause, "Could not install the thread's policy on an OpenCode subagent session.");
            }
        }
        self.threads.insert(child.to_owned(), Thread { provider_thread, app_thread: child_thread, directory, turn: None, roles: HashMap::new(), users: vec![], parent: Some(key.to_owned()), child_turns: 0, generation: 1, runtime_mode: mode });
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn write_subagent(&self, session: &str, key: &str, part: &Value, prompt: &str, title: Option<&str>, ordinal: u64, times: &p1::ToolTimes, now: &str) -> Result<(), StoreError> {
        let sub = &self.subs[key];
        let thread = self.thread_of(session);
        let node = projection::node_id(key);
        let status = match times.item {
            "failed" => "failed",
            "completed" => "completed",
            "pending" => "pending",
            _ => "running",
        };
        let completed = times.completed.clone().map_or(Value::Null, |at| json!(at));
        let run = if sub.ctx.run_id.is_empty() { Value::Null } else { json!(sub.ctx.run_id) };
        let (child_thread, child_provider) = match &sub.child {
            Some(child) => (json!(format!("thread:provider:{DRIVER}:native-thread:{}", projection::enc(child))), json!(format!("provider-thread:provider:{DRIVER}:native-thread:{}", projection::enc(child)))),
            None => (Value::Null, Value::Null),
        };
        let caller_provider_thread = sub.ctx.provider_thread_id.clone();
        let native_ref = projection::native_ref(key, "strong");
        let node_row = json!({"id":node,"threadId":thread,"runId":run,"parentNodeId":sub.ctx.root_node_id,"rootNodeId":sub.ctx.root_node_id,"kind":"subagent","status":times.node,"countsForRun":false,"providerThreadId":if child_provider.is_null() { caller_provider_thread.clone() } else { child_provider.clone() },"providerTurnId":sub.ctx.provider_turn_id,"nativeItemRef":native_ref,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":sub.started_at,"completedAt":completed});
        let row = json!({"id":node,"threadId":thread,"runId":run,"parentNodeId":sub.ctx.root_node_id,"origin":"provider_native","createdBy":"agent","driver":DRIVER,"providerInstanceId":sub.ctx.provider_instance_id,"providerThreadId":child_provider,"childThreadId":child_thread,"nativeTaskRef":native_ref,"prompt":prompt,"title":title,"model":sub.model,"status":status,"result":sub.result,"startedAt":sub.started_at,"completedAt":completed,"updatedAt":now});
        let item = json!({"id":projection::item_id(key),"threadId":thread,"runId":run,"nodeId":node,"providerThreadId":caller_provider_thread,"providerTurnId":sub.ctx.provider_turn_id,"nativeItemRef":native_ref,"parentItemId":null,"ordinal":ordinal,"status":times.item,"title":title,"startedAt":sub.started_at,"completedAt":completed,"updatedAt":now,"type":"subagent","subagentId":node,"origin":"provider_native","driver":DRIVER,"providerInstanceId":sub.ctx.provider_instance_id,"childThreadId":child_thread,"prompt":prompt,"result":sub.result});
        let _ = part;
        self.commit_to(&thread, vec![("node.updated", node_row), ("subagent.updated", row), ("turn-item.updated", item)])
    }
    /// A subagent session's first message while it has no turn opens the runless turn its execution runs in, on its child
    /// thread.
    async fn start_child_turn(&mut self, child: &str, message: &Value) -> Result<(), StoreError> {
        let Some(thread) = self.threads.get_mut(child) else { return Ok(()) };
        thread.child_turns += 1;
        let (turns, provider_thread, child_thread, parent_key) = (thread.child_turns, thread.provider_thread.clone(), thread.app_thread.clone(), thread.parent.clone());
        let Some(sub) = parent_key.and_then(|key| self.subs.get(&key)) else { return Ok(()) };
        let (instance, selection) = (sub.ctx.provider_instance_id.clone(), sub.selection.clone());
        let id = str_of(&message["id"]);
        let started = p1::instant(&message["time"]["created"]).unwrap_or_else(|| at(Utc::now()));
        let native_turn = id.clone();
        let turn_id = format!("provider-turn:provider:{DRIVER}:native-turn:{}", projection::enc(&native_turn));
        let root_node = projection::node_id(&format!("{child}:root:{id}"));
        let thread_row = view(&self.store, &child_thread)?;
        let mut child_selection = selection.clone();
        child_selection["model"] = thread_row["thread"]["modelSelection"]["model"].clone();
        let provider_thread_id = provider_thread["id"].clone();
        let projector = Projector::for_child(&child_thread, &json!(root_node), &instance, provider_thread_id.clone(), json!(turn_id), child_selection, "", None);
        let node_row = json!({"id":root_node,"threadId":child_thread,"runId":null,"parentNodeId":null,"rootNodeId":root_node,"kind":"root_turn","status":"running","countsForRun":false,"providerThreadId":provider_thread_id,"providerTurnId":turn_id,"nativeItemRef":projection::native_ref(&native_turn, "weak"),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":null});
        let turn_row = json!({"id":turn_id,"providerThreadId":provider_thread_id,"nodeId":root_node,"runAttemptId":null,"nativeTurnRef":projection::native_ref(&native_turn, "weak"),"ordinal":turns,"status":"running","startedAt":started,"completedAt":null});
        self.commit_to(&child_thread, vec![("node.updated", node_row), ("provider-turn.updated", turn_row)])?;
        let mut usage = p1::Usage::new();
        usage.prompts.insert(id.clone());
        let thread = self.threads.get_mut(child).unwrap();
        thread.turn = Some(Turn { projector, root: false, user_message: Some(id), admission: Admission { accepted: true, observed: true, ..Admission::default() }, interrupted: false, plan: None, usage, parts: IndexMap::new(), by_message: HashMap::new(), open: vec![], tool_names: HashMap::new(), servers: None, started_at: started, compacted: false, task: None });
        Ok(())
    }
    /// The subagent session's prompt, shown as the user message of its turn.
    fn project_child_user_part(&mut self, child: &str, part: &Value) -> Result<(), StoreError> {
        let child_thread = self.threads[child].app_thread.clone();
        let view = view(&self.store, &child_thread)?;
        let message = str_of(&part["messageID"]);
        let Some(turn) = self.threads.get_mut(child).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        let ordinal = turn.projector.ordinal(&view, &message);
        let ctx = turn.projector.context().clone();
        let provider_turn = turn.projector.provider_turn(&view)?;
        let now = at(Utc::now());
        let (row, item) = crate::subagents::conversation_artifacts(
            &json!({"senderThreadId":null,"messageId":projection::message_id(&message),"turnItemId":projection::item_id(&message),"threadId":child_thread,"rootNodeId":ctx.root_node_id,"providerThreadId":turn.projector.provider_thread(),"providerTurnId":provider_turn,"nativeItemRef":projection::native_ref(&message, "strong"),"role":"user","text":part["text"],"ordinal":ordinal}),
            &turn.started_at.clone(),
        );
        let _ = now;
        self.commit_to(&child_thread, vec![("message.updated", row), ("turn-item.updated", item)])
    }

    // ---- requests ----

    /// Every request is shown on the thread's own turn, under the subagent that leads to the asking session, because a
    /// subagent's thread is not where the user looks. A child's request can arrive before the call that reveals its
    /// relation to a thread, so it is routed when it can be and retried briefly when not.
    async fn route_request(&mut self, native: String, session: String, question: bool, value: Value) -> Result<(), StoreError> {
        if self.routes.contains_key(&native) {
            return Ok(());
        }
        if self.try_route(&native, &session, question, &value).await? {
            return Ok(());
        }
        self.routes.insert(native.clone(), Route { session, question, value, tries: 0 });
        self.schedule_route(&native);
        Ok(())
    }
    fn schedule_route(&self, native: &str) {
        let Some(live) = self.live.as_ref() else { return };
        let tries = self.routes.get(native).map_or(0, |route| route.tries);
        let (clock, sender, native) = (self.clock.clone(), live.sender.clone(), native.to_owned());
        let delay = (200i64 << tries.min(4)).min(2_000);
        tokio::spawn(async move {
            clock.sleep_until(clock.now_ms() + delay).await;
            let _ = sender.send(Frame::Route(native));
        });
    }
    async fn retry_route(&mut self, native: &str) -> Result<(), StoreError> {
        let Some(route) = self.routes.get(native) else { return Ok(()) };
        let (session, question, value) = (route.session.clone(), route.question, route.value.clone());
        if self.try_route(native, &session, question, &value).await? {
            self.routes.remove(native);
            return Ok(());
        }
        let route = self.routes.get_mut(native).unwrap();
        route.tries += 1;
        if route.tries >= ROUTE_RETRIES {
            self.routes.remove(native);
        } else {
            self.schedule_route(native);
        }
        Ok(())
    }
    async fn try_route(&mut self, native: &str, session: &str, question: bool, value: &Value) -> Result<bool, StoreError> {
        if self.settled.contains(native) || self.pending_native.contains_key(native) {
            return Ok(true);
        }
        let Some(owned) = self.resolve_owner(session).await else { return Ok(false) };
        let Some((owner, subagent)) = self.top_level_owner(&owned) else { return Ok(false) };
        self.show_request(&owner, subagent, native, session, question, value)?;
        Ok(true)
    }
    /// The thread state a session belongs to: its own, or a subagent's nearest known ancestor, found by walking the
    /// native parent chain. Every hop is remembered so later requests from the same session need no lookup.
    async fn resolve_owner(&mut self, session: &str) -> Option<String> {
        if self.threads.contains_key(session) {
            return Some(session.to_owned());
        }
        if let Some(owner) = self.owners.get(session) {
            return Some(owner.clone());
        }
        let client = self.client().ok()?.clone();
        let mut cursor = session.to_owned();
        let mut hops = vec![];
        for _ in 0..OWNER_HOPS {
            let info = client.session_get(&cursor).await.ok()?;
            let parent = info["parentID"].as_str().filter(|parent| !parent.is_empty())?.to_owned();
            hops.push(cursor.clone());
            let owner = if self.threads.contains_key(&parent) { Some(parent.clone()) } else { self.owners.get(&parent).cloned() };
            if let Some(owner) = owner {
                for hop in hops {
                    self.owners.insert(hop, owner.clone());
                }
                return Some(owner);
            }
            cursor = parent;
        }
        None
    }
    /// The thread's own session whose turn is running, and the subagent call leading from it to `state`.
    fn top_level_owner(&self, state: &str) -> Option<(String, Option<String>)> {
        let mut owner = state.to_owned();
        let mut subagent = None;
        while let Some(key) = self.threads.get(&owner)?.parent.clone() {
            let sub = self.subs.get(&key)?;
            owner = sub.parent_session.clone();
            subagent = Some(key);
        }
        self.threads.get(&owner)?.turn.as_ref().map(|_| (owner, subagent))
    }
    fn show_request(&mut self, owner: &str, subagent: Option<String>, native: &str, asking: &str, question: bool, value: &Value) -> Result<(), StoreError> {
        if self.pending_native.contains_key(native) {
            return Ok(());
        }
        let tool_name = value["tool"]["callID"].as_str().and_then(|call| self.threads.get(asking)?.turn.as_ref()?.tool_names.get(call).cloned());
        let body = if question {
            RequestBody::Questions(p1::request_questions(value))
        } else {
            let permission = str_of(&value["permission"]);
            let patterns: Vec<&str> = value["patterns"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            RequestBody::Approval { kind: projection::permission_request_kind(&permission, tool_name.as_deref()), prompt: if patterns.is_empty() { permission.clone() } else { patterns.join("\n") }, title: permission, options: Value::Null }
        };
        let thread_id = self.threads[owner].app_thread.clone();
        let view = view(&self.store, &thread_id)?;
        let session_row = self.session_id.clone();
        let parent_node = subagent.as_ref().map_or_else(|| self.threads[owner].turn.as_ref().unwrap().projector.context().root_node_id.clone(), |key| json!(projection::node_id(key)));
        let turn = self.threads.get_mut(owner).and_then(|thread| thread.turn.as_mut()).unwrap();
        let (request_id, events) = turn.projector.request_artifacts(&view, native, &body, &session_row, &parent_node)?;
        self.commit_to(&thread_id, events)?;
        self.pending_native.insert(native.to_owned(), request_id.clone());
        self.pending.insert(request_id, Pending { native: native.to_owned(), session: asking.to_owned(), owner: owner.to_owned(), question: question.then(|| value.clone()) });
        Ok(())
    }
    /// A request OpenCode answered or dropped.
    fn resolve_request(&mut self, native: &str, status: &str) -> Result<(), StoreError> {
        let Some(request_id) = self.pending_native.remove(native) else { return Ok(()) };
        let Some(pending) = self.pending.shift_remove(&request_id) else { return Ok(()) };
        let thread_id = self.threads.get(&pending.owner).map_or_else(|| self.thread_id.clone(), |thread| thread.app_thread.clone());
        let view = view(&self.store, &thread_id)?;
        let events = projection::settle_request_events(&view, &request_id, status)?;
        self.commit_to(&thread_id, events)
    }

    // ---- ending a turn ----

    /// The provider turn's row as the turn left it.
    fn write_turn_row(&mut self, session: &str, status: &str, completed: Option<&str>) -> Result<(), StoreError> {
        let thread_id = self.threads[session].app_thread.clone();
        let view = view(&self.store, &thread_id)?;
        let Some(turn) = self.threads.get(session).and_then(|thread| thread.turn.as_ref()) else { return Ok(()) };
        let id = turn.projector.provider_turn(&view)?;
        let mut row = find(&view, "providerTurns", &id)?.clone();
        row["status"] = json!(status);
        if let Some(message) = &turn.user_message {
            row["nativeTurnRef"] = projection::native_ref(message, "weak");
        }
        if let Some(completed) = completed {
            row["completedAt"] = json!(completed);
            row["turnTokenUsage"] = turn.usage.summary(status);
        }
        self.commit_to(&thread_id, vec![("provider-turn.updated", row)])
    }
    /// Ends a session's turn: open text is written final, the requests it asked settle, the provider thread goes idle, and
    /// the run reaches its terminal state (a subagent's turn has none).
    async fn finish_turn(&mut self, session: &str, terminal: Terminal, broken: bool) -> Result<(), StoreError> {
        let Some(turn) = self.threads.get_mut(session).and_then(|thread| thread.turn.as_mut()) else { return Ok(()) };
        if let Some(task) = turn.task.take() {
            task.abort();
        }
        // A turn ends once: a late event for it finds none.
        let open = std::mem::take(&mut turn.open);
        for id in open {
            self.emit_text(session, &id, true)?;
        }
        let ended: Vec<String> = self.pending.values().filter(|pending| pending.session == session || (pending.owner == session && self.threads[session].turn.as_ref().is_some_and(|turn| turn.root))).map(|pending| pending.native.clone()).collect();
        for native in ended {
            self.resolve_request(&native, "cancelled")?;
        }
        let now = at(Utc::now());
        self.write_turn_row(session, terminal.status(), Some(&now))?;
        let turn = self.threads.get_mut(session).and_then(|thread| thread.turn.take()).unwrap();
        let thread_id = self.threads[session].app_thread.clone();
        let ctx = turn.projector.context().clone();
        let head = turn.user_message.as_deref().map(|message| projection::native_ref(message, "weak"));
        let thread_view = view(&self.store, &thread_id)?;
        // The thread is idle between turns; a broken one is reopened by the next turn.
        let mut provider_thread = self.threads[session].provider_thread.clone();
        if let Ok(row) = find(&thread_view, "providerThreads", &ctx.provider_thread_id) {
            provider_thread = row.clone();
        }
        provider_thread["status"] = json!(if broken { "error" } else { "idle" });
        provider_thread["updatedAt"] = json!(now);
        if let Some(head) = head {
            provider_thread["nativeConversationHeadRef"] = head;
        }
        self.threads.get_mut(session).unwrap().provider_thread = provider_thread.clone();
        if !turn.root {
            let mut events = vec![("provider-thread.updated", provider_thread.clone())];
            if let Ok(node) = find(&thread_view, "nodes", &ctx.root_node_id) {
                let mut node = node.clone();
                node["status"] = json!(terminal.status());
                node["completedAt"] = json!(now);
                events.push(("node.updated", node));
            }
            self.commit_to(&thread_id, events)?;
            // The thread that holds the call shows the child's provider thread too.
            if let Some(parent) = self.threads[session].parent.as_ref().and_then(|key| self.subs.get(key)).map(|sub| self.thread_of(&sub.parent_session)) {
                self.commit_to(&parent, vec![("provider-thread.updated", provider_thread)])?;
            }
            return Ok(());
        }
        self.commit_to(&thread_id, vec![("provider-thread.updated", provider_thread)])?;
        match &terminal {
            // A failed run leaves its provider thread in `error`, whether or not the session survived; the next turn reopens it.
            Terminal::Failed(failed) => fail_run_with(&self.store, &self.thread_id, Some(&ctx.run_id), Some(&ctx), failed, Retain::Nothing)?,
            other => {
                terminal_with(&self.store, &self.thread_id, Some(&ctx.run_id), other.status(), None, Some(&ctx), Retain::Nothing)?;
                settle_queue(&self.store, &self.thread_id, None)?;
            }
        }
        Ok(())
    }
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
    loop {
        if *stopped.borrow() {
            break;
        }
        // Receive the next stream frame only while a session is live.
        let next = async {
            match runtime.as_mut().and_then(|actor| actor.live.as_mut()) {
                Some(live) => live.frames.recv().await,
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
                        if runtime.as_ref().is_some_and(|actor| actor.detached) {
                            runtime = None;
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
            }
            frame = next => {
                let actor = runtime.as_mut().unwrap();
                let handled = tokio::select! { handled = actor.handle_frame(frame) => handled, _ = stopped.changed() => break };
                if let Err(cause) = handled {
                    tracing::error!(%cause, "OpenCode provider event could not be persisted");
                    let failed = crate::provider_failure::make(&json!({"message":cause.to_string(),"class":"transport_error","retryable":false}));
                    let active: Vec<TurnContext> = actor.threads.values().filter_map(|thread| thread.turn.as_ref().filter(|turn| turn.root).map(|turn| turn.projector.context().clone())).collect();
                    for ctx in active {
                        let _ = crate::codex_runtime::fail_run_captured(&actor.store, &actor.thread_id, Some(ctx.run_id.as_str()), Some(&ctx), &failed);
                    }
                    work.close();
                    break;
                }
            }
        }
    }
    work.close();
    if let Some(mut actor) = runtime {
        let ending = if *stopped.borrow() || user_cancelled { "interrupted" } else { "failed" };
        let contexts: Vec<TurnContext> = actor.threads.values().filter_map(|thread| thread.turn.as_ref().filter(|turn| turn.root).map(|turn| turn.projector.context().clone())).collect();
        // A server T3 did not start keeps running after T3 stops, so the turns it would otherwise finish unseen are stopped.
        // So is a prompt a Stop cancelled on its way: it may have been taken, and no turn shows it.
        if actor.external() || user_cancelled {
            let active: Vec<String> = actor.threads.iter().filter(|(_, thread)| thread.turn.is_some() || (user_cancelled && thread.parent.is_none())).map(|(id, _)| id.clone()).collect();
            for id in active {
                if let Ok(client) = actor.client() {
                    let _ = client.abort(&id, Some(CLEANUP_TIMEOUT)).await;
                }
            }
        }
        for ctx in &contexts {
            let _ = crate::codex_runtime::terminal_captured(&actor.store, &actor.thread_id, Some(ctx.run_id.as_str()), ending, None, Some(ctx));
        }
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
        actor.release().await;
    }
    drop(mcp);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_prompt_is_admitted_once_it_is_accepted_and_its_message_shows() {
        let mut admission = Admission { pending: true, ..Admission::default() };
        assert_eq!(admission.advance(Signal::Accepted), Admit::Hold);
        assert_eq!(admission.advance(Signal::UserMessage), Admit::Release);
        assert_eq!(admission.advance(Signal::Idle), Admit::Release);
    }
    #[test]
    fn an_idle_during_admission_is_checked_with_the_server_before_it_ends_the_turn() {
        let mut admission = Admission { pending: true, ..Admission::default() };
        assert_eq!(admission.advance(Signal::Idle), Admit::Hold);
        assert_eq!(admission.advance(Signal::UserMessage), Admit::Hold);
        assert_eq!(admission.advance(Signal::Accepted), Admit::ReconcileIdle);
        assert!(admission.pending);
    }
    #[test]
    fn a_completed_assistant_message_admits_the_prompt() {
        let mut admission = Admission { pending: true, idle_during: true, ..Admission::default() };
        assert_eq!(admission.advance(Signal::AssistantCompleted), Admit::Release);
        assert!(!admission.pending);
    }
    #[test]
    fn session_errors_are_named_by_their_payload() {
        assert_eq!(error_message(&json!({})), "OpenCode session failed without an error payload.");
        assert_eq!(error_message(&json!({"error":{"name":"APIError","data":{"message":"quota"}}})), "quota");
        assert_eq!(error_message(&json!({"error":{"name":"UnknownError"}})), "UnknownError");
        assert!(is_aborted(&json!({"error":{"name":"MessageAbortedError"}})));
    }
}
