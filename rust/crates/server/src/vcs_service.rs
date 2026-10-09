//! The server's git services behind the `vcs.*` methods: repository detection, status and its
//! subscription, ref listing, branch and worktree changes, pulling and initialising. One
//! `VcsService` serves the process (`shared`), so every caller sees the same caches; what a call
//! needs from settings, projects and background policy arrives in a `VcsContext`.
//!
//! Every mutating method refreshes the directory's status in a detached task once it
//! succeeds, as `refreshGitStatus` does in the original, so subscribers see the change.
use crate::{
    background_policy::BackgroundPolicy,
    background_settings,
    persistence::Store,
    server_settings::SettingsService,
    vcs_error::VcsRpcError,
    vcs_input::{
        CreateRefInput, CreateWorktreeInput, InitInput, RemoveWorktreeInput, SwitchRefInput,
    },
    vcs_registry::VcsRegistry,
    vcs_status::{Broadcaster, GitStatusSource, Hooks, StatusSource},
    vcs_worktree::{
        populate_submodules_from_project_file, resolve_worktrees_directory, submodule_setting,
    },
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};
use t3_contracts::{BackgroundScope, ProjectId, ServerSettings};
use t3_vcs::{
    Clock, GitCommandError, GitVcs, SystemClock, VcsDriverKind, VcsError, VcsProcess,
    core::{CreateWorktreeOptions, GitCore, VcsCreateWorktreeInput, WorktreeSubmodules},
    status::VcsStatus,
    wire::{VcsListRefsInput, VcsListRefsResult, VcsPullResult},
};

const DEFAULT_FETCH_INTERVAL: Duration = Duration::from_secs(30);

/// What a call reads from the rest of the server.
#[derive(Clone)]
pub struct VcsContext {
    pub store: Store,
    pub settings: Option<SettingsService>,
    pub background: Option<BackgroundPolicy>,
}

impl VcsContext {
    pub async fn settings_snapshot(&self) -> Option<ServerSettings> {
        match self.settings.as_ref()?.snapshot().await {
            Ok(settings) => Some(settings),
            Err(error) => {
                tracing::warn!(error=%error,"Failed to read server settings for a VCS operation");
                None
            }
        }
    }

    /// The active project rooted at exactly `cwd` (`findActiveByWorkspaceRoot`).
    async fn project_at(&self, cwd: &str) -> Option<ProjectId> {
        let store = self.store.clone();
        let cwd = cwd.to_string();
        tokio::task::spawn_blocking(move || {
            store
                .projections("project")
                .ok()?
                .into_iter()
                .find(|row| row["deletedAt"].is_null() && row["workspaceRoot"] == cwd.as_str())
                .and_then(|row| row["projectId"].as_str()?.parse().ok())
        })
        .await
        .ok()
        .flatten()
    }

    /// `<T3 home>/worktrees`. The state directory is `<T3 home>/userdata`; a state directory
    /// with any other name is itself the home.
    pub(crate) fn default_worktrees_dir(&self) -> PathBuf {
        let state_dir = self.store.state_dir().map(Path::to_path_buf);
        let home = match state_dir {
            Some(dir) if dir.file_name().is_some_and(|name| name == "userdata") => {
                dir.parent().map(Path::to_path_buf).unwrap_or(dir)
            }
            Some(dir) => dir,
            None => std::env::temp_dir().join("t3-home"),
        };
        home.join("worktrees")
    }

    /// The background policy, settings and project reads the broadcaster consults.
    pub fn hooks(&self) -> Hooks {
        let (settings, background) = (self.clone(), self.background.clone());
        let fetch_context = settings.clone();
        let policy_context = settings;
        Hooks {
            fetch_interval: Arc::new(move || {
                let context = fetch_context.clone();
                Box::pin(async move {
                    match context.settings.as_ref() {
                        None => DEFAULT_FETCH_INTERVAL,
                        Some(service) => match service.snapshot().await {
                            Ok(settings) => fetch_interval(&settings),
                            Err(error) => {
                                tracing::warn!(detail=%error,"Failed to read automatic Git fetch interval setting");
                                DEFAULT_FETCH_INTERVAL
                            }
                        },
                    }
                })
            }),
            // No background policy configured means nothing pauses the work.
            scope_allowed: Arc::new(move |cwd| {
                let background = background.clone();
                Box::pin(async move {
                    match background {
                        Some(policy) => {
                            policy
                                .should_run_scope_work(&BackgroundScope::VcsStatus { cwd })
                                .await
                        }
                        None => true,
                    }
                })
            }),
            auto_pull: Arc::new(move |cwd| {
                let context = policy_context.clone();
                Box::pin(async move {
                    let Some(project) = context.project_at(&cwd).await else {
                        return false;
                    };
                    let Some(settings) = context.settings_snapshot().await else {
                        return false;
                    };
                    settings
                        .project_settings_overrides
                        .get(&project)
                        .and_then(|overrides| overrides.default_auto_pull)
                        .unwrap_or(settings.default_auto_pull)
                })
            }),
        }
    }
}

