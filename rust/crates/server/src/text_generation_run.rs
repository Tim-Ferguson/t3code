//! Running one prompt through a provider's CLI (port of `CodexTextGeneration.ts` and
//! `ClaudeTextGeneration.ts`). Both are one-shot, non-interactive runs that can only answer: the
//! prompt goes in on stdin, the CLI is held to a structured answer, and it is given no tools to edit
//! files or run commands with.
//!
//! * Codex: `codex exec --ephemeral --skip-git-repo-check -s read-only --output-schema <file>
//!   --output-last-message <file> -`, in the project's directory; the answer is the file.
//! * Claude: `claude -p --output-format json --json-schema <schema> --tools "" --permission-mode
//!   dontAsk --strict-mcp-config --disable-slash-commands --settings {"disableAllHooks":true,…}`;
//!   the answer is `structured_output` of the JSON on stdout. A thread title runs in a fresh empty
//!   directory, because it needs nothing from the checkout.
//!
//! The binary, environment and home come from the same instance configuration the provider's own
//! sessions use (`CodexInstance::launch`, `ClaudeInstance::binary_path`/`process_environment`).
//! These are batch runs with no protocol to speak, so they do not go through the JSON-lines
//! transports that supervise a provider's sessions: the child is killed if the run is dropped, and
//! a run that outlives its timeout is dropped.
use crate::{
    claude_instance::ClaudeInstance,
    codex::{CodexInstance, tokenize_cli_args},
    codex_managed::Effective,
    provider_auth_flow::StopSession,
    provider_registry::{ProviderRegistry, TextInstance},
    text_generation::{Operation, Request, Runner, Shape, TextGenerationError},
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::{collections::HashMap, path::{Path, PathBuf}, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

/// How long one run may take (`CODEX_TIMEOUT_MS`, `CLAUDE_TIMEOUT_MS`) and how a failed thread
/// title is retried (`Schedule.exponential("2 seconds")`).
#[derive(Debug, Clone)]
pub struct Options {
    pub timeout: Duration,
    pub retry_delay: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(180), retry_delay: Duration::from_secs(2) }
    }
}

// ---------------------------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------------------------

/// `codexExecLaunchArgs`: the configured launch arguments that `codex exec` understands.
pub fn codex_exec_launch_args(launch_args: &str) -> Vec<String> {
    let args = tokenize_cli_args(launch_args);
    let mut kept = vec![];
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--strict-config" || arg.starts_with("--config=") || arg.starts_with("-c=") || arg.starts_with("--enable=") || arg.starts_with("--disable=") {
            kept.push(arg.clone());
        } else if matches!(arg.as_str(), "--config" | "-c" | "--enable" | "--disable") {
            if let Some(value) = args.get(index + 1).filter(|value| !value.starts_with('-')) {
                kept.push(arg.clone());
                kept.push(value.clone());
                index += 1;
            }
        }
        index += 1;
    }
    kept
}

pub struct CodexRun<'a> {
    pub launch_args: &'a str,
    pub model: &'a str,
    pub reasoning_effort: &'a str,
    pub service_tier: Option<&'a str>,
    pub schema_path: &'a str,
    pub output_path: &'a str,
    pub image_paths: &'a [String],
}

/// The arguments of `codex exec` for one run; the prompt follows on stdin (`-`).
pub fn codex_args(run: &CodexRun) -> Vec<String> {
    let mut args: Vec<String> = vec!["exec".into()];
    args.extend(codex_exec_launch_args(run.launch_args));
    args.extend(["--ephemeral", "--skip-git-repo-check", "-s", "read-only", "--model"].map(String::from));
    args.push(run.model.into());
    args.extend(["--config".into(), format!("model_reasoning_effort=\"{}\"", run.reasoning_effort)]);
    if let Some(tier) = run.service_tier {
        args.extend(["--config".into(), format!("service_tier=\"{tier}\"")]);
    }
    args.extend(["--output-schema".into(), run.schema_path.into(), "--output-last-message".into(), run.output_path.into()]);
    for path in run.image_paths {
        args.extend(["--image".into(), path.clone()]);
    }
    args.push("-".into());
    args
}

