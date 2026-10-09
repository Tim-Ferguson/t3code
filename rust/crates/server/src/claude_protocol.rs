//! Claude Code subprocess protocol: launch arguments, environment, message
//! shapes and the newline-delimited JSON transport.
//!
//! Derived from `@anthropic-ai/claude-agent-sdk` 0.3.276 (`sdk.mjs` for the
//! launch code and control flow, `sdk.d.ts` for the message and control
//! request shapes). The SDK is a thin wrapper that spawns the `claude` CLI
//! with `--input-format stream-json --output-format stream-json` and speaks
//! this protocol on stdin/stdout; nothing here runs the SDK.
//!
//! This module knows nothing about orchestration. The adapter consumes
//! [`Event`]s and answers control requests through [`ClaudeProcess`].
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot, watch},
};

/// SDK version the shapes and flags below were derived from.
pub const SDK_VERSION: &str = "0.3.276";
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Which tools the CLI exposes: its default set (`--tools default`) or an explicit list.
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // `List` is the SDK's explicit tool list; read-only sandboxes use it in the source.
pub enum Tools {
    Preset,
    List(Vec<String>),
}
/// How the CLI is bound to a conversation file.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionIdentity {
    /// A new conversation with this id (`--session-id=`).
    New(String),
    /// An existing conversation (`--resume=`).
    Resume(String),
}
#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)] // The adapter only requests adaptive summaries; the SDK's other modes stay expressible.
pub enum Thinking {
    Adaptive { display: Option<String> },
    Disabled,
    Enabled { budget_tokens: Option<u64>, display: Option<String> },
}

/// The subset of SDK `Options` the T3 adapter passes, and nothing else.
#[derive(Clone, Debug)]
pub struct QueryOptions {
    pub binary: PathBuf,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub model: String,
    pub effort: Option<String>,
    pub thinking: Option<Thinking>,
    pub tools: Option<Tools>,
    pub permission_mode: String,
    pub allow_dangerously_skip_permissions: bool,
    pub include_partial_messages: bool,
    pub identity: Option<SessionIdentity>,
    pub resume_session_at: Option<String>,
    /// `forkSession`: the resumed conversation continues under a new session id.
    pub fork_session: bool,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// `canUseTool` installed: permission prompts arrive as `can_use_tool` control requests.
    pub can_use_tool: bool,
    pub mcp_servers: Option<Value>,
    pub strict_mcp_config: bool,
    pub setting_sources: Option<Vec<String>>,
    pub persist_session: bool,
    pub additional_directories: Vec<String>,
    /// `--settings`: an object is serialized to JSON, as the SDK does.
    pub settings: Option<Value>,
    /// SDK `extraArgs`, in insertion order; `None` is a bare flag.
    pub extra_args: Vec<(String, Option<String>)>,
    /// `systemPrompt: {type:"preset", append}`: sent in the `initialize` request.
    pub append_system_prompt: Option<String>,
    /// A custom system prompt replaces the CLI's (`systemPrompt: [text]` in `initialize`).
    pub system_prompt: Option<String>,
    pub supported_dialog_kinds: Vec<String>,
}

