//! Server self-update (`apps/server/src/cloud/selfUpdate.ts`): what the `server.update` RPC does.
//! A remote client (including another T3 Code on a different machine) asks this server to move to
//! an exact version. How depends on who manages the server:
//!
//! * **desktop-managed** (the server runs inside the desktop app): the app's own update feed
//!   decides what it installs ([`DesktopAppUpdate`]); the request's version is ignored.
//! * **boot-service** (a launcher started the server, [`crate::service_launcher_client`]): the
//!   target version is installed as a pinned runtime ([`crate::pinned_runtime`]), its staged
//!   executable is preflighted ([`crate::service_preflight`]), and only then does the launcher get a
//!   `request-update` that makes it restart the child on the new version.
//! * otherwise the request fails with an instruction to install the background service.
//!
//! One update at a time per server. [`RunningThreadContinuation`] wraps the service so that, when the
//! client asked to continue running threads, those threads are marked just before the handoff and
//! unmarked again if the update then fails (but not once the handoff was accepted, because the
//! replacement server resumes them).
//!
//! The original is built on Effect fibers: "interrupted" there is a dropped future here. Dropping
//! a wrapped update before the handoff clears the marks from a detached task; dropping it after
//! the handoff was accepted keeps them.

use std::collections::{BTreeMap, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use t3_contracts::{ServerSelfUpdateCapability, ServerSelfUpdateMethod};

use crate::pinned_runtime::{
    PinnedRuntimeError, PinnedRuntimeInstallInput, PinnedRuntimePaths, ProcessRunner,
    ReleaseFetcher, RuntimeValidator, ensure_pinned_runtime_installed, pinned_runtime_command,
};
use crate::service_launcher_client::{RequestUpdateError, ServiceLauncherClient};
use crate::service_preflight::{ServicePreflightResult, decode_service_preflight_result};
use crate::service_protocol::{SERVICE_LAUNCHER_PROTOCOL, is_exact_service_version};
use crate::ssh_tunnel::BoxFuture;

const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// `ServerSelfUpdateInput`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSelfUpdateInput {
    /// Exact version of the `t3` package to install (never a dist-tag, so the server and the
    /// acknowledging client agree on what was requested).
    pub target_version: String,
    /// Opt-in recovery for provider turns that are running when the server hands off to its
    /// replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_running_threads: Option<bool>,
}

/// `ServerSelfUpdateResult`: the update artifact is installed and the server is about to restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSelfUpdateResult {
    pub target_version: String,
    pub method: ServerSelfUpdateMethod,
    /// Launcher-generated correlation id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_id: Option<String>,
    /// Desktop preparation token; present only for the desktop-app method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop_update_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerSelfUpdateProgressStage {
    Downloading,
    Installing,
}

/// `ServerSelfUpdateError`
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Server update failed: {reason}")]
pub struct ServerSelfUpdateError {
    pub reason: String,
    pub cause: Option<String>,
}

impl ServerSelfUpdateError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            cause: None,
        }
    }

    fn with_cause(reason: impl Into<String>, cause: impl std::fmt::Display) -> Self {
        Self {
            reason: reason.into(),
            cause: Some(cause.to_string()),
        }
    }
}

/// Reports a stage to the caller (the RPC streams it to the client); an error aborts the update.
pub type ProgressReporter = Arc<
    dyn Fn(ServerSelfUpdateProgressStage) -> BoxFuture<'static, Result<(), ServerSelfUpdateError>>
        + Send
        + Sync,
>;
/// Called once the launcher or desktop app has taken over the update.
pub type HandoffCallback = Arc<dyn Fn() + Send + Sync>;

/// A reporter that ignores stages.
pub fn silent_progress() -> ProgressReporter {
    Arc::new(|_| Box::pin(async { Ok(()) }))
}

pub fn no_handoff_callback() -> HandoffCallback {
    Arc::new(|| {})
}

