//! A fake `glab` shared by the GitLab pull request tests: it numbers each call by claiming the next free directory (a lock a killed call could leave held would stall every call after it), keeps
//! its arguments and stdin, answers from the first route whose fragments its arguments (and
//! stdin) contain, and can be switched to fail as a signed-out, rate-limited or missing-resource
//! CLI would by writing `auth`, `rate` or `notfound` to its `mode` file.
#![allow(dead_code)]
use crate::source_control_support::{FakeCliDir, git};
use serde_json::{Value, json};

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A `glab` that numbers each call by claiming a directory, keeps its arguments and stdin, and answers from
/// the first route whose fragments its arguments (and stdin) contain.
pub fn install_glab(dir: &FakeCliDir, routes: &[Value]) {
    install_routed(dir, "glab", routes, ["glab auth login", "HTTP 429", "404 Not Found"]);
}

/// The same fake under the name of `az`, whose failures read as the Azure CLI's do.
pub fn install_az(dir: &FakeCliDir, routes: &[Value]) {
    install_routed(dir, "az", routes, ["Please run az login to setup account.", "HTTP 429 Too Many Requests", "TF401180: The requested pull request was not found"]);
}

/// A fake host CLI named `executable`; `failures` is what it prints on stderr in the `auth`, `rate`
/// and `notfound` modes.
pub fn install_routed(dir: &FakeCliDir, executable: &str, routes: &[Value], failures: [&str; 3]) {
    if routes.iter().all(|route| route["spawn"] == true) {
        return;
    }
    let mut body = format!(
        "d=\"${{0%/*}}\"\nn=1\nwhile ! mkdir \"$d/claim.$n\" 2>/dev/null; do n=$((n+1)); done\ntrap 'trap - EXIT; exit 143' TERM\ntrap 'printf \"%s\\037\" \"$@\" > \"$d/call.$n.args\"' EXIT\ncat > \"$d/call.$n.stdin\"\njoined=\"$*\"\nin=$(cat \"$d/call.$n.stdin\")\nmode=ok\nif [ -f \"$d/mode\" ]; then read -r mode < \"$d/mode\"; fi\ncase \"$mode\" in\nauth) echo {} >&2; exit 1;;\nrate) echo {} >&2; exit 1;;\nnotfound) echo {} >&2; exit 1;;\nesac\n",
        shell_quote(failures[0]),
        shell_quote(failures[1]),
        shell_quote(failures[2]),
    );
    for (index, route) in routes.iter().enumerate() {
        std::fs::write(dir.dir.path().join(format!("route.{index}.out")), route["stdout"].as_str().unwrap_or("")).unwrap();
        std::fs::write(dir.dir.path().join(format!("route.{index}.err")), route["stderr"].as_str().unwrap_or("")).unwrap();
        // `seq` answers a route's Nth call with the Nth output (the last repeats), by claiming a
        // directory per call so a cancelled call leaves nothing held; `exec` runs a shell snippet
        // first, in the directory the CLI was started in.
        let output = match route["seq"].as_array() {
            Some(outputs) => {
                for (step, output) in outputs.iter().enumerate() {
                    std::fs::write(dir.dir.path().join(format!("route.{index}.out.{step}")), output.as_str().unwrap_or("")).unwrap();
                }
                format!("h=0; while ! mkdir \"$d/route.{index}.hit.$h\" 2>/dev/null; do h=$((h+1)); done; [ $h -gt {} ] && h={}; cat \"$d/route.{index}.out.$h\"", outputs.len() - 1, outputs.len() - 1)
            }
            None => format!("cat \"$d/route.{index}.out\""),
        };
        let prelude = route["exec"].as_str().map_or(String::new(), |snippet| format!("{snippet}; "));
        let answer = format!("{prelude}{output}; cat \"$d/route.{index}.err\" >&2; exit {}", route["code"].as_i64().unwrap_or(0));
        let inner = match route["stdinIncludes"].as_str() {
            Some(needle) => format!("case \"$in\" in *{}*) {answer};; esac", shell_quote(needle)),
            None => answer,
        };
        body.push_str(&format!("case \"$joined\" in *{}*) {inner};; esac\n", shell_quote(route["match"].as_str().unwrap())));
    }
    body.push_str("echo \"no route: $joined\" >&2\nexit 97\n");
    let path = dir.dir.path().join(executable);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The fake's PATH holds nothing but the tools linked into it.
    for tool in ["mkdir"] {
        let found = String::from_utf8(std::process::Command::new("which").arg(tool).output().unwrap().stdout).unwrap();
        let _ = std::os::unix::fs::symlink(found.trim(), dir.dir.path().join(tool));
    }
}

