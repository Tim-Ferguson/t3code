//! A real server on a loopback socket for provider management flows, over a temporary T3 home. Every
//! provider is either disabled or a fake executable at an absolute path inside that home, update tools are
//! fakes found only through a temporary search path, and nothing reads or writes outside the temp tree.
#![allow(dead_code)]
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::{Path, PathBuf}};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    config::NativeConfig,
    persistence::Store,
    acp_registry_support::Catalog,
    provider_manage::Management,
    provider_registry::ProviderRegistry,
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

pub type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Harness {
    /// Canonical, so paths compare with what the server reports.
    pub base: PathBuf,
    _directory: tempfile::TempDir,
    pub address: std::net::SocketAddr,
    pub registry: ProviderRegistry,
    pub settings: SettingsService,
    pub store: Store,
    /// `orchestration:read` + `providers:manage`.
    pub manager: String,
    /// `orchestration:read`.
    pub reader: String,
    /// Read and operate, but not manage providers.
    pub operator: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

/// `instances` is the `providerInstances` settings object. Real Codex and Claude are disabled unless it
/// names them, so the registry never resolves a provider binary on the developer's PATH.
pub async fn start(instances: impl FnOnce(&Path) -> Value, configure: impl FnOnce(&Path, &Management)) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let state_dir = base.join("userdata");
    std::fs::create_dir_all(&state_dir).unwrap();
    let mut merged = json!({"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false}});
    for (id, instance) in instances(&base).as_object().unwrap() {
        merged[id] = instance.clone();
    }
    let path = state_dir.join("settings.json");
    std::fs::write(&path, json!({"providerInstances":merged}).to_string()).unwrap();
    let mut options = SettingsOptions::file(path, ServerSecretStore::open(state_dir.join("secrets")).unwrap());
    options.watch = false;
    let settings = SettingsService::start(options).await.unwrap();
    let store = Store::open(state_dir.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_manage".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let manager = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::ProvidersManage]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let operator = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let environment = json!({"environmentId":"manage-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}});
    // The T3 home is `base`: tools install under `<base>/tools`, caches under `<base>/caches`. Management is
    // configured before the first probe, because a managed instance's first probe already reads the install.
    let management = Management::new(Some(base.join("tools")));
    configure(&base, &management);
    let catalog = Catalog::new(base.join("caches"), base.join("tools")).unwrap();
    let registry = ProviderRegistry::discover_with_management(&settings.snapshot().await.unwrap(), &base, Some(catalog), management).await.unwrap();
    // The configuration document wants a native config; build it from the same settings with every provider off.
    let mut disabled = serde_json::to_value(settings.snapshot().await.unwrap()).unwrap();
    for instance in disabled["providerInstances"].as_object_mut().unwrap().values_mut() {
        instance["enabled"] = json!(false);
    }
    let native = NativeConfig::from_settings(serde_json::from_value(disabled).unwrap(), &base, &base, &environment, &auth.descriptor()).await.unwrap();
    let mut snapshot = native.snapshot;
    snapshot["providers"] = json!(registry.snapshots());
    let state = ApiState {
        store: store.clone(),
        auth,
        environment,
        config: Some(snapshot),
        settings: Some(settings.clone()),
        cors_origins: None,
        assets: None,
        providers: Some(registry.clone()),
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: Some(t3_server::provider_auth_service::ProviderAuthService::new(registry.clone(), base.clone(), base.join("caches"), std::sync::Arc::new(|_| Box::pin(async { Ok(()) })))),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    Harness { base, _directory: directory, address, registry, settings, store, manager, reader, operator, stop: Some(stop), server: Some(server) }
}

impl Harness {
    pub async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address).into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    pub async fn finish(mut self) {
        self.settings.shutdown().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
    }
}

