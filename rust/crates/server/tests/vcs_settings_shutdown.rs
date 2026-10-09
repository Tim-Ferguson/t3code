//! Serving a `vcs.*` request remembers the server's settings for pull request lookups, in a
//! process-wide singleton. That memory must not keep the settings worker, which waits on its
//! command channel on the blocking pool, alive: a tokio runtime waits for the blocking pool when it
//! is dropped, so a worker that never exits keeps the process from shutting down.
use serde_json::json;
use std::time::Duration;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
    transport::ApiState,
    vcs_rpc,
};

fn git(cwd: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "user.email=test@test.com", "-c", "user.name=Test", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// Starts the settings, serves a status request that remembers them, and leaves `explicit_shutdown`
/// to say whether the settings are shut down by name or only dropped, as a server that exits does.
async fn serve_a_request(directory: &std::path::Path, explicit_shutdown: bool) {
    let repository = directory.join("repository");
    std::fs::create_dir(&repository).unwrap();
    git(&repository, &["init", "-q"]);
    std::fs::write(repository.join("README.md"), "hello\n").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-q", "-m", "initial"]);
    let store = Store::open(directory.join("state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [7; 32], "t3_session_shutdown".into(), "loopback-browser".into()).unwrap();
    let secrets = ServerSecretStore::open(directory.join("secrets")).unwrap();
    let mut options = SettingsOptions::file(directory.join("settings.json"), secrets);
    options.watch = false;
    let settings = SettingsService::start(options).await.unwrap();
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: Some(settings.clone()),
        cors_origins: None,
        assets: None,
        providers: None,
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };
    let status = vcs_rpc::command(&state, "vcs.refreshStatus", json!({"cwd":repository})).await;
    assert!(status.is_ok(), "{status:?}");
    drop(state);
    if explicit_shutdown {
        settings.shutdown().await;
    }
    drop(settings);
}

/// Dropping the runtime returns. The bound only reports a hang; a run that does not hang does not
/// wait for it.
fn runtime_shuts_down(explicit_shutdown: bool) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(serve_a_request(directory.path(), explicit_shutdown));
    let (done, finished) = std::sync::mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(runtime);
        let _ = done.send(());
    });
    finished.recv_timeout(Duration::from_secs(30)).expect("the runtime did not shut down: the settings worker is still parked on its command channel");
    dropper.join().unwrap();
}

#[test]
fn a_runtime_shuts_down_after_the_settings_are_dropped() {
    runtime_shuts_down(false);
}

#[test]
fn a_runtime_shuts_down_after_the_settings_are_shut_down() {
    runtime_shuts_down(true);
}