/// `codexModelFamily`.
fn codex_model_family(slug: &str) -> &str {
    if slug.starts_with("openai.gpt-") { &slug["openai.".len()..] } else { slug }
}

/// The model slug to pass: the requested slug when the instance lists it, else the listed model
/// whose family it names, else what was asked for.
pub fn codex_model(requested: &str, models: &[Value]) -> String {
    let slug = |model: &Value| model["slug"].as_str().map(str::to_string);
    models
        .iter()
        .find(|model| model["slug"] == requested)
        .and_then(slug)
        .or_else(|| models.iter().find(|model| model["isCustom"] != true && model["slug"].as_str().is_some_and(|candidate| codex_model_family(candidate) == requested)).and_then(slug))
        .unwrap_or_else(|| requested.to_string())
}

fn string_option<'a>(selection: &'a Value, id: &str) -> Option<&'a str> {
    selection["options"].as_array()?.iter().find(|option| option["id"] == id)?["value"].as_str()
}

fn boolean_option(selection: &Value, id: &str) -> Option<bool> {
    selection["options"].as_array()?.iter().find(|option| option["id"] == id)?["value"].as_bool()
}

/// `getCodexServiceTierOptionValue`.
pub fn codex_service_tier(selection: &Value) -> Option<String> {
    string_option(selection, "serviceTier").map(str::to_string).or_else(|| (boolean_option(selection, "fastMode") == Some(true)).then(|| "fast".to_string()))
}

/// The arguments of `claude -p` for one run; the prompt follows on stdin.
pub fn claude_args(shape: Shape, selection: &Value) -> Vec<String> {
    let compiled = crate::claude_models::compile_selection(selection);
    let mut settings = Map::new();
    settings.insert("disableAllHooks".into(), json!(true));
    if let Some(thinking) = compiled.settings.get("alwaysThinkingEnabled").filter(|value| value.is_boolean()) {
        settings.insert("alwaysThinkingEnabled".into(), thinking.clone());
    }
    if compiled.settings.get("fastMode") == Some(&json!(true)) {
        settings.insert("fastMode".into(), json!(true));
    }
    if compiled.settings.get("ultracode") == Some(&json!(true)) {
        settings.insert("ultracode".into(), json!(true));
    }
    let mut args: Vec<String> = ["-p", "--output-format", "json", "--json-schema"].map(String::from).to_vec();
    args.push(serde_json::to_string(shape.schema()).expect("a schema encodes"));
    args.extend(["--model".into(), compiled.api_model_id.clone()]);
    if let Some(effort) = &compiled.effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    args.extend(["--settings".into(), Value::Object(settings).to_string()]);
    // Metadata prompts need no executable capabilities, even when they contain a skill name.
    args.extend(["--tools", "", "--disable-slash-commands", "--strict-mcp-config", "--permission-mode", "dontAsk"].map(String::from));
    args
}

// ---------------------------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------------------------

/// How Claude's stdout failed to be an answer.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaudeReadFailure {
    /// Not the JSON the CLI is documented to print.
    Format,
    /// An answer that does not fit the schema.
    Invalid,
}

