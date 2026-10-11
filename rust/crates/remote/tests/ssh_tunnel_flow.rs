//! The SSH tunnel manager against in-memory processes (`packages/ssh/src/tunnel.test.ts`, whose
//! cases are the spec) and against a fake `ssh` executable at an absolute temp path. The real
//! `ssh` is never run, `~/.ssh` is never read, and no non-loopback host is contacted.
#![cfg(unix)]
mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use support::ssh_fakes::*;
use support::{temp_dir, within, write_script};
use t3_remote::ssh::SshTarget;
use t3_remote::ssh_auth::*;
use t3_remote::ssh_scripts::*;
use t3_remote::ssh_tunnel::*;

const ENGINE_RANGE: &str = "^22.16 || ^23.11 || >=24.10";

fn archive() -> RemoteT3RunnerOptions {
    RemoteT3RunnerOptions {
        archive_version: Some("1.2.3-preview.20260911.4".into()),
        ..Default::default()
    }
}

fn node_script() -> RemoteT3RunnerOptions {
    RemoteT3RunnerOptions {
        node_script_path: Some(
            "/Users/julius/Development/Work/codething-mvp/apps/server/dist/bin.mjs".into(),
        ),
        ..Default::default()
    }
}

fn devbox() -> SshTarget {
    SshTarget {
        alias: "devbox".into(),
        hostname: "devbox.example.com".into(),
        username: Some("julius".into()),
        port: Some(2222),
    }
}

fn plain(alias: &str) -> SshTarget {
    SshTarget {
        alias: alias.into(),
        hostname: alias.into(),
        username: None,
        port: None,
    }
}

// ---- script text -----------------------------------------------------------------------------

#[test]
fn installs_and_runs_the_release_archive_without_node_npm_or_npx() {
    let script = build_remote_t3_runner_script(Some(&archive())).unwrap();
    for needle in [
        "T3_ARCHIVE_VERSION='1.2.3-preview.20260911.4'",
        "T3_NODE_SCRIPT_PATH=''",
        "T3_RELEASE_BASE_URL='https://github.com/pingdotgg/t3code/releases/download'",
        "T3_RUNTIME_DIR=\"$HOME/.t3/runtime/versions/$T3_ARCHIVE_VERSION\"",
        "T3_ARCHIVE=\"t3-$T3_ARCHIVE_VERSION-$T3_PLATFORM-$T3_ARCH.tar.gz\"",
        "SHA256SUMS",
        "exec \"$T3_RUNTIME_DIR/t3\" \"$@\"",
        "T3_LOCK=\"$HOME/.t3/runtime/versions/.$T3_ARCHIVE_VERSION.install.lock\"",
        "while ! mkdir \"$T3_LOCK\" 2>/dev/null; do",
        "mv \"$T3_LOCK/pid.tmp\" \"$T3_LOCK/pid\"",
        "if ! kill -0 \"$T3_LOCK_OWNER\" 2>/dev/null; then",
        "if [ \"$T3_LOCK_UNOWNED\" -ge 5 ]; then",
        "if [ \"$T3_LOCK_WAITED\" -ge 360 ]; then",
        "\"$T3_STAGING/SHA256SUMS\" 30",
        "\"$T3_STAGING/$T3_ARCHIVE\" 240",
    ] {
        assert!(script.contains(needle), "{needle}");
    }
    for absent in ["npx", "npm exec", "t3@latest", "exec t3 \"$@\"", "T3_LOCK_CANDIDATE", "-mmin"] {
        assert!(!script.contains(absent), "{absent}");
    }
    assert_eq!(script.matches("if ! t3_runtime_ready; then").count(), 2);
    let at = |needle: &str| script.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("\"$T3_STAGING/t3\" --version") < at("> \"$T3_STAGING/.install-complete\""));
    // Node discovery is defined for the dev path but only ever invoked inside the node-script
    // branch, which the archive path skips entirely.
    assert_eq!(script.matches("ensure_remote_node_path || true").count(), 1);
    assert!(at("ensure_remote_node_path || true") < at("exec node \"$T3_NODE_SCRIPT_PATH\" \"$@\""));
    assert!(at("exec node \"$T3_NODE_SCRIPT_PATH\" \"$@\"") < at("T3_ARCHIVE_VERSION="));

    let launch = build_remote_launch_script(Some(&RemoteT3RunnerOptions {
        release_base_url: Some("https://mirror.example/t3/".into()),
        ..archive()
    }))
    .unwrap();
    for needle in [
        "T3_ARCHIVE_MODE=1",
        "T3_RELEASE_BASE_URL='https://mirror.example/t3'",
        "\"$RUNNER_FILE\" __ssh-helper pick-port \"$PORT_FILE\"",
        "\"$RUNNER_FILE\" __ssh-helper wait-ready \"$REMOTE_PORT\"",
        "\"$RUNNER_FILE\" __ssh-helper runtime-port \"$DEFAULT_RUNTIME_FILE\"",
    ] {
        assert!(launch.contains(needle), "{needle}");
    }
    assert!(build_remote_launch_script(Some(&node_script())).unwrap().contains("T3_ARCHIVE_MODE=0"));
}

