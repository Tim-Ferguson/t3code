//! Prepares the workspace of a launched thread (port of `prepareInBackground` in
//! `ThreadLaunchService.ts`): creates the git worktree a launch asked for, records it on the
//! thread, runs the project's setup script, then releases the waiting run.
//!
//! This is the `workspace.prepare` outbox effect. The launch commit only records the intent: a
//! thread, and a run that waits in `preparing`. Everything with side effects happens here, after
//! that commit, and reports back through commands: `prepared-run.progress`, `.release` and
//! `.fail` for the run, `thread.metadata.update` for the recorded worktree.
//!
//! Progress is shown to clients through `WorktreeSetupTracker`, which is memory only. A server
//! that stops mid-preparation is handled like the original's interrupted preparation fiber: the
//! next start claims the effect again (`codex_runtime::recover` keeps it pending) and `execute`
//! abandons it as cancelled, removing the worktree the lost attempt claimed (recorded on the
//! effect, see `Context::note`) and failing the run with a retry offered.
use crate::{
    persistence::{Effect, Store, StoreError, read_projection},
    project_setup_script::{self, CompletionShell},
    provider_registry::ProviderRegistry,
    terminal_manager::TerminalManager,
    thread,
    worktree_setup_tracker::{StagePatch, WorktreeSetupTracker},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use t3_contracts::{WorktreeSetupPhase, WorktreeSetupStageId as Stage, WorktreeSetupStageStatus as Status};
use t3_vcs::core::{CreateWorktreeProgress, VcsCreateWorktreeInput};
use tokio::sync::watch;

/// The `t3/<hex>` namespace of temporary branches (`WORKTREE_BRANCH_PREFIX`).
const BRANCH_PREFIX: &str = "t3";

fn temporary_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        // Canonical `t3/<8 hex>`, its `t3-<8 hex>` fallback, and the legacy shapes that stay
        // eligible for branch regeneration. Nothing looser than what was generated.
        regex::Regex::new(&format!(
            "^(?:{BRANCH_PREFIX}[-/][0-9a-f]{{8}}|t3code(?:[-/][0-9a-f]{{8}}|/[0-9a-f]{{8}}-[0-9a-f]{{4}}-4[0-9a-f]{{3}}-[89ab][0-9a-f]{{3}}-[0-9a-f]{{12}}))$"
        ))
        .unwrap()
    })
}

/// `buildTemporaryWorktreeBranchName`: exactly eight lowercase hex characters of `token`.
pub(crate) fn temporary_branch_name(token: &str) -> String {
    let hex: String = token
        .to_lowercase()
        .chars()
        .filter(|c| matches!(c, '0'..='9' | 'a'..='f'))
        .take(8)
        .collect();
    format!("{BRANCH_PREFIX}/{hex}")
}

pub(crate) fn is_temporary_branch(name: &str) -> bool {
    temporary_pattern().is_match(&name.trim().to_lowercase())
}

/// `flattenTemporaryWorktreeBranchName`: git stores refs as paths, so a plain `t3` branch
/// makes every `t3/<hex>` ref impossible. This moves a temporary name to the flat `t3-<hex>`.
pub(crate) fn flatten_temporary_branch_name(name: &str) -> String {
    let normalized = name.trim().to_lowercase();
    let start = normalized.find(['-', '/']).map_or(0, |index| index + 1);
    let token: String = normalized.chars().skip(start).take(8).collect();
    format!("{BRANCH_PREFIX}-{token}")
}

/// Why a preparation failed: the step, and git's or the service's own words.
#[derive(Debug, Clone)]
pub(crate) struct PrepError {
    operation: &'static str,
    detail: String,
}
impl PrepError {
    fn new(operation: &'static str, detail: impl Into<String>) -> Self {
        Self { operation, detail: detail.into() }
    }
    /// `failureDetail`.
    fn describe(&self) -> String {
        format!("Workspace preparation failed during {}: {}", self.operation.replace('-', " "), self.detail)
    }
}

enum Stop {
    Failed(PrepError),
    Cancelled,
    /// The thread was deleted meanwhile. Its deletion already cancelled the run and queued the
    /// cleanup of its terminals; the worktree stays, as it does for any deleted thread.
    Deleted,
}

