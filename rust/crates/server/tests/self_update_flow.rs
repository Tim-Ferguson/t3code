//! `server.updateServer`, `server.updateServerWithProgress` and `server.commitDesktopUpdate` over a real socket. An unconfigured
//! server answers as the original does when updates are unavailable; the desktop-managed one drives a fake desktop app over a real
//! telemetry control channel (an in-memory pipe pair), and nothing is ever downloaded, installed or relaunched.
mod rpc_support;
use rpc_support::*;
#[allow(unused_imports)]
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use t3_server::{
    desktop_app_update::DesktopUpdates,
    desktop_telemetry::{DesktopTelemetryOptions, DesktopTelemetryReceiver},
    self_update_rpc,
    test_support::HANG_BOUND,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

const UNCONFIGURED: &str = "Remote updates require the T3 Code background service. Run `t3 service install` on the server machine.";

/// The desktop app: reads the server's control lines and writes status reports.
struct App {
    lines: BufReader<DuplexStream>,
    reports: DuplexStream,
}

impl App {
    async fn control(&mut self, kind: &str) -> Value {
        loop {
            let mut line = String::new();
            tokio::time::timeout(HANG_BOUND, self.lines.read_line(&mut line)).await.unwrap().unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["type"] == kind {
                return value;
            }
        }
    }
    async fn report(&mut self, request_id: &str, outcome: Option<&str>, status: &str, reason: Option<&str>) {
        let mut report = json!({"version":1,"type":"desktopUpdateStatus","requestId":request_id,"state":{"enabled":true,"status":status,"channel":"latest","currentVersion":"1.0.0","hostArch":"arm64","appArch":"arm64","runningUnderArm64Translation":false,"availableVersion":"1.1.0","downloadedVersion":if status == "downloaded" { json!("1.1.0") } else { Value::Null },"releaseNotes":[],"omittedReleaseCount":0,"downloadPercent":null,"checkedAt":null,"message":null,"errorContext":null,"canRetry":false}});
        if let Some(outcome) = outcome {
            report["outcome"] = json!(outcome);
        }
        if let Some(reason) = reason {
            report["reason"] = json!(reason);
        }
        self.reports.write_all(format!("{report}\n").as_bytes()).await.unwrap();
    }
}

/// A session that may maintain the environment, which updating needs.
fn maintainer(h: &Harness) -> String {
    h.auth.issue_session("maintainer", "bearer-access-token", vec![t3_contracts::AuthEnvironmentScope::OrchestrationRead, t3_contracts::AuthEnvironmentScope::OrchestrationOperate, t3_contracts::AuthEnvironmentScope::EnvironmentMaintain], json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1
}

async fn desktop() -> (DesktopTelemetryReceiver, App) {
    let (input, reports) = tokio::io::duplex(1 << 16);
    let (control, output) = tokio::io::duplex(1 << 16);
    let mut options = DesktopTelemetryOptions::unavailable("desktop");
    options.input = Some((3, Box::pin(input)));
    options.control = Some((4, Box::pin(control)));
    let receiver = DesktopTelemetryReceiver::new(options).await;
    (receiver, App { lines: BufReader::new(output), reports })
}

async fn install(h: &Harness, desktop_mode: bool, receiver: Option<DesktopTelemetryReceiver>) {
    let available = receiver.is_some();
    let receiver = match receiver {
        Some(receiver) => receiver,
        None => DesktopTelemetryReceiver::new(DesktopTelemetryOptions::unavailable("web")).await,
    };
    let service = self_update_rpc::build(desktop_mode, Arc::new(DesktopUpdates::new(receiver, available)), h.base.clone(), h.base.join("rust-state.sqlite").to_string_lossy().into_owned(), &BTreeMap::new()).unwrap();
    self_update_rpc::install(&h.store, service);
}

/// Every chunk value and the final exit of a stream.
async fn stream_until_exit(socket: &mut Socket, id: u64) -> (Vec<Value>, Value) {
    let mut values = vec![];
    loop {
        let message = tokio::time::timeout(HANG_BOUND, next(socket)).await.unwrap();
        match message["_tag"].as_str() {
            Some("Chunk") => {
                values.extend(message["values"].as_array().unwrap().iter().cloned());
                send(socket, json!({"_tag":"Ack","requestId":id})).await;
            }
            Some("Exit") => return (values, message["exit"].clone()),
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_server_that_cannot_update_itself_says_so_as_the_original_does() {
    let h = harness().await;
    let mut socket = h.connect(&maintainer(&h)).await;
    // Nothing installed at all, and a web-mode service: the same answer.
    for installed in [false, true] {
        if installed {
            install(&h, false, None).await;
        }
        let exit = rpc(&mut socket, 1, "server.updateServer", json!({"targetVersion":"1.2.3"})).await;
        assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":UNCONFIGURED}));
        send(&mut socket, json!({"_tag":"Request","id":2,"tag":"server.updateServerWithProgress","payload":{"targetVersion":"1.2.3"},"headers":[]})).await;
        let (values, exit) = stream_until_exit(&mut socket, 2).await;
        assert!(values.is_empty());
        assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":UNCONFIGURED}));
    }
    // A blank or missing version is refused by the payload's decoding before the service sees it.
    let exit = rpc(&mut socket, 3, "server.updateServer", json!({"targetVersion":"  "})).await;
    assert_eq!(exit["_tag"], "Failure");
    // Committing a desktop update without the app.
    let exit = rpc(&mut socket, 4, "server.commitDesktopUpdate", json!({"requestId":"abc"})).await;
    assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":"This server cannot commit a desktop app update."}));
    h.finish().await;
}

#[tokio::test]
async fn a_desktop_server_without_the_app_channel_points_to_the_app() {
    let h = harness().await;
    install(&h, true, None).await;
    let mut socket = h.connect(&maintainer(&h)).await;
    let exit = rpc(&mut socket, 1, "server.updateServer", json!({"targetVersion":"1.2.3"})).await;
    assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":"This server is managed by the T3 Code desktop app on its machine; update the desktop app to update it."}));
    h.finish().await;
}

#[tokio::test]
async fn the_desktop_app_downloads_and_the_client_commits() {
    let h = harness().await;
    let (receiver, mut app) = desktop().await;
    install(&h, true, Some(receiver)).await;
    let mut socket = h.connect(&maintainer(&h)).await;

    send(&mut socket, json!({"_tag":"Request","id":1,"tag":"server.updateServerWithProgress","payload":{"targetVersion":"ignored"},"headers":[]})).await;
    let request = app.control("requestDesktopUpdate").await;
    let id = request["requestId"].as_str().unwrap().to_owned();
    // A report for another request is not ours; checking and downloading are one stage; downloaded is the next.
    app.report("someone-else", None, "downloading", None).await;
    app.report(&id, None, "checking", None).await;
    app.report(&id, None, "downloading", None).await;
    app.report(&id, None, "downloaded", None).await;
    app.report(&id, Some("ready-to-install"), "downloaded", None).await;
    let (values, exit) = stream_until_exit(&mut socket, 1).await;
    assert_eq!(success(&exit), &Value::Null);
    assert_eq!(
        values,
        vec![
            json!({"type":"progress","stage":"downloading"}),
            json!({"type":"progress","stage":"installing"}),
            json!({"type":"complete","result":{"targetVersion":"1.1.0","method":"desktop-app","desktopUpdateToken":id}}),
        ]
    );

    // The commit has the app install; the app reporting it failed is the answer, with its reason.
    let committed = tokio::spawn({
        let mut socket = h.connect(&maintainer(&h)).await;
        let id = id.clone();
        async move { rpc(&mut socket, 9, "server.commitDesktopUpdate", json!({"requestId":id})).await }
    });
    assert_eq!(app.control("commitDesktopUpdate").await["requestId"], id);
    app.report(&id, Some("failed"), "error", Some("The disk is full.")).await;
    let exit = tokio::time::timeout(HANG_BOUND, committed).await.unwrap().unwrap();
    assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":"The disk is full."}));
    h.finish().await;
}

#[tokio::test]
async fn the_desktop_apps_other_answers_are_the_clients() {
    let h = harness().await;
    let (receiver, mut app) = desktop().await;
    install(&h, true, Some(receiver)).await;
    let mut socket = h.connect(&maintainer(&h)).await;
    for (outcome, reason, expected) in [
        ("up-to-date", None, "The T3 Code desktop app on this machine is already up to date on 1.0.0.".to_owned()),
        ("failed", Some("No network."), "No network.".to_owned()),
        ("failed", None, "The desktop app update failed.".to_owned()),
    ] {
        send(&mut socket, json!({"_tag":"Request","id":1,"tag":"server.updateServer","payload":{"targetVersion":"1.2.3"},"headers":[]})).await;
        let id = app.control("requestDesktopUpdate").await["requestId"].as_str().unwrap().to_owned();
        app.report(&id, Some(outcome), "idle", reason).await;
        let exit = loop {
            let message = tokio::time::timeout(HANG_BOUND, next(&mut socket)).await.unwrap();
            if message["_tag"] == "Exit" {
                break message["exit"].clone();
            }
        };
        assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":expected}));
    }
    // One update at a time: a second request while the first waits is refused, and the first still finishes.
    send(&mut socket, json!({"_tag":"Request","id":2,"tag":"server.updateServer","payload":{"targetVersion":"1.2.3"},"headers":[]})).await;
    let id = app.control("requestDesktopUpdate").await["requestId"].as_str().unwrap().to_owned();
    let mut second = h.connect(&maintainer(&h)).await;
    let exit = rpc(&mut second, 1, "server.updateServer", json!({"targetVersion":"1.2.3"})).await;
    assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":"A desktop app update is already in progress."}));
    app.report(&id, Some("ready-to-install"), "downloaded", None).await;
    let exit = loop {
        let message = tokio::time::timeout(HANG_BOUND, next(&mut socket)).await.unwrap();
        if message["_tag"] == "Exit" {
            break message["exit"].clone();
        }
    };
    assert_eq!(success(&exit)["method"], "desktop-app");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn an_update_the_app_never_finishes_times_out_and_is_cancelled() {
    use t3_remote::self_update::{DesktopAppUpdate, silent_progress};
    let (receiver, mut app) = desktop().await;
    let updates = Arc::new(DesktopUpdates::new(receiver, true));
    let running = tokio::spawn({
        let updates = updates.clone();
        async move { updates.run(silent_progress()).await }
    });
    let id = app.control("requestDesktopUpdate").await["requestId"].as_str().unwrap().to_owned();
    // No report ever comes: the backstop ends it, and the app is told to stop.
    tokio::time::advance(Duration::from_secs(20 * 60 + 1)).await;
    let error = tokio::time::timeout(HANG_BOUND, running).await.unwrap().unwrap().unwrap_err();
    assert_eq!(error.reason, "The desktop app did not finish the update in time.");
    assert_eq!(app.control("cancelDesktopUpdate").await["requestId"], id);
    // The slot is free again.
    let again = tokio::spawn({
        let updates = updates.clone();
        async move { updates.run(silent_progress()).await }
    });
    app.control("requestDesktopUpdate").await;
    again.abort();

    // The install backstop is two minutes.
    let id = "install-one";
    let commit = tokio::spawn({
        let updates = updates.clone();
        async move { updates.commit(id, t3_remote::self_update::no_handoff_callback()).await }
    });
    app.control("commitDesktopUpdate").await;
    tokio::time::advance(Duration::from_secs(2 * 60 + 1)).await;
    let error = tokio::time::timeout(HANG_BOUND, commit).await.unwrap().unwrap().unwrap_err();
    assert_eq!(error.reason, "The desktop app did not report an install result in time.");
}

// ---- a server a launcher started ---------------------------------------------------------------------------------

/// The release feed of one version on a loopback socket: its checksum file and its archive, which holds a `t3` that answers
/// the staged preflight as `preflight` says.
struct Feed {
    origin: String,
    _directory: tempfile::TempDir,
    requested: Arc<std::sync::Mutex<Vec<String>>>,
}

fn platform_key() -> &'static str {
    let platform = match std::env::consts::OS { "macos" => "darwin", other => other };
    let arch = match std::env::consts::ARCH { "x86_64" => "x64", "aarch64" => "arm64", other => other };
    t3_remote::cli_release::cli_archive_platform_key(platform, arch).expect("a platform the releases cover")
}

async fn feed(version: &str, preflight: &str, corrupt_checksum: bool) -> Feed {
    use axum::{Router, extract::State, http::StatusCode, routing::get};
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let name = format!("t3-{version}-{}", platform_key());
    let wrapped = directory.path().join(&name);
    std::fs::create_dir_all(&wrapped).unwrap();
    let executable = wrapped.join("t3");
    std::fs::write(&executable, format!("#!/bin/sh\ncase \"$1\" in __service-preflight) printf '%s' '{preflight}';; esac\n")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = directory.path().join(format!("{name}.tar.gz"));
    assert!(std::process::Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(directory.path()).arg(&name).status().unwrap().success());
    let bytes = std::fs::read(&archive).unwrap();
    let digest = if corrupt_checksum { "0".repeat(64) } else { { use sha2::Digest; sha2::Sha256::digest(&bytes) }.iter().map(|byte| format!("{byte:02x}")).collect() };
    let checksums = format!("{digest}  {name}.tar.gz\n");
    let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
    type Files = Arc<(String, Vec<u8>, Arc<std::sync::Mutex<Vec<String>>>)>;
    let files: Files = Arc::new((checksums, bytes, requested.clone()));
    let router = Router::new()
        .route("/{tag}/{file}", get(|State(files): State<Files>, axum::extract::Path((tag, file)): axum::extract::Path<(String, String)>| async move {
            files.2.lock().unwrap().push(format!("{tag}/{file}"));
            if file == "SHA256SUMS" { (StatusCode::OK, files.0.clone().into_bytes()) } else if file.ends_with(".tar.gz") { (StatusCode::OK, files.1.clone()) } else { (StatusCode::NOT_FOUND, Vec::new()) }
        }))
        .with_state(files);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Feed { origin, _directory: directory, requested }
}

/// The launcher on the other end of the channel: it answers each `request-update` as `answer` says and reports what it was asked.
fn launcher(answer: Value) -> (Arc<t3_server::launcher_ipc::IpcLauncherHost>, tokio::sync::mpsc::UnboundedReceiver<Value>) {
    use std::io::{BufRead, Write};
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let host = t3_server::launcher_ipc::IpcLauncherHost::new(ours.try_clone().unwrap(), ours);
    let (asked, received) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut writer = theirs.try_clone().unwrap();
        for line in std::io::BufReader::new(theirs).lines() {
            let Ok(line) = line else { return };
            let message: Value = serde_json::from_str(&line).unwrap();
            let _ = asked.send(message);
            writer.write_all(format!("{answer}\n").as_bytes()).unwrap();
        }
    });
    (host, received)
}

fn launcher_environment(feed: &Feed) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("T3_SERVICE_LAUNCHER_CONTEXT".to_owned(), json!({"protocol":3,"childVersion":env!("CARGO_PKG_VERSION")}).to_string()),
        (t3_remote::cli_release::CLI_RELEASE_BASE_URL_ENV.to_owned(), feed.origin.clone()),
    ])
}

async fn install_managed(h: &Harness, environment: &BTreeMap<String, String>, host: Arc<t3_server::launcher_ipc::IpcLauncherHost>) {
    let receiver = DesktopTelemetryReceiver::new(DesktopTelemetryOptions::unavailable("web")).await;
    let effects = self_update_rpc::UpdateEffects { launcher: Some(host), ..self_update_rpc::UpdateEffects::host(&BTreeMap::new()) };
    let service = self_update_rpc::build_with(false, Arc::new(DesktopUpdates::new(receiver, false)), h.base.clone(), h.base.join("rust-state.sqlite").to_string_lossy().into_owned(), environment, effects).unwrap();
    self_update_rpc::install(&h.store, service);
}

#[test]
fn a_launcher_manages_the_server_the_descriptor_advertises_it() {
    let managed = self_update_rpc::capabilities(false, false, true);
    assert_eq!(managed.self_update, Some(t3_contracts::ServerSelfUpdateCapability::BootService));
    assert!(managed.progress && !managed.desktop_app_update);
    assert_eq!(self_update_rpc::capabilities(false, false, false).self_update, None);
    // A desktop-managed server stays so, whatever else launched it.
    assert_eq!(self_update_rpc::capabilities(true, false, true).self_update, Some(t3_contracts::ServerSelfUpdateCapability::DesktopManaged));
}

#[tokio::test]
async fn a_launcher_managed_server_downloads_proves_and_hands_the_update_to_its_launcher() {
    let version = "9.8.7";
    let release = feed(version, &json!({"status":"ready","version":version,"launcherProtocol":3}).to_string(), false).await;
    let (host, mut asked) = launcher(json!({"type":"update-accepted","updateId":"update-1"}));
    let h = harness().await;
    install_managed(&h, &launcher_environment(&release), host).await;
    let mut socket = h.connect(&maintainer(&h)).await;

    send(&mut socket, json!({"_tag":"Request","id":1,"tag":"server.updateServerWithProgress","payload":{"targetVersion":version},"headers":[]})).await;
    let (values, exit) = stream_until_exit(&mut socket, 1).await;
    assert_eq!(success(&exit), &Value::Null);
    assert_eq!(values, vec![json!({"type":"progress","stage":"downloading"}), json!({"type":"progress","stage":"installing"}), json!({"type":"complete","result":{"targetVersion":version,"method":"boot-service","updateId":"update-1"}})]);
    let request = tokio::time::timeout(HANG_BOUND, asked.recv()).await.unwrap().unwrap();
    assert_eq!(request, json!({"type":"request-update","targetVersion":version,"dbPath":h.base.join("rust-state.sqlite").to_string_lossy()}));
    // The runtime is installed where the launcher will look, complete.
    let paths = t3_remote::pinned_runtime::pinned_runtime_paths(&h.base, version, if cfg!(target_os = "macos") { "darwin" } else { "linux" });
    assert!(paths.entry_path.exists());
    assert_eq!(std::fs::read_to_string(paths.sentinel_path).unwrap().trim(), version);
    assert_eq!(release.requested.lock().unwrap().iter().map(String::as_str).collect::<Vec<_>>(), vec![format!("v{version}/SHA256SUMS"), format!("v{version}/t3-{version}-{}.tar.gz", platform_key())]);
    h.finish().await;
}

#[tokio::test]
async fn a_launcher_that_refuses_the_update_gives_its_reason() {
    let version = "9.8.7";
    let release = feed(version, &json!({"status":"ready","version":version,"launcherProtocol":3}).to_string(), false).await;
    let (host, _asked) = launcher(json!({"type":"update-rejected","reason":"The database needs a newer launcher."}));
    let h = harness().await;
    install_managed(&h, &launcher_environment(&release), host).await;
    let mut socket = h.connect(&maintainer(&h)).await;
    let exit = rpc(&mut socket, 1, "server.updateServer", json!({"targetVersion":version})).await;
    assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":"The database needs a newer launcher."}));
    // The same request again is allowed: a failed update does not hold the slot.
    let exit = rpc(&mut socket, 2, "server.updateServer", json!({"targetVersion":"not-a-version"})).await;
    assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":"'not-a-version' is not an exact t3 version."}));
    h.finish().await;
}

#[tokio::test]
async fn an_archive_that_does_not_match_its_checksum_or_a_blocked_preflight_is_not_activated() {
    let version = "9.8.7";
    for (preflight, corrupt, reason) in [
        (json!({"status":"ready","version":version,"launcherProtocol":3}).to_string(), true, format!("Could not prepare t3@{version}.")),
        (json!({"status":"blocked","version":version,"reason":"The database is newer than this release."}).to_string(), false, "The database is newer than this release.".to_owned()),
    ] {
        let release = feed(version, &preflight, corrupt).await;
        let (host, mut asked) = launcher(json!({"type":"update-accepted","updateId":"never"}));
        let h = harness().await;
        install_managed(&h, &launcher_environment(&release), host).await;
        let mut socket = h.connect(&maintainer(&h)).await;
        let exit = rpc(&mut socket, 1, "server.updateServer", json!({"targetVersion":version})).await;
        assert_eq!(failure(&exit), &json!({"_tag":"ServerSelfUpdateError","reason":reason}));
        assert!(asked.try_recv().is_err(), "the launcher is never asked");
        h.finish().await;
    }
}

#[tokio::test]
async fn a_launcher_that_goes_away_ends_the_exchange() {
    use t3_remote::service_launcher_client::{LauncherEvent, ServiceLauncherHost};
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let host = t3_server::launcher_ipc::IpcLauncherHost::new(ours.try_clone().unwrap(), ours);
    let mut events = host.subscribe();
    assert!(host.connected());
    drop(theirs);
    assert!(matches!(tokio::time::timeout(HANG_BOUND, events.recv()).await.unwrap().unwrap(), LauncherEvent::Disconnect));
    assert!(!host.connected());
    assert!(host.send(&t3_remote::service_protocol::ServiceLauncherChildMessage::Prepared { update_id: "u".into() }).is_err());
}