/// `resolveServerSelfUpdateCapability`
pub fn resolve_server_self_update_capability(
    desktop_managed: bool,
    launcher_managed: bool,
) -> Option<ServerSelfUpdateCapability> {
    if desktop_managed {
        return Some(ServerSelfUpdateCapability::DesktopManaged);
    }
    launcher_managed.then_some(ServerSelfUpdateCapability::BootService)
}

/// `ServerSelfUpdate`'s methods.
#[async_trait]
pub trait SelfUpdateService: Send + Sync {
    async fn update(
        &self,
        input: ServerSelfUpdateInput,
        report_progress: ProgressReporter,
        on_handoff_accepted: HandoffCallback,
    ) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError>;

    /// Starts the prepared desktop install. Success stops this server, so this returns only when
    /// installation fails or times out.
    async fn commit_desktop_update(
        &self,
        request_id: &str,
        on_handoff_accepted: HandoffCallback,
    ) -> Result<Infallible, ServerSelfUpdateError>;
}

/// The desktop app's update service (`DesktopAppUpdate`).
#[async_trait]
pub trait DesktopAppUpdate: Send + Sync {
    /// True when this server was spawned by a desktop app that can be driven over its control channel.
    fn available(&self) -> bool;
    /// Checks and downloads through the desktop app, then returns a token while this server is still
    /// connected. `commit` starts installation.
    async fn run(
        &self,
        report_progress: ProgressReporter,
    ) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError>;
    async fn commit(
        &self,
        request_id: &str,
        on_handoff_accepted: HandoffCallback,
    ) -> Result<Infallible, ServerSelfUpdateError>;
}

/// What `make` reads from the server's config and host.
#[derive(Debug, Clone)]
pub struct ServerSelfUpdateConfig {
    /// `serverConfig.mode === "desktop"`
    pub desktop_mode: bool,
    pub base_dir: PathBuf,
    pub db_path: String,
    /// Node platform names: `linux`, `darwin`, `win32`; and `x64`, `arm64`.
    pub platform: String,
    pub arch: String,
    /// `T3CODE_RELEASE_BASE_URL`
    pub release_base_url: Option<String>,
    /// The process environment (for the Windows `tar` location).
    pub env: BTreeMap<String, String>,
}

/// The `__service-preflight` check of a staged runtime.
struct PreflightValidator {
    runner: Arc<dyn ProcessRunner>,
    db_path: String,
    target_version: String,
}

#[async_trait]
impl RuntimeValidator for PreflightValidator {
    async fn validate(&self, paths: &PinnedRuntimePaths) -> Result<(), PinnedRuntimeError> {
        const STEP: &str = "running the staged service preflight";
        let (command, mut args) = pinned_runtime_command(paths);
        args.extend([
            "__service-preflight".to_owned(),
            "--database-path".to_owned(),
            self.db_path.clone(),
            "--launcher-protocol".to_owned(),
            SERVICE_LAUNCHER_PROTOCOL.to_string(),
        ]);
        let result = self
            .runner
            .run(&command, &args, Some(PREFLIGHT_TIMEOUT))
            .await
            .map_err(|cause| PinnedRuntimeError::step_with_cause(STEP, cause))?;
        if result.code != 0 {
            return Err(PinnedRuntimeError::Install {
                step: STEP.to_owned(),
                exit_code: Some(result.code),
                stdout_length: Some(result.stdout.encode_utf16().count()),
                stderr_length: Some(result.stderr.encode_utf16().count()),
                cause: None,
            });
        }
        let parsed: Value = serde_json::from_str(crate::util::js_trim(&result.stdout))
            .map_err(|cause| {
                PinnedRuntimeError::step_with_cause("decoding the staged service preflight", cause)
            })?;
        let preflight = decode_service_preflight_result(&parsed);
        let Some(preflight) = preflight.filter(|p| p.version() == self.target_version) else {
            return Err(PinnedRuntimeError::step(
                "verifying the staged service preflight",
            ));
        };
        match preflight {
            ServicePreflightResult::Ready { .. } => Ok(()),
            ServicePreflightResult::Blocked { version, reason } => {
                Err(PinnedRuntimeError::PreflightBlocked { version, reason })
            }
        }
    }
}