/// What the server shares with preparations: the progress tracker and the terminal service
/// setup scripts run in.
pub struct WorkspaceRuntime {
    tracker: WorktreeSetupTracker,
    terminals: Mutex<Option<TerminalManager>>,
    shell: Mutex<Option<CompletionShell>>,
    mcp: Mutex<Option<crate::provider_mcp::ProviderMcpSessions>>,
}
impl WorkspaceRuntime {
    pub(crate) fn new() -> Self {
        Self { tracker: WorktreeSetupTracker::default(), terminals: Mutex::new(None), shell: Mutex::new(None), mcp: Mutex::new(None) }
    }
    /// The credentials a thread's cleanup revokes once the thread is archived or deleted.
    pub fn attach_mcp(&self, sessions: crate::provider_mcp::ProviderMcpSessions) {
        self.mcp.lock().unwrap().get_or_insert(sessions);
    }
    pub(crate) fn mcp(&self) -> Option<crate::provider_mcp::ProviderMcpSessions> {
        self.mcp.lock().unwrap().clone()
    }
    pub fn tracker(&self) -> WorktreeSetupTracker {
        self.tracker.clone()
    }
    /// Setup scripts run in these terminals. Without them a project that has a setup script
    /// cannot launch a prepared thread.
    pub fn attach_terminals(&self, terminals: TerminalManager) {
        self.terminals.lock().unwrap().get_or_insert(terminals);
    }
    pub(crate) fn terminals(&self) -> Option<TerminalManager> {
        self.terminals.lock().unwrap().clone()
    }
    pub fn has_terminals(&self) -> bool {
        self.terminals.lock().unwrap().is_some()
    }
    /// Overrides the shell the completion wrapper is written for. By default it follows
    /// `$SHELL`, as the terminal service does.
    pub fn set_completion_shell(&self, shell: &str) {
        *self.shell.lock().unwrap() = Some(project_setup_script::resolve_completion_shell(false, shell));
    }
    fn completion_shell(&self) -> CompletionShell {
        self.shell.lock().unwrap().unwrap_or_else(|| {
            project_setup_script::resolve_completion_shell(cfg!(windows), &std::env::var("SHELL").unwrap_or_default())
        })
    }
}

/// What the cleanup after a failure needs to know.
#[derive(Default)]
struct Shared {
    /// The directory `git worktree add` created, as soon as it exists.
    created: Mutex<Option<String>>,
    /// The terminal the setup script was typed into.
    terminal: Mutex<Option<String>>,
    /// The thread records the workspace.
    recorded: AtomicBool,
}

struct Context {
    store: Store,
    runtime: Arc<WorkspaceRuntime>,
    settings: t3_contracts::ServerSettings,
    providers: ProviderRegistry,
    thread_id: String,
    project_id: t3_contracts::ProjectId,
    project: Value,
    /// The outbox effect this preparation runs as; it carries what a lost attempt created.
    effect_id: String,
    /// Prefix of every command id this preparation issues.
    base: String,
    run_id: Option<String>,
    strategy: Value,
    /// A retry reuses the worktree its failed attempt created and recorded.
    reused: Option<String>,
    tracked: bool,
}

fn ok<T, E: std::fmt::Display>(result: Result<T, E>, operation: &'static str) -> Result<T, PrepError> {
    result.map_err(|error| PrepError::new(operation, error.to_string()))
}

impl Context {
    fn root(&self) -> &str {
        self.project["workspaceRoot"].as_str().unwrap_or("")
    }

    /// Records on the effect something the preparation created, so that a server that dies
    /// before cleaning up can still do it on the next start.
    fn note(&self, key: &str, value: &str) {
        note_on_effect(&self.store, &self.effect_id, key, value);
    }

    /// Dispatches a prepared-run command, or a thread metadata update, under the derived id.
    fn command(&self, suffix: &str, mut command: Value, operation: &'static str) -> Result<(), PrepError> {
        command["commandId"] = json!(format!("{}:{suffix}", self.base));
        command["threadId"] = json!(self.thread_id);
        let kind = command["type"].as_str().unwrap_or("").to_owned();
        let receipt = if kind == "thread.metadata.update" {
            thread::ThreadService::new(self.store.clone()).dispatch(&command, Utc::now())
        } else {
            dispatch_prepared(&self.store, &command, Utc::now())
        };
        let receipt = ok(receipt, operation)?;
        if receipt.status == "rejected" {
            return Err(PrepError::new(operation, receipt.error.unwrap_or(Value::Null).to_string()));
        }
        Ok(())
    }

