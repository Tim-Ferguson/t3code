//! OpenCode 2 provider actor: one per thread. It borrows the instance's server for as long as it lives, reads the
//! server's event stream, turns it into persisted events (via `opencode_projection`), and answers permission asks and
//! question forms from persisted responses. Mirrors `OpenCode2AdapterV2.ts`; structure follows `claude_adapter.rs`.
//!
//! A turn is one `session.prompt` (or `session.command`, or `session.compact`); the session's next
//! `session.execution.*` terminal ends it. A steer is another prompt with `delivery: "steer"`, which OpenCode reads at
//! the running execution's next step boundary, so it ends with the turn it joined. Each runtime mode is a set of
//! session permission rules; plan mode is also OpenCode's `plan` agent.
use crate::{
    codex_runtime::{ActorLifetime, Retain, Work, commit, fail_run_with, find, settle_queue, terminal_with},
    execution::at,
    opencode_api::{ApiError, EventStream, paginate},
    opencode_instance::OpenCodeInstance,
    opencode_projection::{self as projection, Change, DRIVER, Kind, Projector, RequestBody},
    opencode_server::{Connection, Generation},
    persistence::{Effect, Store, StoreError},
    provider_registry::ProviderRegistry,
    provider_turn_context::TurnContext,
    scheduled_tasks::SchedulerClock,
};
use chrono::Utc;
use reqwest::Method;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const ACTIVE_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(5);
const MODEL_TIMEOUT: Duration = Duration::from_secs(5);
/// A lost event stream is resubscribed this many times, this far apart, before the session breaks.
const RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_DELAY_MS: i64 = 2_000;
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(15);
/// OpenCode writes a `: heartbeat` comment every 10 to 15 seconds, so this long without a byte means the server is stuck.
const SILENT_STREAM_MS: i64 = 45_000;
/// The session instructions entry T3 writes its per-turn system prompt to.
const INSTRUCTIONS_KEY: &str = "t3-code";
/// Answers that mean the server refused a prompt; any other failure may have been accepted.
const CLEAR_PROMPT_REJECTIONS: [&str; 5] = ["InvalidRequestError", "ConflictError", "UnauthorizedError", "CommandNotFoundError", "SkillNotFoundError"];
pub(crate) const STILL_STOPPING: &str = "OpenCode is still stopping the previous turn. Send the message again in a moment.";
/// Sent with a declined permission. A reject without a message is OpenCode's "stop": it ends the whole execution.
const DECLINED: &str = "The user declined this request.";

pub(crate) fn error(message: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidCommand(message.to_string())
}
pub(crate) fn view(store: &Store, thread_id: &str) -> Result<Value, StoreError> {
    crate::codex_runtime::projection(store, thread_id)
}
pub(crate) fn thread_ref(id: &str) -> Value {
    json!({"driver":DRIVER,"nativeId":id,"strength":"strong"})
}
pub(crate) fn workspace(store: &Store, thread_id: &str) -> Result<PathBuf, String> {
    let view = view(store, thread_id).map_err(|e| e.to_string())?;
    let project = store.projection("project", view["thread"]["projectId"].as_str().unwrap_or("")).map_err(|e| e.to_string())?.ok_or("Project not found.")?;
    let cwd = view["thread"]["worktreePath"].as_str().or_else(|| project["workspaceRoot"].as_str()).ok_or("Workspace root missing.")?;
    let cwd = std::fs::canonicalize(cwd).map_err(|e| e.to_string())?;
    if !cwd.is_dir() {
        return Err("Workspace root is not a directory.".into());
    }
    Ok(cwd)
}
pub(crate) fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or("").to_owned()
}
fn non_blank(value: &Value) -> Option<String> {
    value.as_str().filter(|text| !text.trim().is_empty()).map(str::to_owned)
}
/// Percent-encoding of a path segment (T3's steer ids carry `:`).
fn segment(value: &str) -> String {
    projection::enc(value)
}

// ---- rules ----

fn rule(action: &str, effect: &str) -> Value {
    json!({"action":action,"resource":"*","effect":effect})
}
/// T3's MCP server is registered per directory, not per session, so each thread gets its own `t3-code-<thread>` entry.
/// OpenCode skips every tool of a server whose name is over 64 characters, so a longer one is replaced by a digest.
pub(crate) fn mcp_server_name(thread_id: &str) -> String {
    use sha2::Digest;
    let name = format!("t3-code-{}", thread_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>());
    if name.len() <= 64 {
        return name;
    }
    let digest = sha2::Sha256::digest(thread_id.as_bytes());
    format!("t3-code-{}", digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect::<String>())
}
/// The rules that keep T3's MCP servers to their own thread, after the mode's: the last matching rule wins.
fn mcp_rules(server: Option<&str>) -> Vec<Value> {
    match server {
        None => vec![],
        Some(name) => vec![json!({"action":"t3-code-*","resource":"*","effect":"deny"}), json!({"action":format!("{name}_*"),"resource":"*","effect":"allow"})],
    }
}
/// A session's permission rules. OpenCode checks the agent's rules and then these, and the last rule that matches
/// decides, so these override the agent's. `paths` are the agent's own allows for its directories; `grants` are
/// "Always allow this session" answers.
pub(crate) fn session_rules(runtime_mode: &str, interaction_mode: &str, paths: &[Value], grants: &[Value], mcp_server: Option<&str>) -> Vec<Value> {
    let mut rules = if runtime_mode == "full-access" { vec![rule("*", "allow")] } else { vec![rule("shell", "ask"), rule("edit", if runtime_mode == "auto-accept-edits" { "allow" } else { "ask" }), rule("external_directory", "ask")] };
    rules.extend(grants.iter().cloned());
    // Plan mode writes only its plan, which `paths` allows again. Shell and read are never denied: the free tier
    // refuses sessions whose rules deny them.
    if interaction_mode == "plan" {
        rules.push(rule("edit", "deny"));
    }
    rules.extend(paths.iter().cloned());
    rules.extend(mcp_rules(mcp_server));
    rules
}
/// The session rules an agent keeps for its own directories, which T3's blanket rules would override.
fn agent_paths(rules: &[Value]) -> Vec<Value> {
    rules.iter().filter(|entry| entry["effect"] == "allow" && entry["resource"] != "*" && matches!(entry["action"].as_str(), Some("edit" | "external_directory"))).cloned().collect()
}

// ---- turns ----

#[derive(Clone)]
pub(crate) enum Terminal {
    Completed,
    Interrupted,
    Failed(Value),
}
impl Terminal {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            Terminal::Completed => "completed",
            Terminal::Interrupted => "interrupted",
            Terminal::Failed(_) => "failed",
        }
    }
    fn status_static(&self) -> &'static str {
        self.status()
    }
}
pub(crate) fn failure(message: &str, class: &str) -> Value {
    crate::provider_failure::make(&json!({"message":message,"class":class}))
}

struct Turn {
    projector: Projector,
    /// The native id the provider turn row names (the prompt id, where fork and rollback cut).
    native_turn: String,
    /// The history item this turn's execution follows, for a turn with no prompt id of T3's: `Some(None)` is an empty history.
    before: Option<Option<String>>,
    interrupted: bool,
    /// Set until the turn's prompt, command or compaction is sent. A Stop before then has nothing on the server to stop.
    unsent: bool,
    /// Steers sent into this turn that OpenCode has not delivered yet, by inbox id.
    steers: HashSet<String>,
    /// This turn's steers OpenCode delivered or cancelled: a retried one is not sent again.
    settled_inbox: HashSet<String>,
    /// How the last execution ended, while the turn waits for the one that delivers its steers.
    held_end: Option<Terminal>,
    /// Set on a turn started after a timed-out Stop's run left the server: its tail can still be on the stream.
    awaiting_start: bool,
    continuation: bool,
}
struct McpRegistration {
    name: String,
    directory: String,
    credential: String,
}
/// A `subagent` tool call. A background one outlives the turn that made it.
struct Call {
    tool_id: String,
    native_id: String,
    /// The calling session, and the turn whose item the call is.
    session: String,
    ctx: TurnContext,
    selection: Value,
    ordinal: u64,
    started_at: String,
    prompt: String,
    title: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    /// The tool returned while the subagent runs on; the report OpenCode gives its parent settles it.
    background: bool,
    child: Option<String>,
    status: String,
    result: Option<String>,
    completed_at: Option<String>,
}
/// What a subagent's session adds: the call that runs it and the thread it shows in. Each of its executions is a
/// runless turn there.
struct ChildInfo {
    parent_session: String,
    tool_id: String,
    thread: Value,
    provider_thread: Value,
    turns: u64,
    /// The prompt its next execution answers, shown as that turn's user message.
    prompt: Option<String>,
    /// The first item queued for the session since its last turn began.
    queued: Option<String>,
}
/// An execution OpenCode ran on a thread's session without T3 asking: the parent's answer once a background subagent
/// ended. Its events are held until the continuation turn T3 opens for it takes them.
struct Wake {
    events: Vec<Value>,
    running: bool,
    reports: Vec<Value>,
    detail: Option<String>,
    dropped: bool,
    after: Option<String>,
    first: Option<String>,
}
struct Report {
    inbox: String,
    child: String,
    report: Value,
    text: String,
}
struct Session {
    id: String,
    provider_thread: Value,
    directory: String,
    agent: String,
    model: Option<Value>,
    rules: Option<Vec<Value>>,
    runtime_mode: String,
    interaction_mode: String,
    grants: Vec<Value>,
    unsettled: bool,
    turn: Option<Turn>,
    stranded: HashSet<String>,
    mcp: Option<McpRegistration>,
    instructions: Option<String>,
    app_thread_id: String,
    calls: indexmap::IndexMap<String, Call>,
    child: Option<ChildInfo>,
    wakes: Vec<Wake>,
    reports: indexmap::IndexMap<String, Report>,
    stopped_children: HashSet<String>,
}
struct PendingRequest {
    session: String,
    answering: bool,
    native: Native,
}
enum Native {
    Permission { id: String, action: String, resources: Vec<String>, save: Vec<String> },
    Form { id: String, form: Value },
}
impl Native {
    fn id(&self) -> &str {
        match self {
            Native::Permission { id, .. } | Native::Form { id, .. } => id,
        }
    }
}

pub(crate) enum Frame {
    Event(Value),
    Closed,
}
struct Live {
    conn: Connection,
    frames: mpsc::UnboundedReceiver<Frame>,
    reader: tokio::task::JoinHandle<()>,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
/// Reads the stream until it ends or goes silent, handing every event to the actor.
pub(crate) fn start_reader(mut stream: EventStream, clock: Arc<dyn SchedulerClock>) -> (mpsc::UnboundedReceiver<Frame>, tokio::task::JoinHandle<()>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
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
    });
    (receiver, handle)
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
    sessions: HashMap<String, Session>,
    /// Which native session each provider thread of this actor is.
    pending: HashMap<String, PendingRequest>,
    busy: HashSet<String>,
    /// Context windows by directory, then `provider/model`.
    windows: HashMap<String, HashMap<String, u64>>,
    /// Each agent's own path allows, by the directory they were listed for.
    agent_rules: HashMap<String, HashMap<String, Vec<Value>>>,
    mcp: Option<crate::mcp_sessions::ProviderSessionConfig>,
    /// Sessions whose `revert.clear` has not had its empty execution yet.
    clearing: HashSet<String>,
    staged_reverts: HashSet<String>,
    detached: bool,
    /// What the session last loaded a thread with; a changed selection or policy resumes it.
    loaded_key: Option<Value>,
    stream_failure: Option<String>,
    /// A subagent's session, by its id, to the root session that started it.
    child_owners: HashMap<String, String>,
    /// What OpenCode announced about a subagent's session, until its call names it.
    announced: HashMap<String, Value>,
}

pub(crate) struct Models;
impl Models {
    pub(crate) fn parse_slug(slug: &str) -> Option<(String, String)> {
        let trimmed = slug.trim();
        let at = trimmed.find('/')?;
        (at > 0 && at < trimmed.len() - 1).then(|| (trimmed[..at].to_owned(), trimmed[at + 1..].to_owned()))
    }
}
/// The model OpenCode should run for a `provider/model` slug and its reasoning variant, or `None` for any other slug:
/// sending none would run OpenCode's default while T3 records the requested model.
fn model_ref(selection: &Value) -> Option<Value> {
    let (provider, model) = Models::parse_slug(selection["model"].as_str()?)?;
    let variant = selection["options"].as_array().into_iter().flatten().find(|option| option["id"] == "variant").and_then(|option| option["value"].as_str().map(str::to_owned));
    let mut reference = json!({"providerID":provider,"id":model});
    if let Some(variant) = variant {
        reference["variant"] = json!(variant);
    }
    Some(reference)
}
fn same_model(left: &Value, right: Option<&Value>) -> bool {
    let variant = |value: &Value| value["variant"].as_str().unwrap_or("default").to_owned();
    right.is_some_and(|right| left["providerID"] == right["providerID"] && left["id"] == right["id"] && variant(left) == variant(right))
}
/// `/name args` naming one of the workspace's commands, which OpenCode expands itself.
pub(crate) fn command_of(text: &str) -> Option<(String, String)> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"^/([^\s/]+)(?:\s+([\s\S]*))?$").unwrap());
    let captures = pattern.captures(text.trim())?;
    Some((captures[1].to_owned(), captures.get(2).map_or("", |text| text.as_str()).to_owned()))
}
/// `\p{Sc}`
fn is_currency(character: char) -> bool {
    matches!(character, '$' | '\u{a2}' | '\u{a3}' | '\u{a4}' | '\u{a5}' | '\u{58f}' | '\u{60b}' | '\u{9f2}' | '\u{9f3}' | '\u{9fb}' | '\u{af1}' | '\u{bf9}' | '\u{e3f}' | '\u{17db}' | '\u{20a0}'..='\u{20bf}' | '\u{a838}' | '\u{fdfc}' | '\u{fe69}' | '\u{ff04}' | '\u{ffe0}' | '\u{ffe1}' | '\u{ffe5}' | '\u{ffe6}')
}
/// `SKILL_MENTION_PATTERN` (`composerInlineTokens.ts`): the names `$name` tokens carry, each opening the text or following
/// whitespace and followed by whitespace or the end. A name may start with a digit, but compact amounts and numeric
/// expressions like `$20k` stay prose, and it must contain a letter.
pub(crate) fn skill_mentions(text: &str) -> Vec<String> {
    static NUMERIC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let numeric = NUMERIC.get_or_init(|| regex::Regex::new(r"^[0-9][0-9_]*(?:[kKmMbBtT]|[eE][0-9]+)?(?:\s|$)").unwrap());
    let mut names = vec![];
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        let opens = previous.is_none_or(char::is_whitespace);
        previous = Some(character);
        if !opens || !is_currency(character) {
            continue;
        }
        let rest = &text[index + character.len_utf8()..];
        let run = &rest[..rest.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))).unwrap_or(rest.len())];
        let valid = run.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) && run.chars().any(|c| c.is_ascii_alphabetic()) && !numeric.is_match(rest) && rest[run.len()..].chars().next().is_none_or(char::is_whitespace);
        if valid {
            names.push(run.to_owned());
        }
    }
    names
}
/// The workspace skills a prompt names with the composer's `$skill` tokens.
fn skills_named(text: &str, known: &HashSet<String>) -> Vec<String> {
    let mut named: Vec<String> = vec![];
    for name in skill_mentions(text) {
        if known.contains(&name) && !named.contains(&name) {
            named.push(name);
        }
    }
    named
}
fn turn_prompt_id(session: &str, attempt: &str) -> String {
    format!("msg_t3_turn_{session}:{attempt}")
}
fn steer_prompt_id(session: &str, message: &str) -> String {
    format!("msg_t3_steer_{session}:{message}")
}
fn wake_turn_id(session: &str, attempt: &str) -> String {
    format!("{session}:wake:{attempt}")
}
/// The user message a provider turn prompted with.
fn prompt_of(turn: &Value) -> Option<String> {
    turn["nativeTurnRef"]["nativeId"].as_str().filter(|id| id.starts_with("msg_")).map(str::to_owned)
}
fn is_wake_turn(turn: &Value) -> bool {
    turn["nativeTurnRef"]["nativeId"].as_str().is_some_and(|id| id.contains(":wake:"))
}
fn is_execution_end(kind: &str) -> bool {
    matches!(kind, "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted")
}
fn session_of(event: &Value) -> Option<String> {
    event["data"]["sessionID"].as_str().map(str::to_owned)
}
fn text_of(content: &Value) -> String {
    content.as_array().into_iter().flatten().filter(|part| part["type"] == "text").filter_map(|part| part["text"].as_str().filter(|text| !text.is_empty())).collect::<Vec<_>>().join("\n")
}
/// A form's fields as T3 questions, or why T3 cannot ask them.
fn form_questions(form: &Value) -> Result<Value, String> {
    let mut questions = vec![];
    for (index, field) in form["fields"].as_array().into_iter().flatten().enumerate() {
        match field["type"].as_str() {
            Some("external") => return Err("a link to open".into()),
            Some("string" | "multiselect") => {}
            other => return Err(format!("a {} value", other.unwrap_or("unknown"))),
        }
        if field["hidden"] == true {
            return Err("a hidden field".into());
        }
        if field["when"].as_array().is_some_and(|when| !when.is_empty()) {
            return Err("a field that depends on another".into());
        }
        let header = non_blank(&field["title"]).map(|title| title.trim().to_owned()).unwrap_or_else(|| format!("Question {}", index + 1));
        let options: Vec<Value> = field["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|option| {
                let label = non_blank(&option["label"]).or_else(|| non_blank(&option["value"])).map(|label| label.trim().to_owned()).unwrap_or_else(|| "Option".into());
                json!({"label":label,"description":non_blank(&option["description"]).map(|text| text.trim().to_owned()).unwrap_or_else(|| label.clone()),"value":option["value"]})
            })
            .collect();
        let question = non_blank(&field["description"]).map(|text| text.trim().to_owned()).unwrap_or_else(|| header.clone());
        questions.push(json!({"id":field["key"],"header":header,"question":question,"options":options,"multiSelect":field["type"] == "multiselect","allowCustomAnswer":field["custom"] == true || options.is_empty()}));
    }
    Ok(json!(questions))
}
/// T3's answers in OpenCode's shape: a list for a multi-select, text otherwise.
fn form_answer(form: &Value, answers: &Value) -> Value {
    let mut answer = serde_json::Map::new();
    for field in form["fields"].as_array().into_iter().flatten() {
        let key = field["key"].as_str().unwrap_or("");
        let raw = &answers[key];
        let values: Vec<String> = match raw {
            Value::Array(values) => values.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            Value::String(text) => vec![text.clone()],
            _ => vec![],
        };
        let values: Vec<String> = values.into_iter().filter(|value| !value.trim().is_empty()).collect();
        if !values.is_empty() {
            answer.insert(key.to_owned(), if field["type"] == "multiselect" { json!(values) } else { json!(values.join(", ")) });
        }
    }
    Value::Object(answer)
}
fn approves(decision: &str) -> bool {
    matches!(decision, "accept" | "acceptForSession" | "acceptAlways")
}
fn session_grant_label(action: &str, save: &[String]) -> String {
    if save.iter().all(|pattern| pattern == "*") { format!("Allow every {action} request this session") } else { format!("Allow {} this session", save.join(", ")) }
}
fn declined_note(action: &str, resources: &[String]) -> String {
    let named = if resources.is_empty() { String::new() } else { format!(" ({})", resources.join(", ")) };
    format!("The user declined the {action} request{named}. Do not retry it; continue without it or ask the user how to proceed.")
}