/// The structured answer in `claude -p --output-format json`: an object with `structured_output`, or
/// (verbose mode) an array of messages whose last `result` carries it.
pub fn read_claude_answer(stdout: &str, shape: Shape) -> Result<Value, ClaudeReadFailure> {
    let parsed: Value = serde_json::from_str(stdout).map_err(|_| ClaudeReadFailure::Format)?;
    let answer = match &parsed {
        Value::Object(object) if object.contains_key("structured_output") => object["structured_output"].clone(),
        Value::Array(messages) => {
            if !messages.iter().all(|message| message.as_object().is_some_and(|message| message.get("type").is_some_and(Value::is_string))) {
                return Err(ClaudeReadFailure::Format);
            }
            messages.iter().rev().find(|message| message["type"] == "result").map(|message| message["structured_output"].clone()).unwrap_or(Value::Null)
        }
        _ => return Err(ClaudeReadFailure::Format),
    };
    shape.decode(&answer).ok_or(ClaudeReadFailure::Invalid)
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

struct Finished {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn drain(mut stream: impl AsyncReadExt + Unpin) -> String {
    let mut bytes = vec![];
    let _ = stream.read_to_end(&mut bytes).await;
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Runs the CLI with the prompt on stdin and collects everything it prints. The child is killed if
/// this future is dropped.
async fn spawn(binary: &Path, args: &[String], environment: &HashMap<String, String>, cwd: &Path, prompt: String) -> std::io::Result<Finished> {
    let mut command = if cfg!(windows) {
        let mut merged: indexmap::IndexMap<String, String> = std::env::vars().collect();
        merged.extend(environment.clone());
        crate::acp_registry_spawn::command(&binary.to_string_lossy(), args, Some(&merged))
    } else {
        let mut command = Command::new(binary);
        command.args(args).envs(environment);
        command
    };
    let mut child = command.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let (stdout, stderr) = (child.stdout.take().expect("piped stdout"), child.stderr.take().expect("piped stderr"));
    // A CLI that exits before reading its prompt closes the pipe; that is its failure to report.
    let write = async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        drop(stdin);
    };
    let (_, stdout, stderr, status) = tokio::join!(write, drain(stdout), drain(stderr), child.wait());
    Ok(Finished { code: status?.code().unwrap_or(-1), stdout, stderr })
}

/// A temporary directory that is removed when it goes out of scope, whatever happened.
struct Scratch(PathBuf);

impl Scratch {
    fn new(prefix: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("{prefix}{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn file(&self, content: &str) -> std::io::Result<String> {
        let path = self.0.join("tmp");
        std::fs::write(&path, content)?;
        Ok(path.to_string_lossy().into_owned())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `resolveRuntime` of `makeCodexTextGeneration`: what a managed Codex instance (signed in with ChatGPT) runs on, resolved for each
/// request (`CodexManagedProvider.ts` gives it `withAccess(runtime.resolve)`). `stop` is called when the sign-in is replaced or signed out
/// while the request runs, and when the returned guard is released; the guard keeps the sign-in admitted for as long as it lives.
pub trait ManagedRuntime: Send + Sync {
    fn resolve(&self, stop: StopSession) -> BoxFuture<'_, Result<(Effective, Box<dyn std::any::Any + Send>), t3_contracts::ProviderSetupError>>;
}

/// How a request starts the CLI.
enum Launch {
    /// `CodexInstance::launch`: the instance's own binary, environment and launch arguments.
    Local(PathBuf, HashMap<String, String>, String),
    Managed(Arc<dyn ManagedRuntime>),
}

/// Tells a stop of the sign-in that the request it ended is gone (its child killed), whichever way the request ended.
struct RunEnded(tokio::sync::watch::Sender<bool>);
impl Drop for RunEnded {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// A provider's CLI as the thing a request runs on. The instance is read from the registry on every
/// request, so a settings change takes effect on the next generation.
pub struct CliRunner {
    instances: Arc<dyn Fn(&str) -> Option<TextInstance> + Send + Sync>,
    options: Arc<dyn Fn() -> Options + Send + Sync>,
    attachments_dir: Option<PathBuf>,
}

/// A CLI that could not be started. The original reports every spawn failure alike, a missing binary
/// included (what it checked for in the failure's text never appears in the one it gets).
/// `extractJsonObject` (`packages/shared/src/schemaJson.ts`): the first balanced `{...}` of a reply that may wrap it in prose or a
/// code fence; the rest of the text if it is never closed, and the text itself if it has no brace.
pub fn extract_json_object(raw: &str) -> String {
    let trimmed = crate::text_generation::js_trim(raw);
    let Some(start) = trimmed.find('{') else { return trimmed.to_owned() };
    let (mut depth, mut in_string, mut escaping) = (0i64, false, false);
    for (offset, character) in trimmed[start..].char_indices() {
        if in_string {
            if escaping {
                escaping = false;
            } else if character == '\\' {
                escaping = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return trimmed[start..start + offset + 1].to_owned();
                }
            }
            _ => {}
        }
    }
    trimmed[start..].to_owned()
}

fn spawn_failure(operation: Operation, label: &str) -> TextGenerationError {
    TextGenerationError::new(operation, format!("Failed to spawn {label} CLI process"))
}

/// A run that ended badly: the CLI's own words, or its exit code.
fn exit_failure(operation: Operation, label: &str, finished: &Finished) -> TextGenerationError {
    let stderr = crate::text_generation::js_trim(&finished.stderr);
    let detail = if stderr.is_empty() { crate::text_generation::js_trim(&finished.stdout) } else { stderr };
    TextGenerationError::new(operation, if detail.is_empty() { format!("{label} CLI command failed with code {}.", finished.code) } else { format!("{label} CLI command failed: {detail}") })
}

impl CliRunner {
    /// Runs on the instances of a provider registry, bounded by its text generation options.
    pub fn for_registry(registry: ProviderRegistry, attachments_dir: Option<PathBuf>) -> Self {
        let options = registry.clone();
        Self { instances: Arc::new(move |id| registry.text_instance(id)), options: Arc::new(move || options.text_generation_options()), attachments_dir }
    }

    /// Runs on instances found by `instances`; tests give it the instances they want to start.
    pub fn new(instances: Arc<dyn Fn(&str) -> Option<TextInstance> + Send + Sync>, options: Options, attachments_dir: Option<PathBuf>) -> Self {
        Self { instances, options: Arc::new(move || options.clone()), attachments_dir }
    }

    async fn codex(&self, instance: CodexInstance, models: Vec<Value>, managed: Option<Arc<dyn ManagedRuntime>>, request: Request) -> Result<Value, TextGenerationError> {
        let operation = request.operation;
        let failed = |detail: &str| TextGenerationError::new(operation, detail);
        if !instance.enabled {
            return Err(failed("Codex is disabled in T3 Code settings."));
        }
        let launch = match managed {
            Some(runtime) => Launch::Managed(runtime),
            None => {
                let (binary, environment, launch_args) = instance.launch().map_err(|error| failed(&error.to_string()))?;
                Launch::Local(binary, environment, launch_args)
            }
        };
        let image_paths: Vec<String> = request
            .attachments
            .iter()
            .filter_map(|attachment| {
                let reference = crate::attachment_store::AttachmentRef::from_value(attachment).filter(|reference| reference.kind == "image")?;
                let path = crate::attachment_store::resolve_attachment_path(self.attachments_dir.as_deref()?, &reference)?;
                (path.is_absolute() && path.is_file()).then(|| path.to_string_lossy().into_owned())
            })
            .collect();
        let schema = serde_json::to_string(request.shape.schema()).expect("a schema encodes");
        let (schema_dir, output_dir) = (Scratch::new("t3code-codex-schema-"), Scratch::new("t3code-codex-output-"));
        let (Ok(schema_dir), Ok(output_dir)) = (schema_dir, output_dir) else { return Err(failed("Failed to write temp file")) };
        let (Ok(schema_path), Ok(output_path)) = (schema_dir.file(&schema), output_dir.file("")) else { return Err(failed("Failed to write temp file")) };
        let model = codex_model(request.model_selection["model"].as_str().unwrap_or(""), &models);
        let effort = string_option(&request.model_selection, "reasoningEffort").unwrap_or_else(|| crate::text_generation::data()["defaults"]["reasoningEffort"].as_str().unwrap_or("low")).to_string();
        // A sign-in that is replaced or ended while this runs stops it (`withAccess`); a stop waits until the run is gone.
        let (ended, ended_signal) = tokio::sync::watch::channel(false);
        let _ended = RunEnded(ended);
        let stopped = Arc::new(tokio::sync::Notify::new());
        let run = async {
            let (binary, environment, launch_args, tier, access) = match launch {
                Launch::Local(binary, environment, launch_args) => (binary, environment, launch_args, codex_service_tier(&request.model_selection), None),
                Launch::Managed(runtime) => {
                    let stop: StopSession = {
                        let (stopped, ended) = (stopped.clone(), ended_signal.clone());
                        Arc::new(move || {
                            stopped.notify_one();
                            let mut ended = ended.clone();
                            Box::pin(async move {
                                let _ = ended.wait_for(|ended| *ended).await;
                            })
                        })
                    };
                    let (effective, access) = runtime.resolve(stop).await.map_err(|error| failed(&error.detail))?;
                    // ChatGPT token sharing takes no service tier.
                    (effective.binary, effective.environment, effective.launch_args, None, Some(access))
                }
            };
            let binary = if binary.as_os_str().is_empty() { PathBuf::from("codex") } else { binary };
            let args = codex_args(&CodexRun { launch_args: &launch_args, model: &model, reasoning_effort: &effort, service_tier: tier.as_deref(), schema_path: &schema_path, output_path: &output_path, image_paths: &image_paths });
            let finished = spawn(&binary, &args, &environment, Path::new(&request.cwd), request.prompt.clone()).await.map_err(|_| spawn_failure(operation, "Codex"))?;
            Ok::<_, TextGenerationError>((finished, access))
        };
        let outcome = tokio::select! {
            biased;
            _ = stopped.notified() => return Err(failed("Codex text generation failed.")),
            outcome = tokio::time::timeout(self.timeout(), run) => outcome,
        };
        // The sign-in stays admitted until the answer is read.
        let (finished, _access) = match outcome {
            Err(_) => return Err(failed("Codex CLI request timed out.")),
            Ok(Err(error)) => return Err(error),
            Ok(Ok(run)) => run,
        };
        if finished.code != 0 {
            return Err(exit_failure(operation, "Codex", &finished));
        }
        let content = std::fs::read_to_string(&output_path).map_err(|_| failed("Failed to read Codex output file."))?;
        serde_json::from_str::<Value>(&content).ok().and_then(|answer| request.shape.decode(&answer)).ok_or_else(|| failed("Codex returned invalid structured output."))
    }

    async fn claude(&self, instance: ClaudeInstance, request: Request) -> Result<Value, TextGenerationError> {
        let operation = request.operation;
        let failed = |detail: &str| TextGenerationError::new(operation, detail);
        if !instance.enabled {
            return Err(failed("Claude is disabled in T3 Code settings."));
        }
        let binary = instance.binary_path();
        let binary = if binary.as_os_str().is_empty() { PathBuf::from("claude") } else { binary };
        let args = claude_args(request.shape, &request.model_selection);
        // Titles need only the supplied prompt, not configuration from the checkout.
        let title_dir = if operation == Operation::ThreadTitle { Some(Scratch::new("t3code-claude-title-").map_err(|_| failed("Failed to create title directory"))?) } else { None };
        let cwd = title_dir.as_ref().map(|directory| directory.0.clone()).unwrap_or_else(|| PathBuf::from(&request.cwd));
        let environment = instance.process_environment();
        let run = spawn(&binary, &args, &environment, &cwd, request.prompt.clone());
        let finished = match tokio::time::timeout(self.timeout(), run).await {
            Err(_) => return Err(failed("Claude CLI request timed out.")),
            Ok(Err(_)) => return Err(spawn_failure(operation, "Claude")),
            Ok(Ok(finished)) => finished,
        };
        if finished.code != 0 {
            return Err(exit_failure(operation, "Claude", &finished));
        }
        read_claude_answer(&finished.stdout, request.shape).map_err(|failure| match failure {
            ClaudeReadFailure::Format => failed("Claude CLI returned unexpected output format."),
            ClaudeReadFailure::Invalid => failed("Claude returned invalid structured output."),
        })
    }

    /// `makePiTextGeneration`: an ephemeral `pi --mode rpc --no-session` with extensions and tools off (no user is present to answer
    /// a dialog, and background text must never change the workspace). The user's model configuration and sign-in still apply.
    async fn pi(&self, instance: crate::pi_instance::PiInstance, request: Request) -> Result<Value, TextGenerationError> {
        use crate::{pi_launch::{self, LaunchOptions}, pi_protocol::{DEFAULT_REQUEST_TIMEOUT, Event, Launch, PiProcess}};
        let operation = request.operation;
        let failed = |detail: &str| TextGenerationError::new(operation, detail);
        let launch_args = pi_launch::resolve_launch_args(instance.config.launch_args.as_str()).map_err(|message| failed(&message))?;
        let launch = pi_launch::build_rpc_launch(&LaunchOptions { launch_args: &launch_args, mcp: None, extension_path: None, ephemeral: true, runtime_mode: None, disable_extensions: true, disable_tools: true });
        let mut env = instance.process_environment();
        env.extend(launch.env);
        let (process, mut events) = PiProcess::spawn(&Launch { binary: instance.binary_path(), args: launch.args, cwd: PathBuf::from(&request.cwd), env, remove_env: launch.remove_env })
            .map_err(|_| failed("Pi text generation failed."))?;
        let run = async {
            let model = request.model_selection["model"].as_str().unwrap_or("");
            if model != "default" {
                // `customModels` accepts arbitrary strings, so an unusable slug is rejected rather than skipped: running Pi's own default
                // here would report success for a model the caller never asked for.
                let Some((provider, model_id)) = crate::pi_commands::parse_model_slug(model) else {
                    return Err(failed(&format!("Pi model '{model}' must use provider/model format.")));
                };
                process.request(json!({"type":"set_model","provider":provider,"modelId":model_id}), DEFAULT_REQUEST_TIMEOUT).await.map_err(|_| failed("Pi text generation failed."))?;
            }
            process.request(json!({"type":"prompt","message":request.prompt}), DEFAULT_REQUEST_TIMEOUT).await.map_err(|_| failed("Pi text generation failed."))?;
            loop {
                match events.recv().await {
                    Some(Event::Record(record)) if record["type"] == "agent_settled" => break,
                    Some(Event::Record(_)) => {}
                    Some(Event::Closed(_)) | None => return Err(failed("Pi text generation failed.")),
                }
            }
            let data = process.request(json!({"type":"get_last_assistant_text"}), DEFAULT_REQUEST_TIMEOUT).await.map_err(|_| failed("Pi text generation failed."))?;
            let text = data["text"].as_str().map(crate::text_generation::js_trim).unwrap_or_default();
            if text.is_empty() {
                return Err(failed("Pi returned empty output."));
            }
            serde_json::from_str::<Value>(&extract_json_object(&text)).ok().and_then(|answer| request.shape.decode(&answer)).ok_or_else(|| failed("Pi returned invalid structured output."))
        };
        let outcome = tokio::time::timeout(self.timeout(), run).await;
        process.terminate().await;
        outcome.unwrap_or_else(|_| Err(failed("Pi request timed out.")))
    }

    /// `makeOpenCodeTextGeneration` / `OpenCode2TextGeneration.make`, by the generation of the instance's server.
    async fn opencode(&self, instance: crate::opencode_instance::OpenCodeInstance, clock: Arc<dyn crate::scheduled_tasks::SchedulerClock>, request: Request) -> Result<Value, TextGenerationError> {
        let run = crate::text_generation_opencode::Run { instance: &instance, clock, attachments_dir: self.attachments_dir.as_deref(), timeout: self.timeout() };
        run.text(&request).await
    }

    /// `makeAntigravityTextGeneration`.
    async fn antigravity(&self, instance: crate::antigravity_instance::AntigravityInstance, request: Request) -> Result<Value, TextGenerationError> {
        crate::text_generation_antigravity::run(&instance, &request, self.timeout()).await
    }

    fn timeout(&self) -> Duration {
        (self.options)().timeout
    }
}

impl Runner for CliRunner {
    fn run<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Value, TextGenerationError>> {
        Box::pin(async move {
            let id = request.model_selection["instanceId"].as_str().unwrap_or("").to_string();
            match (self.instances)(&id) {
                None => Err(TextGenerationError::new(request.operation, format!("No provider instance registered for id '{id}'."))),
                Some(TextInstance::Codex(instance, models)) => Box::pin(self.codex(instance, models, None, request)).await,
                Some(TextInstance::ManagedCodex(instance, models, runtime)) => Box::pin(self.codex(instance, models, Some(runtime), request)).await,
                Some(TextInstance::Claude(instance)) => Box::pin(self.claude(instance, request)).await,
                Some(TextInstance::Pi(instance)) => Box::pin(self.pi(instance, request)).await,
                Some(TextInstance::OpenCode(instance, clock)) => Box::pin(self.opencode(instance, clock, request)).await,
                Some(TextInstance::Antigravity(instance)) => Box::pin(self.antigravity(instance, request)).await,
                Some(TextInstance::Other(driver)) => Err(TextGenerationError::new(request.operation, format!("Provider driver '{driver}' cannot write text in this server."))),
            }
        })
    }
}