    fn progress(&self, phase: &str) -> Result<(), PrepError> {
        let Some(run_id) = &self.run_id else { return Ok(()) };
        self.command(
            &format!("progress:{phase}"),
            json!({"type":"prepared-run.progress","runId":run_id,"phase":phase}),
            "update-thread",
        )
    }

    /// Creates the worktree of a `worktree` launch and returns its path and branch.
    async fn provision(&self, shared: &Shared, mut branch: String) -> Result<(String, String), PrepError> {
        let tracker = &self.runtime.tracker;
        let thread = self.thread_id.as_str();
        let base_ref = self.strategy["baseRef"].as_str().unwrap_or("").to_owned();
        self.progress("worktree")?;
        let git = t3_vcs_core();
        let cwd = PathBuf::from(self.root());
        let mut start_ref = base_ref.clone();
        // "Start from origin" is a stored default; repos without the requested remote branch
        // fall back to the local base branch.
        let start_from_origin = self.strategy["startFromOrigin"] == true
            && ok(git.remote_exists(&cwd, "origin").await, "provision-worktree")?;
        tracker.stage_status(thread, Stage::Fetch, if start_from_origin { Status::Running } else { Status::Skipped }, None);
        if start_from_origin {
            ok(git.fetch_remote(&cwd, "origin", Some(&base_ref)).await, "provision-worktree")?;
            if ok(git.remote_branch_exists(&cwd, "origin", &base_ref).await, "provision-worktree")? {
                start_ref = ok(git.resolve_remote_tracking_commit(&cwd, &base_ref, "origin").await, "provision-worktree")?.0;
            }
            tracker.stage_status(thread, Stage::Fetch, Status::Done, None);
        }
        // `hasCommit`: a plain `t3` branch blocks the whole `t3/` namespace.
        if is_temporary_branch(&branch) && git.resolve_commit(&cwd, &format!("refs/heads/{BRANCH_PREFIX}")).await.is_ok() {
            branch = flatten_temporary_branch_name(&branch);
        }
        tracker.stage_status(thread, Stage::Checkout, Status::Running, None);
        let (claimed, progress_tracker, progress_thread) = (Arc::new(Mutex::new(None::<String>)), tracker.clone(), thread.to_owned());
        let claimed_path = claimed.clone();
        let (claim_store, claim_effect) = (self.store.clone(), self.effect_id.clone());
        let progress = CreateWorktreeProgress {
            on_worktree_claimed: Some(Arc::new(move |path: &Path| {
                let path = path.to_string_lossy().into_owned();
                note_on_effect(&claim_store, &claim_effect, CLAIMED_WORKTREE, &path);
                *claimed_path.lock().unwrap() = Some(path);
            })),
            on_checkout_progress: Some(Arc::new(move |progress| {
                progress_tracker.stage(
                    &progress_thread,
                    Stage::Checkout,
                    StagePatch { percent: Some(Some(progress.percent.min(100) as u8)), ..StagePatch::default() },
                );
            })),
            ..CreateWorktreeProgress::default()
        };
        let created = crate::vcs_worktree::create_for_launch(
            &git,
            &self.settings,
            Some(&self.project_id),
            self.store.state_dir(),
            &VcsCreateWorktreeInput {
                cwd: cwd.clone(),
                ref_name: start_ref,
                new_ref_name: Some(branch.clone()),
                base_ref_name: Some(base_ref),
                path: None,
            },
            Some(progress),
        )
        .await;
        // A checkout that dies after claiming its directory leaves it behind; record it first
        // so the failure path removes it.
        if let Some(path) = claimed.lock().unwrap().clone() {
            *shared.created.lock().unwrap() = Some(path);
        }
        let worktree = ok(created, "provision-worktree")?;
        let path = worktree.path.to_string_lossy().into_owned();
        *shared.created.lock().unwrap() = Some(path.clone());
        tracker.set_worktree(thread, &path, &worktree.ref_name);
        tracker.stage_status(thread, Stage::Checkout, Status::Done, None);
        Ok((path, worktree.ref_name))
    }