/// `ServerSelfUpdate` (`make`).
pub struct ServerSelfUpdate {
    config: ServerSelfUpdateConfig,
    desktop: Arc<dyn DesktopAppUpdate>,
    launcher: Arc<ServiceLauncherClient>,
    runner: Arc<dyn ProcessRunner>,
    fetcher: Arc<dyn ReleaseFetcher>,
    in_flight: AtomicBool,
}

impl ServerSelfUpdate {
    pub fn new(
        config: ServerSelfUpdateConfig,
        desktop: Arc<dyn DesktopAppUpdate>,
        launcher: Arc<ServiceLauncherClient>,
        runner: Arc<dyn ProcessRunner>,
        fetcher: Arc<dyn ReleaseFetcher>,
    ) -> Self {
        Self {
            config,
            desktop,
            launcher,
            runner,
            fetcher,
            in_flight: AtomicBool::new(false),
        }
    }

    /// The `serverSelfUpdate` capability this server advertises.
    pub fn capability(&self) -> Option<ServerSelfUpdateCapability> {
        resolve_server_self_update_capability(self.config.desktop_mode, self.launcher.managed())
    }

    async fn update_through_launcher(
        &self,
        target_version: &str,
        report_progress: &ProgressReporter,
        on_handoff_accepted: &HandoffCallback,
    ) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        report_progress(ServerSelfUpdateProgressStage::Downloading).await?;
        let validator = PreflightValidator {
            runner: self.runner.clone(),
            db_path: self.config.db_path.clone(),
            target_version: target_version.to_owned(),
        };
        let paths = ensure_pinned_runtime_installed(PinnedRuntimeInstallInput {
            base_dir: &self.config.base_dir,
            version: target_version,
            runner: self.runner.as_ref(),
            fetcher: self.fetcher.as_ref(),
            validator: &validator,
            platform: &self.config.platform,
            arch: &self.config.arch,
            release_base_url: self.config.release_base_url.as_deref(),
            on_progress: None,
            env: &self.config.env,
        })
        .await
        .map_err(|error| match &error {
            PinnedRuntimeError::PreflightBlocked { reason, .. } => {
                ServerSelfUpdateError::with_cause(reason.clone(), &error)
            }
            _ => ServerSelfUpdateError::with_cause(
                format!("Could not prepare t3@{target_version}."),
                &error,
            ),
        })?;

        report_progress(ServerSelfUpdateProgressStage::Installing).await?;
        // The handoff must not be abandoned halfway: once asked, the request runs to its answer.
        let launcher = self.launcher.clone();
        let (version, db_path) = (target_version.to_owned(), self.config.db_path.clone());
        let update_id = tokio::spawn(async move { launcher.request_update(&version, &db_path).await })
            .await
            .map_err(|error| ServerSelfUpdateError::with_cause("The update request was interrupted.", error))?
            .map_err(|error| match &error {
                RequestUpdateError::Rejected(rejected) => {
                    ServerSelfUpdateError::with_cause(rejected.reason.clone(), &error)
                }
                RequestUpdateError::Client(_) => ServerSelfUpdateError::with_cause(
                    "Could not ask the service launcher to activate the prepared update.",
                    &error,
                ),
            })?;
        on_handoff_accepted();
        tracing::info!(%update_id, target_version, runtime_path = %paths.entry_path.display(), "Server update prepared; handing off to the service launcher.");
        Ok(ServerSelfUpdateResult {
            target_version: target_version.to_owned(),
            method: ServerSelfUpdateMethod::BootService,
            update_id: Some(update_id),
            desktop_update_token: None,
        })
    }
}

