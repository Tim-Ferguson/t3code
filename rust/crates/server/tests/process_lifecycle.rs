//! The real binary, started and stopped the way a supervisor does: SIGTERM (and SIGHUP) end it as gracefully as SIGINT, a
//! desktop-mode server whose parent is gone stops itself, it prints one readiness line, and a state directory whose import
//! failed is served in the minimal mode that says so. Each server runs in its own temporary state directory with an empty
//! environment; no provider CLI is started and nothing outside the temporary directory is touched.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};
use t3_server::test_support::HANG_BOUND;

struct Server {
    child: Option<Child>,
    /// Lines of standard output as they come; ends when the process (and every holder of the pipe) is gone.
    lines: mpsc::Receiver<String>,
    ready: Value,
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn settings(directory: &Path) -> PathBuf {
    let path = directory.join("settings.json");
    let mut defaults = serde_json::to_value(t3_contracts::ServerSettings::default()).unwrap();
    for (_, config) in defaults["providers"].as_object_mut().unwrap() {
        config["enabled"] = json!(false);
    }
    std::fs::write(&path, serde_json::to_vec(&defaults).unwrap()).unwrap();
    path
}

fn command(root: &Path, state: &Path, extra: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_t3-server"));
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("RUST_LOG", "t3_server=info")
        .args(["serve", "--port", "0", "--state-dir"])
        .arg(state)
        .arg("--settings")
        .arg(settings(root))
        .args(extra);
    command
}

/// Reads standard output on a thread; the first `T3_SERVER_READY` line is parsed and the rest forwarded.
fn readiness(mut child: Child) -> Server {
    let output = child.stdout.take().unwrap();
    let (lines_in, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            if lines_in.send(line).is_err() {
                break;
            }
        }
    });
    let ready = loop {
        let line = lines.recv_timeout(HANG_BOUND).expect("the server printed its readiness line");
        if let Some(rest) = line.strip_prefix("T3_SERVER_READY ") {
            break serde_json::from_str::<Value>(rest).expect("a JSON object after the marker");
        }
    };
    Server { child: Some(child), lines, ready }
}

fn start(root: &Path, state: &Path, extra: &[&str]) -> Server {
    let mut spawn = command(root, state, extra);
    readiness(spawn.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap())
}

fn signal(server: &Server, number: i32) {
    assert_eq!(unsafe { libc::kill(server.child.as_ref().unwrap().id() as i32, number) }, 0);
}

/// Waits for the exit on another thread, so a server that does not stop fails the test instead of hanging it.
fn wait_exit(mut server: Server) -> std::process::ExitStatus {
    let (done, finished) = mpsc::channel();
    let mut child = server.child.take().unwrap();
    std::thread::spawn(move || {
        let _ = done.send(child.wait());
    });
    finished.recv_timeout(HANG_BOUND).expect("the server stopped").unwrap()
}

/// Whether the runtime lock of the state directory is free to take.
fn runtime_lock_is_free(state: &Path) -> bool {
    let lock = state.join("rust-state.sqlite.runtime.lock");
    let file = std::fs::OpenOptions::new().read(true).write(true).open(lock).expect("the lock file exists once a server has run");
    fs2::FileExt::try_lock_exclusive(&file).is_ok()
}

fn get(port: u64, path: &str) -> (u16, String) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
    stream.set_read_timeout(Some(HANG_BOUND)).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut text = String::new();
    stream.read_to_string(&mut text).unwrap();
    let status = text.split(' ').nth(1).unwrap().parse().unwrap();
    (status, text.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned())
}

#[test]
fn the_server_says_when_it_is_listening_and_stops_on_sigterm_and_sighup_as_on_sigint() {
    for (name, number) in [("SIGTERM", libc::SIGTERM), ("SIGHUP", libc::SIGHUP), ("SIGINT", libc::SIGINT)] {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let server = start(root.path(), &state, &[]);
        let port = server.ready["port"].as_u64().unwrap();
        assert!(port > 0);
        assert_eq!(server.ready["pid"], server.child.as_ref().unwrap().id());
        assert_eq!(server.ready["origin"], format!("http://127.0.0.1:{port}"));
        assert_eq!(get(port, "/.well-known/t3/environment").0, 200, "{name}");
        assert!(!runtime_lock_is_free(&state), "{name}: a running server holds the runtime lock");

        // A connection that never finishes its request must not hold the stop up past the grace period.
        let mut stalled = std::net::TcpStream::connect(("127.0.0.1", port as u16)).unwrap();
        write!(stalled, "GET /.well-known/t3/environment HTTP/1.1\r\nHost: localhost\r\n").unwrap();

        signal(&server, number);
        let status = wait_exit(server);
        assert!(status.success(), "{name}: {status:?} {:?}", status.signal());
        assert!(runtime_lock_is_free(&state), "{name}: the lock is released");
        drop(stalled);

        // The next start opens the same state.
        let again = start(root.path(), &state, &[]);
        signal(&again, libc::SIGTERM);
        assert!(wait_exit(again).success(), "{name}: second start");
    }
}

