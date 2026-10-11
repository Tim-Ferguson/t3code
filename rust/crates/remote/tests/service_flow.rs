//! The background-service and self-update machinery that concerns a remote host: pinned runtimes,
//! the launcher client, `server.update`, the update handoff markers and the boot-service flows
//! (`apps/server/src/cloud/{pinnedRuntime,serviceLauncherClient,selfUpdate,updateHandoff,bootService}.test.ts`).
//! Every effect is a fake: no `systemctl`, `launchctl` or `loginctl` runs, nothing is downloaded,
//! and all files live in temp directories named `t3code-rust-port-test-…`.
mod support;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use sha2::{Digest, Sha256};
use support::{temp_dir, within};
use t3_contracts::{ServerSelfUpdateCapability, ServerSelfUpdateMethod};
use t3_remote::boot_service::{BootServiceProblem, boot_service_base_dir_of};
use t3_remote::boot_service_install::*;
use t3_remote::clock::Clock;
use t3_remote::cloud_link::UpdateHandoff;
use t3_remote::pinned_runtime::*;
use t3_remote::self_update::*;
use t3_remote::service_launcher_client::*;
use t3_remote::service_protocol::*;
use t3_remote::ssh_tunnel::BoxFuture;
use t3_remote::update_handoff::*;

// ---- fakes ---------------------------------------------------------------------------------

type RunHandler = Box<dyn Fn(&str, &[String]) -> Result<ProcessRunOutput, String> + Send + Sync>;

struct FakeRunner {
    commands: Mutex<Vec<String>>,
    timeouts: Mutex<BTreeMap<String, Option<Duration>>>,
    handler: RunHandler,
}

impl FakeRunner {
    fn new(handler: impl Fn(&str, &[String]) -> Result<ProcessRunOutput, String> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            commands: Mutex::new(vec![]),
            timeouts: Mutex::new(BTreeMap::new()),
            handler: Box::new(handler),
        })
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn timeout_of(&self, command: &str) -> Option<Duration> {
        self.timeouts.lock().unwrap().get(command).copied().flatten()
    }
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn run(&self, command: &str, args: &[String], timeout: Option<Duration>) -> Result<ProcessRunOutput, String> {
        let line = format!("{command} {}", args.join(" "));
        self.commands.lock().unwrap().push(line.clone());
        self.timeouts.lock().unwrap().insert(line, timeout);
        (self.handler)(command, args)
    }
}

fn ok(stdout: &str) -> Result<ProcessRunOutput, String> {
    Ok(ProcessRunOutput {
        code: 0,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    })
}

fn exit(code: i32) -> Result<ProcessRunOutput, String> {
    Ok(ProcessRunOutput {
        code,
        stdout: String::new(),
        stderr: "err".into(),
    })
}

struct FakeStream {
    chunks: std::vec::IntoIter<Vec<u8>>,
    length: Option<u64>,
}

#[async_trait]
impl ReleaseAssetStream for FakeStream {
    fn content_length(&self) -> Option<u64> {
        self.length
    }

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        Ok(self.chunks.next())
    }
}

/// Serves assets from memory, in `chunk` sized pieces, and records the requests.
struct FakeFetcher {
    assets: BTreeMap<String, Vec<u8>>,
    requested: Mutex<Vec<String>>,
    announce_length: bool,
    /// The archive request never answers (an interrupted download).
    hang_on_archive: AtomicBool,
    archive_started: tokio::sync::Notify,
}

#[async_trait]
impl ReleaseFetcher for FakeFetcher {
    async fn open(&self, url: &str) -> Result<Box<dyn ReleaseAssetStream>, String> {
        self.requested.lock().unwrap().push(url.to_owned());
        if !url.ends_with("SHA256SUMS") && self.hang_on_archive.load(Ordering::SeqCst) {
            self.archive_started.notify_one();
            std::future::pending::<()>().await;
        }
        let bytes = self.assets.get(url).ok_or_else(|| format!("404 for {url}"))?;
        Ok(Box::new(FakeStream {
            chunks: bytes.chunks(4).map(<[u8]>::to_vec).collect::<Vec<_>>().into_iter(),
            length: self.announce_length.then_some(bytes.len() as u64),
        }))
    }
}

const ARCHIVE_BYTES: &[u8] = b"not really a tarball";

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn release_fetcher(version: &str, platform_key: &str, checksum: Option<String>) -> Arc<FakeFetcher> {
    let base = format!("https://github.com/pingdotgg/t3code/releases/download/v{version}");
    let file = format!("t3-{version}-{platform_key}.tar.gz");
    let sums = format!("{}  {file}\n", checksum.unwrap_or_else(|| sha256_hex(ARCHIVE_BYTES)));
    Arc::new(FakeFetcher {
        assets: BTreeMap::from([
            (format!("{base}/SHA256SUMS"), sums.into_bytes()),
            (format!("{base}/{file}"), ARCHIVE_BYTES.to_vec()),
        ]),
        requested: Mutex::new(vec![]),
        announce_length: true,
        hang_on_archive: AtomicBool::new(false),
        archive_started: tokio::sync::Notify::new(),
    })
}

/// `tar` stand-in: writes the executable into the staging directory it is given.
fn tar_writes_t3(command: &str, args: &[String]) -> Option<Result<ProcessRunOutput, String>> {
    if command != "tar" {
        return None;
    }
    let staging = &args[args.iter().position(|a| a == "-C").unwrap() + 1];
    std::fs::write(Path::new(staging).join("t3"), "#!/bin/sh\n").unwrap();
    Some(ok(""))
}

struct AcceptAll;

#[async_trait]
impl RuntimeValidator for AcceptAll {
    async fn validate(&self, _paths: &PinnedRuntimePaths) -> Result<(), PinnedRuntimeError> {
        Ok(())
    }
}

struct RecordingValidator {
    seen: Mutex<Vec<(std::path::PathBuf, bool)>>,
    fail: bool,
}

#[async_trait]
impl RuntimeValidator for RecordingValidator {
    async fn validate(&self, paths: &PinnedRuntimePaths) -> Result<(), PinnedRuntimeError> {
        self.seen
            .lock()
            .unwrap()
            .push((paths.version_dir.clone(), paths.entry_path.exists()));
        if self.fail {
            Err(PinnedRuntimeError::step("verifying the pinned t3 runtime"))
        } else {
            Ok(())
        }
    }
}

fn no_env() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn install_input<'a>(
    base_dir: &'a Path,
    runner: &'a dyn ProcessRunner,
    fetcher: &'a dyn ReleaseFetcher,
    validator: &'a dyn RuntimeValidator,
    env: &'a BTreeMap<String, String>,
    on_progress: Option<&'a (dyn Fn(PinnedRuntimeProgress) + Send + Sync)>,
) -> PinnedRuntimeInstallInput<'a> {
    PinnedRuntimeInstallInput {
        base_dir,
        version: "1.1.0",
        runner,
        fetcher,
        validator,
        platform: "linux",
        arch: "x64",
        release_base_url: None,
        on_progress,
        env,
    }
}

fn staging_dirs(base: &Path) -> Vec<String> {
    std::fs::read_dir(pinned_runtime_versions_dir(base))
        .map(|entries| {
            entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".staging-"))
                .collect()
        })
        .unwrap_or_default()
}

// ---- pinned runtime ----------------------------------------------------------------------------