#[async_trait]
impl SelfUpdateService for ServerSelfUpdate {
    async fn update(
        &self,
        input: ServerSelfUpdateInput,
        report_progress: ProgressReporter,
        on_handoff_accepted: HandoffCallback,
    ) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        match self.capability() {
            Some(ServerSelfUpdateCapability::DesktopManaged) => {
                // input.target_version is meaningless here: the desktop app's own update feed decides
                // what it downloads, and the result carries what it actually got.
                if self.desktop.available() {
                    return self.desktop.run(report_progress).await;
                }
                return Err(ServerSelfUpdateError::new(
                    "This server is managed by the T3 Code desktop app on its machine; update the desktop app to update it.",
                ));
            }
            None | Some(ServerSelfUpdateCapability::Respawn) => {
                return Err(ServerSelfUpdateError::new(
                    "Remote updates require the T3 Code background service. Run `t3 service install` on the server machine.",
                ));
            }
            Some(ServerSelfUpdateCapability::BootService) => {}
        }
        let target_version = crate::util::js_trim(&input.target_version).to_owned();
        if !is_exact_service_version(&target_version) {
            return Err(ServerSelfUpdateError::new(format!(
                "'{target_version}' is not an exact t3 version."
            )));
        }
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return Err(ServerSelfUpdateError::new(
                "A server update is already in progress.",
            ));
        }
        // Releases the slot on failure or cancellation; on success the server is about to be
        // replaced and the slot stays taken.
        struct Slot<'a> {
            flag: &'a AtomicBool,
            keep: bool,
        }
        impl Drop for Slot<'_> {
            fn drop(&mut self) {
                if !self.keep {
                    self.flag.store(false, Ordering::SeqCst);
                }
            }
        }
        let mut slot = Slot {
            flag: &self.in_flight,
            keep: false,
        };
        let result = self
            .update_through_launcher(&target_version, &report_progress, &on_handoff_accepted)
            .await?;
        slot.keep = true;
        Ok(result)
    }

    async fn commit_desktop_update(
        &self,
        request_id: &str,
        on_handoff_accepted: HandoffCallback,
    ) -> Result<Infallible, ServerSelfUpdateError> {
        self.desktop.commit(request_id, on_handoff_accepted).await
    }
}

// ---- continuing running threads across the handoff -------------------------------------------------

type PrepareFn =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<String>, ServerSelfUpdateError>> + Send + Sync>;
type ClearFn =
    Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, Result<(), ServerSelfUpdateError>> + Send + Sync>;

/// `withRunningThreadContinuation`: wraps a [`SelfUpdateService`]. `prepare` marks the running
/// threads (returning their ids) and `clear` removes the marks again.
pub struct RunningThreadContinuation<S> {
    desktop_mode: bool,
    inner: Arc<S>,
    prepare: PrepareFn,
    clear: ClearFn,
    desktop_tokens: Arc<Mutex<HashSet<String>>>,
}

