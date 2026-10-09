//! `git.resolvePullRequest` and `git.preparePullRequestThread`. Each handler decodes its
//! payload, calls one workflow function (boxed), and maps the failure to its wire `_tag`.
use crate::{
    git_workflow,
    git_workflow_followup::{link_created_pull_request, refresh_pushed_pull_requests},
    git_workflow_stacked::{StackedInput, TextGeneration, run_stacked_action},
    text_generation_service::{NoProviders, TextGenerationService},
    pull_request_rpc,
    transport::ApiState,
    vcs_rpc,
    vcs_service::VcsService,
};
use std::sync::{Arc, OnceLock};
use serde::Deserialize;
use serde_json::{Value, json};

pub const METHODS: [&str; 2] = ["git.resolvePullRequest", "git.preparePullRequestThread"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefInput {
    cwd: String,
    reference: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareInput {
    cwd: String,
    reference: String,
    mode: Mode,
    #[serde(default)]
    #[allow(dead_code)]
    thread_id: Option<String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Local,
    Worktree,
}

fn decode_failure(message: impl ToString) -> Value {
    json!({"_tag":"SchemaDecodeError","message":message.to_string()})
}

/// A trimmed non-empty string, the way the contract's `TrimmedNonEmptyString` reads one.
fn trimmed(value: &str, field: &str) -> Result<String, Value> {
    let text = t3_contracts::trim_wire_string(value);
    if text.is_empty() { Err(decode_failure(format!("Expected a non-empty string for {field}."))) } else { Ok(text.to_string()) }
}

/// The working directory and the reference of a `git.resolvePullRequest` payload, trimmed.
fn decode_reference(payload: Value) -> Result<(String, String), Value> {
    let input: RefInput = serde_json::from_value(payload).map_err(decode_failure)?;
    Ok((trimmed(&input.cwd, "cwd")?, trimmed(&input.reference, "reference")?))
}
pub(crate) fn decode_ref(payload: Value) -> Result<(), Value> {
    decode_reference(payload).map(|_| ())
}
pub(crate) fn decode_prepare(payload: Value) -> Result<(), Value> {
    let input: PrepareInput = serde_json::from_value(payload).map_err(decode_failure)?;
    trimmed(&input.cwd, "cwd")?;
    trimmed(&input.reference, "reference")?;
    Ok(())
}

pub async fn command(state: &ApiState, tag: &str, payload: Value) -> Result<Value, Value> {
    let env = pull_request_rpc::env(state);
    match tag {
        "git.resolvePullRequest" => {
            let (cwd, reference) = decode_reference(payload)?;
            Box::pin(git_workflow::resolve_pull_request(&env, &cwd, &reference)).await.map_err(|error| error.wire())
        }
        "git.preparePullRequestThread" => {
            let input: PrepareInput = serde_json::from_value(payload).map_err(decode_failure)?;
            let (cwd, reference) = (trimmed(&input.cwd, "cwd")?, trimmed(&input.reference, "reference")?);
            let context = vcs_rpc::context(state);
            let mode = if input.mode == Mode::Local { "local" } else { "worktree" };
            let result = Box::pin(git_workflow::prepare_pull_request_thread(&env, &context, &cwd, &reference, mode)).await;
            // The status of the checkout moved, whatever the outcome.
            let (status, hooks, refresh_cwd) = (VcsService::shared().status().clone(), context.hooks(), cwd.clone());
            tokio::spawn(async move {
                if let Err(error) = status.refresh_status(&refresh_cwd, &hooks).await {
                    tracing::warn!(cwd=%refresh_cwd,error=%error,"VCS status refresh after preparing a pull request thread failed");
                }
            });
            result.map_err(|error| error.wire())
        }
        _ => Err(json!({"_tag":"NativeMethodUnsupportedError","method":tag})),
    }
}

// ---------------------------------------------------------------------------------------------
// git.runStackedAction
// ---------------------------------------------------------------------------------------------

static TEXT_GENERATION: OnceLock<Arc<dyn TextGeneration>> = OnceLock::new();

/// Installs the process's text generation, once. A server that never installs one writes with the
/// CLIs of its provider registry, or fails the parts of a stacked action that need a model when it
/// has none.
pub fn install_text_generation(text: Arc<dyn TextGeneration>) -> bool {
    TEXT_GENERATION.set(text).is_ok()
}

fn text_generation(state: &ApiState) -> Arc<dyn TextGeneration> {
    TEXT_GENERATION.get().cloned().unwrap_or_else(|| match &state.providers {
        Some(providers) => Arc::new(TextGenerationService::new(providers.clone(), state.store.clone())),
        None => Arc::new(NoProviders),
    })
}

/// A trimmed non-empty string within a bound, the way the contract reads one.
fn bounded(value: &str, field: &str, max: usize) -> Result<String, Value> {
    let text = trimmed(value, field)?;
    if text.encode_utf16().count() > max {
        return Err(decode_failure(format!("Expected at most {max} characters for {field}.")));
    }
    Ok(text)
}

/// `GitRunStackedActionInput`, decoded and bounded before anything starts.
pub fn decode_stacked_input(payload: Value) -> Result<StackedInput, Value> {
    let mut input: StackedInput = serde_json::from_value(payload).map_err(decode_failure)?;
    input.action_id = trimmed(&input.action_id, "actionId")?;
    input.cwd = trimmed(&input.cwd, "cwd")?;
    if let Some(message) = &input.commit_message {
        input.commit_message = Some(bounded(message, "commitMessage", 10_000)?);
    }
    if let Some(paths) = &input.file_paths {
        if paths.is_empty() {
            return Err(decode_failure("Expected at least one path for filePaths."));
        }
        input.file_paths = Some(paths.iter().map(|path| trimmed(path, "filePaths")).collect::<Result<_, _>>()?);
    }
    Ok(input)
}

/// A running stacked action: the events as they happen, then the outcome. Dropping it (the client
/// left, or interrupted the call) stops the action where it stands, as interrupting the
/// original's stream does.
pub struct StackedRun {
    events: tokio::sync::mpsc::UnboundedReceiver<Value>,
    task: Option<tokio::task::JoinHandle<Result<(), Value>>>,
}

impl StackedRun {
    pub async fn next(&mut self) -> Option<Value> {
        self.events.recv().await
    }

    /// The outcome, once the events are drained: a failure is the wire error.
    pub async fn finish(mut self) -> Result<(), Value> {
        match self.task.take() {
            Some(task) => task.await.unwrap_or_else(|_| Err(json!({"_tag":"NativeServiceError","method":"git.runStackedAction","message":"The action stopped unexpectedly."}))),
            None => Ok(()),
        }
    }
}

impl Drop for StackedRun {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub fn open_stacked_action(state: &ApiState, payload: Value) -> Result<StackedRun, Value> {
    let input = decode_stacked_input(payload)?;
    let env = pull_request_rpc::env(state);
    let context = vcs_rpc::context(state);
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    let text = text_generation(state);
    let task = tokio::spawn(async move {
        match Box::pin(run_stacked_action(&env, text.as_ref(), &input, events)).await {
            Ok(result) => {
                if let Some(thread_id) = &input.thread_id {
                    Box::pin(link_created_pull_request(&env, thread_id, &result)).await;
                }
                Box::pin(refresh_pushed_pull_requests(&env, &input, &result)).await;
                let (status, hooks) = (VcsService::shared().status().clone(), context.hooks());
                if let Err(error) = status.refresh_status(&input.cwd, &hooks).await {
                    tracing::warn!(cwd=%input.cwd,error=%error,"VCS status refresh after a stacked action failed");
                }
                Ok(())
            }
            Err(error) => Err(error.wire()),
        }
    });
    Ok(StackedRun { events: receiver, task: Some(task) })
}