/// `resolveServerBackgroundActivitySettings(settings).automaticGitFetchInterval`.
fn fetch_interval(settings: &ServerSettings) -> Duration {
    let resolved = background_settings::resolve_server(
        &serde_json::to_value(settings).expect("serializable settings"),
    );
    resolved["automaticGitFetchInterval"]
        .as_f64()
        .map_or(DEFAULT_FETCH_INTERVAL, |millis| {
            Duration::from_millis(millis.max(0.0) as u64)
        })
}

pub struct VcsService {
    registry: Arc<VcsRegistry>,
    core: GitCore,
    status: Broadcaster<GitStatusSource>,
}

impl VcsService {
    pub fn new(git: GitVcs, clock: Arc<dyn Clock>) -> Self {
        let registry = Arc::new(VcsRegistry::new(git.clone(), clock.clone()));
        let core = git.core();
        let source = GitStatusSource::new(registry.clone(), core.clone(), clock);
        Self {
            registry,
            core,
            status: Broadcaster::new(source),
        }
    }

    /// The process's service, on the system clock and the default git timings.
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<VcsService> = OnceLock::new();
        SHARED.get_or_init(|| Self::new(GitVcs::new(VcsProcess::new()), Arc::new(SystemClock)))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn registry(&self) -> &VcsRegistry {
        &self.registry
    }

    pub fn status(&self) -> &Broadcaster<GitStatusSource> {
        &self.status
    }

    /// The process's git primitives, whose caches the status broadcaster shares.
    pub fn core(&self) -> &GitCore {
        &self.core
    }

    /// Refreshes the status of `cwd` in a detached task and logs a failure.
    fn refresh_in_background(&self, cwd: &str, context: &VcsContext) {
        let (status, hooks, cwd) = (self.status.clone(), context.hooks(), cwd.to_string());
        tokio::spawn(async move {
            if let Err(error) = status.refresh_status(&cwd, &hooks).await {
                tracing::warn!(cwd=%cwd,error=%error,"VCS status refresh failed");
            }
        });
    }

    pub async fn refresh_status(
        &self,
        cwd: &str,
        context: &VcsContext,
    ) -> Result<VcsStatus, VcsRpcError> {
        self.status.refresh_status(cwd, &context.hooks()).await
    }

    pub async fn pull(
        &self,
        cwd: &str,
        context: &VcsContext,
    ) -> Result<VcsPullResult, VcsRpcError> {
        let result = self.status.source().pull_current_branch(cwd).await?;
        self.refresh_in_background(cwd, context);
        Ok(result)
    }

    /// `GitWorkflowService.listRefs`: a directory that is no repository lists nothing.
    pub async fn list_refs(
        &self,
        input: &VcsListRefsInput,
    ) -> Result<VcsListRefsResult, VcsRpcError> {
        const OPERATION: &str = "GitWorkflowService.listRefs";
        match self.registry.detect(&input.cwd, None).await {
            Err(cause) => Err(VcsRpcError::route(
                OPERATION,
                &input.cwd,
                "Failed to detect a VCS repository for this Git command.",
                Some(&cause),
            )),
            Ok(None) => Ok(VcsListRefsResult {
                refs: Vec::new(),
                is_repo: false,
                has_primary_remote: false,
                next_cursor: None,
                total_count: 0,
            }),
            Ok(Some(handle)) if handle.kind != VcsDriverKind::Git => Err(VcsRpcError::route(
                OPERATION,
                &input.cwd,
                format!(
                    "The {OPERATION} command currently supports Git repositories only; detected {}.",
                    handle.kind
                ),
                None,
            )),
            Ok(Some(_)) => Ok(self.core.list_refs(input).await?),
        }
    }