    /// The whole preparation up to releasing the run.
    async fn prepare(&self, shared: &Shared) -> Result<(), PrepError> {
        let tracker = &self.runtime.tracker;
        let thread = self.thread_id.as_str();
        let kind = self.strategy["type"].as_str().unwrap_or("root");
        let requested = self.strategy["branch"].as_str().map(str::to_owned);
        let mut branch = requested.clone();
        if kind == "worktree" && requested.is_none() {
            branch = Some(temporary_branch_name(&uuid::Uuid::new_v4().simple().to_string()));
        }
        let mut worktree_path = (kind == "existing_worktree").then(|| self.strategy["worktreePath"].as_str().map(str::to_owned)).flatten();
        if kind == "worktree" {
            let (path, name) = self.provision(shared, branch.take().unwrap()).await?;
            worktree_path = Some(path);
            branch = Some(name);
        }
        // A reused worktree is already recorded. Root and existing-worktree launches recorded
        // theirs when the thread was created.
        if kind == "worktree" && self.reused.is_none() {
            self.command(
                "workspace",
                json!({"type":"thread.metadata.update","branch":branch,"worktreePath":worktree_path}),
                "update-thread",
            )?;
        }
        shared.recorded.store(true, Ordering::Release);
        // A temporary `t3/<hash>` name is replaced by a generated one in the background, so
        // generation latency never delays the checkout or the provider turn. The temporary name
        // simply sticks if generation or the rename fails.
        if kind == "worktree" && self.reused.is_none() {
            if let (Some(path), Some(name)) = (&worktree_path, &branch) {
                if is_temporary_branch(name) {
                    self.rename_in_background(path.clone(), name.clone());
                }
            }
        }
        let cwd = worktree_path.clone().unwrap_or_else(|| self.root().to_owned());

        self.progress("setup")?;
        tracker.stage_status(thread, Stage::SetupScript, Status::Running, None);
        let scripts = ok(crate::launch::resolved_scripts(&self.settings, &self.project_id, &self.project), "run-setup-script")?;
        let mut await_async = None;
        if let Some(script) = project_setup_script::setup_script(&scripts) {
            let terminals = self.runtime.terminals.lock().unwrap().clone();
            let terminals = terminals.ok_or_else(|| PrepError::new("run-setup-script", "The terminal service is unavailable, so the setup script cannot run."))?;
            let observe: Option<Arc<dyn Fn(String) + Send + Sync>> = self.tracked.then(|| {
                let (tracker, thread) = (tracker.clone(), thread.to_owned());
                Arc::new(move |line: String| tracker.append_tail(&thread, Stage::SetupScript, &line)) as Arc<dyn Fn(String) + Send + Sync>
            });
            let started = Box::pin(project_setup_script::run(
                &terminals,
                project_setup_script::Input {
                    thread_id: thread,
                    project_root: self.root(),
                    cwd: &cwd,
                    script,
                    shell: self.runtime.completion_shell(),
                    observe,
                },
            ))
            .await
            .map_err(|(step, detail)| PrepError::new("run-setup-script", format!("{step}: {detail}")))?;
            *shared.terminal.lock().unwrap() = Some(started.terminal_id.clone());
            self.note(SETUP_TERMINAL, &started.terminal_id);
            tracker.set_setup_script(thread, &started.name, &started.command, &started.terminal_id);
            match started.completion {
                Some(completion) => {
                    let (tracker_owned, thread_owned) = (tracker.clone(), thread.to_owned());
                    let wait = async move {
                        let exit = completion.await.unwrap_or(None);
                        let detail = format!("exited with {}", exit.map_or("no exit code".to_owned(), |code| code.to_string()));
                        tracker_owned.stage(
                            &thread_owned,
                            Stage::SetupScript,
                            StagePatch {
                                status: Some(if exit == Some(0) { Status::Done } else { Status::Failed }),
                                detail: Some(Some(detail)),
                                ..StagePatch::default()
                            },
                        );
                        exit
                    };
                    if started.async_ {
                        await_async = Some(Box::pin(wait) as std::pin::Pin<Box<dyn std::future::Future<Output = Option<i32>> + Send>>);
                    } else {
                        let exit = Box::pin(wait).await;
                        if exit != Some(0) {
                            return Err(PrepError::new(
                                "run-setup-script",
                                format!("Setup script exited with {}.", exit.map_or("no exit code".to_owned(), |code| code.to_string())),
                            ));
                        }
                    }
                }
                None => tracker.stage_status(thread, Stage::SetupScript, Status::Done, None),
            }
        } else {
            tracker.stage_status(thread, Stage::SetupScript, Status::Skipped, None);
        }

        tracker.mark_uncancellable(thread);
        tracker.stage_status(thread, Stage::Agent, Status::Running, None);
        if let Some(run_id) = &self.run_id {
            self.command("release", json!({"type":"prepared-run.release","runId":run_id}), "release-run")?;
        }
        tracker.stage_status(thread, Stage::Agent, Status::Done, None);
        // The agent starts now; an asynchronous script keeps running beside it.
        match await_async {
            Some(wait) => {
                let (tracker, thread) = (tracker.clone(), thread.to_owned());
                tokio::spawn(async move {
                    wait.await;
                    tracker.finish(&thread, WorktreeSetupPhase::Done, None);
                });
            }
            None => {
                tracker.finish(thread, WorktreeSetupPhase::Done, None);
            }
        }
        Ok(())
    }

