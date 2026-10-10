#![cfg(unix)]
//! Antigravity turns through the real WebSocket transport, against a scripted stand-in for the agent
//! (`fixtures/antigravity-turn-agent.py`, run by `fixtures/antigravity-fake-cli.py`): the process and profile a
//! session is launched with and which credential reaches it, the workspace files it asks T3 to read and write, a
//! launch that wants a browser sign-in, and a Stop with the next turn reattaching to the saved session. Commands go
//! over one socket and each stream has a socket of its own. No real Antigravity runs; HOME is in the temp directory.
//! Tests wait on persisted events, never on sleeps.
mod antigravity_support;
mod grok_support;
mod provider_manage_support;
use antigravity_support::{install_fake_agent, registry, settings};
use chrono::Utc;
use grok_support::{Served, Socket, serve};
use provider_manage_support::{rpc, success, within};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_server::{execution::ExecutionService, launch::ThreadLaunchService, persistence::Store, project::ProjectService, provider_registry::ProviderRegistry};

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/antigravity-turn-agent.py");

struct Flow {
    _directory: tempfile::TempDir,
    served: Served,
    store: Store,
    execution: ExecutionService,
    providers: ProviderRegistry,
    thread: String,
    command: Socket,
    next: u64,
    log: PathBuf,
    launches: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
}
impl Flow {
    async fn start(script: impl FnOnce(&std::path::Path) -> Value, config: Value, extra_environment: Vec<Value>, runtime_mode: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let agent = install_fake_agent(&base);
        let (script_path, log, launches) = (base.join("script.json"), base.join("agent.jsonl"), base.join("launch.jsonl"));
        std::fs::write(&script_path, script(&workspace).to_string()).unwrap();
        let mut environment = vec![json!({"name":"ANTIGRAVITY_FIXTURE_AGENT","value":AGENT}), json!({"name":"ANTIGRAVITY_FIXTURE_SCRIPT","value":script_path}), json!({"name":"ANTIGRAVITY_FIXTURE_LOG","value":log}), json!({"name":"ANTIGRAVITY_FIXTURE_LAUNCH_LOG","value":launches})];
        environment.extend(extra_environment);
        let settings = settings(&base, &agent, config, environment);
        let (providers, state) = registry(&settings, &base, &workspace).await;
        let store = Store::open(base.join("rust-state.sqlite")).unwrap();
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
        let thread = ThreadLaunchService::with_providers(store.clone(), providers.clone())
            .launch(json!({"commandId":"launch","projectId":"project","title":"Antigravity","modelSelection":{"instanceId":"antigravity","model":"antigravity-default"},"runtimeMode":runtime_mode,"interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
            .unwrap()["threadId"]
            .as_str()
            .unwrap()
            .to_owned();
        let execution = ExecutionService::start(store.clone(), providers.clone());
        let served = serve(&store, &providers, &execution).await;
        let command = served.connect().await;
        Self { _directory: directory, served, store, execution, providers, thread, command, next: 0, log, launches, workspace, state }
    }
    fn view(&self) -> Value {
        self.store.projection("thread", &self.thread).unwrap().unwrap()
    }
    async fn dispatch(&mut self, command: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let exit = rpc(&mut self.command, id, "orchestration.dispatchCommand", command).await;
        success(&exit).clone()
    }
    async fn send(&mut self, text: &str) -> usize {
        let id = format!("message-{}", self.next + 1);
        self.dispatch(json!({"type":"message.dispatch","commandId":id,"threadId":self.thread,"messageId":id,"text":text,"attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})).await;
        self.view()["runs"].as_array().unwrap().len()
    }
    async fn until<T>(&self, what: &str, mut check: impl FnMut(&Value) -> Option<T>) -> T {
        let mut events = self.store.subscribe();
        within(format!("{what}; runs {}", self.view()["runs"]), async {
            loop {
                if let Some(found) = check(&self.view()) {
                    return found;
                }
                let _ = events.recv().await;
            }
        })
        .await
    }
    async fn run_status(&self, ordinal: usize, wanted: &str) {
        self.until(&format!("run {ordinal} to be {wanted}"), |view| (view["runs"].get(ordinal - 1)?["status"] == wanted).then_some(())).await
    }
    fn received(&self) -> Vec<Value> {
        lines(&self.log)
    }
    fn provider(&self) -> Value {
        self.providers.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap()
    }
    async fn finish(self) {
        self.served.stop().await;
        self.execution.shutdown().await;
    }
}
fn lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}
fn text(body: &str) -> Value {
    json!({"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":body}}})
}
fn method<'a>(received: &'a [Value], name: &str) -> Vec<&'a Value> {
    received.iter().filter(|entry| entry["in"]["method"] == name).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_runs_in_the_instances_profile_with_only_its_credential_and_teaches_the_provider_list() {
    let leaked = vec![json!({"name":"GEMINI_API_KEY","value":"leaked-from-the-instance"}), json!({"name":"GOOGLE_API_KEY","value":"leaked-too"}), json!({"name":"FIXTURE_LEAKED_CREDENTIAL","value":"not a credential variable"})];
    let mut flow = Flow::start(|_| json!({"prompts":[[text("Hello from Antigravity."), {"end":"end_turn"}]]}), json!({}), leaked, "full-access").await;
    assert_eq!(flow.send("Say hello").await, 1);
    flow.run_status(1, "completed").await;
    let received = flow.received();
    // The launch: initialize with the files the client serves, the sign-in method before the session, the attachments directory.
    let initialize = method(&received, "initialize")[0]["in"]["params"].clone();
    assert_eq!(initialize["clientCapabilities"], json!({"fs":{"readTextFile":true,"writeTextFile":true},"terminal":false}));
    assert_eq!(method(&received, "authenticate")[0]["in"]["params"], json!({"methodId":"oauth-personal"}));
    let order: Vec<&str> = received.iter().filter_map(|entry| entry["in"]["method"].as_str()).collect();
    assert_eq!(&order[..4], ["initialize", "authenticate", "session/new", "session/set_config_option"]);
    let session = &method(&received, "session/new")[0]["in"]["params"];
    assert_eq!(session["additionalDirectories"], json!([flow.state.join("attachments")]));
    // The process: private profile, forced file storage, the harness beside it, only the instance's own credential.
    let launch = lines(&flow.launches);
    assert_eq!(launch.len(), 1);
    let env = &launch[0]["env"];
    let profile = env["GEMINI_HOME"].as_str().unwrap();
    assert!(profile.starts_with(flow.state.join("providers/antigravity").to_str().unwrap()), "{profile}");
    assert_eq!(env["AGY_ACP_FORCE_FILE_STORAGE"], "1");
    assert_eq!(env["PYTHONUNBUFFERED"], "1");
    assert!(env["BROWSER"].as_str().unwrap().contains("__T3_ANTIGRAVITY_AUTH_URL__"));
    assert!(env["ANTIGRAVITY_HARNESS_PATH"].as_str().unwrap().ends_with("bin/localharness_external"));
    assert!(env["TMPDIR"].as_str().unwrap().contains("antigravity-tmp"));
    assert!(env["GEMINI_API_KEY"].is_null() && env["GOOGLE_API_KEY"].is_null(), "no credential of the server's or the instance's environment reaches the agent: {env}");
    assert_eq!(env["FIXTURE_LEAKED_CREDENTIAL"], "not a credential variable");
    assert_eq!(launch[0]["args"], json!(if cfg!(target_os = "linux") { vec!["--uid="] } else { vec![] }));
    let settings = std::fs::read_to_string(std::path::Path::new(profile).join("antigravity-acp/settings.json")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&settings).unwrap(), json!({"auth":{"type":"oauth-personal"}}));
    // The model: the provider-default alias selects the manifest's default model, the runtime mode the agent's mode.
    let options = method(&received, "session/set_config_option");
    assert_eq!(options.iter().map(|entry| (entry["in"]["params"]["configId"].clone(), entry["in"]["params"]["value"].clone())).collect::<Vec<_>>(), [(json!("model"), json!("gemini-3.8-flash-high")), (json!("mode"), json!("yolo"))]);
    // What the session proved is in the provider list.
    let snapshot = flow.provider();
    assert_eq!((snapshot["status"].clone(), snapshot["auth"]["status"].clone(), snapshot["version"].clone()), (json!("ready"), json!("authenticated"), json!("1.3.0")));
    assert!(snapshot["models"].as_array().unwrap().iter().any(|model| model["slug"] == "gemini-3.8-flash-high" && model["isDefault"] == true));
    assert_eq!(snapshot["workspaceSnapshots"][0]["cwd"], flow.workspace.to_str().unwrap());
    // The per-process directory goes with the session.
    let temp_parent = std::path::Path::new(env["TMPDIR"].as_str().unwrap()).to_owned();
    flow.finish().await;
    assert!(!temp_parent.exists(), "the runtime directory of the session was removed");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_api_key_method_passes_its_key_to_the_agent_and_never_writes_it_down() {
    let mut flow = Flow::start(|_| json!({"prompts":[[text("ok"), {"end":"end_turn"}]]}), json!({"authMethod":"gemini-api-key","apiKey":"sk-configured"}), vec![json!({"name":"GEMINI_API_KEY","value":"the-instances-own"})], "approval-required").await;
    flow.send("Hi").await;
    flow.run_status(1, "completed").await;
    let received = flow.received();
    assert_eq!(method(&received, "authenticate")[0]["in"]["params"], json!({"methodId":"gemini-api-key"}));
    let launch = lines(&flow.launches);
    assert_eq!(launch[0]["env"]["GEMINI_API_KEY"], "sk-configured", "the configured key replaces whatever the environment held");
    let profile = std::path::Path::new(launch[0]["env"]["GEMINI_HOME"].as_str().unwrap()).to_owned();
    let settings = std::fs::read_to_string(profile.join("antigravity-acp/settings.json")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&settings).unwrap(), json!({"auth":{"type":"gemini-api-key"}}));
    assert!(!settings.contains("sk-configured"));
    assert_eq!(flow.provider()["auth"], json!({"status":"authenticated","type":"gemini-api-key","label":"Gemini API key"}));
    // Nothing the server persisted or published holds the key.
    let persisted = flow.store.projection("thread", &flow.thread).unwrap().unwrap().to_string();
    assert!(!persisted.contains("sk-configured") && !flow.providers.snapshots().iter().any(|snapshot| snapshot.to_string().contains("sk-configured")));
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_reads_and_writes_the_workspace_through_t3_and_cannot_leave_it() {
    let mut flow = Flow::start(
        |workspace| {
            let outside = workspace.parent().unwrap().join("outside.txt");
            let ask = |method: &str, params: Value| json!({"request":{"method":method,"params":params}});
            json!({"prompts":[[
                ask("fs/write_text_file", json!({"path":workspace.join("notes/today.txt"),"content":"line one\nline two"})),
                ask("fs/write_text_file", json!({"path":workspace.join("today.txt"),"content":"line one\nline two\nline three"})),
                ask("fs/read_text_file", json!({"path":workspace.join("today.txt"),"line":2,"limit":1})),
                ask("fs/write_text_file", json!({"path":outside,"content":"escaped"})),
                ask("fs/read_text_file", json!({"path":workspace.join("../outside.txt")})),
                {"end":"end_turn"}]]})
        },
        json!({}),
        vec![],
        "full-access",
    )
    .await;
    flow.send("Work on the files").await;
    flow.run_status(1, "completed").await;
    let answers: Vec<Value> = flow.received().into_iter().filter(|entry| entry["in"]["method"].is_null() && entry["in"]["id"].as_i64().is_some_and(|id| id > 100)).map(|entry| entry["in"].clone()).collect();
    assert_eq!(answers.len(), 5);
    // A directory that does not exist yet cannot be told from a path that leads out.
    assert_eq!(answers[0]["error"]["code"], -32602);
    assert_eq!(answers[1]["result"], json!({}));
    assert_eq!(answers[2]["result"], json!({"content":"line two"}));
    for escaped in &answers[3..] {
        assert_eq!(escaped["error"]["code"], -32602);
        assert!(escaped["error"]["message"].as_str().unwrap().ends_with("is outside the session workspace."));
    }
    assert_eq!(std::fs::read_to_string(flow.workspace.join("today.txt")).unwrap(), "line one\nline two\nline three");
    assert!(!flow.workspace.parent().unwrap().join("outside.txt").exists());
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launch_that_wants_a_browser_sign_in_fails_with_the_message_that_says_so_and_forgets_the_account() {
    let mut flow = Flow::start(|_| json!({"sign_in":"refused","prompts":[]}), json!({}), vec![], "full-access").await;
    flow.send("Hello").await;
    flow.run_status(1, "failed").await;
    let failure = flow.view()["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "error").cloned().unwrap();
    assert!(failure["failure"]["message"].as_str().unwrap().contains("Sign in to Antigravity in Settings before you continue."), "{failure}");
    assert!(!failure.to_string().contains("accounts.google.com"), "the sign-in URL is not shown as an error");
    assert_eq!(flow.provider()["auth"]["status"], "unauthenticated");
    flow.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_cancels_the_prompt_in_the_same_session_and_a_restarted_server_resumes_it_in_a_new_process() {
    let mut flow = Flow::start(|_| json!({"prompts":[[text("Working slowly."), {"hang":true}],[text("Back again."), {"end":"end_turn"}],[text("After the restart."), {"end":"end_turn"}]]}), json!({}), vec![], "full-access").await;
    flow.send("Take your time").await;
    flow.until("the slow turn to start", |view| view["turnItems"].as_array()?.iter().any(|item| item["text"] == "Working slowly.").then_some(())).await;
    let run = flow.view()["runs"][0]["id"].clone();
    flow.dispatch(json!({"type":"run.interrupt","commandId":"stop","threadId":flow.thread,"runId":run})).await;
    flow.run_status(1, "interrupted").await;
    flow.send("Continue").await;
    flow.run_status(2, "completed").await;
    let received = flow.received();
    // The agent was asked to cancel, and the session went on in the same process: it is not created again.
    assert_eq!(method(&received, "session/cancel").len(), 1);
    assert_eq!(method(&received, "session/new").len(), 1);
    assert_eq!(method(&received, "session/resume").len() + method(&received, "session/load").len(), 0);
    let prompts = method(&received, "session/prompt");
    assert_eq!(prompts[0]["pid"], prompts[1]["pid"]);
    // A restarted server reattaches to the saved session with resume, never load (a load replays the history).
    let (store, providers, base_thread) = (flow.store.clone(), flow.providers.clone(), flow.thread.clone());
    let first_pid = prompts[0]["pid"].clone();
    flow.served.stop().await;
    flow.execution.shutdown().await;
    let execution = ExecutionService::start(store.clone(), providers.clone());
    execution.dispatch(&json!({"type":"message.dispatch","commandId":"message-after-restart","threadId":base_thread,"messageId":"message-after-restart","text":"After the restart","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
    let mut events = store.subscribe();
    let reached = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if store.projection("thread", &base_thread).unwrap().unwrap()["runs"].get(2).is_some_and(|run| run["status"] == "completed") {
                return;
            }
            let _ = events.recv().await;
        }
    })
    .await;
    if reached.is_err() {
        let view = store.projection("thread", &base_thread).unwrap().unwrap();
        panic!("the third run did not complete\nruns {}\nsessions {}\nitems {}\nagent {:?}", view["runs"], view["providerSessions"], view["turnItems"], lines(&flow.log).iter().map(|entry| (entry["pid"].clone(), entry["in"]["method"].clone())).collect::<Vec<_>>());
    }
    let received = lines(&flow.log);
    let resumes = method(&received, "session/resume");
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["in"]["params"]["sessionId"], "agy-turn-session");
    assert_ne!(resumes[0]["pid"], first_pid, "a new process resumed it");
    assert_eq!(method(&received, "session/load").len(), 0);
    assert_eq!(method(&received, "session/new").len(), 1);
    assert_eq!(method(&received, "authenticate").len(), 2, "each launch signs in with the configured method first");
    execution.shutdown().await;
}