#[test]
fn a_desktop_mode_server_stops_when_its_parent_is_gone() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    // A shell starts the server and stays until its standard input is closed: closing it is the desktop dying.
    let mut spawn = command(root.path(), &state, &["--mode", "desktop"]);
    let mut program = format!("{:?}", spawn.get_program());
    for argument in spawn.get_args() {
        program.push(' ');
        program.push_str(&format!("{argument:?}"));
    }
    let mut shell = Command::new("/bin/sh");
    shell
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path())
        .env("RUST_LOG", "t3_server=info")
        .arg("-c")
        .arg(format!("{program} </dev/null & echo $!; read parent_lives; exit 0"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut shell = shell.spawn().unwrap();
    let output = shell.stdout.take().unwrap();
    let (lines_in, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if lines_in.send(line).is_err() {
                break;
            }
        }
    });
    // The first line is the server's process id, captured by the shell that started it.
    let pid: i32 = lines.recv_timeout(HANG_BOUND).unwrap().trim().parse().unwrap();
    // The server is up and has noted its parent.
    loop {
        let line = lines.recv_timeout(HANG_BOUND).expect("the server announced itself");
        if line.starts_with("T3_SERVER_READY ") {
            break;
        }
    }
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the server runs while its parent does");
    // The parent goes; the server stops, and its standard output (the shell's pipe) closes when it has.
    drop(shell.stdin.take());
    assert!(shell.wait().unwrap().success());
    loop {
        match lines.recv_timeout(HANG_BOUND) {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("the orphaned server (pid {pid}) is still running"),
        }
    }
    assert!(runtime_lock_is_free(&state), "the orphan's runtime lock is released");
}

#[test]
fn a_state_directory_whose_import_failed_is_served_in_a_minimal_mode_that_says_so() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    // A database file the importer cannot read.
    std::fs::write(state.join("statev2.sqlite"), b"this is not a database, and it is long enough to be mistaken for none").unwrap();
    let assets = root.path().join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("index.html"), "<html>web app</html>").unwrap();
    let assets = assets.to_str().unwrap().to_owned();

    let server = start(root.path(), &state, &["--assets", &assets]);
    let port = server.ready["port"].as_u64().unwrap();
    let (status, body) = get(port, "/api/rust/state-import");
    assert_eq!(status, 200);
    let body: Value = serde_json::from_str(&body).unwrap();
    let block: t3_contracts::StateImportStatus = serde_json::from_value(body["stateImport"].clone()).unwrap();
    assert_eq!(block.status, t3_contracts::StateImportOutcome::Failed);
    assert!(block.error.as_deref().is_some_and(|error| error.contains("Importing the original T3 Code state failed")), "{block:?}");
    let (status, descriptor) = get(port, "/.well-known/t3/environment");
    assert_eq!(status, 200);
    let descriptor: t3_contracts::ExecutionEnvironmentDescriptor = serde_json::from_str(&descriptor).unwrap();
    assert_eq!(serde_json::to_value(descriptor).unwrap()["capabilities"]["connectionProbe"], false);
    assert_eq!(get(port, "/"), (200, "<html>web app</html>".to_owned()));
    // Nothing else is served: no sessions, no RPC.
    assert_eq!(get(port, "/api/auth/session").0, 404);
    assert!(!state.join("rust-state.sqlite").exists(), "no store was made over the failed import");
    signal(&server, libc::SIGTERM);
    let status = wait_exit(server);
    assert!(status.success(), "stopped, not failed: {status:?}");

    // With --strict-import there is nothing to look at: it ends with an error and no readiness line.
    let mut spawn = command(root.path(), &state, &["--strict-import"]);
    let output = spawn.stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("T3_SERVER_READY"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Importing the original T3 Code state failed"));
}

// ---------------------------------------------------------------------------------------------------------------------
// The services `main.rs` builds, reached through the server it builds.
// ---------------------------------------------------------------------------------------------------------------------

mod rpc_support;
use rpc_support::{rpc, send, success};
use t3_server::test_support::HANG_BOUND as BOUND;

async fn within<T>(wait: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(BOUND, wait).await.expect("nothing arrived before the hang bound")
}

/// The first chunk of a stream, acknowledged and interrupted.
async fn first_chunk(socket: &mut rpc_support::Socket, id: u64, tag: &str, payload: Value) -> Value {
    send(socket, json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})).await;
    let chunk = loop {
        let frame = within(rpc_support::next(socket)).await;
        if frame["_tag"] == "Chunk" && frame["requestId"] == id {
            break frame;
        }
        assert!(!(frame["_tag"] == "Exit" && frame["requestId"] == id), "{tag} ended without a chunk: {frame}");
    };
    send(socket, json!({"_tag":"Ack","requestId":id})).await;
    send(socket, json!({"_tag":"Interrupt","requestId":id})).await;
    chunk["values"][0].clone()
}