/// Every call the fake completed since the last look, in the order they started. The fake
/// publishes a call's arguments as its last act (a terminated one, such as the sibling read the
/// server drops when its pair fails, cancels the publication), and its stdin is written before,
/// so a call still running or cancelled is never seen half-written, now or in a later look.
pub fn take_calls(dir: &FakeCliDir) -> Vec<Value> {
    let mut numbered: Vec<(u64, Value)> = vec![];
    for entry in std::fs::read_dir(dir.dir.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(number) = name.strip_prefix("call.").and_then(|rest| rest.strip_suffix(".args")).and_then(|number| number.parse::<u64>().ok()) else { continue };
        let arguments = std::fs::read_to_string(entry.path()).unwrap();
        // The record is published as the call's last act; the file exists a moment before it is filled.
        if arguments.is_empty() {
            continue;
        }
        let args: Vec<&str> = arguments.strip_suffix('\u{1f}').map(|text| text.split('\u{1f}').collect()).unwrap_or_default();
        let stdin = std::fs::read_to_string(dir.dir.path().join(format!("call.{number}.stdin"))).unwrap();
        numbered.push((number, json!({"args": args, "stdin": if stdin.is_empty() { Value::Null } else { json!(stdin) }})));
        std::fs::remove_file(entry.path()).unwrap();
        std::fs::remove_file(dir.dir.path().join(format!("call.{number}.stdin"))).unwrap();
    }
    numbered.sort_by_key(|(number, _)| *number);
    numbered.into_iter().map(|(_, call)| call).collect()
}


// ---- a scripted HTTP host ---------------------------------------------------------------------------

/// A host's API on loopback, for the providers that speak HTTP rather than drive a CLI (Bitbucket).
/// Each route answers a request whose method matches (when it names one) and whose target contains
/// its fragment, first route first; a route with `seq` answers its Nth hit with its Nth step (the
/// last repeats). A request is recorded as it completes, so a reader of `take` never sees one half
/// received; a `mode` switches every answer to a failure as a signed-out, rate-limited or missing
/// host would give it.
pub struct RouteServer {
    /// The API base, ending in the version path the module under test is pointed at.
    pub base: String,
    state: std::sync::Arc<std::sync::Mutex<RouteState>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

struct RouteState {
    routes: Vec<Value>,
    hits: Vec<usize>,
    recorded: Vec<Value>,
    mode: Option<u16>,
    /// Every credential header the server was sent, for tests that look for one where it must not be.
    authorizations: Vec<(String, Option<String>)>,
}

/// The path the API lives under on the server, which the recorded urls leave out.
const API_PREFIX: &str = "/2.0";
/// The original's host, which fixtures carry in the urls a page hands out.
pub const FIXTURE_ORIGIN: &str = "https://api.bitbucket.org/2.0";

impl RouteServer {
    pub async fn start(routes: &[Value]) -> RouteServer {
        Self::start_at(routes, API_PREFIX).await
    }

    /// The same server for an API that lives under `prefix` (none, for a host that serves from its root).
    pub async fn start_at(routes: &[Value], prefix: &'static str) -> RouteServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}{prefix}", listener.local_addr().unwrap());
        let state = std::sync::Arc::new(std::sync::Mutex::new(RouteState {
            routes: routes.to_vec(),
            hits: vec![0; routes.len()],
            recorded: vec![],
            mode: None,
            authorizations: vec![],
        }));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (served, origin) = (state.clone(), base.clone());
        tokio::spawn(async move {
            tokio::pin!(stopped);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        tokio::spawn(serve(stream, served.clone(), origin.clone(), prefix));
                    }
                }
            }
        });
        RouteServer { base, state, stop: Some(stop) }
    }

    /// Every request completed since the last look, as `{method, url, body}` with the url relative
    /// to the API base and the body parsed where it is JSON.
    pub fn take(&self) -> Vec<Value> {
        std::mem::take(&mut self.state.lock().unwrap().recorded)
    }

    /// `Some(401 | 404 | 429)` makes every request fail so; `None` answers from the routes again.
    pub fn set_mode(&self, status: Option<u16>) {
        self.state.lock().unwrap().mode = status;
    }

    pub fn replace_routes(&self, routes: &[Value]) {
        let mut state = self.state.lock().unwrap();
        state.routes = routes.to_vec();
        state.hits = vec![0; routes.len()];
    }

    /// The `Authorization` header of every request, with the path it went to.
    pub fn authorizations(&self) -> Vec<(String, Option<String>)> {
        self.state.lock().unwrap().authorizations.clone()
    }
}

