//! The flows of the background-service installer (`apps/server/src/cloud/bootService.ts`) around
//! the renderers that [`crate::boot_service`] already holds: which platform manager applies, the
//! command steps each flow runs, the prerequisite checks, and the install / restart / uninstall /
//! status sequences. Every effect is behind a trait the caller supplies ([`ProcessRunner`] runs
//! `systemctl`, `loginctl` and `launchctl`; [`ReleaseFetcher`] downloads the pinned runtime), so
//! this crate never touches a service manager; the tests drive it with fakes.
//!
//! # What `t3 service install` does
//!
//! 1. Pick the manager: systemd user units on Linux, launchd agents on macOS with a known uid.
//! 2. Systemd only: `systemctl --user show-environment` must work, and lingering must be on (it is
//!    enabled with `loginctl enable-linger` when it is off but the manager works).
//! 3. Install the pinned runtime for this CLI version and check `t3 --version` reports it
//!    ([`crate::pinned_runtime`]).
//! 4. If a unit is installed (and the service is to be restarted), stop it first. Refuse when the
//!    launcher's state file shows an update in flight or the installed version is newer than this one
//!    (unless `allow_downgrade`).
//! 5. Write `runtime/service-state.json` (the launcher's state, [`crate::service_protocol`]) and the
//!    unit/plist, each through a temp file, fsync and rename; then run the manager's activate steps.
//!    A failure after the stop tries to restart the old service.
//!
//! With `start: false` (`t3 update` when the user declines the restart) the files change but the
//! running service is left alone, and `runtime/.restart-pending` records that it is behind its unit.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::boot_service::{
    BOOT_SERVICE_LAUNCHD_LABEL, BOOT_SERVICE_LOG_FILE, BOOT_SERVICE_PLIST_FILE,
    BOOT_SERVICE_UNIT_FILE, BootServicePlan, BootServiceProblem, boot_service_base_dir_of,
    render_boot_service_plist, render_boot_service_unit,
};
use crate::cli_release::CLI_RELEASE_BASE_URL_ENV;
use crate::pinned_runtime::{
    PinnedRuntimeError, PinnedRuntimeInstallInput, PinnedRuntimePaths, ProcessRunner,
    ReleaseFetcher, RuntimeValidator, ensure_pinned_runtime_installed, pinned_runtime_command,
    pinned_runtime_paths,
};
use crate::service_protocol::{
    SERVICE_RESTART_PENDING_FILE, SERVICE_STATE_FILE, ServiceState, ServiceUpdateRecord,
    compare_exact_service_versions, parse_service_state, service_state_active_version,
    service_state_has_pending_update,
};

/// Stop commands block until the service manager gives up: 90 s by default for systemd's
/// TimeoutStopSec, and ExitTimeOut=90 in the rendered plist. This must stay above both, or the
/// runner cancels the stop mid-shutdown and the next step races a still-loaded service.
const STOP_STEP_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// `BootServiceStep`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootServiceStep {
    pub step: &'static str,
    pub command: &'static str,
    pub args: Vec<String>,
    /// A non-zero exit is logged and ignored. Reserved for steps whose common failures (not loaded,
    /// already enabled) leave a state a later strict step either tolerates or fails loudly on.
    pub optional: bool,
    /// Overrides the runner's default (60 s) for steps that block longer.
    pub timeout: Option<Duration>,
}

