//! What the conversation and the composer show for each kind of turn item the providers now
//! emit. Two kinds of test: the real server with the scripted fake Codex (`tests/fixtures/
//! codex-provider.py`, whose tags are listed in `codex_runtime_flow.rs`), and a mounted app over
//! a hand-made projection for the shapes the fake cannot produce yet (ACP, cancelled runs,
//! background work). Waits are on text that only the awaited state can produce.
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::diff_tests::{Fixture, Harness, open_thread, rendered_text, seed, until};
use crate::runtime::transport_tests::{click_control, control, control_attribute, input_control};
use std::{cell::RefCell, rc::Rc};

fn state_of(props: &Synth) -> Store<UiModel> {
    props.state.borrow().unwrap()
}
fn projection(state: Store<UiModel>) -> Value {
    state.peek().thread.projection.clone().unwrap()
}
async fn create_thread(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, id: &str, runtime: &str, interaction: &str) {
    dom.in_scope(ScopeId::APP, || runtime::command(&props.transport, state, "thread.create", json!({"threadId":id,"projectId":"project","title":id,"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":runtime,"interactionMode":interaction,"branch":null,"worktreePath":null}))).unwrap();
    until(dom, id, |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.iter().any(|t| t.id.as_str() == id))).await;
}
fn send(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, text: &str) {
    input_control(dom, "Message", text);
    dom.in_scope(ScopeId::APP, || runtime::send_message(&props.transport, state));
}
fn item_of_type(state: Store<UiModel>, kind: &str) -> Option<Value> {
    state.peek().thread.projection.as_ref()?["turnItems"].as_array()?.iter().find(|item| item["type"] == kind).cloned()
}