/// Every service `main.rs` constructs answers through the server it builds. The in-process flows build their servers by hand,
/// so a service they wire but `main.rs` forgets (or the reverse) is found only here. With every provider disabled, nothing
/// the machine has installed is started.
#[test]
fn the_services_main_builds_answer_through_the_binary() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let server = start(root.path(), &state, &[]);
    let port = server.ready["port"].as_u64().unwrap();
    let credential = {
        let output = command(root.path(), &state, &[]).arg("pair").stdout(Stdio::piped()).stderr(Stdio::null()).output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().lines().last().unwrap().trim().to_owned()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    runtime.block_on(Box::pin(async {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let exchange = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"),
            ("subject_token", credential.as_str()),
            ("subject_token_type", "urn:t3:params:oauth:token-type:environment-bootstrap"),
            ("requested_token_type", "urn:ietf:params:oauth:token-type:access_token"),
        ];
        let token: Value = http.post(format!("{base}/oauth/token")).form(&exchange).send().await.unwrap().json().await.unwrap();
        let token = token["access_token"].as_str().expect("a bearer token").to_owned();

        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://127.0.0.1:{port}/ws?orchestrationProtocol=2").into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let mut socket = tokio_tungstenite::connect_async(request).await.unwrap().0;

        let mut id = 0;
        let mut call = |tag: &'static str, payload: Value| {
            id += 1;
            (id, tag, payload)
        };
        // NativeConfig, the settings service and the keybindings owner.
        let (n, tag, payload) = call("server.getConfig", json!({}));
        let config = success(&within(rpc(&mut socket, n, tag, payload)).await).clone();
        assert_eq!(config["keybindings"].as_array().unwrap().len(), 80);
        assert!(config["providers"].is_array() && config.get("stateImport").is_none());
        assert_eq!(config["environment"]["capabilities"]["repositoryIdentity"], true);
        // Each of these is a service main.rs makes and hands to the router.
        for (tag, payload) in [
            ("server.getSettings", json!({})),
            ("scheduledTasks.list", json!({})),
            ("server.getHostResources", json!({})),
            ("server.getBackgroundPolicy", json!({})),
            ("server.getProcessDiagnostics", json!({})),
            ("server.getResourceTelemetryHistory", json!({"windowMs":60000,"bucketMs":5000})),
            ("device.list", json!({})),
        ] {
            let (n, tag, payload) = call(tag, payload);
            let exit = within(rpc(&mut socket, n, tag, payload)).await;
            assert_eq!(exit["_tag"], "Success", "{tag}: {exit}");
        }
        for tag in ["subscribeDiscoveredLocalServers", "subscribeTerminalMetadata", "subscribeResourceTelemetry", "subscribeBackgroundPolicy", "subscribeServerLifecycle", "subscribeAuthAccess"] {
            let (n, tag, payload) = call(tag, json!({}));
            let first = first_chunk(&mut socket, n, tag, payload).await;
            assert!(!first.is_null(), "{tag}");
        }
        // ExecutionService and the thread and project services on a real store: a project, then its shell.
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let (n, tag, payload) = call("projects.mutate", json!({"type":"project.create","commandId":"create","projectId":"project","title":"Project","workspaceRoot":workspace}));
        success(&within(rpc(&mut socket, n, tag, payload)).await);
        let (n, tag, payload) = call("orchestration.subscribeShell", json!({}));
        let shell = first_chunk(&mut socket, n, tag, payload).await;
        assert_eq!(shell["snapshot"]["projects"][0]["id"], "project");
        // ManagedProjectFolders, from the store's own directory.
        let (n, tag, payload) = call("projects.ensureScratch", json!({}));
        success(&within(rpc(&mut socket, n, tag, payload)).await);

        // The routers main.rs merges in: the MCP endpoint and the webhook route answer, with the auth they need.
        assert_eq!(http.post(format!("{base}/mcp")).body("{}").send().await.unwrap().status(), 401);
        let hook = http.post(format!("{base}/api/hooks/none/none")).body("{}").send().await.unwrap();
        assert!(hook.headers().contains_key("x-t3-hook-outcome"), "the webhook route is mounted over the scheduled tasks");
        // The MCP OAuth server's discovery document, from the origin the request reached.
        let resource: Value = http.get(format!("{base}/.well-known/oauth-protected-resource/mcp")).send().await.unwrap().json().await.unwrap();
        assert_eq!(resource["resource"], format!("{base}/mcp"));
    }));
    signal(&server, libc::SIGTERM);
    assert!(wait_exit(server).success());
}