impl QueryOptions {
    /// Command line, in the order `ProcessTransport.initialize` builds it.
    pub fn args(&self) -> Vec<String> {
        let mut q: Vec<String> = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"]
            .map(String::from)
            .into();
        match &self.thinking {
            Some(Thinking::Adaptive { display }) => {
                q.extend(["--thinking".into(), "adaptive".into()]);
                if let Some(display) = display {
                    q.extend(["--thinking-display".into(), display.clone()]);
                }
            }
            Some(Thinking::Disabled) => q.extend(["--thinking".into(), "disabled".into()]),
            Some(Thinking::Enabled { budget_tokens, display }) => {
                match budget_tokens {
                    None => q.extend(["--thinking".into(), "adaptive".into()]),
                    Some(budget) => q.extend(["--max-thinking-tokens".into(), budget.to_string()]),
                }
                if let Some(display) = display {
                    q.extend(["--thinking-display".into(), display.clone()]);
                }
            }
            None => {}
        }
        if let Some(effort) = &self.effort {
            q.extend(["--effort".into(), effort.clone()]);
        }
        if !self.model.is_empty() {
            q.extend(["--model".into(), self.model.clone()]);
        }
        if self.can_use_tool {
            q.extend(["--permission-prompt-tool".into(), "stdio".into()]);
        }
        if let Some(SessionIdentity::Resume(id)) = &self.identity {
            q.push(format!("--resume={id}"));
        }
        if !self.allowed_tools.is_empty() {
            q.extend(["--allowedTools".into(), self.allowed_tools.join(",")]);
        }
        if !self.disallowed_tools.is_empty() {
            q.extend(["--disallowedTools".into(), self.disallowed_tools.join(",")]);
        }
        match &self.tools {
            Some(Tools::Preset) => q.extend(["--tools".into(), "default".into()]),
            Some(Tools::List(list)) => q.extend(["--tools".into(), list.join(",")]),
            None => {}
        }
        if let Some(servers) = self.mcp_servers.as_ref().filter(|s| s.as_object().is_some_and(|o| !o.is_empty())) {
            q.extend(["--mcp-config".into(), json!({"mcpServers":servers}).to_string()]);
        }
        if let Some(sources) = &self.setting_sources {
            q.push(format!("--setting-sources={}", sources.join(",")));
        }
        if self.strict_mcp_config {
            q.push("--strict-mcp-config".into());
        }
        q.extend(["--permission-mode".into(), self.permission_mode.clone()]);
        if self.allow_dangerously_skip_permissions {
            q.push("--allow-dangerously-skip-permissions".into());
        }
        if self.include_partial_messages {
            q.push("--include-partial-messages".into());
        }
        for directory in &self.additional_directories {
            q.extend(["--add-dir".into(), directory.clone()]);
        }
        if self.fork_session {
            q.push("--fork-session".into());
        }
        if let Some(at) = &self.resume_session_at {
            q.push(format!("--resume-session-at={at}"));
        }
        if let Some(SessionIdentity::New(id)) = &self.identity {
            q.push(format!("--session-id={id}"));
        }
        if !self.persist_session {
            q.push("--no-session-persistence".into());
        }
        // extraArgs first, then `settings`, mirroring `{...extraArgs, settings}`.
        for (key, value) in &self.extra_args {
            push_flag(&mut q, key, value.as_deref());
        }
        if let Some(settings) = &self.settings {
            let text = match settings {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            push_flag(&mut q, "settings", Some(&text));
        }
        q
    }
    /// Child environment: the supplied map plus the markers the SDK exports.
    pub fn environment(&self) -> HashMap<String, String> {
        let mut env = self.env.clone();
        env.entry("CLAUDE_CODE_ENTRYPOINT".into()).or_insert_with(|| "sdk-ts".into());
        env.entry("CLAUDE_AGENT_SDK_VERSION".into()).or_insert_with(|| SDK_VERSION.into());
        env
    }
    /// First line written to stdin: configures the system prompt and dialogs.
    pub fn initialize_request(&self) -> Value {
        let mut request = serde_json::Map::new();
        request.insert("subtype".into(), json!("initialize"));
        if let Some(prompt) = &self.system_prompt {
            request.insert("systemPrompt".into(), json!([prompt]));
        }
        if let Some(append) = &self.append_system_prompt {
            request.insert("appendSystemPrompt".into(), json!(append));
        }
        if !self.supported_dialog_kinds.is_empty() {
            request.insert("supportedDialogKinds".into(), json!(self.supported_dialog_kinds));
        }
        Value::Object(request)
    }
    pub fn launch(&self) -> Launch {
        Launch { binary: self.binary.clone(), args: self.args(), cwd: self.cwd.clone(), env: self.environment() }
    }
}
fn push_flag(args: &mut Vec<String>, key: &str, value: Option<&str>) {
    match value {
        None => args.push(format!("--{key}")),
        Some(value) if value.len() > 1 && value.starts_with('-') => args.push(format!("--{key}={value}")),
        Some(value) => args.extend([format!("--{key}"), value.to_owned()]),
    }
}

#[derive(Clone, Debug)]
pub struct Launch {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}

/// A user turn written to stdin (`SDKUserMessage`).
pub fn user_message(content: Value, priority: Option<&str>, uuid: Option<&str>) -> Value {
    let mut message = json!({"type":"user","message":{"role":"user","content":content},"parent_tool_use_id":null});
    if let Some(priority) = priority {
        message["priority"] = json!(priority);
    }
    if let Some(uuid) = uuid {
        message["uuid"] = json!(uuid);
    }
    message
}
/// `PermissionResult`, the payload of a `can_use_tool` control response.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "behavior", rename_all = "camelCase")]
pub enum PermissionResult {
    #[serde(rename_all = "camelCase")]
    Allow {
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<Value>,
        #[serde(rename = "toolUseID", skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        decision_classification: Option<&'static str>,
    },
    #[serde(rename_all = "camelCase")]
    Deny {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
        #[serde(rename = "toolUseID", skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        decision_classification: Option<&'static str>,
    },
}

/// What the CLI emits on stdout. Only the discriminants the adapter routes on
/// are decoded here; payloads stay as JSON for the projection layer.
#[derive(Debug)]
pub enum Event {
    /// An `SDKMessage` (`system`, `assistant`, `user`, `stream_event`, `result`, ...).
    Message(Value),
    /// A control request from the CLI (`can_use_tool`, `request_user_dialog`, `hook_callback`, ...).
    Request { request_id: String, request: Value },
    /// The CLI withdrew one of its own requests (for example after an interrupt).
    Cancel { request_id: String },
    /// stdout ended. Always the last event.
    Closed(Exit),
}
#[derive(Debug, Clone, Default)]
pub struct Exit {
    pub code: Option<i32>,
    pub stderr_tail: String,
    /// The transport closed it (shutdown or drop) rather than the CLI exiting on its own.
    pub closed_by_host: bool,
}
impl Exit {
    pub fn describe(&self) -> String {
        let tail = self.stderr_tail.trim();
        let suffix = if tail.is_empty() { String::new() } else { format!(". stderr: {tail}") };
        match self.code {
            Some(0) | None if self.closed_by_host => "Claude Code process was closed.".into(),
            Some(code) => format!("Claude Code process exited with code {code}{suffix}"),
            None => format!("Claude Code process terminated{suffix}"),
        }
    }
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;
/// A running `claude` subprocess. Dropping it kills the child.
pub struct ClaudeProcess {
    writer: mpsc::UnboundedSender<String>,
    pending: Pending,
    shutdown: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

impl ClaudeProcess {
    pub fn spawn(launch: &Launch) -> Result<(Self, mpsc::UnboundedReceiver<Event>), String> {
        // The packaged `cli.js` of older npm versions runs under a JavaScript runtime.
        let script = launch.binary.extension().is_some_and(|extension| matches!(extension.to_str(), Some("js" | "mjs" | "cjs")));
        let mut command = if script { Command::new("node") } else { Command::new(&launch.binary) };
        if script {
            command.arg(&launch.binary);
        }
        command.args(&launch.args).envs(&launch.env).current_dir(&launch.cwd);
        // The SDK deletes these from the inherited environment before spawning.
        command.env_remove("NODE_OPTIONS");
        if !launch.env.contains_key("DEBUG_CLAUDE_AGENT_SDK") {
            command.env_remove("DEBUG");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Failed to spawn Claude Code process: {error}"))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (writer, mut lines) = mpsc::unbounded_channel::<String>();
        let (events, receiver) = mpsc::unbounded_channel();
        let pending: Pending = Default::default();
        let (shutdown, mut stopping) = watch::channel(false);
        let (done_sender, done) = watch::channel(false);
        let tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr_tail = tail.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buffer = [0u8; 1024];
            while let Ok(size) = stderr.read(&mut buffer).await {
                if size == 0 {
                    break;
                }
                let mut tail = stderr_tail.lock().unwrap();
                tail.extend_from_slice(&buffer[..size]);
                let extra = tail.len().saturating_sub(4096);
                tail.drain(..extra);
            }
        });
        let reader_pending = pending.clone();
        let host_closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_host_closed = host_closed.clone();
        tokio::spawn(async move {
            let writer_task = tokio::spawn(async move {
                while let Some(line) = lines.recv().await {
                    if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err() {
                        break;
                    }
                }
                // Ending the writer closes stdin, which is how the CLI is asked to finish.
                drop(stdin);
            });
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                tokio::select! {
                    result = reader.read_until(b'\n', &mut line) => {
                        match result {
                            Ok(0) | Err(_) => break,
                            Ok(_) if line.len() > MAX_LINE_BYTES => break,
                            Ok(_) => route(&line, &events, &reader_pending),
                        }
                    }
                    _ = stopping.changed() => {
                        // Close stdin first and give the CLI a moment to exit by itself.
                        reader_host_closed.store(true, std::sync::atomic::Ordering::SeqCst);
                        writer_task.abort();
                        let wait = tokio::time::timeout(Duration::from_millis(1500), async {
                            loop { line.clear(); if !matches!(reader.read_until(b'\n', &mut line).await, Ok(n) if n > 0) { break; } route(&line, &events, &reader_pending); }
                        }).await;
                        let _ = wait;
                        break;
                    }
                }
            }
            let status = match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
                Ok(status) => status.ok(),
                Err(_) => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            };
            writer_task.abort();
            stderr_task.abort();
            for (_, waiter) in reader_pending.lock().unwrap().drain() {
                let _ = waiter.send(Err("Claude Code process ended before responding.".into()));
            }
            let exit = Exit {
                code: status.and_then(|status| status.code()),
                stderr_tail: String::from_utf8_lossy(&tail.lock().unwrap()).into_owned(),
                closed_by_host: reader_host_closed.load(std::sync::atomic::Ordering::SeqCst),
            };
            let _ = events.send(Event::Closed(exit));
            let _ = done_sender.send(true);
        });
        Ok((Self { writer, pending, shutdown, done }, receiver))
    }
    /// Write one JSON line to stdin.
    pub fn write(&self, value: &Value) -> Result<(), String> {
        self.writer.send(value.to_string()).map_err(|_| "Cannot write to terminated Claude Code process.".to_owned())
    }
    /// Send a `control_request` and wait for its `control_response`. A timeout
    /// withdraws the request with `control_cancel_request`, as the SDK does.
    pub async fn request(&self, request: Value, timeout: Duration) -> Result<Value, String> {
        let id = format!("t3-{}", uuid::Uuid::new_v4().simple());
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), sender);
        if let Err(error) = self.write(&json!({"request_id":id,"type":"control_request","request":request})) {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Claude Code process ended before responding.".into()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                let _ = self.write(&json!({"type":"control_cancel_request","request_id":id}));
                Err("Claude Code control request timed out.".into())
            }
        }
    }
    /// Answer a control request the CLI sent: success carries `response`, failure an error string.
    pub fn respond(&self, request_id: &str, result: Result<Value, String>) -> Result<(), String> {
        let response = match result {
            Ok(response) => json!({"subtype":"success","request_id":request_id,"response":response}),
            Err(error) => json!({"subtype":"error","request_id":request_id,"error":error}),
        };
        self.write(&json!({"type":"control_response","response":response}))
    }
    /// Close stdin, wait for the CLI to exit, kill it if it does not, and reap it.
    pub async fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        let mut done = self.done.clone();
        let _ = done.wait_for(|done| *done).await;
    }
}
impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

