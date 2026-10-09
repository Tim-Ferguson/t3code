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
    assert!(text.contains("Working for"), "the run is going, so the timeline says so: {text}");
    // The thoughts are one group under the live line; the earlier one is inside it.
    click_control(&mut dom, "Thinking");
    until(&mut dom, "the first thought in the open group", |dom| rendered_text(dom).contains("Reading the task")).await;
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
    // The turn settles, its work folds behind one line, and the two calls are one group.
    until(&mut dom, "the settled turn", |dom| control(dom, "Worked for*").is_some()).await;
    click_control(&mut dom, "Worked for*");
    until(&mut dom, "the group", |dom| control(dom, "Used*").is_some()).await;
    click_control(&mut dom, "Used*");
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
    // Two thoughts are one group, which says only how many.
    assert!(text(&dom).contains("Thought (×2)"), "{}", text(&dom));
    click_control(&mut dom, "Thought (×2)");
    let shown = text(&dom);
    assert!(shown.contains("Thought") && shown.contains("interrupted") && shown.contains("Looking at the failing test."), "{shown}");
    assert!(shown.contains("Checked the logs") && !shown.contains("Nothing odd."), "closed until asked: {shown}");
    click_control(&mut dom, "Thought Checked the logs");
    assert!(text(&dom).contains("Nothing odd."));
}

#[tokio::test(flavor = "current_thread")]
async fn tool_rows_carry_their_title_and_why_they_are_not_simply_done() {
    let (mut dom, _) = synth(json!([
        with(base("t1", "dynamic_tool", "completed"), json!({"toolName":"github_app.create_issue","title":"create issue","input":{"title":"x"},"output":[{"type":"text","text":"ok"}],"toolSource":{"key":"mcp:github_app","name":"github app","kind":"integration"}})),
        with(base("t2", "dynamic_tool", "waiting"), json!({"toolName":null,"title":"Action required","input":{}})),
        with(base("t3", "dynamic_tool", "completed"), json!({"toolName":null,"title":"Unsupported ACP update: usage_update","input":{}})),
        with(base("t4", "command_execution", "failed"), json!({"input":"cargo test","output":"boom","exitCode":101})),
        with(base("t5", "web_search", "completed"), json!({"patterns":["rust dioxus signals","dioxus 0.7"]})),
        with(base("t6", "file_change", "cancelled"), json!({"fileName":"src/lib.rs","additions":1,"deletions":0})),
        with(base("t7", "dynamic_tool", "interrupted"), json!({"toolName":"fixture.slow","input":{}})),
    ]), json!([{"id":"run-1","status":"cancelled","ordinal":1}]), json!({}));
    // The calls are one group; its line counts them by kind, naming the integration that made one.
    assert!(text(&dom).contains("Used github app integration, ran 1 command, changed 1 file, and performed 3 other actions"), "{}", text(&dom));
    click_control(&mut dom, "Used github app integration*");
    let shown = text(&dom);
    for expected in ["Create issue", "Unsupported ACP update: usage_update", "cargo test", "failed", "rust dioxus signals, dioxus 0.7", "src/lib.rs", "cancelled", "Fixture.slow", "interrupted"] {
        assert!(shown.contains(expected), "{expected}: {shown}");
    }
    // A call that is only waiting is not an outcome, so the group leaves it out until it runs.
    assert!(!shown.contains("Action required"), "{shown}");
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
    let (mut dom, _) = synth(json!([
        with(base("w1", "command_execution", "running"), json!({"input":"sleep 10","output":"partial output"})),
    ]), json!([{"id":"run-1","status":"running","ordinal":1,"startedAt":"2026-01-01T00:00:00Z"}]), json!({}));
    let shown = text(&dom);
    assert!(shown.contains("Running sleep") && shown.contains("Working for"), "the live line names the call: {shown}");
    click_control(&mut dom, "Running sleep");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let shown = text(&dom);
    assert!(shown.contains("sleep 10") && shown.contains("running"), "opened, the call says where it is: {shown}");
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
    assert_eq!(props.probe.item.get(), rows, "the question is asked in the panel; waiting calls stay out of the conversation, as in the original");
    assert_eq!((props.probe.application.get(), props.probe.composer.get()), (application, composer));
}