#[tokio::test]
async fn the_verified_release_archive_becomes_the_runtime_executable() {
    let base = temp_dir("pinned-install");
    let fetcher = release_fetcher("1.1.0", "linux-x64", None);
    let runner = FakeRunner::new(|c, a| tar_writes_t3(c, a).unwrap_or_else(|| ok("")));
    let progress = Mutex::new(Vec::new());
    let on_progress = |p| progress.lock().unwrap().push(p);
    let env = no_env();
    let validator = RecordingValidator {
        seen: Mutex::new(vec![]),
        fail: false,
    };
    let paths = within(ensure_pinned_runtime_installed(install_input(
        base.path(),
        runner.as_ref(),
        fetcher.as_ref(),
        &validator,
        &env,
        Some(&on_progress),
    )))
    .await
    .unwrap();
    assert_eq!(paths, pinned_runtime_paths(base.path(), "1.1.0", "linux"));
    assert!(paths.entry_path.exists());
    assert_eq!(std::fs::read_to_string(&paths.sentinel_path).unwrap(), "1.1.0\n");
    assert!(staging_dirs(base.path()).is_empty());
    // The staged tree is validated before it is published, with the executable already in place.
    let seen = validator.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].0.file_name().unwrap().to_string_lossy().starts_with(".staging-"));
    assert!(seen[0].1);
    // tar strips the archive's top-level directory into the staging directory.
    let tar = runner.commands().into_iter().find(|c| c.starts_with("tar ")).unwrap();
    assert!(tar.contains("-xf ") && tar.ends_with("--strip-components=1"));
    assert_eq!(runner.timeout_of(&tar), Some(Duration::from_secs(600)));
    assert_eq!(
        fetcher.requested.lock().unwrap().clone(),
        [
            "https://github.com/pingdotgg/t3code/releases/download/v1.1.0/SHA256SUMS",
            "https://github.com/pingdotgg/t3code/releases/download/v1.1.0/t3-1.1.0-linux-x64.tar.gz"
        ]
    );
    let events = progress.lock().unwrap().clone();
    assert_eq!(events.first(), Some(&PinnedRuntimeProgress::Download { received: 0, total: None }));
    assert!(events.contains(&PinnedRuntimeProgress::Download {
        received: ARCHIVE_BYTES.len() as u64,
        total: Some(ARCHIVE_BYTES.len() as u64)
    }));
    let tail: Vec<_> = events
        .iter()
        .filter(|e| !matches!(e, PinnedRuntimeProgress::Download { .. }))
        .collect();
    assert_eq!(
        tail,
        [&PinnedRuntimeProgress::Verify, &PinnedRuntimeProgress::Extract, &PinnedRuntimeProgress::Validate]
    );

    // A complete install is reused without downloading, but still validated.
    let again_fetcher = release_fetcher("9.9.9", "linux-x64", None);
    let events = Mutex::new(Vec::new());
    let record = |p| events.lock().unwrap().push(p);
    within(ensure_pinned_runtime_installed(install_input(
        base.path(),
        runner.as_ref(),
        again_fetcher.as_ref(),
        &validator,
        &env,
        Some(&record),
    )))
    .await
    .unwrap();
    assert!(again_fetcher.requested.lock().unwrap().is_empty());
    assert_eq!(*events.lock().unwrap(), [PinnedRuntimeProgress::Cached]);
    assert_eq!(validator.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn an_archive_that_does_not_match_the_release_checksums_is_refused() {
    let base = temp_dir("pinned-checksum");
    let env = no_env();
    for (checksum, step) in [
        (Some("0".repeat(64)), "verifying the t3 release archive checksum"),
        (None, "finding t3-1.1.0-linux-x64.tar.gz in the t3 release checksums"),
    ] {
        let fetcher = if checksum.is_none() {
            // The checksum file lists a different archive.
            let mut fetcher = release_fetcher("1.1.0", "linux-arm64", None);
            Arc::get_mut(&mut fetcher).unwrap();
            fetcher
        } else {
            release_fetcher("1.1.0", "linux-x64", checksum)
        };
        let runner = FakeRunner::new(|_, _| ok(""));
        let error = within(ensure_pinned_runtime_installed(install_input(
            base.path(),
            runner.as_ref(),
            fetcher.as_ref(),
            &AcceptAll,
            &env,
            None,
        )))
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), format!("Pinned runtime install failed while {step}."));
        assert!(runner.commands().is_empty(), "nothing is extracted");
        assert!(!pinned_runtime_paths(base.path(), "1.1.0", "linux").version_dir.exists());
        assert!(staging_dirs(base.path()).is_empty());
    }
}

#[tokio::test]
async fn failures_name_the_step_and_leave_no_staging() {
    let base = temp_dir("pinned-failures");
    let env = no_env();
    // The archive cannot be selected for this platform.
    let fetcher = release_fetcher("1.1.0", "linux-x64", None);
    let runner = FakeRunner::new(|_, _| ok(""));
    let mut input = install_input(base.path(), runner.as_ref(), fetcher.as_ref(), &AcceptAll, &env, None);
    input.platform = "freebsd";
    assert_eq!(
        within(ensure_pinned_runtime_installed(input)).await.unwrap_err().to_string(),
        "Pinned runtime install failed while selecting a t3 release archive for freebsd-x64."
    );
    // A download that 404s.
    let empty = Arc::new(FakeFetcher {
        assets: BTreeMap::new(),
        requested: Mutex::new(vec![]),
        announce_length: false,
        hang_on_archive: AtomicBool::new(false),
        archive_started: tokio::sync::Notify::new(),
    });
    let error = within(ensure_pinned_runtime_installed(install_input(
        base.path(), runner.as_ref(), empty.as_ref(), &AcceptAll, &env, None,
    )))
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "Pinned runtime install failed while downloading the t3 release checksums.");
    // tar failing carries its exit code and output sizes, not its text.
    let failing_tar = FakeRunner::new(|c, _| if c == "tar" { exit(2) } else { ok("") });
    let error = within(ensure_pinned_runtime_installed(install_input(
        base.path(), failing_tar.as_ref(), fetcher.as_ref(), &AcceptAll, &env, None,
    )))
    .await
    .unwrap_err();
    assert_eq!(
        error,
        PinnedRuntimeError::Install {
            step: "extracting the t3 release archive".into(),
            exit_code: Some(2),
            stdout_length: Some(0),
            stderr_length: Some(3),
            cause: None,
        }
    );
    assert_eq!(
        error.to_string(),
        "Pinned runtime install failed while extracting the t3 release archive (exit code 2)."
    );
    // Validation failing removes staging and leaves no final runtime.
    let runner = FakeRunner::new(|c, a| tar_writes_t3(c, a).unwrap_or_else(|| ok("")));
    let validator = RecordingValidator {
        seen: Mutex::new(vec![]),
        fail: true,
    };
    within(ensure_pinned_runtime_installed(install_input(
        base.path(), runner.as_ref(), fetcher.as_ref(), &validator, &env, None,
    )))
    .await
    .unwrap_err();
    assert!(staging_dirs(base.path()).is_empty());
    assert!(!pinned_runtime_paths(base.path(), "1.1.0", "linux").version_dir.exists());
}

#[tokio::test]
async fn an_incomplete_runtime_is_replaced_and_a_completed_one_survives_failed_validation() {
    let base = temp_dir("pinned-incomplete");
    let env = no_env();
    let paths = pinned_runtime_paths(base.path(), "1.1.0", "linux");
    std::fs::create_dir_all(&paths.version_dir).unwrap();
    std::fs::write(&paths.entry_path, "half written").unwrap();
    std::fs::write(paths.version_dir.join("stale"), "x").unwrap();
    let fetcher = release_fetcher("1.1.0", "linux-x64", None);
    let runner = FakeRunner::new(|c, a| tar_writes_t3(c, a).unwrap_or_else(|| ok("")));
    within(ensure_pinned_runtime_installed(install_input(
        base.path(), runner.as_ref(), fetcher.as_ref(), &AcceptAll, &env, None,
    )))
    .await
    .unwrap();
    assert!(!paths.version_dir.join("stale").exists());
    assert_eq!(std::fs::read_to_string(&paths.entry_path).unwrap(), "#!/bin/sh\n");

    // The sentinel names this version: a validation failure now neither downloads nor deletes.
    let validator = RecordingValidator {
        seen: Mutex::new(vec![]),
        fail: true,
    };
    let quiet = release_fetcher("1.1.0", "linux-x64", None);
    within(ensure_pinned_runtime_installed(install_input(
        base.path(), runner.as_ref(), quiet.as_ref(), &validator, &env, None,
    )))
    .await
    .unwrap_err();
    assert!(quiet.requested.lock().unwrap().is_empty());
    assert!(paths.entry_path.exists() && paths.sentinel_path.exists());
}

/// Dropping an install mid-download (a cancelled request) still removes its staging directory.
#[tokio::test]
async fn an_interrupted_install_removes_its_staging_directory() {
    let base = temp_dir("pinned-interrupt");
    let env = no_env();
    let fetcher = release_fetcher("1.1.0", "linux-x64", None);
    fetcher.hang_on_archive.store(true, Ordering::SeqCst);
    let runner = FakeRunner::new(|_, _| ok(""));
    let install = ensure_pinned_runtime_installed(install_input(
        base.path(), runner.as_ref(), fetcher.as_ref(), &AcceptAll, &env, None,
    ));
    tokio::select! {
        _ = install => panic!("the download never finishes"),
        _ = fetcher.archive_started.notified() => {}
    }
    assert!(staging_dirs(base.path()).is_empty(), "dropped before the select returned");
    assert!(!pinned_runtime_paths(base.path(), "1.1.0", "linux").version_dir.exists());
    assert!(runner.commands().is_empty());
}