fn route(line: &[u8], events: &mpsc::UnboundedSender<Event>, pending: &Pending) {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        tracing::debug!("Ignoring non-JSON Claude Code stdout line");
        return;
    };
    match value["type"].as_str() {
        Some("control_response") => {
            let response = &value["response"];
            let Some(id) = response["request_id"].as_str() else { return };
            if let Some(waiter) = pending.lock().unwrap().remove(id) {
                let _ = waiter.send(if response["subtype"] == "success" {
                    Ok(response["response"].clone())
                } else {
                    Err(response["error"].as_str().unwrap_or("Claude Code control request failed.").to_owned())
                });
            }
        }
        Some("control_request") => {
            if let Some(id) = value["request_id"].as_str() {
                let _ = events.send(Event::Request { request_id: id.to_owned(), request: value["request"].clone() });
            }
        }
        Some("control_cancel_request") => {
            if let Some(id) = value["request_id"].as_str() {
                let _ = events.send(Event::Cancel { request_id: id.to_owned() });
            }
        }
        Some("keep_alive" | "transcript_mirror") => {}
        _ => {
            let _ = events.send(Event::Message(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> QueryOptions {
        QueryOptions {
            binary: "/usr/local/bin/claude".into(),
            cwd: "/tmp/ws".into(),
            env: HashMap::from([("PATH".into(), "/usr/bin".into())]),
            model: "claude-sonnet-4-6".into(),
            effort: Some("high".into()),
            thinking: Some(Thinking::Adaptive { display: Some("summarized".into()) }),
            tools: Some(Tools::Preset),
            permission_mode: "default".into(),
            allow_dangerously_skip_permissions: false,
            include_partial_messages: true,
            identity: Some(SessionIdentity::New("session".into())),
            resume_session_at: None,
            fork_session: false,
            allowed_tools: vec!["mcp__t3-code__*".into()],
            disallowed_tools: vec![],
            can_use_tool: true,
            mcp_servers: None,
            strict_mcp_config: false,
            setting_sources: None,
            persist_session: true,
            additional_directories: vec!["/tmp/ws".into()],
            settings: Some(json!({"showThinkingSummaries":true})),
            extra_args: vec![("thinking-display".into(), Some("summarized".into()))],
            append_system_prompt: Some("append".into()),
            system_prompt: None,
            supported_dialog_kinds: vec![],
        }
    }
    #[test]
    fn flags_follow_the_sdk_order_and_dash_values_use_equals() {
        let args = options().args();
        assert_eq!(&args[..5], ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"]);
        assert_eq!(args.last().unwrap(), "{\"showThinkingSummaries\":true}");
        assert!(args.contains(&"--session-id=session".to_string()));
        let mut dash = options();
        dash.extra_args = vec![("flag".into(), Some("-x".into())), ("bare".into(), None)];
        let args = dash.args();
        assert!(args.contains(&"--flag=-x".to_string()) && args.contains(&"--bare".to_string()));
    }
    #[test]
    fn environment_marks_the_sdk_entrypoint_without_overriding_the_caller() {
        let env = options().environment();
        assert_eq!(env["CLAUDE_CODE_ENTRYPOINT"], "sdk-ts");
        assert_eq!(env["CLAUDE_AGENT_SDK_VERSION"], SDK_VERSION);
        let mut explicit = options();
        explicit.env.insert("CLAUDE_CODE_ENTRYPOINT".into(), "custom".into());
        assert_eq!(explicit.environment()["CLAUDE_CODE_ENTRYPOINT"], "custom");
    }
    #[test]
    fn permission_results_serialize_to_the_sdk_shapes() {
        let allow = PermissionResult::Allow { updated_input: Some(json!({"a":1})), updated_permissions: None, tool_use_id: Some("tool".into()), decision_classification: Some("user_temporary") };
        assert_eq!(serde_json::to_value(allow).unwrap(), json!({"behavior":"allow","updatedInput":{"a":1},"toolUseID":"tool","decisionClassification":"user_temporary"}));
        let deny = PermissionResult::Deny { message: "no".into(), interrupt: Some(true), tool_use_id: None, decision_classification: Some("user_reject") };
        assert_eq!(serde_json::to_value(deny).unwrap(), json!({"behavior":"deny","message":"no","interrupt":true,"decisionClassification":"user_reject"}));
    }
}
