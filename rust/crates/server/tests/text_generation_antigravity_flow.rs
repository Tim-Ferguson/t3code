#![cfg(unix)]
//! One-shot text generation through Antigravity (`AntigravityTextGeneration.ts`), against the scripted stand-in agent
//! (`fixtures/antigravity-turn-agent.py`): the session it is given (an empty temporary directory, no MCP servers, no client
//! file system), what makes the request fail (a tool call, a permission or question, a file written, output beyond the
//! limit, a model the account does not offer, a stalled helper), what is removed afterwards, and which profiles are
//! allowed to write text at all. No real Antigravity runs; HOME is inside the temp directory.
mod antigravity_support;
use antigravity_support::{install_fake_agent, registry, settings};
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_server::{
    provider_registry::ProviderRegistry,
    text_generation::{Operation, Request, Runner, Shape, TextGenerationError},
    text_generation_antigravity::available,
    text_generation_run::{CliRunner, Options},
};

const AGENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/antigravity-turn-agent.py");
const SESSION: &str = "5f1c1c52-3c42-4e5c-9a0b-0f6c2f9a3d11";

struct Fixture {
    _directory: tempfile::TempDir,
    workspace: PathBuf,
    profile: PathBuf,
    registry: ProviderRegistry,
    runner: CliRunner,
    log: PathBuf,
    launches: PathBuf,
}
impl Fixture {
    async fn start(mut script: Value) -> Self {
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-antigravity-text-").tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let agent = install_fake_agent(&base);
        script.as_object_mut().unwrap().entry("session_id").or_insert(json!(SESSION));
        script.as_object_mut().unwrap().entry("conversation_files").or_insert(json!(true));
        let (script_path, log, launches) = (base.join("script.json"), base.join("agent.jsonl"), base.join("launch.jsonl"));
        std::fs::write(&script_path, script.to_string()).unwrap();
        let environment = vec![
            json!({"name":"ANTIGRAVITY_FIXTURE_AGENT","value":AGENT}),
            json!({"name":"ANTIGRAVITY_FIXTURE_SCRIPT","value":script_path}),
            json!({"name":"ANTIGRAVITY_FIXTURE_LOG","value":log}),
            json!({"name":"ANTIGRAVITY_FIXTURE_LAUNCH_LOG","value":launches}),
        ];
        let settings = settings(&base, &agent, json!({}), environment);
        let (registry, _) = registry(&settings, &base, &workspace).await;
        let profile = registry.antigravity("antigravity").unwrap().directories().unwrap().0;
        let runner = CliRunner::for_registry(registry.clone(), None);
        Self { _directory: directory, workspace, profile, registry, runner, log, launches }
    }
    fn request(&self, model: &str) -> Request {
        Request {
            operation: Operation::ThreadTitle,
            cwd: self.workspace.to_string_lossy().into_owned(),
            prompt: "Name this thread: repair the login".into(),
            shape: Shape::ThreadTitle,
            model_selection: json!({"instanceId":"antigravity","model":model}),
            attachments: vec![],
        }
    }
    async fn title(&self) -> Result<Value, TextGenerationError> {
        self.runner.run(self.request("gemini-3.8-flash-high")).await
    }
    /// Every message the agent received from T3, in order.
    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).map(|entry| entry["in"].clone()).collect()
    }
    fn methods(&self) -> Vec<String> {
        self.received().iter().filter_map(|message| message["method"].as_str().map(str::to_owned)).collect()
    }
    fn conversation_files(&self) -> Vec<PathBuf> {
        let directory = self.profile.join("antigravity-acp").join("conversations");
        std::fs::read_dir(directory).map(|entries| entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect()).unwrap_or_default()
    }
    /// The directory the session was started in.
    fn session_cwd(&self) -> Option<PathBuf> {
        self.received().iter().find(|message| message["method"] == "session/new").and_then(|message| message["params"]["cwd"].as_str().map(PathBuf::from))
    }
    /// What a finished request leaves behind: no conversation files, no temporary directory.
    fn assert_cleaned(&self) {
        assert_eq!(self.conversation_files(), Vec::<PathBuf>::new());
        if let Some(cwd) = self.session_cwd() {
            assert!(!cwd.exists(), "{} was not removed", cwd.display());
        }
    }
}
fn chunk(text: &str) -> Value {
    json!({"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}})
}
fn reply(text: &str) -> Value {
    json!([chunk(text), {"end":"end_turn"}])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_is_written_in_an_empty_temporary_directory_and_the_session_is_removed() {
    let fixture = Fixture::start(json!({"prompts":[[chunk("{\"title\":\"  \\\"Repair Google "), chunk("login\\\"  \"}"), {"end":"end_turn"}]]})).await;
    let answer = fixture.title().await.unwrap();
    assert_eq!(answer, json!({"title":"  \"Repair Google login\"  ","needsRefinement":false}));
    let received = fixture.received();
    let initialize = received.iter().find(|message| message["method"] == "initialize").unwrap();
    assert_eq!(initialize["params"]["clientInfo"]["name"], "t3-code-text");
    assert_eq!(initialize["params"]["clientCapabilities"]["fs"], json!({"readTextFile":false,"writeTextFile":false}));
    let new = received.iter().find(|message| message["method"] == "session/new").unwrap();
    assert_eq!(new["params"]["mcpServers"], json!([]));
    let cwd = fixture.session_cwd().unwrap();
    assert_ne!(cwd, fixture.workspace, "the helper runs away from the project");
    assert!(cwd.file_name().unwrap().to_string_lossy().starts_with("t3-antigravity-text-"));
    // The mode is selected before the prompt, and the prompt carries the instruction ahead of the request.
    let methods = fixture.methods();
    let at = |name: &str| methods.iter().position(|method| method == name).unwrap_or_else(|| panic!("{name} in {methods:?}"));
    assert!(at("session/set_mode") < at("session/prompt"));
    assert_eq!(received.iter().find(|message| message["method"] == "session/set_mode").unwrap()["params"]["modeId"], "default");
    let prompt = received.iter().find(|message| message["method"] == "session/prompt").unwrap();
    assert_eq!(
        prompt["params"]["prompt"],
        json!([{"type":"text","text":"Use only the input below. Do not use tools, read or write files, run commands, or ask questions.\nReturn only the requested JSON object.\n\nName this thread: repair the login"}])
    );
    fixture.assert_cleaned();
    assert!(!fixture.conversation_files().iter().any(|path| path.to_string_lossy().contains(SESSION)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_object_inside_prose_is_found_and_the_answer_is_decoded_as_its_shape() {
    let fixture = Fixture::start(json!({"prompts":[reply("Here you go:\n```json\n{\"title\":\"Repair login\",\"needsRefinement\":true}\n```")]})).await;
    assert_eq!(fixture.title().await.unwrap(), json!({"title":"Repair login","needsRefinement":true}));
    fixture.assert_cleaned();
}

#[tokio::test(flavor = "multi_thread")]
async fn output_that_is_not_an_answer_fails_the_request_and_still_cleans_up() {
    for (output, detail) in [
        ("   ".to_owned(), "empty text generation output"),
        ("No JSON here".to_owned(), "Antigravity returned invalid structured output."),
        ("{\"title\":42}".to_owned(), "Antigravity returned invalid structured output."),
        ("x".repeat(128_001), "exceeded the output limit"),
    ] {
        let fixture = Fixture::start(json!({"prompts":[reply(&output)]})).await;
        let error = fixture.title().await.unwrap_err();
        assert!(error.detail.contains(detail), "{error}");
        assert_eq!(error.operation, "generateThreadTitle");
        fixture.assert_cleaned();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_work_and_questions_end_the_request() {
    let permission = json!({"request":{"method":"session/request_permission","params":{"toolCall":{"toolCallId":"t1","title":"Run","kind":"execute"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]}}});
    let read = json!({"request":{"method":"fs/read_text_file","params":{"path":"/etc/hosts"}}});
    let terminal = json!({"request":{"method":"terminal/create","params":{"command":"ls"}}});
    for (steps, detail) in [
        (json!([{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read files"}}, {"end":"end_turn"}]), "attempted tool work"),
        (json!([{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"Read files"}}, {"end":"end_turn"}]), "attempted tool work"),
        (json!([permission, chunk("{\"title\":\"x\"}"), {"end":"end_turn"}]), "requested a tool permission or user input"),
        (json!([read, chunk("{\"title\":\"x\"}"), {"end":"end_turn"}]), "requested a tool or user input"),
        (json!([terminal, chunk("{\"title\":\"x\"}"), {"end":"end_turn"}]), "requested a tool or user input"),
    ] {
        let fixture = Fixture::start(json!({"prompts":[steps]})).await;
        let error = fixture.title().await.unwrap_err();
        assert!(error.detail.contains(detail), "{error}");
        fixture.assert_cleaned();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_cancellation_does_not_return_partial_json_and_a_written_file_fails_the_request() {
    let fixture = Fixture::start(json!({"prompts":[[chunk("{\"title\":\"Partial title\"}"), {"end":"cancelled"}]]})).await;
    let error = fixture.title().await.unwrap_err();
    assert!(error.detail.contains("cancelled"), "{error}");
    fixture.assert_cleaned();

    let fixture = Fixture::start(json!({"prompts":[[{"write_file":"unexpected.txt"}, chunk("{\"title\":\"Ignored output\"}"), {"end":"end_turn"}]]})).await;
    let error = fixture.title().await.unwrap_err();
    assert!(error.detail.contains("wrote files"), "{error}");
    fixture.assert_cleaned();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_the_account_does_not_offer_fails_before_prompting() {
    let fixture = Fixture::start(json!({"prompts":[reply("{\"title\":\"x\"}")]})).await;
    let error = fixture.runner.run(fixture.request("not-in-the-account")).await.unwrap_err();
    assert!(error.detail.contains("select the Antigravity model"), "{error}");
    assert!(!fixture.methods().contains(&"session/prompt".to_owned()));
    fixture.assert_cleaned();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_is_selected_only_when_it_differs_and_the_default_alias_is_never_sent() {
    let fixture = Fixture::start(json!({"prompts":[reply("{\"title\":\"a\"}"), reply("{\"title\":\"b\"}"), reply("{\"title\":\"c\"}")]})).await;
    let selected = |fixture: &Fixture| -> Vec<Value> { fixture.received().iter().filter(|message| message["method"] == "session/set_config_option").map(|message| message["params"].clone()).collect() };
    // The current model is flash-low: asking for it changes nothing; asking for flash-high selects it.
    fixture.runner.run(fixture.request("gemini-3.8-flash-low")).await.unwrap();
    assert_eq!(selected(&fixture), Vec::<Value>::new());
    fixture.runner.run(fixture.request("gemini-3.8-flash-high")).await.unwrap();
    assert_eq!(selected(&fixture).iter().map(|params| params["value"].clone()).collect::<Vec<_>>(), [json!("gemini-3.8-flash-high")]);
    // The alias selects the manifest default when the account offers it, never its own name.
    fixture.runner.run(fixture.request("antigravity-default")).await.unwrap();
    let values: Vec<Value> = selected(&fixture).iter().map(|params| params["value"].clone()).collect();
    assert!(!values.contains(&json!("antigravity-default")), "{values:?}");
    fixture.assert_cleaned();

    // An account that does not offer the manifest default keeps the agent's own choice.
    let fixture = Fixture::start(json!({"models":[{"value":"gemini-9","name":"Nine"}],"current_model":"gemini-9","prompts":[reply("{\"title\":\"a\"}")]})).await;
    fixture.runner.run(fixture.request("antigravity-default")).await.unwrap();
    assert_eq!(selected(&fixture), Vec::<Value>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_id_that_is_not_a_native_one_is_refused_and_no_file_of_ours_is_removed() {
    let fixture = Fixture::start(json!({"session_id":"../../escape","conversation_files":false,"prompts":[reply("{\"title\":\"x\"}")]})).await;
    let outside = fixture.profile.join("antigravity-acp");
    std::fs::create_dir_all(outside.join("conversations")).unwrap();
    std::fs::write(outside.join("conversations").join("keep.meta"), "{}").unwrap();
    let error = fixture.title().await.unwrap_err();
    assert!(error.detail.contains("invalid text helper session ID"), "{error}");
    assert!(outside.join("conversations").join("keep.meta").exists());
    assert!(!fixture.methods().contains(&"session/prompt".to_owned()));
    if let Some(cwd) = fixture.session_cwd() {
        assert!(!cwd.exists());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_session_files_whose_metadata_names_another_workspace_are_kept() {
    let fixture = Fixture::start(json!({"conversation_files":false,"prompts":[reply("{\"title\":\"x\"}")]})).await;
    let directory = fixture.profile.join("antigravity-acp").join("conversations");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(format!("{SESSION}.meta")), json!({"cwd":"/somewhere/else"}).to_string()).unwrap();
    std::fs::write(directory.join(format!("{SESSION}.db")), "kept").unwrap();
    fixture.title().await.unwrap();
    assert_eq!(std::fs::read_to_string(directory.join(format!("{SESSION}.db"))).unwrap(), "kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_helper_times_out_and_is_cleaned_up() {
    let fixture = Fixture::start(json!({"prompts":[[{"hang":true}]]})).await;
    fixture.registry.set_text_generation_options(Options { timeout: std::time::Duration::from_millis(1500), ..Options::default() });
    let error = fixture.title().await.unwrap_err();
    assert!(error.detail.contains("timed out"), "{error}");
    fixture.assert_cleaned();
}

#[test]
fn only_a_profile_without_hooks_or_mcp_configuration_can_write_text() {
    for (name, value, expected) in [
        ("hooks.json", r#"{"default":{"PreInvocation":[{"command":"do-not-run"}]}}"#.to_owned(), false),
        ("mcp_config.json", r#"{"mcpServers":{"server":{"command":"do-not-run"}}}"#.to_owned(), false),
        ("hooks.json", "invalid JSON".to_owned(), false),
        ("mcp_config.json", "x".repeat(64_001), false),
        ("hooks.json", "{}".to_owned(), true),
        ("hooks.json", r#"{"hooks":{}}"#.to_owned(), true),
        ("mcp_config.json", r#"{"mcpServers":{}}"#.to_owned(), true),
        ("hooks.json", "[]".to_owned(), false),
        // A null section is read as absent, so the whole document (one key) is what is judged.
        ("hooks.json", r#"{"hooks":null}"#.to_owned(), false),
    ] {
        let profile = tempfile::Builder::new().prefix("t3code-rust-port-test-profile-").tempdir().unwrap();
        std::fs::create_dir_all(profile.path().join("config")).unwrap();
        std::fs::write(profile.path().join("config").join(name), &value).unwrap();
        assert_eq!(available(profile.path()).unwrap(), expected, "{name}: {}", &value[..value.len().min(60)]);
    }
    let empty = tempfile::Builder::new().prefix("t3code-rust-port-test-profile-").tempdir().unwrap();
    assert!(available(empty.path()).unwrap(), "a profile with no configuration can");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_with_global_hooks_never_starts_a_helper_and_is_listed_as_unable_to_write_text() {
    let fixture = Fixture::start(json!({"prompts":[reply("{\"title\":\"x\"}")]})).await;
    // The provider list says whether text can be written from the profile as it is.
    fixture.registry.refresh_instance("antigravity").await.unwrap();
    let supports = |registry: &ProviderRegistry| registry.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap()["supportsTextGeneration"].clone();
    assert_eq!(supports(&fixture.registry), json!(true));
    let config = fixture.profile.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("hooks.json"), r#"{"default":{"PreInvocation":[{"command":"do-not-run"}]}}"#).unwrap();
    fixture.registry.refresh_instance("antigravity").await.unwrap();
    assert_eq!(supports(&fixture.registry), json!(false));
    let error = fixture.title().await.unwrap_err();
    assert!(error.detail.contains("unavailable for profiles with global hooks or MCP configuration"), "{error}");
    assert!(!fixture.launches.exists(), "no process was started");
}