#[tokio::test(flavor = "current_thread")]
async fn a_subagent_row_names_its_state_and_opens_its_thread_and_a_child_thread_is_read_only() {
    let items = json!([
        with(base("s1", "subagent", "completed"), json!({"title":"Review src/math.ts","subagentId":"n","origin":"tool","driver":"codex","providerInstanceId":"codex","childThreadId":"child","prompt":"Review the math","result":"Looks fine."})),
        with(base("s2", "subagent", "waiting"), json!({"subagentId":"n2","origin":"tool","driver":"codex","providerInstanceId":"codex","childThreadId":null,"prompt":"Wait for it"})),
    ]);
    let (mut dom, _) = synth(items, SETTLED(), json!({}));
    // Children launched by one provider turn are one group, which counts their states.
    let shown = text(&dom);
    assert!(shown.contains("2 subagents") && shown.contains("1 working \u{b7} 1 done"), "{shown}");
    assert!(!shown.contains("Review src/math.ts"), "closed until asked: {shown}");
    click_control(&mut dom, "2 subagents");
    let shown = text(&dom);
    assert!(shown.contains("Review src/math.ts") && shown.contains("Looks fine.") && shown.contains("Waiting"), "{shown}");
    assert!(control(&dom, "Open Review src/math.ts").is_some(), "a child with a thread opens it");
    assert!(control(&dom, "Open Subagent").is_none(), "one without a thread does not");

    // The child thread: no message box, a note saying why, and a way back to the parent.
    let (dom, _) = synth(json!([]), SETTLED(), json!({"lineage":{"parentThreadId":"parent","relationshipToParent":"subagent","rootThreadId":"parent"}}));
    let shown = text(&dom);
    assert!(shown.contains("This subagent is run by its provider and cannot take messages. Message the parent thread instead."), "{shown}");
    assert!(control(&dom, "Message").is_none() && control(&dom, "Open parent thread").is_some());
}