#[test]
fn pinned_runtime_paths_and_errors() {
    let paths = pinned_runtime_paths(Path::new("/h/.t3"), "1.2.3", "linux");
    assert_eq!(paths.entry_path, Path::new("/h/.t3/runtime/versions/1.2.3/t3"));
    assert_eq!(paths.sentinel_path, Path::new("/h/.t3/runtime/versions/1.2.3/.install-complete"));
    assert_eq!(pinned_runtime_paths(Path::new("/h"), "1.2.3", "win32").entry_path, Path::new("/h/runtime/versions/1.2.3/t3.exe"));
    assert_eq!(pinned_runtime_command(&paths), ("/h/.t3/runtime/versions/1.2.3/t3".to_owned(), vec![]));
    assert_eq!(
        PinnedRuntimeError::PreflightBlocked { version: "1".into(), reason: "why".into() }.to_string(),
        "why"
    );
}

// ---- launcher protocol files -----------------------------------------------------------------------

#[test]
fn the_installer_state_file_round_trips_through_the_decoder() {
    let text = ServiceState { active_version: "1.2.3".into(), update: None }.to_json_text();
    assert_eq!(text, "{\n  \"protocol\": 3,\n  \"activeVersion\": \"1.2.3\"\n}\n");
    assert_eq!(parse_service_state(&text).unwrap().active_version, "1.2.3");
    let pending = ServiceState {
        active_version: "1.0.0".into(),
        update: Some(ServiceUpdateRecord::Pending(PendingServiceUpdate {
            id: "u1".into(),
            from_version: "1.0.0".into(),
            target_version: "1.1.0".into(),
            db_path: "/db".into(),
        })),
    }
    .to_json_text();
    assert!(service_state_has_pending_update(&pending));
    assert_eq!(parse_service_state(&pending).unwrap().update.unwrap().id(), "u1");
}

// ---- update handoff ------------------------------------------------------------------------------------

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[tokio::test]
async fn the_desktop_marker_counts_once_and_only_while_fresh() {
    let base = temp_dir("handoff-desktop");
    let runtime = base.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let marker = runtime.join("desktop-update-restart");
    let at = |offset_ms: i64| FixedClock(now_ms() + offset_ms);
    let handoff = |clock: FixedClock| FsUpdateHandoff {
        base_dir: base.path().to_path_buf(),
        clock: Arc::new(clock),
    };
    assert!(!handoff(at(0)).desktop_update_restart_pending().await);

    std::fs::write(&marker, "").unwrap();
    assert!(handoff(at(30_000)).desktop_update_restart_pending().await);
    assert!(!marker.exists(), "reading consumes the marker");
    assert!(!handoff(at(30_000)).desktop_update_restart_pending().await);

    // A marker no server read (a hard kill) cannot keep the tunnel on a later quit.
    std::fs::write(&marker, "").unwrap();
    assert!(!handoff(at(61_000)).desktop_update_restart_pending().await);
    assert!(!marker.exists());
}

#[tokio::test]
async fn a_pending_update_is_a_handoff_unless_the_launcher_is_stopping() {
    let base = temp_dir("handoff-service");
    let runtime = base.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let handoff = FsUpdateHandoff {
        base_dir: base.path().to_path_buf(),
        clock: Arc::new(FixedClock(0)),
    };
    assert!(!handoff.pending_update_handoff_exists().await);
    let pending = ServiceState {
        active_version: "1.0.0".into(),
        update: Some(ServiceUpdateRecord::Pending(PendingServiceUpdate {
            id: "u1".into(),
            from_version: "1.0.0".into(),
            target_version: "1.1.0".into(),
            db_path: "/db".into(),
        })),
    };
    std::fs::write(runtime.join(SERVICE_STATE_FILE), pending.to_json_text()).unwrap();
    assert!(handoff.pending_update_handoff_exists().await);
    assert!(pending_service_update_exists(base.path()));
    // An explicit stop (`systemctl stop`, uninstall) marked by the launcher is not a handoff.
    std::fs::write(runtime.join(SERVICE_STOP_MARKER_FILE), "").unwrap();
    assert!(!handoff.pending_update_handoff_exists().await);
    assert!(pending_service_update_exists(base.path()));
    // A finished update is not pending.
    std::fs::remove_file(runtime.join(SERVICE_STOP_MARKER_FILE)).unwrap();
    std::fs::write(
        runtime.join(SERVICE_STATE_FILE),
        ServiceState { active_version: "1.2.3".into(), update: None }.to_json_text(),
    )
    .unwrap();
    assert!(!handoff.pending_update_handoff_exists().await);
}

// ---- launcher client ----------------------------------------------------------------------------------

struct FakeHost {
    connected: bool,
    sent: Mutex<Vec<ServiceLauncherChildMessage>>,
    events: tokio::sync::broadcast::Sender<LauncherEvent>,
    fail_send: bool,
}

impl FakeHost {
    fn new(connected: bool) -> Arc<Self> {
        Arc::new(Self {
            connected,
            sent: Mutex::new(vec![]),
            events: tokio::sync::broadcast::channel(16).0,
            fail_send: false,
        })
    }

    fn emit(&self, message: &ServiceLauncherParentMessage) {
        let _ = self.events.send(LauncherEvent::Message(serde_json::to_value(message).unwrap()));
    }
}

impl ServiceLauncherHost for FakeHost {
    fn connected(&self) -> bool {
        self.connected
    }