pub async fn send(socket: &mut Socket, value: Value) {
    socket.send(Message::Text(value.to_string().into())).await.unwrap();
}
/// How long any one step of a flow may wait for the server or a fake tool before the test fails naming it.
/// Every wait here is on a real milestone; this bound exists only so a regression that never delivers the
/// milestone fails with a message instead of hanging the whole suite.
pub const STEP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
/// Runs `step` under [`STEP_DEADLINE`]; on expiry the test fails with `what`.
pub async fn within<T>(what: impl std::fmt::Display, step: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(STEP_DEADLINE, step).await {
        Ok(value) => value,
        Err(_) => panic!("no progress within {STEP_DEADLINE:?} while {what}"),
    }
}
pub async fn next(socket: &mut Socket) -> Value {
    next_while(socket, "waiting for the next WebSocket message").await
}
/// [`next`], naming the step in the failure message.
pub async fn next_while(socket: &mut Socket, what: impl std::fmt::Display) -> Value {
    let message = within(what, socket.next()).await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
/// Runs a blocking closure (a FIFO open or read, which has no timeout of its own) on a thread the runtime
/// does not join, so a step that never completes fails the test at [`STEP_DEADLINE`] instead of keeping the
/// runtime from shutting down.
pub async fn blocking_step<T: Send + 'static>(what: impl std::fmt::Display, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = done.send(work());
    });
    within(what, finished).await.unwrap()
}
/// One unary request and its `Exit`. Boxed: debug-build futures of a long flow would otherwise overflow a stack.
/// A server stream holds back its next chunk until the previous one is acknowledged, so a stream cannot
/// share a socket with this: a chunk read here would never be acknowledged and its stream would go quiet.
/// That is refused loudly instead of discarded.
pub fn rpc<'a>(socket: &'a mut Socket, id: u64, tag: &'a str, payload: Value) -> futures_util::future::BoxFuture<'a, Value> {
    Box::pin(async move {
        send(socket, json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})).await;
        loop {
            let value = next_while(socket, format!("waiting for the `{tag}` (request {id}) reply")).await;
            assert_ne!(value["_tag"], "Chunk", "`{tag}` (request {id}) shares a socket with a stream: its chunk {value} would never be acknowledged. Subscribe on a socket of its own.");
            if value["_tag"] == "Exit" && value["requestId"] == id {
                return value["exit"].clone();
            }
        }
    })
}
pub fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}
pub fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}
pub fn defect(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Die", "{exit}");
    &exit["cause"][0]["defect"]
}
pub fn provider<'a>(providers: &'a Value, id: &str) -> &'a Value {
    providers.as_array().unwrap().iter().find(|provider| provider["instanceId"] == id).unwrap_or_else(|| panic!("no provider {id}"))
}