impl Drop for RouteServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

async fn serve(mut stream: tokio::net::TcpStream, state: std::sync::Arc<std::sync::Mutex<RouteState>>, origin: String, prefix: &'static str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut received = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(count) => received.extend_from_slice(&chunk[..count]),
        }
    };
    let head = String::from_utf8_lossy(&received[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let (method, target) = (request_line.next().unwrap_or("").to_string(), request_line.next().unwrap_or("").to_string());
    let header = |name: &str| lines.clone().find_map(|line| line.split_once(':').filter(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.trim().to_string()));
    let length: usize = header("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    let authorization = header("authorization");
    while received.len() < head_end + 4 + length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(count) => received.extend_from_slice(&chunk[..count]),
        }
    }
    let body_text = String::from_utf8_lossy(&received[head_end + 4..head_end + 4 + length]).into_owned();
    let relative = target.strip_prefix(prefix).unwrap_or(&target).to_string();
    // What to answer: a step of the first matching route, or a failure mode.
    let step = {
        let mut state = state.lock().unwrap();
        state.authorizations.push((relative.clone(), authorization));
        let index = state.routes.iter().position(|route| {
            route["method"].as_str().is_none_or(|wanted| wanted == method) && relative.contains(route["match"].as_str().unwrap_or("\u{0}"))
        });
        let step = match (state.mode, index) {
            (Some(status), _) => json!({"status": status, "retryAfter": if status == 429 { json!(30) } else { Value::Null }}),
            (None, None) => json!({"status": 599}),
            (None, Some(index)) => {
                let route = state.routes[index].clone();
                let hit = state.hits[index];
                state.hits[index] += 1;
                match route["seq"].as_array() {
                    Some(steps) => steps[hit.min(steps.len() - 1)].clone(),
                    None => route,
                }
            }
        };
        step
    };
    let recorded = json!({"method": method, "url": relative, "body": serde_json::from_str::<Value>(&body_text).unwrap_or(Value::Null)});
    if step["error"] == "request" {
        // The connection is cut without an answer, as a host that cannot be reached.
        state.lock().unwrap().recorded.push(recorded);
        return;
    }
    let status = step["status"].as_u64().unwrap_or(200);
    let mut body = step["body"].as_str().unwrap_or("").replace(FIXTURE_ORIGIN, &origin).replace("{origin}", origin.strip_suffix(prefix).unwrap_or(&origin));
    if step["truncated"] == true {
        // A body past the byte ceiling the diff read is cut at.
        body.push_str(&"\n".repeat((8 << 20) + 1 - body.len().min(8 << 20)));
    }
    let retry = step["retryAfter"].as_u64().map(|seconds| format!("retry-after: {seconds}\r\n")).unwrap_or_default();
    let retry = match step["location"].as_str() {
        Some(location) => format!("{retry}location: {}\r\n", location.replace("{origin}", origin.strip_suffix(prefix).unwrap_or(&origin))),
        None => retry,
    };
    let retry = match step["headers"].as_object() {
        Some(headers) => headers.iter().fold(retry, |text, (name, value)| format!("{text}{name}: {}\r\n", value.as_str().unwrap_or(""))),
        None => retry,
    };
    let head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{retry}connection: close\r\n\r\n", body.len());
    // Recorded before the answer goes out, so a taker that has seen the response has the record;
    // the request was received whole, which is all a record claims.
    state.lock().unwrap().recorded.push(recorded);
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ---- a server and a socket for the git workflow tests -------------------------------------------------------------

pub type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The server under test on loopback, with a connection that may write source control and one that may not.
pub struct Harness {
    pub address: std::net::SocketAddr,
    pub writer: String,
    pub reader: String,
    pub store: t3_server::persistence::Store,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Harness {
    pub async fn start(base: &std::path::Path, label: &str, providers: Option<t3_server::provider_registry::ProviderRegistry>) -> Harness {
        use t3_contracts::AuthEnvironmentScope;
        let store = t3_server::persistence::Store::open(base.join("rust-state.sqlite")).unwrap();
        let auth = t3_server::auth::AuthService::new(store.clone(), [9; 32], format!("t3_session_{label}"), "loopback-browser".into()).unwrap();
        let issue = |scopes: Vec<AuthEnvironmentScope>| {
            auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), chrono::Utc::now(), chrono::Duration::hours(1)).unwrap().1
        };
        let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
        let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
        let state = t3_server::transport::ApiState {
            store: store.clone(),
            auth,
            environment: json!({"environmentId":format!("{label}-fixture"),"label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
            config: None, settings: None, cors_origins: None, assets: None, providers, execution: None, workspace: None, terminals: None,
            discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, t3_server::transport::router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
        });
        Harness { address, writer, reader, store, stop: Some(stop), server: Some(server) }
    }

    pub async fn connect(&self, token: &str) -> Socket {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    pub async fn finish(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.server.take().unwrap().await.unwrap();
    }
}

/// A request answered once.
pub async fn unary(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    use futures_util::{SinkExt, StreamExt};
    let frame = json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]});
    socket.send(tokio_tungstenite::tungstenite::Message::Text(frame.to_string().into())).await.unwrap();
    loop {
        let message: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if message["_tag"] == "Exit" && message["requestId"] == id {
            return message["exit"].clone();
        }
    }
}