    fn send(&self, message: &ServiceLauncherChildMessage) -> Result<(), String> {
        if self.fail_send {
            return Err("broken pipe".into());
        }
        self.sent.lock().unwrap().push(message.clone());
        Ok(())
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<LauncherEvent> {
        self.events.subscribe()
    }
}

fn context_env(value: serde_json::Value) -> BTreeMap<String, String> {
    BTreeMap::from([(SERVICE_LAUNCHER_CONTEXT_ENV.to_owned(), value.to_string())])
}

fn pending_json() -> serde_json::Value {
    json!({"id": "update-1", "fromVersion": "1.0.0", "targetVersion": "1.1.0", "dbPath": "/db", "status": "pending"})
}

#[tokio::test]
async fn a_trial_child_waits_for_the_launcher_to_commit_its_update_id() {
    let host = FakeHost::new(true);
    let env = context_env(json!({"protocol": 3, "childVersion": "1.1.0", "update": pending_json()}));
    let client = ServiceLauncherClient::new(host.clone(), &env, "1.1.0").unwrap();
    assert!(client.managed());
    let trial = tokio::spawn(async move { client.prepare_trial().await });
    // Only the commit for this update id counts.
    while host.sent.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    host.emit(&ServiceLauncherParentMessage::Committed { update_id: "someone-else".into() });
    host.emit(&ServiceLauncherParentMessage::Committed { update_id: "update-1".into() });
    let outcome = within(trial).await.unwrap().unwrap().unwrap();
    assert_eq!(
        host.sent.lock().unwrap().clone(),
        [ServiceLauncherChildMessage::Prepared { update_id: "update-1".into() }]
    );
    assert_eq!(
        (outcome.id.as_str(), outcome.status, outcome.from_version.as_str(), outcome.target_version.as_str()),
        ("update-1", ServiceUpdateStatus::Committed, "1.0.0", "1.1.0")
    );
}

#[tokio::test]
async fn an_update_request_returns_the_launchers_id_only_after_acceptance() {
    let host = FakeHost::new(true);
    let env = context_env(json!({"protocol": 3, "childVersion": "1.0.0"}));
    let client = Arc::new(ServiceLauncherClient::new(host.clone(), &env, "1.0.0").unwrap());
    for reply in [
        ServiceLauncherParentMessage::UpdateAccepted { update_id: "launcher-id".into() },
        ServiceLauncherParentMessage::UpdateRejected { reason: "requires local update".into() },
    ] {
        let request = tokio::spawn({
            let client = client.clone();
            async move { client.request_update("1.1.0", "/db").await }
        });
        while host.sent.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        // A reply for another exchange is ignored.
        host.emit(&ServiceLauncherParentMessage::Committed { update_id: "x".into() });
        host.emit(&reply);
        let result = within(request).await.unwrap();
        match reply {
            ServiceLauncherParentMessage::UpdateAccepted { .. } => assert_eq!(result.unwrap(), "launcher-id"),
            _ => {
                let error = result.unwrap_err();
                assert_eq!(error.to_string(), "requires local update");
                assert!(matches!(error, RequestUpdateError::Rejected(r) if r.target_version == "1.1.0"));
            }
        }
        assert_eq!(
            host.sent.lock().unwrap().pop().unwrap(),
            ServiceLauncherChildMessage::RequestUpdate { target_version: "1.1.0".into(), db_path: "/db".into() }
        );
    }
}

#[test]
fn contradictory_launcher_context_is_an_error_not_an_unmanaged_start() {
    let host = FakeHost::new(true);
    let err = |env: &BTreeMap<String, String>, connected: bool, version: &str| {
        resolve_service_launcher_startup(env, connected, version).unwrap_err()
    };
    // A pending update selects the target version as the child's.
    let pending = context_env(json!({"protocol": 3, "childVersion": "1.0.0", "update": pending_json()}));
    assert_eq!(err(&pending, true, "1.0.0").operation, ServiceLauncherOperation::DecodeContext);
    assert_eq!(
        err(&BTreeMap::from([(SERVICE_LAUNCHER_CONTEXT_ENV.to_owned(), "garbage".to_owned())]), true, "1.0.0").to_string(),
        "The service launcher supplied invalid startup context."
    );
    let good = context_env(json!({"protocol": 3, "childVersion": "1.0.0"}));
    assert_eq!(err(&good, true, "2.0.0").to_string(), "The service launcher started a different t3 version.");
    assert_eq!(err(&good, false, "1.0.0").to_string(), "The service launcher IPC channel is unavailable.");
    // No context: unmanaged, not an error; asking the launcher anything then fails.
    let startup = resolve_service_launcher_startup(&BTreeMap::new(), false, "1.0.0").unwrap();
    assert_eq!(startup, ServiceLauncherStartup { context: None, managed: false });
    assert!(!resolve_service_launcher_mode(&BTreeMap::new(), true, "1.0.0").unwrap());
    assert!(resolve_service_launcher_mode(&good, true, "1.0.0").unwrap());
    let client = ServiceLauncherClient::new(host, &BTreeMap::new(), "1.0.0").unwrap();
    assert!(!client.managed());
    let unmanaged = tokio::runtime::Builder::new_current_thread().build().unwrap()
        .block_on(client.request_update("1.1.0", "/db")).unwrap_err();
    assert_eq!(unmanaged.to_string(), "This server is not managed by the launcher.");
}

#[tokio::test(start_paused = true)]
async fn an_exchange_fails_on_disconnect_send_errors_and_silence() {
    let env = context_env(json!({"protocol": 3, "childVersion": "1.0.0"}));
    let host = FakeHost::new(true);
    let client = Arc::new(ServiceLauncherClient::new(host.clone(), &env, "1.0.0").unwrap());
    let request = tokio::spawn({
        let client = client.clone();
        async move { client.request_update("1.1.0", "/db").await }
    });
    while host.sent.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    let _ = host.events.send(LauncherEvent::Disconnect);
    assert_eq!(
        within(request).await.unwrap().unwrap_err().to_string(),
        "The service launcher disconnected before acknowledging the request."
    );
    // Silence for 30 seconds is a timeout.
    let silent = within(client.request_update("1.1.0", "/db")).await.unwrap_err();
    assert_eq!(silent.to_string(), "The service launcher did not respond within 30 seconds.");
    let broken = Arc::new(FakeHost {
        fail_send: true,
        ..Arc::into_inner(FakeHost::new(true)).unwrap()
    });
    let client = ServiceLauncherClient::new(broken, &env, "1.0.0").unwrap();
    let error = within(client.request_update("1.1.0", "/db")).await.unwrap_err();
    assert_eq!(error.to_string(), "Could not send a request to the service launcher.");
    // A finished update recorded in the context is reported as the outcome without any exchange.
    let finished = context_env(json!({
        "protocol": 3, "childVersion": "1.0.0",
        "update": {"id": "u", "fromVersion": "1.0.0", "targetVersion": "1.1.0", "status": "rolled-back", "reason": "boom"}
    }));
    let client = ServiceLauncherClient::new(FakeHost::new(true), &finished, "1.0.0").unwrap();
    let outcome = within(client.prepare_trial()).await.unwrap().unwrap();
    assert_eq!((outcome.status, outcome.reason.as_deref()), (ServiceUpdateStatus::RolledBack, Some("boom")));
}

// ---- self update ------------------------------------------------------------------------------------------

struct NoDesktop;

#[async_trait]
impl DesktopAppUpdate for NoDesktop {
    fn available(&self) -> bool {
        false
    }

    async fn run(&self, _report: ProgressReporter) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        panic!("unexpected desktop app update run");
    }

    async fn commit(&self, _id: &str, _accepted: HandoffCallback) -> Result<Infallible, ServerSelfUpdateError> {
        std::future::pending().await
    }
}

struct DesktopFeed;

#[async_trait]
impl DesktopAppUpdate for DesktopFeed {
    fn available(&self) -> bool {
        true
    }

    async fn run(&self, report: ProgressReporter) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        report(ServerSelfUpdateProgressStage::Downloading).await?;
        report(ServerSelfUpdateProgressStage::Installing).await?;
        Ok(ServerSelfUpdateResult {
            target_version: "1.2.0".into(),
            method: ServerSelfUpdateMethod::DesktopApp,
            update_id: None,
            desktop_update_token: None,
        })
    }

    async fn commit(&self, _id: &str, _accepted: HandoffCallback) -> Result<Infallible, ServerSelfUpdateError> {
        std::future::pending().await
    }
}

struct SelfUpdateHarness {
    runner: Arc<FakeRunner>,
    update: ServerSelfUpdate,
    order: Arc<Mutex<Vec<String>>>,
    host: Arc<FakeHost>,
    _base: tempfile::TempDir,
}

fn harness(desktop_mode: bool, managed: bool, preflight_blocked: bool, desktop: Arc<dyn DesktopAppUpdate>) -> SelfUpdateHarness {
    let base = temp_dir("self-update");
    let order: Arc<Mutex<Vec<String>>> = Arc::default();
    let runner = {
        let order = order.clone();
        FakeRunner::new(move |command, args| {
            if let Some(result) = tar_writes_t3(command, args) {
                order.lock().unwrap().push("extract".into());
                return result;
            }
            order.lock().unwrap().push("preflight".into());
            if preflight_blocked {
                ok(&json!({"status": "blocked", "version": "1.1.0", "reason": "local update required"}).to_string())
            } else {
                ok(&json!({"status": "ready", "version": "1.1.0", "launcherProtocol": 3}).to_string())
            }
        })
    };
    let fetcher = release_fetcher("1.1.0", "linux-x64", None);
    let host = FakeHost::new(true);
    let env = if managed {
        context_env(json!({"protocol": 3, "childVersion": "1.0.0"}))
    } else {
        BTreeMap::new()
    };
    let launcher = Arc::new(ServiceLauncherClient::new(host.clone(), &env, "1.0.0").unwrap());
    let update = ServerSelfUpdate::new(
        ServerSelfUpdateConfig {
            desktop_mode,
            base_dir: base.path().to_path_buf(),
            db_path: "/db/state.sqlite".into(),
            platform: "linux".into(),
            arch: "x64".into(),
            release_base_url: None,
            env: BTreeMap::new(),
        },
        desktop,
        launcher,
        runner.clone(),
        fetcher,
    );
    SelfUpdateHarness { runner, update, order, host, _base: base }
}

fn input(version: &str) -> ServerSelfUpdateInput {
    ServerSelfUpdateInput {
        target_version: version.into(),
        continue_running_threads: None,
    }
}

/// Answers the first `request-update` the launcher sees.
fn accept_when_asked(host: Arc<FakeHost>, order: Arc<Mutex<Vec<String>>>) {
    tokio::spawn(async move {
        while host.sent.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        order.lock().unwrap().push("accept".into());
        host.emit(&ServiceLauncherParentMessage::UpdateAccepted { update_id: "launcher-id".into() });
    });
}