impl Actor {
    async fn connect(store: Store, providers: &ProviderRegistry, thread_id: &str, mcp: Option<&crate::provider_mcp::CredentialLease>) -> Result<Self, String> {
        let thread = view(&store, thread_id).map_err(|e| e.to_string())?;
        let instance_id = thread["thread"]["modelSelection"]["instanceId"].as_str().ok_or("Thread has no provider instance.")?;
        let instance = providers.opencode(instance_id).map_err(|e| e.to_string())?;
        let cwd = workspace(&store, thread_id)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let generation = instance.last_probe().map(|probed| probed.generation);
        let capabilities: Value = serde_json::from_str(include_str!("opencode-capabilities.json")).unwrap();
        let _ = generation;
        let session = json!({"id":session_id,"driver":DRIVER,"providerInstanceId":instance_id,"status":"ready","cwd":cwd,"model":thread["thread"]["modelSelection"]["model"],"capabilities":capabilities,"createdAt":now,"updatedAt":now,"lastError":null});
        let clock = providers.idle_release().clock();
        let mut actor = Self { store, thread_id: thread_id.into(), session_id, instance, providers: providers.clone(), clock, cwd, session, live: None, sessions: HashMap::new(), pending: HashMap::new(), busy: HashSet::new(), windows: HashMap::new(), agent_rules: HashMap::new(), mcp: mcp.map(|lease| lease.config.clone()), clearing: HashSet::new(), staged_reverts: HashSet::new(), detached: false, loaded_key: None, stream_failure: None, child_owners: HashMap::new(), announced: HashMap::new() };
        // The session borrows the server for as long as it is open; the stream is subscribed before any session or
        // prompt call, so no event of theirs is missed.
        let connection = actor.instance.connect(&actor.cwd, actor.clock.clone()).await.map_err(|e| e.to_string())?;
        if connection.generation != Generation::V2 {
            return Err("This OpenCode server is 1.x; the 1.x adapter drives it.".into());
        }
        actor.open_stream(connection).await.map_err(|e| e.to_string())?;
        let session = actor.session.clone();
        commit(&actor.store, thread_id, |_| Ok(vec![("provider-session.attached", session.clone())])).map_err(|e| e.to_string())?;
        let cwd = actor.cwd.to_string_lossy().into_owned();
        actor.read_models(&cwd).await;
        Ok(actor)
    }
    async fn open_stream(&mut self, conn: Connection) -> Result<(), ApiError> {
        let stream = conn.api.subscribe("/api/event").await?;
        let (frames, reader) = start_reader(stream, self.clock.clone());
        self.live = Some(Live { conn, frames, reader });
        Ok(())
    }
    fn api(&self) -> Result<&crate::opencode_api::Api, ApiError> {
        self.live.as_ref().map(|live| &live.conn.api).ok_or_else(|| ApiError::Transport("the OpenCode session is closed".into()))
    }
    fn external(&self) -> bool {
        self.live.as_ref().is_some_and(|live| live.conn.external)
    }
    async fn call(&self, method: Method, path: &str, query: &[(String, String)], body: Option<Value>, limit: Option<Duration>) -> Result<Value, ApiError> {
        self.api()?.call(method, path, query, body.as_ref(), limit).await
    }
    async fn session_call(&self, method: Method, session: &str, tail: &str, query: &[(String, String)], body: Option<Value>, limit: Option<Duration>) -> Result<Value, ApiError> {
        self.call(method, &format!("/api/session/{}{tail}", segment(session)), query, body, limit).await
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

    // ---- models, agents, rules ----

    /// Context windows come from the server's model list for a directory; a directory counts as known once its read
    /// starts, so a failed read is retried only after a turn.
    async fn read_models(&mut self, directory: &str) {
        self.windows.entry(directory.to_owned()).or_default();
        let query = vec![("location[directory]".to_owned(), directory.to_owned())];
        let Ok(listed) = self.call(Method::GET, "/api/model", &query, None, Some(MODEL_TIMEOUT)).await else { return };
        let windows = self.windows.entry(directory.to_owned()).or_default();
        for model in listed["data"].as_array().into_iter().flatten() {
            // A model's input limit, when it has one, is its real headroom.
            if let Some(limit) = model["limit"]["input"].as_u64().or_else(|| model["limit"]["context"].as_u64()) {
                windows.insert(format!("{}/{}", str_of(&model["providerID"]), str_of(&model["id"])), limit);
            }
        }
    }
    async fn read_models_once(&mut self, directory: &str) {
        if !self.windows.contains_key(directory) {
            self.read_models(directory).await;
        }
    }
    fn window_of(&self, directory: &str, model: &str) -> Option<u64> {
        self.windows.get(directory)?.get(model).copied()
    }
    /// Each listed agent's own path allows (a fresh server lists no agents for its first moments).
    async fn paths_for(&mut self, directory: &str, agents: &[String]) -> Vec<Value> {
        if !self.agent_rules.contains_key(directory) {
            let query = vec![("location[directory]".to_owned(), directory.to_owned())];
            let mut listed = None;
            let read = async {
                loop {
                    let answer = self.call(Method::GET, "/api/agent", &query, None, Some(MODEL_TIMEOUT)).await.ok()?;
                    if answer["data"].as_array().is_some_and(|list| !list.is_empty()) {
                        return Some(answer);
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            };
            if let Ok(Some(answer)) = tokio::time::timeout(MODEL_TIMEOUT, read).await {
                listed = Some(answer);
            }
            let Some(listed) = listed else { return vec![] };
            let known = listed["data"].as_array().into_iter().flatten().map(|agent| (str_of(&agent["id"]), agent_paths(agent["permissions"].as_array().map(Vec::as_slice).unwrap_or(&[])))).collect();
            self.agent_rules.insert(directory.to_owned(), known);
        }
        let known = &self.agent_rules[directory];
        let mut paths: indexmap::IndexMap<String, Value> = Default::default();
        for agent in agents {
            for entry in known.get(agent).into_iter().flatten() {
                paths.insert(format!("{}\0{}", str_of(&entry["action"]), str_of(&entry["resource"])), entry.clone());
            }
        }
        paths.into_values().collect()
    }
    /// The rules a thread's session runs a policy with. Full access allows every path already, so the agent's own path
    /// rules are read only for the modes that narrow it.
    async fn rules_for(&mut self, directory: &str, agent: &str, grants: &[Value], runtime_mode: &str, interaction_mode: &str, app_thread: Option<&str>) -> Vec<Value> {
        let plan = interaction_mode == "plan";
        let paths = if runtime_mode == "full-access" && !plan { vec![] } else { self.paths_for(directory, &if plan { vec![agent.to_owned(), "plan".to_owned()] } else { vec![agent.to_owned()] }).await };
        let grants = if runtime_mode == "full-access" { vec![] } else { grants.to_vec() };
        let server = app_thread.map(mcp_server_name);
        session_rules(runtime_mode, interaction_mode, &paths, &grants, server.as_deref())
    }
    /// Writes the session's rules for a policy when they differ from what it has.
    async fn write_rules(&mut self, session_id: &str, runtime_mode: &str, interaction_mode: &str) -> Result<(), ApiError> {
        let Some(state) = self.sessions.get(session_id) else { return Ok(()) };
        let (directory, agent, grants, app_thread) = (state.directory.clone(), state.agent.clone(), state.grants.clone(), state.app_thread_id.clone());
        let rules = self.rules_for(&directory, &agent, &grants, runtime_mode, interaction_mode, Some(&app_thread)).await;
        if self.sessions[session_id].rules.as_ref() != Some(&rules) {
            self.session_call(Method::PATCH, session_id, "", &[], Some(json!({"permissions":rules})), None).await?;
            self.sessions.get_mut(session_id).unwrap().rules = Some(rules);
        }
        let state = self.sessions.get_mut(session_id).unwrap();
        state.runtime_mode = runtime_mode.to_owned();
        state.interaction_mode = interaction_mode.to_owned();
        Ok(())
    }

    // ---- the thread's session ----

    fn register(&mut self, provider_thread: Value, native: &Value, directory: &str, permissions: Option<Vec<Value>>, runtime_mode: &str, interaction_mode: &str) {
        let id = str_of(&native["id"]);
        let model = native["model"].as_object().map(|_| native["model"].clone());
        let app_thread = self.thread_id.clone();
        let state = self.sessions.entry(id.clone()).or_insert_with(|| Session { id: id.clone(), provider_thread: provider_thread.clone(), directory: directory.to_owned(), agent: "build".into(), model: None, rules: None, runtime_mode: runtime_mode.to_owned(), interaction_mode: interaction_mode.to_owned(), grants: vec![], unsettled: false, turn: None, stranded: HashSet::new(), mcp: None, instructions: None, app_thread_id: app_thread, calls: Default::default(), child: None, wakes: vec![], reports: Default::default(), stopped_children: HashSet::new() });
        state.provider_thread = provider_thread;
        state.model = model;
        state.directory = directory.to_owned();
        if let Some(agent) = native["agent"].as_str() {
            state.agent = agent.to_owned();
        }
        state.rules = permissions;
    }
    /// Creates the native session for a thread that has none, running OpenCode's default agent.
    async fn ensure_thread(&mut self, provider_thread: &Value, selection: &Value, runtime_mode: &str, interaction_mode: &str) -> Result<String, String> {
        let cwd = self.cwd.to_string_lossy().into_owned();
        self.read_models_once(&cwd).await;
        let model = model_ref(selection).ok_or_else(|| format!("OpenCode model '{}' must use provider/model format", str_of(&selection["model"])))?;
        let thread = self.thread_id.clone();
        let permissions = self.rules_for(&cwd, "build", &[], runtime_mode, interaction_mode, Some(&thread)).await;
        let created = self.call(Method::POST, "/api/session", &[], Some(json!({"location":{"directory":cwd},"model":model,"permissions":permissions})), None).await.map_err(|e| e.to_string())?;
        let native = created["data"].clone();
        let id = str_of(&native["id"]);
        if id.is_empty() {
            return Err("OpenCode did not name the session it created.".into());
        }
        self.register(provider_thread.clone(), &native, &cwd, Some(permissions), runtime_mode, interaction_mode);
        Ok(id)
    }
    /// `forkThread`: OpenCode forks whatever history the source has, cut before the message the first turn after the
    /// fork point prompted with. The fork keeps the source's model, location and rules, so it is given the target
    /// thread's mode and location. The source's thread may belong to another actor, so the server says whether it is busy.
    async fn fork_thread(&mut self, provider_thread: &Value, transfer: &Value, run_id: &str, runtime_mode: &str, interaction_mode: &str) -> Result<String, String> {
        let source_view = crate::codex_runtime::projection(&self.store, transfer["sourceThreadId"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
        let rows = |field: &str| source_view[field].as_array().cloned().unwrap_or_default();
        let source_thread = rows("providerThreads").into_iter().find(|thread| thread["id"] == provider_thread["forkedFrom"]["providerThreadId"]).ok_or_else(|| format!("Native fork transfer {} has no source provider execution.", str_of(&transfer["id"])))?;
        let source = source_thread["nativeThreadRef"]["nativeId"].as_str().ok_or("The source provider thread has no OpenCode session.")?.to_owned();
        self.ensure_quiet(&source, "fork").await?;
        let selected = match provider_thread["forkedFrom"].get("providerTurnId").filter(|id| !id.is_null()) {
            None => None,
            Some(id) => Some(rows("providerTurns").into_iter().find(|turn| turn["id"] == *id).ok_or_else(|| format!("OpenCode fork boundary turn {} was not found", str_of(id)))?),
        };
        let before = match &selected {
            None => None,
            Some(selected) => {
                let prompts = self.user_messages(&source).await.map_err(|e| e.to_string())?;
                boundary_after(&rows("providerTurns"), &source_thread["id"], Some(selected), &prompts)?
            }
        };
        let body = before.map_or_else(|| json!({}), |before| json!({"before":before}));
        let forked = self.session_call(Method::POST, &source, "/fork", &[], Some(body), None).await.map_err(|e| e.to_string())?["data"].clone();
        let id = str_of(&forked["id"]);
        if id.is_empty() {
            return Err("OpenCode did not name the session it forked.".into());
        }
        let cwd = self.cwd.to_string_lossy().into_owned();
        self.register(provider_thread.clone(), &forked, &cwd, forked["permissions"].as_array().cloned(), runtime_mode, interaction_mode);
        // The fork runs the target thread's mode, not whatever the source ran.
        self.write_rules(&id, runtime_mode, interaction_mode).await.map_err(|e| e.to_string())?;
        if forked["location"]["directory"].as_str().is_some_and(|there| there != cwd) {
            self.session_call(Method::POST, &id, "/move", &[], Some(json!({"directory":cwd})), None).await.map_err(|e| e.to_string())?;
        }
        let instance = str_of(&self.session["providerInstanceId"]);
        let consumed = crate::context_transfer::consume_native_fork(transfer, &instance, run_id, &thread_ref(&id), Utc::now());
        commit(&self.store, &self.thread_id, |_| Ok(vec![("context-transfer.updated", consumed.clone())])).map_err(|e| e.to_string())?;
        Ok(id)
    }
    /// Loads a thread whose native session exists: it may have run another mode, or be made by an earlier build.
    async fn resume_thread(&mut self, session_id: &str, provider_thread: &Value, runtime_mode: &str, interaction_mode: &str) -> Result<(), String> {
        let cwd = self.cwd.to_string_lossy().into_owned();
        self.read_models_once(&cwd).await;
        // 1.x session ids survive the upgrade; a server without this session fails the resume.
        let native = self.session_call(Method::GET, session_id, "", &[], None, None).await.map_err(|e| e.to_string())?["data"].clone();
        let loaded = self.sessions.contains_key(session_id);
        let permissions = native["permissions"].as_array().cloned();
        let directory = native["location"]["directory"].as_str().map(str::to_owned).unwrap_or_else(|| cwd.clone());
        self.register(provider_thread.clone(), &native, &cwd, permissions, runtime_mode, interaction_mode);
        let _ = directory;
        // OpenCode keeps no request across its own restart, but a server that outlived T3 may still wait on one T3 no longer shows.
        if !loaded {
            self.stop_leftover_requests(session_id).await;
        }
        self.write_rules(session_id, runtime_mode, interaction_mode).await.map_err(|e| e.to_string())?;
        // A thread moved to another worktree takes its session with it.
        if native["location"]["directory"].as_str().is_some_and(|there| there != cwd) {
            self.session_call(Method::POST, session_id, "/move", &[], Some(json!({"directory":cwd})), None).await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    async fn stop_leftover_requests(&mut self, session_id: &str) {
        let listed = tokio::time::timeout(ACTIVE_CHECK_TIMEOUT, async {
            let permissions = self.session_call(Method::GET, session_id, "/permission", &[], None, None).await?;
            let forms = self.session_call(Method::GET, session_id, "/form", &[], None, None).await?;
            Ok::<_, ApiError>((permissions["data"].as_array().cloned().unwrap_or_default(), forms["data"].as_array().cloned().unwrap_or_default()))
        })
        .await;
        let Ok(Ok((permissions, forms))) = listed else {
            tracing::warn!("Could not list an OpenCode session's open requests.");
            return;
        };
        if permissions.is_empty() && forms.is_empty() {
            return;
        }
        // The stopped run's end is not the next turn's.
        if let Some(state) = self.sessions.get_mut(session_id) {
            state.unsettled = true;
        }
        for request in &permissions {
            self.stop_stale_request(session_id, "permission", &str_of(&request["id"])).await;
        }
        for form in &forms {
            self.stop_stale_request(session_id, "form", &str_of(&form["id"])).await;
        }
    }
    /// A reject without a message and a cancelled form both end OpenCode's execution.
    async fn stop_stale_request(&self, session_id: &str, kind: &str, id: &str) {
        let result = if kind == "permission" {
            self.session_call(Method::POST, session_id, &format!("/permission/{}/reply", segment(id)), &[], Some(json!({"decision":"reject"})), Some(REQUEST_REPLY_TIMEOUT)).await
        } else {
            self.session_call(Method::DELETE, session_id, &format!("/form/{}", segment(id)), &[], None, Some(REQUEST_REPLY_TIMEOUT)).await
        };
        if let Err(cause) = result {
            if !matches!(cause.tag(), Some("PermissionNotFoundError" | "SessionNotFoundError" | "FormNotFoundError" | "FormAlreadySettledError")) {
                tracing::warn!(%cause, "Could not stop a stale OpenCode request");
            }
        }
    }

    // ---- starting a turn ----

    async fn start_turn(&mut self, run_id: &str) -> Result<(), String> {
        if self.detached {
            self.reattach()?;
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
        let continuation = crate::provider_continuation::is_continuation(&message);

        // The native session: created for a thread that has none, resumed when its selection or policy changed.
        let saved = provider_thread["nativeThreadRef"]["nativeId"].as_str().map(str::to_owned);
        let key = json!({"session":saved,"selection":selection,"runtimeMode":runtime_mode,"interactionMode":interaction_mode,"cwd":self.cwd});
        // A fork's first turn forks the source's session instead of making one.
        let fork_transfer = if saved.is_none() && !provider_thread["forkedFrom"].is_null() { crate::context_transfer::native_fork_transfer(&thread_view, &json!(run_id)).cloned() } else { None };
        let session_id = match saved {
            None if fork_transfer.is_some() => {
                let transfer = fork_transfer.as_ref().unwrap();
                let id = self.fork_thread(&provider_thread, transfer, run_id, &runtime_mode, &interaction_mode).await?;
                self.loaded_key = Some(json!({"session":id,"selection":selection,"runtimeMode":runtime_mode,"interactionMode":interaction_mode,"cwd":self.cwd}));
                id
            }
            None => {
                let id = self.ensure_thread(&provider_thread, &selection, &runtime_mode, &interaction_mode).await?;
                self.loaded_key = Some(json!({"session":id,"selection":selection,"runtimeMode":runtime_mode,"interactionMode":interaction_mode,"cwd":self.cwd}));
                id
            }
            Some(id) => {
                if self.loaded_key.as_ref() != Some(&key) || !self.sessions.contains_key(&id) {
                    self.resume_thread(&id, &provider_thread, &runtime_mode, &interaction_mode).await?;
                    self.loaded_key = Some(key);
                }
                id
            }
        };
        // OpenCode already ran this turn on its own; it prompts nothing.
        if continuation {
            return self.run_wake(&session_id, run_id, &selection, &attempt_id).await;
        }
        // After a timed-out Stop the server says whether that run is gone. A run still going is stopped again and this
        // turn fails so it can be sent again; a run that is gone may still have its end on the stream, which the turn skips.
        let after_unsettled = self.sessions[&session_id].unsettled;
        let mut still_stopping = false;
        if after_unsettled {
            let active = self.call(Method::GET, "/api/session/active", &[], None, Some(ACTIVE_CHECK_TIMEOUT)).await.map_err(|e| e.to_string())?;
            still_stopping = active["data"].get(&session_id).is_some();
            if still_stopping {
                let _ = self.session_call(Method::POST, &session_id, "/interrupt", &[], None, Some(INTERRUPT_TIMEOUT)).await;
            } else {
                self.sessions.get_mut(&session_id).unwrap().unsettled = false;
            }
        }
        // A turn T3 will not run still starts and fails, so the refusal is what the user reads.
        let refusal = if still_stopping {
            Some((STILL_STOPPING.to_owned(), "provider_error"))
        } else if model_ref(&selection).is_none() {
            Some((format!("OpenCode model '{}' must use provider/model format", str_of(&selection["model"])), "validation_error"))
        } else {
            None
        };
        if refusal.is_none() {
            self.prepare_session(&session_id, &selection, &runtime_mode, &interaction_mode).await.map_err(|e| e.to_string())?;
            let text = crate::attachment_prompt::provider_text(&self.store, &message);
            self.prepare_turn(&session_id, &selection, &text).await.map_err(|e| e.to_string())?;
        }
        let native_turn = if continuation { wake_turn_id(&session_id, &attempt_id) } else { turn_prompt_id(&session_id, &attempt_id) };
        self.begin_turn(&session_id, run_id, &selection, &native_turn, after_unsettled, continuation).map_err(|e| e.to_string())?;
        if let Some((message, class)) = refusal {
            return self.finish_turn(&session_id, Terminal::Failed(failure(&message, class)), false).await.map_err(|e| e.to_string());
        }
        if let Some(turn) = self.sessions.get_mut(&session_id).and_then(|state| state.turn.as_mut()) {
            turn.unsent = true;
        }
        // An execution OpenCode is running on its own takes this prompt at its next step, so this turn is that execution from here on.
        self.take_running_wake(&session_id).await.map_err(|e| e.to_string())?;
        let sent = self.submit(&session_id, &attempt_id, &message, &run).await;
        match sent {
            Ok(()) => Ok(()),
            Err(cause) => {
                // Deleted outside T3: the thread is broken, and forgetting it makes the next turn resume, fail, and recreate it.
                if cause.tag() == Some("SessionNotFoundError") {
                    self.finish_turn(&session_id, Terminal::Failed(failure("The OpenCode session no longer exists. Send the message again to continue in a new session.", "provider_error")), true).await.map_err(|e| e.to_string())?;
                    self.sessions.remove(&session_id);
                    self.loaded_key = None;
                    return Ok(());
                }
                if self.sessions.get(&session_id).is_some_and(|state| state.turn.is_some()) {
                    // Without a clear rejection the server may have taken the prompt, so the next turn checks before it prompts again.
                    if !cause.tag().is_some_and(|tag| CLEAR_PROMPT_REJECTIONS.contains(&tag)) {
                        self.sessions.get_mut(&session_id).unwrap().unsettled = true;
                    }
                    self.finish_turn(&session_id, Terminal::Failed(failure(&cause.to_string(), "provider_error")), false).await.map_err(|e| e.to_string())?;
                }
                Ok(())
            }
        }
    }
    /// The agent, rules and model a turn runs with.
    async fn prepare_session(&mut self, session_id: &str, selection: &Value, runtime_mode: &str, interaction_mode: &str) -> Result<(), ApiError> {
        // Plan mode is OpenCode's `plan` agent, switched before the rules are written so they keep that agent's own path allows.
        let agent = if interaction_mode == "plan" { "plan" } else { "build" };
        if self.sessions[session_id].agent != agent {
            self.session_call(Method::POST, session_id, "/agent", &[], Some(json!({"agent":agent})), None).await?;
            self.sessions.get_mut(session_id).unwrap().agent = agent.to_owned();
        }
        // The thread's mode may have changed since the session was loaded.
        self.write_rules(session_id, runtime_mode, interaction_mode).await?;
        let model = model_ref(selection).expect("checked by the caller");
        if !same_model(&model, self.sessions[session_id].model.as_ref()) {
            self.session_call(Method::POST, session_id, "/model", &[], Some(json!({"model":model})), None).await?;
            self.sessions.get_mut(session_id).unwrap().model = Some(model);
        }
        // Steers a stopped turn never delivered would reach the model first, and a revert a failed rollback left
        // staged would be committed.
        self.cancel_stranded_steers(session_id).await;
        if self.staged_reverts.contains(session_id) {
            self.clear_revert(session_id).await;
        }
        Ok(())
    }
    /// T3's MCP server for this thread, and the per-turn instructions. The MCP server is registered for the session's
    /// directory under the thread's own name and credential (the session rules allow only it). OpenCode 2 has no
    /// per-prompt system field, so the instructions are a session instructions entry, only rewritten when it changes.
    async fn prepare_turn(&mut self, session_id: &str, selection: &Value, _text: &str) -> Result<(), ApiError> {
        let directory = self.cwd.to_string_lossy().into_owned();
        let name = mcp_server_name(&self.thread_id);
        // An external server may not reach T3's MCP endpoint.
        let wanted = match (&self.mcp, self.external()) {
            (Some(mcp), false) => Some(McpRegistration { name: name.clone(), directory: directory.clone(), credential: mcp.authorization_header.clone() }),
            _ => None,
        };
        let state = &self.sessions[session_id];
        let stale = state.mcp.as_ref().filter(|current| wanted.as_ref().is_none_or(|wanted| current.directory != wanted.directory || current.credential != wanted.credential)).map(|current| (current.name.clone(), current.directory.clone()));
        if let Some((name, directory)) = stale {
            self.remove_mcp(&name, &directory).await;
            self.sessions.get_mut(session_id).unwrap().mcp = None;
        }
        // T3's tools are an addition: a server that cannot add them still runs the turn.
        if let (Some(wanted), Some(config)) = (wanted, self.mcp.clone()) {
            if self.sessions[session_id].mcp.is_none() {
                let body = json!({"config":{"type":"remote","url":config.endpoint,"headers":{"Authorization":wanted.credential},"oauth":false}});
                let query = vec![("location[directory]".to_owned(), directory.clone())];
                match self.call(Method::PUT, &format!("/api/experimental/mcp/{}", segment(&wanted.name)), &query, Some(body), Some(INVENTORY_TIMEOUT)).await {
                    Ok(_) => self.sessions.get_mut(session_id).unwrap().mcp = Some(wanted),
                    Err(cause) => tracing::warn!(%cause, "Could not add T3 Code's MCP server to OpenCode."),
                }
            }
        }
        let has_mcp = self.sessions[session_id].mcp.is_some();
        let model = selection["model"].as_str().unwrap_or("");
        let mut parts = vec![crate::provider_instructions::runtime("OpenCode", model, None, None)];
        if has_mcp {
            parts.push(crate::provider_instructions::codex_context("", "", false, false)["t3_code_orchestration"]["value"].as_str().unwrap_or("").to_owned());
        }
        let instructions = parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n");
        if self.sessions[session_id].instructions.as_deref() != Some(instructions.as_str()) {
            self.call(Method::PUT, &format!("/api/experimental/session/{}/instructions/entries/{}", segment(session_id), segment(INSTRUCTIONS_KEY)), &[], Some(json!({"value":instructions})), None).await?;
            self.sessions.get_mut(session_id).unwrap().instructions = Some(instructions);
        }
        Ok(())
    }
    async fn remove_mcp(&self, name: &str, directory: &str) {
        let query = vec![("location[directory]".to_owned(), directory.to_owned())];
        let _ = self.call(Method::DELETE, &format!("/api/experimental/mcp/{}", segment(name)), &query, None, Some(INVENTORY_TIMEOUT)).await;
    }
    /// Installs a turn T3 started; every path after it ends the turn with a terminal.
    fn begin_turn(&mut self, session_id: &str, run_id: &str, selection: &Value, native_turn: &str, awaiting_start: bool, continuation: bool) -> Result<(), StoreError> {
        let provider_turn_id = uuid::Uuid::new_v4().to_string();
        let mut captured = TurnContext::capture(&view(&self.store, &self.thread_id)?, run_id)?;
        let now = at(Utc::now());
        let session = self.session_id.clone();
        let native = native_turn.to_owned();
        let model_id = self.sessions[session_id].model.clone();
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
            if let Some(model) = &model_id {
                thread["nativeMetadata"] = json!({"modelSelection":reported_selection(&run["providerInstanceId"], model)});
            }
            let turn = json!({"id":provider_turn_id,"providerThreadId":run["providerThreadId"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":{"driver":DRIVER,"nativeId":native,"strength":"weak"},"ordinal":run["ordinal"],"status":"running","startedAt":now,"completedAt":null});
            Ok(vec![("provider-thread.updated", thread), ("provider-turn.updated", turn), ("run.updated", run), ("run-attempt.updated", attempt), ("node.updated", node)])
        })?;
        captured.provider_turn_id = json!(provider_turn_id);
        let view = view(&self.store, &self.thread_id)?;
        let directory = self.sessions[session_id].directory.clone();
        let window = self.window_of(&directory, selection["model"].as_str().unwrap_or(""));
        let mut projector = Projector::new(&self.thread_id, captured, &view, selection.clone(), "", window)?;
        projector.stream_paragraphs(self.streams_paragraphs(&view));
        let status = if self.pending.is_empty() { "running" } else { "waiting" };
        self.set_session_status(status, None)?;
        self.sessions.get_mut(session_id).unwrap().turn = Some(Turn { projector, native_turn: native_turn.to_owned(), before: None, interrupted: false, unsent: false, steers: HashSet::new(), settled_inbox: HashSet::new(), held_end: None, awaiting_start, continuation });
        Ok(())
    }
    /// The session stays `ready` while it lives: a response to a request is accepted only for a ready session, as for the
    /// other adapters. A broken stream marks it `error`.
    /// The thread's project may set its own response streaming mode over the environment's.
    fn streams_paragraphs(&self, view: &Value) -> bool {
        let settings = self.providers.settings();
        let project: Option<t3_contracts::ProjectId> = view["thread"]["projectId"].as_str().and_then(|id| id.parse().ok());
        let mode = project.and_then(|project| settings.project_settings_overrides.get(&project).and_then(|entry| entry.response_streaming_mode)).unwrap_or(settings.response_streaming_mode);
        mode == t3_contracts::ResponseStreamingMode::Paragraph
    }
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

    /// What a turn sends: `/compact` compacts, `/name args` naming a workspace command runs it, and anything else is a
    /// prompt with the `$skill`s it names attached. Commands and skills are read from the session's directory only when
    /// the text could use them. A turn stopped meanwhile sends nothing.
    async fn submit(&mut self, session_id: &str, attempt_id: &str, message: &Value, run: &Value) -> Result<(), ApiError> {
        let _ = run;
        let text = message["text"].as_str().unwrap_or("").trim().to_owned();
        let bare = message["attachments"].as_array().is_none_or(Vec::is_empty);
        let id = turn_prompt_id(session_id, attempt_id);
        let sending = |actor: &mut Actor| -> bool {
            let Some(turn) = actor.sessions.get_mut(session_id).and_then(|state| state.turn.as_mut()) else { return false };
            if turn.interrupted {
                return false;
            }
            turn.unsent = false;
            true
        };
        if bare && text == "/compact" {
            if !sending(self) {
                return Ok(());
            }
            return self.session_call(Method::POST, session_id, "/compact", &[], Some(json!({"id":id})), None).await.map(|_| ());
        }
        let directory = self.cwd.to_string_lossy().into_owned();
        let location = vec![("location[directory]".to_owned(), directory)];
        if let Some((name, args)) = bare.then(|| command_of(&text)).flatten() {
            let commands = self.call(Method::GET, "/api/command", &location, None, Some(INVENTORY_TIMEOUT)).await.ok().and_then(|list| list["data"].as_array().cloned()).unwrap_or_default();
            if commands.iter().any(|entry| entry["name"] == name.as_str()) {
                // `session.command` takes no id of T3's and its answer carries none, so the turn remembers where the history stood before it.
                let newest = self.session_call(Method::GET, session_id, "/message", &[("order".into(), "desc".into()), ("limit".into(), "1".into())], None, None).await?;
                let before = newest["data"][0]["id"].as_str().map(str::to_owned);
                if let Some(turn) = self.sessions.get_mut(session_id).and_then(|state| state.turn.as_mut()) {
                    turn.before = Some(before);
                }
                if !sending(self) {
                    return Ok(());
                }
                return self.session_call(Method::POST, session_id, "/command", &[], Some(json!({"name":name,"text":args})), None).await.map(|_| ());
            }
        }
        // Skills are read only when the text names one at all.
        let skills = if skill_mentions(&text).is_empty() {
            vec![]
        } else {
            let known: HashSet<String> = self.call(Method::GET, "/api/skill", &location, None, Some(INVENTORY_TIMEOUT)).await.ok().and_then(|list| list["data"].as_array().cloned()).unwrap_or_default().iter().filter_map(|skill| skill["id"].as_str().map(str::to_owned)).collect();
            skills_named(&text, &known)
        };
        if !sending(self) {
            return Ok(());
        }
        let prompt = crate::attachment_prompt::provider_text(&self.store, message).trim().to_owned();
        let mut body = json!({"id":id,"text":prompt});
        if !skills.is_empty() {
            body["skills"] = json!(skills.iter().map(|id| json!({"id":id})).collect::<Vec<_>>());
        }
        self.session_call(Method::POST, session_id, "/prompt", &[], Some(body), None).await.map(|_| ())
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
        let Some(session_id) = self.sessions.iter().find(|(_, state)| state.turn.as_ref().is_some_and(|turn| turn.projector.context().provider_turn_id == provider_turn_id && !turn.interrupted)).map(|(id, _)| id.clone()) else {
            return Err(format!("OpenCode turn {} is not active", str_of(&provider_turn_id)));
        };
        let inbox = steer_prompt_id(&session_id, &str_of(&message["id"]));
        // A retried steer that already reached the model is not sent again.
        if self.sessions[&session_id].turn.as_ref().is_some_and(|turn| turn.settled_inbox.contains(&inbox)) {
            return Ok(());
        }
        let state = self.sessions.get_mut(&session_id).unwrap();
        state.turn.as_mut().unwrap().steers.insert(inbox.clone());
        state.stranded.remove(&inbox);
        let text = crate::attachment_prompt::provider_text(&self.store, &message).trim().to_owned();
        let sent = self.session_call(Method::POST, &session_id, "/prompt", &[], Some(json!({"id":inbox,"text":text,"delivery":"steer"})), None).await;
        if let Err(cause) = sent {
            let state = self.sessions.get_mut(&session_id).unwrap();
            if let Some(turn) = state.turn.as_mut() {
                turn.steers.remove(&inbox);
            }
            // Without a clear refusal OpenCode may hold it; the next prompt takes it back.
            if !cause.tag().is_some_and(|tag| CLEAR_PROMPT_REJECTIONS.contains(&tag)) {
                state.stranded.insert(inbox);
            }
            self.end_if_settled(&session_id).await.map_err(|e| e.to_string())?;
            return Err(cause.to_string());
        }
        Ok(())
    }
    /// Takes back steers a finished turn left in OpenCode's inbox. One whose cancel fails may still be there.
    async fn cancel_stranded_steers(&mut self, session_id: &str) {
        let stranded: Vec<String> = self.sessions.get(session_id).map(|state| state.stranded.iter().cloned().collect()).unwrap_or_default();
        for inbox in stranded {
            let cancelled = self.session_call(Method::DELETE, session_id, &format!("/inbox/{}", segment(&inbox)), &[], None, Some(REQUEST_REPLY_TIMEOUT)).await;
            match cancelled {
                Ok(_) => {
                    self.sessions.get_mut(session_id).unwrap().stranded.remove(&inbox);
                }
                Err(cause) => tracing::warn!(%cause, "Could not take back an OpenCode steer."),
            }
        }
    }
    /// Ends a turn held for its steers once none is left to deliver.
    async fn end_if_settled(&mut self, session_id: &str) -> Result<(), StoreError> {
        let held = self.sessions.get(session_id).and_then(|state| state.turn.as_ref()).filter(|turn| turn.steers.is_empty()).and_then(|turn| turn.held_end.clone());
        match held {
            Some(end) => self.finish_turn(session_id, end, false).await,
            None => Ok(()),
        }
    }

    async fn interrupt(&mut self, run_id: Option<&str>) -> Result<(), String> {
        let Some(session_id) = self.sessions.iter().find(|(_, state)| state.turn.as_ref().is_some_and(|turn| Some(turn.projector.context().run_id.as_str()) == run_id)).map(|(id, _)| id.clone()) else {
            return Err("Target run is no longer active.".into());
        };
        self.sessions.get_mut(&session_id).unwrap().turn.as_mut().unwrap().interrupted = true;
        // The session answers with `session.execution.interrupted`, which ends the turn. A server that does not answer
        // in time is stuck, so the turn ends here instead of waiting on it.
        let reply = self.session_call(Method::POST, &session_id, "/interrupt", &[], None, Some(INTERRUPT_TIMEOUT)).await;
        let reply = match reply {
            Err(ApiError::Transport(detail)) if detail.contains("timed out") => {
                self.sessions.get_mut(&session_id).unwrap().unsettled = true;
                return self.finish_turn(&session_id, Terminal::Interrupted, false).await.map_err(|e| e.to_string());
            }
            Err(cause) => {
                // A Stop that never reached the server stopped nothing.
                if let Some(turn) = self.sessions.get_mut(&session_id).and_then(|state| state.turn.as_mut()) {
                    turn.interrupted = false;
                }
                return Err(cause.to_string());
            }
            Ok(reply) => reply,
        };
        let interrupted = reply["interrupted"] == true;
        let Some(turn) = self.sessions.get(&session_id).and_then(|state| state.turn.as_ref()) else { return Ok(()) };
        // Between the executions a late steer spans nothing runs yet. The steers are taken back, and a run they already started is skipped.
        if !interrupted && turn.held_end.is_some() {
            self.sessions.get_mut(&session_id).unwrap().unsettled = true;
            self.finish_turn(&session_id, Terminal::Interrupted, false).await.map_err(|e| e.to_string())?;
            self.cancel_stranded_steers(&session_id).await;
            return Ok(());
        }
        // Nothing was running because the turn has not sent its request yet: it ends here, and never sends it.
        if !interrupted && turn.unsent {
            return self.finish_turn(&session_id, Terminal::Interrupted, false).await.map_err(|e| e.to_string());
        }
        // Nothing was running. Unless the execution already ended (its event is on the way), the turn is still open and nothing stopped.
        if !interrupted {
            self.sessions.get_mut(&session_id).unwrap().turn.as_mut().unwrap().interrupted = false;
            return Err(format!("OpenCode session {session_id} had nothing running to stop"));
        }
        Ok(())
    }

    /// `runtime-request.respond`: the persisted decision or answers go to OpenCode.
    async fn respond(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id {
            return Err("Runtime callback belongs to a different provider session.".into());
        }
        let request_id = effect.request["requestId"].as_str().ok_or("Request id missing.")?.to_owned();
        let Some(entry) = self.pending.get_mut(&request_id).filter(|entry| !entry.answering) else {
            return Err(format!("No pending OpenCode request {request_id}"));
        };
        let decision = effect.request["decision"].as_str().map(str::to_owned);
        let answers = effect.request.get("answers").cloned().filter(|answers| answers.is_object());
        if matches!(entry.native, Native::Permission { .. }) && decision.is_none() {
            return Err(format!("OpenCode approval request {request_id} requires a decision"));
        }
        entry.answering = true;
        let (asker, native) = (entry.session.clone(), match &entry.native {
            Native::Permission { id, action, resources, save } => Native::Permission { id: id.clone(), action: action.clone(), resources: resources.clone(), save: save.clone() },
            Native::Form { id, form } => Native::Form { id: id.clone(), form: form.clone() },
        });
        // OpenCode's own "always" saves a grant for the whole project, so a session-wide answer is a rule on this
        // session instead. The grant is best effort: this request is answered either way.
        if let (Native::Permission { action, save, .. }, Some("acceptForSession" | "acceptAlways")) = (&native, decision.as_deref()) {
            if let Some(state) = self.sessions.get_mut(&asker) {
                for resource in save {
                    if !state.grants.iter().any(|grant| grant["action"] == action.as_str() && grant["resource"] == resource.as_str()) {
                        state.grants.push(json!({"action":action,"resource":resource,"effect":"allow"}));
                    }
                }
                let (mode, interaction) = (state.runtime_mode.clone(), state.interaction_mode.clone());
                if let Err(cause) = self.write_rules(&asker, &mode, &interaction).await {
                    tracing::warn!(%cause, "Could not write a session grant to OpenCode.");
                }
            }
        }
        // Best effort: the decline stands without the note.
        if let (Native::Permission { action, resources, .. }, Some("decline")) = (&native, decision.as_deref()) {
            let note = declined_note(action, resources);
            let _ = self.session_call(Method::POST, &asker, "/synthetic", &[], Some(json!({"text":note,"delivery":"steer","resume":false})), Some(REQUEST_REPLY_TIMEOUT)).await;
        }
        let delivered = match &native {
            Native::Permission { id, .. } => {
                let mut body = json!({"decision":if decision.as_deref().is_some_and(approves) { "once" } else { "reject" }});
                // Without a message OpenCode ends the whole run, which is Cancel.
                if decision.as_deref() == Some("decline") {
                    body["message"] = json!(DECLINED);
                }
                self.deliver(Method::POST, &asker, &format!("/permission/{}/reply", segment(id)), Some(body)).await
            }
            Native::Form { id, form } => match (answers, decision.as_deref()) {
                (Some(answers), Some(decision)) if decision != "decline" && decision != "cancel" => self.deliver(Method::POST, &asker, &format!("/form/{}/reply", segment(id)), Some(json!({"answer":form_answer(form, &answers)}))).await,
                (Some(answers), None) => self.deliver(Method::POST, &asker, &format!("/form/{}/reply", segment(id)), Some(json!({"answer":form_answer(form, &answers)}))).await,
                _ => self.deliver(Method::DELETE, &asker, &format!("/form/{}", segment(id)), None).await,
            },
        };
        self.forget_request(&request_id).map_err(|e| e.to_string())?;
        if !delivered {
            self.abandon_request(&asker, "answer not delivered").await.map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    /// Whether an answer to a paused request reached the server, trying twice. An answer to a request OpenCode already
    /// dropped counts as delivered: nothing waits on it.
    async fn deliver(&self, method: Method, session_id: &str, tail: &str, body: Option<Value>) -> bool {
        for _ in 0..2 {
            match self.session_call(method.clone(), session_id, tail, &[], body.clone(), Some(REQUEST_REPLY_TIMEOUT)).await {
                Ok(_) => return true,
                Err(cause) if matches!(cause.tag(), Some("PermissionNotFoundError" | "SessionNotFoundError" | "FormNotFoundError" | "FormAlreadySettledError")) => return true,
                Err(_) => {}
            }
        }
        false
    }
    /// Stops tracking a request; the session is waiting only while any remain.
    fn forget_request(&mut self, request_id: &str) -> Result<bool, StoreError> {
        if self.pending.remove(request_id).is_none() {
            return Ok(false);
        }
        if self.pending.is_empty() && self.session["status"] == "waiting" {
            self.set_session_status("running", None)?;
        }
        Ok(true)
    }
    /// Ends the turn a request T3 could not answer would block, and stops the session: OpenCode waits on an unanswered
    /// request forever. The stopped run's end is its own, not the next turn's.
    async fn abandon_request(&mut self, session_id: &str, reason: &str) -> Result<(), StoreError> {
        tracing::warn!(reason, "Could not answer an OpenCode request; ending its turn.");
        if self.sessions.get(session_id).is_none_or(|state| state.turn.is_none()) {
            return Ok(());
        }
        self.sessions.get_mut(session_id).unwrap().unsettled = true;
        self.finish_turn(session_id, Terminal::Failed(failure("OpenCode is waiting on a request T3 Code couldn't answer.", "provider_error")), false).await?;
        let _ = self.session_call(Method::POST, session_id, "/interrupt", &[], None, Some(Duration::from_secs(2))).await;
        Ok(())
    }

    // ---- rollback ----

    /// `rollbackThread`: OpenCode stages a revert before the user message the first later turn prompted with, then
    /// commits it. Files stay where they are: T3 restores its own checkpoint when the user asked for files.
    async fn rollback(&mut self, effect: &Effect) -> Result<(), String> {
        let prepared = crate::checkpoint_rollback::prepare(&self.store, effect)?;
        let provider_thread = if prepared.needs_provider() {
            let session_id = prepared.provider_thread["nativeThreadRef"]["nativeId"].as_str().ok_or_else(|| format!("Provider thread {} is missing a native OpenCode session id.", str_of(&prepared.provider_thread["id"])))?.to_owned();
            self.ensure_quiet(&session_id, "roll back").await?;
            let prompts = self.user_messages(&session_id).await.map_err(|e| e.to_string())?;
            let kept = (prepared.target["type"] == "provider_turn").then(|| prepared.target["providerTurn"].clone());
            let boundary = boundary_after(&prepared.provider_thread_turns, &prepared.provider_thread["id"], kept.as_ref(), &prompts)?;
            if let Some(boundary) = boundary {
                self.roll_back_to(&session_id, &boundary).await.map_err(|e| e.to_string())?;
            }
            self.snapshot_thread(&prepared.provider_thread, &session_id).await.map_err(|e| e.to_string())?
        } else {
            prepared.provider_thread.clone()
        };
        crate::checkpoint_rollback::finish(&self.store, effect, prepared, provider_thread).await
    }
    /// Refuses to cut a session's history while something writes to it: a turn of T3's, an execution seen running on
    /// the stream, or a run on the server this runtime does not own.
    async fn ensure_quiet(&mut self, session_id: &str, verb: &str) -> Result<(), String> {
        let busy_here = self.sessions.get(session_id).is_some_and(|state| state.turn.is_some()) || self.busy.contains(session_id);
        let unknown = self.sessions.get(session_id).is_none_or(|state| state.unsettled);
        let busy_there = !busy_here && unknown && self.call(Method::GET, "/api/session/active", &[], None, Some(ACTIVE_CHECK_TIMEOUT)).await.map_err(|e| e.to_string())?["data"].get(session_id).is_some();
        if busy_here || busy_there {
            return Err(format!("Cannot {verb} OpenCode session {session_id} while it is still working"));
        }
        Ok(())
    }
    /// The ids rollback and fork can cut before: the session's prompts, and the reports continuations answer.
    async fn user_messages(&self, session_id: &str) -> Result<HashSet<String>, ApiError> {
        let messages = paginate(self.api()?, &format!("/api/session/{}/message", segment(session_id)), vec![("limit".into(), "100".into())], None).await?;
        Ok(messages.iter().filter(|message| matches!(message["type"].as_str(), Some("user" | "synthetic"))).filter_map(|message| message["id"].as_str().map(str::to_owned)).collect())
    }
    /// Truncates the session's history before `boundary`: OpenCode stages the revert, then commits it. If either fails
    /// the stage is cleared, since OpenCode commits a staged revert on the next prompt.
    async fn roll_back_to(&mut self, session_id: &str, boundary: &str) -> Result<(), ApiError> {
        let staged = self.session_call(Method::POST, session_id, "/revert/stage", &[], Some(json!({"messageID":boundary,"files":false})), Some(REQUEST_REPLY_TIMEOUT)).await;
        let committed = match staged {
            Ok(_) => self.session_call(Method::POST, session_id, "/revert/commit", &[], None, Some(REQUEST_REPLY_TIMEOUT)).await,
            Err(cause) => Err(cause),
        };
        if let Err(cause) = committed {
            self.clear_revert(session_id).await;
            return Err(cause);
        }
        Ok(())
    }
    /// Clears a staged revert and waits out the empty execution `clear` runs, which is no turn of T3's.
    async fn clear_revert(&mut self, session_id: &str) {
        self.clearing.insert(session_id.to_owned());
        let cleared = self.session_call(Method::DELETE, session_id, "/revert", &[], None, Some(REQUEST_REPLY_TIMEOUT)).await;
        if cleared.is_ok() {
            // 2.0.18 wakes the session after every clear, with or without a stage, so that execution always comes.
            let waited = tokio::time::timeout(REQUEST_REPLY_TIMEOUT, async {
                while self.clearing.contains(session_id) {
                    if !self.pump_frame().await {
                        break;
                    }
                }
            })
            .await;
            if waited.is_err() {
                tracing::warn!("OpenCode did not finish clearing a staged revert.");
            }
        }
        self.clearing.remove(session_id);
        if cleared.is_ok() {
            self.staged_reverts.remove(session_id);
        } else {
            self.staged_reverts.insert(session_id.to_owned());
        }
    }
    /// The session's history as a provider thread row: the head is the last user message OpenCode kept.
    async fn snapshot_thread(&self, provider_thread: &Value, session_id: &str) -> Result<Value, ApiError> {
        let history = paginate(self.api()?, &format!("/api/session/{}/message", segment(session_id)), vec![("order".into(), "asc".into()), ("limit".into(), "100".into())], None).await?;
        let last_user = history.iter().rev().find(|message| message["type"] == "user").and_then(|message| message["id"].as_str());
        let mut thread = provider_thread.clone();
        thread["providerSessionId"] = json!(self.session_id);
        thread["nativeConversationHeadRef"] = last_user.map_or(Value::Null, |id| projection::native_ref(id, "weak"));
        thread["status"] = json!("idle");
        thread["updatedAt"] = json!(at(Utc::now()));
        Ok(thread)
    }

    // ---- detaching ----

    async fn detach(&mut self, effect: &Effect) -> Result<(), String> {
        if effect.request["providerSessionId"] != self.session_id || self.detached {
            return Ok(());
        }
        let active: Vec<String> = self.sessions.iter().filter(|(_, state)| state.turn.is_some()).map(|(id, _)| id.clone()).collect();
        for id in active {
            if let Some(turn) = self.sessions.get_mut(&id).and_then(|state| state.turn.as_mut()) {
                turn.interrupted = true;
            }
            let _ = self.session_call(Method::POST, &id, "/interrupt", &[], None, Some(Duration::from_secs(1))).await;
            self.finish_turn(&id, Terminal::Interrupted, false).await.map_err(|e| e.to_string())?;
        }
        self.release().await;
        self.detached = true;
        Ok(())
    }
    /// Gives the server back: T3's MCP registrations are removed and the stream is dropped.
    async fn release(&mut self) {
        let registrations: Vec<(String, String)> = self.sessions.values().filter_map(|state| state.mcp.as_ref().map(|mcp| (mcp.name.clone(), mcp.directory.clone()))).collect();
        for (name, directory) in registrations {
            self.remove_mcp(&name, &directory).await;
        }
        for state in self.sessions.values_mut() {
            state.mcp = None;
        }
        self.pending.clear();
        self.live = None;
    }
    fn reattach(&mut self) -> Result<(), String> {
        Err("The OpenCode session was released; the next effect starts a new one.".into())
    }

    // ---- events ----

    /// Receives and handles one frame; false when there is no stream.
    async fn pump_frame(&mut self) -> bool {
        let frame = match self.live.as_mut() {
            Some(live) => live.frames.recv().await,
            None => return false,
        };
        self.handle_frame(frame).await.unwrap_or_else(|cause| {
            tracing::error!(%cause, "OpenCode provider event could not be persisted");
            false
        })
    }
    async fn handle_frame(&mut self, frame: Option<Frame>) -> Result<bool, StoreError> {
        match frame {
            Some(Frame::Event(event)) => {
                self.handle_event(&event).await?;
                Ok(true)
            }
            Some(Frame::Closed) | None => {
                self.reconnect_after_loss().await?;
                Ok(true)
            }
        }
    }
    fn commit_events(&self, events: projection::Events) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        commit(&self.store, &self.thread_id, |_| Ok(events))
    }
    async fn handle_event(&mut self, event: &Value) -> Result<(), StoreError> {
        let kind = event["type"].as_str().unwrap_or("");
        // The native log keeps the shape of each server-sent event, not what it said.
        if let Some(logger) = crate::provider_event_log::native(&self.store) {
            crate::provider_event_log::write_protocol_summary(&logger, DRIVER, Some("opencode-sdk.sse"), &self.instance.instance_id, &self.session_id, &self.thread_id, "incoming", "notification", kind, event);
        }
        let session_id = session_of(event);
        // `revert.clear` wakes the session into an empty execution of its own. It is no run of T3's.
        if let Some(sid) = session_id.as_deref().filter(|sid| self.clearing.contains(*sid)) {
            if is_execution_end(kind) {
                self.clearing.remove(sid);
                return Ok(());
            }
            if kind == "session.execution.started" {
                return Ok(());
            }
        }
        if let Some(sid) = &session_id {
            if kind == "session.execution.started" {
                self.busy.insert(sid.clone());
            } else if is_execution_end(kind) {
                self.busy.remove(sid);
            }
        }
        // A subagent's session: named on its parent's `subagent` call just after.
        if kind == "session.created" && event["data"]["parentID"].is_string() {
            let (child, parent) = (str_of(&event["data"]["sessionID"]), str_of(&event["data"]["parentID"]));
            if self.sessions.contains_key(&parent) {
                let root = self.root_of(&parent);
                self.child_owners.insert(child.clone(), root);
                self.announced.insert(child, event["data"].clone());
            }
            return Ok(());
        }
        // What a held execution does, and what the subagents it starts do, waits for the turn that takes it.
        if let Some(sid) = &session_id {
            if let Some(holder) = self.holder_of(sid) {
                let ends = *sid == holder && (is_execution_end(kind));
                if let Some(wake) = self.sessions.get_mut(&holder).and_then(|state| state.wakes.last_mut()) {
                    wake.events.push(event.clone());
                    if ends {
                        wake.running = false;
                    }
                }
                return Ok(());
            }
        }
        self.route(event, kind, session_id).await
    }
    /// Handles one event for its session, as the turn running there sees it.
    async fn route(&mut self, event: &Value, kind: &str, session_id: Option<String>) -> Result<(), StoreError> {
        // The end of the run a timed-out Stop left behind; no turn is its own.
        if is_execution_end(kind) {
            if let Some(state) = session_id.as_deref().and_then(|sid| self.sessions.get_mut(sid)).filter(|state| state.unsettled) {
                state.unsettled = false;
                return Ok(());
            }
        }
        match kind {
            "permission.asked" => return self.on_asked(Asked::Permission(event["data"].clone())).await,
            "form.created" => return self.on_asked(Asked::Form(event["data"]["form"].clone())).await,
            // Answered in another OpenCode client, or dropped by OpenCode: a reject it sends on its own (a Stop, or
            // another reject in the same session) cancels the request. T3's own answers are settled where they are sent.
            "permission.replied" | "form.replied" | "form.cancelled" => {
                let native = if kind == "permission.replied" { str_of(&event["data"]["requestID"]) } else { str_of(&event["data"]["id"]) };
                let Some((request_id, entry)) = self.pending.iter().find(|(_, entry)| entry.native.id() == native).map(|(id, entry)| (id.clone(), entry.answering)) else { return Ok(()) };
                if entry {
                    return Ok(());
                }
                let answered = kind == "form.replied" || (kind == "permission.replied" && event["data"]["reply"] != "reject");
                return self.settle_request(&request_id, if answered { "resolved" } else { "cancelled" });
            }
            _ => {}
        }
        let Some(sid) = session_id else { return Ok(()) };
        if !self.sessions.contains_key(&sid) {
            return Ok(());
        }
        if matches!(kind, "session.model.selected" | "session.step.started") {
            if kind == "session.model.selected" {
                self.sessions.get_mut(&sid).unwrap().model = Some(event["data"]["model"].clone());
            }
            self.report_model(&sid, &event["data"]["model"])?;
        }
        if kind == "session.inbox.enqueued" {
            let inbox = str_of(&event["data"]["inboxID"]);
            let item = &event["data"]["item"];
            let idle_child = self.sessions[&sid].child.is_some() && self.sessions[&sid].turn.is_none();
            if idle_child {
                if let Some(info) = self.sessions.get_mut(&sid).and_then(|state| state.child.as_mut()) {
                    info.queued.get_or_insert(inbox.clone());
                }
            }
            if item["type"] == "synthetic" {
                return self.on_report(&sid, &inbox, &item["payload"]).await;
            }
            // A subagent's later prompts (a resumed subagent) open its next turn's message.
            if idle_child && item["type"] == "user" {
                if let Some(info) = self.sessions.get_mut(&sid).and_then(|state| state.child.as_mut()).filter(|info| info.prompt.is_none()) {
                    info.prompt = Some(str_of(&item["payload"]["text"]));
                    return Ok(());
                }
            }
        }
        if matches!(kind, "session.inbox.cancelled" | "session.inbox.delivered") {
            let inbox = str_of(&event["data"]["inboxID"]);
            self.sessions.get_mut(&sid).unwrap().reports.shift_remove(&inbox);
            let settled = self.sessions.get_mut(&sid).and_then(|state| state.turn.as_mut()).is_some_and(|turn| {
                let removed = turn.steers.remove(&inbox);
                if removed {
                    turn.settled_inbox.insert(inbox.clone());
                }
                removed
            });
            // A cancelled steer may leave a held turn nothing to wait for.
            if settled && kind == "session.inbox.cancelled" {
                self.end_if_settled(&sid).await?;
            }
            return Ok(());
        }
        let started = kind == "session.execution.started";
        if self.sessions[&sid].turn.is_none() {
            // Each execution of a subagent's session is a turn on its child thread; one OpenCode starts on a thread's own
            // session is the parent's answer to a background subagent's report.
            if started && self.sessions[&sid].child.is_some() {
                return self.start_child_turn(&sid).await;
            }
            if started {
                return self.on_wake(&sid).await;
            }
            return Ok(());
        }
        let turn = self.sessions.get_mut(&sid).unwrap().turn.as_mut().unwrap();
        // A session runs one execution at a time, and each opens with `started` on this ordered stream, so what comes
        // before it is the stopped run's.
        if turn.awaiting_start {
            if started {
                turn.awaiting_start = false;
            }
            return Ok(());
        }
        self.on_turn_event(&sid, event, kind).await
    }
    /// The provider thread names the model OpenCode reports running.
    fn report_model(&mut self, session_id: &str, model: &Value) -> Result<(), StoreError> {
        if !model.is_object() {
            return Ok(());
        }
        let thread_view = view(&self.store, &self.thread_id)?;
        let provider_thread_id = self.sessions[session_id].provider_thread["id"].clone();
        let Ok(thread) = find(&thread_view, "providerThreads", &provider_thread_id) else { return Ok(()) };
        let selection = reported_selection(&thread["providerInstanceId"], model);
        if thread["nativeMetadata"]["modelSelection"] == selection {
            return Ok(());
        }
        let mut thread = thread.clone();
        thread["nativeMetadata"] = json!({"modelSelection":selection});
        thread["updatedAt"] = json!(at(Utc::now()));
        self.commit_events(vec![("provider-thread.updated", thread)])
    }
    /// One event for a session's running turn: its text, tools, steps and end.
    async fn on_turn_event(&mut self, sid: &str, event: &Value, kind: &str) -> Result<(), StoreError> {
        let data = &event["data"];
        let thread = self.thread_of(sid);
        match kind {
            "session.text.started" | "session.reasoning.started" | "session.text.delta" | "session.reasoning.delta" | "session.text.ended" | "session.reasoning.ended" => {
                let block_kind = if kind.starts_with("session.text.") { Kind::Text } else { Kind::Reasoning };
                let change = if kind.ends_with(".started") { Change::Started } else if kind.ends_with(".delta") { Change::Delta(data["delta"].as_str().unwrap_or("")) } else if data["text"].is_string() { Change::Ended(data["text"].as_str().unwrap_or("")) } else { Change::Started };
                let now_ms = self.clock.now_ms();
                let turn = self.sessions.get_mut(sid).unwrap().turn.as_mut().unwrap();
                if let Some((native, text, completed)) = turn.projector.apply_text(data, block_kind, change, now_ms) {
                    let thread_view = view(&self.store, &thread)?;
                    let events = self.turn_mut(sid).projector.write_text(&thread_view, &native, &text, completed)?;
                    commit(&self.store, &thread, |_| Ok(events))?;
                }
            }
            "session.tool.input.started" => {
                // Its form is the item the user answers; the tool call would repeat it.
                if data["name"] == "question" {
                    return Ok(());
                }
                if data["name"] == "subagent" {
                    return self.start_call(sid, &str_of(&data["id"])).await;
                }
                let thread_view = view(&self.store, &thread)?;
                let events = self.turn_mut(sid).projector.tool_started(&thread_view, &str_of(&data["id"]), &str_of(&data["name"]))?;
                commit(&self.store, &thread, |_| Ok(events))?;
            }
            "session.tool.called" => {
                if self.sessions[sid].calls.contains_key(&str_of(&data["id"])) {
                    return self.update_call(sid, &str_of(&data["id"]), &data["input"]);
                }
                let thread_view = view(&self.store, &thread)?;
                let events = self.turn_mut(sid).projector.tool_called(&thread_view, &str_of(&data["id"]), &data["input"])?;
                commit(&self.store, &thread, |_| Ok(events))?;
            }
            "session.tool.progress" => {
                let id = str_of(&data["id"]);
                if let (true, Some(child)) = (self.sessions[sid].calls.contains_key(&id), string_field(&data["metadata"], "sessionID")) {
                    self.attach_child(sid, &id, &child).await?;
                }
            }
            "session.tool.success" if self.sessions[sid].calls.contains_key(&str_of(&data["id"])) => {
                let id = str_of(&data["id"]);
                if let Some(child) = string_field(&data["metadata"], "sessionID") {
                    self.attach_child(sid, &id, &child).await?;
                }
                // A background call returns at launch; its report settles it.
                if data["metadata"]["status"] != "running" {
                    self.settle_call(sid.to_owned(), id, "completed", Some(subagent_output(&text_of(&data["content"])))).await?;
                }
            }
            "session.tool.failed" if self.sessions[sid].calls.contains_key(&str_of(&data["id"])) => {
                // A refused call (unknown agent, the nesting limit) is a failed subagent.
                let aborted = data["error"]["type"] == "aborted";
                self.settle_call(sid.to_owned(), str_of(&data["id"]), if aborted { "interrupted" } else { "failed" }, Some(str_of(&data["error"]["message"]))).await?;
            }
            "session.tool.success" => {
                let thread_view = view(&self.store, &thread)?;
                let output = text_of(&data["content"]);
                let events = self.turn_mut(sid).projector.tool_finished(&thread_view, &str_of(&data["id"]), "completed", Some(&output), Some(&data["metadata"]))?;
                commit(&self.store, &thread, |_| Ok(events))?;
            }
            "session.tool.failed" => {
                let aborted = data["error"]["type"] == "aborted";
                let thread_view = view(&self.store, &thread)?;
                let message = str_of(&data["error"]["message"]);
                let events = self.turn_mut(sid).projector.tool_finished(&thread_view, &str_of(&data["id"]), if aborted { "interrupted" } else { "failed" }, Some(&message), Some(&data["metadata"]))?;
                commit(&self.store, &thread, |_| Ok(events))?;
            }
            // `/compact` and OpenCode's own compaction when the context fills both run inside a turn's execution.
            "session.compaction.started" => {
                let thread_view = view(&self.store, &thread)?;
                let native = self.turn_mut(sid).native_turn.clone();
                let events = self.turn_mut(sid).projector.compaction_started(&thread_view, &native)?;
                commit(&self.store, &thread, |_| Ok(events))?;
            }
            "session.compaction.ended" | "session.compaction.failed" => {
                let thread_view = view(&self.store, &thread)?;
                let events = if kind == "session.compaction.ended" { self.turn_mut(sid).projector.compaction_ended(&thread_view, "completed", data["text"].as_str())? } else { self.turn_mut(sid).projector.compaction_ended(&thread_view, "failed", None)? };
                commit(&self.store, &thread, |_| Ok(events))?;
            }
            "session.step.ended" | "session.step.failed" => {
                if data["tokens"].is_object() {
                    self.turn_mut(sid).projector.step(&data["tokens"]);
                }
            }
            "session.execution.started" => self.turn_mut(sid).held_end = None,
            "session.execution.succeeded" => {
                let interrupted = self.turn_mut(sid).interrupted;
                self.end_execution(sid, if interrupted { Terminal::Interrupted } else { Terminal::Completed }).await?;
            }
            "session.execution.interrupted" => self.end_execution(sid, Terminal::Interrupted).await?,
            "session.execution.failed" => {
                let message = data["error"]["message"].as_str().filter(|message| !message.is_empty()).unwrap_or("OpenCode ended the turn with an error this version cannot read.");
                let mut failed = failure(message, "provider_error");
                if let Some(code) = data["error"]["type"].as_str() {
                    failed["code"] = json!(code);
                }
                self.end_execution(sid, Terminal::Failed(failed)).await?;
            }
            _ => {}
        }
        Ok(())
    }
    fn turn_mut(&mut self, sid: &str) -> &mut Turn {
        self.sessions.get_mut(sid).unwrap().turn.as_mut().unwrap()
    }
    /// Ends the turn with its execution, unless a steer sent into it is still undelivered: OpenCode then starts
    /// another execution for it, which the turn owns too. A Stop leaves steers undelivered and starts nothing.
    async fn end_execution(&mut self, sid: &str, end: Terminal) -> Result<(), StoreError> {
        let turn = self.turn_mut(sid);
        if matches!(end, Terminal::Completed) && !turn.steers.is_empty() {
            turn.held_end = Some(end);
            return Ok(());
        }
        self.finish_turn(sid, end, false).await
    }

    // ---- requests ----

    async fn on_asked(&mut self, asked: Asked) -> Result<(), StoreError> {
        let asking = str_of(&match &asked {
            Asked::Permission(request) => request["sessionID"].clone(),
            Asked::Form(form) => form["sessionID"].clone(),
        });
        let Some(state) = self.sessions.get(&asking) else { return Ok(()) };
        let shown = state.turn.as_ref().is_some_and(|turn| !turn.awaiting_start);
        if shown {
            return match asked {
                Asked::Permission(request) => self.on_permission_asked(&asking, &request).await,
                Asked::Form(form) => self.on_form_created(&asking, &form).await,
            };
        }
        if state.unsettled || state.turn.as_ref().is_some_and(|turn| turn.awaiting_start) {
            let (kind, id) = match &asked {
                Asked::Permission(request) => ("permission", str_of(&request["id"])),
                Asked::Form(form) => ("form", str_of(&form["id"])),
            };
            self.stop_stale_request(&asking, kind, &id).await;
        }
        Ok(())
    }
    async fn on_permission_asked(&mut self, sid: &str, data: &Value) -> Result<(), StoreError> {
        // The asking session's own turn knows the tool.
        let tool_name = data["source"]["id"].as_str().and_then(|id| self.sessions.get(sid)?.turn.as_ref()?.projector.tool_input(id).map(|(name, _)| name.to_owned()));
        let save: Vec<String> = data["save"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        let resources: Vec<String> = data["resources"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        let action = str_of(&data["action"]);
        let mut options = vec![json!({"decision":"cancel","label":"Cancel"}), json!({"decision":"decline","label":"Decline"})];
        // "Always" in OpenCode saves a grant for the whole project, so the session-wide choice is T3's own rule on this session instead.
        if !save.is_empty() {
            options.push(json!({"decision":"acceptForSession","label":session_grant_label(&action, &save)}));
        }
        options.push(json!({"decision":"accept","label":"Approve"}));
        let body = RequestBody::Approval { title: action.clone(), kind: projection::permission_request_kind(&action, tool_name.as_deref()), prompt: if resources.is_empty() { action.clone() } else { resources.join("\n") }, options: json!(options) };
        self.show_request(sid, Native::Permission { id: str_of(&data["id"]), action, resources, save }, body).await
    }
    async fn on_form_created(&mut self, sid: &str, form: &Value) -> Result<(), StoreError> {
        match form_questions(form) {
            Ok(questions) => self.show_request(sid, Native::Form { id: str_of(&form["id"]), form: form.clone() }, RequestBody::Questions(questions)).await,
            Err(unsupported) => {
                // Cancelling ends OpenCode's execution as a user stop, so the turn is failed here with the reason and
                // that stop's end is skipped.
                tracing::warn!(reason = %unsupported, "Declined an OpenCode form T3 Code cannot show.");
                let cancelled = self.deliver(Method::DELETE, sid, &format!("/form/{}", segment(&str_of(&form["id"]))), None).await;
                if !cancelled {
                    return self.abandon_request(sid, "form cancel failed").await;
                }
                self.sessions.get_mut(sid).unwrap().unsettled = true;
                self.finish_turn(sid, Terminal::Failed(failure(&format!("OpenCode asked for {unsupported}, which T3 Code can't show. The question was declined."), "provider_error")), false).await
            }
        }
    }
    /// Shows a permission ask or question form under the running turn of the session that asked.
    async fn show_request(&mut self, sid: &str, native: Native, body: RequestBody) -> Result<(), StoreError> {
        if self.sessions.get(sid).is_none_or(|state| state.turn.is_none()) || self.pending.values().any(|entry| entry.native.id() == native.id()) {
            return Ok(());
        }
        let thread_view = view(&self.store, &self.thread_id)?;
        let session_row = self.session_id.clone();
        let turn = self.turn_mut(sid);
        let root = turn.projector.context().root_node_id.clone();
        let (request_id, events) = turn.projector.request_artifacts(&thread_view, native.id(), &body, &session_row, &root)?;
        self.commit_events(events)?;
        self.pending.insert(request_id, PendingRequest { session: sid.to_owned(), answering: false, native });
        self.set_session_status("waiting", None)
    }
    /// Settles a request OpenCode answered or dropped on its own.
    fn settle_request(&mut self, request_id: &str, status: &str) -> Result<(), StoreError> {
        if !self.forget_request(request_id)? {
            return Ok(());
        }
        let thread_view = view(&self.store, &self.thread_id)?;
        let events = projection::settle_request_events(&thread_view, request_id, status)?;
        self.commit_events(events)
    }

    // ---- ending a turn ----

    /// Ends a session's turn: open work settles, the provider thread goes idle, and the run reaches its terminal state.
    async fn finish_turn(&mut self, sid: &str, terminal: Terminal, broken: bool) -> Result<(), StoreError> {
        if self.sessions.get(sid).is_some_and(|state| state.child.is_some()) {
            return self.finish_child_turn(sid, terminal).await;
        }
        let Some(mut turn) = self.sessions.get_mut(sid).and_then(|state| state.turn.take()) else { return Ok(()) };
        if let Some(state) = self.sessions.get_mut(sid) {
            state.stranded.extend(turn.steers.drain());
        }
        // OpenCode drops a request when the asking session's execution ends.
        let ended: Vec<String> = self.pending.iter().filter(|(_, entry)| entry.session == sid).map(|(id, _)| id.clone()).collect();
        for id in ended {
            self.pending.remove(&id);
            let thread_view = view(&self.store, &self.thread_id)?;
            let events = projection::settle_request_events(&thread_view, &id, "cancelled")?;
            self.commit_events(events)?;
        }
        let directory = self.sessions[sid].directory.clone();
        let window = self.window_of(&directory, turn.projector.selection()["model"].as_str().unwrap_or(""));
        turn.projector.set_context_window(window);
        let thread_view = view(&self.store, &self.thread_id)?;
        let events = turn.projector.finish(&thread_view, terminal.status(), matches!(terminal, Terminal::Failed(_)))?;
        let mut turn_row_native = turn.native_turn.clone();
        if turn.continuation {
            turn_row_native = turn.native_turn.clone();
        }
        let _ = turn_row_native;
        self.commit_events(events)?;
        let ctx = turn.projector.context().clone();
        // A foreground subagent ends with the turn that waits on it. A background one outlives a finished or interrupted
        // turn (a user Stop has already stopped it), and a failed turn stops it.
        let calls: Vec<(String, bool)> = self.sessions[sid].calls.iter().filter(|(_, call)| call.ctx.run_id == ctx.run_id).map(|(id, call)| (id.clone(), call.background)).collect();
        for (id, background) in calls {
            if background && !matches!(terminal, Terminal::Failed(_)) {
                continue;
            }
            self.settle_call(sid.to_owned(), id, terminal.status_static(), None).await?;
        }
        // A run that ends interrupted without a Stop (the server stopped under it) still tells why, as the original does.
        if matches!(terminal, Terminal::Interrupted) {
            let thread_view = view(&self.store, &self.thread_id)?;
            let signal = crate::execution::run_signal_item_id(&ctx.run_id, "interrupt-result");
            let requested = thread_view["turnItems"].as_array().into_iter().flatten().any(|item| item["runId"] == ctx.run_id.as_str() && item["type"] == "run_interrupt_request");
            if !requested {
                if let Ok(run) = find(&thread_view, "runs", &json!(ctx.run_id)) {
                    let now = at(Utc::now());
                    let ordinal = run["ordinal"].as_u64().unwrap_or(0) * 100 + 98;
                    let item = json!({"id":signal,"threadId":self.thread_id,"runId":ctx.run_id,"nodeId":ctx.root_node_id,"providerThreadId":ctx.provider_thread_id,"providerTurnId":ctx.provider_turn_id,"nativeItemRef":null,"parentItemId":null,"ordinal":ordinal,"status":"interrupted","title":"Interrupted","startedAt":now,"completedAt":now,"updatedAt":now,"type":"run_interrupt_result","message":"Run interrupted by user"});
                    self.commit_events(vec![("turn-item.updated", item)])?;
                }
            }
        }
        // The thread is idle between turns; a broken one is reopened by the next turn.
        let thread_view = view(&self.store, &self.thread_id)?;
        if let Ok(thread) = find(&thread_view, "providerThreads", &ctx.provider_thread_id) {
            let mut thread = thread.clone();
            thread["status"] = json!(if broken { "error" } else { "idle" });
            thread["updatedAt"] = json!(at(Utc::now()));
            self.commit_events(vec![("provider-thread.updated", thread)])?;
        }
        let any_active = self.sessions.values().any(|state| state.turn.is_some());
        let status = if !self.pending.is_empty() { "waiting" } else if any_active { "running" } else { "ready" };
        self.set_session_status(status, None)?;
        match &terminal {
            Terminal::Failed(failed) => fail_run_with(&self.store, &self.thread_id, Some(&ctx.run_id), Some(&ctx), failed, Retain::Nothing)?,
            other => {
                // A subagent a completed turn left running goes on, and reports when it ends.
                let retain = if self.has_background(sid) { Retain::CommandsOnCompletion } else { Retain::Nothing };
                terminal_with(&self.store, &self.thread_id, Some(&ctx.run_id), other.status(), None, Some(&ctx), retain)?;
                settle_queue(&self.store, &self.thread_id, None)?;
            }
        }
        // A spawned server lists its models lazily, so a window still unknown is read again for the next turn.
        if window.is_none() {
            self.read_models(&directory).await;
        }
        Ok(())
    }

    // ---- losing and regaining the stream ----

    /// The stream is the only terminal signal, and it is volatile: events sent while it is down are gone, and a restarted
    /// server never ends the execution it lost. So a lost stream reconnects, then reconciles each running turn from the
    /// server's own state. Only when reconnecting keeps failing are the turns failed and the session broken.
    async fn reconnect_after_loss(&mut self) -> Result<(), StoreError> {
        self.stream_failure = Some("The OpenCode event stream was lost. Reconnecting.".into());
        tracing::warn!("Lost the OpenCode event stream; reconnecting.");
        self.live = None;
        for attempt in 0..RECONNECT_ATTEMPTS {
            if attempt > 0 {
                self.clock.sleep_until(self.clock.now_ms() + RECONNECT_DELAY_MS).await;
            }
            match self.reconnect().await {
                Ok(()) => {
                    self.stream_failure = None;
                    return Ok(());
                }
                Err(cause) => tracing::warn!(%cause, attempt, "Could not reconnect to the OpenCode event stream."),
            }
        }
        self.fail_all("The OpenCode event stream was lost and could not reconnect.").await
    }
    async fn reconnect(&mut self) -> Result<(), String> {
        let connection = self.instance.connect(&self.cwd, self.clock.clone()).await.map_err(|e| e.to_string())?;
        self.open_stream(connection).await.map_err(|e| e.to_string())?;
        // A restarted server forgot T3's MCP servers; the next turn adds them again.
        for state in self.sessions.values_mut() {
            state.mcp = None;
        }
        tokio::time::timeout(RECONCILE_TIMEOUT, self.reconcile()).await.map_err(|_| "reconciling timed out".to_owned())?.map_err(|e| e.to_string())
    }
    async fn fail_all(&mut self, message: &str) -> Result<(), StoreError> {
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        for id in ids {
            self.finish_turn(&id, Terminal::Failed(failure(message, "transport_error")), true).await?;
        }
        self.stream_failure = Some(message.to_owned());
        self.set_session_status("error", Some(message))
    }
    /// Emits what a running turn missed while the stream was down, from the session's history since the turn began:
    /// text, reasoning and tools, each under the same native id its live events would have used, so nothing already
    /// shown is duplicated. Returns how that history says the turn's execution ended.
    async fn backfill(&mut self, sid: &str) -> Result<Option<String>, StoreError> {
        let Some(turn) = self.sessions.get(sid).and_then(|state| state.turn.as_ref()) else { return Ok(None) };
        let prompt = prompt_of(&json!({"nativeTurnRef":{"nativeId":turn.native_turn}}));
        let start: Option<Option<String>> = match (&turn.before, &prompt) {
            (Some(before), _) => Some(before.clone()),
            (None, Some(prompt)) => Some(Some(prompt.clone())),
            (None, None) => None,
        };
        let Some(start) = start else { return Ok(None) };
        let query = vec![("order".to_owned(), "desc".to_owned()), ("limit".to_owned(), "50".to_owned())];
        let mut read: Vec<Value> = vec![];
        let mut next_query = query;
        loop {
            let page = match self.session_call(Method::GET, sid, "/message", &next_query, None, None).await {
                Ok(page) => page,
                Err(cause) => {
                    tracing::warn!(%cause, "Could not read an OpenCode session's history.");
                    return Ok(None);
                }
            };
            let data = page["data"].as_array().cloned().unwrap_or_default();
            let found = start.as_ref().is_some_and(|start| data.iter().any(|message| message["id"] == start.as_str()));
            let empty = data.is_empty();
            read.extend(data);
            let next = page["cursor"]["next"].as_str().map(str::to_owned);
            if found || empty || next.is_none() {
                break;
            }
            next_query = vec![("cursor".to_owned(), next.unwrap())];
        }
        if let Some(start) = &start {
            if let Some(at) = read.iter().position(|message| message["id"] == start.as_str()) {
                read.truncate(at + 1);
            }
        }
        // Without its start the history holds only earlier turns (the stream dropped before the prompt landed): none of it is this turn's.
        let found = match &start {
            None => true,
            Some(start) => read.last().is_some_and(|message| message["id"] == start.as_str()),
        };
        if !found {
            return Ok(None);
        }
        let recent: Vec<Value> = if matches!(self.sessions[sid].turn.as_ref().and_then(|turn| turn.before.clone()), Some(Some(_))) { read[..read.len().saturating_sub(1)].to_vec() } else { read };
        let idle = recent.iter().find(|message| message["type"] == "idle").map(|message| str_of(&message["outcome"]));
        for message in recent.iter().rev() {
            if message["type"] != "assistant" || self.sessions.get(sid).is_none_or(|state| state.turn.is_none()) {
                continue;
            }
            let (mut texts, mut reasonings) = (0u64, 0u64);
            for part in message["content"].as_array().into_iter().flatten() {
                let part_kind = part["type"].as_str().unwrap_or("");
                if part_kind == "text" || part_kind == "reasoning" {
                    let ordinal = if part_kind == "text" { &mut texts } else { &mut reasonings };
                    let data = json!({"assistantMessageID":message["id"],"ordinal":*ordinal});
                    *ordinal += 1;
                    let thread_view = view(&self.store, &self.thread_id)?;
                    let block_kind = if part_kind == "text" { Kind::Text } else { Kind::Reasoning };
                    let text = str_of(&part["text"]);
                    let now_ms = self.clock.now_ms();
                    let turn = self.turn_mut(sid);
                    if let Some((native, text, completed)) = turn.projector.apply_text(&data, block_kind, Change::Ended(&text), now_ms) {
                        let events = turn.projector.write_text(&thread_view, &native, &text, completed)?;
                        self.commit_events(events)?;
                    }
                    continue;
                }
                if part_kind != "tool" {
                    continue;
                }
                let id = str_of(&part["id"]);
                let thread_view = view(&self.store, &self.thread_id)?;
                let state = &part["state"];
                let turn = self.turn_mut(sid);
                turn.projector.tool_adopt(&id, &str_of(&part["name"]), Some(&state["input"]));
                let events = match state["status"].as_str() {
                    Some("completed") => turn.projector.tool_finished(&thread_view, &id, "completed", Some(&text_of(&state["content"])), Some(&state["metadata"]))?,
                    Some("error") => turn.projector.tool_finished(&thread_view, &id, "failed", Some(&str_of(&state["error"]["message"])), Some(&state["metadata"]))?,
                    _ => turn.projector.tool_called(&thread_view, &id, &state["input"])?,
                };
                self.commit_events(events)?;
            }
        }
        Ok(idle)
    }
    /// Settles each thread turn that was running when the stream dropped, from the server: a session still running keeps
    /// its turn (its next events arrive on the new stream); one that stopped ends the turn with the outcome of the `idle`
    /// item after the turn's start. A restarted server writes no such item for the execution it lost, so no `idle`
    /// means the turn was interrupted. Permission asks and question forms are read back from every session still
    /// running, so a run that asked during the gap can be answered.
    async fn reconcile(&mut self) -> Result<(), StoreError> {
        let running: Vec<String> = self.sessions.iter().filter(|(_, state)| state.turn.is_some()).map(|(id, _)| id.clone()).collect();
        if running.is_empty() && self.busy.is_empty() {
            return Ok(());
        }
        let active = self.call(Method::GET, "/api/session/active", &[], None, None).await.map_err(|e| error(e.to_string()))?;
        let active = active["data"].as_object().cloned().unwrap_or_default();
        // An execution that ended while the stream was down never said so.
        self.busy.retain(|sid| active.contains_key(sid));
        for sid in running {
            let Some(turn) = self.sessions.get(&sid).and_then(|state| state.turn.as_ref()) else { continue };
            let was_interrupted = turn.interrupted;
            let ended = self.backfill(&sid).await?;
            if active.contains_key(&sid) || self.sessions.get(&sid).is_none_or(|state| state.turn.is_none()) {
                continue;
            }
            let terminal = match ended.as_deref() {
                Some("succeeded") => if was_interrupted { Terminal::Interrupted } else { Terminal::Completed },
                Some("failed") => Terminal::Failed(failure("OpenCode ended the turn with an error while T3 Code was reconnecting.", "provider_error")),
                _ => Terminal::Interrupted,
            };
            self.finish_turn(&sid, terminal, false).await?;
        }
        // A request asked while the stream was down was never shown, and the run waits on it; one T3 shows that
        // OpenCode no longer lists was answered elsewhere or dropped with its execution.
        let mut listed: HashMap<String, HashSet<String>> = HashMap::new();
        for sid in active.keys() {
            if !self.sessions.contains_key(sid) {
                continue;
            }
            let (Ok(permissions), Ok(forms)) = (self.session_call(Method::GET, sid, "/permission", &[], None, None).await, self.session_call(Method::GET, sid, "/form", &[], None, None).await) else { continue };
            let permissions = permissions["data"].as_array().cloned().unwrap_or_default();
            let forms = forms["data"].as_array().cloned().unwrap_or_default();
            listed.insert(sid.clone(), permissions.iter().chain(forms.iter()).map(|request| str_of(&request["id"])).collect());
            let shown: HashSet<String> = self.pending.values().map(|entry| entry.native.id().to_owned()).collect();
            for request in permissions.iter().filter(|request| !shown.contains(request["id"].as_str().unwrap_or(""))) {
                self.on_asked(Asked::Permission(request.clone())).await?;
            }
            for form in forms.iter().filter(|form| !shown.contains(form["id"].as_str().unwrap_or(""))) {
                self.on_asked(Asked::Form(form.clone())).await?;
            }
        }
        let stale: Vec<String> = self.pending.iter().filter(|(_, entry)| !listed.get(&entry.session).is_some_and(|ids| ids.contains(entry.native.id()))).map(|(id, _)| id.clone()).collect();
        for id in stale {
            self.settle_request(&id, "cancelled")?;
        }
        Ok(())
    }
}


// ---- subagents and background work ----

/// A subagent's answer, without the `<subagent …>` wrapper OpenCode gives the model.
fn subagent_output(text: &str) -> String {
    static WRAPPER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let wrapper = WRAPPER.get_or_init(|| regex::Regex::new(r"(?s)^<subagent\b[^>]*>\n?(.*?)\n?</subagent>$").unwrap());
    wrapper.captures(text.trim()).map_or_else(|| text.to_owned(), |captures| captures[1].to_owned())
}
fn string_field(record: &Value, key: &str) -> Option<String> {
    record.get(key).and_then(Value::as_str).filter(|text| !text.trim().is_empty()).map(str::to_owned)
}
/// How a background subagent ended, from the `state` of OpenCode's report to its parent.
fn report_outcome(state: Option<&str>) -> &'static str {
    match state {
        Some("completed") => "completed",
        Some("error") => "failed",
        Some("cancelled") => "cancelled",
        _ => "unknown",
    }
}
fn child_thread_id(child: &str) -> String {
    format!("thread:provider:{DRIVER}:native-thread:{}", projection::enc(child))
}
fn child_provider_thread_id(child: &str) -> String {
    format!("provider-thread:provider:{DRIVER}:native-thread:{}", projection::enc(child))
}

impl Actor {
    /// The thread a session's rows belong to: the actor's own, or a subagent's child thread.
    fn thread_of(&self, sid: &str) -> String {
        self.sessions.get(sid).map_or_else(|| self.thread_id.clone(), |state| state.app_thread_id.clone())
    }
    /// The thread's own session that a session runs under, through any nesting.
    fn root_of(&self, sid: &str) -> String {
        let mut current = sid.to_owned();
        while let Some(parent) = self.sessions.get(&current).and_then(|state| state.child.as_ref()).map(|child| child.parent_session.clone()) {
            current = parent;
        }
        current
    }
    /// A thread's session and its subagents' sessions, through any nesting.
    fn sessions_of(&self, root: &str) -> Vec<String> {
        self.sessions.keys().filter(|sid| self.root_of(sid) == root).cloned().collect()
    }
    /// What a session still owes its thread once the thread's turn ended: held executions, reports OpenCode queued for the
    /// follow-up it will start, background subagents, and anything a subagent's session still runs.
    fn owes_work(&self, sid: &str) -> bool {
        let Some(state) = self.sessions.get(sid) else { return false };
        !state.wakes.is_empty() || !state.reports.is_empty() || (state.child.is_some() && self.busy.contains(sid)) || state.calls.values().any(|call| call.background)
    }
    /// Work that outlives the thread's turn, on its own session or its subagents'.
    fn has_background(&self, root: &str) -> bool {
        self.sessions_of(root).iter().any(|sid| self.owes_work(sid))
    }

    /// A `subagent` call's item, made when the model starts it.
    async fn start_call(&mut self, sid: &str, tool_id: &str) -> Result<(), StoreError> {
        let thread = self.thread_of(sid);
        let thread_view = view(&self.store, &thread)?;
        let scope = self.sessions[sid].turn.as_ref().map(|turn| turn.projector.scope().to_owned()).unwrap_or_default();
        let native = format!("{scope}{tool_id}");
        let turn = self.turn_mut(sid);
        turn.projector.used_subagents = true;
        let (ctx, selection) = (turn.projector.context().clone(), turn.projector.selection().clone());
        let ordinal = turn.projector.ordinal(&thread_view, &native);
        let call = Call { tool_id: tool_id.to_owned(), native_id: native, session: sid.to_owned(), ctx, selection, ordinal, started_at: at(Utc::now()), prompt: String::new(), title: None, agent: None, model: None, background: false, child: None, status: "running".into(), result: None, completed_at: None };
        self.sessions.get_mut(sid).unwrap().calls.insert(tool_id.to_owned(), call);
        self.emit_subagent(sid, tool_id)
    }
    /// The call's input is known: what it asked the subagent, and whether it waits.
    fn update_call(&mut self, sid: &str, tool_id: &str, input: &Value) -> Result<(), StoreError> {
        let Some(call) = self.sessions.get_mut(sid).and_then(|state| state.calls.get_mut(tool_id)) else { return Ok(()) };
        call.prompt = string_field(input, "prompt").unwrap_or_else(|| call.prompt.clone());
        call.title = string_field(input, "description").or_else(|| call.title.take());
        call.agent = string_field(input, "agent");
        call.model = string_field(input, "model").or_else(|| call.model.take());
        call.background = input["background"] == true;
        self.emit_subagent(sid, tool_id)
    }
    /// A subagent call as its parent turn's item, node and subagent row.
    fn emit_subagent(&self, sid: &str, tool_id: &str) -> Result<(), StoreError> {
        let Some(call) = self.sessions.get(sid).and_then(|state| state.calls.get(tool_id)) else { return Ok(()) };
        self.write_subagent(sid, call)
    }
    fn write_subagent(&self, sid: &str, call: &Call) -> Result<(), StoreError> {
        let now = at(Utc::now());
        let active = call.status == "running";
        let completed = if active { Value::Null } else { json!(call.completed_at.clone().unwrap_or_else(|| now.clone())) };
        let node = projection::node_id(&call.native_id);
        let thread = self.thread_of(sid);
        let (child_thread, child_provider_thread) = match &call.child {
            Some(child) => (json!(child_thread_id(child)), json!(child_provider_thread_id(child))),
            None => (Value::Null, Value::Null),
        };
        let caller_provider_thread = call.ctx.provider_thread_id.clone();
        let run = if call.ctx.run_id.is_empty() { Value::Null } else { json!(call.ctx.run_id) };
        let node_row = json!({"id":node,"threadId":thread,"runId":run,"parentNodeId":call.ctx.root_node_id,"rootNodeId":call.ctx.root_node_id,"kind":"subagent","status":call.status,"countsForRun":false,"providerThreadId":if child_provider_thread.is_null() { caller_provider_thread.clone() } else { child_provider_thread.clone() },"providerTurnId":call.ctx.provider_turn_id,"nativeItemRef":projection::native_ref(&call.native_id, "strong"),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":call.started_at,"completedAt":completed});
        let row = json!({"id":node,"threadId":thread,"runId":run,"parentNodeId":call.ctx.root_node_id,"origin":"provider_native","createdBy":"agent","driver":DRIVER,"providerInstanceId":call.ctx.provider_instance_id,"providerThreadId":child_provider_thread,"childThreadId":child_thread,"nativeTaskRef":projection::native_ref(&call.native_id, "strong"),"prompt":call.prompt,"title":call.title,"model":call.model,"status":call.status,"result":call.result,"startedAt":call.started_at,"completedAt":completed,"updatedAt":now});
        let item = json!({"id":projection::item_id(&call.native_id),"threadId":thread,"runId":run,"nodeId":node,"providerThreadId":caller_provider_thread,"providerTurnId":call.ctx.provider_turn_id,"nativeItemRef":projection::native_ref(&call.native_id, "strong"),"parentItemId":null,"ordinal":call.ordinal,"status":call.status,"title":call.title,"startedAt":call.started_at,"completedAt":completed,"updatedAt":now,"type":"subagent","subagentId":node,"origin":"provider_native","driver":DRIVER,"providerInstanceId":call.ctx.provider_instance_id,"childThreadId":child_thread,"prompt":call.prompt,"result":call.result});
        commit(&self.store, &thread, |_| Ok(vec![("node.updated", node_row.clone()), ("subagent.updated", row.clone()), ("turn-item.updated", item.clone())]))
    }

    /// Gives a subagent call its session once both are known: OpenCode names the session on the call's progress, and
    /// announces it just before. The session becomes a child thread under the call's item.
    async fn attach_child(&mut self, sid: &str, tool_id: &str, child_sid: &str) -> Result<(), StoreError> {
        if self.sessions.get(sid).and_then(|state| state.calls.get(tool_id)).is_none_or(|call| call.child.is_some()) {
            return Ok(());
        }
        let info = self.announced.remove(child_sid);
        let now = at(Utc::now());
        let parent_thread = self.thread_of(sid);
        let parent_view = view(&self.store, &parent_thread)?;
        let (model, title, prompt, call_native, selection, ctx, ordinal) = {
            let call = self.sessions.get_mut(sid).unwrap().calls.get_mut(tool_id).unwrap();
            if let Some(model) = info.as_ref().filter(|info| info["model"].is_object()) {
                call.model = Some(format!("{}/{}", str_of(&model["model"]["providerID"]), str_of(&model["model"]["id"])));
            }
            call.title = call.title.take().or_else(|| string_field(info.as_ref().unwrap_or(&Value::Null), "title"));
            (call.model.clone(), call.title.clone(), call.prompt.clone(), call.native_id.clone(), call.selection.clone(), call.ctx.clone(), call.ordinal)
        };
        let child_thread = child_thread_id(child_sid);
        let provider_thread_id = child_provider_thread_id(child_sid);
        let node = projection::node_id(&call_native);
        let mut child_selection = selection.clone();
        child_selection["model"] = json!(model.clone().unwrap_or_else(|| str_of(&selection["model"])));
        let parent_title = parent_view["thread"]["title"].as_str().unwrap_or_default().to_owned();
        let thread_title = crate::subagents::thread_title(&parent_title, &json!(title), &prompt, ordinal);
        let app_thread = crate::subagents::child_thread(&parent_view["thread"], &json!({"childThreadId":child_thread,"parentNodeId":node,"activeProviderThreadId":provider_thread_id,"providerInstanceId":ctx.provider_instance_id,"modelSelection":child_selection,"title":thread_title,"createdBy":"agent","creationSource":"provider"}), &now);
        crate::subagents::ensure_child_thread(&self.store, &app_thread)?;
        let provider_thread = json!({"id":provider_thread_id,"driver":DRIVER,"providerInstanceId":ctx.provider_instance_id,"providerSessionId":self.session_id,"appThreadId":child_thread,"ownerNodeId":node,"nativeThreadRef":thread_ref(child_sid),"nativeConversationHeadRef":null,"status":"active","firstRunOrdinal":null,"lastRunOrdinal":null,"handoffIds":[],"forkedFrom":null,"pendingBackgroundTasks":[],"contextUsage":null,"nativeMetadata":null,"goal":null,"createdAt":now,"updatedAt":now});
        let directory = self.sessions[sid].directory.clone();
        let (mode, interaction) = (self.sessions[sid].runtime_mode.clone(), self.sessions[sid].interaction_mode.clone());
        let child_state = self.sessions.entry(child_sid.to_owned()).or_insert_with(|| Session { id: child_sid.to_owned(), provider_thread: provider_thread.clone(), directory: directory.clone(), agent: "build".into(), model: None, rules: None, runtime_mode: mode.clone(), interaction_mode: interaction.clone(), grants: vec![], unsettled: false, turn: None, stranded: HashSet::new(), mcp: None, instructions: None, app_thread_id: child_thread.clone(), calls: Default::default(), child: None, wakes: vec![], reports: Default::default(), stopped_children: HashSet::new() });
        child_state.provider_thread = provider_thread.clone();
        child_state.app_thread_id = child_thread.clone();
        child_state.directory = directory;
        if let Some(info) = &info {
            child_state.model = info["model"].is_object().then(|| info["model"].clone());
            if let Some(agent) = info["agent"].as_str() {
                child_state.agent = agent.to_owned();
            }
            // OpenCode gives a new session its parent's rules, which are the thread's.
            child_state.rules = info["permissions"].as_array().cloned();
        }
        let turns = child_state.child.as_ref().map_or(0, |child| child.turns);
        child_state.child = Some(ChildInfo { parent_session: sid.to_owned(), tool_id: tool_id.to_owned(), thread: app_thread, provider_thread: provider_thread.clone(), turns, prompt: Some(prompt), queued: None });
        let root = self.root_of(sid);
        self.child_owners.insert(child_sid.to_owned(), root);
        self.sessions.get_mut(sid).unwrap().calls.get_mut(tool_id).unwrap().child = Some(child_sid.to_owned());
        self.write_child_provider_thread(child_sid, &provider_thread)?;
        self.emit_subagent(sid, tool_id)
    }
    /// A child's provider thread lives in the child thread, and in the thread that holds the child's `Subagent` row.
    fn write_child_provider_thread(&mut self, child_sid: &str, row: &Value) -> Result<(), StoreError> {
        let Some(info) = self.sessions.get_mut(child_sid).and_then(|state| state.child.as_mut()) else { return Ok(()) };
        info.provider_thread = row.clone();
        let parent_session = info.parent_session.clone();
        let child_thread = child_thread_id(child_sid);
        let parent_thread = self.thread_of(&parent_session);
        commit(&self.store, &child_thread, |_| Ok(vec![("provider-thread.updated", row.clone())]))?;
        commit(&self.store, &parent_thread, |_| Ok(vec![("provider-thread.updated", row.clone())]))
    }

    /// Ends a subagent call and every call under it that is still running. A background one outlives the call that made
    /// it, as it outlives a turn, unless that call failed.
    fn settle_call(&mut self, sid: String, tool_id: String, status: &'static str, result: Option<String>) -> futures_util::future::BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            let Some(mut call) = self.sessions.get_mut(&sid).and_then(|state| state.calls.shift_remove(&tool_id)) else { return Ok(()) };
            if call.status != "running" {
                return Ok(());
            }
            call.status = status.into();
            call.completed_at = Some(at(Utc::now()));
            if let Some(result) = result {
                call.result = Some(result);
            }
            if let Some(child_sid) = call.child.clone() {
                let nested: Vec<(String, bool)> = self.sessions.get(&child_sid).map(|state| state.calls.iter().map(|(id, call)| (id.clone(), call.background)).collect()).unwrap_or_default();
                for (nested_id, background) in nested {
                    if background && status != "failed" {
                        continue;
                    }
                    self.settle_call(child_sid.clone(), nested_id, if status == "completed" { "interrupted" } else { status }, None).await?;
                }
                if status != "completed" && self.sessions.get(&child_sid).is_some_and(|state| state.turn.is_some()) {
                    self.finish_turn(&child_sid, Terminal::Interrupted, false).await?;
                }
                if let Some(mut row) = self.sessions.get(&child_sid).and_then(|state| state.child.as_ref()).map(|info| info.provider_thread.clone()) {
                    row["status"] = json!("idle");
                    row["updatedAt"] = json!(call.completed_at);
                    self.write_child_provider_thread(&child_sid, &row)?;
                }
            }
            self.write_subagent(&sid, &call)
        })
    }

    /// Opens the runless turn a subagent's session runs each execution in, on its child thread, with the prompt it
    /// answers as the turn's user message.
    async fn start_child_turn(&mut self, child_sid: &str) -> Result<(), StoreError> {
        let Some(state) = self.sessions.get_mut(child_sid) else { return Ok(()) };
        let Some(info) = state.child.as_mut().filter(|_| state.turn.is_none()) else { return Ok(()) };
        info.turns += 1;
        let (turns, prompt, queued, parent_session, provider_thread, thread_row) = (info.turns, info.prompt.take(), info.queued.take(), info.parent_session.clone(), info.provider_thread.clone(), info.thread.clone());
        let started = at(Utc::now());
        let native_turn = format!("{child_sid}:turn:{turns}");
        let turn_id = format!("provider-turn:provider:{DRIVER}:native-turn:{}", projection::enc(&native_turn));
        let root_node = projection::node_id(&format!("{native_turn}:root"));
        let child_thread = child_thread_id(child_sid);
        let parent_thread = self.thread_of(&parent_session);
        let parent_ctx = self.sessions.get(&parent_session).and_then(|state| state.turn.as_ref()).map(|turn| (turn.projector.context().clone(), turn.projector.selection().clone()));
        let (instance, selection) = match &parent_ctx {
            Some((ctx, selection)) => (ctx.provider_instance_id.clone(), selection.clone()),
            None => (provider_thread["providerInstanceId"].clone(), thread_row["modelSelection"].clone()),
        };
        let mut child_selection = selection.clone();
        child_selection["model"] = thread_row["modelSelection"]["model"].clone();
        let window = self.window_of(&self.sessions[child_sid].directory.clone(), child_selection["model"].as_str().unwrap_or(""));
        let provider_thread_id = provider_thread["id"].clone();
        let projector = Projector::for_child(&child_thread, &json!(root_node), &instance, provider_thread_id.clone(), json!(turn_id), child_selection, &format!("{child_sid}:"), window);
        let node_row = json!({"id":root_node,"threadId":child_thread,"runId":null,"parentNodeId":null,"rootNodeId":root_node,"kind":"root_turn","status":"running","countsForRun":false,"providerThreadId":provider_thread_id,"providerTurnId":turn_id,"nativeItemRef":projection::native_ref(&native_turn, "weak"),"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":started,"completedAt":null});
        let turn_row = json!({"id":turn_id,"providerThreadId":provider_thread_id,"nodeId":root_node,"runAttemptId":null,"nativeTurnRef":projection::native_ref(&native_turn, "weak"),"ordinal":turns,"status":"running","startedAt":started,"completedAt":null});
        commit(&self.store, &child_thread, |_| Ok(vec![("node.updated", node_row.clone()), ("provider-turn.updated", turn_row.clone())]))?;
        self.sessions.get_mut(child_sid).unwrap().turn = Some(Turn { projector, native_turn: native_turn.clone(), before: Some(queued), interrupted: false, unsent: false, steers: HashSet::new(), settled_inbox: HashSet::new(), held_end: None, awaiting_start: false, continuation: false });
        if let Some(prompt) = prompt.filter(|prompt| !prompt.is_empty()) {
            let child_view = view(&self.store, &child_thread)?;
            let native = format!("{native_turn}:prompt");
            let ordinal = self.turn_mut(child_sid).projector.ordinal(&child_view, &native);
            let (message, item) = crate::subagents::conversation_artifacts(
                &json!({"senderThreadId":parent_thread,"messageId":projection::message_id(&native),"turnItemId":projection::item_id(&native),"threadId":child_thread,"rootNodeId":root_node,"providerThreadId":provider_thread_id,"providerTurnId":turn_id,"nativeItemRef":projection::native_ref(&native, "weak"),"role":"user","text":prompt,"ordinal":ordinal}),
                &started,
            );
            commit(&self.store, &child_thread, |_| Ok(vec![("message.updated", message.clone()), ("turn-item.updated", item.clone())]))?;
        }
        Ok(())
    }
    /// A subagent session's execution ended: its turn settles, and a foreground call that did not complete is settled by it.
    async fn finish_child_turn(&mut self, child_sid: &str, terminal: Terminal) -> Result<(), StoreError> {
        let Some(mut turn) = self.sessions.get_mut(child_sid).and_then(|state| state.turn.take()) else { return Ok(()) };
        let ended: Vec<String> = self.pending.iter().filter(|(_, entry)| entry.session == child_sid).map(|(id, _)| id.clone()).collect();
        for id in ended {
            self.pending.remove(&id);
        }
        let child_thread = child_thread_id(child_sid);
        let directory = self.sessions[child_sid].directory.clone();
        let window = self.window_of(&directory, turn.projector.selection()["model"].as_str().unwrap_or(""));
        turn.projector.set_context_window(window);
        let thread_view = view(&self.store, &child_thread)?;
        let events = turn.projector.finish(&thread_view, terminal.status(), matches!(terminal, Terminal::Failed(_)))?;
        commit(&self.store, &child_thread, |_| Ok(events))?;
        let thread_view = view(&self.store, &child_thread)?;
        let now = at(Utc::now());
        let mut events = vec![("provider-turn.updated", turn.projector.provider_turn_row(&thread_view, terminal.status(), Some(&now), None, terminal.status())?)];
        let root = turn.projector.context().root_node_id.clone();
        if let Ok(node) = find(&thread_view, "nodes", &root) {
            let mut node = node.clone();
            node["status"] = json!(terminal.status());
            node["completedAt"] = json!(now);
            events.push(("node.updated", node));
        }
        commit(&self.store, &child_thread, |_| Ok(events))?;
        // A subagent's session is its call's to settle: a turn there is runless, and T3 has no terminal to wait on.
        let info = self.sessions.get(child_sid).and_then(|state| state.child.as_ref()).map(|info| (info.parent_session.clone(), info.tool_id.clone()));
        if let Some((parent_session, tool_id)) = info {
            let background = self.sessions.get(&parent_session).and_then(|state| state.calls.get(&tool_id)).is_some_and(|call| call.background);
            if !background && !matches!(terminal, Terminal::Completed) {
                self.settle_call(parent_session, tool_id, if matches!(terminal, Terminal::Failed(_)) { "failed" } else { "interrupted" }, None).await?;
            }
        }
        Ok(())
    }

    /// A background subagent's end, as OpenCode queues it for its parent.
    async fn on_report(&mut self, sid: &str, inbox: &str, payload: &Value) -> Result<(), StoreError> {
        let metadata = &payload["metadata"];
        let (Some(child), true) = (string_field(metadata, "childID"), string_field(metadata, "source").as_deref() == Some("subagent")) else { return Ok(()) };
        let call_id = self.sessions.get(sid).and_then(|state| state.calls.iter().find(|(_, call)| call.child.as_deref() == Some(child.as_str())).map(|(id, _)| id.clone()));
        let outcome = report_outcome(string_field(metadata, "state").as_deref());
        let text = str_of(&payload["text"]);
        let (label, child_thread) = match call_id.as_ref().and_then(|id| self.sessions[sid].calls.get(id)) {
            Some(call) => (call.title.clone(), Some(child_thread_id(&child))),
            None => (string_field(metadata, "description").or_else(|| string_field(payload, "description")), None),
        };
        let mut report = json!({"kind":"subagent","outcome":outcome});
        if let Some(label) = label {
            report["label"] = json!(label);
        }
        if let Some(child_thread) = child_thread {
            report["childThreadId"] = json!(child_thread);
        }
        self.sessions.get_mut(sid).unwrap().reports.insert(inbox.to_owned(), Report { inbox: inbox.to_owned(), child: child.clone(), report, text: text.clone() });
        let Some(call_id) = call_id else { return Ok(()) };
        let stopped = self.sessions[sid].stopped_children.contains(&child);
        let status = if stopped { "interrupted" } else { match outcome { "failed" => "failed", "cancelled" => "cancelled", _ => "completed" } };
        self.settle_call(sid.to_owned(), call_id, status, Some(subagent_output(&text))).await
    }
    /// An execution OpenCode started on a thread's session with no turn of T3's running: the parent's answer to a background
    /// subagent's report. It is held for the continuation turn it asks for, or stopped when it only reports subagents a Stop ended.
    async fn on_wake(&mut self, sid: &str) -> Result<(), StoreError> {
        let delivered: Vec<Report> = { let state = self.sessions.get_mut(sid).unwrap(); state.reports.drain(..).map(|(_, report)| report).collect() };
        let stopped = !delivered.is_empty() && delivered.iter().all(|entry| self.sessions[sid].stopped_children.contains(&entry.child));
        for entry in &delivered {
            self.sessions.get_mut(sid).unwrap().stopped_children.remove(&entry.child);
        }
        if stopped {
            self.sessions.get_mut(sid).unwrap().unsettled = true;
            let _ = self.session_call(Method::POST, sid, "/interrupt", &[], None, Some(INTERRUPT_TIMEOUT)).await;
            return Ok(());
        }
        let wake = Wake { events: vec![], running: true, reports: delivered.iter().map(|entry| entry.report.clone()).collect(), detail: (!delivered.is_empty()).then(|| delivered.iter().map(|entry| entry.text.clone()).collect::<Vec<_>>().join("\n\n")), dropped: false, after: delivered.last().map(|entry| entry.inbox.clone()), first: delivered.first().map(|entry| entry.inbox.clone()) };
        let (detail, reports) = (wake.detail.clone(), wake.reports.clone());
        self.sessions.get_mut(sid).unwrap().wakes.push(wake);
        tracing::info!("OpenCode started a turn on its own; asking for a continuation.");
        let thread = self.thread_of(sid);
        let notification = crate::notification::background_work_notification(&reports);
        crate::provider_continuation::offer(&self.store, &self.providers, &thread, detail.as_deref(), notification)
    }
    /// The thread whose held execution an event belongs to: the session's own, or a subagent session its held execution started.
    fn holder_of(&self, sid: &str) -> Option<String> {
        if let Some(state) = self.sessions.get(sid) {
            return state.wakes.last().is_some_and(|wake| wake.running).then(|| sid.to_owned());
        }
        // A subagent a held execution started is named only when that execution replays.
        let parent = self.announced.get(sid)?["parentID"].as_str()?.to_owned();
        let holder = self.sessions.get(&parent)?;
        (holder.turn.is_none() && !holder.wakes.is_empty()).then_some(parent)
    }
    /// Feeds a held execution's events to the turn now running on its session.
    async fn replay_wake(&mut self, events: Vec<Value>) -> Result<(), StoreError> {
        for event in events {
            let kind = str_of(&event["type"]);
            let session_id = session_of(&event);
            self.route(&event, &kind, session_id).await?;
        }
        Ok(())
    }
    /// A user turn that starts while OpenCode runs an execution on its own: the prompt joins that execution, so the turn takes
    /// it, and its continuation turn is no longer needed.
    async fn take_running_wake(&mut self, sid: &str) -> Result<(), StoreError> {
        let Some(wake) = self.sessions.get_mut(sid).and_then(|state| state.wakes.last().is_some_and(|wake| wake.running).then(|| state.wakes.pop()).flatten()) else { return Ok(()) };
        self.replay_wake(wake.events).await
    }
    /// The continuation turn for an execution OpenCode started on its own: it takes the oldest one held and ends with it.
    async fn run_wake(&mut self, sid: &str, run_id: &str, selection: &Value, attempt_id: &str) -> Result<(), String> {
        let native_turn = wake_turn_id(sid, attempt_id);
        self.begin_turn(sid, run_id, selection, &native_turn, false, true).map_err(|e| e.to_string())?;
        let wake = { let state = self.sessions.get_mut(sid).unwrap(); (!state.wakes.is_empty()).then(|| state.wakes.remove(0)) };
        let Some(wake) = wake else {
            return self.finish_turn(sid, Terminal::Completed, false).await.map_err(|e| e.to_string());
        };
        if let Some(turn) = self.sessions.get_mut(sid).and_then(|state| state.turn.as_mut()) {
            turn.before = Some(wake.after.clone());
            // Its first report is where its history begins, so fork and rollback cut there.
            if let Some(first) = &wake.first {
                turn.native_turn = first.clone();
            }
        }
        self.replay_wake(wake.events).await.map_err(|e| e.to_string())
    }
    /// Stops a thread's background subagents and any execution OpenCode is running on its own. Their sessions end as
    /// interrupted; the reports OpenCode then queues wake the parent into an execution that is stopped as well.
    async fn stop_background(&mut self) -> Result<(), String> {
        let Some(root) = self.sessions.iter().find(|(_, state)| state.child.is_none()).map(|(id, _)| id.clone()) else { return Ok(()) };
        let calls: Vec<(String, String, String)> = self.sessions_of(&root).iter().flat_map(|sid| self.sessions[sid].calls.iter().filter(|(_, call)| call.background).filter_map(|(id, call)| Some((sid.clone(), id.clone(), call.child.clone()?))).collect::<Vec<_>>()).collect();
        for (caller, tool, child) in calls {
            self.sessions.get_mut(&caller).unwrap().stopped_children.insert(child.clone());
            let reached = self.session_call(Method::POST, &child, "/interrupt", &[], None, Some(INTERRUPT_TIMEOUT)).await;
            if reached.is_ok() || reached.as_ref().err().and_then(ApiError::tag) == Some("SessionNotFoundError") {
                self.settle_call(caller, tool, "interrupted", None).await.map_err(|e| e.to_string())?;
            } else {
                tracing::warn!("Could not stop an OpenCode subagent.");
            }
        }
        // A held execution no turn will take is stopped, not replayed.
        let running = self.sessions.get(&root).is_some_and(|state| state.wakes.iter().any(|wake| wake.running));
        if let Some(state) = self.sessions.get_mut(&root) {
            for wake in state.wakes.drain(..) {
                let _ = wake.dropped;
            }
        }
        if running {
            self.sessions.get_mut(&root).unwrap().unsettled = true;
            let _ = self.session_call(Method::POST, &root, "/interrupt", &[], None, Some(INTERRUPT_TIMEOUT)).await;
        }
        Ok(())
    }
}

enum Asked {
    Permission(Value),
    Form(Value),
}
/// The model OpenCode reports running, as the provider thread names it.
pub(crate) fn reported_selection(instance: &Value, model: &Value) -> Value {
    let mut selection = json!({"instanceId":instance,"model":format!("{}/{}", str_of(&model["providerID"]), str_of(&model["id"])),"options":[]});
    if let Some(variant) = model["variant"].as_str() {
        selection["options"] = json!([{"id":"variant","value":variant}]);
    }
    selection
}
/// Where fork and rollback cut to keep the session's turns up to `kept` (all of them before the first turn when
/// `None`): before the message that starts the next turn that reached OpenCode, which is in `prompts`. `None` means
/// nothing follows, so there is no cut; a later turn from before T3 chose prompt ids has no known message, so no cut is safe.
fn boundary_after(turns: &[Value], provider_thread_id: &Value, kept: Option<&Value>, prompts: &HashSet<String>) -> Result<Option<String>, String> {
    let mut later: Vec<&Value> = turns.iter().filter(|turn| turn["providerThreadId"] == *provider_thread_id && kept.is_none_or(|kept| turn["ordinal"].as_u64() > kept["ordinal"].as_u64())).collect();
    later.sort_by_key(|turn| turn["ordinal"].as_u64());
    if later.iter().any(|turn| prompt_of(turn).is_none() && !is_wake_turn(turn)) {
        return Err("This OpenCode conversation has turns from an earlier T3 Code version, so it can't be cut there.".into());
    }
    Ok(later.iter().filter_map(|turn| prompt_of(turn)).find(|prompt| prompts.contains(prompt)))
}

/// Whether the thread's OpenCode instance is a 1.x install. An install that cannot be probed is left to the 2.x adapter,
/// whose connect reports why.
async fn is_one_x(store: &Store, providers: &ProviderRegistry, thread_id: &str) -> bool {
    let Ok(thread) = view(store, thread_id) else { return false };
    let Some(instance) = thread["thread"]["modelSelection"]["instanceId"].as_str().and_then(|id| providers.opencode(id).ok()) else { return false };
    matches!(instance.probe(false).await, Ok(probed) if probed.generation == Generation::V1)
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
    // The install's generation picks the protocol: a 1.x server is driven by its own adapter.
    let one_x = tokio::select! { one_x = is_one_x(&store, &providers, &thread_id) => one_x, _ = stopped.changed() => return };
    if one_x {
        return crate::opencode1_adapter::actor(store, providers, thread_id, work, stopped, canceled, _lifetime, mcp).await;
    }
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
                // Reconnecting can wait on backoff; a stop does not wait for it.
                let handled = tokio::select! { handled = actor.handle_frame(frame) => handled, _ = stopped.changed() => break };
                if let Err(cause) = handled {
                    tracing::error!(%cause, "OpenCode provider event could not be persisted");
                    let failed = crate::provider_failure::make(&json!({"message":cause.to_string(),"class":"transport_error","retryable":false}));
                    let active: Vec<TurnContext> = actor.sessions.values().filter_map(|state| state.turn.as_ref().map(|turn| turn.projector.context().clone())).collect();
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
        // A server T3 did not start keeps running after T3 stops, so the turns it would otherwise finish unseen are stopped.
        if actor.external() {
            let active: Vec<String> = actor.sessions.iter().filter(|(_, state)| state.turn.is_some()).map(|(id, _)| id.clone()).collect();
            for id in active {
                let _ = actor.session_call(Method::POST, &id, "/interrupt", &[], None, Some(Duration::from_secs(1))).await;
            }
        }
        let contexts: Vec<TurnContext> = actor.sessions.values().filter_map(|state| state.turn.as_ref().map(|turn| turn.projector.context().clone())).collect();
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