#[test]
fn archive_versions_must_be_a_single_exact_version_segment() {
    for archive_version in [
        "../other",
        "1.2.3/evil",
        "1.2.3\\evil",
        "1.2.3-preview.1 x",
        "1.2.3-preview.1\nrm -rf /",
        "v1.2.3",
    ] {
        let error = build_remote_t3_runner_script(Some(&RemoteT3RunnerOptions {
            archive_version: Some(archive_version.into()),
            ..Default::default()
        }))
        .unwrap_err();
        assert_eq!(
            error,
            RunnerScriptError::InvalidArchiveVersion {
                archive_version: archive_version.into()
            }
        );
    }
}

#[test]
fn a_runner_needs_an_archive_version_or_a_node_script() {
    let blank = RemoteT3RunnerOptions {
        archive_version: Some("  ".into()),
        ..Default::default()
    };
    for input in [None, Some(&RemoteT3RunnerOptions::default()), Some(&blank)] {
        assert_eq!(
            build_remote_t3_runner_script(input).unwrap_err(),
            RunnerScriptError::MissingRunner
        );
    }
    assert_eq!(
        build_remote_launch_script(None).unwrap_err().to_string(),
        "A remote t3 runner needs an archive version or a node script path."
    );
}

#[test]
fn the_remote_node_engine_range_comes_only_from_the_caller() {
    let script = build_remote_t3_runner_script(Some(&node_script())).unwrap();
    assert!(script.contains("T3_NODE_ENGINE_RANGE=''"));
    assert!(!script.contains(ENGINE_RANGE));

    let dev = RemoteT3RunnerOptions {
        node_engine_range: Some(ENGINE_RANGE.into()),
        ..node_script()
    };
    let script = build_remote_t3_runner_script(Some(&dev)).unwrap();
    for needle in [
        "T3_NODE_SCRIPT_PATH='/Users/julius/Development/Work/codething-mvp/apps/server/dist/bin.mjs'",
        "exec node \"$T3_NODE_SCRIPT_PATH\" \"$@\"",
        "T3_ARCHIVE_VERSION=''",
        "prepend_path_if_dir \"$HOME/.local/bin\"",
        "T3_NODE_ENGINE_RANGE='^22.16 || ^23.11 || >=24.10'",
        "remote_node_satisfies_engine()",
        "function satisfiesSemverRange",
        "satisfiesSemverRange(rawVersion, range)",
        "prepend_path_if_dir \"/home/linuxbrew/.linuxbrew/bin\"",
        "prepend_path_if_dir \"$VOLTA_HOME/bin\"",
        "eval \"$(fnm env --shell bash)\"",
        "fnm use --silent-if-unchanged",
        "nvm use --silent default",
        "for T3_NODE_BIN in \"$NVM_DIR\"/versions/node/*/bin",
    ] {
        assert!(script.contains(needle), "{needle}");
    }
    assert!(!script.contains("npx"));
}

#[test]
fn launch_pairing_and_stop_scripts_carry_the_runner_and_the_state_key() {
    let state_key = "711bc738002d72fd";
    let launch = build_remote_launch_script(Some(&archive())).unwrap();
    for needle in [
        "[ -n \"$REMOTE_PID\" ] && [ -n \"$REMOTE_PORT\" ] && kill -0 \"$REMOTE_PID\" 2>/dev/null",
        "RUNNER_CHANGED=1",
        "if ! ensure_remote_node_path; then",
        "\"$RUNNER_FILE\" serve --host 127.0.0.1",
        "--base-dir \"$DEFAULT_SERVER_HOME\"",
        "Remote T3 server did not become ready",
        "wait_ready \"60000\"",
        "if [ -s \"$LOG_FILE\" ]; then",
        "It wrote nothing to %s",
        "T3_ARCHIVE_VERSION='1.2.3-preview.20260911.4'",
        "DEFAULT_RUNTIME_FILE=\"$DEFAULT_SERVER_HOME/userdata/server-runtime.json\"",
        "if (!Number.isInteger(pid) || pid <= 0 || !Number.isInteger(port))",
        "PID_TO_STOP=\"${REMOTE_PID:-$DEFAULT_RUNTIME_PID}\"",
        "printf 'external\\n' >\"$MANAGED_FILE\"",
    ] {
        assert!(launch.contains(needle), "{needle}");
    }
    assert!(!launch.contains("server-home"));
    let at = |needle: &str| launch.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(at("if [ \"$REMOTE_MANAGED\" = \"managed\" ]") < at("printf 'external\\n' >\"$MANAGED_FILE\""));
    assert!(at("DEFAULT_RUNTIME_INFO=\"$(resolve_default_runtime_port") < at("elif [ -n \"$REMOTE_PID\" ]"));

    let dev = build_remote_launch_script(Some(&RemoteT3RunnerOptions {
        node_engine_range: Some(ENGINE_RANGE.into()),
        ..node_script()
    }))
    .unwrap();
    assert!(dev.contains("does not satisfy required range "));

    let pairing = build_remote_pairing_script(state_key, Some(&archive())).unwrap();
    assert!(pairing.contains("\"$RUNNER_FILE\" auth pairing create --base-dir \"$PAIRING_BASE_DIR\" --json"));
    assert!(pairing.contains("PAIRING_BASE_DIR=\"$DEFAULT_SERVER_HOME\""));
    assert!(pairing.contains("T3_ARCHIVE_VERSION='1.2.3-preview.20260911.4'"));
    assert!(!pairing.contains("server-home"));

    let stop = build_remote_stop_script(state_key);
    for needle in [
        "if [ \"$REMOTE_MANAGED\" != \"external\" ] && [ -n \"$REMOTE_PID\" ]",
        "kill \"$REMOTE_PID\" 2>/dev/null || true",
        "rm -f \"$PID_FILE\" \"$PORT_FILE\" \"$MANAGED_FILE\"",
    ] {
        assert!(stop.contains(needle), "{needle}");
    }
}