/// Write an executable file, creating its directory.
pub fn write_executable(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
/// A fake Codex CLI reporting `version`, installed the way npm lays out a global package:
/// `<prefix>/lib/node_modules/@openai/codex/bin/codex` linked from `<prefix>/bin/codex`.
/// Returns the path to run (`<prefix>/bin/codex`).
pub fn npm_codex(prefix: &Path, version: &str) -> PathBuf {
    let package_bin = prefix.join("lib/node_modules/@openai/codex/bin");
    std::fs::create_dir_all(&package_bin).unwrap();
    let script = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/provider-manage-codex.py")).unwrap();
    write_executable(&package_bin.join("codex"), &script);
    std::fs::write(package_bin.join("version"), version).unwrap();
    std::fs::create_dir_all(prefix.join("bin")).unwrap();
    std::os::unix::fs::symlink("../lib/node_modules/@openai/codex/bin/codex", prefix.join("bin/codex")).unwrap();
    prefix.join("bin/codex")
}
pub fn set_codex_version(prefix: &Path, version: &str) {
    std::fs::write(prefix.join("lib/node_modules/@openai/codex/bin/version"), version).unwrap();
}

/// A loopback npm registry answering `/<package>/latest` with a scripted version.
pub struct Registry {
    pub url: String,
    pub requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub version: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Registry {
    pub async fn start(version: Option<&str>) -> Self {
        use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let version = std::sync::Arc::new(std::sync::Mutex::new(version.map(str::to_owned)));
        type Shared = (std::sync::Arc<std::sync::atomic::AtomicUsize>, std::sync::Arc<std::sync::Mutex<Option<String>>>);
        async fn latest(State((requests, version)): State<Shared>, uri: axum::http::Uri) -> impl IntoResponse {
            // Anything else on a loopback port (a stray probe from another process) is not a registry lookup.
            // A panic here would only kill the connection task, so it answers 404 and is not counted.
            if !uri.path().ends_with("/latest") {
                return StatusCode::NOT_FOUND.into_response();
            }
            requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match version.lock().unwrap().clone() {
                Some(version) => (StatusCode::OK, json!({"name":"x","version":version}).to_string()).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        let app = Router::new().fallback(get(latest)).with_state((requests.clone(), version.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        Self { url, requests, version, task }
    }
    pub fn count(&self) -> usize {
        self.requests.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl Drop for Registry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---- scripted release archives and a loopback release host ----

/// One archive member: `kind` is `file`, `dir` or `symlink`; `name` is stored verbatim.
pub fn member(kind: &str, name: &str, contents: &str, mode: u32) -> Value {
    json!({"kind":kind,"name":name,"contents":contents,"mode":mode})
}
/// The members of a well-formed Codex package for `version`/`target`; its `bin/codex` reports `reported`.
pub fn package(version: &str, target: &str, reported: &str) -> Vec<Value> {
    vec![
        member("dir", "bin/", "", 0o755),
        member("dir", "codex-path/", "", 0o755),
        member("file", "codex-package.json", &json!({"layoutVersion":1,"version":version,"target":target,"entrypoint":"bin/codex"}).to_string(), 0o644),
        member("file", "bin/codex", &format!("#!/bin/sh\ncase \"$1\" in --version) echo 'codex-cli {reported}';; esac\nexit 0\n"), 0o755),
        member("file", "bin/codex-code-mode-host", "#!/bin/sh\nexit 0\n", 0o755),
        member("file", "codex-path/rg", "#!/bin/sh\nexit 0\n", 0o755),
    ]
}
/// A gzipped tar of exactly `members`, in order, built by Python's tarfile so that any name can be stored.
pub fn archive(members: &[Value]) -> Vec<u8> {
    let script = "import sys,json,tarfile,io\nspec=json.load(sys.stdin)\nbuf=io.BytesIO()\nwith tarfile.open(fileobj=buf,mode='w:gz') as t:\n for m in spec:\n  i=tarfile.TarInfo(m['name'])\n  i.mode=m['mode']\n  if m['kind']=='dir':\n   i.type=tarfile.DIRTYPE;t.addfile(i)\n  elif m['kind']=='symlink':\n   i.type=tarfile.SYMTYPE;i.linkname=m['contents'];t.addfile(i)\n  else:\n   d=m['contents'].encode();i.size=len(d);t.addfile(i,io.BytesIO(d))\nsys.stdout.buffer.write(buf.getvalue())\n";
    let mut child = std::process::Command::new("python3").args(["-I", "-c", script]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), Value::Array(members.to_vec()).to_string().as_bytes()).unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    output.stdout
}
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

pub enum Route {
    Body(Vec<u8>),
    /// Promises `declared` bytes, sends `body`, then drops the connection.
    Truncated { body: Vec<u8>, declared: usize },
    /// Sends `first` bytes, announces itself, and sends the rest only when released.
    Gated { body: Vec<u8>, first: usize, started: std::sync::Arc<tokio::sync::Notify>, release: std::sync::Arc<tokio::sync::Notify> },
}
pub struct Releases {
    pub url: String,
    pub hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    routes: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Route>>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Releases {
    pub async fn start() -> Self {
        use axum::{Router, body::Body, extract::State, http::{StatusCode, Uri}, response::{IntoResponse, Response}, routing::get};
        type Routes = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Route>>>>;
        let routes: Routes = Default::default();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        async fn serve(State((routes, hits)): State<(Routes, std::sync::Arc<std::sync::atomic::AtomicUsize>)>, uri: Uri) -> Response {
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let Some(route) = routes.lock().unwrap().get(uri.path()).cloned() else {
                return StatusCode::NOT_FOUND.into_response();
            };
            match &*route {
                Route::Body(body) => body.clone().into_response(),
                Route::Truncated { body, declared } => {
                    let body = body.clone();
                    let stream = futures_util::stream::iter(vec![Ok(axum::body::Bytes::from(body)), Err(std::io::Error::other("connection dropped"))]);
                    Response::builder().header("content-length", declared.to_string()).body(Body::from_stream(stream)).unwrap()
                }
                Route::Gated { body, first, started, release } => {
                    let (head, tail) = (body[..*first].to_vec(), body[*first..].to_vec());
                    let (started, release) = (started.clone(), release.clone());
                    let stream = futures_util::stream::unfold(0u8, move |step| {
                        let (head, tail, started, release) = (head.clone(), tail.clone(), started.clone(), release.clone());
                        async move {
                            match step {
                                0 => Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(head)), 1)),
                                1 => {
                                    started.notify_one();
                                    release.notified().await;
                                    Some((Ok(axum::body::Bytes::from(tail)), 2))
                                }
                                _ => None,
                            }
                        }
                    });
                    Response::builder().body(Body::from_stream(stream)).unwrap()
                }
            }
        }
        let app = Router::new().fallback(get(serve)).with_state((routes.clone(), hits.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        Self { url, hits, routes, task }
    }
    pub fn serve(&self, path: &str, route: Route) -> String {
        self.routes.lock().unwrap().insert(path.to_owned(), std::sync::Arc::new(route));
        format!("{}{path}", self.url)
    }
    pub fn count(&self) -> usize {
        self.hits.load(std::sync::atomic::Ordering::SeqCst)
    }
}
impl Drop for Releases {
    fn drop(&mut self) {
        self.task.abort();
    }
}