/// A request whose answer is a stream: every chunk is acknowledged, and the events and the exit are returned.
pub async fn stream(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> (Vec<Value>, Value) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let frame = json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]});
    socket.send(Message::Text(frame.to_string().into())).await.unwrap();
    let mut events = vec![];
    loop {
        let message: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        match message["_tag"].as_str() {
            Some("Chunk") if message["requestId"] == id => {
                events.extend(message["values"].as_array().unwrap().iter().cloned());
                socket.send(Message::Text(json!({"_tag":"Ack","requestId":id}).to_string().into())).await.unwrap();
            }
            Some("Exit") if message["requestId"] == id => return (events, message["exit"].clone()),
            _ => {}
        }
    }
}

pub fn error_of(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}

pub fn value_of(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}

/// A bare repository holding `main`, a `feature` branch with a file of its own, and that branch as
/// `refs/pull/7/head`, as hosts that publish the pull request ref do; and a checkout cloned from it
/// that knows the host by `url` under the remote `upstream`.
pub struct Checkout {
    pub origin: std::path::PathBuf,
    pub checkout: std::path::PathBuf,
    pub head_sha: String,
}

pub fn checkout_with_host(base: &std::path::Path, url: &str) -> Checkout {
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
    let origin = base.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--bare", "-q", "--initial-branch=main"]);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "--initial-branch=main"]);
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(&seed, &["push", "-q", &path(&origin), "HEAD:refs/heads/main"]);
    git(&seed, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(seed.join("feature.txt"), "feature\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "feature work"]);
    let head_sha = git(&seed, &["rev-parse", "HEAD"]);
    git(&seed, &["push", "-q", &path(&origin), "HEAD:refs/heads/feature", &format!("{head_sha}:refs/pull/7/head")]);
    let checkout = base.join("checkout");
    git(base, &["clone", "-q", &path(&origin), &path(&checkout)]);
    git(&checkout, &["remote", "add", "upstream", url]);
    Checkout { origin, checkout, head_sha }
}

/// Both built-in providers run the stand-in text writer; each records into a directory of its own.
pub fn text_settings(dir: &std::path::Path) -> t3_contracts::ServerSettings {
    let fake = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text-generation-provider.py");
    let instance = |driver: &str, enabled: bool, delegate: &str| {
        json!({"driver":driver,"enabled":enabled,"config":{"binaryPath":fake,"homePath":dir.join("provider-home")},"environment":[{"name":"T3_TEXTGEN_DIR","value":dir},{"name":"T3_FAKE_DELEGATE","value":delegate}]})
    };
    serde_json::from_value(json!({"providerInstances":{"codex":instance("codex", true, "codex"),"claudeAgent":instance("claudeAgent", false, "claude")}})).unwrap()
}

/// What call `call` of the stand-in text writer will answer.
pub fn text_plan(dir: &std::path::Path, call: usize, plan: Value) {
    std::fs::write(dir.join(format!("{call:03}.plan.json")), plan.to_string()).unwrap();
}