    fn rename_in_background(&self, worktree: String, old_branch: String) {
        let Some(run_id) = self.run_id.clone() else { return };
        let projection = self.store.projection("thread", &self.thread_id).ok().flatten();
        let message = projection.as_ref().and_then(|view| {
            let run = view["runs"].as_array()?.iter().find(|run| run["id"] == run_id.as_str())?;
            view["messages"].as_array()?.iter().find(|message| message["id"] == run["userMessageId"]).cloned()
        });
        let Some(message) = message else { return };
        let (store, providers, base, thread_id, project_id) = (self.store.clone(), self.providers.clone(), self.base.clone(), self.thread_id.clone(), self.project_id.to_string());
        tokio::spawn(async move {
            let generation = crate::text_generation_service::TextGenerationService::new(providers, store.clone());
            let attachments = message["attachments"].as_array().cloned().unwrap_or_default();
            let renamed = async {
                let name = generation.branch_name(&project_id, &worktree, message["text"].as_str().unwrap_or(""), &attachments).await.map_err(|error| error.to_string())?;
                let branch = t3_vcs_core().rename_branch(Path::new(&worktree), &old_branch, &name.branch, name.exact_name).await.map_err(|error| error.to_string())?;
                let command = json!({"type":"thread.metadata.update","commandId":format!("{base}:branch-rename"),"threadId":thread_id,"branch":branch,"worktreePath":worktree});
                thread::ThreadService::new(store).dispatch(&command, Utc::now()).map_err(|error| error.to_string())?;
                Ok::<(), String>(())
            };
            if let Err(error) = renamed.await {
                tracing::warn!(thread_id = %thread_id, %old_branch, %error, "Thread worktree branch rename failed");
            }
        });
    }

