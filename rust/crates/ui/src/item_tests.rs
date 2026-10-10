//! What the conversation and the composer show for each kind of turn item the providers now
//! emit. Two kinds of test: the real server with the scripted fake Codex (`tests/fixtures/
//! codex-provider.py`, whose tags are listed in `codex_runtime_flow.rs`), and a mounted app over
//! a hand-made projection for the shapes the fake cannot produce yet (ACP, cancelled runs,
//! background work). Waits are on text that only the awaited state can produce.
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::diff_tests::{Fixture, Harness, open_thread, rendered_text, seed, until};
use crate::runtime::keys_tests::key_on as key_on_label;
use crate::runtime::transport_tests::{click_control, control, control_attribute, input_control};
use std::{cell::RefCell, rc::Rc};
use t3_server::secret_requests::SecretRequests;

fn state_of(props: &Synth) -> Store<UiModel> {
    props.state.borrow().unwrap()
}
fn projection(state: Store<UiModel>) -> Value {
    state.peek().thread.projection.clone().unwrap()
}
async fn create_thread(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, id: &str, runtime: &str, interaction: &str) {
    dom.in_scope(ScopeId::APP, || runtime::command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":id,"projectId":"project","title":id,"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":runtime,"interactionMode":interaction,"branch":null,"worktreePath":null}))).unwrap();
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

/// "Implement in a new thread" makes a thread in the same project, titled for the plan, whose first
/// turn implements it with the plan as its source; the plan's own thread is left as it was.
#[tokio::test(flavor = "current_thread")]
async fn a_plan_can_be_implemented_in_a_new_thread_with_the_plan_as_its_source() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    create_thread(&mut dom, &props, state, "planner", "full-access", "plan").await;
    open_thread(&mut dom, &props, state, "planner").await;
    send(&mut dom, &props, state, "Make a plan [plan]");
    until(&mut dom, "the plan card and its follow-up", |dom| rendered_text(dom).contains("Plan ready") && control(dom, "Implement").is_some()).await;
    let plan = projection(state)["plans"].as_array().unwrap().iter().find(|plan| plan["kind"] == "proposed_plan").cloned().unwrap();
    // With feedback written the button refines, and the menu is not offered.
    input_control(&mut dom, "Message", "Also add tests");
    assert!(control(&dom, "Implementation actions").is_none());
    input_control(&mut dom, "Message", "");
    click_control(&mut dom, "Implementation actions");
    key_on_menu_escape(&mut dom);
    assert!(control(&dom, "Implement in a new thread").is_none(), "Escape closes the menu");
    click_control(&mut dom, "Implementation actions");
    click_control(&mut dom, "Implement in a new thread");
    // Once clicked, the menu cannot start a second one while the first is being made.
    assert_eq!(control_attribute(&dom, "Implementation actions", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    until(&mut dom, "the new thread to open with its turn", |dom| {
        state.peek().active_thread.as_deref().is_some_and(|id| id != "planner") && rendered_text(dom).contains("PLEASE IMPLEMENT THIS PLAN:")
    }).await;
    let opened = state.peek().active_thread.clone().unwrap();
    let shell = state.peek().shell.snapshot.as_ref().unwrap().threads.iter().find(|thread| thread.id.as_str() == opened).cloned().unwrap();
    assert!(shell.title.starts_with("Implement"), "{}", shell.title);
    assert_eq!(shell.project_id.as_str(), "project");
    assert_eq!(serde_json::to_value(shell.interaction_mode).unwrap(), "default");
    let runs = projection(state)["runs"].as_array().unwrap().clone();
    assert_eq!(runs.last().unwrap()["sourcePlanRef"], json!({"threadId":"planner","planId":plan["id"]}), "{runs:?}");
    assert!(state.peek().plan_launch.is_none());
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

/// Presses a key in the box with this label as a browser reports it: the `key` text on `keydown`.
/// Returns whether the page would still run the browser's own action (a line break in a textarea).
pub(crate) fn keydown(dom: &mut VirtualDom, label: &str, key: Key, modifiers: Modifiers, composing: bool) -> bool {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    let code = if key == Key::Enter { Code::Enter } else { Code::Unidentified };
    let data = dioxus_html::SerializedKeyboardData::new(key, code, Location::Standard, false, modifiers, composing);
    let event = dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(data))) as Rc<dyn std::any::Any>, true);
    dom.runtime().handle_event("keydown", event.clone(), element);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    event.default_action_enabled()
}

/// Enter in the message box sends what the Send button sends, in every state the box has: idle,
/// a run in progress (queued, as the button would), nothing typed. Shift-Enter and an IME's Enter
/// leave the text alone, and a send cancels the browser's line break so none is added as well.
#[tokio::test(flavor = "current_thread")]
async fn enter_in_the_message_box_sends_like_the_send_button() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;

    // Nothing typed: Enter sends nothing, and adds no line either.
    assert!(!keydown(&mut dom, "Message", Key::Enter, Modifiers::empty(), false));
    assert!(projection(state)["turnItems"].as_array().is_none_or(|items| items.is_empty()));

    // A line break, an IME confirmation and a modified key are the browser's to handle.
    input_control(&mut dom, "Message", "hello world");
    assert!(keydown(&mut dom, "Message", Key::Enter, Modifiers::SHIFT, false), "Shift-Enter keeps its line break");
    assert!(keydown(&mut dom, "Message", Key::Enter, Modifiers::empty(), true), "Enter during composition keeps its meaning");
    assert!(keydown(&mut dom, "Message", Key::Character("a".into()), Modifiers::empty(), false));
    assert_eq!(state.peek().draft, "hello world", "none of them sent");

    // Plain Enter sends and cancels the line break.
    assert!(!keydown(&mut dom, "Message", Key::Enter, Modifiers::empty(), false));
    until(&mut dom, "the reply", |dom| rendered_text(dom).contains("Fixture response: hello world")).await;
    assert_eq!(state.peek().draft, "");

    // While a run is in progress the message queues, as the Send button would.
    send(&mut dom, &props, state, "First [hold]");
    until(&mut dom, "the turn to run", |dom| control(dom, "Stop generation").is_some()).await;
    input_control(&mut dom, "Message", "queued by enter");
    assert!(!keydown(&mut dom, "Message", Key::Enter, Modifiers::empty(), false));
    until(&mut dom, "the queue", |_| t3_client::timeline_items::queued_runs(&projection(state)).iter().any(|run| run.text == "queued by enter")).await;
    assert_eq!(state.peek().draft, "");
    click_control(&mut dom, "Stop generation");
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
        let rows: Vec<_> = props.items.as_array().unwrap().iter().enumerate().map(|(index, item)| json!({"sourceThreadId":item["sourceThreadId"].as_str().unwrap_or("thread"),"sourceItemId":item["id"],"position":index,"visibility":item["visibility"].as_str().unwrap_or("local"),"item":item})).collect();
        model.thread.projection = Some(json!({"thread":{"id":"thread","activeProviderThreadId":null},"visibleTurnItems":rows,"turnItems":props.items,"runs":props.runs,"runtimeRequests":props.extra["requests"].clone(),"plans":props.extra["plans"].clone(),"subagents":props.extra["subagents"].clone(),"messages":props.extra["messages"].clone(),"providerTurns":[],"providerThreads":[],"updatedAt":"2026-01-01T00:00:00Z"}));
        model
    });
    *props.state.borrow_mut() = Some(state);
    let transport = use_hook(runtime::TransportHandle::default);
    rsx! { Application { state, transport } }
}
fn synth(items: Value, runs: Value, extra: Value) -> (VirtualDom, Synth) {
    let props = Synth { state: Rc::new(RefCell::new(None)), probe: RenderProbe::default(), items, runs, extra: json!({"tasks":[],"requests":[],"plans":[],"subagents":[],"messages":[],"lineage":null}).as_object().unwrap().iter().map(|(key, value)| (key.clone(), extra.get(key).cloned().unwrap_or(value.clone()))).collect() };
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
    dom.in_scope(ScopeId::APP, || runtime::command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":"recorded","projectId":"project","title":"recorded","modelSelection":{"instanceId":"codex","model":scenario["model"]},"runtimeMode":"full-access","interactionMode":scenario["interactionMode"],"branch":null,"worktreePath":null}))).unwrap();
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
    // A snoozed thread folds into the Snoozed shelf, which is closed until its header is pressed.
    until(&mut dom, "the shelf", |dom| control(dom, "Snoozed (1)").is_some()).await;
    assert!(control(&dom, "Thread thread-a").is_none());
    click_control(&mut dom, "Snoozed (1)");
    until(&mut dom, "the answer", |dom| control_attribute(dom, "Thread thread-a", "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;
    until(&mut dom, "the wake time on the row", |dom| rendered_text(dom).contains("Snoozed \u{b7} ")).await;
    key_on(&mut dom, "Thread thread-a", Key::ContextMenu);
    click_control(&mut dom, "Wake thread");
    until(&mut dom, "the wake", |_| thread_shell(state, "thread-a").snoozed_until.flatten().is_none()).await;
    until(&mut dom, "the answer", |dom| control_attribute(dom, "Thread thread-a", "aria-busy").and_then(|(_, value)| value).as_deref() == Some("false")).await;
    until(&mut dom, "the row awake", |dom| !rendered_text(dom).contains("Snoozed")).await;

    // A finished turn reads as read until it is marked unread; opening the thread reads it again.
    // A message to an existing thread leaves its title alone, so the row keeps its name.
    open_thread(&mut dom, &props, state, "thread-b").await;
    super::runtime::diff_tests::turn(&mut dom, &props, state, 1).await;
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

/// The lineage panel offers to stop a subagent this app runs while it works, named for it, and not one the
/// provider runs itself nor one that has finished. (The write itself is `run.interrupt`, as the stop button's.)
#[tokio::test(flavor = "current_thread")]
async fn the_lineage_panel_offers_to_stop_only_a_running_app_owned_subagent() {
    let subagent = |id: &str, origin: &str, status: &str| json!({"id":format!("agent-{id}"),"origin":origin,"status":status,"startedAt":"2026-01-01T00:00:00Z","childThreadId":id,"title":format!("Helper {id}"),"prompt":"p"});
    let (mut dom, props) = synth(json!([]), SETTLED(), json!({"subagents":[subagent("own", "app_owned", "running"), subagent("native", "provider_native", "running"), subagent("done", "app_owned", "completed")]}));
    let state = state_of(&props);
    {
        let mut shell = state.shell();
        let mut shell = shell.write();
        let snapshot = shell.snapshot.as_mut().unwrap();
        let template = snapshot.threads[0].clone();
        for id in ["own", "native", "done"] {
            let mut child = template.clone();
            child.id = id.parse().unwrap();
            child.title = format!("Helper {id}");
            child.lineage = serde_json::from_value(json!({"parentThreadId":"thread","relationshipToParent":"subagent","rootThreadId":"thread"})).unwrap();
            child.active_run_id = Some(format!("run-{id}").parse().unwrap());
            child.latest_run_id = child.active_run_id.clone();
            if id != "done" {
                child.activity_run_status = Some(Some(t3_contracts::ActivityRunStatus::Running));
                child.activity_run_started_at = Some(Some(serde_json::from_value(json!("2026-01-01T00:00:00Z")).unwrap()));
            }
            snapshot.threads.push(child);
        }
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Thread lineage");
    assert!(text(&dom).contains("Helper own") && text(&dom).contains("Helper native"), "{}", text(&dom));
    assert!(control(&dom, "Stop subagent Helper own").is_some(), "{}", text(&dom));
    assert!(control(&dom, "Stop subagent Helper native").is_none(), "the provider's own subagent is its to stop");
    assert!(control(&dom, "Stop subagent Helper done").is_none());
    // Without a connection that may operate, the button is there and cannot be pressed.
    assert_eq!(control_attribute(&dom, "Stop subagent Helper own", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
}

/// A delegated task's wake reaches the parent as a message an agent sent: the recorded session's parent
/// (`delegated_task_status`, see `codex_delegated_replay.rs`) is woken with exactly this text, written by
/// the server for an agent, and the timeline says who sent it instead of showing it as the person's own.
#[tokio::test(flavor = "current_thread")]
async fn a_delegated_tasks_wake_is_attributed_to_another_agent() {
    let wake_text = "Delegated task task-1 reached a terminal state. Use task_status with taskId task-1 to read the result.";
    let (dom, _) = synth(json!([
        with(base("typed", "user_message", "completed"), json!({"messageId":"typed","text":"Please delegate it","inputIntent":"turn_start","createdBy":"user","creationSource":"web"})),
        with(base("wake", "user_message", "completed"), json!({"messageId":"wake","text":wake_text,"inputIntent":"turn_start","createdBy":"agent","creationSource":"server"})),
        with(base("sent", "user_message", "completed"), json!({"messageId":"sent","text":"Here is what I found","inputIntent":"turn_start","createdBy":"agent","creationSource":"mcp","senderThreadId":"helper"})),
        with(base("scheduled", "user_message", "completed"), json!({"messageId":"scheduled","text":"Nightly checks","inputIntent":"turn_start","createdBy":"user","creationSource":"scheduled","scheduledTaskId":"task-9"})),
    ]), SETTLED(), json!({}));
    let shown = text(&dom);
    assert!(shown.contains(wake_text) && shown.contains("Please delegate it"), "{shown}");
    // The person's own message carries no line; the wake and the other agent's message say so, and the
    // second links to the thread that sent it.
    assert_eq!(shown.matches("Sent by another agent").count(), 2, "{shown}");
    assert!(control(&dom, "Open sending thread").is_some());
    assert_eq!(shown.matches("Sent by automation").count(), 1, "{shown}");
}

/// A drop held in its new place (and given up again) rebuilds the row that moved and nothing else:
/// not the rows beside it, the application, the conversation or the composer. The pointer's drag
/// is the page's, so nothing here is built while it is in progress.
#[tokio::test(flavor = "current_thread")]
async fn a_held_drop_repaints_only_the_row_that_moved() {
    let (mut dom, props) = synth(json!([]), SETTLED(), json!({}));
    let state = state_of(&props);
    {
        let mut shell = state.shell();
        let mut shell = shell.write();
        let snapshot = shell.snapshot.as_mut().unwrap();
        let template = snapshot.threads[0].clone();
        for index in 0..30 {
            let mut thread = template.clone();
            thread.id = format!("other-{index}").parse().unwrap();
            thread.title = format!("Other {index}");
            if index == 3 || index == 4 {
                thread.pinned_at = Some(Some(serde_json::from_value(json!("2026-01-01T00:00:00Z")).unwrap()));
                thread.pin_order_key = Some(Some(if index == 3 { "g" } else { "m" }.parse().unwrap()));
            }
            snapshot.threads.push(thread);
        }
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let counts = |props: &Synth| (props.probe.sidebar_row.get(), props.probe.application.get(), props.probe.item.get(), props.probe.composer.get());
    let (rows, application, items, composer) = counts(&props);
    state.reorder().write().held = Some(crate::sidebar_sections::Held { project: "p".into(), id: "other-9".into(), section: t3_client::sidebar::Section::Pinned, order: vec!["other-3".into(), "other-9".into(), "other-4".into()], answered_at: None });
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let (moved_rows, moved_application, moved_items, moved_composer) = counts(&props);
    assert_eq!(moved_rows, rows + 1, "only the row dropped was built again");
    assert_eq!((moved_application, moved_items, moved_composer), (application, items, composer));
    // Given up (the server refused): the same single row, back where the server has it.
    *state.reorder().write() = Default::default();
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let (back_rows, back_application, back_items, back_composer) = counts(&props);
    assert_eq!(back_rows, moved_rows + 1);
    assert_eq!((back_application, back_items, back_composer), (application, items, composer));
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

#[tokio::test(flavor = "current_thread")]
async fn a_report_of_finished_background_work_reads_as_the_work_and_a_subagent_report_as_its_row() {
    let report = |id: &str, source: Value, outcome: &str, summary: &str| with(base(id, "notification", "completed"), json!({"source":source,"outcome":outcome,"summary":summary,"startedAt":"2026-01-01T15:07:00Z"}));
    let items = json!([
        report("n1", json!({"kind":"background_command"}), "completed", "Command \"sleep 600\" finished"),
        report("n2", json!({"kind":"background_task","work":"subagent","childThreadId":"child-a"}), "failed", "Subagent \"Review\" failed"),
        report("n3", json!({"kind":"background_task","work":"subagent","childThreadId":"gone"}), "completed", "Subagent \"Lost\" finished"),
        with(base("t1", "dynamic_tool", "cancelled"), json!({"toolName":"Bash","input":{},"toolNonExecutionKind":"denied"})),
    ]);
    let agent = json!({"id":"agent-a","title":"Review src/math.ts","driver":"claudeAgent","providerInstanceId":"claude","childThreadId":"child-a","status":"completed","result":"Looks fine.","startedAt":"2026-01-01T14:00:00Z","completedAt":"2026-01-01T14:05:00Z"});
    let (mut dom, state) = synth(items, SETTLED(), json!({"subagents":[agent]}));
    let _ = &mut dom;
    let shown = text(&dom);
    let arrived = t3_client::timeline_items::short_timestamp("2026-01-01T15:07:00Z", scheduled_tasks_page::zone(chrono::Utc::now().timestamp_millis()));
    assert!(shown.contains("Command \"sleep 600\" finished"), "{shown}");
    // The subagent the parent knows is its own row, worded by the report (a failure), stamped when it arrived.
    assert!(shown.contains("Review src/math.ts") && shown.contains("Failed") && shown.contains(&arrived), "{shown}");
    assert!(control(&dom, "Open Review src/math.ts").is_some());
    // One it has no record of is a plain row that still opens the thread.
    assert!(shown.contains("Subagent \"Lost\" finished") && control(&dom, "Open subagent thread").is_some(), "{shown}");
    // A tool the provider declined to run says how.
    assert!(shown.contains("denied"), "{shown}");
    let _ = state;
}

#[tokio::test(flavor = "current_thread")]
async fn a_settled_answer_has_a_footer_with_copy_and_time_and_a_running_turn_has_none() {
    let items = json!([user("u", "Hi"), assistant("a", "All done."), command("c", "ls", "completed")]);
    let done = json!([{"id":"run-1","status":"completed","ordinal":1,"startedAt":"2026-01-01T00:00:00Z","completedAt":"2026-01-01T00:00:30Z"}]);
    let (dom, _) = synth(items, done, json!({}));
    let shown = text(&dom);
    let arrived = t3_client::timeline_items::day_aware_timestamp("2026-01-01T00:00:00Z", scheduled_tasks_page::zone(chrono::Utc::now().timestamp_millis()), chrono::Utc::now());
    assert!(control(&dom, "Copy message").is_some(), "{shown}");
    assert!(shown.contains(&arrived), "{shown} should name {arrived}");
    assert!(shown.find("All done.") < shown.find(&arrived), "the footer is under the answer: {shown}");

    // While the turn runs there is no footer, and a streaming answer cannot be copied.
    let running = json!([{"id":"run-1","status":"running","ordinal":1,"startedAt":"2026-01-01T00:00:00Z"}]);
    let (dom, _) = synth(json!([user("u", "Hi"), assistant("a", "Partial")]), running, json!({}));
    assert!(control(&dom, "Copy message").is_none(), "{}", text(&dom));
}

/// Submits the form with this accessible label, as the browser would, with these fields.
fn submit_form(dom: &mut VirtualDom, label: &str, fields: &[(&str, &str)]) {
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing form {label}"));
    let values = fields.iter().map(|(name, value)| dioxus_html::SerializedFormObject { key: (*name).to_owned(), text: Some((*value).to_owned()), file: None }).collect();
    let data = dioxus_html::SerializedFormData::new(String::new(), values);
    dom.runtime().handle_event("submit", dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(data))) as Rc<dyn std::any::Any>, true), element);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}

#[tokio::test(flavor = "current_thread")]
async fn a_secret_request_is_answered_from_its_card_and_the_value_is_never_shown_or_kept() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    // The request is what an agent's `request_secret` leaves in the thread: a live run and a pending card.
    let mut thread = fixture.store.projection("thread", "thread-a").unwrap().unwrap();
    thread["runs"] = json!([{"id":"run-live","threadId":"thread-a","ordinal":1,"providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"fixture-model"},"providerThreadId":null,"userMessageId":"m","rootNodeId":"node","activeAttemptId":null,"status":"running","requestedAt":"2026-01-01T00:00:00.000Z","startedAt":"2026-01-01T00:00:00.000Z","completedAt":null,"checkpointId":null,"contextHandoffId":null}]);
    let card = json!({"id":"ask","threadId":"thread-a","runId":"run-live","nodeId":"node","providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":1,"status":"waiting","title":"npm token","startedAt":"2026-01-01T00:00:00.000Z","completedAt":null,"updatedAt":"2026-01-01T00:00:00.000Z","type":"secret_request","label":"npm token","reason":"Needed to publish","secretStatus":"pending"});
    thread["turnItems"] = json!([card]);
    thread["visibleTurnItems"] = json!([{"sourceThreadId":"thread-a","sourceItemId":"ask","position":0,"visibility":"local","item":card}]);
    fixture.store.transaction(|transaction| t3_server::persistence::write_projection(transaction, "thread", "thread-a", &thread)).unwrap();
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "the card", |dom| control(dom, "Secret request").is_some()).await;
    let shown = rendered_text(&dom);
    assert!(shown.contains("npm token") && shown.contains("Needed to publish") && shown.contains("Stored securely, never shown to the agent"), "{shown}");
    // Nothing to save until there is text.
    assert_eq!(control_attribute(&dom, "Save securely", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    input_control(&mut dom, "npm token", "  hunter2  ");
    assert_ne!(control_attribute(&dom, "Save securely", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));

    submit_form(&mut dom, "Secret request", &[("secret", "  hunter2  ")]);
    // From the click until the server answers, the card answers nothing again.
    assert_eq!(control_attribute(&dom, "Save securely", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    until(&mut dom, "the answered line", |dom| rendered_text(dom).contains("npm token \u{b7} Saved securely and kept private")).await;
    assert!(control(&dom, "Secret request").is_none(), "the form is gone once answered");
    // The value is in no row, no state the page holds, and the server's projection has only the status.
    assert!(!rendered_text(&dom).contains("hunter2"));
    assert!(!projection(state).to_string().contains("hunter2"));
    let kept = SecretRequests::for_store(&fixture.store).unwrap().saved_ref("thread-a", "ask");
    assert!(kept.is_some_and(|reference| reference.starts_with("secret-ref:")));
    fixture.finish(dom).await;
}

/// How many rows a thread of thousands of items builds: when it mounts, when its biggest fold is
/// opened, and when that fold's biggest group is. (Scrolling builds none: every row a fold or a
/// group shows already exists.) The numbers are what the decision about windowing rests on.
#[tokio::test(flavor = "current_thread")]
async fn a_thread_of_thousands_of_items_builds_rows_by_what_is_open() {
    // 120 settled turns of a message, 22 tool calls and an answer: 2,880 items.
    let mut items = vec![];
    let mut runs = vec![];
    for turn in 0..120 {
        let run = format!("run-{turn}");
        let tag = |mut item: Value| {
            item["runId"] = json!(run);
            item
        };
        items.push(tag(user(&format!("u{turn}"), "Do the thing")));
        for call in 0..22 {
            items.push(tag(command(&format!("c{turn}-{call}"), &format!("step {call}"), "completed")));
        }
        items.push(tag(assistant(&format!("a{turn}"), "Done.")));
        runs.push(json!({"id":run,"status":"completed","ordinal":turn + 1,"startedAt":"2026-01-01T00:00:00Z","completedAt":"2026-01-01T00:01:00Z"}));
    }
    let started = std::time::Instant::now();
    let (mut dom, props) = synth(json!(items), json!(runs), json!({}));
    let mounted = (props.probe.item.get(), started.elapsed());
    eprintln!("MEASURE mount of 120 turns / {} items: {} rows built in {:?}", items.len(), mounted.0, mounted.1);
    assert!(mounted.0 < 400, "settled turns fold, so only their messages are built: {}", mounted.0);

    let before = props.probe.item.get();
    let started = std::time::Instant::now();
    click_control(&mut dom, "Worked for*");
    eprintln!("MEASURE opening one fold: {} rows built in {:?}", props.probe.item.get() - before, started.elapsed());

    // One long turn: 3,000 calls in a single group, the worst case for a list inside an open group.
    let mut items = vec![user("u", "Run the suite")];
    items.extend((0..3000).map(|call| command(&format!("c{call}"), &format!("step {call}"), "completed")));
    items.push(assistant("a", "Done."));
    let (mut dom, props) = synth(json!(items), SETTLED_WITH_END(), json!({}));
    let mounted = props.probe.item.get();
    let started = std::time::Instant::now();
    click_control(&mut dom, "Worked for*");
    let fold = (props.probe.item.get() - mounted, started.elapsed());
    eprintln!("MEASURE opening the fold of a 3,000 call turn: {} rows built in {:?}", fold.0, fold.1);
    let before = props.probe.item.get();
    let started = std::time::Instant::now();
    click_control(&mut dom, "Ran 3000 commands");
    let opened = props.probe.item.get() - before;
    eprintln!("MEASURE opening its 3,000 call group: {opened} rows built in {:?}", started.elapsed());
    // Only the first page of the group is built; the rest is asked for, and appended below.
    assert_eq!(opened, work_rows::MEMBER_PAGE);
    assert!(text(&dom).contains("step 0") && !text(&dom).contains("step 200") && text(&dom).contains("Show 200 more (2800 hidden)"), "{}", text(&dom).len());
    click_control(&mut dom, "Show more calls");
    assert_eq!(props.probe.item.get() - before, 2 * work_rows::MEMBER_PAGE, "asking builds the next page, not the rest");
    assert!(text(&dom).contains("step 399") && !text(&dom).contains("step 400") && text(&dom).contains("(2600 hidden)"));
}
const SETTLED_WITH_END: fn() -> Value = || json!([{"id":"run-1","status":"completed","ordinal":1,"startedAt":"2026-01-01T00:00:00Z","completedAt":"2026-01-01T00:01:00Z"}]);

#[tokio::test(flavor = "current_thread")]
async fn a_thread_is_renamed_in_place_and_a_new_thread_starts_on_its_branch() {
    use crate::runtime::keys_tests::key_on;
    use dioxus::prelude::Key;
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    dom.in_scope(ScopeId::APP, || runtime::command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":"on-branch","projectId":"project","title":"Feature work","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":"feature-x","worktreePath":null}))).unwrap();
    until(&mut dom, "the branch thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.iter().any(|t| t.id.as_str() == "on-branch"))).await;
    let title = |state: Store<UiModel>| thread_shell(state, "on-branch").title;

    // Escape leaves the title alone, and an empty one is refused.
    key_on(&mut dom, "Thread Feature work", Key::ContextMenu);
    click_control(&mut dom, "Rename thread");
    input_control(&mut dom, "Thread title", "Not this");
    key_on(&mut dom, "Thread title", Key::Escape);
    assert!(control(&dom, "Thread title").is_none() && title(state) == "Feature work");
    key_on(&mut dom, "Thread Feature work", Key::ContextMenu);
    click_control(&mut dom, "Rename thread");
    input_control(&mut dom, "Thread title", "   ");
    key_on(&mut dom, "Thread title", Key::Enter);
    assert!(rendered_text(&dom).contains("Thread title cannot be empty") && title(state) == "Feature work");

    // Enter sends one rename, and the row says the new title once the server has.
    key_on(&mut dom, "Thread Feature work", Key::ContextMenu);
    click_control(&mut dom, "Rename thread");
    input_control(&mut dom, "Thread title", "  Renamed work  ");
    key_on(&mut dom, "Thread title", Key::Enter);
    until(&mut dom, "the new title", |_| title(state) == "Renamed work").await;
    until(&mut dom, "the row", |dom| control(dom, "Thread Renamed work").is_some()).await;

    // The menu offers a new thread on the thread's branch, and the new-thread form starts there.
    key_on(&mut dom, "Thread Renamed work", Key::ContextMenu);
    assert!(control(&dom, "New thread on feature-x").is_some());
    click_control(&mut dom, "New thread on feature-x");
    assert!(state.peek().active_thread.is_none(), "a draft, not an existing thread");
    let choices = state.peek().new_thread_choices.clone();
    assert!(choices.values().any(|choice| choice.base_ref == "feature-x"), "{:?}", choices.len());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_snoozed_row_wakes_when_its_time_passes_with_one_timer_and_nothing_else_changing() {
    let (mut dom, props) = synth(json!([]), SETTLED(), json!({}));
    let state = state_of(&props);
    // A server that snoozes: without one no thread is shown as snoozed. The thread is the open one, so
    // its row stays visible on the folded Snoozed shelf.
    {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::runtime::transport_tests::api_fixture(&directory, "environment-a").config.unwrap();
        state.typed_config().set(Some(serde_json::from_value::<t3_contracts::ServerConfig>(config).unwrap()));
    }
    let wake = chrono::Utc::now() + chrono::Duration::milliseconds(2000);
    {
        let mut shell = state.shell();
        let mut shell = shell.write();
        let thread = &mut shell.snapshot.as_mut().unwrap().threads[0];
        thread.snoozed_until = Some(Some(serde_json::from_value(json!(wake.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))).unwrap()));
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(text(&dom).contains("Snoozed \u{b7} "), "{}", text(&dom));
    let before = (props.probe.application.get(), props.probe.sidebar_row.get());
    // Nothing is sent to the page and no event arrives: the row changes because its time has come.
    until(&mut dom, "the wake", |dom| !rendered_text(dom).contains("Snoozed")).await;
    // It woke on its timer and no one has visited since, so the row says so.
    assert!(text(&dom).contains("Woke"), "{}", text(&dom));
    let after = (props.probe.application.get(), props.probe.sidebar_row.get());
    assert!(after.0 <= before.0 + 2 && after.1 <= before.1 + 2, "one timer fires once: {before:?} then {after:?}");
}

/// A 1x1 PNG.
const PIXEL: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[tokio::test(flavor = "current_thread")]
async fn an_image_a_tool_viewed_is_shown_from_a_signed_url_only_while_its_row_is_open() {
    let fixture = Fixture::start(false).await;
    std::fs::write(fixture.workspace.join("pic.png"), PIXEL).unwrap();
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let mut thread = fixture.store.projection("thread", "thread-a").unwrap().unwrap();
    thread["runs"] = json!([{"id":"run-live","threadId":"thread-a","ordinal":1,"providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"fixture-model"},"providerThreadId":null,"userMessageId":"m","rootNodeId":"node","activeAttemptId":null,"status":"running","requestedAt":"2026-01-01T00:00:00.000Z","startedAt":"2026-01-01T00:00:00.000Z","completedAt":null,"checkpointId":null,"contextHandoffId":null}]);
    let tool = json!({"id":"look","threadId":"thread-a","runId":"run-live","nodeId":"node","providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":1,"status":"completed","title":"Look at the logo","startedAt":"2026-01-01T00:00:00.000Z","completedAt":"2026-01-01T00:00:01.000Z","updatedAt":"2026-01-01T00:00:01.000Z","type":"dynamic_tool","toolName":"view_image","input":{"path":"pic.png"},"output":null,"viewedImagePath":"pic.png"});
    thread["turnItems"] = json!([tool]);
    thread["visibleTurnItems"] = json!([{"sourceThreadId":"thread-a","sourceItemId":"look","position":0,"visibility":"local","item":tool}]);
    fixture.store.transaction(|transaction| t3_server::persistence::write_projection(transaction, "thread", "thread-a", &thread)).unwrap();
    open_thread(&mut dom, &props, state, "thread-a").await;
    // The running turn's one call is its live line; opening it lists the call, and opening that shows what it did.
    until(&mut dom, "the live line", |dom| control(dom, "Read pic.png").is_some()).await;
    click_control(&mut dom, "Read pic.png");
    until(&mut dom, "the call", |dom| control(dom, "Look at the logo").is_some()).await;
    assert!(control(&dom, "Viewed image pic.png").is_none(), "nothing is loaded for a closed row");
    click_control(&mut dom, "Look at the logo");
    until(&mut dom, "the image", |dom| control_attribute(dom, "Viewed image pic.png", "src").is_some_and(|(_, src)| src.is_some())).await;
    let src = control_attribute(&dom, "Viewed image pic.png", "src").and_then(|(_, src)| src).unwrap();
    assert!(src.starts_with(&fixture.address) && src.contains("/api/assets/"), "resolved against the connected environment: {src}");
    let served = reqwest::get(&src).await.unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.bytes().await.unwrap().as_ref(), PIXEL);
    click_control(&mut dom, "Look at the logo");
    assert!(control(&dom, "Viewed image pic.png").is_none(), "closing drops the picture");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn images_a_tool_returned_are_shown_from_signed_urls_only_while_its_row_is_open() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let mut thread = fixture.store.projection("thread", "thread-a").unwrap().unwrap();
    thread["runs"] = json!([{"id":"run-live","threadId":"thread-a","ordinal":1,"providerInstanceId":"codex","modelSelection":{"instanceId":"codex","model":"fixture-model"},"providerThreadId":null,"userMessageId":"m","rootNodeId":"node","activeAttemptId":null,"status":"running","requestedAt":"2026-01-01T00:00:00.000Z","startedAt":"2026-01-01T00:00:00.000Z","completedAt":null,"checkpointId":null,"contextHandoffId":null}]);
    let data = standard_base64(PIXEL);
    // An SVG the tool returned is never served inline; the PNG is, as the first (and only) image.
    let output = json!([{"type":"text","text":"captured"},{"type":"image","mimeType":"image/svg+xml","data":"PHN2Zz48L3N2Zz4="},{"type":"image","mimeType":"image/png","data":data}]);
    let tool = json!({"id":"shot","threadId":"thread-a","runId":"run-live","nodeId":"node","providerThreadId":null,"providerTurnId":null,"nativeItemRef":null,"parentItemId":null,"ordinal":1,"status":"completed","title":"Take a screenshot","startedAt":"2026-01-01T00:00:00.000Z","completedAt":"2026-01-01T00:00:01.000Z","updatedAt":"2026-01-01T00:00:01.000Z","type":"dynamic_tool","toolName":"screenshot","input":{},"output":output});
    thread["turnItems"] = json!([tool]);
    thread["visibleTurnItems"] = json!([{"sourceThreadId":"thread-a","sourceItemId":"shot","position":0,"visibility":"local","item":tool}]);
    fixture.store.transaction(|transaction| t3_server::persistence::write_projection(transaction, "thread", "thread-a", &thread)).unwrap();
    open_thread(&mut dom, &props, state, "thread-a").await;
    // The running turn's one call is its live line; opening it lists the call, and opening that shows what it did.
    until(&mut dom, "the live line", |dom| control(dom, "Take a screenshot").is_some()).await;
    click_control(&mut dom, "Take a screenshot");
    until(&mut dom, "the call", |dom| crate::runtime::transport_tests::all_controls(dom, "Take a screenshot").len() == 2).await;
    assert!(control(&dom, "Tool output image 1").is_none(), "nothing is loaded for a closed row");
    crate::runtime::transport_tests::click_nth_control(&mut dom, "Take a screenshot", 1);
    // The timeline withholds a tool's output; asking for it brings the images, still without their bytes.
    until(&mut dom, "the withheld output", |dom| control(dom, "Load full output").is_some()).await;
    assert!(control(&dom, "Tool output image 1").is_none());
    click_control(&mut dom, "Load full output");
    until(&mut dom, "the image", |dom| control_attribute(dom, "Tool output image 1", "src").is_some_and(|(_, src)| src.is_some())).await;
    assert!(control(&dom, "Tool output image 2").is_none(), "the SVG is not an image to show");
    let src = control_attribute(&dom, "Tool output image 1", "src").and_then(|(_, src)| src).unwrap();
    assert!(src.starts_with(&fixture.address) && src.contains("/api/assets/"), "{src}");
    let served = reqwest::get(&src).await.unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.bytes().await.unwrap().as_ref(), PIXEL);
    crate::runtime::transport_tests::click_nth_control(&mut dom, "Take a screenshot", 1);
    assert!(control(&dom, "Tool output image 1").is_none(), "closing drops the picture");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_open_in_button_offers_what_the_server_offers_and_is_disabled_without_the_operate_grant() {
    use crate::runtime::keys_tests::key_on;
    use dioxus::prelude::Key;
    // Whatever editors this machine has, the server is made to offer exactly these (and nothing is
    // launched). The config is set last, with no wait after it, so a later push cannot undo it.
    fn offer(state: Store<UiModel>, dom: &mut VirtualDom, editors: Vec<t3_contracts::EditorId>) {
        let mut config = state.typed_config();
        config.write().as_mut().unwrap().available_editors = t3_contracts::ForwardCompatibleArray(editors);
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    }
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    offer(state, &mut dom, vec![t3_contracts::EditorId::Zed, t3_contracts::EditorId::FileManager]);
    assert!(control(&dom, "Open in Zed").is_some(), "the first offered editor is the main button");
    assert!(control(&dom, "Zed").is_none() && control(&dom, "Reveal in file manager").is_none(), "the menu is closed");
    click_control(&mut dom, "Choose editor");
    assert!(control(&dom, "Zed").is_some() && control(&dom, "Reveal in file manager").is_some() && control(&dom, "Cursor").is_none());
    key_on(&mut dom, "Choose editor", Key::Escape);
    assert!(control(&dom, "Zed").is_none(), "Escape closes the menu");
    // Without the operate grant the buttons are there and disabled, with the reason in a title.
    let (mut reader, props) = fixture.client(Gate::default(), &[t3_contracts::AuthEnvironmentScope::OrchestrationRead]);
    let read_state = seed_free(&mut reader, &props).await;
    open_thread(&mut reader, &props, read_state, "thread-a").await;
    offer(read_state, &mut reader, vec![t3_contracts::EditorId::Zed]);
    assert_eq!(control_attribute(&reader, "Open in Zed", "disabled").and_then(|(_, value)| value).as_deref(), Some("true"));
    drop(reader);
    fixture.finish(dom).await;
}
async fn seed_free(dom: &mut VirtualDom, props: &Harness) -> Store<UiModel> {
    let state = props.state.borrow().unwrap();
    until(dom, "shell", |_| state.peek().shell.synchronized && state.peek().typed_config.is_some()).await;
    until(dom, "threads", |_| state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.len() == 2)).await;
    state
}

#[tokio::test(flavor = "current_thread")]
async fn a_forks_long_inherited_history_folds_like_settled_turns_and_opening_it_builds_few_rows() {
    // 120 turns of a source thread (a message, 22 calls and an answer each) that the fork carries,
    // then the marker the fork adds, then the fork's own turn.
    let mut items = vec![];
    for turn in 0..120 {
        let run = format!("source-run-{turn}");
        let tag = |mut item: Value| {
            item["runId"] = json!(run);
            item["threadId"] = json!("source");
            item["sourceThreadId"] = json!("source");
            item["visibility"] = json!("inherited");
            item
        };
        items.push(tag(user(&format!("u{turn}"), &format!("Inherited question {turn}"))));
        for call in 0..22 {
            items.push(tag(command(&format!("c{turn}-{call}"), &format!("step {call}"), "completed")));
        }
        items.push(tag(assistant(&format!("a{turn}"), &format!("Inherited answer {turn}"))));
    }
    let mut marker = with(base("fork-marker", "fork", "completed"), json!({"runId":null,"source":{"type":"run","threadId":"source","runId":"source-run-119"},"targetThreadId":"thread","visibility":"synthetic"}));
    marker["threadId"] = json!("thread");
    items.push(marker);
    items.push(with(user("own-user", "In the fork"), json!({"runId":"own-run"})));
    items.push(with(assistant("own-answer", "Own answer"), json!({"runId":"own-run"})));
    let runs = json!([{"id":"own-run","status":"completed","ordinal":1,"startedAt":"2026-01-01T00:00:00Z","completedAt":"2026-01-01T00:01:00Z"}]);
    let started = std::time::Instant::now();
    let (mut dom, props) = synth(json!(items), runs, json!({}));
    let built = props.probe.item.get();
    eprintln!("MEASURE mount of a fork with 120 inherited turns / {} items: {built} rows built in {:?}", items.len(), started.elapsed());
    assert!(built < 400, "inherited turns fold as settled ones do, so only their messages are built: {built}");
    assert!(text(&dom).contains("Forked from conversation") && control(&dom, "Open source conversation").is_some(), "{}", text(&dom).len());
    assert!(text(&dom).contains("Own answer") && text(&dom).contains("Inherited answer 119"));
    // Opening one inherited turn builds that turn's rows and no others.
    let before = props.probe.item.get();
    click_control(&mut dom, "Worked for*");
    let opened = props.probe.item.get() - before;
    assert!(opened < 60, "one fold opens, not the whole history: {opened}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_an_agent_created_is_a_card_under_the_answer_with_a_way_in() {
    let created = with(base("created", "thread_created", "completed"), json!({"targetThreadId":"child","title":"Child task"}));
    let (mut dom, props) = synth(json!([user("u", "Split the work"), created, assistant("a", "Started it.")]), SETTLED_WITH_END(), json!({}));
    assert!(control(&dom, "Open Child task").is_some(), "{}", text(&dom));
    assert!(text(&dom).contains("Child task") && text(&dom).contains("Open chat"));
    click_control(&mut dom, "Open Child task");
    assert_eq!(state_of(&props).peek().active_thread.as_deref(), Some("child"));
}

fn key_on_menu_escape(dom: &mut VirtualDom) {
    key_on_label(dom, "Implementation menu", dioxus::prelude::Key::Escape);
}

/// Standard padded base64, for a test that needs a tool's inline image.
fn standard_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    bytes
        .chunks(3)
        .flat_map(|chunk| {
            let n = chunk.iter().enumerate().fold(0u32, |n, (at, byte)| n | (u32::from(*byte) << (16 - 8 * at)));
            (0..4).map(move |at| if at <= chunk.len() { ALPHABET[((n >> (18 - 6 * at)) & 63) as usize] as char } else { '=' })
        })
        .collect()
}