#[test]
fn the_remote_port_picker_works_without_a_state_file_path() {
    assert!(REMOTE_PICK_PORT_SCRIPT.contains("const filePath = process.argv[2] ?? \"\";"));
}

// ---- launching and pairing -----------------------------------------------------------------------

#[tokio::test]
async fn launch_json_is_accepted_after_remote_shell_startup_noise() {
    let dir = temp_dir("tunnel-noise");
    let spawner = FakeSpawner::always("loaded nvm default\n{\"remotePort\":3774}\n");
    let rt = runtime(spawner.clone(), FakeProbe::ok(), dir.path());
    let launch = within(launch_or_reuse_remote_server(
        &rt,
        &devbox(),
        &Default::default(),
        Some(&archive()),
    ))
    .await
    .unwrap();
    assert_eq!(launch.remote_port, 3774);
    let call = &spawner.recorded()[0];
    assert_eq!(&call.args[call.args.len() - 5..call.args.len() - 1], ["sh", "-l", "-s", "--"]);
}

fn finishing_after(delay: Duration, stdout: &'static str) -> Arc<FakeSpawner> {
    FakeSpawner::new(move |_| {
        let process = FakeProcess::running();
        let handle = process.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            handle.finish(stdout, "", 0);
        });
        process
    })
}