#[tokio::test]
async fn an_update_is_staged_and_preflighted_before_the_launcher_is_asked() {
    let h = harness(false, true, false, Arc::new(NoDesktop));
    assert_eq!(h.update.capability(), Some(ServerSelfUpdateCapability::BootService));
    accept_when_asked(h.host.clone(), h.order.clone());
    let stages = Arc::new(Mutex::new(Vec::new()));
    let report: ProgressReporter = {
        let stages = stages.clone();
        Arc::new(move |stage| {
            stages.lock().unwrap().push(stage);
            Box::pin(async { Ok(()) })
        })
    };
    let accepted = Arc::new(AtomicBool::new(false));
    let flag = accepted.clone();
    let result = within(h.update.update(input(" 1.1.0 "), report, Arc::new(move || flag.store(true, Ordering::SeqCst))))
        .await
        .unwrap();
    assert_eq!(
        result,
        ServerSelfUpdateResult {
            target_version: "1.1.0".into(),
            method: ServerSelfUpdateMethod::BootService,
            update_id: Some("launcher-id".into()),
            desktop_update_token: None,
        }
    );
    assert_eq!(*h.order.lock().unwrap(), ["extract", "preflight", "accept"]);
    assert_eq!(
        *stages.lock().unwrap(),
        [ServerSelfUpdateProgressStage::Downloading, ServerSelfUpdateProgressStage::Installing]
    );
    assert!(accepted.load(Ordering::SeqCst));
    // The preflight runs the staged executable against this server's database and launcher protocol.
    let preflight = h
        .runner
        .commands()
        .into_iter()
        .find(|c| c.contains("__service-preflight"))
        .unwrap();
    assert!(preflight.contains("/runtime/versions/.staging-"), "the staged executable is preflighted, not the published one");
    assert!(preflight.ends_with("/t3 __service-preflight --database-path /db/state.sqlite --launcher-protocol 3"));
    assert_eq!(h.runner.timeout_of(&preflight), Some(Duration::from_secs(30)));
}

#[tokio::test]
async fn invalid_versions_and_unmanaged_servers_are_refused_before_staging() {
    let web = harness(false, true, false, Arc::new(NoDesktop));
    let error = within(web.update.update(input("latest"), silent_progress(), no_handoff_callback())).await.unwrap_err();
    assert_eq!(error.reason, "'latest' is not an exact t3 version.");
    assert_eq!(error.to_string(), "Server update failed: 'latest' is not an exact t3 version.");
    let desktop = harness(true, false, false, Arc::new(NoDesktop));
    let error = within(desktop.update.update(input("1.1.0"), silent_progress(), no_handoff_callback())).await.unwrap_err();
    assert!(error.reason.contains("desktop app"));
    let bare = harness(false, false, false, Arc::new(NoDesktop));
    assert_eq!(bare.update.capability(), None);
    let error = within(bare.update.update(input("1.1.0"), silent_progress(), no_handoff_callback())).await.unwrap_err();
    assert_eq!(
        error.reason,
        "Remote updates require the T3 Code background service. Run `t3 service install` on the server machine."
    );
    assert!(web.order.lock().unwrap().is_empty() && desktop.order.lock().unwrap().is_empty() && bare.order.lock().unwrap().is_empty());
}

#[tokio::test]
async fn desktop_managed_updates_go_to_the_desktop_app_when_it_can() {
    let h = harness(true, false, false, Arc::new(DesktopFeed));
    assert_eq!(h.update.capability(), Some(ServerSelfUpdateCapability::DesktopManaged));
    let stages = Arc::new(Mutex::new(Vec::new()));
    let report: ProgressReporter = {
        let stages = stages.clone();
        Arc::new(move |stage| {
            stages.lock().unwrap().push(stage);
            Box::pin(async { Ok(()) })
        })
    };
    let result = within(h.update.update(input("1.1.0"), report, no_handoff_callback())).await.unwrap();
    assert_eq!((result.target_version.as_str(), result.method), ("1.2.0", ServerSelfUpdateMethod::DesktopApp));
    assert_eq!(stages.lock().unwrap().len(), 2);
    assert!(h.order.lock().unwrap().is_empty(), "the launcher staging path must not run");
}

#[tokio::test]
async fn a_preflight_refusal_keeps_its_reason() {
    let h = harness(false, true, true, Arc::new(NoDesktop));
    let error = within(h.update.update(input("1.1.0"), silent_progress(), no_handoff_callback())).await.unwrap_err();
    assert_eq!(error.reason, "local update required");
    // The slot is free again, so a retry is accepted.
    let error = within(h.update.update(input("1.1.0"), silent_progress(), no_handoff_callback())).await.unwrap_err();
    assert_eq!(error.reason, "local update required");
}

#[tokio::test]
async fn only_one_update_runs_at_a_time() {
    let h = harness(false, true, false, Arc::new(NoDesktop));
    let update = Arc::new(h.update);
    let first = tokio::spawn({
        let update = update.clone();
        async move { update.update(input("1.1.0"), silent_progress(), no_handoff_callback()).await }
    });
    while h.host.sent.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    let error = update.update(input("1.1.1"), silent_progress(), no_handoff_callback()).await.unwrap_err();
    assert_eq!(error.reason, "A server update is already in progress.");
    h.host.emit(&ServiceLauncherParentMessage::UpdateAccepted { update_id: "launcher-id".into() });
    assert_eq!(within(first).await.unwrap().unwrap().update_id.as_deref(), Some("launcher-id"));
}

#[tokio::test]
async fn the_launcher_rejecting_an_update_is_shown_with_its_reason() {
    let h = harness(false, true, false, Arc::new(NoDesktop));
    let update = Arc::new(h.update);
    let request = tokio::spawn({
        let update = update.clone();
        async move { update.update(input("1.1.0"), silent_progress(), no_handoff_callback()).await }
    });
    while h.host.sent.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    h.host.emit(&ServiceLauncherParentMessage::UpdateRejected { reason: "requires local update".into() });
    assert_eq!(within(request).await.unwrap().unwrap_err().reason, "requires local update");
}

// ---- continuing running threads ------------------------------------------------------------------------

struct ScriptedUpdate {
    events: Arc<Mutex<Vec<String>>>,
    method: ServerSelfUpdateMethod,
    /// `commit` calls the handoff callback, then fails with this (or never returns when `None`).
    commit_error: Option<ServerSelfUpdateError>,
    update_error: Option<ServerSelfUpdateError>,
    accept_before_hanging: bool,
}

#[async_trait]
impl SelfUpdateService for ScriptedUpdate {
    async fn update(
        &self,
        _input: ServerSelfUpdateInput,
        report: ProgressReporter,
        accepted: HandoffCallback,
    ) -> Result<ServerSelfUpdateResult, ServerSelfUpdateError> {
        report(ServerSelfUpdateProgressStage::Downloading).await?;
        report(ServerSelfUpdateProgressStage::Installing).await?;
        if self.accept_before_hanging {
            accepted();
            std::future::pending::<()>().await;
        }
        if let Some(error) = &self.update_error {
            return Err(error.clone());
        }
        Ok(ServerSelfUpdateResult {
            target_version: "1.1.0".into(),
            method: self.method,
            update_id: Some("update-id".into()),
            desktop_update_token: (self.method == ServerSelfUpdateMethod::DesktopApp).then(|| "desktop-token".into()),
        })
    }

    async fn commit_desktop_update(&self, _id: &str, accepted: HandoffCallback) -> Result<Infallible, ServerSelfUpdateError> {
        self.events.lock().unwrap().push("commit".into());
        match &self.commit_error {
            Some(error) => Err(error.clone()),
            None => {
                accepted();
                std::future::pending().await
            }
        }
    }
}

fn continuation(
    desktop_mode: bool,
    inner: ScriptedUpdate,
    events: Arc<Mutex<Vec<String>>>,
    clear_error: Option<ServerSelfUpdateError>,
) -> RunningThreadContinuation<ScriptedUpdate> {
    let (prepare_events, clear_events) = (events.clone(), events);
    RunningThreadContinuation::new(
        desktop_mode,
        Arc::new(inner),
        Arc::new(move || -> BoxFuture<'static, _> {
            let events = prepare_events.clone();
            Box::pin(async move {
                events.lock().unwrap().push("prepare".into());
                Ok(vec!["thread-running".to_owned()])
            })
        }),
        Arc::new(move |ids: Vec<String>| -> BoxFuture<'static, _> {
            let (events, error) = (clear_events.clone(), clear_error.clone());
            Box::pin(async move {
                events.lock().unwrap().push(format!("clear:{}", ids.join(",")));
                error.map_or(Ok(()), Err)
            })
        }),
    )
}

fn scripted(events: &Arc<Mutex<Vec<String>>>, method: ServerSelfUpdateMethod) -> ScriptedUpdate {
    ScriptedUpdate {
        events: events.clone(),
        method,
        commit_error: None,
        update_error: None,
        accept_before_hanging: false,
    }
}