impl<S: SelfUpdateService + 'static> RunningThreadContinuation<S> {
    pub fn new(
        desktop_mode: bool,
        inner: Arc<S>,
        prepare: PrepareFn,
        clear: ClearFn,
    ) -> Self {
        Self {
            desktop_mode,
            inner,
            prepare,
            clear,
            desktop_tokens: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Runs `work`, clearing the marks when it fails (the clear's own failure replaces the error) or
    /// is dropped before the handoff was accepted.
    async fn clear_on_error<T>(
        &self,
        work: impl std::future::Future<Output = Result<T, ServerSelfUpdateError>>,
        thread_ids: &Arc<Mutex<Vec<String>>>,
        handoff_accepted: &Arc<AtomicBool>,
    ) -> Result<T, ServerSelfUpdateError> {
        struct CancelGuard {
            armed: bool,
            clear: ClearFn,
            thread_ids: Arc<Mutex<Vec<String>>>,
            handoff_accepted: Arc<AtomicBool>,
        }
        impl Drop for CancelGuard {
            fn drop(&mut self) {
                if self.armed && !self.handoff_accepted.load(Ordering::SeqCst) {
                    let ids = self.thread_ids.lock().unwrap().clone();
                    let clear = self.clear.clone();
                    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                        runtime.spawn(async move {
                            let _ = clear(ids).await;
                        });
                    }
                }
            }
        }
        let mut guard = CancelGuard {
            armed: true,
            clear: self.clear.clone(),
            thread_ids: thread_ids.clone(),
            handoff_accepted: handoff_accepted.clone(),
        };
        let outcome = work.await;
        guard.armed = false;
        match outcome {
            Ok(value) => Ok(value),
            Err(error) => {
                let ids = thread_ids.lock().unwrap().clone();
                (self.clear)(ids).await?;
                Err(error)
            }
        }
    }
}

#[async_trait]
impl<S: SelfUpdateService + 'static> SelfUpdateService for RunningThreadContinuation<S> {
    async fn update(
        &self,
        request: ServerSelfUpdateInput,
        report_progress: ProgressReporter,
        on_handoff_accepted: HandoffCallback,
    ) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        let thread_ids = Arc::new(Mutex::new(Vec::<String>::new()));
        let handoff_accepted = Arc::new(AtomicBool::new(false));
        let prepared = Arc::new(AtomicBool::new(false));
        let continue_threads = request.continue_running_threads == Some(true);
        let desktop_mode = self.desktop_mode;
        let wrapped: ProgressReporter = {
            let (prepare, ids, prepared) = (self.prepare.clone(), thread_ids.clone(), prepared);
            Arc::new(move |stage| {
                let (prepare, ids, prepared, report) =
                    (prepare.clone(), ids.clone(), prepared.clone(), report_progress.clone());
                Box::pin(async move {
                    if continue_threads
                        && !desktop_mode
                        && stage == ServerSelfUpdateProgressStage::Installing
                        && !prepared.load(Ordering::SeqCst)
                    {
                        let marked = prepare().await?;
                        prepared.store(true, Ordering::SeqCst);
                        *ids.lock().unwrap() = marked;
                    }
                    report(stage).await
                })
            })
        };
        let accepted_flag = handoff_accepted.clone();
        let accepted: HandoffCallback = Arc::new(move || {
            accepted_flag.store(true, Ordering::SeqCst);
            on_handoff_accepted();
        });
        let result = self
            .clear_on_error(
                self.inner.update(request, wrapped, accepted),
                &thread_ids,
                &handoff_accepted,
            )
            .await?;
        if result.method == ServerSelfUpdateMethod::DesktopApp && continue_threads {
            if let Some(token) = &result.desktop_update_token {
                self.desktop_tokens.lock().unwrap().insert(token.clone());
            }
        }
        Ok(result)
    }

    async fn commit_desktop_update(
        &self,
        request_id: &str,
        on_handoff_accepted: HandoffCallback,
    ) -> Result<Infallible, ServerSelfUpdateError> {
        let should_continue = self.desktop_tokens.lock().unwrap().remove(request_id);
        let thread_ids = Arc::new(Mutex::new(Vec::<String>::new()));
        let handoff_accepted = Arc::new(AtomicBool::new(false));
        let accepted_flag = handoff_accepted.clone();
        let accepted: HandoffCallback = Arc::new(move || {
            accepted_flag.store(true, Ordering::SeqCst);
            on_handoff_accepted();
        });
        let work = async {
            if should_continue {
                let marked = (self.prepare)().await?;
                *thread_ids.lock().unwrap() = marked;
            }
            self.inner.commit_desktop_update(request_id, accepted).await
        };
        // A token is spent only by a commit that reached the handoff, so a retry (after a failure or a
        // cancellation before the handoff) marks again.
        struct Restore<'a> {
            tokens: &'a Mutex<HashSet<String>>,
            request_id: &'a str,
            armed: bool,
            handoff_accepted: Arc<AtomicBool>,
        }
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                if self.armed && !self.handoff_accepted.load(Ordering::SeqCst) {
                    self.tokens.lock().unwrap().insert(self.request_id.to_owned());
                }
            }
        }
        let _restore = Restore {
            tokens: &self.desktop_tokens,
            request_id,
            armed: should_continue,
            handoff_accepted: handoff_accepted.clone(),
        };
        let outcome = self.clear_on_error(work, &thread_ids, &handoff_accepted).await;
        outcome
    }
}