fn step(
    step: &'static str,
    command: &'static str,
    args: &[&str],
    optional: bool,
    timeout: Option<Duration>,
) -> BootServiceStep {
    BootServiceStep {
        step,
        command,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        optional,
        timeout,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootServiceKind {
    Systemd,
    Launchd,
}

/// `BootServiceManager`: platform service-manager integration as data, so install / uninstall /
/// status never branch on platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootServiceManager {
    pub kind: BootServiceKind,
    pub unit_path: PathBuf,
    home_dir: String,
    environment_path: String,
    /// Before rewriting files, when a unit is already installed.
    pub stop: Vec<BootServiceStep>,
    /// After files are written. The last entry starts the service.
    pub activate: Vec<BootServiceStep>,
    /// Best-effort recovery after a failed repair of an installed service.
    pub restart: Vec<BootServiceStep>,
    /// Uninstall, before the unit file is removed.
    pub deactivate: Vec<BootServiceStep>,
    /// Uninstall, after the unit file is removed.
    pub finalize: Vec<BootServiceStep>,
}

impl BootServiceManager {
    pub fn render(&self, plan: &BootServicePlan) -> String {
        match self.kind {
            BootServiceKind::Systemd => render_boot_service_unit(plan),
            BootServiceKind::Launchd => {
                render_boot_service_plist(plan, &self.home_dir, &self.environment_path)
            }
        }
    }
}

fn systemd_manager(home_dir: &str) -> BootServiceManager {
    BootServiceManager {
        kind: BootServiceKind::Systemd,
        unit_path: Path::new(home_dir)
            .join(".config")
            .join("systemd")
            .join("user")
            .join(BOOT_SERVICE_UNIT_FILE),
        home_dir: home_dir.to_owned(),
        environment_path: String::new(),
        stop: vec![step(
            "stopping the installed service",
            "systemctl",
            &["--user", "stop", BOOT_SERVICE_UNIT_FILE],
            false,
            Some(STOP_STEP_TIMEOUT),
        )],
        activate: vec![
            step(
                "reloading systemd user units",
                "systemctl",
                &["--user", "daemon-reload"],
                false,
                None,
            ),
            step(
                "enabling the service",
                "systemctl",
                &["--user", "enable", BOOT_SERVICE_UNIT_FILE],
                false,
                None,
            ),
            // Start last. No administrative state write occurs after this succeeds.
            step(
                "starting the service",
                "systemctl",
                &["--user", "restart", BOOT_SERVICE_UNIT_FILE],
                false,
                None,
            ),
        ],
        restart: vec![step(
            "restarting the service after a failed update",
            "systemctl",
            &["--user", "restart", BOOT_SERVICE_UNIT_FILE],
            false,
            None,
        )],
        deactivate: vec![step(
            "stopping the service",
            "systemctl",
            &["--user", "disable", "--now", BOOT_SERVICE_UNIT_FILE],
            false,
            Some(STOP_STEP_TIMEOUT),
        )],
        finalize: vec![step(
            "reloading systemd user units",
            "systemctl",
            &["--user", "daemon-reload"],
            false,
            None,
        )],
    }
}

fn launchd_manager(home_dir: &str, uid: u32, environment_path: &str) -> BootServiceManager {
    let unit_path = Path::new(home_dir)
        .join("Library")
        .join("LaunchAgents")
        .join(BOOT_SERVICE_PLIST_FILE);
    let domain_target = format!("gui/{uid}");
    let service_target = format!("{domain_target}/{BOOT_SERVICE_LAUNCHD_LABEL}");
    let unit = unit_path.to_string_lossy().into_owned();
    // bootout/enable are optional: they fail on not-loaded states that are fine to proceed from. The
    // strict `bootstrap` runs last and is also the start: loading a RunAtLoad/KeepAlive plist starts
    // the job, so a separate kickstart would kill and restart a server it just booted.
    BootServiceManager {
        kind: BootServiceKind::Launchd,
        unit_path,
        home_dir: home_dir.to_owned(),
        environment_path: environment_path.to_owned(),
        // Without --wait, bootout returns in milliseconds while the job drains for up to
        // ExitTimeOut, and a bootstrap during the drain fails EIO.
        stop: vec![step(
            "stopping the installed launch agent",
            "launchctl",
            &["bootout", "--wait", &service_target],
            true,
            Some(STOP_STEP_TIMEOUT),
        )],
        activate: vec![
            // A persisted `launchctl disable` override refuses bootstrap; clear it.
            step(
                "enabling the launch agent",
                "launchctl",
                &["enable", &service_target],
                true,
                None,
            ),
            step(
                "starting the service",
                "launchctl",
                &["bootstrap", &domain_target, &unit],
                false,
                None,
            ),
        ],
        restart: vec![step(
            "restarting the service after a failed update",
            "launchctl",
            &["bootstrap", &domain_target, &unit],
            false,
            None,
        )],
        // No `launchctl disable` here: a persisted override would sabotage a later reinstall.
        deactivate: vec![step(
            "stopping the service",
            "launchctl",
            &["bootout", "--wait", &service_target],
            true,
            Some(STOP_STEP_TIMEOUT),
        )],
        finalize: vec![],
    }
}

/// `selectBootServiceManager`: `None` means this host cannot run the background service.
pub fn select_boot_service_manager(
    platform: &str,
    home_dir: &str,
    uid: Option<u32>,
    environment_path: &str,
) -> Option<BootServiceManager> {
    if home_dir.is_empty() {
        return None;
    }
    match (platform, uid) {
        ("linux", _) => Some(systemd_manager(home_dir)),
        ("darwin", Some(uid)) => Some(launchd_manager(home_dir, uid, environment_path)),
        _ => None,
    }
}

/// The `PATH` the launchd plist carries: the installer's own directories (those that are safe
/// XML text), the directory of the running executable, and the usual system ones.
pub fn boot_service_environment_path(installer_path: &str, exec_path: &str) -> String {
    let exec_dir = match Path::new(exec_path).parent() {
        Some(parent) if parent.as_os_str().is_empty() => ".".to_owned(),
        Some(parent) => parent.to_string_lossy().into_owned(),
        None => "/".to_owned(),
    };
    let mut directories: Vec<String> = Vec::new();
    let mut add = |directory: String| {
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    };
    for directory in installer_path.split(':') {
        let xml_safe = directory
            .chars()
            .all(|c| (c as u32) >= 0x20 || matches!(c, '\t' | '\n' | '\r'));
        if !directory.is_empty() && xml_safe {
            add(directory.to_owned());
        }
    }
    add(exec_dir);
    for fixed in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        add(fixed.to_owned());
    }
    directories.join(":")
}