    /// The failure path: settle the card, remove what was never recorded, and fail the run.
    async fn abandon(&self, shared: &Shared, stop: Stop) {
        let tracker = &self.runtime.tracker;
        let thread = self.thread_id.as_str();
        if matches!(stop, Stop::Deleted) {
            tracker.finish(thread, WorktreeSetupPhase::Cancelled, None);
            return;
        }
        let cancelled = matches!(stop, Stop::Cancelled);
        let detail = match &stop {
            Stop::Failed(error) => error.describe(),
            _ => "Workspace preparation failed: Worktree setup cancelled.".to_owned(),
        };
        tracker.finish(
            thread,
            if cancelled { WorktreeSetupPhase::Cancelled } else { WorktreeSetupPhase::Failed },
            (!cancelled).then_some(detail.as_str()),
        );
        // A cancelled setup leaves nothing behind. A failed one keeps a worktree the thread
        // recorded, so a retry reuses it, and removes one it never recorded, which a retry
        // would otherwise duplicate.
        let created = shared.created.lock().unwrap().clone();
        if let Some(path) = created.filter(|_| self.tracked && (cancelled || !shared.recorded.load(Ordering::Acquire))) {
            let terminal = shared.terminal.lock().unwrap().clone();
            let terminals = self.runtime.terminals.lock().unwrap().clone();
            if let (Some(terminal), Some(terminals)) = (terminal, terminals) {
                if let Ok(close) = serde_json::from_value::<t3_contracts::TerminalCloseInput>(
                    json!({"threadId":self.thread_id,"terminalId":terminal,"deleteHistory":true}),
                ) {
                    let _ = terminals.close(close).await;
                }
            }
            // The thread forgets the worktree only once it is gone; a failed removal leaves
            // the directory for the user to clean up rather than reusing a checkout that may
            // be half written.
            match t3_vcs_core().remove_worktree(Path::new(self.root()), Path::new(&path), true).await {
                Ok(()) => {
                    let _ = self.command(
                        "cancel-workspace",
                        json!({"type":"thread.metadata.update","worktreePath":null,"branch":null}),
                        "update-thread",
                    );
                }
                Err(error) => tracing::warn!(thread_id = thread, %path, %error, "Failed to remove an abandoned thread worktree"),
            }
        }
        if let Some(run_id) = &self.run_id {
            let failure = crate::provider_failure::make(&json!({"message":detail,"class":"validation_error","retryable":false}));
            let failed = self.command("fail", json!({"type":"prepared-run.fail","runId":run_id,"failure":failure}), "update-thread");
            match failed {
                Ok(()) => {
                    let _ = crate::codex_runtime::settle_queue(&self.store, thread, Some(run_id));
                }
                Err(error) => tracing::warn!(thread_id = thread, detail = %error.detail, "Failed to persist thread workspace preparation failure"),
            }
        } else {
            tracing::warn!(thread_id = thread, %detail, "Thread workspace preparation failed");
        }
    }
}

/// Effect request keys under which a preparation records what it created.
const CLAIMED_WORKTREE: &str = "claimedWorktree";
const SETUP_TERMINAL: &str = "setupTerminal";

/// Best effort: a note that cannot be written only costs the cleanup after a crash.
fn note_on_effect(store: &Store, effect_id: &str, key: &str, value: &str) {
    let written = store.transaction(|transaction| {
        transaction.execute(
            "UPDATE rust_effect_outbox SET request_json=json_set(request_json,?1,?2) WHERE id=?3",
            rusqlite::params![format!("$.{key}"), value, effect_id],
        )?;
        Ok(())
    });
    if let Err(error) = written {
        tracing::warn!(effect = effect_id, key, %error, "Failed to record workspace preparation progress");
    }
}

fn t3_vcs_core() -> t3_vcs::core::GitCore {
    crate::vcs_service::VcsService::shared().core().clone()
}

/// Dispatches one of the commands a preparation issues against the run it prepares.
pub(crate) fn dispatch_prepared(store: &Store, command: &Value, now: chrono::DateTime<Utc>) -> Result<crate::persistence::Receipt, StoreError> {
    let command_id = command["commandId"].as_str().unwrap_or("");
    let thread_id = command["threadId"].as_str().unwrap_or("");
    let kind = command["type"].as_str().unwrap_or("");
    store.dispatch(
        command_id,
        "thread",
        thread_id,
        kind,
        now,
        |transaction| {
            let projection = read_projection(transaction, "thread", thread_id)?
                .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
            match kind {
                "prepared-run.progress" => crate::prepared_run::plan_progress(command, &projection, now),
                "prepared-run.fail" => crate::prepared_run::plan_fail(command, &projection, now),
                "prepared-run.release" => crate::execution::plan_release_in(command, &projection, now, Some(transaction)),
                other => Err(StoreError::InvalidCommand(format!("{other} is not a preparation command."))),
            }
        },
        thread::reduce,
    )
}