fn stage_recorder(events: &Arc<Mutex<Vec<String>>>) -> ProgressReporter {
    let events = events.clone();
    Arc::new(move |stage| {
        events.lock().unwrap().push(format!("{stage:?}").to_lowercase());
        Box::pin(async { Ok(()) })
    })
}

fn continue_input() -> ServerSelfUpdateInput {
    ServerSelfUpdateInput {
        target_version: "1.1.0".into(),
        continue_running_threads: Some(true),
    }
}

#[tokio::test]
async fn running_threads_are_marked_at_the_boot_service_handoff() {
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let wrapped = continuation(false, scripted(&events, ServerSelfUpdateMethod::BootService), events.clone(), None);
    within(wrapped.update(continue_input(), stage_recorder(&events), no_handoff_callback())).await.unwrap();
    assert_eq!(*events.lock().unwrap(), ["downloading", "prepare", "installing"]);
}

#[tokio::test]
async fn desktop_threads_are_marked_only_when_the_prepared_update_commits() {
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let commit_error = ServerSelfUpdateError::new("install failed");
    let mut inner = scripted(&events, ServerSelfUpdateMethod::DesktopApp);
    inner.commit_error = Some(commit_error.clone());
    let wrapped = continuation(true, inner, events.clone(), None);
    within(wrapped.update(continue_input(), stage_recorder(&events), no_handoff_callback())).await.unwrap();
    assert_eq!(*events.lock().unwrap(), ["downloading", "installing"]);
    for round in 0..2 {
        let error = within(wrapped.commit_desktop_update("desktop-token", no_handoff_callback())).await.unwrap_err();
        assert_eq!(error, commit_error);
        // Each failed commit prepares, commits, clears — and the next one starts over.
        let tail: Vec<String> = events.lock().unwrap().iter().skip(2 + round * 3).cloned().collect();
        assert_eq!(tail, ["prepare", "commit", "clear:thread-running"]);
    }
}

#[tokio::test]
async fn a_failed_marker_cleanup_replaces_the_update_error() {
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut inner = scripted(&events, ServerSelfUpdateMethod::BootService);
    inner.update_error = Some(ServerSelfUpdateError::new("update failed"));
    let wrapped = continuation(false, inner, events.clone(), Some(ServerSelfUpdateError::new("marker cleanup failed")));
    let error = within(wrapped.update(continue_input(), silent_progress(), no_handoff_callback())).await.unwrap_err();
    assert_eq!(error.reason, "marker cleanup failed");
}

