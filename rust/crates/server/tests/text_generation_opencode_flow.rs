#![cfg(unix)]
//! One-shot text generation through OpenCode, 1.x and 2.x (`OpenCodeTextGeneration.ts`, `OpenCode2TextGeneration.ts`),
//! against the replaying stand-in server (`fixtures/opencode-fake-cli.py`) the instance is configured to use as its own
//! (`serverUrl`), so nothing is spawned. Every request the Rust side sends is held to the schema of the SDK the original
//! drives the server with (`opencode1-openapi.json`, `opencode2-openapi.json`) as well as to the recording. The 2.x
//! recordings are the original tests' own (`text-generation-opencode2.jsonl`, `tools/generate_text_generation_opencode_fixtures.mjs`).
//! No real `opencode` runs, HOME is inside the temp directory, and a failed replay is reported by the server's own status.
mod opencode_support;
use opencode_support::FakeServer;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use t3_server::{
    provider_registry::ProviderRegistry,
    text_generation::{Operation, Request, Runner, Shape, TextGenerationError},
    text_generation_run::{CliRunner, Options},
};

const SESSION_1: &str = "ses_text_1x";

struct Fixture {
    _directory: tempfile::TempDir,
    workspace: PathBuf,
    attachments: PathBuf,
    registry: ProviderRegistry,
    runner: CliRunner,
    server: FakeServer,
    /// A recording that ends with the server's exit: the exit happens when the connection to it is dropped.
    ends_with_exit: bool,
}
impl Fixture {
    async fn start(protocol: &str, version: &str, entries: Value) -> Self {
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-opencode-text-").tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let (workspace, home, attachments) = (base.join("workspace"), base.join("home"), base.join("attachments"));
        for path in [&workspace, &home, &attachments] {
            std::fs::create_dir_all(path).unwrap();
        }
        let text = entries.to_string().replace("<work>", &workspace.to_string_lossy());
        let entries: Value = serde_json::from_str(&text).unwrap();
        // A 2.x recording holds no status check; the instance's own is answered at once from short lists (an empty model
        // list is read again for five seconds).
        let list = |data: Value| json!({"location":{"directory":workspace},"data":data,"cursor":{}});
        let model = json!({"id":"big-pickle","modelID":"big-pickle","providerID":"opencode","name":"Big Pickle"});
        let canned = json!({"model.list":list(json!([model])),"agent.list":list(json!([])),"skill.list":list(json!([])),"command.list":list(json!([{"name":"init","description":"x"}]))});
        let server = FakeServer::start_canned(&base, "text_generation", protocol, version, &entries, Some("hunter2"), &canned);
        let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
            "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
            "opencode":{"driver":"opencode","enabled":true,"config":{"enabled":true,"binaryPath":base.join("unused"),"serverUrl":server.url,"serverPassword":"hunter2"},
                "environment":[{"name":"HOME","value":home},{"name":"XDG_DATA_HOME","value":home.join("share")},{"name":"OPENCODE_API_KEY","value":""}]}}}))
        .unwrap();
        let registry = ProviderRegistry::discover(&settings, &workspace).await.unwrap();
        let runner = CliRunner::for_registry(registry.clone(), Some(attachments.clone()));
        let ends_with_exit = entries.as_array().and_then(|entries| entries.last()).is_some_and(|entry| entry["type"] == "runtime_exit");
        Self { _directory: directory, workspace, attachments, registry, runner, server, ends_with_exit }
    }
    fn request(&self, operation: Operation, shape: Shape, model_selection: Value) -> Request {
        Request { operation, cwd: self.workspace.to_string_lossy().into_owned(), prompt: "Name this thread: repair the login".into(), shape, model_selection, attachments: vec![] }
    }
    fn title(&self, model: &str) -> Request {
        self.request(Operation::ThreadTitle, Shape::ThreadTitle, json!({"instanceId":"opencode","model":model}))
    }
    /// Runs a request; a request that failed says where the replay stood.
    async fn run(&self, request: Request) -> Result<Value, TextGenerationError> {
        let result = self.runner.run(request).await;
        if result.is_err() {
            eprintln!("replay status after a failed request: {}", self.server.replay_status());
        }
        result
    }
    /// Where the replay stands: it must have consumed every entry, and nothing may have mismatched.
    fn assert_replayed(&self) {
        let status = self.server.replay_status();
        assert_eq!(status["failure"], Value::Null, "{status}");
        let remaining = status["total"].as_u64().unwrap() - status["cursor"].as_u64().unwrap();
        assert!(remaining == 0 || (remaining == 1 && self.ends_with_exit), "{status}");
    }
    fn assert_untouched(&self) {
        let status = self.server.replay_status();
        assert_eq!((status["failure"].clone(), status["cursor"].clone()), (Value::Null, json!(0)), "{status}");
    }
}
fn expect(frame: Value) -> Value {
    json!({"type":"expect_outbound","frame":frame})
}
fn respond(operation: &str, data: Value) -> Value {
    json!({"type":"emit_inbound","frame":{"type":"sdk.response","operation":operation,"data":data}})
}
fn text_part(text: &str) -> Value {
    json!({"type":"text","text":text})
}
/// A part of an answer, as the SDK describes it.
fn text_out(text: &str) -> Value {
    json!({"id":"prt_1","sessionID":SESSION_1,"messageID":"msg_1","type":"text","text":text})
}
fn reasoning_out(text: &str) -> Value {
    json!({"id":"prt_2","sessionID":SESSION_1,"messageID":"msg_1","type":"reasoning","text":text,"time":{"start":1}})
}
fn created(id: &str) -> Value {
    respond("session.create", json!({"id":id,"slug":"quiet","version":"1.14.39","projectID":"global","directory":"<work>","title":"T3 Code generateThreadTitle","time":{"created":1,"updated":1}}))
}
/// A 1.x run: a session is created, a prompt is answered with `reply`.
fn one_x(operation: &str, prompt_input: Value, reply: Value) -> Value {
    json!([
        expect(json!({"type":"session.create","input":{"title":format!("T3 Code {operation}"),"permission":[{"permission":"*","pattern":"*","action":"deny"}],"directory":"<work>"}})),
        created(SESSION_1),
        expect(json!({"type":"session.prompt","input":prompt_input})),
        respond("session.prompt", reply),
    ])
}
fn assistant(error: Option<Value>) -> Value {
    let mut info = json!({"id":"msg_1","sessionID":SESSION_1,"role":"assistant","time":{"created":1},"parentID":"msg_0","modelID":"gpt-5","providerID":"openai","mode":"build","agent":"build",
        "path":{"cwd":"/w","root":"/w"},"cost":0,"tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}});
    if let Some(error) = error {
        info["error"] = error;
    }
    info
}
fn answer(parts: Vec<Value>) -> Value {
    json!({"info":assistant(None),"parts":parts})
}

#[tokio::test(flavor = "multi_thread")]
async fn one_x_runs_the_prompt_in_a_denying_session_and_joins_the_text_parts() {
    let entries = one_x(
        "generateThreadTitle",
        json!({"sessionID":SESSION_1,"model":{"providerID":"openai","modelID":"gpt-5"},"agent":"plan","variant":"high","parts":[text_part("Name this thread: repair the login")],"directory":"<work>"}),
        answer(vec![reasoning_out("ignored"), text_out("Sure: ```json\n{\"title\":\"Repair "), reasoning_out("ignored too"), text_out("login\"}\n```  ")]),
    );
    let fixture = Fixture::start("opencode-sdk.sse", "1.14.39", entries).await;
    let selection = json!({"instanceId":"opencode","model":"openai/gpt-5","options":[{"id":"agent","value":"plan"},{"id":"variant","value":"high"}]});
    let answer = fixture.run(fixture.request(Operation::ThreadTitle, Shape::ThreadTitle, selection)).await.unwrap();
    assert_eq!(answer, json!({"title":"Repair login","needsRefinement":false}));
    fixture.assert_replayed();
}

#[tokio::test(flavor = "multi_thread")]
async fn one_x_sends_only_the_images_it_can_hand_a_model_and_leaves_agent_and_variant_out_when_unset() {
    let fixture_files = ["shot.png", "icon.svg", "notes.txt"];
    let entries = one_x(
        "generateBranchName",
        json!({"sessionID":SESSION_1,"model":{"providerID":"openai","modelID":"gpt-5"},"parts":[text_part("<any>"), {"type":"file","mime":"image/png","filename":"shot.png","url":"<any>"}],"directory":"<work>"}),
        answer(vec![text_out("{\"branch\":\"repair-login\"}")]),
    );
    let fixture = Fixture::start("opencode-sdk.sse", "1.14.39", entries).await;
    let mut request = fixture.request(Operation::BranchName, Shape::Branch, json!({"instanceId":"opencode","model":"openai/gpt-5"}));
    for (index, name) in fixture_files.iter().enumerate() {
        let (mime, id) = match *name {
            "shot.png" => ("image/png", "thread-0123456789abcdef-one"),
            "icon.svg" => ("image/svg+xml", "thread-0123456789abcdef-two"),
            _ => ("text/plain", "thread-0123456789abcdef-three"),
        };
        let kind = if *name == "notes.txt" { "file" } else { "image" };
        let extension = name.rsplit('.').next().unwrap();
        std::fs::write(fixture.attachments.join(format!("{id}.{extension}")), "x").unwrap();
        request.attachments.push(json!({"type":kind,"id":id,"name":name,"mimeType":mime,"sizeBytes":1+index}));
    }
    // The svg is an image the model cannot be handed; the text file is not an image at all.
    let answer = fixture.run(request).await.unwrap();
    assert_eq!(answer, json!({"branch":"repair-login"}));
    fixture.assert_replayed();
}

#[tokio::test(flavor = "multi_thread")]
async fn one_x_failures_say_what_failed_and_keep_the_providers_words_for_the_provider_error() {
    let create_failure = json!([expect(json!({"type":"session.create"})), respond("session.create", json!({"status":500,"body":{"name":"UnknownError","data":{"message":"boom"}}}))]);
    let no_payload = json!([expect(json!({"type":"session.create"})), respond("session.create", Value::Null)]);
    let prompt_failure = |reply: Value| one_x("generateThreadTitle", json!({"sessionID":SESSION_1}), reply);
    let cases: Vec<(&str, Value, &str)> = vec![
        ("session.create fails", create_failure, "OpenCode session.create request failed."),
        ("session.create has no payload", no_payload, "OpenCode session.create returned no session payload."),
        ("prompt fails", prompt_failure(json!({"status":500,"body":{"name":"UnknownError","data":{"message":"boom"}}})), "OpenCode session.prompt request failed."),
        ("provider error with a message", prompt_failure(json!({"info":assistant(Some(json!({"name":"ProviderAuthError","data":{"providerID":"openai","message":"  Bad key  "}}))),"parts":[]})), "Bad key"),
        ("provider error with only a name", prompt_failure(json!({"info":assistant(Some(json!({"name":"MessageAbortedError","data":{"message":""}}))),"parts":[text_out("{}")]})), "MessageAbortedError"),
        ("no text", prompt_failure(answer(vec![reasoning_out("thinking"), text_out("  ")])), "OpenCode returned empty output."),
        ("not an answer", prompt_failure(answer(vec![text_out("no object here")])), "OpenCode returned invalid structured output."),
        ("wrong type", prompt_failure(answer(vec![text_out("{\"title\":3}")])), "OpenCode returned invalid structured output."),
    ];
    for (name, entries, detail) in cases {
        let fixture = Fixture::start("opencode-sdk.sse", "1.14.39", entries).await;
        let error: TextGenerationError = fixture.run(fixture.title("openai/gpt-5")).await.unwrap_err();
        assert_eq!(error.detail, detail, "{name}");
        assert_eq!(error.operation, "generateThreadTitle", "{name}");
        fixture.assert_replayed();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_x_needs_a_provider_slash_model_slug_and_a_server_it_can_use_before_it_creates_anything() {
    for slug in ["gpt-5", "/gpt-5", "openai/", "  "] {
        let fixture = Fixture::start("opencode-sdk.sse", "1.14.39", json!([])).await;
        let error = fixture.run(fixture.title(slug)).await.unwrap_err();
        assert_eq!(error.detail, "OpenCode model selection must use the 'provider/model' format.", "{slug:?}");
        fixture.assert_untouched();
    }
    // A server older than the minimum is refused with the reason, and no session is created on it.
    let fixture = Fixture::start("opencode-sdk.sse", "1.14.18", one_x("generateThreadTitle", json!({"sessionID":SESSION_1}), answer(vec![text_out("{}")]))).await;
    let error = fixture.run(fixture.title("openai/gpt-5")).await.unwrap_err();
    assert_eq!(error.detail, "OpenCode v1.14.18 is too old. Upgrade to v1.14.19 or newer.");
    fixture.assert_untouched();
}

fn recordings() -> Vec<(String, Vec<Value>)> {
    include_str!("fixtures/text-generation-opencode2.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .map(|row| (row["name"].as_str().unwrap().to_owned(), row["entries"].as_array().unwrap().clone()))
        .collect()
}
fn recording(name: &str) -> Value {
    json!(recordings().into_iter().find(|(candidate, _)| candidate == name).unwrap().1)
}

#[tokio::test(flavor = "multi_thread")]
async fn two_x_writes_a_title_in_a_temporary_session_that_asks_and_removes_it_afterwards() {
    let fixture = Fixture::start("opencode2-http.sse", "2.0.18", recording("title_generation")).await;
    let answer = fixture.run(fixture.title("opencode/big-pickle")).await.unwrap();
    assert_eq!(answer, json!({"title":"Fix OAuth Login Redirect Loop","needsRefinement":false}));
    // Every entry of the original's live recording was played in order, including the session's removal.
    fixture.assert_replayed();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_x_ends_at_once_when_the_stream_is_lost_or_the_execution_does_not_succeed() {
    for (name, detail) in [
        ("stream_lost", "The OpenCode event stream was lost."),
        // The provider's own error is kept out of the detail.
        ("refused", "OpenCode could not generate the text."),
        ("interrupted", "OpenCode stopped the generation."),
        // TODO(divergence): the original says "OpenCode ended the generation in a way this version cannot read." when its schema
        // rejects the frame; this port reads events by their fields and cannot tell, so it reports the readable reason.
        ("unreadable_end", "OpenCode stopped the generation."),
    ] {
        let fixture = Fixture::start("opencode2-http.sse", "2.0.18", recording(name)).await;
        let error = tokio::time::timeout(t3_server::test_support::HANG_BOUND, fixture.run(fixture.title("opencode/big-pickle"))).await.expect(name).unwrap_err();
        assert_eq!(error.detail, detail, "{name}");
        assert_eq!(error.operation, "generateThreadTitle", "{name}");
        fixture.assert_replayed();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_x_interrupts_a_generation_that_outlives_its_time_and_still_removes_the_session() {
    let session = "ses_f0ec85cf6ffebfalpvV2E9nocu";
    let mut entries: Vec<Value> = recording("stream_lost").as_array().unwrap().iter().filter(|entry| entry["type"] != "runtime_exit").cloned().collect();
    // Nothing more arrives; the request is interrupted before the session is removed.
    let removal = entries.split_off(entries.len() - 2);
    entries.push(expect(json!({"type":"session.interrupt","input":{"sessionID":session}})));
    entries.push(respond("session.interrupt", Value::Null));
    entries.extend(removal);
    let fixture = Fixture::start("opencode2-http.sse", "2.0.18", json!(entries)).await;
    fixture.registry.set_text_generation_options(Options { timeout: Duration::from_millis(800), ..Options::default() });
    let error = tokio::time::timeout(t3_server::test_support::HANG_BOUND, fixture.run(fixture.title("opencode/big-pickle"))).await.unwrap().unwrap_err();
    assert_eq!(error.detail, "OpenCode did not finish generating in time.");
    fixture.assert_replayed();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_x_needs_a_slug_and_hands_over_images_by_file_uri_with_their_variant() {
    let session = "ses_f0ec85cf6ffebfalpvV2E9nocu";
    let fixture = Fixture::start("opencode2-http.sse", "2.0.18", json!([])).await;
    let error = fixture.run(fixture.title("no-slash")).await.unwrap_err();
    assert_eq!(error.detail, "OpenCode model selection must use the 'provider/model' format.");
    fixture.assert_untouched();

    let mut entries = recording("title_generation").as_array().unwrap().clone();
    // The recording's create and prompt, with the variant the selection names and the image the request carries.
    for entry in entries.iter_mut() {
        match entry["frame"]["type"].as_str() {
            Some("session.create") if entry["type"] == "expect_outbound" => entry["frame"]["input"]["model"]["variant"] = json!("high"),
            Some("session.prompt") if entry["type"] == "expect_outbound" => {
                entry["frame"]["input"] = json!({"sessionID":session,"text":"<any>","files":[{"uri":"<any>","name":"shot.png"}]});
            }
            _ => {}
        }
    }
    let fixture = Fixture::start("opencode2-http.sse", "2.0.18", json!(entries)).await;
    std::fs::write(fixture.attachments.join("thread-0123456789abcdef-one.png"), "x").unwrap();
    let mut request = fixture.request(Operation::ThreadTitle, Shape::ThreadTitle, json!({"instanceId":"opencode","model":"opencode/big-pickle","options":[{"id":"variant","value":"high"}]}));
    request.attachments.push(json!({"type":"image","id":"thread-0123456789abcdef-one","name":"shot.png","mimeType":"image/png","sizeBytes":1}));
    fixture.run(request).await.unwrap();
    fixture.assert_replayed();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_x_rejects_an_ask_as_it_arrives_and_goes_on_to_the_reply() {
    let session = "ses_f0ec85cf6ffebfalpvV2E9nocu";
    let mut entries = recording("title_generation").as_array().unwrap().clone();
    let at = entries.iter().position(|entry| entry["label"] == "session.execution.started").unwrap() + 1;
    let asked = json!({"type":"emit_inbound","frame":{"type":"sdk.event","event":{"id":"evt_ask","type":"permission.asked","data":{"sessionID":session,"id":"per_1"}}}});
    let other = json!({"type":"emit_inbound","frame":{"type":"sdk.event","event":{"id":"evt_other","type":"permission.asked","data":{"sessionID":"ses_someone_else","id":"per_2"}}}});
    let reply = expect(json!({"type":"permission.reply","input":{"sessionID":session,"requestID":"per_1","decision":"reject","message":"Tools are not available while generating text."}}));
    entries.splice(at..at, [other, asked, reply, respond("permission.reply", Value::Null)]);
    let fixture = Fixture::start("opencode2-http.sse", "2.0.18", json!(entries)).await;
    let answer = fixture.run(fixture.title("opencode/big-pickle")).await.unwrap();
    assert_eq!(answer["title"], "Fix OAuth Login Redirect Loop");
    // Only the session's own ask was answered; the other session's was not.
    fixture.assert_replayed();
}