// ---- errors ----------------------------------------------------------------------------------

/// `BootServiceError`
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BootServiceError {
    #[error("Background setup supports Linux with systemd and macOS with launchd; this machine reports '{platform}'.")]
    Unsupported { platform: String },
    #[error("{}", command_message(.step, .exit_code))]
    Command {
        step: String,
        exit_code: Option<i32>,
        stdout_length: Option<usize>,
        stderr_length: Option<usize>,
        cause: Option<String>,
    },
    #[error("Could not set up the T3 Code background service.")]
    Install { cause: String },
    #[error("[{}] {}", .problem.code(), .problem.describe())]
    Prerequisite {
        problem: BootServiceProblem,
        cause: Option<String>,
    },
    #[error("A remote server update is still pending. Wait for it to finish, then retry.")]
    UpdatePending,
    #[error("Refusing to replace t3@{installed_version} with older t3@{target_version}. Run the command again with --allow-downgrade to continue.")]
    DowngradeRefused {
        installed_version: String,
        target_version: String,
    },
}

fn command_message(step: &str, exit_code: &Option<i32>) -> String {
    match exit_code {
        None => format!("Background setup failed while {step}."),
        Some(code) => format!("Background setup failed while {step} (exit code {code})."),
    }
}

fn install_error(cause: impl std::fmt::Display) -> BootServiceError {
    BootServiceError::Install {
        cause: cause.to_string(),
    }
}

/// `BootServiceStatus`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootServiceStatus {
    pub supported: bool,
    pub installed: bool,
    pub current: bool,
    pub installed_version: Option<String>,
    /// The T3 home the installed unit serves. The unit name is fixed per user, so a caller working
    /// against another base dir must not treat this service as its own.
    pub installed_base_dir: Option<String>,
    pub problems: Vec<BootServiceProblem>,
    pub unit_path: String,
    pub log_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct BootServiceInstallOptions {
    pub allow_downgrade: bool,
    /// `false` writes the files but leaves the running service alone.
    pub start: Option<bool>,
}

/// What the original reads from its Effect context and constructor input.
#[derive(Debug, Clone)]
pub struct BootServiceConfig {
    pub base_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub cli_version: String,
    /// The running executable (`HostProcessExecutablePath`); only its directory matters.
    pub exec_path: String,
    pub platform: String,
    pub arch: String,
    pub uid: Option<u32>,
    /// `$HOME`; empty when unset.
    pub home_dir: String,
    /// `$PATH` of the installer.
    pub installer_path: String,
    pub env: BTreeMap<String, String>,
}

impl BootServiceConfig {
    pub fn release_base_url(&self) -> Option<&String> {
        self.env.get(CLI_RELEASE_BASE_URL_ENV)
    }
}

struct VersionValidator {
    runner: Arc<dyn ProcessRunner>,
    cli_version: String,
}