/// Cold remote launches download and unpack a release, so they get far more than the default
/// 60 s of an ssh command: 90 s for the dev node runner, 15 minutes for the archive runner.
#[tokio::test(start_paused = true)]
async fn cold_remote_launches_may_exceed_the_default_ssh_timeout() {
    let dir = temp_dir("tunnel-budget");
    for (runner, delay, ok) in [
        (node_script(), 75, true),
        (node_script(), 95, false),
        (archive(), 800, true),
        (archive(), 901, false),
    ] {
        let spawner = finishing_after(Duration::from_secs(delay), "{\"remotePort\":3774}\n");
        let rt = runtime(spawner, FakeProbe::ok(), dir.path());
        let result = within(launch_or_reuse_remote_server(
            &rt,
            &devbox(),
            &Default::default(),
            Some(&runner),
        ))
        .await;
        if ok {
            assert_eq!(result.unwrap().remote_port, 3774, "{delay}s");
        } else {
            let limit = if is_node_script_runner(Some(&runner)) { 90_000 } else { 900_000 };
            assert_eq!(
                result.unwrap_err().to_string(),
                format!("SSH command timed out after {limit}ms."),
                "{delay}s"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn pairing_on_a_cold_archive_remote_gets_the_archive_budget() {
    let dir = temp_dir("tunnel-pairing-budget");
    let rt = runtime(
        finishing_after(Duration::from_secs(600), "{\"credential\":\"LCL4R2TPHDKQ\"}\n"),
        FakeProbe::ok(),
        dir.path(),
    );
    assert_eq!(
        within(issue_remote_pairing_token(&rt, &devbox(), &Default::default(), Some(&archive())))
            .await
            .unwrap(),
        "LCL4R2TPHDKQ"
    );
    let rt = runtime(
        finishing_after(Duration::from_secs(61), "{\"credential\":\"x\"}\n"),
        FakeProbe::ok(),
        dir.path(),
    );
    assert_eq!(
        within(issue_remote_pairing_token(&rt, &devbox(), &Default::default(), Some(&node_script())))
            .await
            .unwrap_err()
            .to_string(),
        "SSH command timed out after 60000ms."
    );
}

#[tokio::test]
async fn pretty_printed_pairing_json_is_accepted_even_after_shell_noise() {
    let dir = temp_dir("tunnel-pairing-json");
    for stdout in [
        "{\n  \"id\": \"88941235\",\n  \"credential\": \"LCL4R2TPHDKQ\",\n  \"scopes\": [\"orchestration:read\"]\n}\n\n",
        "loaded nvm default\n{\n  \"id\": \"88941235\",\n  \"credential\": \"LCL4R2TPHDKQ\"\n}\n\n",
    ] {
        let rt = runtime(FakeSpawner::always(stdout), FakeProbe::ok(), dir.path());
        assert_eq!(
            within(issue_remote_pairing_token(&rt, &devbox(), &Default::default(), Some(&archive())))
                .await
                .unwrap(),
            "LCL4R2TPHDKQ"
        );
    }
}

// ---- readiness -----------------------------------------------------------------------------------

/// Each probe is bounded, so one hung request cannot stall the retries.
#[tokio::test(start_paused = true)]
async fn each_http_readiness_probe_is_bounded() {
    let probe = FakeProbe::hanging();
    let error = within(wait_for_http_ready(
        probe.as_ref(),
        WaitForHttpReadyInput {
            base_url: "http://127.0.0.1:41773/".into(),
            path: None,
            timeout: Some(Duration::from_millis(1_000)),
            interval: Some(Duration::from_millis(100)),
            probe_timeout: Some(Duration::from_millis(250)),
        },
    ))
    .await
    .unwrap_err();
    assert_eq!(error.tag(), "SshReadinessError");
    assert_eq!(
        error.to_string(),
        "Timed out waiting 1000ms for backend readiness at http://127.0.0.1:41773/."
    );
    // 250 ms per probe plus 100 ms between: the fourth attempt would start at 1050 ms.
    assert_eq!(probe.urls.lock().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn readiness_retries_until_the_server_answers_2xx() {
    let probe = FakeProbe::answering(vec![Err("connection refused".into()), Ok(503), Ok(200)]);
    within(wait_for_http_ready(
        probe.as_ref(),
        WaitForHttpReadyInput::new("http://127.0.0.1:41773/").timeout(Duration::from_secs(20)),
    ))
    .await
    .unwrap();
    assert_eq!(probe.urls.lock().unwrap().len(), 3);
    assert_eq!(probe.urls.lock().unwrap()[0], "http://127.0.0.1:41773/");

    let never = FakeProbe::answering(vec![Ok(503)]);
    let error = within(wait_for_http_ready(
        never.as_ref(),
        WaitForHttpReadyInput::new("http://127.0.0.1:41773/").timeout(Duration::from_millis(1_050)),
    ))
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Timed out waiting 1050ms for backend readiness at http://127.0.0.1:41773/."
    );
}

#[test]
fn readiness_causes_keep_primitive_reasons() {
    let cause = json!({
        "_tag": "HttpClientError",
        "message": "Backend readiness probe failed.",
        "reason": "authentication failed",
        "cause": "upstream closed",
        "ignored": 1,
    });
    assert_eq!(
        describe_readiness_cause(&cause),
        json!({
            "_tag": "HttpClientError",
            "message": "Backend readiness probe failed.",
            "reason": "authentication failed",
            "cause": "upstream closed",
        })
    );
    assert_eq!(describe_readiness_cause(&json!("text")), json!("text"));
}

// ---- the manager ---------------------------------------------------------------------------------

/// What the scripted remote did.
struct RemoteLog {
    tunnels: Arc<Mutex<Vec<Arc<FakeProcess>>>>,
    stops: Arc<AtomicUsize>,
    launches: Arc<AtomicUsize>,
}

/// Answers like the remote: `-G` resolves, `-N` is a tunnel that runs until killed, `sh -l -s --`
/// is the launch, and the other `sh -s` is a stop (failing the first time when asked).
fn remote(fail_first_stop: bool) -> (Arc<FakeSpawner>, RemoteLog) {
    let log = RemoteLog {
        tunnels: Arc::default(),
        stops: Arc::default(),
        launches: Arc::default(),
    };
    let (tunnels, stops, launches) = (log.tunnels.clone(), log.stops.clone(), log.launches.clone());
    let spawner = FakeSpawner::new(move |call| -> Arc<dyn SshProcess> {
        if call.has("-G") {
            FakeProcess::finished("", "", 0)
        } else if call.has("-N") {
            let tunnel = FakeProcess::running();
            tunnels.lock().unwrap().push(tunnel.clone());
            tunnel
        } else if call.has("--") {
            launches.fetch_add(1, Ordering::SeqCst);
            FakeProcess::finished("{\"remotePort\":3773}\n", "", 0)
        } else if call.stdin.as_deref().is_some_and(|s| s.contains("tail -n 80")) {
            FakeProcess::finished("", "", 0)
        } else if stops.fetch_add(1, Ordering::SeqCst) == 0 && fail_first_stop {
            FakeProcess::finished("", "Remote T3 server did not stop within 2 seconds.\n", 1)
        } else {
            FakeProcess::finished("{\"stopped\":true}\n", "", 0)
        }
    });
    (spawner, log)
}

fn manager_with(rt: SshRuntime) -> SshEnvironmentManager {
    SshEnvironmentManager::new(
        rt,
        SshEnvironmentManagerOptions {
            resolve_cli_runner: Some(Box::new(archive)),
        },
    )
}

fn kills(log: &RemoteLog, index: usize) -> usize {
    log.tunnels.lock().unwrap()[index].kills.load(Ordering::SeqCst)
}

/// A disconnect closes the tunnel and stops the remote; a failed stop reaches the caller, and the
/// next connect starts fresh either way.
#[tokio::test]
async fn disconnect_closes_the_tunnel_and_the_next_connect_starts_fresh() {
    for fail in [false, true] {
        let dir = temp_dir("tunnel-restart");
        let (spawner, log) = remote(fail);
        let manager = manager_with(runtime(spawner.clone(), FakeProbe::ok(), dir.path()));
        let first = within(manager.ensure_environment(&devbox(), false)).await.unwrap();
        assert_eq!(first.http_base_url, "http://127.0.0.1:41773/");
        let tunnel = spawner.recorded().into_iter().find(|c| c.has("-N")).unwrap();
        for option in ["ControlMaster=no", "ControlPath=none", "ControlPersist=no"] {
            assert!(tunnel.has(option), "{option}");
        }

        let disconnected = within(manager.disconnect_environment(&devbox())).await;
        if fail {
            let error = disconnected.unwrap_err();
            assert_eq!(error.tag(), "SshCommandError");
            assert_eq!(error.to_string(), "Remote T3 server did not stop within 2 seconds.");
        } else {
            disconnected.unwrap();
        }
        assert_eq!(kills(&log, 0), 1);
        assert_eq!(log.stops.load(Ordering::SeqCst), 1);
        if fail {
            within(manager.disconnect_environment(&devbox())).await.unwrap();
            assert_eq!(kills(&log, 0), 1);
            assert_eq!(log.stops.load(Ordering::SeqCst), 2);
        }

        within(manager.ensure_environment(&devbox(), false)).await.unwrap();
        assert_eq!(spawner.count(|c| c.has("-N")), 2);
        assert_eq!(kills(&log, 0), 1);

        // Quitting closes the live tunnel and stops its remote server.
        manager.shutdown().await;
        assert_eq!(kills(&log, 1), 1);
        assert_eq!(log.stops.load(Ordering::SeqCst), if fail { 3 } else { 2 });
    }
}

#[tokio::test]
async fn a_live_tunnel_is_reused() {
    let dir = temp_dir("tunnel-reuse");
    let (spawner, log) = remote(false);
    let manager = manager_with(runtime(spawner.clone(), FakeProbe::ok(), dir.path()));
    let first = within(manager.ensure_environment(&devbox(), false)).await.unwrap();
    let second = within(manager.ensure_environment(&devbox(), false)).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(spawner.count(|c| c.has("-N")), 1);
    assert_eq!(log.launches.load(Ordering::SeqCst), 1);
}

/// A tunnel that stops answering is closed (and its managed remote stopped) before a new one is
/// made; a new tunnel that never answers fails the connect and is killed without touching the remote.
#[tokio::test(start_paused = true)]
async fn a_stale_tunnel_is_replaced() {
    let dir = temp_dir("tunnel-stale");
    let (spawner, log) = remote(false);
    let probe = FakeProbe::answering(vec![Ok(200), Err("refused".into())]);
    let manager = manager_with(runtime(spawner.clone(), probe, dir.path()));
    within(manager.ensure_environment(&devbox(), false)).await.unwrap();
    let error = within(manager.ensure_environment(&devbox(), false)).await.unwrap_err();
    assert_eq!(error.tag(), "SshReadinessError");
    assert_eq!(spawner.count(|c| c.has("-N")), 2);
    assert_eq!(kills(&log, 0), 1, "the stale tunnel was closed");
    assert_eq!(log.stops.load(Ordering::SeqCst), 1, "and its managed remote stopped");
    assert_eq!(kills(&log, 1), 1, "the replacement that never answered was killed");
    assert!(manager.tunnel_keys().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_tunnel_that_exits_before_it_is_ready_fails_with_its_stderr() {
    let dir = temp_dir("tunnel-exit");
    let spawner = FakeSpawner::new(|call| -> Arc<dyn SshProcess> {
        if call.has("-N") {
            FakeProcess::finished("", "bind: Address already in use\n", 255)
        } else if call.has("--") {
            FakeProcess::finished("{\"remotePort\":3773}\n", "", 0)
        } else {
            FakeProcess::finished("", "", 0)
        }
    });
    let manager = manager_with(runtime(spawner.clone(), FakeProbe::hanging(), dir.path()));
    let error = within(manager.ensure_environment(&plain("devbox"), false))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "bind: Address already in use");
    // The failure also read the remote log tail for the diagnostics.
    assert!(spawner
        .recorded()
        .iter()
        .any(|c| c.stdin.as_deref().is_some_and(|s| s.contains("tail -n 80"))));
}

/// While a tunnel or its remote server is still shutting down, a reconnect of the same target
/// waits for it (one lock per target), other targets are not held up, and two reconnects share
/// one new tunnel.
#[tokio::test]
async fn reconnects_wait_for_the_target_to_finish_shutting_down() {
    for stalled in ["local tunnel", "remote server"] {
        let dir = temp_dir("tunnel-stall");
        let launches = Arc::new(AtomicUsize::new(0));
        let tunnels = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let remote_running = Arc::new(AtomicBool::new(false));
        let resolutions = Arc::new(AtomicUsize::new(0));
        let reconnects_resolved = Arc::new(tokio::sync::Notify::new());
        let stop_started = Arc::new(tokio::sync::Notify::new());
        let first_tunnel: Arc<Mutex<Option<Arc<FakeProcess>>>> = Arc::default();
        let first_stop: Arc<Mutex<Option<Arc<FakeProcess>>>> = Arc::default();
        let hold_stop = stalled == "remote server";
        let spawner = {
            let (launches, tunnels, stops, running) =
                (launches.clone(), tunnels.clone(), stops.clone(), remote_running.clone());
            let (resolutions, resolved) = (resolutions.clone(), reconnects_resolved.clone());
            let (first_tunnel, first_stop, stop_started) =
                (first_tunnel.clone(), first_stop.clone(), stop_started.clone());
            FakeSpawner::new(move |call| -> Arc<dyn SshProcess> {
                let is_target = call.args.iter().any(|a| a == "devbox");
                if call.has("-G") {
                    // Initial connect, disconnect, and the two reconnects each resolve the target.
                    if is_target && resolutions.fetch_add(1, Ordering::SeqCst) + 1 == 4 {
                        resolved.notify_one();
                    }
                    FakeProcess::finished("", "", 0)
                } else if call.has("-N") {
                    let tunnel = FakeProcess::running();
                    if is_target && tunnels.fetch_add(1, Ordering::SeqCst) == 0 {
                        *first_tunnel.lock().unwrap() = Some(tunnel.clone());
                    }
                    tunnel
                } else if call.has("--") {
                    if is_target {
                        launches.fetch_add(1, Ordering::SeqCst);
                        running.store(true, Ordering::SeqCst);
                    }
                    FakeProcess::finished("{\"remotePort\":3773}\n", "", 0)
                } else if is_target && stops.fetch_add(1, Ordering::SeqCst) == 0 && hold_stop {
                    // The first stop runs until the test lets it finish.
                    let stop = FakeProcess::running();
                    *first_stop.lock().unwrap() = Some(stop.clone());
                    stop_started.notify_one();
                    stop
                } else {
                    FakeProcess::finished("{\"stopped\":true}\n", "", 0)
                }
            })
        };
        let manager = manager_with(runtime(spawner, FakeProbe::ok(), dir.path()));
        within(manager.ensure_environment(&plain("devbox"), false)).await.unwrap();

        let tunnel = first_tunnel.lock().unwrap().clone().unwrap();
        let (release_tunnel, tunnel_kill_started) = tunnel.stall_kill();
        let disconnect = tokio::spawn({
            let manager = manager.clone();
            async move { manager.disconnect_environment(&plain("devbox")).await }
        });
        if stalled == "local tunnel" {
            within(tunnel_kill_started.notified()).await;
        } else {
            release_tunnel.notify_one();
            within(stop_started.notified()).await;
        }
        let first = tokio::spawn({
            let manager = manager.clone();
            async move { manager.ensure_environment(&plain("devbox"), false).await }
        });
        let second = tokio::spawn({
            let manager = manager.clone();
            async move { manager.ensure_environment(&plain("devbox"), false).await }
        });
        within(reconnects_resolved.notified()).await;
        // Another target is not held up by the stalled one.
        within(manager.ensure_environment(&plain("other"), false)).await.unwrap();
        let launches_before = launches.load(Ordering::SeqCst);
        if stalled == "local tunnel" {
            release_tunnel.notify_one();
        } else {
            let stop = first_stop.lock().unwrap().clone().unwrap();
            remote_running.store(false, Ordering::SeqCst);
            stop.finish("{\"stopped\":true}\n", "", 0);
        }
        within(disconnect).await.unwrap().unwrap();
        let first = within(first).await.unwrap().unwrap();
        let second = within(second).await.unwrap().unwrap();
        assert_eq!(launches_before, 1, "{stalled}");
        assert_eq!(launches.load(Ordering::SeqCst), 2, "{stalled}");
        assert_eq!(tunnels.load(Ordering::SeqCst), 2, "{stalled}");
        assert!(remote_running.load(Ordering::SeqCst), "{stalled}");
        assert_eq!(first.http_base_url, second.http_base_url);
    }
}

// ---- password prompts -------------------------------------------------------------------------------

struct ScriptedPrompt {
    answers: Mutex<std::collections::VecDeque<Option<String>>>,
    requests: Mutex<Vec<SshPasswordRequest>>,
}

#[async_trait]
impl SshPasswordPrompt for ScriptedPrompt {
    fn is_available(&self) -> bool {
        true
    }

    async fn request(&self, request: SshPasswordRequest) -> Result<Option<String>, SshPasswordPromptError> {
        self.requests.lock().unwrap().push(request);
        Ok(self.answers.lock().unwrap().pop_front().flatten())
    }
}

fn prompting(answers: Vec<Option<&str>>) -> Arc<ScriptedPrompt> {
    Arc::new(ScriptedPrompt {
        answers: Mutex::new(answers.into_iter().map(|a| a.map(str::to_owned)).collect()),
        requests: Default::default(),
    })
}

/// A launch that rejects every attempt except the one served the password `good`.
fn password_remote(good: &'static str) -> Arc<FakeSpawner> {
    FakeSpawner::new(move |call| -> Arc<dyn SshProcess> {
        if call.has("-G") {
            return FakeProcess::finished("", "", 0);
        }
        if call.has("-N") {
            return FakeProcess::running();
        }
        if call.env.get("T3_SSH_AUTH_SECRET").map(String::as_str) == Some(good) {
            FakeProcess::finished("{\"remotePort\":3773}\n", "", 0)
        } else {
            FakeProcess::finished("", "julius@devbox: Permission denied (publickey,password).\n", 255)
        }
    })
}

#[tokio::test]
async fn a_rejected_login_prompts_for_a_password_and_retries_with_askpass() {
    let dir = temp_dir("tunnel-prompt");
    let prompt = prompting(vec![Some("wrong"), Some("hunter2")]);
    let spawner = password_remote("hunter2");
    let mut rt = runtime(spawner.clone(), FakeProbe::ok(), dir.path());
    rt.prompt = prompt.clone();
    let manager = manager_with(rt);
    within(manager.ensure_environment(&devbox(), false)).await.unwrap();

    let requests = prompt.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].attempt, 1);
    assert_eq!(requests[1].attempt, 2);
    assert_eq!(requests[0].destination, "devbox");
    assert_eq!(requests[0].username.as_deref(), Some("julius"));
    assert_eq!(requests[0].prompt, "Enter the SSH password for julius@devbox.");

    let launches: Vec<RecordedCall> = spawner.recorded().into_iter().filter(|c| c.has("--")).collect();
    assert_eq!(launches.len(), 3);
    // With a prompt available the first try is non-interactive; afterwards ssh asks the helper.
    assert!(launches[0].has("BatchMode=yes") && !launches[0].env.contains_key("SSH_ASKPASS"));
    assert!(launches[1].has("BatchMode=no"));
    assert_eq!(launches[1].env["SSH_ASKPASS_REQUIRE"], "force");
    assert_eq!(launches[1].env["T3_SSH_AUTH_SECRET"], "wrong");
    assert_eq!(launches[2].env["T3_SSH_AUTH_SECRET"], "hunter2");
    let askpass = std::fs::read_to_string(&launches[2].env["SSH_ASKPASS"]).unwrap();
    assert!(askpass.contains("printf \"%s\\n\" \"$T3_SSH_AUTH_SECRET\""));

    // The accepted password is remembered for this target: the tunnel start reuses it silently.
    let tunnel = spawner.recorded().into_iter().find(|c| c.has("-N")).unwrap();
    assert_eq!(tunnel.env["T3_SSH_AUTH_SECRET"], "hunter2");
    assert_eq!(prompt.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn prompts_stop_after_two_rejections_and_cancelling_fails_the_connect() {
    let dir = temp_dir("tunnel-prompt-limits");
    let prompt = prompting(vec![Some("a"), Some("b"), Some("c")]);
    let mut rt = runtime(password_remote("never"), FakeProbe::ok(), dir.path());
    rt.prompt = prompt.clone();
    let manager = manager_with(rt);
    let error = within(manager.ensure_environment(&devbox(), false)).await.unwrap_err();
    assert_eq!(error.tag(), "SshCommandError");
    assert!(error.to_string().contains("Permission denied"));
    assert_eq!(prompt.requests.lock().unwrap().len(), 2);

    let mut rt = runtime(password_remote("never"), FakeProbe::ok(), dir.path());
    rt.prompt = prompting(vec![None]);
    let manager = manager_with(rt);
    let error = within(manager.ensure_environment(&devbox(), false)).await.unwrap_err();
    assert_eq!(error.tag(), "SshPasswordPromptError");
    assert_eq!(error.to_string(), "SSH authentication cancelled for julius@devbox.");
}

#[tokio::test]
async fn without_a_prompt_service_ssh_uses_its_own_prompts_and_failures_pass_through() {
    let dir = temp_dir("tunnel-no-prompt");
    let spawner = password_remote("never");
    let manager = manager_with(runtime(spawner.clone(), FakeProbe::ok(), dir.path()));
    let error = within(manager.ensure_environment(&devbox(), false)).await.unwrap_err();
    assert!(error.to_string().contains("Permission denied"));
    let launch = spawner.recorded().into_iter().find(|c| c.has("--")).unwrap();
    // Interactive: ssh may ask on the terminal, with the askpass helper as the fallback.
    assert!(launch.has("BatchMode=no"));
    assert_eq!(launch.env["SSH_ASKPASS_REQUIRE"], "force");
    assert_eq!(launch.env["DISPLAY"], "t3code");
    assert!(!launch.env.contains_key("T3_SSH_AUTH_SECRET"));
    assert_eq!(spawner.count(|c| c.has("--")), 1);
}

// ---- a fake ssh executable -----------------------------------------------------------------------------

/// `ProcessSshSpawner` with a fake `ssh` at an absolute temp path: arguments, stdin, environment,
/// exit codes and stderr go through real processes.
#[tokio::test]
async fn a_fake_ssh_executable_sees_the_arguments_stdin_and_environment() {
    let dir = temp_dir("fake-ssh");
    let log = dir.path().join("log");
    let ssh = write_script(
        dir.path(),
        "ssh",
        &format!(
            "echo \"$*\" > {log}\ncat >> {log}\necho \"secret=$T3_SSH_AUTH_SECRET\" >> {log}\necho out\necho err >&2",
            log = log.display()
        ),
    );
    let rt = runtime(Arc::new(ProcessSshSpawner::new(ssh.display().to_string())), FakeProbe::ok(), dir.path());
    let result = within(run_ssh_command(
        &rt,
        &devbox(),
        RunSshCommandOptions {
            auth: SshAuthOptions {
                auth_secret: Some(Some("pw".into())),
                batch_mode: Some(BatchMode::No),
                interactive_auth: Some(true),
            },
            remote_command_args: vec!["sh".into(), "-s".into()],
            stdin: Some("echo hi\n".into()),
            ..Default::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!(result.stdout, "out\n");
    assert_eq!(result.stderr, "err\n");
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.starts_with("-o BatchMode=no -o ConnectTimeout=10 -p 2222 julius@devbox sh -s\n"));
    assert!(logged.contains("echo hi\n"));
    assert!(logged.contains("secret=pw"));

    for (body, expected) in [
        ("echo 'Permission denied (publickey).' >&2; exit 255", "Permission denied (publickey)."),
        ("echo '{\"credential\": \"pairing-token\"}'; exit 1", "{\"credential\": \"[redacted]\"}"),
        ("exit 4", "SSH command failed for julius@devbox (exit 4)."),
    ] {
        let ssh = write_script(dir.path(), "ssh-failing", body);
        let rt = runtime(Arc::new(ProcessSshSpawner::new(ssh.display().to_string())), FakeProbe::ok(), dir.path());
        let error = within(run_ssh_command(&rt, &devbox(), Default::default())).await.unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
    let rt = runtime(Arc::new(ProcessSshSpawner::new("/nonexistent/ssh")), FakeProbe::ok(), dir.path());
    assert!(within(run_ssh_command(&rt, &devbox(), Default::default())).await.is_err());
}

/// A hung ssh is killed when the command times out (virtual time: the timeout elapses at once, the
/// kill and the exit it causes are real).
#[tokio::test(start_paused = true)]
async fn a_hung_ssh_is_terminated_at_the_timeout() {
    let dir = temp_dir("fake-ssh-hang");
    let pid_file = dir.path().join("pid");
    let ssh = write_script(dir.path(), "ssh", &format!("echo $$ > {}\nexec sleep 600", pid_file.display()));
    let rt = runtime(Arc::new(ProcessSshSpawner::new(ssh.display().to_string())), FakeProbe::ok(), dir.path());
    let error = within(run_ssh_command(
        &rt,
        &devbox(),
        RunSshCommandOptions {
            timeout: Some(Duration::from_millis(50)),
            ..Default::default()
        },
    ))
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "SSH command timed out after 50ms.");
    // The pid is the one the script wrote for the child this test spawned; it is gone after the kill.
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let pid: i32 = pid.trim().parse().unwrap();
        // SAFETY: signal 0 only tests whether that child still exists.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the timed-out ssh must be gone");
    }
}