#[tokio::test(start_paused = true)]
async fn markers_are_kept_once_the_handoff_was_accepted_and_cleared_before_it() {
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    // Interrupted (dropped) after the launcher accepted the handoff: the replacement resumes them.
    let mut inner = scripted(&events, ServerSelfUpdateMethod::BootService);
    inner.accept_before_hanging = true;
    let wrapped = continuation(false, inner, events.clone(), None);
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        wrapped.update(continue_input(), silent_progress(), no_handoff_callback()),
    )
    .await;
    assert!(outcome.is_err(), "interrupted");
    tokio::task::yield_now().await;
    assert_eq!(*events.lock().unwrap(), ["prepare"]);

    // Interrupted before any handoff: the marks are removed again.
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut inner = scripted(&events, ServerSelfUpdateMethod::BootService);
    inner.accept_before_hanging = false;
    let report: ProgressReporter = {
        let events = events.clone();
        Arc::new(move |stage| {
            let events = events.clone();
            Box::pin(async move {
                events.lock().unwrap().push(format!("{stage:?}"));
                if stage == ServerSelfUpdateProgressStage::Installing {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        })
    };
    let wrapped = continuation(false, inner, events.clone(), None);
    assert!(tokio::time::timeout(Duration::from_secs(5), wrapped.update(continue_input(), report, no_handoff_callback()))
        .await
        .is_err());
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    assert!(events.lock().unwrap().contains(&"clear:thread-running".to_owned()));
}

#[tokio::test]
async fn desktop_markers_survive_an_accepted_commit_handoff() {
    let events: Arc<Mutex<Vec<String>>> = Arc::default();
    let wrapped = continuation(true, scripted(&events, ServerSelfUpdateMethod::DesktopApp), events.clone(), None);
    within(wrapped.update(continue_input(), silent_progress(), no_handoff_callback())).await.unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_millis(50),
        wrapped.commit_desktop_update("desktop-token", no_handoff_callback()),
    )
    .await;
    assert!(outcome.is_err(), "the commit hands off and never returns");
    tokio::task::yield_now().await;
    assert_eq!(*events.lock().unwrap(), ["prepare", "commit"]);
}

// ---- the boot service ------------------------------------------------------------------------------------

struct Control {
    fail_command: Mutex<Option<String>>,
    linger: Mutex<String>,
    enabled: AtomicBool,
    active: AtomicBool,
    state_after_stop: Mutex<Option<(std::path::PathBuf, String)>>,
}

struct ServiceHarness {
    service: BootService,
    home: tempfile::TempDir,
    base: std::path::PathBuf,
    runner: Arc<FakeRunner>,
    control: Arc<Control>,
}

fn service_config(home: &Path, platform: &str, version: &str, installer_path: &str) -> BootServiceConfig {
    let base = home.join(".t3");
    BootServiceConfig {
        base_dir: base.clone(),
        logs_dir: base.join("userdata").join("logs"),
        cli_version: version.into(),
        exec_path: "/usr/bin/t3".into(),
        platform: platform.into(),
        arch: "x64".into(),
        uid: Some(501),
        home_dir: home.to_string_lossy().into_owned(),
        installer_path: installer_path.into(),
        env: BTreeMap::new(),
    }
}

/// Makes the pinned runtime for `version` present, so install only validates it.
fn preinstall(config: &BootServiceConfig, version: &str) {
    let paths = pinned_runtime_paths(&config.base_dir, version, &config.platform);
    std::fs::create_dir_all(&paths.version_dir).unwrap();
    std::fs::write(&paths.entry_path, "#!/bin/sh\n").unwrap();
    std::fs::write(&paths.sentinel_path, format!("{version}\n")).unwrap();
}

fn service_harness(platform: &str) -> ServiceHarness {
    let home = temp_dir("boot-service");
    let config = service_config(home.path(), platform, "1.2.3", "/Users/theo/Tools:/usr/bin");
    preinstall(&config, "1.2.3");
    let control = Arc::new(Control {
        fail_command: Mutex::new(None),
        linger: Mutex::new("yes".into()),
        enabled: AtomicBool::new(true),
        active: AtomicBool::new(true),
        state_after_stop: Mutex::new(None),
    });
    let runner = {
        let control = control.clone();
        FakeRunner::new(move |command, args| {
            let line = format!("{command} {}", args.join(" "));
            let failed = control.fail_command.lock().unwrap().as_deref() == Some(line.as_str());
            if !failed && line == "loginctl enable-linger --no-ask-password 501" {
                *control.linger.lock().unwrap() = "yes".into();
            }
            if !failed && line == "systemctl --user enable t3code.service" {
                control.enabled.store(true, Ordering::SeqCst);
            }
            if !failed && line == "systemctl --user restart t3code.service" {
                control.active.store(true, Ordering::SeqCst);
            }
            if line == "systemctl --user stop t3code.service" || line.starts_with("launchctl bootout --wait ") {
                if let Some((path, text)) = control.state_after_stop.lock().unwrap().clone() {
                    std::fs::write(path, text).unwrap();
                }
            }
            let stdout = if args.first().map(String::as_str) == Some("--version") {
                // The runtime under test reports the version of the directory it was launched from.
                let version = command.split("/versions/").nth(1).and_then(|r| r.split('/').next()).unwrap_or("1.2.3");
                format!("t3 v{version}\n")
            } else if command == "loginctl" && args.first().map(String::as_str) == Some("show-user") {
                format!("{}\n", control.linger.lock().unwrap())
            } else if args.get(1).map(String::as_str) == Some("is-enabled") {
                if control.enabled.load(Ordering::SeqCst) { "enabled\n" } else { "disabled\n" }.to_owned()
            } else {
                String::new()
            };
            let inactive = args.get(1).map(String::as_str) == Some("is-active") && !control.active.load(Ordering::SeqCst);
            Ok(ProcessRunOutput {
                code: i32::from(failed || inactive),
                stdout,
                stderr: String::new(),
            })
        })
    };
    let base = config.base_dir.clone();
    ServiceHarness {
        service: BootService::new(config, runner.clone(), release_fetcher("0.0.0", "linux-x64", None)),
        home,
        base,
        runner,
        control,
    }
}

fn service_for(h: &ServiceHarness, version: &str, base: Option<&Path>) -> BootService {
    let mut config = service_config(h.home.path(), "linux", version, "/Users/theo/Tools:/usr/bin");
    if let Some(base) = base {
        config.base_dir = base.to_path_buf();
        config.logs_dir = base.join("userdata").join("logs");
    }
    preinstall(&config, version);
    BootService::new(config, h.runner.clone(), release_fetcher("0.0.0", "linux-x64", None))
}

fn state_path(base: &Path) -> std::path::PathBuf {
    base.join("runtime").join(SERVICE_STATE_FILE)
}

#[tokio::test]
async fn install_is_refused_while_lingering_needs_an_administrator() {
    let h = service_harness("linux");
    let before = h.service.status().await.unwrap();
    *h.control.linger.lock().unwrap() = "no".into();
    *h.control.fail_command.lock().unwrap() = Some("loginctl enable-linger --no-ask-password 501".into());
    std::fs::remove_file(pinned_runtime_paths(&h.base, "1.2.3", "linux").sentinel_path).unwrap();

    let error = h.service.install(Default::default()).await.unwrap_err();
    assert!(matches!(
        error,
        BootServiceError::Prerequisite { problem: BootServiceProblem::LingerDisabled, .. }
    ));
    let message = error.to_string();
    assert!(message.contains("sudo loginctl enable-linger \"$(id -un)\"") && message.contains("last login session ends"));
    assert!(!Path::new(&before.unit_path).exists());
    assert!(!state_path(&h.base).exists());
    assert!(!h.runner.commands().iter().any(|c| c.contains("--version")), "no runtime was validated");
    assert!(!h.runner.commands().iter().any(|c| c.contains("daemon-reload") || c.contains("restart")));
    assert!(std::fs::read_to_string(&before.log_path).unwrap().contains("[linger-disabled]"));
}

#[tokio::test]
async fn a_partial_install_is_detected_and_a_running_service_survives_a_failed_repair() {
    let h = service_harness("linux");
    let plan = h.service.install(Default::default()).await.unwrap();
    let state_before = std::fs::read_to_string(state_path(&h.base)).unwrap();
    let unit_before = std::fs::read_to_string(&plan.unit_path).unwrap();
    *h.control.linger.lock().unwrap() = "no".into();
    *h.control.fail_command.lock().unwrap() = Some("loginctl enable-linger --no-ask-password 501".into());
    let status = h.service.status().await.unwrap();
    assert!(!status.current);
    assert_eq!(status.problems, [BootServiceProblem::LingerDisabled]);
    assert!(matches!(h.service.install(Default::default()).await, Err(BootServiceError::Prerequisite { .. })));
    assert_eq!(std::fs::read_to_string(state_path(&h.base)).unwrap(), state_before);
    assert_eq!(std::fs::read_to_string(&plan.unit_path).unwrap(), unit_before);
    assert!(!h.runner.commands().iter().any(|c| c == "systemctl --user stop t3code.service"));
}

#[tokio::test]
async fn lingering_is_enabled_before_installing_and_stopped_services_are_repaired() {
    let h = service_harness("linux");
    *h.control.linger.lock().unwrap() = "no".into();
    h.service.install(Default::default()).await.unwrap();
    assert_eq!(*h.control.linger.lock().unwrap(), "yes");
    let commands = h.runner.commands();
    let at = |c: &str| commands.iter().position(|x| x == c).unwrap();
    assert!(at("loginctl enable-linger --no-ask-password 501") < at("systemctl --user daemon-reload"));

    h.control.enabled.store(false, Ordering::SeqCst);
    h.control.active.store(false, Ordering::SeqCst);
    let status = h.service.status().await.unwrap();
    assert!(!status.current);
    assert_eq!(status.problems, [BootServiceProblem::ServiceDisabled, BootServiceProblem::ServiceStopped]);
    h.service.install(Default::default()).await.unwrap();
    assert!(h.service.status().await.unwrap().current);
}

#[tokio::test]
async fn failed_prerequisite_probes_install_nothing() {
    for (command, problem) in [
        ("systemctl --user show-environment", BootServiceProblem::UserManagerUnavailable),
        ("loginctl show-user 501 --property=Linger --value", BootServiceProblem::LingerUnavailable),
    ] {
        let h = service_harness("linux");
        *h.control.fail_command.lock().unwrap() = Some(command.into());
        let error = h.service.install(Default::default()).await.unwrap_err();
        assert!(matches!(error, BootServiceError::Prerequisite { problem: p, .. } if p == problem), "{command}");
        assert!(!state_path(&h.base).exists());
    }
}

#[tokio::test]
async fn install_reports_current_state_and_uninstalls() {
    let h = service_harness("linux");
    let plan = h.service.install(Default::default()).await.unwrap();
    let commands = h.runner.commands();
    assert_eq!(
        parse_service_state(&std::fs::read_to_string(state_path(&h.base)).unwrap()).unwrap().active_version,
        "1.2.3"
    );
    assert_eq!(plan.program, [pinned_runtime_paths(&h.base, "1.2.3", "linux").entry_path.to_string_lossy().into_owned(), "__service-launcher".to_owned()]);
    let unit = std::fs::read_to_string(&plan.unit_path).unwrap();
    assert!(unit.contains("__service-launcher") && unit.contains("OOMPolicy=continue"));
    assert!(plan.unit_path.ends_with(".config/systemd/user/t3code.service"));
    assert_eq!(boot_service_base_dir_of(&unit).as_deref(), Some(h.base.to_str().unwrap()));
    let status = h.service.status().await.unwrap();
    assert!(status.supported && status.installed && status.current);
    assert_eq!(status.installed_version.as_deref(), Some("1.2.3"));
    assert_eq!(status.installed_base_dir.as_deref(), Some(h.base.to_str().unwrap()));
    assert!(status.problems.is_empty());
    assert!(status.log_path.ends_with("userdata/logs/boot-service.log"));
    assert!(commands.iter().any(|c| c.ends_with("--version")));
    assert!(commands.ends_with(&[
        "systemctl --user daemon-reload".to_owned(),
        "systemctl --user enable t3code.service".to_owned(),
        "systemctl --user restart t3code.service".to_owned(),
    ]));

    // A second install stops the installed unit first, with the long stop timeout.
    h.runner.commands.lock().unwrap().clear();
    h.service.install(Default::default()).await.unwrap();
    assert_eq!(h.runner.commands().iter().find(|c| c.contains("stop")).unwrap(), "systemctl --user stop t3code.service");
    assert_eq!(h.runner.timeout_of("systemctl --user stop t3code.service"), Some(Duration::from_secs(120)));

    assert!(h.service.uninstall().await.unwrap());
    assert!(!Path::new(&plan.unit_path).exists());
    assert!(h.runner.commands().ends_with(&[
        "systemctl --user disable --now t3code.service".to_owned(),
        "systemctl --user daemon-reload".to_owned(),
    ]));
    assert!(!h.service.uninstall().await.unwrap());
    assert!(!h.service.status().await.unwrap().installed);
}

#[tokio::test]
async fn downgrades_are_refused_without_an_override() {
    let h = service_harness("linux");
    let newer = service_for(&h, "1.3.0", None);
    newer.install(Default::default()).await.unwrap();
    let error = h.service.install(Default::default()).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Refusing to replace t3@1.3.0 with older t3@1.2.3. Run the command again with --allow-downgrade to continue."
    );
    assert_eq!(
        service_state_active_version(&std::fs::read_to_string(state_path(&h.base)).unwrap()).as_deref(),
        Some("1.3.0")
    );
    // The failed attempt restarted the service it had stopped.
    assert_eq!(h.runner.commands().last().unwrap(), "systemctl --user restart t3code.service");
    h.service
        .install(BootServiceInstallOptions { allow_downgrade: true, start: None })
        .await
        .unwrap();
    assert_eq!(
        service_state_active_version(&std::fs::read_to_string(state_path(&h.base)).unwrap()).as_deref(),
        Some("1.2.3")
    );
    // Equal precedence (build metadata differs) repairs without an override.
    let build = service_for(&h, "1.2.3+build.5", None);
    build.install(Default::default()).await.unwrap();
}