#[async_trait::async_trait]
impl RuntimeValidator for VersionValidator {
    async fn validate(&self, paths: &PinnedRuntimePaths) -> Result<(), PinnedRuntimeError> {
        const STEP: &str = "verifying the pinned t3 runtime";
        let (command, mut args) = pinned_runtime_command(paths);
        args.push("--version".to_owned());
        let result = self
            .runner
            .run(&command, &args, Some(Duration::from_secs(30)))
            .await
            .map_err(|cause| PinnedRuntimeError::step_with_cause(STEP, cause))?;
        // `/\bv(\S+)\s*$/` over stdout: the version after the last `v`-word.
        let reported = result
            .stdout
            .trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .and_then(|word| word.strip_prefix('v'))
            .filter(|v| !v.is_empty());
        if result.code == 0 && reported == Some(self.cli_version.as_str()) {
            Ok(())
        } else {
            Err(PinnedRuntimeError::Install {
                step: STEP.to_owned(),
                exit_code: Some(result.code),
                stdout_length: Some(result.stdout.encode_utf16().count()),
                stderr_length: Some(result.stderr.encode_utf16().count()),
                cause: None,
            })
        }
    }
}

/// `BootService`
pub struct BootService {
    config: BootServiceConfig,
    runner: Arc<dyn ProcessRunner>,
    fetcher: Arc<dyn ReleaseFetcher>,
    manager: Option<BootServiceManager>,
    unit_path: String,
    log_path: PathBuf,
    state_path: PathBuf,
    restart_pending_path: PathBuf,
    runtime_paths: PinnedRuntimePaths,
}