/// Resolves once the thread is deleted. The cleanup of a deleted thread queues behind this
/// preparation (effects of one thread run one at a time), so a preparation waiting on a long
/// setup script must stop waiting by itself.
async fn deleted(store: &Store, mut committed: tokio::sync::broadcast::Receiver<Vec<crate::persistence::StoredEvent>>, thread: &str) {
    loop {
        let gone = store.projection("thread", thread).ok().flatten().is_some_and(|projection| !projection["thread"]["deletedAt"].is_null());
        if gone {
            return;
        }
        if let Err(tokio::sync::broadcast::error::RecvError::Closed) = committed.recv().await {
            return std::future::pending().await;
        }
    }
}

async fn cancelled(mut receiver: watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

/// Runs one `workspace.prepare` effect. A preparation that fails is recorded on its run and
/// the effect still succeeds: only an unreadable store fails the effect itself.
pub(crate) async fn execute(
    store: &Store,
    providers: &ProviderRegistry,
    runtime: &Arc<WorkspaceRuntime>,
    effect: &Effect,
    mut stopped: watch::Receiver<bool>,
) -> Result<(), String> {
    let request = &effect.request;
    let thread_id = effect.thread_id.as_str();
    let committed = store.subscribe();
    let projection = match store.projection("thread", thread_id).map_err(|error| error.to_string())? {
        Some(projection) if projection["thread"]["deletedAt"].is_null() => projection,
        _ => return Ok(()),
    };
    let run_id = request["runId"].as_str().map(str::to_owned);
    // Preparation that already ended, by interrupt or restart, is not started again.
    if let Some(run_id) = &run_id {
        let preparing = projection["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["id"] == run_id.as_str() && run["status"] == "preparing"));
        if !preparing {
            return Ok(());
        }
    }
    let project_id = projection["thread"]["projectId"].as_str().unwrap_or("");
    let project = store
        .projection("project", project_id)
        .map_err(|error| error.to_string())?
        .filter(|project| project["deletedAt"].is_null());
    let base = request["baseCommandId"].as_str().unwrap_or(&effect.command_id).to_owned();
    let mut strategy = request["workspaceStrategy"].clone();
    let mut reused = None;
    // A worktree the failed attempt already created and recorded is reused, not created again.
    if request["retry"] == true && strategy["type"] == "worktree" {
        if let (Some(path), Some(branch)) = (projection["thread"]["worktreePath"].as_str(), projection["thread"]["branch"].as_str()) {
            reused = Some(strategy["baseRef"].as_str().unwrap_or("").to_owned());
            strategy = json!({"type":"existing_worktree","worktreePath":path,"branch":branch});
        }
    }
    let tracked = strategy["type"] == "worktree" || reused.is_some();
    let handle = if strategy["type"] == "worktree" {
        Some(runtime.tracker.begin(thread_id, strategy["branch"].as_str(), strategy["baseRef"].as_str(), &[Stage::Fetch, Stage::Checkout, Stage::SetupScript, Stage::Agent]))
    } else if let Some(base_ref) = &reused {
        Some(runtime.tracker.begin(thread_id, strategy["branch"].as_str(), Some(base_ref), &[Stage::SetupScript, Stage::Agent]))
    } else {
        None
    };
    let (cancel_flag, settled) = match handle {
        Some(handle) => (handle.cancelled, Some(handle.settled)),
        None => (watch::channel(false).1, None),
    };
    let shared = Shared::default();
    let Some(project) = project else {
        let context = Context {
            store: store.clone(), runtime: runtime.clone(), settings: providers.settings(), providers: providers.clone(), thread_id: thread_id.to_owned(),
            project_id: project_id.parse().map_err(|error: t3_contracts::ValidationError| error.to_string())?, project: Value::Null, effect_id: effect.id.clone(),
            base, run_id, strategy, reused, tracked,
        };
        context.abandon(&shared, Stop::Failed(PrepError::new("resolve-project", "Project no longer exists."))).await;
        if let Some(settled) = settled {
            settled.send_replace(true);
        }
        return Ok(());
    };
    let context = Context {
        store: store.clone(),
        runtime: runtime.clone(),
        settings: providers.settings(),
        providers: providers.clone(),
        thread_id: thread_id.to_owned(),
        project_id: project_id.parse().map_err(|error: t3_contracts::ValidationError| error.to_string())?,
        project,
        effect_id: effect.id.clone(),
        base,
        run_id,
        strategy,
        reused,
        tracked,
    };
    // A claim after the first means the process that ran this preparation was lost. Its
    // setup script and git checkout died with it, so it ends as an interrupted preparation
    // does in the original: cancelled, with the worktree it created removed and the run
    // failed so the user can retry.
    if effect.attempt_count > 1 {
        shared.created.lock().unwrap().clone_from(&request[CLAIMED_WORKTREE].as_str().map(str::to_owned));
        shared.terminal.lock().unwrap().clone_from(&request[SETUP_TERMINAL].as_str().map(str::to_owned));
        Box::pin(context.abandon(&shared, Stop::Cancelled)).await;
        if let Some(settled) = settled {
            settled.send_replace(true);
        }
        return Ok(());
    }
    let outcome = tokio::select! {
        result = Box::pin(context.prepare(&shared)) => result.map_err(Stop::Failed),
        _ = cancelled(cancel_flag) => Err(Stop::Cancelled),
        _ = deleted(store, committed, thread_id) => Err(Stop::Deleted),
        _ = stopped.changed() => Err(Stop::Cancelled),
    };
    if let Err(stop) = outcome {
        Box::pin(context.abandon(&shared, stop)).await;
    }
    if let Some(settled) = settled {
        settled.send_replace(true);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_branch_names_follow_the_original_shapes() {
        assert_eq!(temporary_branch_name("0123456789abcdef"), "t3/01234567");
        // A UUID-shaped token still produces the canonical form.
        assert_eq!(temporary_branch_name("A1B2C3D4-e5f6-4789-8abc-def012345678"), "t3/a1b2c3d4");
        assert!(is_temporary_branch("t3/0123abcd"));
        assert!(is_temporary_branch(" T3-0123ABCD "));
        assert!(is_temporary_branch("t3code/0123abcd"));
        assert!(is_temporary_branch("t3code-0123abcd"));
        assert!(is_temporary_branch("t3code/6f9619ff-8b86-4d11-b42d-00c04fc964ff"));
        // Nothing looser than what was generated.
        assert!(!is_temporary_branch("t3/0123abc"));
        assert!(!is_temporary_branch("t3/0123abcde"));
        assert!(!is_temporary_branch("t3"));
        assert!(!is_temporary_branch("feature/0123abcd"));
        assert!(!is_temporary_branch("t3code/6f9619ff-8b86-1d11-b42d-00c04fc964ff"));
        assert_eq!(flatten_temporary_branch_name("t3/0123abcd"), "t3-0123abcd");
        assert_eq!(flatten_temporary_branch_name("T3CODE/0123ABCD"), "t3-0123abcd");
        assert_eq!(flatten_temporary_branch_name("t3code/6f9619ff-8b86-4d11-b42d-00c04fc964ff"), "t3-6f9619ff");
    }

    #[test]
    fn failures_read_as_the_step_that_failed() {
        assert_eq!(
            PrepError::new("provision-worktree", "no space").describe(),
            "Workspace preparation failed during provision worktree: no space"
        );
        assert_eq!(
            PrepError::new("run-setup-script", "exited").describe(),
            "Workspace preparation failed during run setup script: exited"
        );
    }
}

#[cfg(test)]
mod parity {
    use super::*;

    /// Temporary branch names and the shapes the original still recognises.
    #[test]
    fn branch_names_match_the_original() {
        let rows: Vec<Value> = include_str!("../tests/fixtures/workspace-preparation.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for row in rows.iter().filter(|row| row["operation"] == "branch-names") {
            for case in row["cases"].as_array().unwrap() {
                assert_eq!(temporary_branch_name(case["token"].as_str().unwrap()), case["name"].as_str().unwrap(), "{case}");
            }
        }
        for row in rows.iter().filter(|row| row["operation"] == "temporary-branches") {
            for case in row["cases"].as_array().unwrap() {
                let name = case["name"].as_str().unwrap();
                assert_eq!(is_temporary_branch(name), case["temporary"], "{case}");
                assert_eq!(flatten_temporary_branch_name(name), case["flat"].as_str().unwrap(), "{case}");
            }
        }
    }
}
