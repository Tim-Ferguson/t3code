#![allow(dead_code)]
//! The harness the MCP tool tests share: a store, the scripted fake Codex, an execution service and the
//! MCP HTTP service.
use t3_server::test_support::HANG_BOUND;
use chrono::Utc;
use serde_json::{Value, json};
use std::{sync::Arc};
use t3_server::{
    execution::ExecutionService, launch::ThreadLaunchService, mcp_control::McpControlTools, mcp_http::McpHttpService, mcp_sessions::{CredentialRequest, McpSessionRegistry},
    persistence::Store, project::ProjectService, provider_registry::ProviderRegistry, secret_requests::SecretRequests,
    server_secret_store::ServerSecretStore, server_settings::{SettingsOptions, SettingsService},
};

pub struct Harness {
    pub _directory: tempfile::TempDir,
    pub store: Store,
    pub execution: ExecutionService,
    pub providers: ProviderRegistry,
    pub registry: McpSessionRegistry,
    pub address: std::net::SocketAddr,
    pub requests: std::path::PathBuf,
    pub settings: SettingsService,
    pub client: reqwest::Client,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
    http: McpHttpService,
}

pub struct Caller {
    pub header: String,
    pub session: String,
    pub thread: String,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with_environment(vec![]).await
    }

    /// `environment` is added to the fake Codex's own (a recording to serve, for instance).
    pub async fn with_environment(extra: Vec<(&str, String)>) -> Self {
        Self::with_instances(extra, vec![]).await
    }

    /// A second fake Codex instance, `codexChild`, whose environment is `child` (its own recording, for instance).
    pub async fn with_instances(extra: Vec<(&str, String)>, child: Vec<(&str, String)>) -> Self {
        let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).with_max_level(tracing::Level::WARN).try_init();
        let directory = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(directory.path()).unwrap();
        let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
        let workspace = cwd.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace,"defaultModelSelection":{"instanceId":"codex","model":"fixture-model"}}), Utc::now()).unwrap();
        let requests = cwd.join("codex-requests.jsonl");
        let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
        let mut instances = json!({
            "claudeAgent":{"driver":"claudeAgent","enabled":false},
            "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":std::iter::once(json!({"name":"FIXTURE_RPC_RECORD","value":requests})).chain([("CODEX_FIXTURE_WORKSPACE".to_owned(), workspace.to_string_lossy().into_owned()), ("T3CODE_CODEX_STREAM_FLUSH_MS".to_owned(), "600000".to_owned()), ("CODEX_FIXTURE_LOG".to_owned(), cwd.join("fixture.log").to_string_lossy().into_owned())].iter().map(|(name,value)| json!({"name":name,"value":value}))).chain(extra.iter().map(|(name,value)| json!({"name":name,"value":value}))).collect::<Vec<_>>()}});
        if !child.is_empty() {
            let environment: Vec<Value> = child.iter().map(|(name, value)| json!({"name":name,"value":value})).chain([json!({"name":"CODEX_FIXTURE_WORKSPACE","value":workspace}), json!({"name":"T3CODE_CODEX_STREAM_FLUSH_MS","value":"600000"}), json!({"name":"CODEX_FIXTURE_LOG","value":cwd.join("child-fixture.log")})]).collect();
            instances["codexChild"] = json!({"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":cwd.join("codex-child-home")},"environment":environment});
        }
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":instances})).unwrap();
        let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let registry = McpSessionRegistry::new("environment-1".parse().unwrap(), Some(address), Arc::new(|| Utc::now().timestamp_millis()), 10 * 60 * 1000);
        // The settings service the environment tools read and write, without a watcher on the file.
        let mut options = SettingsOptions::file(cwd.join("settings.json"), ServerSecretStore::open(cwd.join("secrets")).unwrap());
        options.watch = false;
        let settings = SettingsService::start(options).await.unwrap();
        let environment = json!({"environmentId":"environment-1","label":"Test server","platform":{"os":"linux","arch":"x64"},"serverVersion":"0.0.0-test","orchestrationProtocolVersion":2});
        let services = t3_server::mcp_control::McpServices { settings: Some(settings.clone()), environment: Some(environment), ..Default::default() };
        let http = McpHttpService::new(registry.clone(), None).with_controls(McpControlTools { store: store.clone(), execution: Some(execution.clone()), clock: Arc::new(Utc::now), services });
        let router = http.router();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
        });
        Self { _directory: directory, store, execution, providers, registry, address, requests, settings, client: reqwest::Client::new(), stop: Some(stop), server: Some(server), http }
    }

    /// A thread of the project whose first turn runs on the fake Codex and stays running.
    pub async fn running_thread(&self, id: &str, runtime_mode: &str, interaction_mode: &str) -> String {
        let launched = ThreadLaunchService::with_providers(self.store.clone(), self.providers.clone()).launch(json!({"commandId":format!("launch-{id}"),"threadId":id,"projectId":"project","title":format!("Thread {id}"),"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":runtime_mode,"interactionMode":interaction_mode,"workspaceStrategy":{"type":"root"},"initialMessage":{"messageId":format!("first-{id}"),"text":"[hold] first","attachments":[]}}), Utc::now()).unwrap();
        assert_eq!(launched["threadId"], id);
        self.until(id, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "running")).then_some(())).await;
        id.to_owned()
    }

    /// A thread whose first message is `prompt`, on `model`, left for the caller to follow.
    pub fn start_thread(&self, id: &str, model: &str, runtime_mode: &str, prompt: &str) {
        ThreadLaunchService::with_providers(self.store.clone(), self.providers.clone()).launch(json!({"commandId":format!("launch-{id}"),"threadId":id,"projectId":"project","title":format!("Thread {id}"),"modelSelection":{"instanceId":"codex","model":model},"runtimeMode":runtime_mode,"interactionMode":"default","workspaceStrategy":{"type":"root"},"initialMessage":{"messageId":format!("first-{id}"),"text":prompt,"attachments":[]}}), Utc::now()).unwrap();
    }

    pub async fn until<T>(&self, thread: &str, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        tokio::time::timeout(HANG_BOUND, async {
            loop {
                if let Some(view) = self.store.projection("thread", thread).unwrap() {
                    if let Some(found) = check(&view) {
                        return found;
                    }
                }
                let _ = events.recv().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a persisted event on {thread}: runs {} subagents {}", self.store.projection("thread", thread).unwrap().map_or(Value::Null, |view| json!(view["runs"].as_array().unwrap().iter().map(|run| json!({"id":run["id"],"status":run["status"],"cohort":run["delegatedCompletion"]})).collect::<Vec<_>>())), self.store.projection("thread", thread).unwrap().map_or(Value::Null, |view| view["subagents"].clone())))
    }

    /// A credential for `thread` as its provider session holds it, initialized as an MCP client.
    pub async fn caller(&self, thread: &str) -> Caller {
        let credential = self.registry.issue(CredentialRequest { thread_id: thread.parse().unwrap(), provider_instance_id: "codex".parse().unwrap(), browser_tools_available: Some(false), capabilities: None }).unwrap();
        let initialize = self.client.post(format!("http://{}/mcp", self.address)).header("authorization", &credential.authorization_header).header("accept", "application/json,text/event-stream").json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"tools-flow","version":"1"}}})).send().await.unwrap();
        let session = initialize.headers()["mcp-session-id"].to_str().unwrap().to_owned();
        Caller { header: credential.authorization_header, session, thread: thread.to_owned() }
    }

    /// The JSON-RPC response of one tool call.
    pub async fn rpc(&self, caller: &Caller, method: &str, params: Value) -> Value {
        self.client
            .post(format!("http://{}/mcp", self.address))
            .header("authorization", &caller.header)
            .header("accept", "application/json,text/event-stream")
            .header("mcp-session-id", &caller.session)
            .header("mcp-protocol-version", "2025-06-18")
            .json(&json!({"jsonrpc":"2.0","id":2,"method":method,"params":params}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// A successful call: its structured result, checked against the catalog's output schema.
    pub async fn ok(&self, caller: &Caller, name: &str, arguments: Value) -> Value {
        let response = self.rpc(caller, "tools/call", json!({"name":name,"arguments":arguments})).await;
        assert_eq!(response["result"]["isError"], false, "{name}: {response}");
        let structured = response["result"]["structuredContent"].clone();
        assert!(t3_server::mcp_tool_input::output_issues(name, &structured).is_empty(), "{name} result breaks its output schema: {:?} in {structured}", t3_server::mcp_tool_input::output_issues(name, &structured));
        structured
    }

    /// A call and the text the model reads: whether it failed, and what the content says.
    pub async fn text(&self, caller: &Caller, name: &str, arguments: Value) -> (bool, String) {
        let response = self.rpc(caller, "tools/call", json!({"name":name,"arguments":arguments})).await;
        (response["result"]["isError"] == true, response["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("{name}: {response}")).to_owned())
    }

    /// A credential for `thread` that carries the preview capability, as one of a thread with browser access has.
    pub async fn caller_with_preview(&self, thread: &str) -> Caller {
        let credential = self.registry.issue(CredentialRequest { thread_id: thread.parse().unwrap(), provider_instance_id: "codex".parse().unwrap(), browser_tools_available: Some(true), capabilities: None }).unwrap();
        let initialize = self.client.post(format!("http://{}/mcp", self.address)).header("authorization", &credential.authorization_header).header("accept", "application/json,text/event-stream").json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"tools-flow","version":"1"}}})).send().await.unwrap();
        let session = initialize.headers()["mcp-session-id"].to_str().unwrap().to_owned();
        Caller { header: credential.authorization_header, session, thread: thread.to_owned() }
    }

    /// A refused call: the original's `OrchestratorMcpFailure`.
    pub async fn refused(&self, caller: &Caller, name: &str, arguments: Value) -> Value {
        let response = self.rpc(caller, "tools/call", json!({"name":name,"arguments":arguments})).await;
        assert_eq!(response["result"]["isError"], true, "{name}: {response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// What the fake Codex was asked, in order.
    pub fn provider_requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.requests).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    pub fn turn_texts(&self, method: &str) -> Vec<String> {
        self.provider_requests().iter().filter(|request| request["method"] == method).map(|request| request["params"]["input"].as_array().into_iter().flatten().filter_map(|item| item["text"].as_str()).collect::<Vec<_>>().join("\n")).collect()
    }
    /// The first frame the fake Codex's recording refused, if any.
    pub fn mismatch(&self) -> Option<Value> {
        let log = std::fs::read_to_string(self._directory.path().join("fixture.log")).ok()?;
        log.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).find(|entry| entry.get("mismatch").is_some())
    }
    pub fn view(&self, thread: &str) -> Value {
        self.store.projection("thread", thread).unwrap().unwrap()
    }
    pub fn threads(&self) -> Vec<String> {
        self.store.projections("thread").unwrap().iter().map(|view| view["thread"]["id"].as_str().unwrap().to_owned()).collect()
    }
    /// Every stored event and projection as text, to prove a value is in none of it.
    pub fn stored_text(&self) -> String {
        self.store
            .read(|connection| {
                let mut text = String::new();
                for table in ["rust_application_events", "rust_projections"] {
                    let mut statement = connection.prepare(&format!("SELECT * FROM {table}"))?;
                    let columns = statement.column_count();
                    let mut rows = statement.query([])?;
                    while let Some(row) = rows.next()? {
                        for index in 0..columns {
                            if let Ok(value) = row.get::<_, String>(index) {
                                text.push_str(&value);
                            }
                        }
                    }
                }
                Ok(text)
            })
            .unwrap()
    }

    /// For a test that already shut the execution service down.
    pub async fn http_only_finish(mut self) {
        self.http.shutdown().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
    }

    pub async fn finish(mut self) {
        self.http.shutdown().await;
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