impl BootService {
    pub fn new(
        config: BootServiceConfig,
        runner: Arc<dyn ProcessRunner>,
        fetcher: Arc<dyn ReleaseFetcher>,
    ) -> Self {
        let environment_path =
            boot_service_environment_path(&config.installer_path, &config.exec_path);
        let manager = select_boot_service_manager(
            &config.platform,
            &config.home_dir,
            config.uid,
            &environment_path,
        );
        let runtime_dir = config.base_dir.join("runtime");
        Self {
            unit_path: manager
                .as_ref()
                .map(|m| m.unit_path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            log_path: config.logs_dir.join(BOOT_SERVICE_LOG_FILE),
            state_path: runtime_dir.join(SERVICE_STATE_FILE),
            restart_pending_path: runtime_dir.join(SERVICE_RESTART_PENDING_FILE),
            runtime_paths: pinned_runtime_paths(
                &config.base_dir,
                &config.cli_version,
                &config.platform,
            ),
            config,
            runner,
            fetcher,
            manager,
        }
    }

    fn plan(&self) -> BootServicePlan {
        BootServicePlan {
            // The executable hosts the launcher as a hidden subcommand of itself, so the unit runs
            // the pinned runtime directly.
            program: vec![
                self.runtime_paths.entry_path.to_string_lossy().into_owned(),
                "__service-launcher".to_owned(),
            ],
            base_dir: self.config.base_dir.to_string_lossy().into_owned(),
            log_path: self.log_path.to_string_lossy().into_owned(),
            unit_path: self.unit_path.clone(),
        }
    }

    fn require_manager(&self) -> Result<&BootServiceManager, BootServiceError> {
        self.manager
            .as_ref()
            .ok_or_else(|| BootServiceError::Unsupported {
                platform: self.config.platform.clone(),
            })
    }

    fn log_failure(&self, error: &BootServiceError) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(file, "{} {error}", crate::clock::format_iso(now_ms()));
        }
    }

    async fn run_step(
        &self,
        step: &str,
        command: &str,
        args: &[String],
        timeout: Option<Duration>,
    ) -> Result<crate::pinned_runtime::ProcessRunOutput, BootServiceError> {
        let result = self
            .runner
            .run(command, args, timeout)
            .await
            .map_err(|cause| BootServiceError::Command {
                step: step.to_owned(),
                exit_code: None,
                stdout_length: None,
                stderr_length: None,
                cause: Some(cause),
            })
            .and_then(|output| {
                if output.code == 0 {
                    Ok(output)
                } else {
                    Err(BootServiceError::Command {
                        step: step.to_owned(),
                        exit_code: Some(output.code),
                        stdout_length: Some(output.stdout.encode_utf16().count()),
                        stderr_length: Some(output.stderr.encode_utf16().count()),
                        cause: None,
                    })
                }
            });
        if let Err(error) = &result {
            self.log_failure(error);
        }
        result
    }

    async fn run_steps(&self, steps: &[BootServiceStep]) -> Result<(), BootServiceError> {
        for entry in steps {
            let outcome = self
                .run_step(entry.step, entry.command, &entry.args, entry.timeout)
                .await;
            // run_step already appended a failure to the log, so an ignored optional step still
            // leaves a trace.
            if !entry.optional {
                outcome?;
            }
        }
        Ok(())
    }

    async fn probe(&self, command: &str, args: &[&str]) -> Option<crate::pinned_runtime::ProcessRunOutput> {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        self.runner
            .run(command, &args, Some(PROBE_TIMEOUT))
            .await
            .ok()
            .filter(|output| output.code == 0)
    }

    fn linger_args(&self) -> Vec<String> {
        let mut args = vec!["show-user".to_owned()];
        args.extend(self.config.uid.map(|uid| uid.to_string()));
        args.push("--property=Linger".to_owned());
        args.push("--value".to_owned());
        args
    }

    /// `readSystemdProblems`
    async fn read_systemd_problems(&self, include_service: bool) -> Vec<BootServiceProblem> {
        let linger_args = self.linger_args();
        let linger_refs: Vec<&str> = linger_args.iter().map(String::as_str).collect();
        let (manager, linger) = tokio::join!(
            self.probe("systemctl", &["--user", "show-environment"]),
            self.probe("loginctl", &linger_refs)
        );
        let mut problems = Vec::new();
        if manager.is_none() {
            problems.push(BootServiceProblem::UserManagerUnavailable);
        }
        let lingering = linger
            .as_ref()
            .map(|output| output.stdout.trim().to_owned())
            .unwrap_or_default();
        if lingering != "yes" {
            problems.push(if lingering == "no" {
                BootServiceProblem::LingerDisabled
            } else {
                BootServiceProblem::LingerUnavailable
            });
        }
        if include_service && manager.is_some() {
            let (enabled, active) = tokio::join!(
                self.probe("systemctl", &["--user", "is-enabled", BOOT_SERVICE_UNIT_FILE]),
                self.probe("systemctl", &["--user", "is-active", BOOT_SERVICE_UNIT_FILE])
            );
            if enabled.as_ref().map(|o| o.stdout.trim()) != Some("enabled") {
                problems.push(BootServiceProblem::ServiceDisabled);
            }
            if active.is_none() {
                problems.push(BootServiceProblem::ServiceStopped);
            }
        }
        problems
    }

    /// `requireSystemdPrerequisites`: enables lingering when only that is missing.
    async fn require_systemd_prerequisites(&self) -> Result<(), BootServiceError> {
        let problems = self.read_systemd_problems(false).await;
        if let Some(unavailable) = problems
            .iter()
            .find(|p| **p != BootServiceProblem::LingerDisabled)
        {
            return Err(BootServiceError::Prerequisite {
                problem: *unavailable,
                cause: None,
            });
        }
        if !problems.contains(&BootServiceProblem::LingerDisabled) {
            return Ok(());
        }
        let mut args = vec!["enable-linger".to_owned(), "--no-ask-password".to_owned()];
        args.extend(self.config.uid.map(|uid| uid.to_string()));
        self.run_step("enabling lingering for this user", "loginctl", &args, None)
            .await
            .map_err(|cause| BootServiceError::Prerequisite {
                problem: BootServiceProblem::LingerDisabled,
                cause: Some(cause.to_string()),
            })?;
        if let Some(remaining) = self.read_systemd_problems(false).await.first() {
            return Err(BootServiceError::Prerequisite {
                problem: *remaining,
                cause: None,
            });
        }
        Ok(())
    }

    /// `install`
    pub async fn install(
        &self,
        options: BootServiceInstallOptions,
    ) -> Result<BootServicePlan, BootServiceError> {
        let manager = self.require_manager()?;
        std::fs::create_dir_all(&self.config.logs_dir).map_err(install_error)?;

        // A permissions failure must not leave a partial install or stop a working server.
        if manager.kind == BootServiceKind::Systemd {
            if let Err(error) = self.require_systemd_prerequisites().await {
                self.log_failure(&error);
                return Err(error);
            }
        }

        // Prepare every immutable artifact before stopping the installed unit.
        let validator = VersionValidator {
            runner: self.runner.clone(),
            cli_version: self.config.cli_version.clone(),
        };
        ensure_pinned_runtime_installed(PinnedRuntimeInstallInput {
            base_dir: &self.config.base_dir,
            version: &self.config.cli_version,
            runner: self.runner.as_ref(),
            fetcher: self.fetcher.as_ref(),
            validator: &validator,
            platform: &self.config.platform,
            arch: &self.config.arch,
            release_base_url: self.config.release_base_url().map(String::as_str),
            on_progress: None,
            env: &self.config.env,
        })
        .await
        .map_err(|error| match &error {
            PinnedRuntimeError::Install {
                step,
                exit_code,
                stdout_length,
                stderr_length,
                ..
            } => BootServiceError::Command {
                step: step.clone(),
                exit_code: *exit_code,
                stdout_length: *stdout_length,
                stderr_length: *stderr_length,
                cause: Some(error.to_string()),
            },
            _ => install_error(&error),
        })?;
        let installed = Path::new(&self.unit_path).exists();
        // With start=false the service keeps running while its files change. The launcher reads the
        // state file once at startup and the unit only matters on the next start, so that is safe as
        // long as the launcher is not in the middle of a remote update, which is the one time it
        // writes the state file itself. That case is refused below, before anything is written, from
        // the same read the downgrade check uses; the stop that normally serialises against the
        // launcher is skipped on purpose.
        let start = options.start != Some(false);
        if installed && start {
            self.run_steps(&manager.stop).await?;
        }

        let plan = self.plan();
        let outcome = self.write_and_activate(manager, &plan, installed, start, &options).await;
        if let Err(error) = &outcome {
            if installed && start {
                let _ = self.run_steps(&manager.restart).await;
            }
            return Err(error.clone());
        }
        Ok(plan)
    }

    async fn write_and_activate(
        &self,
        manager: &BootServiceManager,
        plan: &BootServicePlan,
        installed: bool,
        start: bool,
        options: &BootServiceInstallOptions,
    ) -> Result<(), BootServiceError> {
        if installed {
            if let Ok(previous) = std::fs::read_to_string(&self.state_path) {
                if service_state_has_pending_update(&previous) {
                    return Err(BootServiceError::UpdatePending);
                }
                // A remote update can finish after the CLI checks status. Read its final version
                // after the launcher stops and before changing files.
                if let Some(installed_version) = service_state_active_version(&previous) {
                    if !options.allow_downgrade
                        && compare_exact_service_versions(&self.config.cli_version, &installed_version)
                            .is_lt()
                    {
                        return Err(BootServiceError::DowngradeRefused {
                            installed_version,
                            target_version: self.config.cli_version.clone(),
                        });
                    }
                }
            }
        }
        let unit_path = Path::new(&self.unit_path);
        std::fs::create_dir_all(unit_path.parent().unwrap_or(Path::new("."))).map_err(install_error)?;
        if !start && installed {
            // Written first: once the files below name the new version, the running service is
            // behind them, and a failure between the two writes must not leave it looking current.
            // The launcher removes the marker when it starts, `restart` and a started install do too.
            if let Some(directory) = self.restart_pending_path.parent() {
                std::fs::create_dir_all(directory).map_err(install_error)?;
            }
            write_private(&self.restart_pending_path, &format!("{}\n", self.config.cli_version))
                .map_err(install_error)?;
        }
        write_durably(
            &self.state_path,
            &ServiceState {
                active_version: self.config.cli_version.clone(),
                update: None::<ServiceUpdateRecord>,
            }
            .to_json_text(),
        )
        .map_err(install_error)?;
        if !start && installed {
            // The launcher only writes this file while a remote update is in flight. One that began
            // after the check above lands either before this write (then the launcher's copy in
            // memory is what it keeps acting on) or after it, which this read catches.
            let written = std::fs::read_to_string(&self.state_path).map_err(install_error)?;
            if service_state_active_version(&written).as_deref() != Some(self.config.cli_version.as_str()) {
                return Err(BootServiceError::UpdatePending);
            }
        }
        write_durably(unit_path, &manager.render(plan)).map_err(install_error)?;
        if start {
            self.run_steps(&manager.activate).await?;
            let _ = std::fs::remove_file(&self.restart_pending_path);
        }
        Ok(())
    }

    /// `restart`: stop and start the installed service on the version its unit names. Only when the
    /// unit serves this base dir: the unit name is per user, so another home's service is left
    /// alone. `false` when nothing was restarted.
    pub async fn restart(&self) -> Result<bool, BootServiceError> {
        let manager = self.require_manager()?;
        let Ok(unit) = std::fs::read_to_string(&self.unit_path) else {
            return Ok(false);
        };
        let same_home = boot_service_base_dir_of(&unit).is_some_and(|installed| {
            std::path::absolute(&installed).ok() == std::path::absolute(&self.config.base_dir).ok()
        });
        if !same_home {
            return Ok(false);
        }
        self.run_steps(&manager.stop).await?;
        if let Err(error) = self.run_steps(&manager.activate).await {
            // Same recovery as a failed repair: a service that was running should not be left
            // stopped because daemon-reload or enable failed.
            let _ = self.run_steps(&manager.restart).await;
            return Err(error);
        }
        let _ = std::fs::remove_file(&self.restart_pending_path);
        Ok(true)
    }

    /// `uninstall`: `false` when nothing was installed.
    pub async fn uninstall(&self) -> Result<bool, BootServiceError> {
        let manager = self.require_manager()?;
        if !Path::new(&self.unit_path).exists() {
            return Ok(false);
        }
        self.run_steps(&manager.deactivate).await?;
        std::fs::remove_file(&self.unit_path).map_err(install_error)?;
        self.run_steps(&manager.finalize).await?;
        Ok(true)
    }

    /// `status`
    pub async fn status(&self) -> Result<BootServiceStatus, BootServiceError> {
        let log_path = self.log_path.to_string_lossy().into_owned();
        let Some(manager) = &self.manager else {
            return Ok(BootServiceStatus {
                supported: false,
                installed: false,
                current: false,
                installed_version: None,
                installed_base_dir: None,
                problems: vec![],
                unit_path: self.unit_path.clone(),
                log_path,
            });
        };
        if !Path::new(&self.unit_path).exists() {
            return Ok(BootServiceStatus {
                supported: true,
                installed: false,
                current: false,
                installed_version: None,
                installed_base_dir: None,
                problems: vec![],
                unit_path: self.unit_path.clone(),
                log_path,
            });
        }
        let unit = std::fs::read_to_string(&self.unit_path).map_err(install_error)?;
        let runtime_entry_exists = self.runtime_paths.entry_path.exists();
        let runtime_sentinel = std::fs::read_to_string(&self.runtime_paths.sentinel_path).ok();
        let state_text = std::fs::read_to_string(&self.state_path).ok();
        let state = state_text.as_deref().and_then(parse_service_state);
        let installed_version = state_text.as_deref().and_then(service_state_active_version);
        let installed_base_dir = boot_service_base_dir_of(&unit);
        let normalize = |contents: &str| -> String {
            if manager.kind == BootServiceKind::Launchd {
                static PATH_VALUE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
                PATH_VALUE
                    .get_or_init(|| {
                        regex::Regex::new(r"(<key>PATH</key>\n\s*<string>)[^<]*(</string>)")
                            .expect("path pattern")
                    })
                    .replace(contents, "$1$2")
                    .into_owned()
            } else {
                contents.to_owned()
            }
        };
        let mut problems = if manager.kind == BootServiceKind::Systemd {
            self.read_systemd_problems(true).await
        } else {
            vec![]
        };
        if self.restart_pending_path.exists() {
            problems.push(BootServiceProblem::RestartPending);
        }
        let current = problems.is_empty()
            && normalize(&unit) == normalize(&manager.render(&self.plan()))
            && runtime_entry_exists
            && runtime_sentinel
                .as_deref()
                .is_some_and(|s| s.trim() == self.config.cli_version)
            && state
                .as_ref()
                .is_some_and(|s| s.active_version == self.config.cli_version)
            && !state
                .as_ref()
                .is_some_and(|s| matches!(s.update, Some(ServiceUpdateRecord::Pending(_))));
        Ok(BootServiceStatus {
            supported: true,
            installed: true,
            current,
            installed_version,
            installed_base_dir,
            problems,
            unit_path: self.unit_path.clone(),
            log_path,
        })
    }
}

fn now_ms() -> i64 {
    use crate::clock::Clock;
    crate::clock::SystemClock.now_ms()
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// `writeDurably`: a temp file in the same directory, flushed, then renamed over the target, and the
/// directory flushed, so a crash leaves either the old file or the new one.
fn write_durably(path: &Path, contents: &str) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let temp = directory.join(format!(".service-write-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
        return result;
    }
    // Windows has no directory fsync (EPERM); NTFS journals the rename.
    if let Ok(dir) = std::fs::File::open(directory) {
        let _ = dir.sync_all();
    }
    Ok(())
}