#[tokio::test(flavor = "current_thread")]
async fn reasoning_todo_and_usage_show_while_the_turn_runs_and_nothing_keeps_working_after_it() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    send(&mut dom, &props, state, "Work [think][todo][usage][hold]");
    // The second thought's heading can only come from the second reasoning item.
    until(&mut dom, "both thoughts", |dom| rendered_text(dom).contains("Planning the change")).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Reading the task") && text.contains("Thought"), "{text}");
    assert!(text.contains("Working for"), "the run is going, so the timeline says so: {text}");
    until(&mut dom, "the plan and the usage", |dom| rendered_text(dom).contains("Plan updated") && control(dom, "Context window 0.1% used").is_some()).await;
    assert!(control(&dom, "Stop generation").is_some());

    click_control(&mut dom, "Stop generation");
    until(&mut dom, "the run to end", |dom| !rendered_text(dom).contains("Working for") && control(dom, "Stop generation").is_none()).await;
    let text = rendered_text(&dom);
    // The projection still calls the plan active, but nothing is marked as being worked on.
    let todo = item_of_type(state, "todo_list").unwrap();
    assert!(todo["steps"].as_array().unwrap().iter().all(|step| step["status"] == "completed"));
    assert!(text.contains("Plan updated") && text.contains("2/2") && !text.contains('\u{25c9}'), "{text}");
    assert!(text.contains("Thought") && !text.contains("Thinking"), "{text}");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_plan_is_accepted_by_implementing_it_and_the_next_message_names_it() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    create_thread(&mut dom, &props, state, "planner", "full-access", "plan").await;
    open_thread(&mut dom, &props, state, "planner").await;
    send(&mut dom, &props, state, "Make a plan [plan]");
    until(&mut dom, "the plan card and its follow-up", |dom| rendered_text(dom).contains("Proposed plan") && rendered_text(dom).contains("Plan ready") && control(dom, "Implement").is_some()).await;
    let plan = projection(state)["plans"].as_array().unwrap().iter().find(|plan| plan["kind"] == "proposed_plan").cloned().unwrap();
    assert_eq!(plan["status"], "active");
    assert!(control(&dom, "Refine").is_none(), "an empty box implements");

    // Feedback turns the same button into Refine and keeps the thread in plan mode.
    input_control(&mut dom, "Message", "Also add tests");
    assert!(control(&dom, "Refine").is_some() && control(&dom, "Implement").is_none());
    input_control(&mut dom, "Message", "");
    click_control(&mut dom, "Implement");
    until(&mut dom, "the implementation turn", |dom| rendered_text(dom).contains("PLEASE IMPLEMENT THIS PLAN:")).await;
    until(&mut dom, "the plan to be accepted", |_| projection(state)["plans"].as_array().unwrap().iter().any(|candidate| candidate["id"] == plan["id"] && candidate["status"] != "active")).await;
    let runs = projection(state)["runs"].as_array().unwrap().clone();
    assert_eq!(runs.last().unwrap()["sourcePlanRef"]["planId"], plan["id"], "{runs:?}");
    until(&mut dom, "the banner to go", |dom| !rendered_text(dom).contains("Plan ready")).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_approval_is_answered_once_in_the_originals_words_and_goes_when_answered() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    create_thread(&mut dom, &props, state, "asker", "approval-required", "default").await;
    open_thread(&mut dom, &props, state, "asker").await;
    send(&mut dom, &props, state, "Write it [file]");
    until(&mut dom, "the approval", |dom| control(dom, "Approve").is_some()).await;
    // Decline and Approve are the buttons; everything else is behind "More approval options".
    assert!(control(&dom, "Decline").is_some() && control(&dom, "Approve").is_some() && control(&dom, "More approval options").is_some());
    assert!(control(&dom, "Always allow this session").is_none());
    assert_ne!(control_attribute(&dom, "Approve", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"), "answerable while live");
    click_control(&mut dom, "Approve");
    // From the click until the server answers, neither button answers again.
    assert_eq!(control_attribute(&dom, "Approve", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert_eq!(control_attribute(&dom, "Decline", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    until(&mut dom, "the answer", |dom| !rendered_text(dom).contains("File change approval")).await;
    let request = projection(state)["runtimeRequests"].as_array().unwrap().first().cloned().unwrap();
    assert_eq!(request["status"], "resolved");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_codex_withdraws_stops_being_answerable_and_reads_as_cancelled() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    create_thread(&mut dom, &props, state, "asker", "approval-required", "default").await;
    open_thread(&mut dom, &props, state, "asker").await;
    send(&mut dom, &props, state, "Write it [withdraw]");
    until(&mut dom, "the withdrawn request", |_| item_of_type(state, "approval_request").is_some_and(|item| item["status"] == "cancelled")).await;
    until(&mut dom, "the cancelled row", |dom| rendered_text(dom).contains("cancelled")).await;
    assert!(control(&dom, "Approve").is_none(), "a withdrawn request is not offered: {}", rendered_text(&dom));
    until(&mut dom, "the request to follow its item", |_| projection(state)["runtimeRequests"].as_array().unwrap().iter().all(|request| request["status"] != "pending")).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_retries_and_a_usage_limit_read_as_the_original_words_them() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    send(&mut dom, &props, state, "Flaky [retry]");
    until(&mut dom, "the recovery", |dom| rendered_text(dom).contains("Provider recovered (2/5 retries)")).await;
    send(&mut dom, &props, state, "Too much [usage-limit]");
    until(&mut dom, "the usage limit", |dom| rendered_text(dom).contains("Usage limit reached. Retry after")).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_and_dynamic_tools_are_named_for_their_tool() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    send(&mut dom, &props, state, "Use tools [mcp-tool]");
    until(&mut dom, "both tool rows", |dom| rendered_text(dom).contains("Create issue") && rendered_text(dom).contains("Fixture.lookup")).await;
    // A tool call's input and output are not built until the row is opened.
    assert!(!rendered_text(&dom).contains("\"title\": \"Fixture\""));
    click_control(&mut dom, "Create issue");
    // The call's input is shown; its output was withheld by the server and is fetched on request.
    assert!(rendered_text(&dom).contains("\"title\": \"Fixture\"") && rendered_text(&dom).contains("Load full output"), "{}", rendered_text(&dom));
    click_control(&mut dom, "Create issue");
    assert!(!rendered_text(&dom).contains("\"title\": \"Fixture\""), "{}", rendered_text(&dom));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_message_sent_while_the_turn_runs_is_queued_and_can_be_edited_and_removed() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    send(&mut dom, &props, state, "First [hold]");
    until(&mut dom, "the turn to run", |dom| control(dom, "Stop generation").is_some()).await;

    input_control(&mut dom, "Message", "second");
    assert!(control(&dom, "Queue message").is_some(), "a running thread queues by default");
    click_control(&mut dom, "Queue message");
    until(&mut dom, "the queue", |dom| control(dom, "1 queued message").is_some() && rendered_text(dom).contains("second")).await;

    // Edit it in the composer: the box holds its text and the send button saves instead.
    click_control(&mut dom, "Edit queued message");
    assert_eq!(state.peek().draft, "second");
    assert!(control(&dom, "Update queued message").is_some() && control(&dom, "Cancel editing queued message").is_some());
    input_control(&mut dom, "Message", "second, edited");
    click_control(&mut dom, "Update queued message");
    until(&mut dom, "the edit", |_| projection(state)["messages"].as_array().unwrap().iter().any(|message| message["text"] == "second, edited")).await;
    until(&mut dom, "the composer to settle", |dom| control(dom, "Update queued message").is_none() && state.peek().draft.is_empty() && rendered_text(dom).contains("second, edited")).await;

    // Cancelling an edit puts back what was being written.
    input_control(&mut dom, "Message", "unsent thought");
    click_control(&mut dom, "Edit queued message");
    assert_eq!(state.peek().draft, "second, edited");
    click_control(&mut dom, "Cancel editing queued message");
    assert_eq!(state.peek().draft, "unsent thought");

    click_control(&mut dom, "Remove queued message");
    until(&mut dom, "the queue to empty", |dom| control(dom, "1 queued message").is_none()).await;
    let runs = projection(state)["runs"].as_array().unwrap().clone();
    assert_eq!(runs.last().unwrap()["status"], "cancelled", "{runs:?}");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_command_left_running_after_its_run_ended_is_background_work_with_a_stop() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    send(&mut dom, &props, state, "[background] start the server");
    until(&mut dom, "the run to end with the command still going", |dom| rendered_text(dom).contains("Running: bun run dev") && !rendered_text(dom).contains("Working for")).await;
    let shown = rendered_text(&dom);
    assert!(shown.contains("bun run dev") && !shown.contains("Working for"), "the run is over, only its command is not: {shown}");
    assert!(item_of_type(state, "command_execution").is_some_and(|item| item["status"] == "running"));
    assert!(control(&dom, "Stop generation").is_none());
    click_control(&mut dom, "Stop background work");
    until(&mut dom, "the work to end", |dom| !rendered_text(dom).contains("Running: bun run dev")).await;
    until(&mut dom, "the item to be stopped", |_| item_of_type(state, "command_execution").is_some_and(|item| item["status"] == "interrupted")).await;
    until(&mut dom, "the row to say it was stopped", |dom| rendered_text(dom).contains("interrupted")).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_model_picker_opens_from_its_shortcut_searches_jumps_and_closes_on_escape() {
    use crate::runtime::keys_tests::{chord, key_on, press};
    let fixture = Fixture::start_with(false, &[("FIXTURE_SECOND_MODEL", "1")]).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert!(control(&dom, "Search models").is_none());
    assert_eq!(press(&mut dom, chord("m", true, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.toggle"));
    until(&mut dom, "the picker", |dom| control(dom, "Search models").is_some()).await;
    assert!(control(&dom, "Codex · Fixture Model").is_some() && control(&dom, "Codex · Fixture Alternate").is_some());

    input_control(&mut dom, "Search models", "altern");
    assert!(control(&dom, "Codex · Fixture Alternate").is_some() && control(&dom, "Codex · Fixture Model").is_none());
    input_control(&mut dom, "Search models", "zzzzzz");
    assert!(rendered_text(&dom).contains("No models found"));
    key_on(&mut dom, "Search models", Key::Escape);
    assert!(control(&dom, "Search models").is_none(), "Escape closes the picker");

    // With the picker open, the number of a row picks it.
    assert_eq!(press(&mut dom, chord("m", true, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.toggle"));
    until(&mut dom, "the picker again", |dom| control(dom, "Search models").is_some()).await;
    assert_eq!(press(&mut dom, chord("2", false, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.jump.2"));
    until(&mut dom, "the second model", |dom| control(dom, "Model").and_then(|(_, value)| value).is_some_and(|value| value.contains("fixture-model-alt"))).await;
    assert!(control(&dom, "Search models").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_slash_menu_offers_commands_and_picks_with_the_keyboard_and_a_mention_finds_files() {
    use crate::runtime::keys_tests::key_on;
    use t3_contracts::AuthEnvironmentScope::{FilesystemRead, OrchestrationOperate, OrchestrationRead};
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.client(Gate::default(), &[OrchestrationRead, OrchestrationOperate, FilesystemRead]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert!(control(&dom, "Suggestions").is_none());
    input_control(&mut dom, "Message", "/mod");
    assert!(control(&dom, "Suggestions").is_some() && rendered_text(&dom).contains("Switch response model for this thread"), "{}", rendered_text(&dom));
    key_on(&mut dom, "Message", Key::Escape);
    assert!(control(&dom, "Suggestions").is_none(), "Escape closes the menu");
    input_control(&mut dom, "Message", "/mode");
    assert!(control(&dom, "Suggestions").is_some(), "more typing asks again");
    key_on(&mut dom, "Message", Key::Enter);
    assert_eq!(state.peek().draft, "", "the command was taken out of the prompt");

    input_control(&mut dom, "Message", "look at @READ");
    until(&mut dom, "the file", |dom| rendered_text(dom).contains("README.md")).await;
    key_on(&mut dom, "Message", Key::Enter);
    assert_eq!(state.peek().draft, "look at @README.md ");
    assert!(control(&dom, "Suggestions").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_messages_are_reordered_from_the_keyboard_and_a_held_queue_is_resumed() {
    use crate::runtime::keys_tests::key_on;
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    send(&mut dom, &props, state, "First [hold]");
    until(&mut dom, "the turn to run", |dom| control(dom, "Stop generation").is_some()).await;
    for text in ["second", "third"] {
        input_control(&mut dom, "Message", text);
        click_control(&mut dom, "Queue message");
        until(&mut dom, text, |_| t3_client::timeline_items::queued_runs(&projection(state)).iter().any(|run| run.text == text)).await;
    }
    let order = || t3_client::timeline_items::queued_runs(&projection(state)).into_iter().map(|run| run.text).collect::<Vec<_>>();
    assert_eq!(order(), ["second", "third"]);
    key_on(&mut dom, "Reorder queued message (drag, or press the arrow keys)", Key::ArrowDown);
    until(&mut dom, "the new order", |_| order() == ["third", "second"]).await;

    // Stopping holds the queue; resuming delivers it.
    click_control(&mut dom, "Stop generation");
    until(&mut dom, "the held queue", |dom| control(dom, "Resume thread").is_some()).await;
    click_control(&mut dom, "Resume thread");
    until(&mut dom, "the queue to be delivered", |dom| rendered_text(dom).contains("Fixture response: third")).await;
    fixture.finish(dom).await;
}

// ---- Shapes the fake provider cannot produce yet, over a hand-made projection. ----

#[derive(Clone)]
struct Synth {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    probe: RenderProbe,
    items: Value,
    runs: Value,
    extra: Value,
}
fn synth_app(props: Synth) -> Element {
    use_context_provider(|| props.probe.clone());
    let state = use_store(|| {
        let thread: t3_contracts::ThreadShell = serde_json::from_value(json!({
            "createdBy":"user","creationSource":"web","id":"thread","projectId":"p","title":"Work",
            "providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"model"},
            "runtimeMode":"approval-required","interactionMode":"default","branch":null,"worktreePath":null,
            "lineage":if props.extra["lineage"].is_null() {json!({"parentThreadId":null,"relationshipToParent":null,"rootThreadId":"thread"})} else {props.extra["lineage"].clone()},
            "forkedFrom":null,"activeProviderThreadId":null,"latestRunId":null,"activeRunId":null,"status":"idle",
            "pendingRuntimeRequest":null,"latestVisibleMessage":null,"latestUserMessageAt":null,
            "hasActionableProposedPlan":false,"itemCount":1,"visibleItemCount":1,"pendingBackgroundTasks":props.extra["tasks"].clone(),
            "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z","archivedAt":null,
            "settledOverride":null,"settledAt":null,"deletedAt":null
        })).unwrap();
        let project = serde_json::from_value(json!({"id":"p","title":"P","workspaceRoot":"/p","defaultModelSelection":null,"scripts":[],"createdAt":"now","updatedAt":"now"})).unwrap();
        let mut model = UiModel::default();
        model.shell.snapshot = Some(t3_contracts::ShellSnapshot { schema_version: 2, snapshot_sequence: 1, projects: vec![project], threads: vec![thread], archived_threads: vec![] });
        model.active_thread = Some("thread".into());
        model.destination = Some(serde_json::from_value(json!("environment-a")).unwrap());
        let rows: Vec<_> = props.items.as_array().unwrap().iter().enumerate().map(|(index, item)| json!({"sourceThreadId":"thread","sourceItemId":item["id"],"position":index,"visibility":"local","item":item})).collect();
        model.thread.projection = Some(json!({"thread":{"id":"thread","activeProviderThreadId":null},"visibleTurnItems":rows,"turnItems":props.items,"runs":props.runs,"runtimeRequests":props.extra["requests"].clone(),"plans":props.extra["plans"].clone(),"messages":props.extra["messages"].clone(),"providerTurns":[],"providerThreads":[],"updatedAt":"2026-01-01T00:00:00Z"}));
        model
    });
    *props.state.borrow_mut() = Some(state);
    let transport = use_hook(runtime::TransportHandle::default);
    rsx! { Application { state, transport } }
}
fn synth(items: Value, runs: Value, extra: Value) -> (VirtualDom, Synth) {
    let props = Synth { state: Rc::new(RefCell::new(None)), probe: RenderProbe::default(), items, runs, extra: json!({"tasks":[],"requests":[],"plans":[],"messages":[],"lineage":null}).as_object().unwrap().iter().map(|(key, value)| (key.clone(), extra.get(key).cloned().unwrap_or(value.clone()))).collect() };
    let mut dom = VirtualDom::new_with_props(synth_app, props.clone());
    dom.rebuild_in_place();
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    (dom, props)
}
fn base(id: &str, kind: &str, status: &str) -> Value {
    json!({"id":id,"type":kind,"status":status,"threadId":"thread","runId":"run-1","ordinal":0,"updatedAt":"2026-01-01T00:00:00Z"})
}
fn with(mut item: Value, fields: Value) -> Value {
    for (key, value) in fields.as_object().unwrap() {
        item[key] = value.clone();
    }
    item
}
fn text(dom: &VirtualDom) -> String {
    rendered_text(dom)
}
const SETTLED: fn() -> Value = || json!([{"id":"run-1","status":"completed","ordinal":1}]);

#[tokio::test(flavor = "current_thread")]
async fn compaction_reads_as_a_divider_with_the_words_of_each_stage() {
    let (dom, _) = synth(json!([
        with(base("c1", "compaction", "running"), json!({"driver":"acp"})),
        with(base("c2", "compaction", "completed"), json!({"driver":"acp","beforeTokenCount":100000,"afterTokenCount":20000,"summary":"Kept the plan and the failing test."})),
        with(base("c3", "compaction", "completed"), json!({"driver":"codex"})),
    ]), SETTLED(), json!({}));
    let shown = text(&dom);
    assert!(shown.contains("Compacting context") && shown.contains("Context compacted 100K → 20K tokens") && shown.contains("Kept the plan and the failing test."), "{shown}");
    assert!(control(&dom, "Compacting context").is_some() && control(&dom, "Context compacted").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn reasoning_may_have_no_title_and_may_be_interrupted() {
    let (mut dom, _) = synth(json!([
        with(base("r1", "reasoning", "interrupted"), json!({"text":"Looking at the **failing** test. Then more.","streaming":false})),
        with(base("r2", "reasoning", "completed"), json!({"title":"Checked the logs","text":"Nothing odd.","streaming":false})),
    ]), SETTLED(), json!({}));
    let shown = text(&dom);
    assert!(shown.contains("Thought") && shown.contains("interrupted") && shown.contains("Looking at the failing test."), "{shown}");
    assert!(shown.contains("Checked the logs") && !shown.contains("Nothing odd."), "closed until asked: {shown}");
    click_control(&mut dom, "Thought Checked the logs");
    assert!(text(&dom).contains("Nothing odd."));
}

#[tokio::test(flavor = "current_thread")]
async fn tool_rows_carry_their_title_and_why_they_are_not_simply_done() {
    let (dom, _) = synth(json!([
        with(base("t1", "dynamic_tool", "completed"), json!({"toolName":"github_app.create_issue","title":"create issue","input":{"title":"x"},"output":[{"type":"text","text":"ok"}],"toolSource":{"key":"mcp:github_app","name":"github app","kind":"integration"}})),
        with(base("t2", "dynamic_tool", "waiting"), json!({"toolName":null,"title":"Action required","input":{}})),
        with(base("t3", "dynamic_tool", "completed"), json!({"toolName":null,"title":"Unsupported ACP update: usage_update","input":{}})),
        with(base("t4", "command_execution", "failed"), json!({"input":"cargo test","output":"boom","exitCode":101})),
        with(base("t5", "web_search", "completed"), json!({"patterns":["rust dioxus signals","dioxus 0.7"]})),
        with(base("t6", "file_change", "cancelled"), json!({"fileName":"src/lib.rs","additions":1,"deletions":0})),
        with(base("t7", "dynamic_tool", "interrupted"), json!({"toolName":"fixture.slow","input":{}})),
    ]), json!([{"id":"run-1","status":"cancelled","ordinal":1}]), json!({}));
    let shown = text(&dom);
    for expected in ["Create issue", "Action required", "waiting", "Unsupported ACP update: usage_update", "cargo test", "failed", "rust dioxus signals, dioxus 0.7", "src/lib.rs", "cancelled", "Fixture.slow", "interrupted"] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }
    assert!(!shown.contains("background"), "nothing here is left over from a finished run unless it is still going: {shown}");
}

#[tokio::test(flavor = "current_thread")]
async fn work_still_running_after_its_run_ended_is_background_work_not_a_turn_in_progress() {
    let (dom, _) = synth(json!([
        with(base("b1", "command_execution", "running"), json!({"input":"npm run dev","output":""})),
        with(base("b2", "todo_list", "running"), json!({"planId":"plan","steps":[{"id":"s","text":"Serve","status":"running"}]})),
    ]), json!([{"id":"run-1","status":"failed","ordinal":1}]), json!({"tasks":[{"kind":"command","taskId":"t","description":"Start the dev server"}]}));
    let shown = text(&dom);
    assert!(shown.contains("npm run dev") && shown.contains("background"), "{shown}");
    assert!(!shown.contains("Working for") && !shown.contains("Working...") && !shown.contains('\u{25c9}'), "{shown}");
    // The composer strip names what runs and offers Stop.
    assert!(shown.contains("Running: Start the dev server"), "{shown}");
    assert!(control(&dom, "Background work").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn a_running_item_of_the_active_run_is_working_and_says_so() {
    let (dom, _) = synth(json!([
        with(base("w1", "command_execution", "running"), json!({"input":"sleep 10","output":"partial output"})),
    ]), json!([{"id":"run-1","status":"running","ordinal":1,"startedAt":"2026-01-01T00:00:00Z"}]), json!({}));
    let shown = text(&dom);
    assert!(shown.contains("sleep 10") && shown.contains("running") && shown.contains("Working for"), "{shown}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_long_streaming_plan_collapses_only_once_it_is_complete() {
    let long = format!("# Big plan\n\n{}", (0..30).map(|i| format!("- step {i}")).collect::<Vec<_>>().join("\n"));
    let (mut dom, _) = synth(json!([
        with(base("p1", "proposed_plan", "running"), json!({"planId":"plan","markdown":long.clone(),"streaming":true})),
        with(base("p2", "proposed_plan", "completed"), json!({"planId":"plan2","markdown":long,"streaming":false})),
    ]), SETTLED(), json!({}));
    let shown = text(&dom);
    assert!(shown.contains("Big plan") && shown.contains("step 29"), "a plan still streaming shows everything: {shown}");
    assert_eq!(shown.matches("Expand plan").count(), 1, "{shown}");
    assert!(!shown.contains("step 25") || shown.matches("step 25").count() == 1);
    click_control(&mut dom, "Plan actions");
    let shown = text(&dom);
    assert!(shown.contains("Copy to clipboard") && shown.contains("Download as markdown"), "{shown}");
}

#[tokio::test(flavor = "current_thread")]
async fn an_agent_form_arrives_as_questions_and_a_message_question_can_be_dismissed() {
    let questions = json!([{"id":"q","header":"Pick","question":"Which one?","options":[{"label":"A","description":"first"},{"label":"B","description":"second"}]}]);
    let (dom, _) = synth(json!([
        with(base("u1", "user_input_request", "waiting"), json!({"requestId":"form","questions":questions.clone()})),
        with(base("u2", "user_input_request", "waiting"), json!({"requestId":"later","questions":questions,"responseMode":"message"})),
    ]), json!([{"id":"run-1","status":"waiting","ordinal":1}]), json!({"requests":[
        {"id":"form","status":"pending","kind":"user_input","responseCapability":{"type":"live"}},
        {"id":"later","status":"pending","kind":"user_input","responseCapability":{"type":"message"}}
    ]}));
    let shown = text(&dom);
    assert!(shown.contains("Which one?") && shown.contains("Your input is needed") && shown.contains("Dismiss"), "{shown}");
    assert!(control(&dom, "Submit answers").is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn an_approval_for_a_dead_provider_cannot_be_answered_and_says_why() {
    let (dom, _) = synth(json!([
        with(base("a1", "approval_request", "waiting"), json!({"requestId":"ask","requestKind":"mcp-elicitation","prompt":"Allow ChatGPT to use Fixture?","appName":"Fixture","options":[{"decision":"cancel","label":"Cancel"},{"decision":"decline","label":"Decline"},{"decision":"accept","label":"Allow once"},{"decision":"acceptForSession","label":"Allow for this session","warning":"Lasts until the thread ends"},{"decision":"acceptAlways","label":"Always allow","warning":"Applies to every thread"}]})),
    ]), json!([{"id":"run-1","status":"waiting","ordinal":1}]), json!({"requests":[{"id":"ask","status":"pending","kind":"mcp-elicitation","responseCapability":{"type":"not_resumable"}}]}));
    let shown = text(&dom);
    assert!(shown.contains("App access approval") && shown.contains("Fixture") && shown.contains("Provider process is gone — interrupt or restart the run to respond."), "{shown}");
    assert_eq!(control_attribute(&dom, "Allow once", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert!(control(&dom, "Decline").is_some() && control(&dom, "Allow once").is_some(), "the primary pair is Decline and the accepting option");
}

#[tokio::test(flavor = "current_thread")]
async fn an_unknown_item_and_a_subagent_never_break_the_conversation() {
    let (dom, _) = synth(json!([
        with(base("s1", "subagent", "running"), json!({"subagentId":"n","origin":"tool","driver":"codex","providerInstanceId":"codex","childThreadId":"child","prompt":"Review","result":null})),
        with(base("x1", "from_the_future", "completed"), json!({})),
        with(base("m1", "assistant_message", "completed"), json!({"messageId":"m","text":"Still here.","streaming":false})),
    ]), SETTLED(), json!({}));
    assert!(text(&dom).contains("Still here."));
}

#[tokio::test(flavor = "current_thread")]
async fn the_context_meter_reads_the_projection_and_is_absent_without_a_report() {
    let (mut dom, props) = synth(json!([]), SETTLED(), json!({}));
    assert!(control(&dom, "Context window 0.1% used").is_none());
    let state = state_of(&props);
    {
        let mut thread = state.thread();
        let mut model = thread.write();
        let projection = model.projection.as_mut().unwrap();
        projection["providerTurns"] = json!([{"id":"turn","status":"running","tokenUsage":{"usedTokens":150000,"maxTokens":200000,"updatedAt":"2026-01-01T00:00:00Z"}}]);
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(control(&dom, "Context window 75% used").is_some());
    click_control(&mut dom, "Context window 75% used");
    let shown = text(&dom);
    assert!(shown.contains("Context Window") && shown.contains("75% · 150k/200k"), "{shown}");
    assert!(control(&dom, "Context window usage").is_some());
}

// ---- Render isolation. ----

fn many(count: usize) -> Value {
    Value::Array((0..count).map(|i| with(base(&format!("m{i}"), "assistant_message", "completed"), json!({"messageId":format!("m{i}"),"text":"A settled message.","streaming":false}))).collect())
}

#[tokio::test(flavor = "current_thread")]
async fn streaming_into_a_thought_or_a_plan_rebuilds_only_that_row() {
    let mut items = many(300).as_array().unwrap().clone();
    items.push(with(base("think", "reasoning", "running"), json!({"text":"Reading","streaming":true})));
    items.push(with(base("plan", "proposed_plan", "running"), json!({"planId":"p","markdown":"# Plan\n\n1. First","streaming":true})));
    let (mut dom, props) = synth(Value::Array(items), json!([{"id":"run-1","status":"running","ordinal":1,"startedAt":"2026-01-01T00:00:00Z"}]), json!({}));
    let state = state_of(&props);
    let (rows, application, composer) = (props.probe.item.get(), props.probe.application.get(), props.probe.composer.get());
    for (index, (reasoning, plan)) in [("Reading the", "# Plan\n\n1. First\n2. Second"), ("Reading the task", "# Plan\n\n1. First\n2. Second\n3. Third")].into_iter().enumerate() {
        for (id, field, value) in [("think", "text", reasoning), ("plan", "markdown", plan)] {
            let mut thread = state.thread();
            let mut model = thread.write();
            let projection = model.projection.as_mut().unwrap();
            for list in ["turnItems", "visibleTurnItems"] {
                for row in projection[list].as_array_mut().unwrap() {
                    let item = if list == "turnItems" { row } else { &mut row["item"] };
                    if item["id"] == id {
                        item[field] = json!(value);
                    }
                }
            }
        }
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!(props.probe.item.get(), rows + 2 * (index + 1), "only the two streaming rows were built again");
    }
    assert_eq!((props.probe.application.get(), props.probe.composer.get()), (application, composer));
}

#[tokio::test(flavor = "current_thread")]
async fn an_approval_appearing_builds_its_own_row_and_the_panel_but_not_the_conversation() {
    let (mut dom, props) = synth(many(300), SETTLED(), json!({}));
    let state = state_of(&props);
    let (rows, application, composer) = (props.probe.item.get(), props.probe.application.get(), props.probe.composer.get());
    {
        let mut thread = state.thread();
        let mut model = thread.write();
        let projection = model.projection.as_mut().unwrap();
        let item = with(base("ask", "approval_request", "waiting"), json!({"requestId":"r","requestKind":"command","prompt":"rm -rf build","options":[{"decision":"decline","label":"Decline"},{"decision":"accept","label":"Approve"}]}));
        projection["turnItems"].as_array_mut().unwrap().push(item.clone());
        projection["visibleTurnItems"].as_array_mut().unwrap().push(json!({"sourceThreadId":"thread","sourceItemId":"ask","position":300,"visibility":"local","item":item}));
        projection["runtimeRequests"] = json!([{"id":"r","status":"pending","kind":"command","responseCapability":{"type":"live"}}]);
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(text(&dom).contains("Command approval") && text(&dom).contains("rm -rf build"));
    assert_eq!(props.probe.item.get(), rows + 1, "one new row");
    assert_eq!((props.probe.application.get(), props.probe.composer.get()), (application, composer));
}

#[tokio::test(flavor = "current_thread")]
async fn a_subagent_row_names_its_state_and_opens_its_thread_and_a_child_thread_is_read_only() {
    let items = json!([
        with(base("s1", "subagent", "completed"), json!({"title":"Review src/math.ts","subagentId":"n","origin":"tool","driver":"codex","providerInstanceId":"codex","childThreadId":"child","prompt":"Review the math","result":"Looks fine."})),
        with(base("s2", "subagent", "waiting"), json!({"subagentId":"n2","origin":"tool","driver":"codex","providerInstanceId":"codex","childThreadId":null,"prompt":"Wait for it"})),
    ]);
    let (mut dom, _) = synth(items, SETTLED(), json!({}));
    let shown = text(&dom);
    assert!(shown.contains("Review src/math.ts") && shown.contains("Completed") && shown.contains("Waiting"), "{shown}");
    click_control(&mut dom, "Review src/math.ts");
    assert!(control(&dom, "Open subagent thread").is_some() || text(&dom).contains("Open subagent thread"), "{}", text(&dom));
    assert!(text(&dom).contains("Looks fine."));

    // The child thread: no message box, a note saying why, and a way back to the parent.
    let (dom, _) = synth(json!([]), SETTLED(), json!({"lineage":{"parentThreadId":"parent","relationshipToParent":"subagent","rootThreadId":"parent"}}));
    let shown = text(&dom);
    assert!(shown.contains("This subagent is run by its provider and cannot take messages. Message the parent thread instead."), "{shown}");
    assert!(control(&dom, "Message").is_none() && control(&dom, "Open parent thread").is_some());
}