fn command(id: &str, input: &str, status: &str) -> Value {
    with(base(id, "command_execution", status), json!({"input":input,"exitCode":if status == "completed" { json!(0) } else { Value::Null },"output":"","startedAt":"2026-01-01T00:00:00Z"}))
}
fn user(id: &str, text: &str) -> Value {
    with(base(id, "user_message", "completed"), json!({"messageId":id,"text":text,"inputIntent":"turn_start","createdBy":"user","creationSource":"web"}))
}
fn assistant(id: &str, text: &str) -> Value {
    with(base(id, "assistant_message", "completed"), json!({"messageId":id,"text":text,"streaming":false}))
}
fn set_field(state: Store<UiModel>, id: &str, field: &str, value: Value) {
    let mut thread = state.thread();
    let mut model = thread.write();
    let projection = model.projection.as_mut().unwrap();
    for list in ["turnItems", "visibleTurnItems"] {
        for row in projection[list].as_array_mut().unwrap() {
            let item = if list == "turnItems" { row } else { &mut row["item"] };
            if item["id"] == id {
                item[field] = value.clone();
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_settled_turn_folds_its_work_behind_one_line_and_its_group_opens_to_the_calls() {
    let items = json!([
        user("u1", "Tidy the repo"),
        with(base("think", "reasoning", "completed"), json!({"text":"Looking around first."})),
        command("c1", "ls -la", "completed"),
        command("c2", "git status", "completed"),
        assistant("a1", "All tidy."),
    ]);
    let runs = json!([{"id":"run-1","status":"completed","ordinal":1,"startedAt":"2026-01-01T00:00:00Z","completedAt":"2026-01-01T00:01:30Z"}]);
    let (mut dom, _props) = synth(items, runs, json!({}));
    let shown = text(&dom);
    assert!(shown.contains("Tidy the repo") && shown.contains("All tidy."), "{shown}");
    assert!(shown.contains("Worked for 1m 30s"), "{shown}");
    assert!(!shown.contains("ls -la") && !shown.contains("Ran 2 commands"), "the work is folded away: {shown}");
    assert_eq!(control_attribute(&dom, "Worked for 1m 30s", "aria-expanded").and_then(|(_, value)| value).as_deref(), Some("false"), "the fold is a button with its state in the page");
    click_control(&mut dom, "Worked for 1m 30s");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let shown = text(&dom);
    assert!(shown.contains("Ran 2 commands") && !shown.contains("ls -la"), "the calls are one group, closed: {shown}");
    click_control(&mut dom, "Ran 2 commands");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let shown = text(&dom);
    assert!(shown.contains("ls -la") && shown.contains("git status"), "{shown}");
    // The same click closes the group, and the fold closes the lot.
    click_control(&mut dom, "Ran 2 commands");
    click_control(&mut dom, "Worked for 1m 30s");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(!text(&dom).contains("Ran 2 commands"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_running_turn_keeps_its_work_and_says_what_it_is_doing_now() {
    let items = json!([user("u1", "Build it"), command("c1", "cargo check", "completed"), command("c2", "cargo build --release", "running")]);
    let runs = json!([{"id":"run-1","status":"running","ordinal":1,"startedAt":"2026-01-01T00:00:00Z"}]);
    let (mut dom, props) = synth(items, runs, json!({}));
    let state = state_of(&props);
    let shown = text(&dom);
    assert!(shown.contains("Running cargo") && shown.contains("Working for"), "{shown}");
    assert!(!shown.contains("Worked for"), "a running turn is not folded: {shown}");
    // A call that finished successfully keeps the line while the turn goes on, as in the original.
    set_field(state, "c2", "status", json!("completed"));
    set_field(state, "c2", "exitCode", json!(0));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(text(&dom).contains("Running cargo"), "{}", text(&dom));
    // A last call that failed gives the row back to "Thinking", with its group still reachable.
    set_field(state, "c2", "status", json!("failed"));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(text(&dom).contains("Thinking"), "{}", text(&dom));
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_streaming_inside_a_group_rebuilds_that_call_and_the_summary_only_when_its_words_change() {
    let mut items = many(300).as_array().unwrap().clone();
    items.push(user("u1", "Build it"));
    items.push(command("c1", "cargo check", "completed"));
    items.push(command("c2", "cargo build --release", "running"));
    let runs = json!([{"id":"run-1","status":"running","ordinal":1,"startedAt":"2026-01-01T00:00:00Z"}]);
    let (mut dom, props) = synth(Value::Array(items), runs, json!({}));
    let state = state_of(&props);
    click_control(&mut dom, "Running cargo");
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(text(&dom).contains("cargo build --release"), "the group lists its calls: {}", text(&dom));
    let (rows, summaries, application, composer) = (props.probe.item.get(), props.probe.summary.get(), props.probe.application.get(), props.probe.composer.get());
    for (step, output) in ["Compiling a", "Compiling a\nCompiling b", "Compiling a\nCompiling b\nCompiling c"].into_iter().enumerate() {
        set_field(state, "c2", "output", json!(output));
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!(props.probe.item.get(), rows + step + 1, "only the streaming call was built again");
        assert_eq!(props.probe.summary.get(), summaries, "its words did not change, so neither did the line");
    }
    // The next call is another command, so the line names it.
    {
        let mut thread = state.thread();
        let mut model = thread.write();
        let projection = model.projection.as_mut().unwrap();
        let item = command("c3", "git status", "running");
        projection["turnItems"].as_array_mut().unwrap().push(item.clone());
        projection["visibleTurnItems"].as_array_mut().unwrap().push(json!({"sourceThreadId":"thread","sourceItemId":"c3","position":999,"visibility":"local","item":item}));
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(text(&dom).contains("Running git"), "{}", text(&dom));
    assert_eq!(props.probe.summary.get(), summaries + 1, "the line named cargo and now names git");
    assert_eq!((props.probe.application.get(), props.probe.composer.get()), (application, composer));
}

/// The subagent items of the open conversation, as the titles their rows are named by.
fn subagent_labels(state: Store<UiModel>) -> Vec<String> {
    projection(state)["visibleTurnItems"].as_array().unwrap().iter().filter(|row| row["item"]["type"] == "subagent").map(|row| format!("Open {}", t3_client::subagent_display::display_title(row["item"]["title"].as_str().unwrap_or("Subagent")))).collect()
}

/// A real recording of Codex's subagents (the original's), served by the fake Codex: the parent's
/// row opens the child thread, which is read-only and holds the grandchild's row in turn, and the
/// way back leads to the parent.
#[tokio::test(flavor = "current_thread")]
async fn recorded_codex_subagents_open_their_child_threads_down_a_chain_and_back() {
    let file: Value = serde_json::from_str(include_str!("../../server/tests/fixtures/codex-replay.json")).unwrap();
    let scenario = file["scenarios"].as_array().unwrap().iter().find(|row| row["name"] == "subagent_v2_nested").expect("the recording").clone();
    let scratch = tempfile::tempdir().unwrap();
    let transcript = scratch.path().join("transcript.ndjson");
    std::fs::write(&transcript, scenario["transcript"].as_array().unwrap().iter().map(|entry| entry.to_string() + "\n").collect::<String>()).unwrap();
    let transcript = transcript.to_string_lossy().into_owned();
    let fixture = Fixture::start_with(false, &[("CODEX_FIXTURE_TRANSCRIPT", transcript.as_str()), ("T3CODE_CODEX_STREAM_FLUSH_MS", "600000")]).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    // The recording was made with this model; the fake serves it only to the same requests.
    dom.in_scope(ScopeId::APP, || runtime::command(&props.transport, state, "thread.create", json!({"threadId":"recorded","projectId":"project","title":"recorded","modelSelection":{"instanceId":"codex","model":scenario["model"]},"runtimeMode":"full-access","interactionMode":scenario["interactionMode"],"branch":null,"worktreePath":null}))).unwrap();
    until(&mut dom, "the thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.iter().any(|t| t.id.as_str() == "recorded"))).await;
    open_thread(&mut dom, &props, state, "recorded").await;
    send(&mut dom, &props, state, scenario["steps"][0]["text"].as_str().unwrap());
    // The turn settles and its work folds; the child it launched is a row of its own below.
    until(&mut dom, "the settled turn", |dom| control(dom, "Worked for*").is_some()).await;
    click_control(&mut dom, "Worked for*");
    let first = subagent_labels(state).into_iter().next().expect("the parent holds its subagent");
    assert_eq!(first, "Open Chain Level 1", "the recording names its task by path");
    until(&mut dom, "the subagent row", |dom| control(dom, &first).is_some()).await;
    assert!(rendered_text(&dom).contains("Chain Level 1") && rendered_text(&dom).contains("Hello."), "{}", rendered_text(&dom));

    click_control(&mut dom, &first);
    until(&mut dom, "the child thread", |dom| rendered_text(dom).contains("This subagent is run by its provider and cannot take messages.") && control(dom, "Open parent thread").is_some()).await;
    assert!(control(&dom, "Message").is_none(), "a child takes no messages");
    // The child holds its own child, as a row that opens it.
    until(&mut dom, "the child's conversation", |dom| control(dom, "Worked for*").is_some() || control(dom, "Open Chain Level 2*").is_some()).await;
    if control(&dom, "Open Chain Level 2*").is_none() {
        click_control(&mut dom, "Worked for*");
    }
    let nested = subagent_labels(state).into_iter().next().expect("the child holds a subagent");
    until(&mut dom, "the grandchild's row", |dom| control(dom, &nested).is_some()).await;
    click_control(&mut dom, &nested);
    until(&mut dom, "the grandchild thread", |dom| rendered_text(dom).contains("Hello.") && control(dom, "Open parent thread").is_some()).await;
    assert!(control(&dom, "Message").is_none());

    // Back up the chain, one parent at a time, to the thread that can take messages.
    click_control(&mut dom, "Open parent thread");
    until(&mut dom, "the child again", |dom| control(dom, &nested).is_some() || control(dom, "Worked for*").is_some()).await;
    click_control(&mut dom, "Open parent thread");
    until(&mut dom, "the top-level thread", |dom| control(dom, "Message").is_some()).await;
    assert_eq!(state.peek().active_thread.as_deref(), Some("recorded"));
    fixture.finish(dom).await;
}

/// What the connected server says threads can do; the fixture's server says pinning and snoozing.
fn set_thread_actions(state: Store<UiModel>, offered: bool) {
    let mut config = state.typed_config();
    let mut config = config.write();
    let capabilities = &mut config.as_mut().expect("the server's config").environment.capabilities;
    capabilities.thread_pinning = Some(offered);
    capabilities.thread_snooze = Some(offered);
}
fn thread_shell(state: Store<UiModel>, id: &str) -> t3_contracts::ThreadShell {
    state.peek().shell.snapshot.as_ref().unwrap().threads.iter().find(|thread| thread.id.as_str() == id).cloned().unwrap()
}

/// A thread's context menu, by the keyboard: pin and unpin, snooze and wake, mark unread. Each is
/// one request, offered only where the server says it can, and the row shows what the server answered.
#[tokio::test(flavor = "current_thread")]
async fn a_thread_row_pins_snoozes_and_marks_unread_from_its_menu() {
    use crate::runtime::keys_tests::key_on;
    use dioxus::prelude::Key;
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    // Without the capabilities, the menu leaves the actions out.
    set_thread_actions(state, false);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    key_on(&mut dom, "Thread thread-b", Key::ContextMenu);
    assert!(control(&dom, "Pin thread").is_none() && control(&dom, "Mark unread").is_some() && control(&dom, "Copy thread ID").is_some());
    key_on(&mut dom, "Thread actions", Key::Escape);
    assert!(control(&dom, "Thread actions").is_none(), "Escape closes the menu");
    set_thread_actions(state, true);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);

    key_on(&mut dom, "Thread thread-b", Key::ContextMenu);
    click_control(&mut dom, "Pin thread");
    until(&mut dom, "the pin", |_| t3_client::sidebar::is_pinned(&thread_shell(state, "thread-b"))).await;
    assert!(control(&dom, "Thread actions").is_none(), "the menu closes when an action is chosen");
    // The row is busy until the server has answered, and its menu offers nothing meanwhile.
    until(&mut dom, "the answer", |dom| control_attribute(dom, "Thread thread-b", "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;
    until(&mut dom, "the pin on the row", |dom| control(dom, "Pinned").is_some() || rendered_text(dom).contains('\u{25c6}')).await;
    let order = crate::keys::sidebar_order(state);
    assert_eq!(order.first().map(String::as_str), Some("thread-b"), "pinned threads list first: {order:?}");
    key_on(&mut dom, "Thread thread-b", Key::ContextMenu);
    assert!(control(&dom, "Pin thread").is_none());
    click_control(&mut dom, "Unpin thread");
    until(&mut dom, "the unpin", |_| !t3_client::sidebar::is_pinned(&thread_shell(state, "thread-b"))).await;
    until(&mut dom, "the answer", |dom| control_attribute(dom, "Thread thread-b", "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;

    // Snoozing hides the thread's urgency behind a wake time; waking brings it back.
    key_on(&mut dom, "Thread thread-a", Key::ContextMenu);
    click_control(&mut dom, "Snooze: In 1 hour*");
    until(&mut dom, "the snooze", |_| thread_shell(state, "thread-a").snoozed_until.flatten().is_some()).await;
    until(&mut dom, "the answer", |dom| control_attribute(dom, "Thread thread-a", "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;
    until(&mut dom, "the wake time on the row", |dom| rendered_text(dom).contains("Snoozed \u{b7} ")).await;
    key_on(&mut dom, "Thread thread-a", Key::ContextMenu);
    click_control(&mut dom, "Wake thread");
    until(&mut dom, "the wake", |_| thread_shell(state, "thread-a").snoozed_until.flatten().is_none()).await;
    until(&mut dom, "the answer", |dom| control_attribute(dom, "Thread thread-a", "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;
    until(&mut dom, "the row awake", |dom| !rendered_text(dom).contains("Snoozed")).await;

    // A finished turn reads as read until it is marked unread; opening the thread reads it again.
    // The first message names the thread, so the row is found by its new title.
    open_thread(&mut dom, &props, state, "thread-b").await;
    super::runtime::diff_tests::turn(&mut dom, &props, state, 1).await;
    until(&mut dom, "the thread's new title", |_| thread_shell(state, "thread-b").title != "thread-b").await;
    let row = format!("Thread {}", thread_shell(state, "thread-b").title);
    open_thread(&mut dom, &props, state, "thread-a").await;
    key_on(&mut dom, &row, Key::ContextMenu);
    click_control(&mut dom, "Mark unread");
    until(&mut dom, "the unread state on the row", |dom| rendered_text(dom).contains("Completed") && control_attribute(dom, &row, "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;
    open_thread(&mut dom, &props, state, "thread-b").await;
    until(&mut dom, "the visit", |dom| !rendered_text(dom).contains("Completed") && !t3_client::sidebar::has_unseen_completion(&thread_shell(state, "thread-b")) && control(dom, &row).is_some()).await;
    fixture.finish(dom).await;
}

/// One thread's status changing repaints its own row, not the 40 beside it nor the conversation.
#[tokio::test(flavor = "current_thread")]
async fn a_status_change_on_one_thread_repaints_one_sidebar_row() {
    let (mut dom, props) = synth(json!([]), SETTLED(), json!({}));
    let state = state_of(&props);
    {
        let mut shell = state.shell();
        let mut shell = shell.write();
        let snapshot = shell.snapshot.as_mut().unwrap();
        let template = snapshot.threads[0].clone();
        for index in 0..40 {
            let mut thread = template.clone();
            thread.id = format!("other-{index}").parse().unwrap();
            thread.title = format!("Other {index}");
            snapshot.threads.push(thread);
        }
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(text(&dom).contains("Other 39"));
    let (rows, items, composer) = (props.probe.sidebar_row.get(), props.probe.item.get(), props.probe.composer.get());
    {
        let mut shell = state.shell();
        let mut shell = shell.write();
        let thread = shell.snapshot.as_mut().unwrap().threads.iter_mut().find(|thread| thread.id.as_str() == "other-7").unwrap();
        thread.status = t3_contracts::ThreadStatus::Running;
        thread.activity_run_status = Some(Some(t3_contracts::ActivityRunStatus::Running));
        thread.latest_run_id = Some("run-x".parse().unwrap());
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert_eq!(props.probe.sidebar_row.get(), rows + 1, "only the thread that changed was built again");
    assert_eq!(props.probe.item.get(), items, "the conversation was not touched");
    assert_eq!(props.probe.composer.get(), composer);
    assert!(text(&dom).contains("Working"), "{}", text(&dom));
    assert_eq!(control_attribute(&dom, "Thread Other 7", "data-thread-status").and_then(|(_, value)| value).as_deref(), Some("working"));
}

/// "Save to workspace" on a plan writes its markdown where the reader says, once per click, and
/// is offered only to a connection that may write files.
#[tokio::test(flavor = "current_thread")]
async fn a_plan_is_saved_into_the_workspace_from_its_menu() {
    use crate::runtime::keys_tests::key_on;
    use dioxus::prelude::Key;
    use t3_contracts::AuthEnvironmentScope as Scope;
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), &[Scope::OrchestrationRead, Scope::OrchestrationOperate, Scope::FilesystemWrite]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    create_thread(&mut dom, &props, state, "planner", "full-access", "plan").await;
    open_thread(&mut dom, &props, state, "planner").await;
    send(&mut dom, &props, state, "Make a plan [plan]");
    until(&mut dom, "the plan card", |dom| rendered_text(dom).contains("Proposed plan") && control(dom, "Plan actions").is_some()).await;
    click_control(&mut dom, "Plan actions");
    click_control(&mut dom, "Save to workspace");
    assert!(control(&dom, "Save plan to workspace").is_some());
    // Escape leaves without writing.
    key_on(&mut dom, "Save plan to workspace", Key::Escape);
    assert!(control(&dom, "Save plan to workspace").is_none());
    click_control(&mut dom, "Plan actions");
    click_control(&mut dom, "Save to workspace");
    input_control(&mut dom, "Workspace path", "docs/plan.md");
    click_control(&mut dom, "Save plan");
    assert_eq!(control_attribute(&dom, "Save plan", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"), "one request per click");
    until(&mut dom, "the file", |_| fixture.workspace.join("docs/plan.md").is_file()).await;
    until(&mut dom, "the dialog to close", |dom| control(dom, "Save plan to workspace").is_none()).await;
    let saved = std::fs::read_to_string(fixture.workspace.join("docs/plan.md")).unwrap();
    assert!(saved.contains("Do the work."), "{saved}");
    fixture.finish(dom).await;

    // A connection that cannot write files is not offered the action.
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    create_thread(&mut dom, &props, state, "planner", "full-access", "plan").await;
    open_thread(&mut dom, &props, state, "planner").await;
    send(&mut dom, &props, state, "Make a plan [plan]");
    until(&mut dom, "the plan card", |dom| control(dom, "Plan actions").is_some()).await;
    click_control(&mut dom, "Plan actions");
    assert_eq!(control_attribute(&dom, "Save to workspace", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    fixture.finish(dom).await;
}