    /// Creates a worktree for `ref_name` (or a new branch off it) and returns its path and
    /// branch. The checkout goes under the `worktreesDirectory` setting unless the input names
    /// a path. Submodules follow the project and environment `worktreeSubmodules` setting; when
    /// neither sets one, the new checkout's own `t3.json` decides, which can only be read once
    /// the checkout exists.
    pub async fn create_worktree(
        &self,
        input: &CreateWorktreeInput,
        context: &VcsContext,
    ) -> Result<Value, VcsRpcError> {
        self.registry
            .ensure_git_command("GitWorkflowService.createWorktree", &input.cwd)
            .await?;
        let settings = context.settings_snapshot().await;
        let worktrees_setting = settings
            .as_ref()
            .map(|settings| settings.worktrees_directory.as_str().to_string())
            .unwrap_or_default();
        let setting = match &settings {
            Some(settings) => {
                let project = if settings.project_settings_overrides.is_empty() {
                    None
                } else {
                    context.project_at(&input.cwd).await
                };
                submodule_setting(settings, project.as_ref())
            }
            None => None,
        };
        let default_dir = context.default_worktrees_dir();
        let worktrees_directory = if input.path.is_some() {
            default_dir
        } else {
            resolve_worktrees_directory(&worktrees_setting, &default_dir).ok_or_else(|| {
                VcsRpcError::from(GitCommandError {
                    operation: "GitVcsDriver.createWorktree".into(),
                    command: "git worktree add".into(),
                    cwd: input.cwd.clone(),
                    argument_count: None,
                    exit_code: None,
                    stdout_length: None,
                    stderr_length: None,
                    output_length: None,
                    reason: None,
                    detail: format!("The worktree location \"{worktrees_setting}\" must be an absolute folder on this machine, not a drive root. Change it in Settings → Storage."),
                    cause: None,
                })
            })?
        };
        let worktree = self
            .core
            .create_worktree(
                &VcsCreateWorktreeInput {
                    cwd: PathBuf::from(&input.cwd),
                    ref_name: input.ref_name.clone(),
                    new_ref_name: input.new_ref_name.clone(),
                    base_ref_name: input.base_ref_name.clone(),
                    path: input.path.as_ref().map(PathBuf::from),
                },
                &CreateWorktreeOptions {
                    worktrees_directory,
                    submodules: setting.unwrap_or(WorktreeSubmodules::None),
                    progress: None,
                },
            )
            .await?;
        if setting.is_none() {
            populate_submodules_from_project_file(&self.core, &worktree.path).await;
        }
        self.refresh_in_background(&input.cwd, context);
        Ok(json!({"worktree":{"path":worktree.path.to_string_lossy(),"refName":worktree.ref_name}}))
    }

    pub async fn remove_worktree(
        &self,
        input: &RemoveWorktreeInput,
        context: &VcsContext,
    ) -> Result<(), VcsRpcError> {
        self.registry
            .ensure_git_command("GitWorkflowService.removeWorktree", &input.cwd)
            .await?;
        self.core
            .remove_worktree(
                Path::new(&input.cwd),
                Path::new(&input.path),
                input.force.unwrap_or(false),
            )
            .await?;
        self.refresh_in_background(&input.cwd, context);
        Ok(())
    }

    pub async fn create_ref(
        &self,
        input: &CreateRefInput,
        context: &VcsContext,
    ) -> Result<String, VcsRpcError> {
        self.registry
            .ensure_git_command("GitWorkflowService.createRef", &input.cwd)
            .await?;
        let ref_name = self
            .core
            .create_ref(
                Path::new(&input.cwd),
                &input.ref_name,
                input.switch_ref.unwrap_or(false),
            )
            .await?;
        self.refresh_in_background(&input.cwd, context);
        Ok(ref_name)
    }

    /// The branch now checked out; `None` when HEAD ended up detached.
    pub async fn switch_ref(
        &self,
        input: &SwitchRefInput,
        context: &VcsContext,
    ) -> Result<Option<String>, VcsRpcError> {
        self.registry
            .ensure_git_command("GitWorkflowService.switchRef", &input.cwd)
            .await?;
        let ref_name = self
            .core
            .switch_ref(Path::new(&input.cwd), &input.ref_name)
            .await?;
        self.refresh_in_background(&input.cwd, context);
        Ok(ref_name)
    }

    /// `VcsProvisioningService.initRepository`: git unless the caller names another kind.
    pub async fn init(&self, input: &InitInput, context: &VcsContext) -> Result<(), VcsRpcError> {
        let kind = match input.kind {
            None => VcsDriverKind::Git,
            Some(VcsDriverKind::Unknown) => {
                return Err(VcsError::UnsupportedOperation {
                    operation: "VcsProvisioningService.resolveRequestedKind".into(),
                    kind: VcsDriverKind::Unknown,
                    detail: "A concrete VCS driver kind is required for repository provisioning."
                        .into(),
                }
                .into());
            }
            Some(kind) => kind,
        };
        self.registry
            .driver(kind)?
            .init_repository(Path::new(&input.cwd))
            .await?;
        self.refresh_in_background(&input.cwd, context);
        Ok(())
    }
}