#[tokio::test]
async fn install_without_start_rewrites_files_and_marks_a_restart_pending() {
    let h = service_harness("linux");
    h.service.install(Default::default()).await.unwrap();
    h.runner.commands.lock().unwrap().clear();
    let newer = service_for(&h, "1.3.0", None);
    newer.install(BootServiceInstallOptions { allow_downgrade: false, start: Some(false) }).await.unwrap();
    // The service is left running: nothing was stopped, reloaded or restarted.
    assert!(!h.runner.commands().iter().any(|c| c.contains("stop") || c.contains("daemon-reload") || c.contains("restart")));
    assert_eq!(
        std::fs::read_to_string(h.base.join("runtime").join(SERVICE_RESTART_PENDING_FILE)).unwrap(),
        "1.3.0\n"
    );
    let status = newer.status().await.unwrap();
    assert!(!status.current);
    assert!(status.problems.contains(&BootServiceProblem::RestartPending));
    assert_eq!(status.installed_version.as_deref(), Some("1.3.0"));
    // A restart switches to the version the unit names and clears the marker.
    assert!(newer.restart().await.unwrap());
    assert!(!h.base.join("runtime").join(SERVICE_RESTART_PENDING_FILE).exists());
    assert!(newer.status().await.unwrap().current);
}

#[tokio::test]
async fn install_without_start_refuses_while_a_remote_update_is_pending() {
    let h = service_harness("linux");
    h.service.install(Default::default()).await.unwrap();
    let pending = ServiceState {
        active_version: "1.2.3".into(),
        update: Some(ServiceUpdateRecord::Pending(PendingServiceUpdate {
            id: "u".into(),
            from_version: "1.2.3".into(),
            target_version: "1.4.0".into(),
            db_path: "/db".into(),
        })),
    }
    .to_json_text();
    std::fs::write(state_path(&h.base), &pending).unwrap();
    let newer = service_for(&h, "1.3.0", None);
    let error = newer.install(BootServiceInstallOptions { allow_downgrade: false, start: Some(false) }).await.unwrap_err();
    assert_eq!(error, BootServiceError::UpdatePending);
    assert_eq!(std::fs::read_to_string(state_path(&h.base)).unwrap(), pending, "nothing was overwritten");
    // The same with the service stopped first: the launcher finished its write during the stop.
    let error = newer.install(Default::default()).await.unwrap_err();
    assert_eq!(error, BootServiceError::UpdatePending);
    assert_eq!(std::fs::read_to_string(state_path(&h.base)).unwrap(), pending);
    assert_eq!(
        error.to_string(),
        "A remote server update is still pending. Wait for it to finish, then retry."
    );
}

#[tokio::test]
async fn restart_is_a_noop_for_other_homes_and_recovers_from_failed_activation() {
    let h = service_harness("linux");
    // Nothing installed: nothing restarted.
    assert!(!h.service.restart().await.unwrap());
    h.service.install(Default::default()).await.unwrap();
    h.runner.commands.lock().unwrap().clear();
    assert!(h.service.restart().await.unwrap());
    assert_eq!(
        h.runner.commands(),
        [
            "systemctl --user stop t3code.service",
            "systemctl --user daemon-reload",
            "systemctl --user enable t3code.service",
            "systemctl --user restart t3code.service",
        ]
    );
    // The unit names another T3 home: leave that service alone.
    let other = service_for(&h, "1.2.3", Some(&h.home.path().join("other-home")));
    h.runner.commands.lock().unwrap().clear();
    assert!(!other.restart().await.unwrap());
    assert!(h.runner.commands().is_empty());
    // Activation failing brings the previous service back.
    *h.control.fail_command.lock().unwrap() = Some("systemctl --user enable t3code.service".into());
    h.runner.commands.lock().unwrap().clear();
    let error = h.service.restart().await.unwrap_err();
    assert_eq!(error.to_string(), "Background setup failed while enabling the service (exit code 1).");
    assert_eq!(h.runner.commands().last().unwrap(), "systemctl --user restart t3code.service");
}

#[tokio::test]
async fn launchd_installs_bootstraps_and_keeps_the_agent_current() {
    let h = service_harness("darwin");
    let plan = h.service.install(Default::default()).await.unwrap();
    assert!(plan.unit_path.ends_with("Library/LaunchAgents/com.t3tools.t3code.service.plist"));
    let commands = h.runner.commands();
    assert!(commands.iter().any(|c| c == "launchctl enable gui/501/com.t3tools.t3code.service"));
    assert_eq!(
        commands.last().unwrap(),
        &format!("launchctl bootstrap gui/501 {}", plan.unit_path)
    );
    let plist = std::fs::read_to_string(&plan.unit_path).unwrap();
    assert!(plist.contains("<string>/Users/theo/Tools:/usr/bin:/usr/bin:") || plist.contains("/Users/theo/Tools"));
    assert!(plist.contains("<string>/usr/bin</string>") || plist.contains(":/usr/bin"));
    let status = h.service.status().await.unwrap();
    assert!(status.current && status.problems.is_empty());
    // The installer's PATH changing does not make an installed agent stale.
    let mut config = service_config(h.home.path(), "darwin", "1.2.3", "/somewhere/else:/usr/bin");
    config.base_dir = h.base.clone();
    config.logs_dir = h.base.join("userdata").join("logs");
    let moved = BootService::new(config, h.runner.clone(), release_fetcher("0.0.0", "darwin-x64", None));
    assert!(moved.status().await.unwrap().current);
    // A bootout of an agent that is not loaded is ignored; uninstall removes the plist.
    h.runner.commands.lock().unwrap().clear();
    assert!(h.service.uninstall().await.unwrap());
    assert_eq!(
        h.runner.commands(),
        ["launchctl bootout --wait gui/501/com.t3tools.t3code.service"]
    );
}

#[tokio::test]
async fn unsupported_platforms_fail_closed() {
    let home = temp_dir("boot-service-win");
    let config = service_config(home.path(), "win32", "1.2.3", "");
    let service = BootService::new(config, FakeRunner::new(|_, _| ok("")), release_fetcher("0.0.0", "linux-x64", None));
    assert_eq!(
        service.install(Default::default()).await.unwrap_err().to_string(),
        "Background setup supports Linux with systemd and macOS with launchd; this machine reports 'win32'."
    );
    let status = service.status().await.unwrap();
    assert!(!status.supported && !status.installed && !status.current);
    // No home directory: also unsupported.
    let mut config = service_config(home.path(), "linux", "1.2.3", "");
    config.home_dir = String::new();
    assert!(select_boot_service_manager("linux", "", Some(1), "").is_none());
    assert!(select_boot_service_manager("darwin", "/h", None, "").is_none());
    let _ = config;
}

#[test]
fn the_launchd_path_keeps_safe_installer_directories_and_system_ones() {
    assert_eq!(
        boot_service_environment_path("/Users/theo/Tools:/usr/bin:", "/usr/local/bin/t3"),
        "/Users/theo/Tools:/usr/bin:/usr/local/bin:/opt/homebrew/bin:/bin:/usr/sbin:/sbin"
    );
    assert_eq!(
        boot_service_environment_path("", "/opt/t3/t3"),
        "/opt/t3:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    );
    // Directories that cannot be written into XML text are dropped.
    assert_eq!(
        boot_service_environment_path("/bad\u{1}dir:/ok", "/usr/bin/t3"),
        "/ok:/usr/bin:/opt/homebrew/bin:/usr/local/bin:/bin:/usr/sbin:/sbin"
    );
}

#[test]
fn prerequisite_and_command_errors_read_like_the_originals() {
    assert_eq!(
        BootServiceError::Command {
            step: "starting the service".into(),
            exit_code: None,
            stdout_length: None,
            stderr_length: None,
            cause: None
        }
        .to_string(),
        "Background setup failed while starting the service."
    );
    assert_eq!(
        BootServiceError::Prerequisite { problem: BootServiceProblem::ServiceStopped, cause: None }.to_string(),
        format!("[service-stopped] {}", BootServiceProblem::ServiceStopped.describe())
    );
    assert_eq!(
        BootServiceError::Install { cause: "x".into() }.to_string(),
        "Could not set up the T3 Code background service."
    );
    let _ = (AtomicUsize::new(0), ServiceUpdateRecord::Pending(PendingServiceUpdate {
        id: String::new(),
        from_version: String::new(),
        target_version: String::new(),
        db_path: String::new(),
    }));
}
